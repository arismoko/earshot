//! Per-speaker jitter buffer.
//!
//! UDP frames arrive with uneven delays, sometimes out of order, duplicated or not at all. The
//! buffer holds a few frames, then hands the mixer exactly one item per 20 ms tick, in sequence
//! order: the frame, a "lost" marker (decode with Opus concealment), or silence when the speaker
//! has stopped. It holds no clock: the caller's tick is the clock, which keeps it deterministic.

use std::collections::BTreeMap;

/// What to play for one speaker in this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pop {
    /// The next frame, in order.
    Frame(Vec<u8>),
    /// The next frame is missing: conceal it.
    Lost,
    /// Nothing to play (not started, or the speaker stopped).
    Silent,
}

#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    /// Frames to collect before starting to play (3 = 60 ms of added delay).
    pub target_frames: usize,
    /// Ticks to wait for `target_frames` once a frame is here; then what's buffered plays anyway,
    /// so a very short sound (a frame or two) plays now instead of before the next one.
    pub max_wait_ticks: u32,
    /// Most frames held at once; past this the oldest go, which bounds the delay.
    pub max_frames: usize,
    /// Ticks in a row with nothing buffered that are concealed before going silent.
    pub conceal_ticks: u32,
    /// Ticks in a row with nothing buffered before the talk spurt counts as over.
    pub end_ticks: u32,
    /// A sequence this far ahead of the expected one starts a new talk spurt instead of being
    /// treated as a long loss.
    pub restart_gap: u64,
    /// When the next frame is missing but later ones are here, wait until this many later
    /// frames have arrived (or as many ticks have passed) before giving it up as lost, so a
    /// late packet can still take its place (`audio_packet_threshold`). 0 gives it up at once.
    pub reorder_threshold: usize,
}

impl Default for JitterConfig {
    fn default() -> Self {
        Self {
            target_frames: 3,
            max_wait_ticks: 6,
            max_frames: 12,
            conceal_ticks: 2,
            end_ticks: 10,
            restart_gap: 50,
            reorder_threshold: 0,
        }
    }
}

impl JitterConfig {
    /// The config for SVC's `output_buffer_size` (frames collected before playing) and
    /// `audio_packet_threshold`.
    pub fn buffered(output_buffer_size: usize, packet_threshold: usize) -> Self {
        let target_frames = output_buffer_size.max(1);
        Self {
            target_frames,
            max_frames: (target_frames + packet_threshold + 4).max(12),
            reorder_threshold: packet_threshold,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Collecting frames before playing; `waited` counts ticks since the first one came.
    Buffering { waited: u32 },
    /// Playing; `next` is the sequence to hand out next.
    Playing {
        next: u64,
        empty_run: u32,
        held: usize,
    },
}

pub struct JitterBuffer {
    cfg: JitterConfig,
    frames: BTreeMap<u64, Vec<u8>>,
    state: State,
}

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        Self {
            cfg,
            frames: BTreeMap::new(),
            state: State::Buffering { waited: 0 },
        }
    }

    /// Changes the buffering. Frames held stay; the new limits apply from the next push or pop.
    pub fn set_config(&mut self, cfg: JitterConfig) {
        self.cfg = cfg;
    }

    /// Frames held right now.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Whether the speaker is currently being played.
    pub fn is_playing(&self) -> bool {
        matches!(self.state, State::Playing { .. })
    }

    /// Adds a frame that arrived from the network.
    pub fn push(&mut self, sequence: u64, opus: Vec<u8>) {
        if let State::Playing { next, .. } = self.state {
            if sequence < next {
                // Too late: its slot was already played or concealed. Unless it's far behind,
                // which means the sender restarted its counter.
                if next - sequence < self.cfg.restart_gap {
                    return;
                }
                self.restart();
            } else if sequence - next >= self.cfg.restart_gap {
                self.restart();
            }
        }
        self.frames.entry(sequence).or_insert(opus);

        while self.frames.len() > self.cfg.max_frames {
            let (oldest, _) = self.frames.pop_first().unwrap();
            if let State::Playing { next, .. } = &mut self.state {
                *next = (*next).max(oldest + 1);
            }
        }
    }

    /// Takes what to play in this 20 ms tick.
    pub fn pop(&mut self) -> Pop {
        match self.state {
            State::Buffering { waited } => {
                if self.frames.is_empty() {
                    return Pop::Silent;
                }
                if self.frames.len() < self.cfg.target_frames && waited < self.cfg.max_wait_ticks {
                    self.state = State::Buffering { waited: waited + 1 };
                    return Pop::Silent;
                }
                let (&first, _) = self.frames.first_key_value().unwrap();
                self.state = State::Playing {
                    next: first,
                    empty_run: 0,
                    held: 0,
                };
                self.pop()
            }
            State::Playing {
                next,
                empty_run,
                held,
            } => {
                if let Some(frame) = self.frames.remove(&next) {
                    self.state = State::Playing {
                        next: next + 1,
                        empty_run: 0,
                        held: 0,
                    };
                    return Pop::Frame(frame);
                }
                if !self.frames.is_empty() {
                    // A later frame is here, so this one is lost, not just slow: unless the
                    // threshold asks us to give it a few more ticks to turn up.
                    if self.frames.len() < self.cfg.reorder_threshold
                        && held < self.cfg.reorder_threshold
                    {
                        self.state = State::Playing {
                            next,
                            empty_run: 0,
                            held: held + 1,
                        };
                        return Pop::Silent;
                    }
                    self.state = State::Playing {
                        next: next + 1,
                        empty_run: 0,
                        held: 0,
                    };
                    return Pop::Lost;
                }
                let empty_run = empty_run + 1;
                if empty_run >= self.cfg.end_ticks {
                    self.state = State::Buffering { waited: 0 };
                    return Pop::Silent;
                }
                if empty_run <= self.cfg.conceal_ticks {
                    // Probably a delayed frame mid-speech: conceal and move on.
                    self.state = State::Playing {
                        next: next + 1,
                        empty_run,
                        held: 0,
                    };
                    Pop::Lost
                } else {
                    // Probably the end of speech: stay quiet but keep our place for a while.
                    self.state = State::Playing {
                        next,
                        empty_run,
                        held: 0,
                    };
                    Pop::Silent
                }
            }
        }
    }

    fn restart(&mut self) {
        self.frames.clear();
        self.state = State::Buffering { waited: 0 };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(seq: u64) -> Vec<u8> {
        vec![seq as u8]
    }

    fn buf() -> JitterBuffer {
        JitterBuffer::new(JitterConfig::default())
    }

    #[test]
    fn waits_for_the_target_then_plays_in_order() {
        let mut b = buf();
        b.push(0, frame(0));
        assert_eq!(b.pop(), Pop::Silent);
        b.push(1, frame(1));
        assert_eq!(b.pop(), Pop::Silent);
        b.push(2, frame(2));
        assert_eq!(b.pop(), Pop::Frame(frame(0)));
        assert_eq!(b.pop(), Pop::Frame(frame(1)));
        assert_eq!(b.pop(), Pop::Frame(frame(2)));
    }

    #[test]
    fn reorders_frames() {
        let mut b = buf();
        for s in [2, 0, 1, 4, 3] {
            b.push(s, frame(s));
        }
        let out: Vec<Pop> = (0..5).map(|_| b.pop()).collect();
        assert_eq!(
            out,
            (0..5).map(|s| Pop::Frame(frame(s))).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ignores_duplicates() {
        let mut b = buf();
        for s in [0, 0, 1, 1, 2] {
            b.push(s, frame(s));
        }
        assert_eq!(b.len(), 3);
    }

    #[test]
    fn a_missing_frame_is_concealed() {
        let mut b = buf();
        for s in [0, 1, 3, 4] {
            b.push(s, frame(s));
        }
        assert_eq!(b.pop(), Pop::Frame(frame(0)));
        assert_eq!(b.pop(), Pop::Frame(frame(1)));
        assert_eq!(b.pop(), Pop::Lost);
        assert_eq!(b.pop(), Pop::Frame(frame(3)));
    }

    #[test]
    fn a_late_frame_is_dropped() {
        let mut b = buf();
        for s in [0, 1, 3] {
            b.push(s, frame(s));
        }
        b.pop();
        b.pop();
        assert_eq!(b.pop(), Pop::Lost); // 2 never came
        b.push(2, frame(2)); // now it does: too late
        assert_eq!(b.pop(), Pop::Frame(frame(3)));
        assert!(b.is_empty());
    }

    #[test]
    fn a_short_underrun_is_concealed_and_a_slow_frame_still_plays_after_it() {
        let mut b = buf();
        for s in 0..3 {
            b.push(s, frame(s));
        }
        for _ in 0..3 {
            b.pop();
        }
        assert_eq!(b.pop(), Pop::Lost); // 3 is late: conceal
        b.push(4, frame(4));
        assert_eq!(b.pop(), Pop::Frame(frame(4)));
    }

    #[test]
    fn end_of_speech_goes_silent_then_back_to_buffering() {
        let cfg = JitterConfig::default();
        let mut b = buf();
        for s in 0..3 {
            b.push(s, frame(s));
        }
        for _ in 0..3 {
            b.pop();
        }
        let tail: Vec<Pop> = (0..cfg.end_ticks).map(|_| b.pop()).collect();
        assert_eq!(&tail[..cfg.conceal_ticks as usize], &[Pop::Lost, Pop::Lost]);
        assert!(
            tail[cfg.conceal_ticks as usize..]
                .iter()
                .all(|p| *p == Pop::Silent)
        );
        assert!(!b.is_playing());
    }

    #[test]
    fn a_new_talk_spurt_after_a_big_jump_restarts() {
        let mut b = buf();
        for s in 0..3 {
            b.push(s, frame(s));
        }
        b.pop();
        // The speaker comes back much later with a much higher sequence.
        for s in 500..503 {
            b.push(s, frame(s));
        }
        assert_eq!(b.pop(), Pop::Frame(frame(500)));
    }

    #[test]
    fn a_reset_counter_restarts_too() {
        let mut b = buf();
        for s in 1000..1003 {
            b.push(s, frame(s));
        }
        b.pop();
        for s in 0..3 {
            b.push(s, frame(s));
        }
        assert_eq!(b.pop(), Pop::Frame(frame(0)));
    }

    #[test]
    fn a_very_short_sound_plays_after_the_wait_not_with_the_next_one() {
        let cfg = JitterConfig::default();
        let mut b = buf();
        b.push(0, frame(0));
        b.push(1, frame(1));
        // Short of the target: it waits max_wait_ticks, then plays what came.
        for _ in 0..cfg.max_wait_ticks {
            assert_eq!(b.pop(), Pop::Silent);
        }
        assert_eq!(b.pop(), Pop::Frame(frame(0)));
        assert_eq!(b.pop(), Pop::Frame(frame(1)));
        // The talk spurt ends and nothing stale is left for the next one.
        for _ in 0..cfg.end_ticks {
            b.pop();
        }
        assert!(b.is_empty() && !b.is_playing());
        for s in 2..5 {
            b.push(s, frame(s));
        }
        assert_eq!(b.pop(), Pop::Frame(frame(2)));
    }

    #[test]
    fn waiting_only_counts_once_a_frame_is_here() {
        let cfg = JitterConfig::default();
        let mut b = buf();
        // A long quiet before anyone talks doesn't use up the wait.
        for _ in 0..cfg.max_wait_ticks * 3 {
            assert_eq!(b.pop(), Pop::Silent);
        }
        b.push(0, frame(0));
        assert_eq!(b.pop(), Pop::Silent);
        b.push(1, frame(1));
        b.push(2, frame(2));
        assert_eq!(b.pop(), Pop::Frame(frame(0)));
    }

    #[test]
    fn overflow_drops_the_oldest_and_bounds_delay() {
        let cfg = JitterConfig::default();
        let mut b = buf();
        for s in 0..(cfg.max_frames as u64 + 5) {
            b.push(s, frame(s));
        }
        assert_eq!(b.len(), cfg.max_frames);
        assert_eq!(b.pop(), Pop::Frame(frame(5)));
    }

    #[test]
    fn buffer_size_sets_how_many_frames_are_collected_first() {
        let mut b = JitterBuffer::new(JitterConfig::buffered(5, 0));
        for s in 0..4 {
            b.push(s, frame(s));
            assert_eq!(b.pop(), Pop::Silent, "{} of 5", s + 1);
        }
        b.push(4, frame(4));
        assert_eq!(b.pop(), Pop::Frame(frame(0)));
        let mut b = JitterBuffer::new(JitterConfig::buffered(1, 0));
        b.push(0, frame(0));
        assert_eq!(
            b.pop(),
            Pop::Frame(frame(0)),
            "a buffer of one plays at once"
        );
    }

    #[test]
    fn the_threshold_waits_for_a_late_frame_to_take_its_place() {
        let mut b = JitterBuffer::new(JitterConfig::buffered(1, 3));
        for s in [0, 1, 3] {
            b.push(s, frame(s));
        }
        assert_eq!(b.pop(), Pop::Frame(frame(0)));
        assert_eq!(b.pop(), Pop::Frame(frame(1)));
        assert_eq!(b.pop(), Pop::Silent, "2 is held for, not yet given up");
        b.push(2, frame(2));
        assert_eq!(b.pop(), Pop::Frame(frame(2)), "it arrived in time");
        assert_eq!(b.pop(), Pop::Frame(frame(3)));
    }

    #[test]
    fn the_threshold_gives_up_after_enough_later_frames_or_ticks() {
        // Enough later frames: lost at once.
        let mut b = JitterBuffer::new(JitterConfig::buffered(1, 2));
        for s in [0, 2, 3] {
            b.push(s, frame(s));
        }
        assert_eq!(b.pop(), Pop::Frame(frame(0)));
        assert_eq!(b.pop(), Pop::Lost);
        assert_eq!(b.pop(), Pop::Frame(frame(2)));
        // Too few later frames: waits the threshold in ticks, then gives up.
        let mut b = JitterBuffer::new(JitterConfig::buffered(1, 3));
        for s in [0, 2] {
            b.push(s, frame(s));
        }
        assert_eq!(b.pop(), Pop::Frame(frame(0)));
        for _ in 0..3 {
            assert_eq!(b.pop(), Pop::Silent);
        }
        assert_eq!(b.pop(), Pop::Lost);
        assert_eq!(b.pop(), Pop::Frame(frame(2)));
    }

    #[test]
    fn the_buffer_can_hold_what_the_threshold_asks_for() {
        let c = JitterConfig::buffered(16, 16);
        assert!(c.max_frames >= c.target_frames + c.reorder_threshold);
    }
}
