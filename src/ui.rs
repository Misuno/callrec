//! Terminal UI: the screen from spec 3, keys, mouse, keyboard focus, the
//! meeting title field, dimmed unavailable buttons and level meters with peak
//! hold (spec 5.5).
//!
//! [`Ui`] only turns snapshots into frames and input into [`UiAction`]s; the
//! terminal and the event loop are set up in `main.rs`.

use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::app::{self, ChannelView, Command, DeviceStatus, LogEntry, Severity, Snapshot, State};
use crate::mixer::{Channel, LEVEL_FLOOR_DB};

/// Lowest level the meter shows; quieter is an empty bar.
pub const METER_MIN_DB: f64 = -60.0;
/// How fast the meter falls after the level drops.
pub const METER_DECAY_DB_PER_SEC: f64 = 30.0;
/// How long the peak mark stays before it falls (spec 5.5).
pub const PEAK_HOLD: Duration = Duration::from_secs(1);
/// Longest meeting title the field accepts, in characters.
pub const MAX_TITLE_CHARS: usize = 80;
/// The recording and pause marks on the buttons blink with this half-period.
const BLINK: Duration = Duration::from_millis(500);
/// From this terminal width on, the device panels stand side by side.
const SIDE_BY_SIDE_MIN_WIDTH: u16 = 100;
/// Device panel: borders, description, system name, meter.
const DEVICE_PANEL_HEIGHT: u16 = 5;
/// Title field and status panel: borders and one line.
const STATUS_HEIGHT: u16 = 3;
/// Buttons: borders and one line.
const BUTTONS_HEIGHT: u16 = 3;
/// Cells of a meter line besides the bar: " ▕", "▏ -18 dB" and a margin.
const METER_EXTRA_WIDTH: u16 = 12;
/// Button widths: the longest label ("▶ Продолжить  ❚❚ 00:00:00",
/// "✔ Сохранить", "⌫ Удалить запись", "✕ Выход") plus borders and padding.
/// All four fit in 80 columns.
const RECORD_BUTTON_WIDTH: u16 = 29;
const SAVE_BUTTON_WIDTH: u16 = 15;
const DELETE_BUTTON_WIDTH: u16 = 20;
const QUIT_BUTTON_WIDTH: u16 = 11;
/// Narrowest content of a dialog, in cells.
const DIALOG_MIN_WIDTH: usize = 40;
/// Spaces between dialog buttons.
const DIALOG_BUTTON_GAP: usize = 3;
const DIALOG_BUTTON_GAP_CELLS: u16 = 3;

/// What the user asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiAction {
    Command(Command),
    /// Save the recording, if any, and exit.
    Quit,
}

/// Questions on top of the screen (spec 3). They know nothing about each
/// other except which one a button opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dialog {
    /// `q` while recording: exit and keep the recording marked as
    /// interrupted?
    Quit,
    /// Save the recording, go back to it or delete it?
    Save,
    /// The title is empty: give one or save without it.
    Name,
    /// Delete the recording or go back to it?
    Delete,
}

/// Buttons of the dialogs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DialogButton {
    Save,
    Delete,
    /// Close the dialog; the recording stays as it is.
    Back,
    Leave,
}

/// What a key did to the title text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Changed,
    /// Only the caret moved, or nothing could be done.
    Moved,
    /// Not an editing key.
    Ignored,
}

/// Elements the keyboard focus moves between, in Tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Title,
    Record,
    Save,
    Delete,
    Quit,
}

const FOCUS_ORDER: [Focus; 5] = [
    Focus::Title,
    Focus::Record,
    Focus::Save,
    Focus::Delete,
    Focus::Quit,
];

/// The button row, left to right.
const BUTTON_ORDER: [Focus; 4] = [Focus::Record, Focus::Save, Focus::Delete, Focus::Quit];

/// What a click hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Focus(Focus),
    Dialog(DialogButton),
}

/// Level meter with a smooth fall and a held peak.
#[derive(Debug, Clone)]
struct Meter {
    level: f64,
    peak: f64,
    peak_at: Instant,
    updated: Instant,
}

impl Meter {
    fn new(now: Instant) -> Self {
        Self {
            level: LEVEL_FLOOR_DB,
            peak: LEVEL_FLOOR_DB,
            peak_at: now,
            updated: now,
        }
    }

    fn update(&mut self, level_db: f64, now: Instant) {
        let fall =
            METER_DECAY_DB_PER_SEC * now.saturating_duration_since(self.updated).as_secs_f64();
        self.updated = now;
        self.level = level_db.max(self.level - fall).max(LEVEL_FLOOR_DB);
        if level_db >= self.peak {
            self.peak = level_db;
            self.peak_at = now;
        } else if now.saturating_duration_since(self.peak_at) > PEAK_HOLD {
            self.peak = (self.peak - fall).max(self.level);
        }
    }

    /// The bar: `width` cells between `METER_MIN_DB` and 0 dB, the peak cell
    /// marked.
    fn bar(&self, width: u16) -> (String, Option<usize>) {
        let range = -METER_MIN_DB;
        let threshold =
            |cell: u16| METER_MIN_DB + (f64::from(cell) + 0.5) / f64::from(width) * range;
        let bar = (0..width)
            .map(|cell| {
                if self.level >= threshold(cell) {
                    '█'
                } else {
                    '▁'
                }
            })
            .collect();
        let peak = (0..width)
            .rev()
            .find(|&cell| self.peak >= threshold(cell))
            .map(usize::from);
        (bar, peak)
    }
}

#[derive(Debug)]
pub struct Ui {
    snapshot: Snapshot,
    mic: Meter,
    them: Meter,
    /// Clickable areas of the last frame.
    targets: Vec<(Rect, Target)>,
    dialog: Option<Dialog>,
    /// Selected button of the dialog, by position.
    selected: usize,
    focus: Focus,
    /// Enter acts on release; see [`Ui::set_enter_on_release`].
    enter_on_release: bool,
    /// Where Enter was pressed, while waiting for its release.
    enter_pressed: Option<(Option<Dialog>, Focus)>,
    /// Meeting title as typed.
    title: String,
    /// Caret position in `title`, in characters.
    caret: usize,
    now: Instant,
    /// Start of the current pause, for the timer on the pause button.
    paused_since: Option<Instant>,
}

impl Ui {
    #[must_use]
    pub fn new(snapshot: Snapshot, now: Instant) -> Self {
        let mut ui = Self {
            snapshot: snapshot.clone(),
            mic: Meter::new(now),
            them: Meter::new(now),
            targets: Vec::new(),
            dialog: None,
            selected: 0,
            focus: Focus::Record,
            enter_on_release: false,
            enter_pressed: None,
            title: String::new(),
            caret: 0,
            now,
            paused_since: None,
        };
        ui.update(snapshot, now);
        ui
    }

    pub fn update(&mut self, snapshot: Snapshot, now: Instant) {
        self.now = now;
        self.mic.update(snapshot.mic.level_db, now);
        self.them.update(snapshot.them.level_db, now);
        if snapshot.state == State::Paused {
            self.paused_since.get_or_insert(now);
        } else {
            self.paused_since = None;
        }
        if !is_recording(snapshot.state) {
            // Nothing left to ask: the recording was saved, deleted or ended
            // on its own. Its buttons are gone, so is their focus.
            self.dialog = None;
            if matches!(self.focus, Focus::Save | Focus::Delete) {
                self.focus = Focus::Record;
            }
        }
        self.snapshot = snapshot;
    }

    /// Whether the terminal reports key releases (kitty keyboard protocol).
    /// Then Enter acts when released, otherwise when pressed (spec 3).
    pub fn set_enter_on_release(&mut self, on: bool) {
        self.enter_on_release = on;
    }

    /// A key event. Keys act when pressed and auto-repeat as usual; only
    /// Enter, when the terminal reports releases, acts on release.
    pub fn key(&mut self, key: KeyEvent) -> Option<UiAction> {
        if key.code == KeyCode::Enter && self.enter_on_release {
            return self.enter_by_release(key);
        }
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Ctrl+C saves and exits without asking (spec 6.7).
            return Some(UiAction::Quit);
        }
        self.dispatch(key)
    }

    /// Enter acts when released, and only on the screen it was pressed on: a
    /// held Enter presses its button once and never the one that comes next.
    fn enter_by_release(&mut self, key: KeyEvent) -> Option<UiAction> {
        let screen = (self.dialog, self.focus);
        match key.kind {
            KeyEventKind::Press => {
                self.enter_pressed = Some(screen);
                None
            }
            KeyEventKind::Repeat => None,
            KeyEventKind::Release => {
                if self.enter_pressed.take() == Some(screen) {
                    self.dispatch(key)
                } else {
                    None
                }
            }
        }
    }

    fn dispatch(&mut self, key: KeyEvent) -> Option<UiAction> {
        if let Some(dialog) = self.dialog {
            return self.dialog_key(dialog, key);
        }
        if self.focus == Focus::Title {
            return self.title_key(key);
        }
        match key.code {
            KeyCode::Tab => self.cycle_focus(true),
            KeyCode::BackTab => self.cycle_focus(false),
            KeyCode::Right => self.move_between_buttons(true),
            KeyCode::Left => self.move_between_buttons(false),
            KeyCode::Up => self.focus_title(),
            KeyCode::Enter => return self.activate(self.focus),
            // The same keys on the Russian layout.
            KeyCode::Char(c) => match c.to_lowercase().next().unwrap_or(c) {
                ' ' => return self.activate(Focus::Record),
                's' | 'ы' => return self.activate(Focus::Save),
                'd' | 'в' => return self.activate(Focus::Delete),
                'q' | 'й' => return self.activate(Focus::Quit),
                'r' | 'к' => self.focus_title(),
                _ => {}
            },
            _ => {}
        }
        None
    }

    pub fn mouse(&mut self, mouse: MouseEvent) -> Option<UiAction> {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        let position = Position::new(mouse.column, mouse.row);
        let target = self
            .targets
            .iter()
            .find(|(area, _)| area.contains(position))
            .map(|(_, target)| *target)?;
        match target {
            Target::Focus(Focus::Title) => {
                self.focus_title();
                None
            }
            Target::Focus(focus) => {
                self.focus = focus;
                self.activate(focus)
            }
            Target::Dialog(button) => self.press_dialog(button),
        }
    }

    /// Keys while a dialog is open: arrows and Tab move the selection, Enter
    /// presses it, `Esc` is "Back", letters are shortcuts. In the name
    /// prompt letters are text.
    fn dialog_key(&mut self, dialog: Dialog, key: KeyEvent) -> Option<UiAction> {
        let buttons = dialog_buttons(dialog);
        match key.code {
            KeyCode::Esc => return self.press_dialog(DialogButton::Back),
            KeyCode::Enter => {
                let selected = buttons.get(self.selected).map(|&(_, button, _)| button);
                return selected.and_then(|button| self.press_dialog(button));
            }
            _ => {}
        }
        if dialog == Dialog::Name {
            return match self.edit_title(key) {
                Edit::Changed => Some(self.title_changed()),
                Edit::Moved | Edit::Ignored => None,
            };
        }
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                let forward = matches!(key.code, KeyCode::Right | KeyCode::Tab);
                let count = buttons.len().max(1);
                self.selected = if forward {
                    (self.selected + 1) % count
                } else {
                    (self.selected + count - 1) % count
                };
                None
            }
            KeyCode::Char(c) => {
                let c = c.to_lowercase().next().unwrap_or(c);
                let button = buttons
                    .iter()
                    .find(|(_, _, keys)| keys.contains(c))
                    .map(|&(_, button, _)| button);
                button.and_then(|button| self.press_dialog(button))
            }
            _ => None,
        }
    }

    /// A dialog button, by key or click. Each dialog only decides what its
    /// own buttons do: send a command, close, or open another dialog.
    fn press_dialog(&mut self, button: DialogButton) -> Option<UiAction> {
        let dialog = self.dialog?;
        self.dialog = None;
        match (dialog, button) {
            (_, DialogButton::Back) => None,
            (_, DialogButton::Leave) => Some(UiAction::Quit),
            (Dialog::Save, DialogButton::Save) if self.title.trim().is_empty() => {
                self.open(Dialog::Name);
                None
            }
            (_, DialogButton::Save) => Some(UiAction::Command(Command::Save)),
            (Dialog::Save, DialogButton::Delete) => {
                self.open(Dialog::Delete);
                None
            }
            (_, DialogButton::Delete) => Some(UiAction::Command(Command::Discard)),
        }
    }

    /// Opens a dialog with its default button selected: "Save" in the save
    /// dialog and the name prompt, "Back" in the others. The name prompt puts
    /// the caret after the title.
    fn open(&mut self, dialog: Dialog) {
        let default = match dialog {
            Dialog::Save | Dialog::Name => DialogButton::Save,
            Dialog::Quit | Dialog::Delete => DialogButton::Back,
        };
        self.selected = dialog_buttons(dialog)
            .iter()
            .position(|&(_, button, _)| button == default)
            .unwrap_or(0);
        if dialog == Dialog::Name {
            self.caret = self.title.chars().count();
        }
        self.dialog = Some(dialog);
    }

    /// Keys while the title field has the focus: typing edits the title, so
    /// the shortcuts are off until the focus leaves the field.
    fn title_key(&mut self, key: KeyEvent) -> Option<UiAction> {
        match self.edit_title(key) {
            Edit::Changed => return Some(self.title_changed()),
            Edit::Moved => return None,
            Edit::Ignored => {}
        }
        match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Down => self.focus = Focus::Record,
            KeyCode::Tab => self.cycle_focus(true),
            KeyCode::BackTab => self.cycle_focus(false),
            _ => {}
        }
        None
    }

    /// Typing, deleting and moving the caret in the title.
    fn edit_title(&mut self, key: KeyEvent) -> Edit {
        let len = self.title.chars().count();
        match key.code {
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if c.is_control() || len >= MAX_TITLE_CHARS {
                    return Edit::Moved;
                }
                let at = byte_index(&self.title, self.caret);
                self.title.insert(at, c);
                self.caret += 1;
                Edit::Changed
            }
            KeyCode::Backspace => {
                if self.caret == 0 {
                    return Edit::Moved;
                }
                self.caret -= 1;
                let at = byte_index(&self.title, self.caret);
                self.title.remove(at);
                Edit::Changed
            }
            KeyCode::Delete => {
                if self.caret >= len {
                    return Edit::Moved;
                }
                let at = byte_index(&self.title, self.caret);
                self.title.remove(at);
                Edit::Changed
            }
            KeyCode::Left => {
                self.caret = self.caret.saturating_sub(1);
                Edit::Moved
            }
            KeyCode::Right => {
                self.caret = (self.caret + 1).min(len);
                Edit::Moved
            }
            KeyCode::Home => {
                self.caret = 0;
                Edit::Moved
            }
            KeyCode::End => {
                self.caret = len;
                Edit::Moved
            }
            _ => Edit::Ignored,
        }
    }

    fn title_changed(&self) -> UiAction {
        UiAction::Command(Command::SetTitle(self.title.clone()))
    }

    fn focus_title(&mut self) {
        if self.focus != Focus::Title {
            self.caret = self.title.chars().count();
        }
        self.focus = Focus::Title;
    }

    /// Elements that exist now: "Save" and "Delete" only with a recording.
    fn shown(&self, focus: Focus) -> bool {
        !matches!(focus, Focus::Save | Focus::Delete) || is_recording(self.snapshot.state)
    }

    fn cycle_focus(&mut self, forward: bool) {
        let order: Vec<Focus> = FOCUS_ORDER.into_iter().filter(|&f| self.shown(f)).collect();
        match cycle(&order, self.focus, forward) {
            Focus::Title => self.focus_title(),
            focus => self.focus = focus,
        }
    }

    /// Left and right move along the button row and stop at its ends.
    fn move_between_buttons(&mut self, right: bool) {
        let row: Vec<Focus> = BUTTON_ORDER
            .into_iter()
            .filter(|&f| self.shown(f))
            .collect();
        let index = row.iter().position(|&f| f == self.focus).unwrap_or(0);
        let next = if right {
            index + 1
        } else {
            index.saturating_sub(1)
        };
        if let Some(&focus) = row.get(next).or(row.get(index)) {
            self.focus = focus;
        }
    }

    /// Enter on the element, a click on it or its shortcut. "Save" and
    /// "Delete" pause the recording and open their dialog; quitting while
    /// recording asks first.
    fn activate(&mut self, focus: Focus) -> Option<UiAction> {
        let state = self.snapshot.state;
        let recording = is_recording(state);
        let command = match focus {
            Focus::Title => {
                self.focus_title();
                return None;
            }
            Focus::Record => record_command(state),
            Focus::Save | Focus::Delete if recording => {
                self.open(if focus == Focus::Save {
                    Dialog::Save
                } else {
                    Dialog::Delete
                });
                Command::Pause
            }
            Focus::Save | Focus::Delete => return None,
            Focus::Quit if recording => {
                self.open(Dialog::Quit);
                return None;
            }
            Focus::Quit => return Some(UiAction::Quit),
        };
        app::allows(state, &command).then_some(UiAction::Command(command))
    }

    /// Full-screen layout: device panels on top (side by side when wide
    /// enough), the title field and status, the log in the remaining space,
    /// buttons at the bottom.
    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        self.targets.clear();
        let side_by_side = frame.area().width >= SIDE_BY_SIDE_MIN_WIDTH;
        let devices_height = if side_by_side {
            DEVICE_PANEL_HEIGHT
        } else {
            DEVICE_PANEL_HEIGHT * 2
        };
        let [devices, status_row, log, buttons] = Layout::vertical([
            Constraint::Length(devices_height),
            Constraint::Length(STATUS_HEIGHT),
            Constraint::Fill(1),
            Constraint::Length(BUTTONS_HEIGHT),
        ])
        .areas(frame.area());
        let [mic_area, them_area] = if side_by_side {
            Layout::horizontal([Constraint::Fill(1); 2]).areas(devices)
        } else {
            Layout::vertical([Constraint::Length(DEVICE_PANEL_HEIGHT); 2]).areas(devices)
        };
        let [title_area, status] = Layout::horizontal([Constraint::Fill(1); 2]).areas(status_row);

        let snapshot = &self.snapshot;
        render_channel(frame, mic_area, Channel::Mic, &snapshot.mic, &self.mic);
        render_channel(frame, them_area, Channel::Them, &snapshot.them, &self.them);
        frame.render_widget(
            Paragraph::new(status_line(snapshot)).block(Block::bordered().title(" callrec ")),
            status,
        );

        let log_block = Block::bordered().title(" Журнал ");
        let visible = usize::from(log_block.inner(log).height);
        let entries = snapshot.log.len();
        let lines: Vec<_> = snapshot
            .log
            .iter()
            .skip(entries.saturating_sub(visible))
            .map(log_line)
            .collect();
        frame.render_widget(Paragraph::new(lines).block(log_block), log);

        self.draw_title(frame, title_area);
        self.draw_buttons(frame, buttons);
        if let Some(dialog) = self.dialog {
            self.draw_dialog(frame, dialog);
        }
    }

    /// The meeting title field with the terminal cursor at the caret while it
    /// has the focus.
    fn draw_title(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let focused = self.focus == Focus::Title && self.dialog.is_none();
        let block = Block::bordered()
            .title(" Название встречи [r] ")
            .border_type(if focused {
                BorderType::Thick
            } else {
                BorderType::Plain
            })
            .border_style(focus_style(focused));
        let inner = block.inner(area);
        // One cell of padding on the left, one for the caret after the text.
        let width = usize::from(inner.width).saturating_sub(2);
        let skip = self.caret.saturating_sub(width);
        let line = if self.title.is_empty() && !focused {
            Line::styled(" без названия", Style::default().fg(Color::DarkGray))
        } else {
            let visible: String = self.title.chars().skip(skip).take(width).collect();
            Line::from(format!(" {visible}"))
        };
        frame.render_widget(Paragraph::new(line).block(block), area);
        self.targets.push((area, Target::Focus(Focus::Title)));

        if focused && inner.height > 0 {
            let offset = u16::try_from(self.caret - skip).unwrap_or(u16::MAX);
            let x = inner.x.saturating_add(1).saturating_add(offset);
            if x < inner.right() {
                frame.set_cursor_position(Position::new(x, inner.y));
            }
        }
    }

    /// Record/pause, save, delete and quit buttons. "Save" and "Delete" exist
    /// only with a recording. The record button shows the recording time with
    /// a blinking mark while recording, and how long the pause lasts while
    /// paused.
    fn draw_buttons(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let state = self.snapshot.state;
        let widths: Vec<(Focus, u16)> = [
            (Focus::Record, RECORD_BUTTON_WIDTH),
            (Focus::Save, SAVE_BUTTON_WIDTH),
            (Focus::Delete, DELETE_BUTTON_WIDTH),
            (Focus::Quit, QUIT_BUTTON_WIDTH),
        ]
        .into_iter()
        .filter(|&(focus, _)| self.shown(focus))
        .collect();
        let areas = Layout::horizontal(
            widths
                .iter()
                .map(|&(_, width)| Constraint::Length(width))
                .chain([Constraint::Fill(1)]),
        )
        .spacing(1)
        .split(area);

        let red = Style::default().fg(Color::Red);
        let yellow = Style::default().fg(Color::Yellow);
        let (record, record_accent) = match state {
            State::Idle => (
                Line::from(vec![Span::styled("● ", red), Span::raw("Запись")]),
                None,
            ),
            State::Recording => (
                Line::from(vec![
                    Span::raw("❚❚ Пауза  "),
                    Span::styled(
                        if blink_on(self.snapshot.duration) {
                            "●"
                        } else {
                            " "
                        },
                        red,
                    ),
                    Span::raw(format!(" {}", format_duration(self.snapshot.duration))),
                ]),
                Some(Color::Red),
            ),
            State::Paused => {
                let paused = self.paused_since.map_or(Duration::ZERO, |since| {
                    self.now.saturating_duration_since(since)
                });
                (
                    Line::from(vec![
                        Span::raw("▶ Продолжить  "),
                        Span::styled(if blink_on(paused) { "❚❚" } else { "  " }, yellow),
                        Span::raw(format!(" {}", format_duration(paused))),
                    ]),
                    Some(Color::Yellow),
                )
            }
            State::Saving => (Line::from("◌ Сохранение…"), None),
        };

        for (&(focus, _), &button_area) in widths.iter().zip(areas.iter()) {
            let (key, label, accent) = match focus {
                Focus::Record => ("пробел", record.clone(), record_accent),
                Focus::Save => ("s", Line::from("✔ Сохранить"), None),
                Focus::Delete => ("d", Line::from("⌫ Удалить запись"), None),
                Focus::Title | Focus::Quit => ("q", Line::from("✕ Выход"), None),
            };
            self.draw_button(frame, button_area, focus, key, label, accent);
        }
    }

    /// One button: a box with the key in its title. The focused button has a
    /// thick border; an unavailable one is dimmed and not clickable.
    fn draw_button(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        focus: Focus,
        key: &str,
        label: Line<'static>,
        accent: Option<Color>,
    ) {
        let enabled = match focus {
            Focus::Record => app::allows(self.snapshot.state, &record_command(self.snapshot.state)),
            Focus::Save | Focus::Delete => is_recording(self.snapshot.state),
            Focus::Title | Focus::Quit => true,
        };
        let focused = self.focus == focus && self.dialog.is_none();
        let dim = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM);
        let border = if !enabled {
            dim
        } else if let Some(color) = accent {
            Style::default().fg(color)
        } else {
            focus_style(focused)
        };
        let border = if focused {
            border.add_modifier(Modifier::BOLD)
        } else {
            border
        };
        let block = Block::bordered()
            .title(format!(" {key} "))
            .border_type(if focused {
                BorderType::Thick
            } else {
                BorderType::Rounded
            })
            .border_style(border);
        let text = if enabled {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        frame.render_widget(
            Paragraph::new(label)
                .style(text)
                .alignment(Alignment::Center)
                .block(block),
            area,
        );
        if enabled {
            self.targets.push((area, Target::Focus(focus)));
        }
    }

    /// The open dialog over the screen. Only its buttons are clickable.
    fn draw_dialog(&mut self, frame: &mut Frame<'_>, dialog: Dialog) {
        let DialogContent { title, color, text } = dialog_content(dialog);
        let name_field = dialog == Dialog::Name;

        let buttons_width: usize = dialog_buttons(dialog)
            .iter()
            .map(|(label, _, _)| label.chars().count() + DIALOG_BUTTON_GAP)
            .sum();
        let content_width = text
            .iter()
            .map(|line| line.chars().count())
            .chain([buttons_width, DIALOG_MIN_WIDTH])
            .max()
            .unwrap_or(DIALOG_MIN_WIDTH);
        // Borders, one cell of padding on each side.
        let width = u16::try_from(content_width + 4).unwrap_or(u16::MAX);
        // Borders, text, a blank line, the field and a blank line, buttons.
        let rows = 2 + text.len() + 1 + if name_field { 2 } else { 0 } + 1;
        let height = u16::try_from(rows).unwrap_or(u16::MAX);
        let area = centered(frame.area(), width, height);
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .title(title)
            .border_style(Style::default().fg(color));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line<'static>> = text
            .iter()
            .map(|line| Line::from(format!(" {line}")))
            .collect();
        lines.push(Line::default());
        let field_row = lines.len();
        let field_width = usize::from(inner.width).saturating_sub(2);
        let skip = self.caret.saturating_sub(field_width.saturating_sub(1));
        if name_field {
            let visible: String = self.title.chars().skip(skip).take(field_width).collect();
            let padding = field_width.saturating_sub(visible.chars().count());
            lines.push(Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("{visible}{}", " ".repeat(padding)),
                    Style::default().add_modifier(Modifier::UNDERLINED),
                ),
            ]));
            lines.push(Line::default());
        }
        let buttons_row = lines.len();
        let selected = Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED);
        let mut spans = vec![Span::raw(" ")];
        let mut areas = Vec::new();
        let mut x = inner.x.saturating_add(1);
        let y = inner
            .y
            .saturating_add(u16::try_from(buttons_row).unwrap_or(u16::MAX));
        for (index, &(label, button, _)) in dialog_buttons(dialog).iter().enumerate() {
            let style = match (index == self.selected, button) {
                (true, _) => selected,
                (false, DialogButton::Delete) => Style::default().fg(Color::Red),
                (false, _) => Style::default(),
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::raw(" ".repeat(DIALOG_BUTTON_GAP)));
            let label_width = text_width(label);
            areas.push((Rect::new(x, y, label_width, 1), Target::Dialog(button)));
            x = x.saturating_add(label_width + DIALOG_BUTTON_GAP_CELLS);
        }
        lines.push(Line::from(spans));
        frame.render_widget(Paragraph::new(lines), inner);

        self.targets.clear();
        self.targets.extend(
            areas
                .into_iter()
                .filter(|(rect, _)| inner.contains(rect.as_position())),
        );
        if name_field {
            let offset = u16::try_from(self.caret - skip).unwrap_or(u16::MAX);
            let x = inner.x.saturating_add(1).saturating_add(offset);
            let y = inner
                .y
                .saturating_add(u16::try_from(field_row).unwrap_or(u16::MAX));
            if inner.contains(Position::new(x, y)) {
                frame.set_cursor_position(Position::new(x, y));
            }
        }
    }
}

/// Title, border color and text of a dialog.
struct DialogContent {
    title: &'static str,
    color: Color,
    text: Vec<&'static str>,
}

fn dialog_content(dialog: Dialog) -> DialogContent {
    match dialog {
        Dialog::Quit => DialogContent {
            title: " Выход ",
            color: Color::Yellow,
            text: vec![
                "Запись не сохранена. Выйти?",
                "Она сохранится с пометкой «прервано».",
            ],
        },
        Dialog::Save => DialogContent {
            title: " Сохранение ",
            color: Color::Yellow,
            text: vec!["Запись на паузе. Сохранить её?"],
        },
        Dialog::Name => DialogContent {
            title: " Название встречи ",
            color: Color::Cyan,
            text: vec![
                "Название не задано. Введите его",
                "или оставьте поле пустым.",
            ],
        },
        Dialog::Delete => DialogContent {
            title: " Удаление ",
            color: Color::Red,
            text: vec!["Удалить запись? Это нельзя отменить."],
        },
    }
}

/// Buttons of a dialog, left to right: label, button, shortcut letters.
/// Deleting has no shortcut: it takes the selection and Enter or a click.
fn dialog_buttons(dialog: Dialog) -> &'static [(&'static str, DialogButton, &'static str)] {
    match dialog {
        Dialog::Quit => &[
            ("[y] Выйти", DialogButton::Leave, "yн"),
            ("[Esc] Остаться", DialogButton::Back, ""),
        ],
        Dialog::Save => &[
            ("[s] Сохранить", DialogButton::Save, "sы"),
            ("[Esc] Назад", DialogButton::Back, ""),
            ("Удалить", DialogButton::Delete, ""),
        ],
        Dialog::Name => &[
            ("[Enter] Сохранить", DialogButton::Save, ""),
            ("[Esc] Назад", DialogButton::Back, ""),
        ],
        Dialog::Delete => &[
            ("Удалить", DialogButton::Delete, ""),
            ("[Esc] Назад", DialogButton::Back, ""),
        ],
    }
}

fn is_recording(state: State) -> bool {
    matches!(state, State::Recording | State::Paused)
}

/// What the record key and button do in `state`: start when idle, pause or
/// resume otherwise.
fn record_command(state: State) -> Command {
    if state == State::Idle {
        Command::Start
    } else {
        Command::TogglePause
    }
}

/// Border of an element that can have the keyboard focus.
fn focus_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

/// The item after (or before) `current` in `items`, wrapping around.
fn cycle<T: Copy + PartialEq>(items: &[T], current: T, forward: bool) -> T {
    let count = items.len();
    let index = items.iter().position(|&item| item == current).unwrap_or(0);
    let next = if forward {
        index + 1
    } else {
        index + count.saturating_sub(1)
    };
    items.get(next % count.max(1)).copied().unwrap_or(current)
}

/// Whether a blinking mark is visible `elapsed` after it started blinking.
fn blink_on(elapsed: Duration) -> bool {
    (elapsed.as_millis() / BLINK.as_millis()).is_multiple_of(2)
}

/// Byte offset of the character at `index` (or the end) in `text`.
fn byte_index(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map_or(text.len(), |(i, _)| i)
}

fn text_width(text: &str) -> u16 {
    u16::try_from(text.chars().count()).unwrap_or(u16::MAX)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}
/// A device panel: the channel in the title, the device and its meter inside.
/// A missing device turns the panel red, a silent channel yellow.
fn render_channel(
    frame: &mut Frame<'_>,
    area: Rect,
    channel: Channel,
    view: &ChannelView,
    meter: &Meter,
) {
    let label = match channel {
        Channel::Mic => "Микрофон (L)",
        Channel::Them => "Собеседники (R)",
    };
    let missing = view.device == DeviceStatus::Missing;
    let highlight = if missing {
        Style::default().fg(Color::Red)
    } else if view.silent {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    let (description, name) = match &view.device {
        DeviceStatus::Unknown => ("…".to_owned(), String::new()),
        DeviceStatus::Missing => ("нет устройства".to_owned(), String::new()),
        DeviceStatus::Linked(device) => {
            let description = match channel {
                Channel::Mic => device.description.clone(),
                Channel::Them => format!("{} (монитор)", device.description),
            };
            (description, device.name.clone())
        }
    };
    let block = Block::bordered()
        .title(Span::styled(
            format!(" {label} "),
            highlight.add_modifier(Modifier::BOLD),
        ))
        .border_style(highlight);
    let bar_width = block.inner(area).width.saturating_sub(METER_EXTRA_WIDTH);

    let (bar, peak) = meter.bar(bar_width);
    let bar_style = if missing {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    let mut bar_spans = vec![Span::raw(" ▕")];
    for (i, cell) in bar.chars().enumerate() {
        let style = if Some(i) == peak {
            bar_style.add_modifier(Modifier::BOLD).fg(Color::LightGreen)
        } else {
            bar_style
        };
        let cell = if Some(i) == peak && cell != '█' {
            '│'
        } else {
            cell
        };
        bar_spans.push(Span::styled(cell.to_string(), style));
    }
    bar_spans.push(Span::raw("▏"));
    bar_spans.push(Span::raw(format!(" {:>4.0} dB", meter.level)));

    let lines = vec![
        Line::from(vec![Span::raw(" "), Span::styled(description, highlight)]),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(name, Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(bar_spans),
    ];
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn status_line(snapshot: &Snapshot) -> Line<'static> {
    let (text, style) = match snapshot.state {
        State::Idle => ("ОЖИДАНИЕ", Style::default()),
        State::Recording => (
            "● ЗАПИСЬ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        State::Paused => (
            "❚❚ ПАУЗА",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        State::Saving => ("СОХРАНЕНИЕ…", Style::default().fg(Color::Cyan)),
    };
    let mut spans = vec![Span::raw(" "), Span::styled(format!("{text:<10}"), style)];
    if snapshot.state != State::Idle || snapshot.path.is_some() {
        spans.push(Span::raw(format!(
            "  {}",
            format_duration(snapshot.duration)
        )));
    }
    if let Some(path) = &snapshot.path {
        spans.push(Span::raw(format!("   {}", app::display_path(path))));
    }
    Line::from(spans)
}

fn log_line(entry: &LogEntry) -> Line<'static> {
    let style = match entry.severity {
        Severity::Info => Style::default(),
        Severity::Warning => Style::default().fg(Color::Yellow),
        Severity::Error => Style::default().fg(Color::Red),
    };
    Line::from(vec![
        Span::styled(
            format!(" {}  ", entry.at.format("%H:%M:%S")),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(entry.text.clone(), style),
    ])
}

/// Answer to the start-up question about unfinished recordings (spec 6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryChoice {
    /// `y`: decode through errors and encode again.
    Recover,
    /// `d`: delete them.
    Delete,
    /// `l`: leave them for later.
    Later,
    /// Ctrl+C or `q`: exit without touching them.
    Quit,
}

#[must_use]
pub fn recovery_choice(key: KeyEvent) -> Option<RecoveryChoice> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Some(RecoveryChoice::Quit);
    }
    let KeyCode::Char(c) = key.code else {
        return None;
    };
    match c.to_lowercase().next().unwrap_or(c) {
        'y' | 'н' => Some(RecoveryChoice::Recover),
        'd' | 'в' => Some(RecoveryChoice::Delete),
        'l' | 'д' => Some(RecoveryChoice::Later),
        'q' | 'й' => Some(RecoveryChoice::Quit),
        _ => None,
    }
}

/// The start-up dialog listing unfinished recordings. With `progress`, the
/// keys are hidden and the progress text is shown instead.
pub fn draw_recovery(frame: &mut Frame<'_>, files: &[std::path::PathBuf], progress: Option<&str>) {
    let block = Block::bordered()
        .title(" callrec: незавершённые записи ")
        .border_style(Style::default().fg(Color::Yellow));
    let inner = block.inner(frame.area());
    frame.render_widget(block, frame.area());

    let mut lines = vec![
        Line::default(),
        Line::from("  Найдены записи, прерванные аварийным завершением:"),
        Line::default(),
    ];
    for file in files {
        lines.push(Line::from(format!("    {}", app::display_path(file))));
    }
    lines.push(Line::default());
    match progress {
        Some(text) => lines.push(Line::styled(
            format!("  {text}"),
            Style::default().fg(Color::Cyan),
        )),
        None => lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "[y] Восстановить",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
            Span::styled("[d] Удалить", Style::default().fg(Color::Red)),
            Span::raw("   "),
            Span::raw("[l] Оставить на потом"),
        ])),
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

/// `HH:MM:SS`.
fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pw::Device;
    use chrono::{Local, TimeZone};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::PathBuf;

    fn view(device: DeviceStatus, level_db: f64) -> ChannelView {
        ChannelView {
            device,
            level_db,
            silent: false,
        }
    }

    fn linked(name: &str, description: &str) -> DeviceStatus {
        DeviceStatus::Linked(Device {
            name: name.into(),
            description: description.into(),
        })
    }

    fn snapshot(state: State) -> Snapshot {
        Snapshot {
            state,
            duration: Duration::from_secs(23 * 60 + 41),
            path: (state != State::Idle).then(|| PathBuf::from("/calls/2026-10-21_14-00.flac")),
            mic: view(linked("bluez_input.70", "Jabra Evolve2 65 Mono"), -18.0),
            them: view(linked("bluez_output.70", "Jabra Evolve2 65"), -31.0),
            log: vec![LogEntry {
                at: Local.with_ymd_and_hms(2026, 10, 21, 14, 23, 5).unwrap(),
                text: "Микрофон: Встроенный микрофон → Jabra Evolve2 65 Mono".into(),
                severity: Severity::Info,
            }],
        }
    }

    fn render(ui: &mut Ui) -> Terminal<TestBackend> {
        render_sized(ui, 80, 24)
    }

    fn render_sized(ui: &mut Ui, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        terminal
    }

    fn entry(text: String) -> LogEntry {
        LogEntry {
            at: Local.with_ymd_and_hms(2026, 10, 21, 14, 23, 5).unwrap(),
            text,
            severity: Severity::Info,
        }
    }

    fn lines(terminal: &Terminal<TestBackend>) -> Vec<String> {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// Position of `text` on the screen.
    fn find(terminal: &Terminal<TestBackend>, text: &str) -> (u16, u16) {
        for (y, line) in lines(terminal).iter().enumerate() {
            if let Some(byte) = line.find(text) {
                let x = line[..byte].chars().count();
                return (u16::try_from(x).unwrap(), u16::try_from(y).unwrap());
            }
        }
        panic!("{text:?} not on screen:\n{}", lines(terminal).join("\n"));
    }

    fn style_at(terminal: &Terminal<TestBackend>, (x, y): (u16, u16)) -> Style {
        terminal.backend().buffer().cell((x, y)).unwrap().style()
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn click((column, row): (u16, u16)) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn code(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[allow(clippy::unnecessary_wraps)]
    fn command(command: Command) -> Option<UiAction> {
        Some(UiAction::Command(command))
    }

    fn set_title(title: &str) -> Option<UiAction> {
        command(Command::SetTitle(title.into()))
    }

    /// Row of the button panel's top border.
    fn buttons_top(terminal: &Terminal<TestBackend>) -> String {
        let rows = lines(terminal);
        rows[rows.len() - usize::from(BUTTONS_HEIGHT)].clone()
    }

    #[test]
    fn recording_screen() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        let terminal = render_sized(&mut ui, 120, 30);
        let screen = lines(&terminal).join("\n");
        for text in [
            "callrec",
            "Микрофон (L)",
            "Jabra Evolve2 65 Mono",
            "bluez_input.70",
            "-18 dB",
            "Собеседники (R)",
            "Jabra Evolve2 65 (монитор)",
            "-31 dB",
            "Название встречи",
            "без названия",
            "● ЗАПИСЬ    00:23:41   /calls/2026-10-21_14-00.flac",
            "Журнал",
            "14:23:05  Микрофон: Встроенный микрофон → Jabra Evolve2 65 Mono",
            " пробел ",
            "❚❚ Пауза  ● 00:23:41",
            "✔ Сохранить",
            "⌫ Удалить запись",
            "❚❚ Пауза",
            "✕ Выход",
        ] {
            assert!(screen.contains(text), "{text:?} missing in:\n{screen}");
        }
    }

    #[test]
    fn narrow_screen_stacks_devices() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        let terminal = render_sized(&mut ui, SIDE_BY_SIDE_MIN_WIDTH - 1, 30);
        let (mic_x, mic_y) = find(&terminal, "Микрофон (L)");
        let (them_x, them_y) = find(&terminal, "Собеседники (R)");
        assert_eq!(mic_x, them_x);
        assert_eq!(them_y, mic_y + DEVICE_PANEL_HEIGHT);
    }

    #[test]
    fn wide_screen_puts_devices_side_by_side() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        let terminal = render_sized(&mut ui, SIDE_BY_SIDE_MIN_WIDTH, 30);
        let (mic_x, mic_y) = find(&terminal, "Микрофон (L)");
        let (them_x, them_y) = find(&terminal, "Собеседники (R)");
        assert_eq!(mic_y, them_y);
        assert!(them_x >= SIDE_BY_SIDE_MIN_WIDTH / 2 && mic_x < them_x);
    }

    #[test]
    fn meters_stretch_with_the_screen() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        let narrow = find(&render_sized(&mut ui, 80, 30), "-18 dB").0;
        let wide = find(&render_sized(&mut ui, 160, 30), "-18 dB").0;
        assert_eq!(narrow, 80 - 8);
        assert_eq!(wide, 80 - 8);
        let wide = find(&render_sized(&mut ui, 240, 30), "-18 dB").0;
        assert_eq!(wide, 120 - 8);
    }

    #[test]
    fn buttons_stay_at_the_bottom() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        for height in [24, 50] {
            let terminal = render_sized(&mut ui, 80, height);
            let (_, y) = find(&terminal, "❚❚ Пауза");
            assert_eq!(y, height - 2);
        }
    }

    #[test]
    fn log_fills_the_middle_with_the_latest_events() {
        let mut s = snapshot(State::Recording);
        s.log = (1..=40).map(|i| entry(format!("событие {i}"))).collect();
        let mut ui = Ui::new(s, Instant::now());

        let terminal = render_sized(&mut ui, 80, 40);
        let screen = lines(&terminal).join("\n");
        // 40 rows: 10 for devices, 3 status, 3 buttons, 2 log borders.
        assert!(screen.contains("событие 40"));
        assert!(screen.contains("событие 19"));
        assert!(!screen.contains("событие 18 "), "{screen}");
        let (_, last) = find(&terminal, "событие 40");
        assert_eq!(last, 40 - BUTTONS_HEIGHT - 2);
    }

    #[test]
    fn status_texts() {
        for (state, status, button) in [
            (State::Idle, "ОЖИДАНИЕ", "● Запись"),
            (State::Paused, "❚❚ ПАУЗА", "▶ Продолжить"),
            (State::Saving, "СОХРАНЕНИЕ…", "◌ Сохранение…"),
        ] {
            let mut ui = Ui::new(snapshot(state), Instant::now());
            let terminal = render(&mut ui);
            find(&terminal, status);
            find(&terminal, button);
        }
    }

    #[test]
    fn recording_mark_blinks_with_the_timer() {
        let mut s = snapshot(State::Recording);
        let mut ui = Ui::new(s.clone(), Instant::now());
        find(&render(&mut ui), "❚❚ Пауза  ● 00:23:41");
        s.duration += BLINK;
        ui.update(s, Instant::now());
        find(&render(&mut ui), "❚❚ Пауза    00:23:41");
    }

    #[test]
    fn pause_button_shows_how_long_the_pause_lasts() {
        let start = Instant::now();
        let mut ui = Ui::new(snapshot(State::Paused), start);
        find(&render(&mut ui), "▶ Продолжить  ❚❚ 00:00:00");
        ui.update(snapshot(State::Paused), start + Duration::from_secs(75));
        find(&render(&mut ui), "▶ Продолжить  ❚❚ 00:01:15");
        ui.update(
            snapshot(State::Paused),
            start + Duration::from_millis(75_500),
        );
        find(&render(&mut ui), "▶ Продолжить     00:01:15");
        // A new pause starts from zero.
        ui.update(snapshot(State::Recording), start + Duration::from_secs(80));
        ui.update(snapshot(State::Paused), start + Duration::from_secs(90));
        find(&render(&mut ui), "▶ Продолжить  ❚❚ 00:00:00");
    }

    #[test]
    fn unavailable_buttons_are_dimmed_and_inert() {
        let mut ui = Ui::new(snapshot(State::Saving), Instant::now());
        let terminal = render(&mut ui);
        let record = find(&terminal, "◌ Сохранение…");
        assert!(
            style_at(&terminal, record)
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert_eq!(ui.mouse(click(record)), None);
        assert_eq!(ui.key(key(' ')), None);

        let mut ui = Ui::new(snapshot(State::Idle), Instant::now());
        let terminal = render(&mut ui);
        assert_eq!(
            ui.mouse(click(find(&terminal, "Запись"))),
            command(Command::Start)
        );
    }

    fn screen(ui: &mut Ui) -> String {
        lines(&render(ui)).join("\n")
    }

    #[test]
    fn save_and_delete_exist_only_with_a_recording() {
        let mut ui = Ui::new(snapshot(State::Idle), Instant::now());
        let idle = screen(&mut ui);
        assert!(
            !idle.contains("Сохранить") && !idle.contains("Удалить"),
            "{idle}"
        );
        assert_eq!(ui.key(key('s')), None);
        assert_eq!(ui.key(key('d')), None);
        assert!(!screen(&mut ui).contains("Сохранить её?"));

        for state in [State::Recording, State::Paused] {
            let mut ui = Ui::new(snapshot(state), Instant::now());
            let screen = screen(&mut ui);
            assert!(screen.contains("✔ Сохранить"), "{screen}");
            assert!(screen.contains("⌫ Удалить запись"), "{screen}");
        }
    }

    #[test]
    fn space_starts_and_pauses() {
        for (state, expected) in [
            (State::Idle, command(Command::Start)),
            (State::Recording, command(Command::TogglePause)),
            (State::Paused, command(Command::TogglePause)),
            (State::Saving, None),
        ] {
            let mut ui = Ui::new(snapshot(state), Instant::now());
            assert_eq!(ui.key(key(' ')), expected, "{state:?}");
        }
        let mut ui = Ui::new(snapshot(State::Idle), Instant::now());
        assert_eq!(ui.key(key('p')), None);
    }

    #[test]
    fn keys_work_on_russian_layout() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        // Only the keys written on the buttons work; `p` is not one of them.
        for c in ['p', 'з', 'P', 'x'] {
            assert_eq!(ui.key(key(c)), None);
        }
        assert_eq!(ui.key(key('ы')), command(Command::Pause));
        assert!(screen(&mut ui).contains("Сохранить её?"));
        ui.key(code(KeyCode::Esc));
        assert_eq!(ui.key(key('в')), command(Command::Pause));
        assert!(screen(&mut ui).contains("Удалить запись? Это нельзя отменить."));
        ui.key(code(KeyCode::Esc));
        assert_eq!(ui.key(key('й')), None);
        assert!(screen(&mut ui).contains("Запись не сохранена. Выйти?"));
        ui.key(code(KeyCode::Esc));
        assert_eq!(ui.key(key('к')), None);
        assert_eq!(ui.key(key('ы')), set_title("ы"));
    }

    #[test]
    fn save_button_pauses_and_asks() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        let terminal = render(&mut ui);
        assert_eq!(
            ui.mouse(click(find(&terminal, "✔ Сохранить"))),
            command(Command::Pause)
        );
        let screen = screen(&mut ui);
        assert!(
            screen.contains("Запись на паузе. Сохранить её?"),
            "{screen}"
        );
        assert!(
            screen.contains("[s] Сохранить   [Esc] Назад   Удалить"),
            "{screen}"
        );
        // Other keys do nothing while the dialog is open.
        for c in [' ', 'p', 'q', 'r', 'd'] {
            assert_eq!(ui.key(key(c)), None);
        }
        assert!(screen_has(&mut ui, "Сохранить её?"));
    }

    fn screen_has(ui: &mut Ui, text: &str) -> bool {
        screen(ui).contains(text)
    }

    #[test]
    fn titled_recording_is_saved_at_once() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(key('r'));
        ui.key(key('Р'));
        ui.key(code(KeyCode::Enter));
        ui.key(key('s'));
        // "Save" is preselected.
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::Save));
        assert!(!screen_has(&mut ui, "Сохранить её?"));

        let mut ui = Ui::new(snapshot(State::Paused), Instant::now());
        ui.key(key('r'));
        ui.key(key('Р'));
        ui.key(code(KeyCode::Enter));
        ui.key(key('s'));
        assert_eq!(ui.key(key('s')), command(Command::Save));
    }

    #[test]
    fn untitled_recording_is_offered_a_name() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(key('s'));
        assert_eq!(ui.key(code(KeyCode::Enter)), None);
        find(&render(&mut ui), "Название не задано. Введите его");
        // Letters are text here, not answers.
        assert_eq!(ui.key(key('n')), set_title("n"));
        assert_eq!(ui.key(key('s')), set_title("ns"));
        let mut terminal = render(&mut ui);
        let (x, y) = find(&terminal, "Название не задано");
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            // The field is three rows below the first line of text.
            Position::new(x + 2, y + 3)
        );
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::Save));
    }

    #[test]
    fn name_can_stay_empty() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(key('s'));
        ui.key(code(KeyCode::Enter));
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::Save));

        // A blank title counts as none.
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(key('r'));
        ui.key(key(' '));
        ui.key(code(KeyCode::Enter));
        ui.key(key('s'));
        ui.key(code(KeyCode::Enter));
        find(&render(&mut ui), "Название не задано. Введите его");
    }

    #[test]
    fn back_closes_and_leaves_the_recording_paused() {
        // "Back" in the save dialog, by arrow and Enter or by Esc.
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(key('s'));
        ui.key(code(KeyCode::Right));
        assert_eq!(ui.key(code(KeyCode::Enter)), None);
        assert!(!screen_has(&mut ui, "Сохранить её?"));
        ui.key(key('s'));
        assert_eq!(ui.key(code(KeyCode::Esc)), None);
        assert!(!screen_has(&mut ui, "Сохранить её?"));

        // Esc in the name prompt closes it too; it does not go back.
        ui.key(key('s'));
        ui.key(code(KeyCode::Enter));
        assert_eq!(ui.key(code(KeyCode::Esc)), None);
        let screen = screen(&mut ui);
        assert!(!screen.contains("Название не задано"), "{screen}");
        assert!(!screen.contains("Сохранить её?"), "{screen}");
        // Paused, so space resumes.
        ui.update(snapshot(State::Paused), Instant::now());
        assert_eq!(ui.key(key(' ')), command(Command::TogglePause));
    }

    #[test]
    fn delete_in_save_dialog_opens_delete_dialog() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(key('s'));
        ui.key(code(KeyCode::Left));
        assert_eq!(ui.key(code(KeyCode::Enter)), None);
        let screen = screen(&mut ui);
        assert!(
            screen.contains("Удалить запись? Это нельзя отменить."),
            "{screen}"
        );
        assert!(screen.contains("Удалить   [Esc] Назад"), "{screen}");
        // "Back" is preselected and closes; the save dialog does not return.
        assert_eq!(ui.key(code(KeyCode::Enter)), None);
        let screen = screen_text(&mut ui);
        assert!(!screen.contains("Удалить запись?"), "{screen}");
        assert!(!screen.contains("Сохранить её?"), "{screen}");
    }

    fn screen_text(ui: &mut Ui) -> String {
        screen(ui)
    }

    #[test]
    fn delete_button_pauses_and_asks() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        assert_eq!(ui.key(key('d')), command(Command::Pause));
        assert!(screen_has(&mut ui, "Удалить запись? Это нельзя отменить."));
        // No shortcut deletes.
        assert_eq!(ui.key(key('d')), None);
        assert_eq!(ui.key(key('y')), None);
        assert_eq!(ui.key(code(KeyCode::Left)), None);
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::Discard));
    }

    #[test]
    fn dialog_buttons_are_clickable() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(key('s'));
        let terminal = render(&mut ui);
        // Buttons under the dialog do not react.
        assert_eq!(ui.mouse(click(find(&terminal, "❚❚ Пауза"))), None);
        let (x, y) = find(&terminal, "[Esc] Назад   Удалить");
        assert_eq!(ui.mouse(click((x + 14, y))), None);
        let terminal = render(&mut ui);
        let (x, y) = find(&terminal, "Удалить   [Esc] Назад");
        assert_eq!(ui.mouse(click((x, y))), command(Command::Discard));
    }

    #[test]
    fn dialog_closes_when_recording_ends() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(key('s'));
        ui.update(snapshot(State::Idle), Instant::now());
        assert!(!screen_has(&mut ui, "Сохранить её?"));
        assert_eq!(ui.key(key(' ')), command(Command::Start));
    }

    #[test]
    fn enter_activates_the_focused_button() {
        let mut ui = Ui::new(snapshot(State::Idle), Instant::now());
        // The record button has the focus from the start.
        assert!(buttons_top(&render(&mut ui)).starts_with('┏'));
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::Start));

        ui.update(snapshot(State::Recording), Instant::now());
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::TogglePause));
        assert_eq!(ui.key(code(KeyCode::Right)), None);
        let top = buttons_top(&render(&mut ui));
        assert!(top.starts_with('╭') && top.contains('┏'), "{top}");
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::Pause));
        assert!(screen_has(&mut ui, "Сохранить её?"));
        ui.key(code(KeyCode::Esc));

        ui.key(code(KeyCode::Right));
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::Pause));
        assert!(screen_has(&mut ui, "Удалить запись?"));
        ui.key(code(KeyCode::Esc));

        // The row ends at the quit button.
        ui.key(code(KeyCode::Right));
        ui.key(code(KeyCode::Right));
        assert_eq!(ui.key(code(KeyCode::Enter)), None);
        assert!(screen_has(&mut ui, "Запись не сохранена. Выйти?"));
    }

    #[test]
    fn left_stops_at_the_first_button() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(code(KeyCode::Left));
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::TogglePause));
    }

    #[test]
    fn focus_skips_and_leaves_hidden_buttons() {
        let mut ui = Ui::new(snapshot(State::Idle), Instant::now());
        ui.key(code(KeyCode::Right));
        assert_eq!(ui.key(code(KeyCode::Enter)), Some(UiAction::Quit));

        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(code(KeyCode::Right));
        ui.update(snapshot(State::Idle), Instant::now());
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::Start));
    }

    #[test]
    fn tab_cycles_through_buttons_and_title() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        for _ in 0..4 {
            ui.key(code(KeyCode::Tab));
        }
        // Now in the title field: letters are text, not shortcuts.
        assert_eq!(ui.key(key('p')), set_title("p"));
        assert_eq!(ui.key(key(' ')), set_title("p "));
        ui.key(code(KeyCode::Tab));
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::TogglePause));
        ui.key(code(KeyCode::BackTab));
        assert_eq!(ui.key(key('q')), set_title("p q"));

        // Idle: from the record button straight to quit.
        let mut ui = Ui::new(snapshot(State::Idle), Instant::now());
        ui.key(code(KeyCode::Tab));
        assert_eq!(ui.key(code(KeyCode::Enter)), Some(UiAction::Quit));
    }
    #[test]
    fn title_is_edited_at_the_caret() {
        let mut ui = Ui::new(snapshot(State::Idle), Instant::now());
        ui.key(code(KeyCode::Up));
        assert_eq!(ui.key(key('П')), set_title("П"));
        assert_eq!(ui.key(key('л')), set_title("Пл"));
        assert_eq!(ui.key(key('а')), set_title("Пла"));
        ui.key(code(KeyCode::Left));
        ui.key(code(KeyCode::Left));
        assert_eq!(ui.key(key('x')), set_title("Пxла"));
        assert_eq!(ui.key(code(KeyCode::Backspace)), set_title("Пла"));
        ui.key(code(KeyCode::Home));
        assert_eq!(ui.key(code(KeyCode::Backspace)), None);
        assert_eq!(ui.key(code(KeyCode::Delete)), set_title("ла"));
        ui.key(code(KeyCode::End));
        assert_eq!(ui.key(code(KeyCode::Delete)), None);
        assert_eq!(ui.key(key('!')), set_title("ла!"));

        // Enter leaves the field; the shortcuts work again.
        assert_eq!(ui.key(code(KeyCode::Enter)), None);
        assert_eq!(ui.key(key(' ')), command(Command::Start));
    }

    #[test]
    fn title_field_shows_text_and_caret() {
        let mut ui = Ui::new(snapshot(State::Idle), Instant::now());
        ui.key(code(KeyCode::Up));
        for c in "Ретро".chars() {
            ui.key(key(c));
        }
        ui.key(code(KeyCode::Left));
        let mut terminal = render(&mut ui);
        let (x, y) = find(&terminal, "Ретро");
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            Position::new(x + 4, y)
        );
        // Without the focus there is no cursor, the text stays.
        ui.key(code(KeyCode::Esc));
        let terminal = render(&mut ui);
        find(&terminal, "Ретро");
    }

    #[test]
    fn long_title_scrolls_and_is_limited() {
        let mut ui = Ui::new(snapshot(State::Idle), Instant::now());
        ui.key(code(KeyCode::Up));
        for _ in 0..MAX_TITLE_CHARS - 1 {
            ui.key(key('a'));
        }
        assert_eq!(
            ui.key(key('z')),
            set_title(&format!("{}z", "a".repeat(MAX_TITLE_CHARS - 1)))
        );
        assert_eq!(ui.key(key('x')), None);
        // The end with the caret is visible in a field narrower than the title.
        find(&render(&mut ui), "aaz");
    }

    #[test]
    fn click_on_title_focuses_it() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        let terminal = render(&mut ui);
        assert_eq!(ui.mouse(click(find(&terminal, "без названия"))), None);
        assert_eq!(ui.key(key('q')), set_title("q"));
    }

    #[test]
    fn ctrl_c_quits_while_editing_title() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(code(KeyCode::Up));
        assert_eq!(
            ui.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(UiAction::Quit)
        );
    }

    #[test]
    fn quit_while_idle_is_immediate() {
        let mut ui = Ui::new(snapshot(State::Idle), Instant::now());
        assert_eq!(ui.key(key('q')), Some(UiAction::Quit));
    }

    #[test]
    fn quit_while_recording_asks_first() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        assert_eq!(ui.key(key('q')), None);
        find(&render(&mut ui), "Она сохранится с пометкой «прервано».");
        // Other commands are blocked while the question is open; "stay" is
        // preselected.
        assert_eq!(ui.key(key(' ')), None);
        assert_eq!(ui.key(key('p')), None);
        assert_eq!(ui.key(code(KeyCode::Enter)), None);
        assert_eq!(ui.key(key(' ')), command(Command::TogglePause));

        ui.key(key('q'));
        assert_eq!(ui.key(key('n')), None, "n is not a button key");
        assert!(screen(&mut ui).contains("Запись не сохранена. Выйти?"));
        ui.key(code(KeyCode::Esc));
        ui.key(key('q'));
        ui.key(code(KeyCode::Left));
        assert_eq!(ui.key(code(KeyCode::Enter)), Some(UiAction::Quit));

        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(key('q'));
        let terminal = render(&mut ui);
        assert_eq!(
            ui.mouse(click(find(&terminal, "[y] Выйти"))),
            Some(UiAction::Quit)
        );
    }

    #[test]
    fn escape_cancels_quit() {
        let mut ui = Ui::new(snapshot(State::Paused), Instant::now());
        ui.key(key('q'));
        assert_eq!(ui.key(code(KeyCode::Esc)), None);
        assert_eq!(ui.key(key(' ')), command(Command::TogglePause));
    }

    #[test]
    fn space_never_resumes_from_a_dialog() {
        for state in [State::Recording, State::Paused] {
            for opener in ['s', 'd', 'q'] {
                let mut ui = Ui::new(snapshot(state), Instant::now());
                ui.key(key(opener));
                let before = screen(&mut ui);
                assert_eq!(ui.key(key(' ')), None, "{state:?} {opener}");
                assert_eq!(screen(&mut ui), before);
            }
            // In the name prompt a space is text, not a command.
            let mut ui = Ui::new(snapshot(state), Instant::now());
            ui.key(key('s'));
            ui.key(code(KeyCode::Enter));
            assert_eq!(ui.key(key(' ')), set_title(" "));
        }
    }

    fn release(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Release)
    }

    fn repeat(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind(code, KeyModifiers::NONE, KeyEventKind::Repeat)
    }

    /// The UI in a terminal that reports key releases.
    fn with_releases(state: State) -> Ui {
        let mut ui = Ui::new(snapshot(state), Instant::now());
        ui.set_enter_on_release(true);
        ui
    }

    #[test]
    fn enter_acts_on_release_where_reported() {
        let mut ui = with_releases(State::Recording);
        ui.key(code(KeyCode::Right));
        // Pressed and held: nothing yet.
        assert_eq!(ui.key(code(KeyCode::Enter)), None);
        for _ in 0..20 {
            assert_eq!(ui.key(repeat(KeyCode::Enter)), None);
        }
        assert!(!screen(&mut ui).contains("Сохранить её?"));
        // Released: the save button.
        assert_eq!(ui.key(release(KeyCode::Enter)), command(Command::Pause));
        assert!(screen(&mut ui).contains("Сохранить её?"));
    }

    #[test]
    fn enter_release_on_another_screen_does_nothing() {
        // Delete selected in the save dialog. The release opens the delete
        // dialog; the next release without a new press does nothing there.
        let mut ui = with_releases(State::Paused);
        ui.key(key('s'));
        ui.key(code(KeyCode::Left));
        ui.key(code(KeyCode::Enter));
        assert_eq!(ui.key(release(KeyCode::Enter)), None);
        assert!(screen(&mut ui).contains("Удалить запись? Это нельзя отменить."));
        assert_eq!(ui.key(release(KeyCode::Enter)), None);
        assert!(screen(&mut ui).contains("Удалить запись?"));

        // Enter pressed in the save dialog, released after the dialog was
        // closed by a click: the release belongs to the closed dialog.
        let mut ui = with_releases(State::Paused);
        ui.key(key('s'));
        ui.key(code(KeyCode::Enter));
        ui.key(code(KeyCode::Esc));
        assert_eq!(ui.key(release(KeyCode::Enter)), None);
        assert!(!screen(&mut ui).contains("Сохранить её?"));
    }

    #[test]
    fn enter_acts_on_press_where_releases_are_not_reported() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(code(KeyCode::Right));
        assert_eq!(ui.key(code(KeyCode::Enter)), command(Command::Pause));
    }

    #[test]
    fn other_keys_act_on_press_and_repeat() {
        let mut ui = with_releases(State::Recording);
        assert_eq!(ui.key(key(' ')), command(Command::TogglePause));
        assert_eq!(
            ui.key(KeyEvent::new_with_kind(
                KeyCode::Char(' '),
                KeyModifiers::NONE,
                KeyEventKind::Repeat
            )),
            command(Command::TogglePause)
        );
        assert_eq!(ui.key(release(KeyCode::Char(' '))), None);
    }

    #[test]
    fn ctrl_c_quits_from_any_dialog() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(key('s'));
        assert_eq!(
            ui.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(UiAction::Quit)
        );
    }
    #[test]
    fn ctrl_c_quits_without_asking() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        assert_eq!(
            ui.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(UiAction::Quit)
        );
    }
    #[test]
    fn missing_device_is_red() {
        let mut s = snapshot(State::Recording);
        s.mic = view(DeviceStatus::Missing, LEVEL_FLOOR_DB);
        let mut ui = Ui::new(s, Instant::now());
        let terminal = render(&mut ui);
        let at = find(&terminal, "нет устройства");
        assert_eq!(style_at(&terminal, at).fg, Some(Color::Red));
    }

    #[test]
    fn silent_channel_is_yellow() {
        let mut s = snapshot(State::Recording);
        s.them.silent = true;
        let mut ui = Ui::new(s, Instant::now());
        let terminal = render(&mut ui);
        let at = find(&terminal, "Собеседники (R)");
        assert_eq!(style_at(&terminal, at).fg, Some(Color::Yellow));
    }

    #[test]
    fn meter_holds_peak_then_falls() {
        let start = Instant::now();
        let mut meter = Meter::new(start);
        meter.update(-10.0, start);
        let half = start + Duration::from_millis(500);
        meter.update(-60.0, half);
        // Falls smoothly, the peak stays.
        assert!((meter.level - (-25.0)).abs() < 1e-9, "{}", meter.level);
        assert!((meter.peak - (-10.0)).abs() < f64::EPSILON);

        let later = start + Duration::from_millis(1500);
        meter.update(-60.0, later);
        assert!((meter.level - (-55.0)).abs() < 1e-9, "{}", meter.level);
        assert!(meter.peak < -10.0);
        assert!(meter.peak >= meter.level);
    }

    #[test]
    fn meter_bar_scale() {
        let now = Instant::now();
        let mut meter = Meter::new(now);
        assert_eq!(meter.bar(4), ("▁▁▁▁".to_owned(), None));
        meter.update(-30.0, now);
        assert_eq!(meter.bar(4), ("██▁▁".to_owned(), Some(1)));
        meter.update(0.0, now);
        assert_eq!(meter.bar(4), ("████".to_owned(), Some(3)));
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(Duration::from_secs(0)), "00:00:00");
        assert_eq!(
            format_duration(Duration::from_secs(23 * 60 + 41)),
            "00:23:41"
        );
        assert_eq!(
            format_duration(Duration::from_secs(3 * 3600 + 5)),
            "03:00:05"
        );
    }

    #[test]
    fn recovery_dialog() {
        let files = [PathBuf::from("/calls/2026-10-21_14-00.flac.part")];
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| draw_recovery(frame, &files, None))
            .unwrap();
        find(&terminal, "/calls/2026-10-21_14-00.flac.part");
        find(
            &terminal,
            "[y] Восстановить   [d] Удалить   [l] Оставить на потом",
        );
        terminal
            .draw(|frame| draw_recovery(frame, &files, Some("Восстановление…")))
            .unwrap();
        find(&terminal, "Восстановление…");

        assert_eq!(recovery_choice(key('y')), Some(RecoveryChoice::Recover));
        assert_eq!(recovery_choice(key('в')), Some(RecoveryChoice::Delete));
        assert_eq!(recovery_choice(key('l')), Some(RecoveryChoice::Later));
        assert_eq!(recovery_choice(key('x')), None);
        assert_eq!(
            recovery_choice(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(RecoveryChoice::Quit)
        );
    }

    #[test]
    fn small_terminal_does_not_panic() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        ui.key(key('q'));
        for (w, h) in [(1, 1), (10, 3), (30, 8)] {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|frame| ui.draw(frame)).unwrap();
        }
    }
}
