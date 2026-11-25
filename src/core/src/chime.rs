//! The short sounds Earshot plays when the voice group changes: two notes rising when someone
//! joins your group (or you join one), falling when someone leaves (or you do). Made here, from
//! sine waves with a soft attack and decay: Earshot's own sounds.
//!
//! [`group_change`] decides which one a change in the group calls for.

use std::f32::consts::PI;

use crate::wire::Uuid;

const RATE: f32 = 48_000.0;
/// Each note's length, and the gap between them.
const NOTE_SECONDS: f32 = 0.09;
const GAP_SECONDS: f32 = 0.015;
/// Peak level: well under a voice, so it never startles.
const PEAK: f32 = 0.22;
/// The two notes: an E and the A above it (a fourth).
const LOW_HZ: f32 = 659.25;
const HIGH_HZ: f32 = 880.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chime {
    /// Low then high.
    Join,
    /// High then low.
    Leave,
}

/// The chime as 48 kHz mono samples.
pub fn samples(chime: Chime) -> Vec<f32> {
    let (first, second) = match chime {
        Chime::Join => (LOW_HZ, HIGH_HZ),
        Chime::Leave => (HIGH_HZ, LOW_HZ),
    };
    let mut out = note(first);
    out.extend(std::iter::repeat_n(0.0, (GAP_SECONDS * RATE) as usize));
    out.extend(note(second));
    out
}

/// One note: a sine with a soft 5 ms attack and an exponential decay to silence.
fn note(hz: f32) -> Vec<f32> {
    let n = (NOTE_SECONDS * RATE) as usize;
    let attack = (0.005 * RATE) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / RATE;
            let rise = (i as f32 / attack as f32).min(1.0);
            let fall = (-6.0 * i as f32 / n as f32).exp() * (1.0 - i as f32 / n as f32);
            PEAK * rise * fall * (2.0 * PI * hz * t).sin()
        })
        .collect()
}

/// Our group and who else is in it, as the server's states say.
pub type GroupView = Option<(Uuid, Vec<Uuid>)>;

/// The chime for going from `before` to `after`, if any: joining or leaving a group ourselves,
/// or, in the same group, someone joining or leaving it (a join wins when both happen at once).
pub fn group_change(before: &GroupView, after: &GroupView) -> Option<Chime> {
    match (before, after) {
        (None, Some(_)) => Some(Chime::Join),
        (Some(_), None) => Some(Chime::Leave),
        (Some((a, _)), Some((b, _))) if a != b => Some(Chime::Join),
        (Some((_, old)), Some((_, new))) => {
            if new.iter().any(|p| !old.contains(p)) {
                Some(Chime::Join)
            } else if old.iter().any(|p| !new.contains(p)) {
                Some(Chime::Leave)
            } else {
                None
            }
        }
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(s: &[f32]) -> f32 {
        s.iter().fold(0.0, |m, x| m.max(x.abs()))
    }

    #[test]
    fn chimes_are_short_quiet_and_end_in_silence() {
        for c in [Chime::Join, Chime::Leave] {
            let s = samples(c);
            let seconds = s.len() as f32 / RATE;
            assert!((0.15..0.25).contains(&seconds), "{seconds} s");
            assert!(peak(&s) > 0.1 && peak(&s) <= PEAK, "peak {}", peak(&s));
            assert!(
                s.first().unwrap().abs() < 1e-3 && s.last().unwrap().abs() < 1e-3,
                "no click at either end"
            );
        }
    }

    #[test]
    fn join_rises_and_leave_falls() {
        // Count zero crossings in each half: the higher note crosses more often.
        let crossings = |s: &[f32]| s.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        let half = |c| {
            let s = samples(c);
            let mid = s.len() / 2;
            (crossings(&s[..mid]), crossings(&s[mid..]))
        };
        let (a, b) = half(Chime::Join);
        assert!(b > a, "join: {a} then {b}");
        let (a, b) = half(Chime::Leave);
        assert!(a > b, "leave: {a} then {b}");
    }

    #[test]
    fn group_changes() {
        let (g, h) = (Uuid([1; 16]), Uuid([2; 16]));
        let (ann, bob) = (Uuid([10; 16]), Uuid([11; 16]));
        let view = |group: Uuid, members: &[Uuid]| Some((group, members.to_vec()));
        assert_eq!(group_change(&None, &None), None);
        assert_eq!(
            group_change(&None, &view(g, &[])),
            Some(Chime::Join),
            "we joined"
        );
        assert_eq!(
            group_change(&view(g, &[ann]), &None),
            Some(Chime::Leave),
            "we left"
        );
        assert_eq!(
            group_change(&view(g, &[ann]), &view(h, &[])),
            Some(Chime::Join),
            "we moved"
        );
        assert_eq!(
            group_change(&view(g, &[ann]), &view(g, &[ann, bob])),
            Some(Chime::Join),
            "bob joined"
        );
        assert_eq!(
            group_change(&view(g, &[ann, bob]), &view(g, &[ann])),
            Some(Chime::Leave),
            "bob left"
        );
        assert_eq!(
            group_change(&view(g, &[ann, bob]), &view(g, &[bob, ann])),
            None,
            "order doesn't matter"
        );
        assert_eq!(
            group_change(&view(g, &[ann]), &view(g, &[bob])),
            Some(Chime::Join),
            "a swap: join wins"
        );
    }
}
