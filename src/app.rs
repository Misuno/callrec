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

use crate::mixer::{Block, Channel, Mixer, TICK, TickClock};
use crate::pw::{Device, PcmChunk, PwEvent};
use crate::writer::{self, Recording, Saved, WriteOutcome, WriterError};

/// Number of events kept in the log (spec 3).
pub const LOG_LINES: usize = 3;
/// A channel quieter than this while recording counts as silent (spec 6.3).
pub const SILENCE_THRESHOLD_DB: f64 = -60.0;
/// How long a channel must stay silent before it is highlighted (spec 6.3).
pub const SILENCE_DURATION: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording,
    Paused,
    Saving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// `r`: start recording.
    Start,
    /// `p`: pause or resume.
    TogglePause,
    /// `s`: stop and save.
    Stop,
    /// `q` (after confirmation), Ctrl+C, SIGTERM: save if recording, exit.
    Quit,
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
    fn start(&self, at: DateTime<Local>) -> Result<Self::Recorder, WriterError>;
}

/// A recording in progress. Implemented by [`Recording`] and by test fakes.
pub trait Recorder {
    fn path(&self) -> PathBuf;
    fn duration(&self) -> Duration;
    fn write_block(&mut self, stereo: &[i16]) -> Result<WriteOutcome, WriterError>;
    fn pause(&mut self, at: DateTime<Local>);
    fn resume(&mut self, at: DateTime<Local>);
    fn device_change(&mut self, channel: Channel, device: &str, description: &str);
    fn stop(self, at: DateTime<Local>) -> Result<Saved, WriterError>;
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

    fn start(&self, at: DateTime<Local>) -> Result<Recording, WriterError> {
        Recording::start(&self.dir, at)
    }
}

impl Recorder for Recording {
    fn path(&self) -> PathBuf {
        Recording::path(self)
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

    fn stop(self, at: DateTime<Local>) -> Result<Saved, WriterError> {
        Recording::stop(self, at)
    }
}

#[derive(Debug)]
struct ChannelState {
    device: DeviceStatus,
    level_db: f64,
    /// Consecutive recorded blocks below `SILENCE_THRESHOLD_DB`.
    silent_blocks: u32,
}

impl ChannelState {
    fn new() -> Self {
        Self {
            device: DeviceStatus::Unknown,
            level_db: crate::mixer::LEVEL_FLOOR_DB,
            silent_blocks: 0,
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

/// Blocks in `SILENCE_DURATION`.
fn silence_blocks() -> u32 {
    u32::try_from(SILENCE_DURATION.as_millis() / TICK.as_millis()).unwrap_or(u32::MAX)
}

#[derive(Debug)]
pub struct App<S: Storage> {
    storage: S,
    state: State,
    recorder: Option<S::Recorder>,
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
            last_path: None,
            last_duration: Duration::ZERO,
            mic: ChannelState::new(),
            them: ChannelState::new(),
            log: VecDeque::with_capacity(LOG_LINES + 1),
            write_error: None,
        }
    }

    #[must_use]
    pub fn state(&self) -> State {
        self.state
    }

    /// Whether a command does anything in the current state (spec 3).
    #[must_use]
    pub fn allows(&self, command: Command) -> bool {
        allows(self.state, command)
    }

    /// Handles a command. `Stop` and `Quit` while recording leave the app in
    /// `Saving`; call [`App::finish_saving`] after the UI has shown it.
    pub fn command(&mut self, command: Command, now: DateTime<Local>) {
        if !self.allows(command) {
            return;
        }
        match command {
            Command::Start => self.start(now),
            Command::TogglePause => self.toggle_pause(now),
            Command::Stop | Command::Quit => {
                if self.recorder.is_some() {
                    self.state = State::Saving;
                }
            }
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
        match self.storage.start(now) {
            Ok(mut recorder) => {
                for (channel, state) in [(Channel::Mic, &self.mic), (Channel::Them, &self.them)] {
                    if let DeviceStatus::Linked(device) = &state.device {
                        recorder.device_change(channel, &device.name, &device.description);
                    }
                }
                self.last_path = Some(recorder.path());
                self.last_duration = Duration::ZERO;
                self.recorder = Some(recorder);
                self.state = State::Recording;
                self.write_error = None;
                self.mic.silent_blocks = 0;
                self.them.silent_blocks = 0;
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
            }
            State::Paused => {
                recorder.resume(now);
                self.state = State::Recording;
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
        let Some(recorder) = self.recorder.take() else {
            return;
        };
        self.last_duration = recorder.duration();
        match recorder.stop(now) {
            Ok(saved) => {
                let files = saved
                    .files
                    .iter()
                    .map(|f| display_path(f))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.log(now, Severity::Info, format!("Сохранено: {files}"));
            }
            Err(e) => self.log(now, Severity::Error, format!("Ошибка сохранения: {e}")),
        }
    }

    /// Takes one mixer block: levels always, audio only while recording.
    pub fn block(&mut self, block: &Block, now: DateTime<Local>) {
        let recording = self.state == State::Recording;
        for (state, tick) in [(&mut self.mic, &block.mic), (&mut self.them, &block.them)] {
            state.level_db = tick.level_db;
            if recording {
                if tick.level_db < SILENCE_THRESHOLD_DB {
                    state.silent_blocks = state.silent_blocks.saturating_add(1);
                } else {
                    state.silent_blocks = 0;
                }
            }
        }
        if !recording {
            return;
        }
        let Some(recorder) = self.recorder.as_mut() else {
            return;
        };
        match recorder.write_block(&block.stereo) {
            Ok(WriteOutcome::Written | WriteOutcome::Skipped) => self.write_error = None,
            Ok(WriteOutcome::Restarted { saved, cause }) => {
                self.write_error = None;
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
        }
    }

    fn stream_device(&mut self, channel: Channel, device: Option<Device>, now: DateTime<Local>) {
        let state = match channel {
            Channel::Mic => &mut self.mic,
            Channel::Them => &mut self.them,
        };
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
        while self.log.len() > LOG_LINES {
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
            path: self.last_path.clone(),
            mic: self.mic.view(recording),
            them: self.them.view(recording),
            log: self.log.iter().cloned().collect(),
        }
    }
}

/// Whether a command does anything in `state` (spec 3). The UI dims the
/// buttons for which this is false.
#[must_use]
pub fn allows(state: State, command: Command) -> bool {
    match command {
        Command::Start => state == State::Idle,
        Command::TogglePause | Command::Stop => matches!(state, State::Recording | State::Paused),
        Command::Quit => true,
    }
}

#[must_use]
pub fn channel_label(channel: Channel) -> &'static str {
    match channel {
        Channel::Mic => "Микрофон",
        Channel::Them => "Собеседники",
    }
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
                let command = command.unwrap_or(Command::Quit);
                app.command(command, Local::now());
                if app.state() == State::Saving {
                    publish(app);
                    app.finish_saving(Local::now());
                }
                if command == Command::Quit {
                    publish(app);
                    return;
                }
                changed = true;
            },
            recv(crossbeam_channel::at(deadline)) -> _ => {},
        }
        for _ in 0..clock.take_due(Instant::now()) {
            app.block(&mixer.tick(), Local::now());
            changed = true;
        }
        if changed {
            publish(app);
        }
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
        Start,
        Write,
        Pause,
        Resume,
        Device(Channel, String),
        Stop,
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

        fn start(&self, _at: DateTime<Local>) -> Result<FakeRecorder, WriterError> {
            let mut fake = self.0.borrow_mut();
            fake.calls.push(Call::Start);
            if fake.fail_start {
                return Err(io_error());
            }
            Ok(FakeRecorder(Rc::clone(&self.0)))
        }
    }

    impl Recorder for FakeRecorder {
        fn path(&self) -> PathBuf {
            PathBuf::from("/calls/2026-10-21_14-00.flac")
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

        fn stop(self, _at: DateTime<Local>) -> Result<Saved, WriterError> {
            let mut fake = self.0.borrow_mut();
            fake.calls.push(Call::Stop);
            if fake.fail_stop {
                return Err(io_error());
            }
            Ok(Saved {
                files: vec![PathBuf::from("/calls/2026-10-21_14-00.flac")],
                metadata: PathBuf::from("/calls/2026-10-21_14-00.json"),
            })
        }
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
        assert!(app.allows(Command::Start));
        assert!(!app.allows(Command::TogglePause));
        assert!(!app.allows(Command::Stop));

        app.command(Command::Start, at(0));
        assert_eq!(app.state(), State::Recording);
        assert!(!app.allows(Command::Start));
        app.block(&block(-20.0, -30.0), at(0));

        app.command(Command::TogglePause, at(1));
        assert_eq!(app.state(), State::Paused);
        app.block(&block(-20.0, -30.0), at(1));
        app.command(Command::TogglePause, at(2));
        assert_eq!(app.state(), State::Recording);
        app.block(&block(-20.0, -30.0), at(2));

        app.command(Command::Stop, at(3));
        assert_eq!(app.state(), State::Saving);
        assert!(!app.allows(Command::Stop));
        app.finish_saving(at(3));
        assert_eq!(app.state(), State::Idle);

        assert_eq!(
            calls(&fake),
            vec![
                Call::Start,
                Call::Write,
                Call::Pause,
                Call::Resume,
                Call::Write,
                Call::Stop
            ]
        );
        let snapshot = app.snapshot();
        assert_eq!(snapshot.duration, TICK * 2);
        assert_eq!(
            snapshot.path,
            Some(PathBuf::from("/calls/2026-10-21_14-00.flac"))
        );
        assert_eq!(texts(&app), ["Сохранено: /calls/2026-10-21_14-00.flac"]);
    }

    #[test]
    fn commands_outside_their_states_are_ignored() {
        let (mut app, fake) = app();
        app.command(Command::TogglePause, at(0));
        app.command(Command::Stop, at(0));
        app.finish_saving(at(0));
        assert_eq!(app.state(), State::Idle);
        app.command(Command::Start, at(0));
        app.command(Command::Start, at(1));
        assert_eq!(calls(&fake), vec![Call::Start]);
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
        assert_eq!(texts(&app), ["Мало места: свободно меньше 1 ГБ"]);
    }

    #[test]
    fn failed_save_returns_to_idle_with_error() {
        let (mut app, fake) = app();
        fake.borrow_mut().fail_stop = true;
        app.command(Command::Start, at(0));
        app.command(Command::Stop, at(1));
        app.finish_saving(at(1));
        assert_eq!(app.state(), State::Idle);
        assert_eq!(
            texts(&app),
            ["Ошибка сохранения: не удалось запустить flac: тест"]
        );
        let entry = app.snapshot().log.pop().unwrap();
        assert_eq!(entry.severity, Severity::Error);
    }

    #[test]
    fn quit_while_paused_saves() {
        let (mut app, fake) = app();
        app.command(Command::Start, at(0));
        app.command(Command::TogglePause, at(1));
        app.command(Command::Quit, at(2));
        assert_eq!(app.state(), State::Saving);
        app.finish_saving(at(2));
        assert_eq!(calls(&fake).last(), Some(&Call::Stop));
    }

    #[test]
    fn quit_while_idle_does_nothing() {
        let (mut app, fake) = app();
        app.command(Command::Quit, at(0));
        assert_eq!(app.state(), State::Idle);
        assert_eq!(calls(&fake), vec![]);
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
            ["Микрофон: Встроенный микрофон → Jabra Evolve2 65 Mono"]
        );
        assert_eq!(
            calls(&fake),
            vec![
                Call::Start,
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
        assert_eq!(
            texts(&app),
            ["Ошибка записи: не удалось запустить flac: тест"]
        );
        // Recovered, then failed again: logged again.
        fake.borrow_mut().fail_write = false;
        app.block(&block(-20.0, -20.0), at(2));
        fake.borrow_mut().fail_write = true;
        app.block(&block(-20.0, -20.0), at(3));
        assert_eq!(texts(&app).len(), 2);
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
            ["flac перезапущен (сбой), сохранено: /calls/x-part1.flac"]
        );
    }

    #[test]
    fn log_keeps_last_entries() {
        let (mut app, _fake) = app();
        for i in 0..5 {
            app.notify(at(i), Severity::Info, format!("{i}"));
        }
        assert_eq!(texts(&app), ["2", "3", "4"]);
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
        command_tx.send(Command::Quit).unwrap();
        run(&mut app, pcm_rx, event_rx, &command_rx, &snapshot_tx);

        let states: Vec<State> = snapshot_rx.try_iter().map(|s| s.state).collect();
        assert!(states.contains(&State::Recording), "{states:?}");
        assert!(states.contains(&State::Saving), "{states:?}");
        assert_eq!(states.last(), Some(&State::Idle));
        assert_eq!(calls(&fake).last(), Some(&Call::Stop));
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
