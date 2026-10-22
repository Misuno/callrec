//! Per-channel sample queues, the 100 ms mixer clock and level metering
//! (spec 5.4, 5.5). Does not depend on `pipewire`, so it can be unit-tested.
//!
//! The latency of a channel is the number of samples waiting in its queue.
//! Both channels stay in sync as long as both queues sit at the same level,
//! so every queue is kept at `TARGET_LEVEL` independently of the other one.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Capture sample rate, Hz.
pub const SAMPLE_RATE: u32 = 16_000;
/// Mixer tick interval.
pub const TICK: Duration = Duration::from_millis(100);
/// Samples taken from each channel per tick (one `TICK` at `SAMPLE_RATE`).
pub const BLOCK_SAMPLES: usize = 1600;
/// Reserve kept in a queue on top of one block to absorb arrival jitter.
pub const RESERVE_SAMPLES: usize = 1600;
/// Queue level expected right before a block is taken.
pub const TARGET_LEVEL: usize = BLOCK_SAMPLES + RESERVE_SAMPLES;
/// Above this level the oldest samples are dropped down to `TARGET_LEVEL`.
pub const MAX_LEVEL: usize = 4800;
/// Length of the drift-correction window in ticks (2 s).
pub const DRIFT_WINDOW_TICKS: u32 = 20;
/// Allowed deviation of the queue level from `TARGET_LEVEL` (20 ms).
pub const DRIFT_TOLERANCE: usize = 320;
/// Lower bound of reported levels, dBFS.
pub const LEVEL_FLOOR_DB: f64 = -90.0;
/// Full-scale amplitude of a signed 16-bit sample.
const FULL_SCALE: f64 = 32_768.0;

/// Capture channel. `Mic` is the left channel of the file, `Them` the right one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Mic,
    Them,
}

/// What happened to one channel during a tick.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ChannelTick {
    /// RMS level of the block, dBFS, not below `LEVEL_FLOOR_DB`.
    pub level_db: f64,
    /// Zeros written into the block in place of captured audio.
    pub padded: usize,
    /// Captured samples discarded by overflow or drift correction.
    pub dropped: usize,
    /// Zeros inserted into the queue by drift correction.
    pub inserted: usize,
}

/// One tick of output: `BLOCK_SAMPLES` stereo frames.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// Interleaved stereo PCM: L = mic, R = them.
    pub stereo: Vec<i16>,
    pub mic: ChannelTick,
    pub them: ChannelTick,
}

/// Aligns both capture channels on the mixer clock and produces stereo blocks.
#[derive(Debug)]
pub struct Mixer {
    mic: ChannelQueue,
    them: ChannelQueue,
    mic_block: Vec<i16>,
    them_block: Vec<i16>,
}

impl Default for Mixer {
    fn default() -> Self {
        Self::new()
    }
}

impl Mixer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            mic: ChannelQueue::new(),
            them: ChannelQueue::new(),
            mic_block: Vec::with_capacity(BLOCK_SAMPLES),
            them_block: Vec::with_capacity(BLOCK_SAMPLES),
        }
    }

    /// Appends captured samples to a channel queue.
    pub fn push(&mut self, channel: Channel, samples: &[i16]) {
        self.queue_mut(channel)
            .samples
            .extend(samples.iter().copied());
    }

    /// Number of samples currently queued for a channel.
    #[cfg(test)]
    pub fn queued(&self, channel: Channel) -> usize {
        match channel {
            Channel::Mic => self.mic.samples.len(),
            Channel::Them => self.them.samples.len(),
        }
    }

    /// Runs one tick: takes one block from each queue and interleaves them.
    pub fn tick(&mut self) -> Block {
        let mic = self.mic.tick(&mut self.mic_block);
        let them = self.them.tick(&mut self.them_block);
        let mut stereo = Vec::with_capacity(BLOCK_SAMPLES * 2);
        for (&left, &right) in self.mic_block.iter().zip(&self.them_block) {
            stereo.push(left);
            stereo.push(right);
        }
        Block { stereo, mic, them }
    }

    fn queue_mut(&mut self, channel: Channel) -> &mut ChannelQueue {
        match channel {
            Channel::Mic => &mut self.mic,
            Channel::Them => &mut self.them,
        }
    }
}

#[derive(Debug)]
struct ChannelQueue {
    samples: VecDeque<i16>,
    /// False while the queue fills up to `TARGET_LEVEL` (at start and after
    /// an underrun); the channel outputs silence meanwhile.
    primed: bool,
    window_ticks: u32,
    window_min: usize,
    window_max: usize,
}

impl ChannelQueue {
    fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_LEVEL + BLOCK_SAMPLES),
            primed: false,
            window_ticks: 0,
            window_min: usize::MAX,
            window_max: 0,
        }
    }

    /// Writes exactly `BLOCK_SAMPLES` samples into `out`.
    fn tick(&mut self, out: &mut Vec<i16>) -> ChannelTick {
        let mut tick = ChannelTick::default();
        out.clear();
        self.fill_block(out, &mut tick);
        tick.level_db = rms_dbfs(out);
        tick
    }

    fn fill_block(&mut self, out: &mut Vec<i16>, tick: &mut ChannelTick) {
        if !self.primed {
            if self.samples.len() < TARGET_LEVEL {
                out.resize(BLOCK_SAMPLES, 0);
                tick.padded = BLOCK_SAMPLES;
                return;
            }
            self.primed = true;
            self.reset_window();
        }

        let level = self.samples.len();
        if level > MAX_LEVEL {
            tick.dropped += self.drop_oldest(level.saturating_sub(TARGET_LEVEL));
            self.reset_window();
        } else {
            self.window_ticks = self.window_ticks.saturating_add(1);
            self.window_min = self.window_min.min(level);
            self.window_max = self.window_max.max(level);
        }

        let take = self.samples.len().min(BLOCK_SAMPLES);
        out.extend(self.samples.drain(..take));
        if take < BLOCK_SAMPLES {
            out.resize(BLOCK_SAMPLES, 0);
            tick.padded = BLOCK_SAMPLES - take;
            self.primed = false;
            self.reset_window();
            return;
        }

        if self.window_ticks >= DRIFT_WINDOW_TICKS {
            self.correct_drift(tick);
        }
    }

    /// Pulls the queue level back to `TARGET_LEVEL` when it stayed off target
    /// for the whole window. Min/max over the window ignore arrival jitter.
    fn correct_drift(&mut self, tick: &mut ChannelTick) {
        if self.window_min > TARGET_LEVEL + DRIFT_TOLERANCE {
            tick.dropped += self.drop_oldest(self.window_min - TARGET_LEVEL);
        } else if self.window_max + DRIFT_TOLERANCE < TARGET_LEVEL {
            let missing = TARGET_LEVEL - self.window_max;
            for _ in 0..missing {
                self.samples.push_front(0);
            }
            tick.inserted = missing;
        }
        self.reset_window();
    }

    fn drop_oldest(&mut self, count: usize) -> usize {
        let count = count.min(self.samples.len());
        self.samples.drain(..count);
        count
    }

    fn reset_window(&mut self) {
        self.window_ticks = 0;
        self.window_min = usize::MAX;
        self.window_max = 0;
    }
}

/// Drift-free mixer clock: tick `n` is due at `start + n * TICK`, so late
/// wake-ups do not accumulate error.
#[derive(Debug)]
pub struct TickClock {
    start: Instant,
    ticks: u64,
}

impl TickClock {
    #[must_use]
    pub fn new(start: Instant) -> Self {
        Self { start, ticks: 0 }
    }

    /// Number of ticks due at `now` that were not taken yet; marks them taken.
    /// After a late wake-up this is more than one: all of them must be run.
    pub fn take_due(&mut self, now: Instant) -> u64 {
        let elapsed = now.saturating_duration_since(self.start);
        let total = u64::try_from(elapsed.as_nanos() / TICK.as_nanos()).unwrap_or(u64::MAX);
        let due = total.saturating_sub(self.ticks);
        self.ticks = self.ticks.max(total);
        due
    }

    /// When the next tick becomes due. `None` only if it does not fit `Instant`.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Instant> {
        let next = u32::try_from(self.ticks.saturating_add(1)).ok()?;
        self.start.checked_add(TICK.checked_mul(next)?)
    }
}

/// RMS level of `samples` in dBFS: `20 * log10(rms / 32768)`, not below
/// `LEVEL_FLOOR_DB`. Empty input is silence.
#[must_use]
pub fn rms_dbfs(samples: &[i16]) -> f64 {
    let Ok(count) = u32::try_from(samples.len()) else {
        return LEVEL_FLOOR_DB;
    };
    if count == 0 {
        return LEVEL_FLOOR_DB;
    }
    let sum: f64 = samples
        .iter()
        .map(|&s| {
            let s = f64::from(s);
            s * s
        })
        .sum();
    let rms = (sum / f64::from(count)).sqrt();
    // log10(0) is -inf, which max() turns into the floor.
    (20.0 * (rms / FULL_SCALE).log10()).max(LEVEL_FLOOR_DB)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a channel with a counter signal, so every sample identifies its
    /// capture position.
    struct Source {
        channel: Channel,
        next: usize,
    }

    impl Source {
        fn new(channel: Channel) -> Self {
            Self { channel, next: 0 }
        }

        fn samples(&mut self, count: usize) -> Vec<i16> {
            let out = (self.next..self.next + count)
                .map(|i| i16::try_from(i % 30_000 + 1).unwrap())
                .collect();
            self.next += count;
            out
        }

        fn push(&mut self, mixer: &mut Mixer, count: usize) {
            let samples = self.samples(count);
            mixer.push(self.channel, &samples);
        }

        /// Advances the counter without delivering data (lost while switching).
        fn lose(&mut self, count: usize) {
            self.next += count;
        }
    }

    fn left(block: &Block) -> Vec<i16> {
        block
            .stereo
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[l, _]| *l)
            .collect()
    }

    fn right(block: &Block) -> Vec<i16> {
        block
            .stereo
            .as_chunks::<2>()
            .0
            .iter()
            .map(|[_, r]| *r)
            .collect()
    }

    fn counter(from: usize, count: usize) -> Vec<i16> {
        Source {
            channel: Channel::Mic,
            next: from,
        }
        .samples(count)
    }

    #[test]
    fn block_matches_tick_and_rate() {
        assert_eq!(
            BLOCK_SAMPLES as u128,
            u128::from(SAMPLE_RATE) * TICK.as_millis() / 1000
        );
    }

    #[test]
    fn outputs_silence_until_target_level_is_reached() {
        let mut mixer = Mixer::new();
        let mut mic = Source::new(Channel::Mic);

        mic.push(&mut mixer, BLOCK_SAMPLES);
        let block = mixer.tick();
        assert_eq!(block.stereo.len(), BLOCK_SAMPLES * 2);
        assert!(left(&block).iter().all(|&s| s == 0));
        assert_eq!(block.mic.padded, BLOCK_SAMPLES);
        assert_eq!(mixer.queued(Channel::Mic), BLOCK_SAMPLES);

        mic.push(&mut mixer, BLOCK_SAMPLES);
        let block = mixer.tick();
        assert_eq!(left(&block), counter(0, BLOCK_SAMPLES));
        assert_eq!(block.mic.padded, 0);
        assert_eq!(mixer.queued(Channel::Mic), RESERVE_SAMPLES);
    }

    #[test]
    fn interleaves_mic_left_and_them_right() {
        let mut mixer = Mixer::new();
        mixer.push(Channel::Mic, &[7; TARGET_LEVEL]);
        mixer.push(Channel::Them, &[-3; TARGET_LEVEL]);
        let block = mixer.tick();
        assert!(left(&block).iter().all(|&s| s == 7));
        assert!(right(&block).iter().all(|&s| s == -3));
    }

    #[test]
    fn shortage_is_padded_with_silence_and_queue_refills() {
        let mut mixer = Mixer::new();
        let mut mic = Source::new(Channel::Mic);
        mic.push(&mut mixer, TARGET_LEVEL);
        mixer.tick();
        mixer.tick(); // consumes the reserve, queue is empty now

        mic.push(&mut mixer, 1000);
        let block = mixer.tick();
        let mut expected = counter(TARGET_LEVEL, 1000);
        expected.resize(BLOCK_SAMPLES, 0);
        assert_eq!(left(&block), expected);
        assert_eq!(block.mic.padded, BLOCK_SAMPLES - 1000);

        // Refilling: silence until the target level is back.
        mic.push(&mut mixer, BLOCK_SAMPLES);
        assert_eq!(mixer.tick().mic.padded, BLOCK_SAMPLES);
        mic.push(&mut mixer, BLOCK_SAMPLES);
        let block = mixer.tick();
        assert_eq!(left(&block), counter(TARGET_LEVEL + 1000, BLOCK_SAMPLES));
    }

    #[test]
    fn gap_in_one_channel_does_not_shift_channels() {
        let mut mixer = Mixer::new();
        let mut mic = Source::new(Channel::Mic);
        let mut them = Source::new(Channel::Them);

        let mut silent_them_ticks = 0;
        for tick in 0..40 {
            mic.push(&mut mixer, BLOCK_SAMPLES);
            // Device switch: "them" delivers nothing for 3 ticks.
            if (10..13).contains(&tick) {
                them.lose(BLOCK_SAMPLES);
            } else {
                them.push(&mut mixer, BLOCK_SAMPLES);
            }
            let block = mixer.tick();
            let (l, r) = (left(&block), right(&block));
            if tick >= 1 {
                // Mic is never interrupted by the gap in the other channel.
                assert_eq!(l, counter((tick - 1) * BLOCK_SAMPLES, BLOCK_SAMPLES));
            }
            if r.iter().all(|&s| s == 0) {
                silent_them_ticks += 1;
            } else {
                assert_eq!(r, l, "channels shifted at tick {tick}");
            }
        }
        // 1 tick of initial fill, then exactly as much silence as audio was
        // lost (3 ticks): the reserve covers the first lost tick, the refill
        // after the underrun adds it back.
        assert_eq!(silent_them_ticks, 4);
    }

    #[test]
    fn overflow_drops_oldest_down_to_target() {
        let mut mixer = Mixer::new();
        let mut mic = Source::new(Channel::Mic);
        mic.push(&mut mixer, TARGET_LEVEL);
        mixer.tick();

        // A burst after a device switch.
        mic.push(&mut mixer, 6000);
        let level = mixer.queued(Channel::Mic);
        assert!(level > MAX_LEVEL);
        let block = mixer.tick();
        assert_eq!(block.mic.dropped, level - TARGET_LEVEL);
        let first_kept = BLOCK_SAMPLES + level - TARGET_LEVEL;
        assert_eq!(left(&block), counter(first_kept, BLOCK_SAMPLES));
        assert_eq!(mixer.queued(Channel::Mic), RESERVE_SAMPLES);
    }

    #[test]
    fn level_at_max_is_not_overflow() {
        let mut mixer = Mixer::new();
        mixer.push(Channel::Mic, &[1; MAX_LEVEL]);
        let block = mixer.tick();
        assert_eq!(block.mic.dropped, 0);
        assert_eq!(mixer.queued(Channel::Mic), MAX_LEVEL - BLOCK_SAMPLES);
    }

    /// Runs a channel whose clock differs from the mixer clock and checks that
    /// the latency stays near the target without underruns or overflows.
    fn run_drifting_channel(per_tick: usize) -> (usize, usize) {
        let bound =
            DRIFT_TOLERANCE + 2 * DRIFT_WINDOW_TICKS as usize * per_tick.abs_diff(BLOCK_SAMPLES);
        let mut mixer = Mixer::new();
        let mut mic = Source::new(Channel::Mic);
        let (mut dropped, mut inserted) = (0, 0);
        mic.push(&mut mixer, TARGET_LEVEL);
        mixer.tick();
        for _ in 0..2000 {
            mic.push(&mut mixer, per_tick);
            let block = mixer.tick();
            assert_eq!(block.mic.padded, 0);
            assert!(mixer.queued(Channel::Mic).abs_diff(RESERVE_SAMPLES) <= bound);
            dropped += block.mic.dropped;
            inserted += block.mic.inserted;
        }
        (dropped, inserted)
    }

    #[test]
    fn fast_device_clock_is_trimmed() {
        let (dropped, inserted) = run_drifting_channel(BLOCK_SAMPLES + 8);
        assert!(dropped > 0);
        assert_eq!(inserted, 0);
    }

    #[test]
    fn slow_device_clock_is_padded() {
        let (dropped, inserted) = run_drifting_channel(BLOCK_SAMPLES - 8);
        assert_eq!(dropped, 0);
        assert!(inserted > 0);
    }

    #[test]
    fn jitter_within_reserve_is_not_corrected() {
        let mut mixer = Mixer::new();
        mixer.push(Channel::Mic, &[1; TARGET_LEVEL]);
        mixer.tick();
        // Same average rate, but data arrives in uneven portions.
        for tick in 0..200 {
            let portion = if tick % 2 == 0 { 1000 } else { 2200 };
            mixer.push(Channel::Mic, &vec![1; portion]);
            let block = mixer.tick();
            assert_eq!(block.mic.padded, 0);
            assert_eq!(block.mic.dropped, 0);
            assert_eq!(block.mic.inserted, 0);
        }
    }

    #[test]
    fn levels() {
        assert!((rms_dbfs(&[]) - LEVEL_FLOOR_DB).abs() < f64::EPSILON);
        assert!((rms_dbfs(&[0; BLOCK_SAMPLES]) - LEVEL_FLOOR_DB).abs() < f64::EPSILON);
        assert!(rms_dbfs(&[i16::MAX; BLOCK_SAMPLES]).abs() < 0.01);
        assert!(rms_dbfs(&[i16::MIN; BLOCK_SAMPLES]).abs() < f64::EPSILON);
        // 3277 / 32768 ≈ 0.1 → −20 dB
        assert!((rms_dbfs(&[3277; BLOCK_SAMPLES]) + 20.0).abs() < 0.01);
        assert!((rms_dbfs(&[-3277; BLOCK_SAMPLES]) + 20.0).abs() < 0.01);
        // Below the floor: amplitude 1 is about −90.3 dB.
        assert!((rms_dbfs(&[1; BLOCK_SAMPLES]) - LEVEL_FLOOR_DB).abs() < f64::EPSILON);
    }

    #[test]
    fn block_levels_are_reported_per_channel() {
        let mut mixer = Mixer::new();
        mixer.push(Channel::Mic, &[3277; TARGET_LEVEL]);
        let block = mixer.tick();
        assert!((block.mic.level_db + 20.0).abs() < 0.01);
        assert!((block.them.level_db - LEVEL_FLOOR_DB).abs() < f64::EPSILON);
    }

    #[test]
    fn clock_does_not_accumulate_error() {
        let start = Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let mut clock = TickClock::new(start);

        assert_eq!(clock.take_due(start), 0);
        assert_eq!(clock.next_deadline(), Some(at(100)));
        assert_eq!(clock.take_due(at(99)), 0);
        assert_eq!(clock.take_due(at(100)), 1);
        // Late wake-up: the next deadline is still on the 100 ms grid.
        assert_eq!(clock.take_due(at(250)), 1);
        assert_eq!(clock.next_deadline(), Some(at(300)));
        assert_eq!(clock.take_due(at(300)), 1);
        // Stalled for a while: every missed tick is reported.
        assert_eq!(clock.take_due(at(1000)), 7);
        assert_eq!(clock.take_due(at(1000)), 0);
        assert_eq!(clock.next_deadline(), Some(at(1100)));
    }

    #[test]
    fn clock_before_start_has_nothing_due() {
        let start = Instant::now() + Duration::from_secs(1);
        let mut clock = TickClock::new(start);
        assert_eq!(clock.take_due(Instant::now()), 0);
    }
}
