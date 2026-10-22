//! callrec — TUI call recorder. Specification: `instruction.md`.

// Wired into the application in later stages (spec 7.1).
#[allow(dead_code)]
mod app;
#[allow(dead_code)]
mod mixer;
#[allow(dead_code)]
mod pw;
#[allow(dead_code)]
mod writer;

use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::mixer::{Channel, Mixer, TickClock};
use crate::pw::PwEvent;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--probe") => {
            let seconds = args
                .next()
                .map(|s| s.parse::<u64>())
                .transpose()
                .context("--probe [секунды]")?;
            probe(seconds.map(Duration::from_secs))
        }
        _ => Ok(()),
    }
}

/// Temporary console mode for stage 3 (spec 7.1): prints devices, events and
/// levels. Replaced by the TUI in later stages.
#[allow(clippy::print_stdout)]
fn probe(limit: Option<Duration>) -> anyhow::Result<()> {
    let (pcm_tx, pcm_rx) = crossbeam_channel::bounded(pw::PCM_QUEUE_CHUNKS);
    let (event_tx, event_rx) = crossbeam_channel::unbounded();
    let pipewire = pw::spawn(pcm_tx, event_tx).context("запуск потока PipeWire")?;

    let start = Instant::now();
    let mut mixer = Mixer::new();
    let mut clock = TickClock::new(start);
    let mut ticks: u64 = 0;
    let (mut mic_db, mut them_db) = (f64::MIN, f64::MIN);
    let mut padded = (0, 0);
    let mut corrected = (0, 0);
    // Samples received per channel: the real delivery rate against the
    // mixer clock.
    let mut received: (usize, usize) = (0, 0);

    while limit.is_none_or(|limit| start.elapsed() < limit) {
        for chunk in pcm_rx.try_iter() {
            match chunk.channel {
                Channel::Mic => received.0 += chunk.samples().len(),
                Channel::Them => received.1 += chunk.samples().len(),
            }
            mixer.push(chunk.channel, chunk.samples());
        }
        for event in event_rx.try_iter() {
            println!(
                "{:>7.2} с  {}",
                start.elapsed().as_secs_f64(),
                describe(&event)
            );
            if matches!(event, PwEvent::Fatal(_)) {
                return Ok(());
            }
        }
        for _ in 0..clock.take_due(Instant::now()) {
            let block = mixer.tick();
            mic_db = mic_db.max(block.mic.level_db);
            them_db = them_db.max(block.them.level_db);
            padded.0 += block.mic.padded;
            padded.1 += block.them.padded;
            corrected.0 += block.mic.dropped + block.mic.inserted;
            corrected.1 += block.them.dropped + block.them.inserted;
            ticks += 1;
            if ticks.is_multiple_of(10) {
                println!(
                    "{:>7.2} с  L {:>6.1} dB  R {:>6.1} dB  тишина-дополнение L {} R {}  коррекция L {} R {}  очередь L {} R {}  получено L {} R {}",
                    start.elapsed().as_secs_f64(),
                    mic_db,
                    them_db,
                    padded.0,
                    padded.1,
                    corrected.0,
                    corrected.1,
                    mixer.queued(Channel::Mic),
                    mixer.queued(Channel::Them),
                    received.0,
                    received.1,
                );
                (mic_db, them_db) = (f64::MIN, f64::MIN);
                padded = (0, 0);
                corrected = (0, 0);
            }
        }
        if let Some(deadline) = clock.next_deadline() {
            thread::sleep(deadline.saturating_duration_since(Instant::now()));
        }
    }
    pipewire.shutdown();
    Ok(())
}

fn describe(event: &PwEvent) -> String {
    let channel = |c: &Channel| match c {
        Channel::Mic => "Микрофон",
        Channel::Them => "Собеседники",
    };
    let device = |d: &Option<pw::Device>| {
        d.as_ref().map_or("нет устройства".to_owned(), |d| {
            format!("{} ({})", d.description, d.name)
        })
    };
    match event {
        PwEvent::DefaultChanged {
            channel: c,
            device: d,
        } => {
            format!("{}: по умолчанию {}", channel(c), device(d))
        }
        PwEvent::StreamDevice {
            channel: c,
            device: d,
        } => {
            format!("{}: поток подключён к {}", channel(c), device(d))
        }
        PwEvent::ManualReconnect { channel: c } => {
            format!("{}: переподключено вручную", channel(c))
        }
        PwEvent::Connected => "PipeWire: подключено".to_owned(),
        PwEvent::Disconnected { reason } => format!("PipeWire: нет соединения: {reason}"),
        PwEvent::Error(e) => format!("ошибка: {e}"),
        PwEvent::Fatal(e) => format!("фатальная ошибка: {e}"),
    }
}
