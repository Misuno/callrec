//! callrec — TUI call recorder. Specification: `instruction.md`.
//!
//! Thread startup, signals, terminal setup and restore (also on panic), the
//! start-up recovery dialog and the UI event loop (spec 5.1, 6.6, 6.7).

mod app;
mod mixer;
mod pw;
mod ui;
mod writer;

use std::fs::{self, File, TryLockError};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use chrono::Local;
use crossbeam_channel::TryRecvError;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::app::{App, Command, DiskStorage, LogEntry, QuitReason, Severity};
use crate::ui::{RecoveryChoice, Ui, UiAction};

/// UI refresh interval (spec 5.1).
const FRAME: Duration = Duration::from_millis(50);
/// Lock file that keeps a second instance from touching our `.flac.part`.
const LOCK_FILE: &str = ".callrec.lock";
/// Signals that save and exit (spec 6.7), with their names for the log.
/// The index + 1 is stored in the quit flag; 0 means no signal.
const QUIT_SIGNALS: [(i32, &str); 3] = [
    (signal_hook::consts::SIGTERM, "SIGTERM"),
    (signal_hook::consts::SIGINT, "SIGINT"),
    (signal_hook::consts::SIGHUP, "SIGHUP"),
];
/// Whether keyboard enhancement flags were pushed and must be popped.
static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

type Term = Terminal<CrosstermBackend<Stdout>>;

fn main() -> anyhow::Result<()> {
    let dir = writer::default_dir().context("каталог записей")?;
    fs::create_dir_all(&dir).with_context(|| format!("создание {}", dir.display()))?;
    let _lock = lock_instance(&dir)?;

    // SIGTERM, SIGINT and SIGHUP save and exit without asking (spec 6.7).
    let quit_signal = Arc::new(AtomicUsize::new(0));
    for (index, (signal, _)) in QUIT_SIGNALS.iter().enumerate() {
        signal_hook::flag::register_usize(*signal, Arc::clone(&quit_signal), index + 1)
            .context("обработчик сигналов")?;
    }

    install_panic_hook();
    let guard = TerminalGuard::enter().context("настройка терминала")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout())).context("терминал")?;

    let mut app = App::new(DiskStorage::new(dir.clone()));
    match writer::find_unfinished(&dir) {
        Ok(files) if !files.is_empty() => {
            if !recovery_dialog(&mut terminal, &files, &mut app, &quit_signal)? {
                return Ok(());
            }
        }
        Ok(_) => {}
        Err(e) => app.notify(
            Local::now(),
            Severity::Error,
            format!("Поиск незавершённых записей: {e}"),
        ),
    }

    let log = run_session(&mut terminal, app, &quit_signal)?;
    drop(terminal);
    drop(guard);
    print_log(&log);
    Ok(())
}

/// Starts the PipeWire and recorder threads and runs the UI until the
/// recorder thread has saved and exited. Returns the final event log.
fn run_session(
    terminal: &mut Term,
    mut app: App<DiskStorage>,
    quit_signal: &AtomicUsize,
) -> anyhow::Result<Vec<LogEntry>> {
    let (pcm_tx, pcm_rx) = crossbeam_channel::bounded(pw::PCM_QUEUE_CHUNKS);
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let pipewire = pw::spawn(pcm_tx, event_tx).context("запуск потока PipeWire")?;

    let (command_tx, command_rx) = crossbeam_channel::unbounded();
    let (snapshot_tx, snapshot_rx) = crossbeam_channel::unbounded();
    let recorder = thread::Builder::new()
        .name("recorder".into())
        .spawn(move || app::run(&mut app, pcm_rx, event_rx, &command_rx, &snapshot_tx))
        .context("запуск потока записи")?;

    let first = snapshot_rx.recv().context("поток записи завершился")?;
    let mut log = first.log.clone();
    let mut ui = Ui::new(first, Instant::now());
    ui.set_enter_on_release(KEYBOARD_ENHANCED.load(Ordering::Relaxed));
    let mut terminal_ok = true;
    let mut quitting = false;
    // Asks the recorder thread to save and exit; the loop then waits for it.
    let quit = |quitting: &mut bool, reason: QuitReason| {
        if !*quitting {
            *quitting = true;
            let _ = command_tx.send(Command::Quit(reason));
        }
    };

    loop {
        // A lost terminal (closed window) must not lose the recording: keep
        // going without drawing until the recorder has saved.
        if terminal_ok && terminal.draw(|frame| ui.draw(frame)).is_err() {
            terminal_ok = false;
            quit(&mut quitting, QuitReason::TerminalLost);
        }
        if let Some(name) = received_signal(quit_signal) {
            quit(&mut quitting, QuitReason::Signal(name));
        }

        let action = if terminal_ok {
            read_event(&mut ui).unwrap_or_else(|_| {
                terminal_ok = false;
                quit(&mut quitting, QuitReason::TerminalLost);
                None
            })
        } else {
            thread::sleep(FRAME);
            None
        };
        match action {
            Some(UiAction::Command(command)) => {
                let _ = command_tx.send(command);
            }
            Some(UiAction::Quit(reason)) => quit(&mut quitting, reason),
            None => {}
        }

        let mut finished = false;
        loop {
            match snapshot_rx.try_recv() {
                Ok(snapshot) => {
                    log.clone_from(&snapshot.log);
                    ui.update(snapshot, Instant::now());
                }
                Err(TryRecvError::Empty) => break,
                // The recorder thread exits only after saving.
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            break;
        }
    }

    if recorder.join().is_err() {
        log.push(LogEntry {
            at: Local::now(),
            text: "Поток записи аварийно завершился".into(),
            severity: Severity::Error,
        });
    }
    pipewire.shutdown();
    Ok(log)
}

/// Name of the quit signal received so far, if any.
fn received_signal(flag: &AtomicUsize) -> Option<&'static str> {
    let index = flag.load(Ordering::Relaxed).checked_sub(1)?;
    // An unknown value still means a signal arrived.
    Some(QUIT_SIGNALS.get(index).map_or("?", |(_, name)| *name))
}

/// Waits up to one frame for input and turns it into an action.
fn read_event(ui: &mut Ui) -> io::Result<Option<UiAction>> {
    if !event::poll(FRAME)? {
        return Ok(None);
    }
    Ok(match event::read()? {
        Event::Key(key) => ui.key(key),
        Event::Mouse(mouse) => ui.mouse(mouse),
        _ => None,
    })
}

/// Asks what to do with unfinished recordings (spec 6.6). Returns `false`
/// if the user chose to exit.
fn recovery_dialog(
    terminal: &mut Term,
    files: &[PathBuf],
    app: &mut App<DiskStorage>,
    quit_signal: &AtomicUsize,
) -> anyhow::Result<bool> {
    let choice = loop {
        terminal.draw(|frame| ui::draw_recovery(frame, files, None))?;
        if received_signal(quit_signal).is_some() {
            break RecoveryChoice::Quit;
        }
        if event::poll(FRAME)?
            && let Event::Key(key) = event::read()?
            && let Some(choice) = ui::recovery_choice(key)
        {
            break choice;
        }
    };

    match choice {
        RecoveryChoice::Quit => return Ok(false),
        RecoveryChoice::Later => app.notify(
            Local::now(),
            Severity::Warning,
            format!("Незавершённые записи оставлены: {}", files.len()),
        ),
        RecoveryChoice::Recover => {
            for file in files {
                let progress = format!("Восстановление {}…", app::display_path(file));
                terminal.draw(|frame| ui::draw_recovery(frame, files, Some(&progress)))?;
                let (severity, text) = match writer::recover(file) {
                    Ok(path) => (
                        Severity::Info,
                        format!("Восстановлено: {}", app::display_path(&path)),
                    ),
                    Err(e) => (Severity::Error, format!("Не восстановлено: {e}")),
                };
                app.notify(Local::now(), severity, text);
            }
        }
        RecoveryChoice::Delete => {
            for file in files {
                let (severity, text) = match writer::discard(file) {
                    Ok(()) => (
                        Severity::Info,
                        format!("Удалено: {}", app::display_path(file)),
                    ),
                    Err(e) => (Severity::Error, format!("Не удалено: {e}")),
                };
                app.notify(Local::now(), severity, text);
            }
        }
    }
    Ok(true)
}

/// Holds an exclusive lock on `<dir>/.callrec.lock` for the process lifetime.
/// A second instance would otherwise offer to recover or delete the
/// `.flac.part` this one is writing.
fn lock_instance(dir: &Path) -> anyhow::Result<File> {
    let path = dir.join(LOCK_FILE);
    let file = File::create(&path).with_context(|| format!("создание {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => bail!("callrec уже запущен"),
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("блокировка {}", path.display()))
        }
    }
}

/// Raw mode, alternate screen, mouse capture and, where the terminal supports
/// it, the kitty keyboard protocol with key release reports (so Enter acts on
/// release, spec 3); undone on drop.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        // From here on Drop restores the terminal, also if the next call fails.
        let guard = Self;
        crossterm::execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
        // Enter, Tab and Backspace report releases only when all keys are sent
        // as escape codes; alternate keys keep Shift+letter an uppercase letter.
        if supports_keyboard_enhancement().unwrap_or(false) {
            crossterm::execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                        | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES
                )
            )?;
            KEYBOARD_ENHANCED.store(true, Ordering::Relaxed);
        }
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    // Best effort: there is nowhere left to report a failure.
    if KEYBOARD_ENHANCED.swap(false, Ordering::Relaxed) {
        let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen,
        crossterm::cursor::Show
    );
}

/// Restores the terminal before the default panic message is printed.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default(info);
    }));
}

/// The last events (e.g. where the recording was saved) after the UI is gone.
fn print_log(log: &[LogEntry]) {
    let mut stdout = io::stdout().lock();
    for entry in log {
        // Nothing to do if stdout is closed.
        let _ = writeln!(stdout, "{}  {}", entry.at.format("%H:%M:%S"), entry.text);
    }
}
