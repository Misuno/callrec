//! Terminal UI: the screen from spec 3, keys, mouse, dimmed unavailable
//! buttons and level meters with peak hold (spec 5.5).
//!
//! [`Ui`] only turns snapshots into frames and input into [`UiAction`]s; the
//! terminal and the event loop are set up in `main.rs`.

use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{self, ChannelView, Command, DeviceStatus, LogEntry, Severity, Snapshot, State};
use crate::mixer::{Channel, LEVEL_FLOOR_DB};

/// Lowest level the meter shows; quieter is an empty bar.
pub const METER_MIN_DB: f64 = -60.0;
/// How fast the meter falls after the level drops.
pub const METER_DECAY_DB_PER_SEC: f64 = 30.0;
/// How long the peak mark stays before it falls (spec 5.5).
pub const PEAK_HOLD: Duration = Duration::from_secs(1);
/// Width of the label column ("Собеседники (R) ").
const LABEL_WIDTH: usize = 17;

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiAction {
    Command(Command),
    /// Save the recording, if any, and exit.
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Button {
    Command(Command),
    Quit,
    ConfirmYes,
    ConfirmNo,
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
    buttons: Vec<(Rect, Button)>,
    confirm_quit: bool,
}

impl Ui {
    #[must_use]
    pub fn new(snapshot: Snapshot, now: Instant) -> Self {
        let mut ui = Self {
            snapshot: snapshot.clone(),
            mic: Meter::new(now),
            them: Meter::new(now),
            buttons: Vec::new(),
            confirm_quit: false,
        };
        ui.update(snapshot, now);
        ui
    }

    pub fn update(&mut self, snapshot: Snapshot, now: Instant) {
        self.mic.update(snapshot.mic.level_db, now);
        self.them.update(snapshot.them.level_db, now);
        if !is_recording(snapshot.state) {
            // Nothing left to confirm: the recording ended on its own.
            self.confirm_quit = false;
        }
        self.snapshot = snapshot;
    }

    pub fn key(&mut self, key: KeyEvent) -> Option<UiAction> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Ctrl+C saves and exits without asking (spec 6.7).
            return Some(UiAction::Quit);
        }
        let KeyCode::Char(c) = key.code else {
            return (key.code == KeyCode::Esc)
                .then(|| self.press(Button::ConfirmNo))
                .flatten();
        };
        // The same keys on the Russian layout.
        let button = match c.to_lowercase().next().unwrap_or(c) {
            'r' | 'к' => Button::Command(Command::Start),
            'p' | 'з' => Button::Command(Command::TogglePause),
            's' | 'ы' => Button::Command(Command::Stop),
            'q' | 'й' => Button::Quit,
            'y' | 'н' => Button::ConfirmYes,
            'n' | 'т' => Button::ConfirmNo,
            _ => return None,
        };
        self.press(button)
    }

    pub fn mouse(&mut self, mouse: MouseEvent) -> Option<UiAction> {
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        let position = Position::new(mouse.column, mouse.row);
        let button = self
            .buttons
            .iter()
            .find(|(area, _)| area.contains(position))
            .map(|(_, button)| *button)?;
        self.press(button)
    }

    fn press(&mut self, button: Button) -> Option<UiAction> {
        if self.confirm_quit {
            return match button {
                Button::ConfirmYes | Button::Quit => Some(UiAction::Quit),
                Button::ConfirmNo => {
                    self.confirm_quit = false;
                    None
                }
                Button::Command(_) => None,
            };
        }
        match button {
            Button::Command(command) => {
                app::allows(self.snapshot.state, command).then_some(UiAction::Command(command))
            }
            Button::Quit if is_recording(self.snapshot.state) => {
                self.confirm_quit = true;
                None
            }
            Button::Quit => Some(UiAction::Quit),
            Button::ConfirmYes | Button::ConfirmNo => None,
        }
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>) {
        self.buttons.clear();
        let block = Block::bordered().title(" callrec ");
        let inner = block.inner(frame.area());
        frame.render_widget(block, frame.area());

        let bar_width = usize::from(inner.width)
            .saturating_sub(2 + LABEL_WIDTH + 2 + 8)
            .clamp(4, 40);
        let bar_width = u16::try_from(bar_width).unwrap_or(4);
        let snapshot = &self.snapshot;
        let mut lines = vec![Line::default()];
        lines.extend(channel_lines(
            Channel::Mic,
            &snapshot.mic,
            &self.mic,
            bar_width,
        ));
        lines.push(Line::default());
        lines.extend(channel_lines(
            Channel::Them,
            &snapshot.them,
            &self.them,
            bar_width,
        ));
        lines.push(Line::default());
        lines.push(status_line(snapshot));
        lines.push(Line::default());
        for entry in snapshot.log.iter().rev().take(app::LOG_LINES).rev() {
            lines.push(log_line(entry));
        }
        for _ in snapshot.log.len()..app::LOG_LINES {
            lines.push(Line::default());
        }
        lines.push(Line::default());

        let row = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let (buttons_line, buttons) = buttons_line(snapshot.state);
        lines.push(buttons_line);
        if row < inner.height {
            let y = inner.y + row;
            for (offset, width, button) in buttons {
                self.buttons
                    .push((Rect::new(inner.x + offset, y, width, 1), button));
            }
        }
        frame.render_widget(Paragraph::new(lines), inner);

        if self.confirm_quit {
            self.draw_confirm(frame);
        }
    }

    fn draw_confirm(&mut self, frame: &mut Frame<'_>) {
        let text = "Идёт запись. Сохранить и выйти?";
        let yes = "[y] Да";
        let no = "[n] Нет";
        let width = u16::try_from(text.chars().count() + 6).unwrap_or(u16::MAX);
        let area = centered(frame.area(), width, 5);
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .title(" Выход ")
            .border_style(Style::default().fg(Color::Yellow));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines = vec![
            Line::from(format!(" {text}")),
            Line::from(vec![
                Span::raw(" "),
                Span::styled(yes, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("   "),
                Span::raw(no),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
        let y = inner.y + 1;
        let yes_width = text_width(yes);
        self.buttons.clear();
        self.buttons
            .push((Rect::new(inner.x + 1, y, yes_width, 1), Button::ConfirmYes));
        self.buttons.push((
            Rect::new(inner.x + 1 + yes_width + 3, y, text_width(no), 1),
            Button::ConfirmNo,
        ));
    }
}

fn is_recording(state: State) -> bool {
    matches!(state, State::Recording | State::Paused)
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

fn channel_lines(
    channel: Channel,
    view: &ChannelView,
    meter: &Meter,
    bar_width: u16,
) -> Vec<Line<'static>> {
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
    let pad = " ".repeat(2 + LABEL_WIDTH);

    let (bar, peak) = meter.bar(bar_width);
    let bar_style = if missing {
        Style::default().fg(Color::Red)
    } else {
        Style::default().fg(Color::Green)
    };
    let mut bar_spans = vec![Span::raw(pad.clone()), Span::raw("▕")];
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

    vec![
        Line::from(vec![
            Span::styled(
                format!("  {label:<LABEL_WIDTH$}"),
                highlight.add_modifier(Modifier::BOLD),
            ),
            Span::styled(description, highlight),
        ]),
        Line::from(vec![
            Span::raw(pad),
            Span::styled(name, Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(bar_spans),
    ]
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
    let mut spans = vec![Span::raw("  "), Span::styled(format!("{text:<10}"), style)];
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
            format!("  {}  ", entry.at.format("%H:%M:%S")),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(entry.text.clone(), style),
    ])
}

/// The button row and the position of each button in it.
fn buttons_line(state: State) -> (Line<'static>, Vec<(u16, u16, Button)>) {
    let pause = if state == State::Paused {
        "[p] Продолжить"
    } else {
        "[p] Пауза"
    };
    let buttons = [
        ("[r] Запись", Button::Command(Command::Start)),
        (pause, Button::Command(Command::TogglePause)),
        ("[s] Стоп", Button::Command(Command::Stop)),
        ("[q] Выход", Button::Quit),
    ];
    let mut spans = vec![Span::raw("  ")];
    let mut areas = Vec::new();
    let mut offset: u16 = 2;
    for (text, button) in buttons {
        let enabled = match button {
            Button::Command(command) => app::allows(state, command),
            _ => true,
        };
        let style = if enabled {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        };
        let width = text_width(text);
        if enabled {
            areas.push((offset, width, button));
        }
        spans.push(Span::styled(text, style));
        spans.push(Span::raw("   "));
        offset = offset.saturating_add(width + 3);
    }
    (Line::from(spans), areas)
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
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| ui.draw(frame)).unwrap();
        terminal
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

    #[test]
    fn recording_screen() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        let terminal = render(&mut ui);
        let screen = lines(&terminal).join("\n");
        for text in [
            "callrec",
            "Микрофон (L)     Jabra Evolve2 65 Mono",
            "bluez_input.70",
            "-18 dB",
            "Собеседники (R)  Jabra Evolve2 65 (монитор)",
            "-31 dB",
            "● ЗАПИСЬ    00:23:41   /calls/2026-10-21_14-00.flac",
            "14:23:05  Микрофон: Встроенный микрофон → Jabra Evolve2 65 Mono",
            "[r] Запись   [p] Пауза   [s] Стоп   [q] Выход",
        ] {
            assert!(screen.contains(text), "{text:?} missing in:\n{screen}");
        }
    }

    #[test]
    fn status_texts() {
        for (state, text) in [
            (State::Idle, "ОЖИДАНИЕ"),
            (State::Paused, "❚❚ ПАУЗА"),
            (State::Saving, "СОХРАНЕНИЕ…"),
        ] {
            let mut ui = Ui::new(snapshot(state), Instant::now());
            let terminal = render(&mut ui);
            find(&terminal, text);
        }
        let mut ui = Ui::new(snapshot(State::Paused), Instant::now());
        find(&render(&mut ui), "[p] Продолжить");
    }

    #[test]
    fn unavailable_buttons_are_dimmed_and_inert() {
        let mut ui = Ui::new(snapshot(State::Idle), Instant::now());
        let terminal = render(&mut ui);
        let pause = find(&terminal, "[p] Пауза");
        let start = find(&terminal, "[r] Запись");
        assert!(
            style_at(&terminal, pause)
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert!(
            !style_at(&terminal, start)
                .add_modifier
                .contains(Modifier::DIM)
        );

        assert_eq!(ui.mouse(click(pause)), None);
        assert_eq!(
            ui.mouse(click(start)),
            Some(UiAction::Command(Command::Start))
        );
        assert_eq!(ui.key(key('p')), None);
        assert_eq!(ui.key(key('r')), Some(UiAction::Command(Command::Start)));
    }

    #[test]
    fn keys_work_on_russian_layout() {
        let mut ui = Ui::new(snapshot(State::Recording), Instant::now());
        assert_eq!(
            ui.key(key('з')),
            Some(UiAction::Command(Command::TogglePause))
        );
        assert_eq!(ui.key(key('ы')), Some(UiAction::Command(Command::Stop)));
        assert_eq!(ui.key(key('S')), Some(UiAction::Command(Command::Stop)));
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
        let terminal = render(&mut ui);
        find(&terminal, "Идёт запись. Сохранить и выйти?");
        // Other commands are blocked while the question is open.
        assert_eq!(ui.key(key('s')), None);
        assert_eq!(ui.key(key('n')), None);
        assert_eq!(ui.key(key('s')), Some(UiAction::Command(Command::Stop)));

        assert_eq!(ui.key(key('q')), None);
        let terminal = render(&mut ui);
        assert_eq!(
            ui.mouse(click(find(&terminal, "[y] Да"))),
            Some(UiAction::Quit)
        );
    }

    #[test]
    fn escape_cancels_quit() {
        let mut ui = Ui::new(snapshot(State::Paused), Instant::now());
        ui.key(key('q'));
        assert_eq!(
            ui.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            ui.key(key('p')),
            Some(UiAction::Command(Command::TogglePause))
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
