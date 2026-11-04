//! FLAC encoding through an external `flac` process, `.flac.part` files, JSON
//! metadata and recovery of unfinished recordings (spec 5.6, 5.7, 6.4, 6.6,
//! 6.8). Does not depend on `pipewire`, so it can be unit-tested.
//!
//! Error messages end up in the UI event log, so they are in Russian.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::{DateTime, Local, SecondsFormat};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use rustix::fs::{CWD, RenameFlags, renameat_with};
use rustix::io::Errno;
use serde::Serialize;

use crate::mixer::{Channel, SAMPLE_RATE};

/// External encoder, looked up in `PATH`.
const FLAC: &str = "flac";
/// Directory under `$HOME` that holds recordings.
const RECORDINGS_DIR: &str = "Calls";
/// Interleaved channels in the file: L = mic, R = them.
const CHANNELS: usize = 2;
/// Free space required before a recording starts (spec 6.8).
pub const MIN_FREE_BYTES: u64 = 1_000_000_000;
/// Blocks buffered between the writer thread and `flac` (5 s of audio). A full
/// buffer means `flac` stopped reading; it is handled like a crash.
const ENCODER_QUEUE_BLOCKS: usize = 50;
/// How much of the `flac` stderr is kept for error messages.
const STDERR_LIMIT: u64 = 4096;
/// Suffix of files that are still being written.
const PART_SUFFIX: &str = ".part";
/// Suffix of an unfinished recording (spec 6.6).
const UNFINISHED_SUFFIX: &str = ".flac.part";
/// Longest meeting title kept in a file name, in bytes. Names stay well below
/// the 255-byte limit with the date, suffixes and extensions added.
pub const MAX_TITLE_BYTES: usize = 150;
/// Start time in file names.
const NAME_TIME_FORMAT: &str = "%Y-%m-%d_%H-%M";
/// Added to the name of a recording that was not stopped before the app
/// closed (spec 5.7).
pub const INTERRUPTED_MARK: &str = "прервано";

#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    #[error("не найден домашний каталог")]
    NoHome,
    #[error("{action} {}: {source}", .path.display())]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("не удалось запустить flac: {0}")]
    Spawn(#[source] io::Error),
    #[error("flac ({}): {detail}", .path.display())]
    Encoder { path: PathBuf, detail: String },
}

fn io_err(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> WriterError {
    let path = path.to_path_buf();
    move |source| WriterError::Io {
        action,
        path,
        source,
    }
}

/// `$HOME/Calls`.
pub fn default_dir() -> Result<PathBuf, WriterError> {
    std::env::home_dir()
        .map(|home| home.join(RECORDINGS_DIR))
        .ok_or(WriterError::NoHome)
}

/// Bytes available to the user in `dir`, or in its closest existing ancestor
/// when `dir` is not created yet.
pub fn free_space(dir: &Path) -> Result<u64, WriterError> {
    let existing = dir.ancestors().find(|p| p.exists()).unwrap_or(dir);
    let stat = rustix::fs::statvfs(existing).map_err(|e| {
        io_err("проверка свободного места", existing)(e.into())
    })?;
    Ok(stat.f_bavail.saturating_mul(stat.f_frsize))
}

/// Whether `dir` has at least `MIN_FREE_BYTES` free (spec 6.8).
pub fn has_enough_space(dir: &Path) -> Result<bool, WriterError> {
    free_space(dir).map(|free| free >= MIN_FREE_BYTES)
}

/// A pause in the recording: position in the file and wall-clock length.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pause {
    pub at_sec: f64,
    pub duration_sec: f64,
}

/// A device a capture stream got connected to, at a position in the file.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DeviceChange {
    pub at_sec: f64,
    pub channel: &'static str,
    pub device: String,
    pub description: String,
}

#[derive(Debug, Serialize)]
struct Metadata<'a> {
    title: &'a str,
    /// The app closed without stopping the recording.
    interrupted: bool,
    started_at: String,
    finished_at: String,
    duration_sec: f64,
    channels: BTreeMap<&'static str, &'static str>,
    files: Vec<String>,
    pauses: &'a [Pause],
    device_changes: &'a [DeviceChange],
}

/// Result of writing one block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    Written,
    /// Paused: the block is not part of the recording.
    Skipped,
    /// `flac` failed (spec 6.4). The failed segment was kept as `saved` and
    /// the block went to a new segment.
    Restarted {
        saved: PathBuf,
        cause: String,
    },
}

/// Files of a recording that was stopped successfully.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Saved {
    pub files: Vec<PathBuf>,
    pub metadata: PathBuf,
}

/// A recording in progress: one or more FLAC segments plus metadata.
///
/// Segment 1 is written to `<stem>.flac.part` and becomes `<stem>.flac`. If
/// `flac` fails, the current segment is kept as `<stem>-part<N>.flac` and
/// recording continues in `<stem>-part<N+1>.flac.part`. Positions (`at_sec`)
/// count the audio written so far across all segments, without pauses.
///
/// The stem is `<title>_<start time>` (see [`file_base`]). The title may change
/// while recording; [`Recording::stop`] renames the files to the final one.
#[derive(Debug)]
pub struct Recording {
    dir: PathBuf,
    /// Name the stem was made from, before a `-2`, `-3` suffix.
    base: String,
    stem: String,
    started_at: DateTime<Local>,
    encoder: Option<FlacEncoder>,
    segment: u32,
    /// Segments are named `-part<N>` once the first one has failed.
    split: bool,
    files: Vec<PathBuf>,
    frames: u64,
    paused_at: Option<DateTime<Local>>,
    pauses: Vec<Pause>,
    device_changes: Vec<DeviceChange>,
}

impl Recording {
    /// Creates `dir`, picks a free file name from `title` and `started_at` and
    /// starts `flac`.
    pub fn start(
        dir: &Path,
        started_at: DateTime<Local>,
        title: &str,
    ) -> Result<Self, WriterError> {
        fs::create_dir_all(dir).map_err(io_err("создание каталога", dir))?;
        let base = file_base(title, started_at);
        let stem = unique_stem(dir, &base)?;
        let mut recording = Self {
            dir: dir.to_path_buf(),
            base,
            stem,
            started_at,
            encoder: None,
            segment: 1,
            split: false,
            files: Vec::new(),
            frames: 0,
            paused_at: None,
            pauses: Vec::new(),
            device_changes: Vec::new(),
        };
        recording.start_segment()?;
        Ok(recording)
    }

    /// Path of the recording under its current stem.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.join(format!("{}.flac", self.stem))
    }

    /// Path the recording gets when it is stopped with `title`, as shown in
    /// the UI. A `-2` suffix for a taken name is only picked at stop.
    #[must_use]
    pub fn path_for(&self, title: &str) -> PathBuf {
        let base = file_base(title, self.started_at);
        if base == self.base {
            self.path()
        } else {
            self.dir.join(format!("{base}.flac"))
        }
    }

    /// Recorded audio so far, without pauses.
    #[must_use]
    pub fn duration(&self) -> Duration {
        frames_to_duration(self.frames)
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }

    /// Writes interleaved stereo samples. While paused the block is skipped.
    /// If `flac` failed, the segment is saved and a new one is started; an
    /// error means no encoder could be started and the block is lost. The next
    /// call tries again.
    pub fn write_block(&mut self, stereo: &[i16]) -> Result<WriteOutcome, WriterError> {
        if self.is_paused() {
            return Ok(WriteOutcome::Skipped);
        }
        let frames = u64::try_from(stereo.len() / CHANNELS).unwrap_or(u64::MAX);
        let mut bytes = to_le_bytes(stereo);
        let mut outcome = WriteOutcome::Written;

        if let Some(encoder) = self.encoder.as_mut() {
            match encoder.send(bytes) {
                Ok(()) => {
                    self.frames = self.frames.saturating_add(frames);
                    return Ok(outcome);
                }
                Err((returned, cause)) => {
                    bytes = returned;
                    outcome = self.fail_segment(cause);
                }
            }
        }

        self.start_segment()?;
        if let Some(encoder) = self.encoder.as_mut()
            && let Err((_, detail)) = encoder.send(bytes)
        {
            return Err(WriterError::Encoder {
                path: encoder.part.clone(),
                detail,
            });
        }
        self.frames = self.frames.saturating_add(frames);
        Ok(outcome)
    }

    pub fn pause(&mut self, at: DateTime<Local>) {
        if self.paused_at.is_none() {
            self.paused_at = Some(at);
        }
    }

    pub fn resume(&mut self, at: DateTime<Local>) {
        if let Some(since) = self.paused_at.take() {
            self.pauses.push(Pause {
                at_sec: seconds(self.duration()),
                duration_sec: seconds(elapsed(since, at)),
            });
        }
    }

    /// Records the device a channel is connected to at the current position.
    pub fn device_change(&mut self, channel: Channel, device: &str, description: &str) {
        self.device_changes.push(DeviceChange {
            at_sec: seconds(self.duration()),
            channel: channel_name(channel),
            device: device.to_owned(),
            description: description.to_owned(),
        });
    }

    /// Finishes `flac`, renames the segment to `.flac` and writes the JSON.
    /// If `flac` fails here, the segment stays `.flac.part` so it is offered
    /// for recovery at the next start; the JSON is written anyway. Files are
    /// named after `title`; if it changed since the start, segments saved
    /// earlier are renamed. A file that cannot be renamed keeps its name.
    /// `interrupted`: the app was closed without stopping the recording; the
    /// name gets `INTERRUPTED_MARK` (spec 5.7).
    pub fn stop(
        mut self,
        finished_at: DateTime<Local>,
        title: &str,
        interrupted: bool,
    ) -> Result<Saved, WriterError> {
        // Saved while paused: the audio ends where the pause began, and the
        // time after it is not a pause within the recording.
        let finished_at = self.paused_at.take().unwrap_or(finished_at);
        let mut base = file_base(title, self.started_at);
        if interrupted {
            base = format!("{base}_{INTERRUPTED_MARK}");
        }
        let mut failure = self.retitle(base).err();
        if let Some(encoder) = self.encoder.take() {
            let part = encoder.part.clone();
            let target = self.segment_path();
            match encoder.finish() {
                Ok(()) => match rename_new(&part, &target) {
                    Ok(()) => self.files.push(target),
                    Err(e) => {
                        self.files.push(part.clone());
                        failure.get_or_insert(io_err("переименование", &part)(e));
                    }
                },
                Err(e) => {
                    self.files.push(part);
                    failure.get_or_insert(e);
                }
            }
        }
        let metadata = self.write_metadata(finished_at, title.trim(), interrupted);
        match (failure, metadata) {
            (None, Ok(metadata)) => Ok(Saved {
                files: self.files,
                metadata,
            }),
            (Some(e), _) | (None, Err(e)) => Err(e),
        }
    }

    /// Stops `flac` and deletes every file of this recording: the running
    /// segment and the ones saved after `flac` failures. No metadata is
    /// written. Only on an explicit user choice (spec 3, 6.6). Files that
    /// cannot be deleted stay; the first error is returned.
    pub fn discard(mut self) -> Result<(), WriterError> {
        if let Some(encoder) = self.encoder.take() {
            self.files.push(encoder.part.clone());
            // The file is deleted, so how flac ended does not matter.
            let _ = encoder.abort();
        }
        let mut failure = None;
        for file in &self.files {
            match fs::remove_file(file) {
                // flac creates its file only after the first input.
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    failure.get_or_insert(io_err("удаление", file)(e));
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Moves the recording to a stem made from `base`: renames the segments saved
    /// so far. The running segment is renamed when it is finished. On error
    /// the files that could not be renamed keep their old names.
    fn retitle(&mut self, base: String) -> Result<(), WriterError> {
        if base == self.base {
            return Ok(());
        }
        let stem = unique_stem(&self.dir, &base)?;
        let mut failure = None;
        let files = std::mem::take(&mut self.files);
        for file in files {
            let renamed = file
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix(self.stem.as_str()))
                .map(|rest| self.dir.join(format!("{stem}{rest}")));
            let Some(target) = renamed else {
                self.files.push(file);
                continue;
            };
            match rename_new(&file, &target) {
                Ok(()) => self.files.push(target),
                Err(e) => {
                    failure.get_or_insert(io_err("переименование", &file)(e));
                    self.files.push(file);
                }
            }
        }
        self.base = base;
        self.stem = stem;
        failure.map_or(Ok(()), Err)
    }

    /// Final path of the current segment.
    fn segment_path(&self) -> PathBuf {
        if self.split {
            self.dir
                .join(format!("{}-part{}.flac", self.stem, self.segment))
        } else {
            self.path()
        }
    }

    fn start_segment(&mut self) -> Result<(), WriterError> {
        let target = self.segment_path();
        let part = with_suffix(&target, PART_SUFFIX);
        // flac refuses to overwrite, but by then the existing file would be
        // renamed as if it were ours. Never touch files we did not create.
        for path in [&target, &part] {
            if path.exists() {
                return Err(io_err("создание файла", path)(
                    io::ErrorKind::AlreadyExists.into(),
                ));
            }
        }
        self.encoder = Some(FlacEncoder::spawn(part)?);
        Ok(())
    }

    /// Stops a failed encoder and keeps what it wrote as `-part<N>.flac`.
    fn fail_segment(&mut self, cause: String) -> WriteOutcome {
        let Some(encoder) = self.encoder.take() else {
            return WriteOutcome::Written;
        };
        let part = encoder.part.clone();
        let mut cause = cause;
        if let Some(detail) = encoder.abort() {
            cause = format!("{cause}; {detail}");
        }
        self.split = true;
        let target = self.segment_path();
        let saved = match rename_new(&part, &target) {
            Ok(()) => target,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                cause = format!("{cause}; файл не был создан");
                part
            }
            Err(e) => {
                // The `.part` file stays and is offered for recovery later.
                cause = format!("{cause}; переименование {}: {e}", part.display());
                part
            }
        };
        if saved.exists() {
            self.files.push(saved.clone());
        }
        self.segment = self.segment.saturating_add(1);
        WriteOutcome::Restarted { saved, cause }
    }

    fn write_metadata(
        &self,
        finished_at: DateTime<Local>,
        title: &str,
        interrupted: bool,
    ) -> Result<PathBuf, WriterError> {
        let path = self.dir.join(format!("{}.json", self.stem));
        let part = with_suffix(&path, PART_SUFFIX);
        let metadata = Metadata {
            title,
            interrupted,
            started_at: self.started_at.to_rfc3339_opts(SecondsFormat::Secs, false),
            finished_at: finished_at.to_rfc3339_opts(SecondsFormat::Secs, false),
            duration_sec: seconds(self.duration()),
            channels: BTreeMap::from([("0", "me"), ("1", "them")]),
            files: self
                .files
                .iter()
                .filter_map(|f| f.file_name())
                .map(|f| f.to_string_lossy().into_owned())
                .collect(),
            pauses: &self.pauses,
            device_changes: &self.device_changes,
        };
        let json = serde_json::to_vec_pretty(&metadata).map_err(io::Error::other);
        json.and_then(|json| fs::write(&part, json))
            .map_err(io_err("запись метаданных", &part))?;
        rename_new(&part, &path).map_err(io_err("переименование", &part))?;
        Ok(path)
    }

    /// Simulates a `flac` crash once it has created its output file. flac
    /// opens the file only after it has read the first input.
    #[cfg(test)]
    fn kill_encoder(&mut self) {
        if let Some(encoder) = self.encoder.as_mut() {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while !encoder.part.exists() && std::time::Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert!(encoder.part.exists(), "flac did not create its file");
            encoder.child.kill().unwrap();
            encoder.child.wait().unwrap();
        }
    }
}

/// A running `flac` process. Writes go through a bounded queue to a feeder
/// thread, so a stalled `flac` never blocks the caller (the mixer clock).
#[derive(Debug)]
struct FlacEncoder {
    part: PathBuf,
    child: Child,
    tx: Option<Sender<Vec<u8>>>,
    feeder: Option<JoinHandle<io::Result<()>>>,
    stderr: Option<JoinHandle<String>>,
}

impl FlacEncoder {
    fn spawn(part: PathBuf) -> Result<Self, WriterError> {
        let mut child = encoder_command(&part, false)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(WriterError::Spawn)?;
        let stdin = child.stdin.take();
        let stderr = child.stderr.take();
        let (tx, rx) = crossbeam_channel::bounded(ENCODER_QUEUE_BLOCKS);
        // From here on Drop closes stdin and reaps the process on every path.
        let mut encoder = Self {
            part,
            child,
            tx: Some(tx),
            feeder: None,
            stderr: None,
        };
        let stdin = stdin.ok_or_else(|| WriterError::Spawn(io::Error::other("нет stdin")))?;
        encoder.feeder = Some(
            thread::Builder::new()
                .name("flac-feeder".into())
                .spawn(move || feed(stdin, &rx))
                .map_err(WriterError::Spawn)?,
        );
        // Without the reader only error text is lost; flac then gets EPIPE
        // on stderr, which it ignores.
        encoder.stderr = stderr.and_then(|stderr| {
            thread::Builder::new()
                .name("flac-stderr".into())
                .spawn(move || collect_stderr(stderr))
                .ok()
        });
        Ok(encoder)
    }

    /// Queues bytes for `flac`. On failure returns the bytes and the cause.
    fn send(&mut self, bytes: Vec<u8>) -> Result<(), (Vec<u8>, String)> {
        match self.child.try_wait() {
            Ok(None) => {}
            Ok(Some(status)) => {
                return Err((bytes, format!("flac неожиданно завершился ({status})")));
            }
            Err(e) => return Err((bytes, format!("состояние процесса flac: {e}"))),
        }
        let Some(tx) = self.tx.as_ref() else {
            return Err((bytes, "flac уже остановлен".to_owned()));
        };
        tx.try_send(bytes).map_err(|e| match e {
            TrySendError::Full(bytes) => (bytes, "flac не принимает данные".to_owned()),
            TrySendError::Disconnected(bytes) => (bytes, "ошибка записи в flac".to_owned()),
        })
    }

    /// Closes stdin and waits for `flac` to write the file header and exit.
    fn finish(mut self) -> Result<(), WriterError> {
        match self.close() {
            None => Ok(()),
            Some(detail) => Err(WriterError::Encoder {
                path: self.part.clone(),
                detail,
            }),
        }
    }

    /// Kills `flac` and reaps it. Returns details of the failure, if any.
    fn abort(mut self) -> Option<String> {
        // Already exited is fine: the process is reaped by close() anyway.
        let _ = self.child.kill();
        self.close()
    }

    /// Closes stdin, joins the threads and reaps the process. Idempotent.
    fn close(&mut self) -> Option<String> {
        self.tx = None;
        let fed = match self.feeder.take().map(JoinHandle::join) {
            None | Some(Ok(Ok(()))) => Ok(()),
            Some(Ok(Err(e))) => Err(e.to_string()),
            Some(Err(_)) => Err("поток записи в flac аварийно завершился".to_owned()),
        };
        let status = self.child.wait();
        let stderr = self
            .stderr
            .take()
            .and_then(|handle| handle.join().ok())
            .unwrap_or_default();
        describe_failure(fed, &status, stderr.trim())
    }
}

impl Drop for FlacEncoder {
    fn drop(&mut self) {
        // Not finished explicitly: still let flac finish the file normally.
        let _ = self.close();
    }
}

fn describe_failure(
    fed: Result<(), String>,
    status: &io::Result<ExitStatus>,
    stderr: &str,
) -> Option<String> {
    let mut problems = Vec::new();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => problems.push(format!("код завершения: {status}")),
        Err(e) => problems.push(format!("ожидание процесса: {e}")),
    }
    if let Err(e) = fed {
        problems.push(format!("запись: {e}"));
    }
    if problems.is_empty() {
        return None;
    }
    if !stderr.is_empty() {
        problems.push(stderr.to_owned());
    }
    Some(problems.join("; "))
}

fn feed(mut stdin: ChildStdin, rx: &Receiver<Vec<u8>>) -> io::Result<()> {
    for bytes in rx {
        stdin.write_all(&bytes)?;
    }
    // stdin is dropped here: flac sees EOF and finalizes the file.
    Ok(())
}

fn collect_stderr(mut stderr: ChildStderr) -> String {
    let mut text = Vec::new();
    // Errors only cost the diagnostic text. Drain the rest so flac never
    // blocks on a full pipe.
    let _ = (&mut stderr).take(STDERR_LIMIT).read_to_end(&mut text);
    let _ = io::copy(&mut stderr, &mut io::sink());
    String::from_utf8_lossy(&text).into_owned()
}

/// `flac` reading raw PCM in the recording format from stdin into `output`.
fn encoder_command(output: &Path, force: bool) -> Command {
    let mut command = Command::new(FLAC);
    command.args([
        "--silent",
        "--force-raw-format",
        "--endian=little",
        "--sign=signed",
        "--bps=16",
    ]);
    command
        .arg(format!("--channels={CHANNELS}"))
        .arg(format!("--sample-rate={SAMPLE_RATE}"));
    if force {
        command.arg("--force");
    }
    command.arg("-o").arg(output).arg("-");
    command
}

/// `.flac.part` files left by a crash (spec 6.6), sorted by name.
pub fn find_unfinished(dir: &Path) -> Result<Vec<PathBuf>, WriterError> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_err("чтение каталога", dir)(e)),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err("чтение каталога", dir))?;
        let path = entry.path();
        if entry
            .file_name()
            .to_string_lossy()
            .ends_with(UNFINISHED_SUFFIX)
            && path.is_file()
        {
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

/// Decodes an unfinished `X.flac.part` through errors and encodes it again
/// into `X.flac`. The `.part` file is removed only after `X.flac` exists.
pub fn recover(part: &Path) -> Result<PathBuf, WriterError> {
    let name = part.to_string_lossy();
    let Some(target) = name.strip_suffix(PART_SUFFIX).map(PathBuf::from) else {
        return Err(io_err("восстановление", part)(
            io::ErrorKind::InvalidInput.into(),
        ));
    };
    if target.exists() {
        return Err(io_err("восстановление", &target)(
            io::ErrorKind::AlreadyExists.into(),
        ));
    }
    // Not `.flac.part`: an interrupted recovery must not look like a recording.
    let tmp = with_suffix(&target, ".tmp");

    let mut decoder = Command::new(FLAC)
        .args([
            "--decode",
            "--silent",
            "--decode-through-errors",
            "--force-raw-format",
            "--endian=little",
            "--sign=signed",
            "--stdout",
        ])
        .arg(part)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Decode errors are expected in a damaged file and may be many lines.
        .stderr(Stdio::null())
        .spawn()
        .map_err(WriterError::Spawn)?;
    let encoded = match decoder.stdout.take() {
        Some(pcm) => encoder_command(&tmp, true)
            .stdin(Stdio::from(pcm))
            .stdout(Stdio::null())
            .output(),
        None => Err(io::Error::other("нет stdout декодера")),
    };
    let decoder_status = decoder.wait();

    let failure = match (&decoder_status, &encoded) {
        (Ok(d), Ok(e)) if d.success() && e.status.success() => None,
        (Ok(d), _) if !d.success() => Some(format!("декодирование: {d}")),
        (Err(e), _) => Some(format!("декодирование: {e}")),
        (_, Ok(e)) => Some(format!(
            "кодирование: {}; {}",
            e.status,
            String::from_utf8_lossy(&e.stderr).trim()
        )),
        (_, Err(e)) => Some(format!("кодирование: {e}")),
    };
    if let Some(detail) = failure {
        // Best effort: a leftover .tmp is overwritten by the next attempt.
        let _ = fs::remove_file(&tmp);
        return Err(WriterError::Encoder {
            path: part.to_path_buf(),
            detail,
        });
    }
    rename_new(&tmp, &target).map_err(io_err("переименование", &tmp))?;
    fs::remove_file(part).map_err(io_err("удаление", part))?;
    Ok(target)
}

/// Deletes an unfinished recording. Only on an explicit user choice (spec 6.6).
pub fn discard(part: &Path) -> Result<(), WriterError> {
    fs::remove_file(part).map_err(io_err("удаление", part))
}

/// File name of a recording without extension: `<title>_<start time>`, or
/// just the start time without a title. The title is made safe for a file
/// name: no `/`, control characters or leading dots, at most
/// `MAX_TITLE_BYTES`.
#[must_use]
pub fn file_base(title: &str, started_at: DateTime<Local>) -> String {
    let time = started_at.format(NAME_TIME_FORMAT);
    let mut safe = String::new();
    for c in title.trim().chars() {
        let c = if c == '/' { '-' } else { c };
        if c.is_control() || safe.len() + c.len_utf8() > MAX_TITLE_BYTES {
            continue;
        }
        safe.push(c);
    }
    let safe = safe.trim_start_matches('.').trim();
    if safe.is_empty() {
        time.to_string()
    } else {
        format!("{safe}_{time}")
    }
}

/// First of `base`, `base-2`, `base-3`, … that no file in `dir` uses.
fn unique_stem(dir: &Path, base: &str) -> Result<String, WriterError> {
    let names: Vec<String> = fs::read_dir(dir)
        .map_err(io_err("чтение каталога", dir))?
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    (1..=u32::MAX)
        .map(|n| {
            if n == 1 {
                base.to_owned()
            } else {
                format!("{base}-{n}")
            }
        })
        .find(|stem| !names.iter().any(|name| uses_stem(name, stem)))
        .ok_or_else(|| {
            io_err("выбор имени файла", dir)(io::ErrorKind::AlreadyExists.into())
        })
}

/// Whether `name` is one of the files of a recording named `stem`.
fn uses_stem(name: &str, stem: &str) -> bool {
    name.strip_prefix(stem)
        .is_some_and(|rest| rest.starts_with('.') || rest.starts_with("-part"))
}

/// Renames without ever replacing an existing file.
fn rename_new(from: &Path, to: &Path) -> io::Result<()> {
    match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        // Filesystems without RENAME_NOREPLACE.
        Err(Errno::INVAL | Errno::NOSYS) => {
            if to.exists() {
                Err(io::ErrorKind::AlreadyExists.into())
            } else {
                fs::rename(from, to)
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn frames_to_duration(frames: u64) -> Duration {
    let rate = u64::from(SAMPLE_RATE);
    Duration::from_secs(frames / rate)
        .saturating_add(Duration::from_nanos(frames % rate * 1_000_000_000 / rate))
}

fn elapsed(from: DateTime<Local>, to: DateTime<Local>) -> Duration {
    // A clock step backwards gives zero rather than an error.
    to.signed_duration_since(from).to_std().unwrap_or_default()
}

/// Seconds rounded to 0.1, as in the metadata.
fn seconds(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 10.0).round() / 10.0
}

fn channel_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Mic => "me",
        Channel::Them => "them",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use serde_json::Value;

    const FRAMES_PER_BLOCK: usize = 1600;

    fn at(secs: i64) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 21, 14, 0, 12).unwrap() + chrono::TimeDelta::seconds(secs)
    }

    /// Stereo block with values unique to `index`.
    fn block(index: usize) -> Vec<i16> {
        (0..FRAMES_PER_BLOCK * CHANNELS)
            .map(|i| i16::try_from((index * 7919 + i) % 30_000).unwrap() - 15_000)
            .collect()
    }

    fn blocks(range: std::ops::Range<usize>) -> Vec<i16> {
        range.flat_map(block).collect()
    }

    fn decode(path: &Path) -> Vec<i16> {
        let out = Command::new(FLAC)
            .args([
                "-d",
                "--silent",
                "--force-raw-format",
                "--endian=little",
                "--sign=signed",
                "-c",
            ])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect()
    }

    fn rec_path(dir: &Path, suffix: &str) -> PathBuf {
        dir.join(format!("2026-10-21_14-00{suffix}"))
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn records_flac_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = Recording::start(dir.path(), at(0), "").unwrap();
        assert_eq!(rec.path(), dir.path().join("2026-10-21_14-00.flac"));
        rec.device_change(Channel::Mic, "alsa_input.pci", "Встроенный микрофон");
        rec.device_change(Channel::Them, "alsa_output.pci", "Встроенные динамики");
        for i in 0..20 {
            assert_eq!(rec.write_block(&block(i)).unwrap(), WriteOutcome::Written);
        }
        assert_eq!(rec.duration(), Duration::from_secs(2));

        let saved = rec.stop(at(5), "", false).unwrap();
        let flac = dir.path().join("2026-10-21_14-00.flac");
        let json = dir.path().join("2026-10-21_14-00.json");
        assert_eq!(saved.files, vec![flac.clone()]);
        assert_eq!(saved.metadata, json);
        assert_eq!(
            names(dir.path()),
            ["2026-10-21_14-00.flac", "2026-10-21_14-00.json"]
        );
        assert_eq!(decode(&flac), blocks(0..20));

        let meta = read_json(&json);
        assert_eq!(
            meta["started_at"],
            at(0).to_rfc3339_opts(SecondsFormat::Secs, false)
        );
        assert_eq!(
            meta["finished_at"],
            at(5).to_rfc3339_opts(SecondsFormat::Secs, false)
        );
        assert_eq!(meta["duration_sec"], 2.0);
        assert_eq!(
            meta["channels"],
            serde_json::json!({ "0": "me", "1": "them" })
        );
        assert_eq!(meta["files"], serde_json::json!(["2026-10-21_14-00.flac"]));
        assert_eq!(meta["pauses"], serde_json::json!([]));
        assert_eq!(meta["interrupted"], false);
        assert_eq!(meta["title"], "");
        assert_eq!(
            meta["device_changes"],
            serde_json::json!([
                { "at_sec": 0.0, "channel": "me", "device": "alsa_input.pci", "description": "Встроенный микрофон" },
                { "at_sec": 0.0, "channel": "them", "device": "alsa_output.pci", "description": "Встроенные динамики" },
            ])
        );
    }

    #[test]
    fn positions_exclude_pauses() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = Recording::start(dir.path(), at(0), "").unwrap();
        for i in 0..15 {
            rec.write_block(&block(i)).unwrap();
        }
        rec.pause(at(10));
        assert!(rec.is_paused());
        assert_eq!(rec.write_block(&block(99)).unwrap(), WriteOutcome::Skipped);
        rec.resume(at(40));
        rec.device_change(Channel::Mic, "bluez_input.70", "Jabra");
        for i in 15..20 {
            rec.write_block(&block(i)).unwrap();
        }
        // Saved while paused: the recording ends where the pause began.
        rec.pause(at(50));
        let saved = rec.stop(at(55), "", false).unwrap();

        assert_eq!(decode(&saved.files[0]), blocks(0..20));
        let meta = read_json(&saved.metadata);
        assert_eq!(meta["duration_sec"], 2.0);
        assert_eq!(
            meta["pauses"],
            serde_json::json!([
                { "at_sec": 1.5, "duration_sec": 30.0 },
            ])
        );
        assert_eq!(meta["device_changes"][0]["at_sec"], 1.5);
        assert_eq!(
            meta["finished_at"],
            at(50).to_rfc3339_opts(SecondsFormat::Secs, false)
        );
    }

    #[test]
    fn existing_names_get_numeric_suffix() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("2026-10-21_14-00.flac"), b"").unwrap();
        fs::write(dir.path().join("2026-10-21_14-00-2-part1.flac"), b"").unwrap();
        let rec = Recording::start(dir.path(), at(0), "").unwrap();
        assert_eq!(rec.path(), dir.path().join("2026-10-21_14-00-3.flac"));
        let saved = rec.stop(at(1), "", false).unwrap();
        assert_eq!(saved.files, vec![rec_path(dir.path(), "-3.flac")]);
        assert_eq!(saved.metadata, rec_path(dir.path(), "-3.json"));
    }

    #[test]
    fn stem_ownership() {
        let stem = "2026-10-21_14-00";
        assert!(uses_stem("2026-10-21_14-00.flac", stem));
        assert!(uses_stem("2026-10-21_14-00.flac.part", stem));
        assert!(uses_stem("2026-10-21_14-00.json", stem));
        assert!(uses_stem("2026-10-21_14-00-part2.flac.part", stem));
        assert!(!uses_stem("2026-10-21_14-00-2.flac", stem));
        assert!(!uses_stem("2026-10-21_14-01.flac", stem));
    }

    #[test]
    fn title_goes_before_start_time() {
        assert_eq!(file_base("", at(0)), "2026-10-21_14-00");
        assert_eq!(file_base("   ", at(0)), "2026-10-21_14-00");
        assert_eq!(
            file_base("  Планёрка с Jabra ", at(0)),
            "Планёрка с Jabra_2026-10-21_14-00"
        );
        // Not a path, not hidden, no control characters.
        assert_eq!(file_base("a/b\tc\n", at(0)), "a-bc_2026-10-21_14-00");
        assert_eq!(file_base("../x", at(0)), "-x_2026-10-21_14-00");
        assert_eq!(file_base("..", at(0)), "2026-10-21_14-00");

        let long = "я".repeat(200);
        let base = file_base(&long, at(0));
        let title = base.strip_suffix("_2026-10-21_14-00").unwrap();
        assert_eq!(title, "я".repeat(MAX_TITLE_BYTES / 2));
    }

    #[test]
    fn title_at_start_names_the_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = Recording::start(dir.path(), at(0), "Планёрка").unwrap();
        rec.write_block(&block(0)).unwrap();
        let flac = dir.path().join("Планёрка_2026-10-21_14-00.flac");
        assert_eq!(rec.path(), flac);
        assert_eq!(rec.path_for("Планёрка"), flac);

        let saved = rec.stop(at(5), "Планёрка", false).unwrap();
        assert_eq!(saved.files, vec![flac]);
        assert_eq!(
            names(dir.path()),
            [
                "Планёрка_2026-10-21_14-00.flac",
                "Планёрка_2026-10-21_14-00.json"
            ]
        );
        assert_eq!(read_json(&saved.metadata)["title"], "Планёрка");
    }

    #[test]
    fn title_changed_while_recording_renames_all_parts() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = Recording::start(dir.path(), at(0), "").unwrap();
        for i in 0..3 {
            rec.write_block(&block(i)).unwrap();
        }
        rec.kill_encoder();
        rec.write_block(&block(3)).unwrap();
        assert_eq!(
            rec.path_for("Ретро"),
            dir.path().join("Ретро_2026-10-21_14-00.flac")
        );
        // The time is the start of the recording, not the stop.
        let saved = rec.stop(at(3600), " Ретро ", false).unwrap();

        let part1 = dir.path().join("Ретро_2026-10-21_14-00-part1.flac");
        let part2 = dir.path().join("Ретро_2026-10-21_14-00-part2.flac");
        assert_eq!(saved.files, vec![part1.clone(), part2.clone()]);
        assert!(part1.exists());
        assert_eq!(decode(&part2), blocks(3..4));
        assert_eq!(
            saved.metadata,
            dir.path().join("Ретро_2026-10-21_14-00.json")
        );
        let meta = read_json(&saved.metadata);
        assert_eq!(meta["title"], "Ретро");
        assert_eq!(
            meta["files"],
            serde_json::json!([
                "Ретро_2026-10-21_14-00-part1.flac",
                "Ретро_2026-10-21_14-00-part2.flac"
            ])
        );
        assert_eq!(names(dir.path()).len(), 3);
    }

    #[test]
    fn interrupted_recording_is_marked() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = Recording::start(dir.path(), at(0), "Ретро").unwrap();
        rec.write_block(&block(0)).unwrap();
        let saved = rec.stop(at(5), "Ретро", true).unwrap();
        assert_eq!(
            saved.files,
            vec![dir.path().join("Ретро_2026-10-21_14-00_прервано.flac")]
        );
        let meta = read_json(&saved.metadata);
        assert_eq!(meta["interrupted"], true);
        assert_eq!(meta["title"], "Ретро");

        let rec = Recording::start(dir.path(), at(60), "").unwrap();
        let saved = rec.stop(at(61), "", true).unwrap();
        assert_eq!(
            saved.metadata,
            dir.path().join("2026-10-21_14-01_прервано.json")
        );
        assert_eq!(read_json(&saved.metadata)["interrupted"], true);
    }

    #[test]
    fn discard_deletes_only_this_recording() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("other.flac"), b"keep").unwrap();
        let mut rec = Recording::start(dir.path(), at(0), "").unwrap();
        for i in 0..3 {
            rec.write_block(&block(i)).unwrap();
        }
        rec.kill_encoder();
        rec.write_block(&block(3)).unwrap();
        assert!(dir.path().join("2026-10-21_14-00-part1.flac").exists());

        rec.discard().unwrap();
        assert_eq!(names(dir.path()), ["other.flac"]);
    }

    #[test]
    fn discard_before_any_audio() {
        let dir = tempfile::tempdir().unwrap();
        let rec = Recording::start(dir.path(), at(0), "").unwrap();
        rec.discard().unwrap();
        assert_eq!(names(dir.path()), [] as [String; 0]);
    }

    #[test]
    fn retitled_recording_gets_suffix_for_taken_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Ретро_2026-10-21_14-00.flac"), b"old").unwrap();
        let mut rec = Recording::start(dir.path(), at(0), "").unwrap();
        rec.write_block(&block(0)).unwrap();
        let saved = rec.stop(at(5), "Ретро", false).unwrap();
        assert_eq!(
            saved.files,
            vec![dir.path().join("Ретро_2026-10-21_14-00-2.flac")]
        );
        // The other recording is untouched.
        assert_eq!(
            fs::read(dir.path().join("Ретро_2026-10-21_14-00.flac")).unwrap(),
            b"old"
        );
    }

    #[test]
    fn flac_crash_continues_in_next_part() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = Recording::start(dir.path(), at(0), "").unwrap();
        for i in 0..3 {
            rec.write_block(&block(i)).unwrap();
        }
        rec.kill_encoder();

        let part1 = dir.path().join("2026-10-21_14-00-part1.flac");
        match rec.write_block(&block(3)).unwrap() {
            WriteOutcome::Restarted { saved, cause } => {
                assert_eq!(saved, part1);
                assert!(cause.contains("flac"), "{cause}");
            }
            other => panic!("expected restart, got {other:?}"),
        }
        for i in 4..6 {
            assert_eq!(rec.write_block(&block(i)).unwrap(), WriteOutcome::Written);
        }
        // The block that hit the failure is not lost.
        assert_eq!(rec.duration(), Duration::from_millis(600));

        let saved = rec.stop(at(10), "", false).unwrap();
        let part2 = dir.path().join("2026-10-21_14-00-part2.flac");
        assert_eq!(saved.files, vec![part1, part2.clone()]);
        assert_eq!(decode(&part2), blocks(3..6));
        let meta = read_json(&saved.metadata);
        assert_eq!(
            meta["files"],
            serde_json::json!(["2026-10-21_14-00-part1.flac", "2026-10-21_14-00-part2.flac"])
        );
        assert_eq!(
            find_unfinished(dir.path()).unwrap(),
            [] as [std::path::PathBuf; 0]
        );
    }

    #[test]
    fn failed_stop_keeps_part_for_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = Recording::start(dir.path(), at(0), "").unwrap();
        rec.write_block(&block(0)).unwrap();
        rec.kill_encoder();

        let err = rec.stop(at(1), "", false).unwrap_err();
        assert!(matches!(err, WriterError::Encoder { .. }), "{err}");
        let part = dir.path().join("2026-10-21_14-00.flac.part");
        assert_eq!(find_unfinished(dir.path()).unwrap(), vec![part]);
        assert!(!dir.path().join("2026-10-21_14-00.flac").exists());
        let meta = read_json(&dir.path().join("2026-10-21_14-00.json"));
        assert_eq!(
            meta["files"],
            serde_json::json!(["2026-10-21_14-00.flac.part"])
        );
    }

    #[test]
    fn dropped_recording_leaves_decodable_part() {
        let dir = tempfile::tempdir().unwrap();
        let mut rec = Recording::start(dir.path(), at(0), "").unwrap();
        for i in 0..5 {
            rec.write_block(&block(i)).unwrap();
        }
        drop(rec);
        let part = dir.path().join("2026-10-21_14-00.flac.part");
        assert_eq!(decode(&part), blocks(0..5));
    }

    /// Writes a complete FLAC file and cuts it, as if the process died mid-write.
    fn truncated_part(dir: &Path, name: &str) -> PathBuf {
        let full = dir.join("full.flac");
        let mut encoder = FlacEncoder::spawn(full.clone()).unwrap();
        encoder.send(to_le_bytes(&blocks(0..50))).unwrap();
        encoder.finish().unwrap();
        let bytes = fs::read(&full).unwrap();
        let part = dir.join(name);
        fs::write(&part, &bytes[..bytes.len() * 3 / 5]).unwrap();
        fs::remove_file(full).unwrap();
        part
    }

    #[test]
    fn recovers_truncated_part() {
        let dir = tempfile::tempdir().unwrap();
        let part = truncated_part(dir.path(), "2026-10-21_14-00.flac.part");
        assert_eq!(find_unfinished(dir.path()).unwrap(), vec![part.clone()]);

        let recovered = recover(&part).unwrap();
        assert_eq!(recovered, dir.path().join("2026-10-21_14-00.flac"));
        assert_eq!(names(dir.path()), ["2026-10-21_14-00.flac"]);
        let samples = decode(&recovered);
        let original = blocks(0..50);
        assert!(samples.len() > original.len() / 3, "{}", samples.len());
        assert_eq!(samples[..], original[..samples.len()]);
    }

    #[test]
    fn recovery_never_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let part = truncated_part(dir.path(), "x.flac.part");
        fs::write(dir.path().join("x.flac"), b"keep").unwrap();
        assert!(recover(&part).is_err());
        assert!(part.exists());
        assert_eq!(fs::read(dir.path().join("x.flac")).unwrap(), b"keep");
    }

    #[test]
    fn unreadable_part_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("x.flac.part");
        fs::write(&part, b"not a flac file").unwrap();
        assert!(recover(&part).is_err());
        assert_eq!(names(dir.path()), ["x.flac.part"]);
    }

    #[test]
    fn discard_removes_part() {
        let dir = tempfile::tempdir().unwrap();
        let part = dir.path().join("x.flac.part");
        fs::write(&part, b"").unwrap();
        discard(&part).unwrap();
        assert_eq!(names(dir.path()), [] as [std::string::String; 0]);
    }

    #[test]
    fn missing_dir_has_nothing_unfinished() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            find_unfinished(&dir.path().join("Calls")).unwrap(),
            [] as [std::path::PathBuf; 0]
        );
    }

    #[test]
    fn free_space_of_missing_dir_uses_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_space(&dir.path().join("Calls/nested")).unwrap() > 0);
    }

    #[test]
    fn rename_never_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();
        assert_eq!(
            rename_new(&a, &b).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&b).unwrap(), b"b");
    }

    #[test]
    fn time_conversions() {
        assert_eq!(frames_to_duration(16_000), Duration::from_secs(1));
        assert_eq!(frames_to_duration(1_600), Duration::from_millis(100));
        assert!((seconds(Duration::from_millis(1_383_480)) - 1383.5).abs() < f64::EPSILON);
        assert_eq!(elapsed(at(10), at(5)), Duration::ZERO);
    }
}
