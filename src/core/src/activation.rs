//! When to send the microphone: push-to-talk, or voice activation by level.

/// Level of a frame in dBFS (0 = full scale), floored at -127 for silence.
pub fn level_dbfs(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return -127.0;
    }
    let mean_square = frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32;
    if mean_square <= 0.0 {
        return -127.0;
    }
    (10.0 * mean_square.log10()).max(-127.0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    /// Send while the key is held.
    PushToTalk,
    /// Send while the microphone is louder than `threshold_dbfs`.
    Voice { threshold_dbfs: f32 },
}

/// Decides, frame by frame, whether the microphone is on.
#[derive(Debug, Clone)]
pub struct Activation {
    pub mode: Mode,
    /// Frames to keep sending after the level drops (voice mode), so word endings aren't cut
    /// (`voice_deactivation_delay`).
    pub hold_frames: u32,
    /// Frames to keep sending after the push-to-talk key is let go (`ptt_deactivation_delay`).
    pub ptt_hold_frames: u32,
    held: u32,
}

impl Activation {
    /// With SVC's default delays: 25 frames (500 ms) after the voice, 5 (100 ms) after the key.
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            hold_frames: 25,
            ptt_hold_frames: 5,
            held: 0,
        }
    }

    /// Whether this 20 ms frame should be sent.
    pub fn frame(&mut self, level_dbfs: f32, key_down: bool) -> bool {
        match self.mode {
            Mode::PushToTalk => {
                if key_down {
                    self.held = self.ptt_hold_frames;
                    true
                } else if self.held > 0 {
                    self.held -= 1;
                    true
                } else {
                    false
                }
            }
            Mode::Voice { threshold_dbfs } => {
                if level_dbfs >= threshold_dbfs {
                    self.held = self.hold_frames;
                    true
                } else if self.held > 0 {
                    self.held -= 1;
                    true
                } else {
                    false
                }
            }
        }
    }
}

/// The push-to-talk key: held down to talk, or (`toggle`) pressed once to start talking and once
/// more to stop.
#[derive(Debug, Clone, Default)]
pub struct PttKey {
    pub toggle: bool,
    on: bool,
}

impl PttKey {
    pub fn new(toggle: bool) -> Self {
        Self { toggle, on: false }
    }

    /// The key went down (`pressed`) or up. Only presses while playing count: typing in chat
    /// mustn't switch the microphone on, though letting go of a held key always stops it.
    pub fn key(&mut self, pressed: bool, playing: bool) {
        if self.toggle {
            if pressed && playing {
                self.on = !self.on;
            }
        } else {
            self.on = pressed && playing;
        }
    }

    /// Stop talking whatever the style (a menu opened, the connection closed, the style changed).
    pub fn release(&mut self) {
        self.on = false;
    }

    /// Whether the microphone should be on now.
    pub fn is_on(&self) -> bool {
        self.on
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hold_to_talk_follows_the_key() {
        let mut k = PttKey::new(false);
        k.key(true, true);
        assert!(k.is_on());
        k.key(false, true);
        assert!(!k.is_on());
        k.key(true, false);
        assert!(!k.is_on(), "not while typing");
    }

    #[test]
    fn toggle_switches_on_each_press() {
        let mut k = PttKey::new(true);
        k.key(true, true);
        k.key(false, true);
        assert!(k.is_on(), "stays on after letting go");
        k.key(true, true);
        assert!(!k.is_on(), "the next press switches it off");
        k.key(true, false);
        assert!(!k.is_on(), "presses while typing don't count");
        k.key(true, true);
        k.release();
        assert!(!k.is_on(), "release always stops");
    }

    #[test]
    fn levels() {
        assert_eq!(level_dbfs(&[0.0; 960]), -127.0);
        assert_eq!(level_dbfs(&[]), -127.0);
        assert!((level_dbfs(&[1.0; 960]) - 0.0).abs() < 1e-4);
        assert!((level_dbfs(&[0.1; 960]) + 20.0).abs() < 1e-3);
    }

    #[test]
    fn push_to_talk_follows_the_key_and_trails_by_its_delay() {
        let mut a = Activation::new(Mode::PushToTalk);
        assert!(!a.frame(0.0, false));
        assert!(a.frame(-127.0, true));
        for _ in 0..5 {
            assert!(
                a.frame(-127.0, false),
                "the default 5 frames after the key is let go"
            );
        }
        assert!(!a.frame(-127.0, false));
        a.ptt_hold_frames = 0;
        assert!(a.frame(-127.0, true));
        assert!(!a.frame(-127.0, false), "no delay: stops with the key");
        a.ptt_hold_frames = 100;
        assert!(a.frame(-127.0, true));
        for i in 0..100 {
            assert!(a.frame(-127.0, false), "frame {i}");
        }
        assert!(!a.frame(-127.0, false));
    }

    #[test]
    fn voice_activation_holds_after_speech_by_its_delay() {
        let mut a = Activation::new(Mode::Voice {
            threshold_dbfs: -40.0,
        });
        assert!(!a.frame(-60.0, false));
        assert!(a.frame(-30.0, false));
        for _ in 0..25 {
            assert!(a.frame(-60.0, false), "held");
        }
        assert!(!a.frame(-60.0, false), "released after the hold");
        a.hold_frames = 2;
        assert!(a.frame(-30.0, false));
        assert!(a.frame(-60.0, false) && a.frame(-60.0, false));
        assert!(!a.frame(-60.0, false));
    }
}
