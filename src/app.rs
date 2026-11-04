//! Application state machine `Idle → Recording ⇄ Paused → Saving → Idle`,
//! commands, the event log and channel highlighting (spec 3, 5.6, 6.2, 6.3).
//!
//! [`App`] holds no threads or I/O of its own: recordings go through the
//! [`Storage`] and [`Recorder`] traits, so transitions are unit-tested with
//! fakes. [`run`] is the recorder thread that drives it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use crossbeam_channel::{Receiver, Sender, select};

use crate::mixer::{Block, Channel, ChannelTick, Mixer, SAMPLE_RATE, TICK, TickClock};
use crate::pw::{Device, PcmChunk, PwEvent};
use crate::writer::{self, Recording, Saved, WriteOutcome, WriterError};

/// Number of events kept in the log; the UI shows as many as fit (spec 3).
pub const LOG_CAPACITY: usize = 100;
/// A channel quieter than this while recording counts as silent (spec 6.3).
pub const SILENCE_THRESHOLD_DB: f64 = -60.0;
/// How long a channel must stay silent before it is highlighted (spec 6.3).
pub const SILENCE_DURATION: Duration = Duration::from_secs(60);
/// Lost audio of a channel is logged at most this often; gaps in between
/// are summed into one entry.
pub const DROPOUT_LOG_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording,
    Paused,
    Saving,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Space while idle: start recording.
    Start,
    /// Space while recording or paused: pause or resume.
    TogglePause,
    /// Pause before a dialog about the recording; nothing if already paused.
    Pause,
    /// "Save" in the save dialog: stop the recording and save it.
    Save,
    /// "Delete" in the delete dialog: stop the recording and delete it.
    Discard,
    /// `q` (after confirmation), Ctrl+C, SIGTERM: save if recording, exit.
    /// A recording that was not saved by the user is marked as interrupted
    /// (spec 5.7). The reason goes to the event log.
    Quit(QuitReason),
    /// The meeting title was edited. It names the files when the recording
    /// is saved, so it can be changed at any time (spec 3, 5.7).
    SetTitle(String),
}

/// Why the app exits; shown in the event log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitReason {
    /// `q` or the "Exit" button (confirmed if recording).
    User,
    /// Ctrl+C in the terminal.
    CtrlC,
    /// A signal, by name (`SIGTERM`, `SIGINT`, `SIGHUP`).
    Signal(&'static str),
    /// The terminal cannot be drawn or read (window closed).
    TerminalLost,
    /// The UI thread stopped sending commands.
    UiGone,
}

impl std::fmt::Display for QuitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::User => f.write_str("по команде пользователя"),
            Self::CtrlC => f.write_str("по Ctrl+C"),
            Self::Signal(name) => write!(f, "по сигналу {name}"),
            Self::TerminalLost => f.write_str("терминал недоступен"),
            Self::UiGone => f.write_str("интерфейс завершился"),
        }
    }
}

/// Device a capture stream is linked to, as far as the app knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceStatus {
    /// Nothing reported yet (start-up).
    Unknown,
    /// Not linked to any device: the channel records silence (spec 6.2).
    Missing,
    Linked(Device),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChannelView {
    pub device: DeviceStatus,
    /// Level of the last block, dBFS.
    pub level_db: f64,
    /// Below `SILENCE_THRESHOLD_DB` for `SILENCE_DURATION` while recording.
    pub silent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub at: DateTime<Local>,
    pub text: String,
    pub severity: Severity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

/// Everything the UI shows.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub state: State,
    /// Recorded audio without pauses.
    pub duration: Duration,
    /// Final path of the current or last recording.
    pub path: Option<PathBuf>,
    pub mic: ChannelView,
    pub them: ChannelView,
    /// Oldest first.
    pub log: Vec<LogEntry>,
}

/// Where recordings go. Implemented by [`DiskStorage`] and by test fakes.
pub trait Storage {
    type Recorder: Recorder;
    fn has_enough_space(&self) -> Result<bool, WriterError>;
    fn start(&self, at: DateTime<Local>, title: &str) -> Result<Self::Recorder, WriterError>;
}

/// A recording in progress. Implemented by [`Recording`] and by test fakes.
pub trait Recorder {
    /// Where the recording is saved if it is stopped with `title`.
    fn path_for(&self, title: &str) -> PathBuf;
    fn duration(&self) -> Duration;
    fn write_block(&mut self, stereo: &[i16]) -> Result<WriteOutcome, WriterError>;
    fn pause(&mut self, at: DateTime<Local>);
    fn resume(&mut self, at: DateTime<Local>);
    fn device_change(&mut self, channel: Channel, device: &str, description: &str);
    /// `interrupted`: the app closed without the recording being stopped.
    fn stop(
        self,
        at: DateTime<Local>,
        title: &str,
        interrupted: bool,
    ) -> Result<Saved, WriterError>;
    /// Stops and deletes all files of the recording.
    fn discard(self) -> Result<(), WriterError>;
}

/// Recordings in a directory on disk (`~/Calls`).
#[derive(Debug)]
pub struct DiskStorage {
    dir: PathBuf,
}

impl DiskStorage {
    #[must_use]
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }
}

impl Storage for DiskStorage {
    type Recorder = Recording;

    fn has_enough_space(&self) -> Result<bool, WriterError> {
        writer::has_enough_space(&self.dir)
    }

    fn start(&self, at: DateTime<Local>, title: &str) -> Result<Recording, WriterError> {
        Recording::start(&self.dir, at, title)
    }
}

impl Recorder for Recording {
    fn path_for(&self, title: &str) -> PathBuf {
        Recording::path_for(self, title)
    }

    fn duration(&self) -> Duration {
        Recording::duration(self)
    }

    fn write_block(&mut self, stereo: &[i16]) -> Result<WriteOutcome, WriterError> {
        Recording::write_block(self, stereo)
    }

    fn pause(&mut self, at: DateTime<Local>) {
        Recording::pause(self, at);
    }

    fn resume(&mut self, at: DateTime<Local>) {
        Recording::resume(self, at);
    }

    fn device_change(&mut self, channel: Channel, device: &str, description: &str) {
        Recording::device_change(self, channel, device, description);
    }

    fn stop(
        self,
        at: DateTime<Local>,
        title: &str,
        interrupted: bool,
    ) -> Result<Saved, WriterError> {
        Recording::stop(self, at, title, interrupted)
    }

    fn discard(self) -> Result<(), WriterError> {
        Recording::discard(self)
    }
}

#[derive(Debug)]
struct ChannelState {
    device: DeviceStatus,
    level_db: f64,
    /// Consecutive recorded blocks below `SILENCE_THRESHOLD_DB`.
    silent_blocks: u32,
    dropouts: Dropouts,
}

impl ChannelState {
    fn new() -> Self {
        Self {
            device: DeviceStatus::Unknown,
            level_db: crate::mixer::LEVEL_FLOOR_DB,
            silent_blocks: 0,
            dropouts: Dropouts::new(),
        }
    }

    fn view(&self, recording: bool) -> ChannelView {
        ChannelView {
            device: self.device.clone(),
            level_db: self.level_db,
            silent: recording && self.silent_blocks >= silence_blocks(),
        }
    }
}

/// Audio a channel lost while recording: padding where the queue ran dry and
/// overflow drops. Gaps are summed so that a burst of them gives one log
/// entry. Samples the PipeWire thread could not hand over are kept apart:
/// they mostly reappear as padding, so they name the cause rather than add
/// to the total.
#[derive(Debug)]
struct Dropouts {
    /// Samples lost since the last report.
    samples: usize,
    /// Separate gaps since the last report.
    gaps: u32,
    /// The last recorded block had a gap, which may go on.
    in_gap: bool,
    /// Samples the PipeWire thread dropped on a full PCM queue since the
    /// last report.
    queue_dropped: usize,
    /// Recorded blocks since the last report.
    blocks_since_report: u32,
}

impl Dropouts {
    fn new() -> Self {
        Self {
            samples: 0,
            gaps: 0,
            in_gap: false,
            queue_dropped: 0,
            // The first gap is reported without waiting.
            blocks_since_report: u32::MAX,
        }
    }

    /// One recorded block with `lost` samples missing from it.
    fn block(&mut self, lost: usize) {
        self.blocks_since_report = self.blocks_since_report.saturating_add(1);
        if lost == 0 {
            self.in_gap = false;
            return;
        }
        if !self.in_gap {
            self.gaps = self.gaps.saturating_add(1);
        }
        self.in_gap = true;
        self.samples = self.samples.saturating_add(lost);
    }

    /// Samples dropped before they reached the mixer.
    fn dropped(&mut self, samples: usize) {
        self.queue_dropped = self.queue_dropped.saturating_add(samples);
    }

    /// What to log: once the gap is over and the last report is
    /// `DROPOUT_LOG_INTERVAL` old, or right now if `force`.
    fn take(&mut self, force: bool) -> Option<DropoutReport> {
        let due = !self.in_gap && self.blocks_since_report >= blocks_in(DROPOUT_LOG_INTERVAL);
        if (self.gaps == 0 && self.queue_dropped == 0) || !(due || force) {
            return None;
        }
        let report = DropoutReport {
            gaps: self.gaps,
            samples: self.samples,
            queue_dropped: self.queue_dropped,
        };
        self.gaps = 0;
        self.samples = 0;
        self.queue_dropped = 0;
        self.blocks_since_report = 0;
        Some(report)
    }
}

/// Lost audio of a channel since the last log entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DropoutReport {
    gaps: u32,
    samples: usize,
    queue_dropped: usize,
}

impl DropoutReport {
    /// Log text without the channel label.
    fn text(self) -> String {
        // Without a gap in the mixer the queue drop is all there is to say.
        let mut text = match self.gaps {
            0 => format!("пропуск звука {} мс", samples_to_ms(self.queue_dropped)),
            1 => format!("пропуск звука {} мс", samples_to_ms(self.samples)),
            gaps => format!(
                "пропуски звука ({gaps}), всего {} мс",
                samples_to_ms(self.samples)
            ),
        };
        if self.queue_dropped > 0 {
            text.push_str(" (поток записи не успевал)");
        }
        text
    }
}

/// Samples missing from a block: padding and overflow drops. Drift
/// correction is normal operation and is not counted.
fn lost_samples(tick: &ChannelTick) -> usize {
    tick.padded.saturating_add(tick.overflow)
}

/// Number of `TICK` blocks in `duration`.
fn blocks_in(duration: Duration) -> u32 {
    u32::try_from(duration.as_millis() / TICK.as_millis()).unwrap_or(u32::MAX)
}

/// Blocks in `SILENCE_DURATION`.
fn silence_blocks() -> u32 {
    blocks_in(SILENCE_DURATION)
}

/// Duration of `samples` mono samples at `SAMPLE_RATE`, milliseconds.
fn samples_to_ms(samples: usize) -> u64 {
    u64::try_from(samples)
        .unwrap_or(u64::MAX)
        .saturating_mul(1000)
        / u64::from(SAMPLE_RATE)
}

#[derive(Debug)]
pub struct App<S: Storage> {
    storage: S,
    state: State,
    recorder: Option<S::Recorder>,
    /// Meeting title for the file name; empty for none.
    title: String,
    /// The app quit without the user saving the recording.
    interrupted: bool,
    /// The recording in `Saving` is deleted instead of saved.
    discarding: bool,
    /// Path and duration of the last recording, shown after it is saved.
    last_path: Option<PathBuf>,
    last_duration: Duration,
    mic: ChannelState,
    them: ChannelState,
    log: VecDeque<LogEntry>,
    /// Last write error, so a failing `flac` does not flood the log.
    write_error: Option<String>,
}

impl<S: Storage> App<S> {
    pub fn new(storage: S) -> Self {
        Self {
            storage,
            state: State::Idle,
            recorder: None,
            title: String::new(),
            interrupted: false,
            discarding: false,
            last_path: None,
            last_duration: Duration::ZERO,
            mic: ChannelState::new(),
            them: ChannelState::new(),
            log: VecDeque::with_capacity(LOG_CAPACITY + 1),
            write_error: None,
        }
    }

    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// Whether a command does anything in the current state (spec 3).
    #[must_use]
    pub fn allows(&self, command: &Command) -> bool {
        allows(self.state, command)
    }

    /// Handles a command. `Save`, `Discard` and `Quit` with a recording leave
    /// the app in `Saving`; call [`App::finish_saving`] after the UI has
    /// shown it.
    pub fn command(&mut self, command: Command, now: DateTime<Local>) {
        if !self.allows(&command) {
            return;
        }
        match command {
            Command::Start => self.start(now),
            Command::TogglePause => self.toggle_pause(now),
            Command::Pause => {
                if self.state == State::Recording {
                    self.toggle_pause(now);
                }
            }
            Command::Save => self.state = State::Saving,
            Command::Discard => {
                self.discarding = true;
                self.state = State::Saving;
            }
            Command::Quit(reason) => {
                if self.recorder.is_some() {
                    // Not saved by the user: keep it, marked (spec 5.7).
                    self.interrupted = true;
                    self.state = State::Saving;
                    self.log(
                        now,
                        Severity::Warning,
                        format!("Выход ({reason}): запись сохраняется с пометкой «прервано»"),
                    );
                } else {
                    self.log(now, Severity::Info, format!("Выход ({reason})"));
                }
            }
            Command::SetTitle(title) => self.title = title,
        }
    }

    fn start(&mut self, now: DateTime<Local>) {
        match self.storage.has_enough_space() {
            Ok(true) => {}
            Ok(false) => self.log(
                now,
                Severity::Warning,
                format!(
                    "Мало места: свободно меньше {} ГБ",
                    writer::MIN_FREE_BYTES / 1_000_000_000
                ),
            ),
            Err(e) => self.log(now, Severity::Warning, format!("Свободное место: {e}")),
        }
        match self.storage.start(now, &self.title) {
            Ok(mut recorder) => {
                for (channel, state) in [(Channel::Mic, &self.mic), (Channel::Them, &self.them)] {
                    if let DeviceStatus::Linked(device) = &state.device {
                        recorder.device_change(channel, &device.name, &device.description);
                    }
                }
                self.last_path = None;
                self.last_duration = Duration::ZERO;
                let path = display_path(&recorder.path_for(&self.title));
                self.recorder = Some(recorder);
                self.state = State::Recording;
                self.write_error = None;
                self.mic.silent_blocks = 0;
                self.them.silent_blocks = 0;
                self.mic.dropouts = Dropouts::new();
                self.them.dropouts = Dropouts::new();
                self.log(now, Severity::Info, format!("Запись начата: {path}"));
                for channel in [Channel::Mic, Channel::Them] {
                    if self.channel(channel).device == DeviceStatus::Missing {
                        let label = channel_label(channel);
                        self.log(
                            now,
                            Severity::Error,
                            format!("{label}: нет устройства, пишется тишина"),
                        );
                    }
                }
            }
            Err(e) => self.log(now, Severity::Error, format!("Запись не началась: {e}")),
        }
    }

    fn toggle_pause(&mut self, now: DateTime<Local>) {
        let Some(recorder) = self.recorder.as_mut() else {
            return;
        };
        match self.state {
            State::Recording => {
                recorder.pause(now);
                self.state = State::Paused;
                self.log(now, Severity::Info, "Пауза".into());
            }
            State::Paused => {
                recorder.resume(now);
                self.state = State::Recording;
                self.log(now, Severity::Info, "Запись продолжена".into());
            }
            State::Idle | State::Saving => {}
        }
    }

    /// Stops `flac`, renames the files and writes the metadata, then goes
    /// back to `Idle`. Does nothing outside `Saving`.
    pub fn finish_saving(&mut self, now: DateTime<Local>) {
        if self.state != State::Saving {
            return;
        }
        self.state = State::Idle;

        let interrupted = std::mem::take(&mut self.interrupted);
        let discarding = std::mem::take(&mut self.discarding);
        let Some(recorder) = self.recorder.take() else {
            return;
        };
        // Gaps still waiting for their log entry belong to this recording.
        self.report_dropouts(now, true);
        if discarding {
            self.last_path = None;
            self.last_duration = Duration::ZERO;
            match recorder.discard() {
                Ok(()) => self.log(now, Severity::Warning, "Запись удалена".into()),
                Err(e) => self.log(now, Severity::Error, format!("Запись не удалена: {e}")),
            }
            return;
        }
        self.last_duration = recorder.duration();
        self.last_path = Some(recorder.path_for(&self.title));
        match recorder.stop(now, &self.title, interrupted) {
            Ok(saved) => {
                if let Some(first) = saved.files.first() {
                    self.last_path = Some(first.clone());
                }
                let files = saved
                    .files
                    .iter()
                    .map(|f| display_path(f))
                    .collect::<Vec<_>>()
                    .join(", ");
                let duration = format_duration(self.last_duration);
                self.log(
                    now,
                    Severity::Info,
                    format!("Сохранено ({duration}): {files}"),
                );
            }
            Err(e) => self.log(now, Severity::Error, format!("Ошибка сохранения: {e}")),
        }
    }

    /// Takes one mixer block: levels always, audio only while recording.
    pub fn block(&mut self, block: &Block, now: DateTime<Local>) {
        let recording = self.state == State::Recording;
        for (channel, tick) in [(Channel::Mic, &block.mic), (Channel::Them, &block.them)] {
            let state = self.channel_mut(channel);
            state.level_db = tick.level_db;
            if !recording {
                continue;
            }
            let was_silent = state.silent_blocks >= silence_blocks();
            if tick.level_db < SILENCE_THRESHOLD_DB {
                state.silent_blocks = state.silent_blocks.saturating_add(1);
            } else {
                state.silent_blocks = 0;
            }
            let silent = state.silent_blocks >= silence_blocks();
            let label = channel_label(channel);
            if silent && !was_silent {
                let secs = SILENCE_DURATION.as_secs();
                self.log(
                    now,
                    Severity::Warning,
                    format!("{label}: тишина дольше {secs} с"),
                );
            } else if was_silent && !silent {
                self.log(now, Severity::Info, format!("{label}: звук появился"));
            }
            // A channel without a device records silence; that is logged as
            // "нет устройства", not as lost audio.
            let state = self.channel_mut(channel);
            let lost = if matches!(state.device, DeviceStatus::Linked(_)) {
                lost_samples(tick)
            } else {
                0
            };
            state.dropouts.block(lost);
        }
        if !recording {
            return;
        }
        self.report_dropouts(now, false);
        let Some(recorder) = self.recorder.as_mut() else {
            return;
        };
        let outcome = recorder.write_block(&block.stereo);
        if outcome.is_ok() && self.write_error.take().is_some() {
            self.log(now, Severity::Info, "Запись снова идёт".into());
        }
        match outcome {
            Ok(WriteOutcome::Written | WriteOutcome::Skipped) => {}
            Ok(WriteOutcome::Restarted { saved, cause }) => {
                self.log(
                    now,
                    Severity::Warning,
                    format!(
                        "flac перезапущен ({cause}), сохранено: {}",
                        display_path(&saved)
                    ),
                );
            }
            Err(e) => {
                let text = format!("Ошибка записи: {e}");
                if self.write_error.as_ref() != Some(&text) {
                    self.write_error = Some(text.clone());
                    self.log(now, Severity::Error, text);
                }
            }
        }
    }

    pub fn pw_event(&mut self, event: PwEvent, now: DateTime<Local>) {
        match event {
            PwEvent::StreamDevice { channel, device } => self.stream_device(channel, device, now),
            PwEvent::ManualReconnect { channel } => self.log(
                now,
                Severity::Warning,
                format!("{}: переподключено вручную", channel_label(channel)),
            ),
            PwEvent::Connected => self.log(now, Severity::Info, "PipeWire: подключено".into()),
            PwEvent::Disconnected { reason } => self.log(
                now,
                Severity::Error,
                format!("PipeWire: нет соединения ({reason})"),
            ),
            PwEvent::Error(e) => self.log(now, Severity::Error, format!("PipeWire: {e}")),
            PwEvent::Fatal(e) => {
                self.log(now, Severity::Error, format!("PipeWire недоступен: {e}"));
            }
            // The stream follows the default; its actual device is what is
            // shown and logged (StreamDevice).
            PwEvent::DefaultChanged { .. } => {}
            PwEvent::PcmDropped { channel, samples } => {
                if self.state == State::Recording {
                    self.channel_mut(channel).dropouts.dropped(samples);
                }
            }
        }
    }

    /// Logs lost audio of both channels that is due (see [`Dropouts::take`]).
    fn report_dropouts(&mut self, now: DateTime<Local>, force: bool) {
        for channel in [Channel::Mic, Channel::Them] {
            let Some(report) = self.channel_mut(channel).dropouts.take(force) else {
                continue;
            };
            let label = channel_label(channel);
            self.log(
                now,
                Severity::Warning,
                format!("{label}: {}", report.text()),
            );
        }
    }

    fn channel(&self, channel: Channel) -> &ChannelState {
        match channel {
            Channel::Mic => &self.mic,
            Channel::Them => &self.them,
        }
    }

    fn channel_mut(&mut self, channel: Channel) -> &mut ChannelState {
        match channel {
            Channel::Mic => &mut self.mic,
            Channel::Them => &mut self.them,
        }
    }

    fn stream_device(&mut self, channel: Channel, device: Option<Device>, now: DateTime<Local>) {
        let state = self.channel_mut(channel);
        let new = device.map_or(DeviceStatus::Missing, DeviceStatus::Linked);
        if state.device == new {
            return;
        }
        let old = std::mem::replace(&mut state.device, new.clone());
        let label = channel_label(channel);
        let (severity, text) = match (&old, &new) {
            (DeviceStatus::Linked(a), DeviceStatus::Linked(b)) => (
                Severity::Info,
                format!("{label}: {} → {}", a.description, b.description),
            ),
            (_, DeviceStatus::Linked(b)) if old == DeviceStatus::Missing => {
                (Severity::Info, format!("{label}: {}", b.description))
            }
            // The first report at start-up is not an event.
            (DeviceStatus::Unknown, _) => (Severity::Info, String::new()),
            (_, _) => (Severity::Error, format!("{label}: нет устройства")),
        };
        if !text.is_empty() {
            self.log(now, severity, text);
        }
        if let (DeviceStatus::Linked(device), Some(recorder)) = (&new, self.recorder.as_mut()) {
            recorder.device_change(channel, &device.name, &device.description);
        }
    }

    fn log(&mut self, at: DateTime<Local>, severity: Severity, text: String) {
        self.log.push_back(LogEntry { at, text, severity });
        while self.log.len() > LOG_CAPACITY {
            self.log.pop_front();
        }
    }

    /// Adds a message from outside the state machine (start-up, recovery).
    pub fn notify(&mut self, at: DateTime<Local>, severity: Severity, text: String) {
        self.log(at, severity, text);
    }

    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        let recording = self.state == State::Recording;
        Snapshot {
            state: self.state,
            duration: self
                .recorder
                .as_ref()
                .map_or(self.last_duration, Recorder::duration),
            path: self
                .recorder
                .as_ref()
                .map(|recorder| recorder.path_for(&self.title))
                .or_else(|| self.last_path.clone()),
            mic: self.mic.view(recording),
            them: self.them.view(recording),
            log: self.log.iter().cloned().collect(),
        }
    }
}

/// Whether a command does anything in `state` (spec 3). The UI dims the
/// buttons for which this is false.
#[must_use]
pub fn allows(state: State, command: &Command) -> bool {
    match command {
        Command::Start => state == State::Idle,
        Command::TogglePause | Command::Pause | Command::Save | Command::Discard => {
            matches!(state, State::Recording | State::Paused)
        }
        Command::Quit(_) | Command::SetTitle(_) => true,
    }
}

#[must_use]
pub fn channel_label(channel: Channel) -> &'static str {
    match channel {
        Channel::Mic => "Микрофон",
        Channel::Them => "Собеседники",
    }
}

/// `HH:MM:SS`.
#[must_use]
pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// Path with `$HOME` shown as `~`.
#[must_use]
pub fn display_path(path: &Path) -> String {
    std::env::home_dir()
        .and_then(|home| {
            path.strip_prefix(home)
                .ok()
                .map(|p| format!("~/{}", p.display()))
        })
        .unwrap_or_else(|| path.display().to_string())
}

/// Recorder thread (spec 5.1): mixes PCM on the 100 ms clock, feeds the app
/// and publishes a snapshot after every change. Returns after `Quit`, once
/// the recording is saved, or when the command channel closes.
pub fn run<S: Storage>(
    app: &mut App<S>,
    pcm: Receiver<PcmChunk>,
    events: Receiver<PwEvent>,
    commands: &Receiver<Command>,
    snapshots: &Sender<Snapshot>,
) {
    let (mut pcm, mut events) = (pcm, events);
    let mut mixer = Mixer::new();
    let mut clock = TickClock::new(Instant::now());
    let publish = |app: &App<S>| {
        // The UI is gone only during shutdown.
        let _ = snapshots.send(app.snapshot());
    };
    publish(app);

    loop {
        let deadline = clock
            .next_deadline()
            .unwrap_or_else(|| Instant::now() + TICK);
        let mut changed = false;
        select! {
            recv(pcm) -> chunk => match chunk {
                Ok(chunk) => mixer.push(chunk.channel, chunk.samples()),
                // PipeWire thread gone: the mixer keeps padding with silence.
                Err(_) => pcm = crossbeam_channel::never(),
            },
            recv(events) -> event => match event {
                Ok(event) => {
                    app.pw_event(event, Local::now());
                    changed = true;
                }
                Err(_) => events = crossbeam_channel::never(),
            },
            recv(commands) -> command => {
                let command = command.unwrap_or(Command::Quit(QuitReason::UiGone));
                let quit = matches!(command, Command::Quit(_));
                app.command(command, Local::now());
                if app.state() == State::Saving {
                    publish(app);
                    app.finish_saving(Local::now());
                }
                if quit {
                    publish(app);
                    return;
                }
                changed = true;
            },
            recv(crossbeam_channel::at(deadline)) -> _ => {},
        }
        let due = clock.take_due(Instant::now());
        if due > 0 {
            run_ticks(app, &mut mixer, &pcm, due);
            changed = true;
        }
        if changed {
            publish(app);
        }
    }
}

/// Runs `due` mixer ticks. Before each tick, chunks already waiting in `pcm`
/// are pushed until a queue holds a block plus reserve. After this thread
/// stalls (e.g. on a slow write), the backlog is thus replayed tick by tick:
/// pushing all of it first would overflow the queues, and ticking first would
/// pad blocks with silence while the audio for them is still in `pcm`.
fn run_ticks<S: Storage>(app: &mut App<S>, mixer: &mut Mixer, pcm: &Receiver<PcmChunk>, due: u64) {
    for _ in 0..due {
        while mixer.wants_samples() {
            // Empty or disconnected: tick with what there is.
            let Ok(chunk) = pcm.try_recv() else {
                break;
            };
            mixer.push(chunk.channel, chunk.samples());
        }
        app.block(&mixer.tick(), Local::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mixer::{BLOCK_SAMPLES, ChannelTick};
    use chrono::TimeZone;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, Clone, PartialEq)]
    enum Call {
        Start(String),
        Write,
        Pause,
        Resume,
        Device(Channel, String),
        Stop(String, bool),
        Discard,
    }

    #[derive(Debug, Default)]
    #[allow(clippy::struct_excessive_bools)]
    struct Fake {
        calls: Vec<Call>,
        space: Option<bool>,
        fail_start: bool,
        fail_write: bool,
        restart_on_write: bool,
        fail_stop: bool,
        blocks: u32,
    }

    #[derive(Debug, Clone, Default)]
    struct FakeStorage(Rc<RefCell<Fake>>);

    #[derive(Debug)]
    struct FakeRecorder(Rc<RefCell<Fake>>);

    fn io_error() -> WriterError {
        WriterError::Spawn(std::io::Error::other("тест"))
    }

    impl Storage for FakeStorage {
        type Recorder = FakeRecorder;

        fn has_enough_space(&self) -> Result<bool, WriterError> {
            self.0.borrow().space.ok_or_else(io_error)
        }

        fn start(&self, _at: DateTime<Local>, title: &str) -> Result<FakeRecorder, WriterError> {
            let mut fake = self.0.borrow_mut();
            fake.calls.push(Call::Start(title.to_owned()));
            if fake.fail_start {
                return Err(io_error());
            }
            Ok(FakeRecorder(Rc::clone(&self.0)))
        }
    }

    impl Recorder for FakeRecorder {
        fn path_for(&self, title: &str) -> PathBuf {
            fake_path(title, "flac")
        }

        fn duration(&self) -> Duration {
            TICK * self.0.borrow().blocks
        }

        fn write_block(&mut self, _stereo: &[i16]) -> Result<WriteOutcome, WriterError> {
            let mut fake = self.0.borrow_mut();
            fake.calls.push(Call::Write);
            if fake.fail_write {
                return Err(io_error());
            }
            fake.blocks += 1;
            if fake.restart_on_write {
                fake.restart_on_write = false;
                return Ok(WriteOutcome::Restarted {
                    saved: PathBuf::from("/calls/x-part1.flac"),
                    cause: "сбой".into(),
                });
            }
            Ok(WriteOutcome::Written)
        }

        fn pause(&mut self, _at: DateTime<Local>) {
            self.0.borrow_mut().calls.push(Call::Pause);
        }

        fn resume(&mut self, _at: DateTime<Local>) {
            self.0.borrow_mut().calls.push(Call::Resume);
        }

        fn device_change(&mut self, channel: Channel, device: &str, _description: &str) {
            self.0
                .borrow_mut()
                .calls
                .push(Call::Device(channel, device.to_owned()));
        }

        fn stop(
            self,
            _at: DateTime<Local>,
            title: &str,
            interrupted: bool,
        ) -> Result<Saved, WriterError> {
            let mut fake = self.0.borrow_mut();
            fake.calls.push(Call::Stop(title.to_owned(), interrupted));
            if fake.fail_stop {
                return Err(io_error());
            }
            Ok(Saved {
                files: vec![fake_path(title, "flac")],
                metadata: fake_path(title, "json"),
            })
        }

        fn discard(self) -> Result<(), WriterError> {
            self.0.borrow_mut().calls.push(Call::Discard);
            Ok(())
        }
    }

    /// File names as the real writer makes them.
    fn fake_path(title: &str, extension: &str) -> PathBuf {
        let base = writer::file_base(title, at(0));
        PathBuf::from(format!("/calls/{base}.{extension}"))
    }

    fn at(secs: i64) -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 10, 21, 14, 0, 0).unwrap() + chrono::TimeDelta::seconds(secs)
    }

    fn app() -> (App<FakeStorage>, Rc<RefCell<Fake>>) {
        let storage = FakeStorage::default();
        storage.0.borrow_mut().space = Some(true);
        let fake = Rc::clone(&storage.0);
        (App::new(storage), fake)
    }

    fn block(mic_db: f64, them_db: f64) -> Block {
        Block {
            stereo: vec![0; BLOCK_SAMPLES * 2],
            mic: ChannelTick {
                level_db: mic_db,
                ..ChannelTick::default()
            },
            them: ChannelTick {
                level_db: them_db,
                ..ChannelTick::default()
            },
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn device(name: &str, description: &str) -> Option<Device> {
        Some(Device {
            name: name.to_owned(),
            description: description.to_owned(),
        })
    }

    fn texts(app: &App<FakeStorage>) -> Vec<String> {
        app.snapshot().log.into_iter().map(|e| e.text).collect()
    }

    fn calls(fake: &Rc<RefCell<Fake>>) -> Vec<Call> {
        std::mem::take(&mut fake.borrow_mut().calls)
    }

    #[test]
    fn full_cycle() {
        let (mut app, fake) = app();
        assert_eq!(app.state(), State::Idle);
        assert!(app.allows(&Command::Start));
        assert!(!app.allows(&Command::TogglePause));
        assert!(!app.allows(&Command::Save));

        app.command(Command::Start, at(0));
        assert_eq!(app.state(), State::Recording);
        assert!(!app.allows(&Command::Start));
        app.block(&block(-20.0, -30.0), at(0));

        app.command(Command::TogglePause, at(1));
        assert_eq!(app.state(), State::Paused);
        app.block(&block(-20.0, -30.0), at(1));
        app.command(Command::TogglePause, at(2));
        assert_eq!(app.state(), State::Recording);
        app.block(&block(-20.0, -30.0), at(2));

        // Saved straight from recording.
        app.command(Command::Save, at(3));
        assert_eq!(app.state(), State::Saving);
        app.finish_saving(at(3));
        assert_eq!(app.state(), State::Idle);

        assert_eq!(
            calls(&fake),
            vec![
                Call::Start(String::new()),
                Call::Write,
                Call::Pause,
                Call::Resume,
                Call::Write,
                Call::Stop(String::new(), false)
            ]
        );
        let snapshot = app.snapshot();
        assert_eq!(snapshot.duration, TICK * 2);
        assert_eq!(
            snapshot.path,
            Some(PathBuf::from("/calls/2026-10-21_14-00.flac"))
        );
        assert_eq!(
            texts(&app),
            [
                "Запись начата: /calls/2026-10-21_14-00.flac",
                "Пауза",
                "Запись продолжена",
                "Сохранено (00:00:00): /calls/2026-10-21_14-00.flac",
            ]
        );
    }

    #[test]
    fn discard_deletes_instead_of_saving() {
        let (mut app, fake) = app();
        assert!(!app.allows(&Command::Discard));
        app.command(Command::Start, at(0));
        app.block(&block(-20.0, -30.0), at(1));
        app.command(Command::Discard, at(2));
        assert_eq!(app.state(), State::Saving);
        app.finish_saving(at(2));

        assert_eq!(app.state(), State::Idle);
        assert_eq!(
            calls(&fake),
            vec![Call::Start(String::new()), Call::Write, Call::Discard]
        );
        let snapshot = app.snapshot();
        assert_eq!(snapshot.path, None);
        assert_eq!(snapshot.duration, Duration::ZERO);
        assert_eq!(
            texts(&app),
            [
                "Запись начата: /calls/2026-10-21_14-00.flac",
                "Запись удалена"
            ]
        );

        // The next stop saves again.
        app.command(Command::Start, at(3));
        app.command(Command::Save, at(4));
        app.finish_saving(at(4));
        assert_eq!(calls(&fake).last(), Some(&Call::Stop(String::new(), false)));
    }

    #[test]
    fn title_can_change_at_any_time() {
        let (mut app, fake) = app();
        assert!(app.allows(&Command::SetTitle(String::new())));
        app.command(Command::SetTitle("Планёрка".into()), at(0));
        app.command(Command::Start, at(0));
        assert_eq!(app.snapshot().path, Some(fake_path("Планёрка", "flac")));

        app.command(Command::TogglePause, at(1));
        app.command(Command::SetTitle("Ретро".into()), at(2));
        assert_eq!(app.snapshot().path, Some(fake_path("Ретро", "flac")));
        app.command(Command::Save, at(3));
        app.finish_saving(at(3));

        assert_eq!(
            calls(&fake),
            vec![
                Call::Start("Планёрка".into()),
                Call::Pause,
                Call::Stop("Ретро".into(), false)
            ]
        );
        assert_eq!(app.snapshot().path, Some(fake_path("Ретро", "flac")));
        // The next recording starts with the title as it is now.
        app.command(Command::Start, at(4));
        assert_eq!(calls(&fake), vec![Call::Start("Ретро".into())]);
    }

    #[test]
    fn commands_outside_their_states_are_ignored() {
        let (mut app, fake) = app();
        app.command(Command::TogglePause, at(0));
        app.command(Command::Save, at(0));
        app.command(Command::Discard, at(0));
        app.finish_saving(at(0));
        assert_eq!(app.state(), State::Idle);
        app.command(Command::Start, at(0));
        app.command(Command::Start, at(1));
        assert_eq!(calls(&fake), vec![Call::Start(String::new())]);
    }

    #[test]
    fn timer_excludes_pauses() {
        let (mut app, _fake) = app();
        app.command(Command::Start, at(0));
        for _ in 0..5 {
            app.block(&block(-20.0, -20.0), at(0));
        }
        app.command(Command::TogglePause, at(1));
        for _ in 0..50 {
            app.block(&block(-20.0, -20.0), at(1));
        }
        assert_eq!(app.snapshot().duration, TICK * 5);
    }

    #[test]
    fn start_failure_stays_idle() {
        let (mut app, fake) = app();
        fake.borrow_mut().fail_start = true;
        app.command(Command::Start, at(0));
        assert_eq!(app.state(), State::Idle);
        assert_eq!(
            texts(&app),
            ["Запись не началась: не удалось запустить flac: тест"]
        );
    }

    #[test]
    fn low_space_warns_and_still_records() {
        let (mut app, fake) = app();
        fake.borrow_mut().space = Some(false);
        app.command(Command::Start, at(0));
        assert_eq!(app.state(), State::Recording);
        assert_eq!(
            texts(&app),
            [
                "Мало места: свободно меньше 1 ГБ",
                "Запись начата: /calls/2026-10-21_14-00.flac",
            ]
        );
    }

    #[test]
    fn failed_save_returns_to_idle_with_error() {
        let (mut app, fake) = app();
        fake.borrow_mut().fail_stop = true;
        app.command(Command::Start, at(0));
        app.command(Command::Save, at(1));
        app.finish_saving(at(1));
        assert_eq!(app.state(), State::Idle);
        assert_eq!(
            texts(&app),
            [
                "Запись начата: /calls/2026-10-21_14-00.flac",
                "Ошибка сохранения: не удалось запустить flac: тест",
            ]
        );
        let entry = app.snapshot().log.pop().unwrap();
        assert_eq!(entry.severity, Severity::Error);
    }

    #[test]
    fn quit_while_paused_saves_as_interrupted() {
        let (mut app, fake) = app();
        app.command(Command::Start, at(0));
        app.command(Command::TogglePause, at(1));
        app.command(Command::Quit(QuitReason::User), at(2));
        assert_eq!(app.state(), State::Saving);
        app.finish_saving(at(2));
        assert_eq!(calls(&fake).last(), Some(&Call::Stop(String::new(), true)));
    }

    #[test]
    fn pause_command_only_pauses() {
        let (mut app, fake) = app();
        assert!(!app.allows(&Command::Pause));
        app.command(Command::Start, at(0));
        app.command(Command::Pause, at(1));
        assert_eq!(app.state(), State::Paused);
        app.command(Command::Pause, at(2));
        assert_eq!(app.state(), State::Paused);
        assert_eq!(calls(&fake), vec![Call::Start(String::new()), Call::Pause]);
    }

    #[test]
    fn quit_while_idle_does_nothing() {
        let (mut app, fake) = app();
        app.command(Command::Quit(QuitReason::User), at(0));
        assert_eq!(app.state(), State::Idle);
        assert_eq!(calls(&fake), vec![]);
        assert_eq!(texts(&app), ["Выход (по команде пользователя)"]);
    }

    #[test]
    fn quit_reason_is_logged() {
        let reasons = [
            (QuitReason::User, "по команде пользователя"),
            (QuitReason::CtrlC, "по Ctrl+C"),
            (QuitReason::Signal("SIGTERM"), "по сигналу SIGTERM"),
            (QuitReason::TerminalLost, "терминал недоступен"),
            (QuitReason::UiGone, "интерфейс завершился"),
        ];
        for (reason, text) in reasons {
            let (mut app, _fake) = app();
            app.command(Command::Start, at(0));
            app.command(Command::Quit(reason), at(1));
            let log = app.snapshot().log;
            let entry = log.last().unwrap();
            assert_eq!(
                entry.text,
                format!("Выход ({text}): запись сохраняется с пометкой «прервано»")
            );
            assert_eq!(entry.severity, Severity::Warning);
        }
    }

    /// App with both channels linked to a device, recording.
    fn recording_with_devices() -> (App<FakeStorage>, Rc<RefCell<Fake>>) {
        let (mut app, fake) = app();
        for channel in [Channel::Mic, Channel::Them] {
            app.pw_event(
                PwEvent::StreamDevice {
                    channel,
                    device: device("a", "A"),
                },
                at(0),
            );
        }
        app.command(Command::Start, at(0));
        app.log.clear();
        (app, fake)
    }

    /// A block whose mic channel is missing `lost` samples.
    fn mic_gap(padded: usize, overflow: usize) -> Block {
        let mut b = block(-20.0, -20.0);
        b.mic.padded = padded;
        b.mic.overflow = overflow;
        b
    }

    #[test]
    fn dropout_is_logged_when_gap_ends() {
        let (mut app, _fake) = recording_with_devices();
        app.block(&mic_gap(BLOCK_SAMPLES, 0), at(1));
        app.block(&mic_gap(800, 0), at(1));
        // The gap is still going on.
        assert_eq!(texts(&app), Vec::<String>::new());
        app.block(&block(-20.0, -20.0), at(1));
        assert_eq!(texts(&app), ["Микрофон: пропуск звука 150 мс"]);
        assert_eq!(app.snapshot().log[0].severity, Severity::Warning);
        // Drift correction alone is not a gap.
        let mut drift = block(-20.0, -20.0);
        drift.mic.dropped = 320;
        drift.mic.inserted = 320;
        for _ in 0..blocks_in(DROPOUT_LOG_INTERVAL) {
            app.block(&drift, at(2));
        }
        assert_eq!(texts(&app).len(), 1);
    }

    #[test]
    fn dropouts_are_summed_between_reports() {
        let (mut app, _fake) = recording_with_devices();
        app.block(&mic_gap(1600, 0), at(1));
        app.block(&block(-20.0, -20.0), at(1));
        // Within the interval: collected, not logged.
        app.block(&mic_gap(0, 800), at(2));
        app.block(&block(-20.0, -20.0), at(2));
        app.block(&mic_gap(400, 0), at(2));
        app.block(&block(-20.0, -20.0), at(2));
        assert_eq!(texts(&app), ["Микрофон: пропуск звука 100 мс"]);
        for _ in 0..blocks_in(DROPOUT_LOG_INTERVAL) {
            app.block(&block(-20.0, -20.0), at(3));
        }
        assert_eq!(
            texts(&app),
            [
                "Микрофон: пропуск звука 100 мс",
                "Микрофон: пропуски звука (2), всего 75 мс",
            ]
        );
    }

    #[test]
    fn pcm_queue_drops_name_the_cause_and_are_not_added() {
        let (mut app, _fake) = recording_with_devices();
        let dropped = |samples| PwEvent::PcmDropped {
            channel: Channel::Them,
            samples,
        };
        // The dropped samples show up in the mixer as padding.
        app.pw_event(dropped(3200), at(1));
        let mut gap = block(-20.0, -20.0);
        gap.them.padded = 3200;
        app.block(&gap, at(1));
        app.block(&block(-20.0, -20.0), at(1));
        // A drop the drift correction absorbed: no gap, still reported.
        app.pw_event(dropped(320), at(2));
        for _ in 0..blocks_in(DROPOUT_LOG_INTERVAL) {
            app.block(&block(-20.0, -20.0), at(3));
        }
        assert_eq!(
            texts(&app),
            [
                "Собеседники: пропуск звука 200 мс (поток записи не успевал)",
                "Собеседники: пропуск звука 20 мс (поток записи не успевал)",
            ]
        );
    }

    /// Interleaved chunks of both channels, `samples` per channel, as the
    /// PipeWire thread sends them (small buffers, both streams running).
    fn backlog(samples: usize) -> Receiver<PcmChunk> {
        const PIECE: usize = 341;
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut sent = 0;
        while sent < samples {
            let len = PIECE.min(samples - sent);
            for channel in [Channel::Mic, Channel::Them] {
                tx.send(PcmChunk::new(channel, &vec![1000; len])).unwrap();
            }
            sent += len;
        }
        rx
    }

    #[test]
    fn backlog_after_stall_is_replayed_without_gaps() {
        use crate::mixer::TARGET_LEVEL;
        let (mut app, fake) = recording_with_devices();
        let mut mixer = Mixer::new();
        // The thread stalled for 3 s while PipeWire kept sending: 30 ticks
        // are due and their audio waits in the channel, plus the usual
        // block and reserve.
        let ticks = 30;
        let pcm = backlog(TARGET_LEVEL + ticks * BLOCK_SAMPLES);
        run_ticks(&mut app, &mut mixer, &pcm, ticks as u64);

        assert_eq!(fake.borrow().blocks, 30);
        assert_eq!(texts(&app), Vec::<String>::new());
        // Real audio, not padding: 1000 / 32768 is about -30 dBFS.
        assert!(app.snapshot().mic.level_db > -31.0);
        assert!(app.snapshot().them.level_db > -31.0);
        // What is left is the usual latency (a block plus reserve), in the
        // channel or the queue: nothing was dropped or padded.
        let left: usize = pcm
            .try_iter()
            .filter(|c| c.channel == Channel::Mic)
            .map(|c| c.samples().len())
            .sum();
        assert_eq!(left + mixer.queued(Channel::Mic), TARGET_LEVEL);
    }

    #[test]
    fn ticks_without_audio_are_padded() {
        let (mut app, fake) = recording_with_devices();
        let mut mixer = Mixer::new();
        let (_tx, pcm) = crossbeam_channel::unbounded();
        run_ticks(&mut app, &mut mixer, &pcm, 3);
        assert_eq!(fake.borrow().blocks, 3);
        app.block(&block(-20.0, -20.0), at(1));
        assert_eq!(
            texts(&app),
            [
                "Микрофон: пропуск звука 300 мс",
                "Собеседники: пропуск звука 300 мс"
            ]
        );
    }

    #[test]
    fn pending_dropouts_are_logged_on_save() {
        let (mut app, _fake) = recording_with_devices();
        app.block(&mic_gap(1600, 0), at(1));
        app.command(Command::Save, at(2));
        app.finish_saving(at(2));
        let texts = texts(&app);
        assert_eq!(texts[0], "Микрофон: пропуск звука 100 мс");
        assert!(texts[1].starts_with("Сохранено"), "{texts:?}");
    }

    #[test]
    fn gaps_without_device_or_recording_are_not_dropouts() {
        let (mut app, _fake) = app();
        // Unknown device: the channel records silence by design.
        app.command(Command::Start, at(0));
        app.block(&mic_gap(BLOCK_SAMPLES, 0), at(1));
        app.block(&block(-20.0, -20.0), at(1));
        app.command(Command::TogglePause, at(2));
        app.log.clear();

        // Paused: neither mixer gaps nor PipeWire drops count.
        app.pw_event(
            PwEvent::StreamDevice {
                channel: Channel::Mic,
                device: device("a", "A"),
            },
            at(2),
        );
        app.block(&mic_gap(BLOCK_SAMPLES, 0), at(3));
        app.pw_event(
            PwEvent::PcmDropped {
                channel: Channel::Mic,
                samples: 1600,
            },
            at(3),
        );
        app.command(Command::TogglePause, at(4));
        app.block(&block(-20.0, -20.0), at(4));
        assert_eq!(texts(&app), ["Запись продолжена"]);
    }

    #[test]
    fn device_switch_is_logged_and_recorded() {
        let (mut app, fake) = app();
        let mic = |d| PwEvent::StreamDevice {
            channel: Channel::Mic,
            device: d,
        };
        app.pw_event(mic(device("alsa_input.pci", "Встроенный микрофон")), at(0));
        // The first report is the starting point, not an event.
        assert_eq!(texts(&app), Vec::<String>::new());

        app.command(Command::Start, at(1));
        app.pw_event(
            mic(device("bluez_input.70", "Jabra Evolve2 65 Mono")),
            at(2),
        );
        assert_eq!(
            texts(&app),
            [
                "Запись начата: /calls/2026-10-21_14-00.flac",
                "Микрофон: Встроенный микрофон → Jabra Evolve2 65 Mono",
            ]
        );
        assert_eq!(
            calls(&fake),
            vec![
                Call::Start(String::new()),
                Call::Device(Channel::Mic, "alsa_input.pci".into()),
                Call::Device(Channel::Mic, "bluez_input.70".into()),
            ]
        );
    }

    #[test]
    fn device_switch_while_idle_is_not_recorded() {
        let (mut app, fake) = app();
        for d in [device("a", "A"), device("b", "B")] {
            app.pw_event(
                PwEvent::StreamDevice {
                    channel: Channel::Them,
                    device: d,
                },
                at(0),
            );
        }
        assert_eq!(texts(&app), ["Собеседники: A → B"]);
        assert_eq!(calls(&fake), vec![]);
    }

    #[test]
    fn missing_device_is_highlighted_and_recovers() {
        let (mut app, _fake) = app();
        let them = |d| PwEvent::StreamDevice {
            channel: Channel::Them,
            device: d,
        };
        assert_eq!(app.snapshot().them.device, DeviceStatus::Unknown);
        app.pw_event(them(device("a", "A")), at(0));
        app.pw_event(them(None), at(1));
        assert_eq!(app.snapshot().them.device, DeviceStatus::Missing);
        app.pw_event(them(device("a", "A")), at(2));
        assert_eq!(
            app.snapshot().them.device,
            DeviceStatus::Linked(Device {
                name: "a".into(),
                description: "A".into()
            })
        );
        assert_eq!(
            texts(&app),
            ["Собеседники: нет устройства", "Собеседники: A"]
        );
        assert_eq!(app.snapshot().log[0].severity, Severity::Error);
    }

    #[test]
    fn long_silence_is_highlighted_while_recording() {
        let (mut app, _fake) = app();
        let quiet = block(-70.0, -20.0);
        // Not recording: silence does not count.
        for _ in 0..silence_blocks() {
            app.block(&quiet, at(0));
        }
        app.command(Command::Start, at(0));
        assert!(!app.snapshot().mic.silent);

        for _ in 0..silence_blocks() - 1 {
            app.block(&quiet, at(0));
        }
        assert!(!app.snapshot().mic.silent);
        app.block(&quiet, at(60));
        assert!(app.snapshot().mic.silent);
        assert!(!app.snapshot().them.silent);

        // Paused: highlighting is off, the counter is kept.
        app.command(Command::TogglePause, at(61));
        assert!(!app.snapshot().mic.silent);
        app.command(Command::TogglePause, at(62));
        assert!(app.snapshot().mic.silent);

        app.block(&block(-30.0, -20.0), at(63));
        assert!(!app.snapshot().mic.silent);

        // Entering and leaving silence is logged once each; the pause in
        // between does not repeat it.
        assert_eq!(
            texts(&app),
            [
                "Запись начата: /calls/2026-10-21_14-00.flac",
                "Микрофон: тишина дольше 60 с",
                "Пауза",
                "Запись продолжена",
                "Микрофон: звук появился",
            ]
        );
        assert_eq!(app.snapshot().log[1].severity, Severity::Warning);
    }

    #[test]
    fn start_without_device_is_logged() {
        let (mut app, _fake) = app();
        app.pw_event(
            PwEvent::StreamDevice {
                channel: Channel::Mic,
                device: device("a", "A"),
            },
            at(0),
        );
        app.pw_event(
            PwEvent::StreamDevice {
                channel: Channel::Mic,
                device: None,
            },
            at(1),
        );
        // Unknown (nothing reported yet) is not "missing".
        app.command(Command::Start, at(2));
        assert_eq!(
            texts(&app),
            [
                "Микрофон: нет устройства",
                "Запись начата: /calls/2026-10-21_14-00.flac",
                "Микрофон: нет устройства, пишется тишина",
            ]
        );
        assert_eq!(app.snapshot().log[2].severity, Severity::Error);
    }

    #[test]
    fn save_reports_duration() {
        let (mut app, _fake) = app();
        app.command(Command::Start, at(0));
        for _ in 0..25 {
            app.block(&block(-20.0, -20.0), at(1));
        }
        app.command(Command::Save, at(3));
        app.finish_saving(at(3));
        assert_eq!(
            texts(&app).last().map(String::as_str),
            Some("Сохранено (00:00:02): /calls/2026-10-21_14-00.flac")
        );
    }

    #[test]
    fn levels_update_in_every_state() {
        let (mut app, _fake) = app();
        app.block(&block(-12.5, -40.0), at(0));
        let snapshot = app.snapshot();
        assert!((snapshot.mic.level_db + 12.5).abs() < f64::EPSILON);
        assert!((snapshot.them.level_db + 40.0).abs() < f64::EPSILON);
    }

    #[test]
    fn repeated_write_errors_are_logged_once() {
        let (mut app, fake) = app();
        app.command(Command::Start, at(0));
        fake.borrow_mut().fail_write = true;
        for _ in 0..10 {
            app.block(&block(-20.0, -20.0), at(1));
        }
        let error = "Ошибка записи: не удалось запустить flac: тест";
        assert_eq!(
            texts(&app),
            ["Запись начата: /calls/2026-10-21_14-00.flac", error]
        );
        // Recovered, then failed again: both logged.
        fake.borrow_mut().fail_write = false;
        app.block(&block(-20.0, -20.0), at(2));
        app.block(&block(-20.0, -20.0), at(2));
        fake.borrow_mut().fail_write = true;
        app.block(&block(-20.0, -20.0), at(3));
        assert_eq!(
            texts(&app),
            [
                "Запись начата: /calls/2026-10-21_14-00.flac",
                error,
                "Запись снова идёт",
                error,
            ]
        );
    }

    #[test]
    fn flac_restart_is_logged() {
        let (mut app, fake) = app();
        app.command(Command::Start, at(0));
        fake.borrow_mut().restart_on_write = true;
        app.block(&block(-20.0, -20.0), at(1));
        assert_eq!(app.state(), State::Recording);
        assert_eq!(
            texts(&app),
            [
                "Запись начата: /calls/2026-10-21_14-00.flac",
                "flac перезапущен (сбой), сохранено: /calls/x-part1.flac",
            ]
        );
    }

    #[test]
    fn log_keeps_last_entries() {
        let (mut app, _fake) = app();
        let total = LOG_CAPACITY + 2;
        for i in 0..total {
            app.notify(
                at(i64::try_from(i).unwrap()),
                Severity::Info,
                format!("{i}"),
            );
        }
        let expected: Vec<String> = (2..total).map(|i| i.to_string()).collect();
        assert_eq!(texts(&app), expected);
    }

    #[test]
    fn pipewire_events_are_logged() {
        let (mut app, _fake) = app();
        app.pw_event(
            PwEvent::Disconnected {
                reason: "connection error (-32)".into(),
            },
            at(0),
        );
        app.pw_event(PwEvent::Connected, at(1));
        app.pw_event(
            PwEvent::ManualReconnect {
                channel: Channel::Mic,
            },
            at(2),
        );
        assert_eq!(
            texts(&app),
            [
                "PipeWire: нет соединения (connection error (-32))",
                "PipeWire: подключено",
                "Микрофон: переподключено вручную",
            ]
        );
    }

    #[test]
    fn recorder_thread_saves_on_quit() {
        let (mut app, fake) = app();
        let (_pcm_tx, pcm_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let (snapshot_tx, snapshot_rx) = crossbeam_channel::unbounded();
        command_tx.send(Command::Start).unwrap();
        command_tx
            .send(Command::Quit(QuitReason::Signal("SIGTERM")))
            .unwrap();
        run(&mut app, pcm_rx, event_rx, &command_rx, &snapshot_tx);

        let states: Vec<State> = snapshot_rx.try_iter().map(|s| s.state).collect();
        assert!(states.contains(&State::Recording), "{states:?}");
        assert!(states.contains(&State::Saving), "{states:?}");
        assert_eq!(states.last(), Some(&State::Idle));
        assert_eq!(calls(&fake).last(), Some(&Call::Stop(String::new(), true)));
    }

    #[test]
    fn recorder_thread_exits_when_commands_close() {
        let (mut app, _fake) = app();
        let (_pcm_tx, pcm_rx) = crossbeam_channel::unbounded();
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        let (command_tx, command_rx) = crossbeam_channel::unbounded::<Command>();
        let (snapshot_tx, _snapshot_rx) = crossbeam_channel::unbounded();
        drop(command_tx);
        run(&mut app, pcm_rx, event_rx, &command_rx, &snapshot_tx);
    }
}
