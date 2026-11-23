//! Positional audio: how loud a speaker is in each ear.
//!
//! The SVC server already drops audio from players beyond its voice distance (PROTOCOL.md), so
//! this only shapes what arrives: volume falls off with distance and reaches silence at the
//! voice distance, and equal-power panning puts the voice on the side the speaker is on.
//!
//! Coordinates are Minecraft's: x east, y up, z south. Yaw is in degrees, 0 facing south (+z),
//! 90 facing west (-x), as the game reports it.

use crate::options::AudioType;

/// Where a sound is heard from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Listener {
    pub pos: [f64; 3],
    pub yaw_degrees: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Falloff {
    /// Within this many blocks, full volume.
    pub full_volume_distance: f64,
    /// At and beyond this, silence. Use the server's voice distance.
    pub max_distance: f64,
}

impl Falloff {
    pub fn with_max(max_distance: f64) -> Self {
        // Full volume for the nearest 1/8 of the range (6 blocks at SVC's default 48).
        Self {
            full_volume_distance: max_distance / 8.0,
            max_distance,
        }
    }

    /// Volume 0..=1 at a distance: 1 up close, then a quadratic fall to 0 at `max_distance`.
    /// Quadratic sounds more natural than linear and reaches 0 smoothly, with no jump at the edge.
    pub fn gain(&self, distance: f64) -> f32 {
        if !distance.is_finite() || distance >= self.max_distance {
            return 0.0;
        }
        if distance <= self.full_volume_distance {
            return 1.0;
        }
        let t = (distance - self.full_volume_distance)
            / (self.max_distance - self.full_volume_distance);
        ((1.0 - t) * (1.0 - t)) as f32
    }
}

/// Left and right gains for a speaker at `source`, placed the way `audio_type` says.
pub fn stereo_gains(
    listener: &Listener,
    source: [f64; 3],
    falloff: &Falloff,
    audio_type: AudioType,
) -> (f32, f32) {
    let d = [
        source[0] - listener.pos[0],
        source[1] - listener.pos[1],
        source[2] - listener.pos[2],
    ];
    let distance = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    let gain = falloff.gain(distance);
    match audio_type {
        AudioType::Normal => {
            let pan = pan(listener.yaw_degrees, d[0], d[2]);
            // Equal-power pan law: left^2 + right^2 stays gain^2, so turning doesn't change loudness.
            let angle = (pan + 1.0) * std::f32::consts::FRAC_PI_4;
            (gain * angle.cos(), gain * angle.sin())
        }
        AudioType::Reduced => {
            let (l, r) = reduced_pan(
                lateral_angle(listener.yaw_degrees, d[0], d[2]),
                d[1].abs() as f32,
            );
            (gain * l, gain * r)
        }
        AudioType::Off => {
            let g = gain * std::f32::consts::FRAC_1_SQRT_2;
            (g, g)
        }
    }
}

/// The speaker's direction in degrees, -180 to 180: 0 straight ahead, positive to the right.
fn lateral_angle(yaw_degrees: f32, dx: f64, dz: f64) -> f32 {
    let yaw = (yaw_degrees as f64).to_radians();
    let ahead = -yaw.sin() * dx + yaw.cos() * dz;
    let right = -yaw.cos() * dx - yaw.sin() * dz;
    right.atan2(ahead).to_degrees() as f32
}

/// The reduced 3D placement: the angle folds to a side value that is largest at 90 degrees
/// (so ahead and behind sound the same), a height difference of 32 blocks or more removes it,
/// and each ear keeps a base of 0.3 with any shortfall from full volume shared out evenly.
fn reduced_pan(angle: f32, height_diff: f32) -> (f32, f32) {
    let t = angle / 180.0;
    let side = if t.abs() <= 0.5 {
        t
    } else {
        t.signum() * (1.0 - t.abs())
    };
    let side = side * (1.0 - (height_diff / 32.0).min(1.0));
    let base = 0.3;
    let left = if side < 0.0 {
        (side * 1.4).abs() + base
    } else {
        base
    };
    let right = if side >= 0.0 { side * 1.4 + base } else { base };
    let extra = 1.0 - left.max(right);
    (left + extra, right + extra)
}

/// -1 = fully left, 0 = ahead or behind, 1 = fully right, from the horizontal offset (dx, dz).
fn pan(yaw_degrees: f32, dx: f64, dz: f64) -> f32 {
    let horizontal = (dx * dx + dz * dz).sqrt();
    // Directly above, below or inside the listener: no side to pick.
    if horizontal < 0.5 {
        return 0.0;
    }
    let yaw = (yaw_degrees as f64).to_radians();
    // Facing (-sin yaw, cos yaw); the right-hand side is (-cos yaw, -sin yaw).
    let right = (-yaw.cos(), -yaw.sin());
    ((dx * right.0 + dz * right.1) / horizontal).clamp(-1.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-4;

    fn at_origin(yaw: f32) -> Listener {
        Listener {
            pos: [0.0, 64.0, 0.0],
            yaw_degrees: yaw,
        }
    }

    #[test]
    fn gain_is_full_nearby_and_zero_at_the_voice_distance() {
        let f = Falloff::with_max(48.0);
        assert_eq!(f.gain(0.0), 1.0);
        assert_eq!(f.gain(6.0), 1.0);
        assert_eq!(f.gain(48.0), 0.0);
        assert_eq!(f.gain(100.0), 0.0);
        assert_eq!(f.gain(f64::NAN), 0.0);
        let mid = f.gain(27.0); // halfway between 6 and 48
        assert!((mid - 0.25).abs() < EPS, "{mid}");
    }

    #[test]
    fn gain_never_increases_with_distance() {
        let f = Falloff::with_max(48.0);
        let mut prev = 1.0;
        for i in 0..=480 {
            let g = f.gain(i as f64 / 10.0);
            assert!(g <= prev + EPS, "gain rose at {}", i as f64 / 10.0);
            prev = g;
        }
        // And the edge is smooth: just inside it is almost silent.
        assert!(f.gain(47.9) < 0.001);
    }

    #[test]
    fn speaker_to_the_east_is_on_the_right_when_facing_north() {
        // Facing north (yaw 180, -z), east (+x) is to the right.
        let (l, r) = stereo_gains(
            &at_origin(180.0),
            [10.0, 64.0, 0.0],
            &Falloff::with_max(48.0),
            AudioType::Normal,
        );
        assert!(r > 0.0 && l.abs() < EPS, "l {l} r {r}");
    }

    #[test]
    fn speaker_to_the_west_is_on_the_right_when_facing_south() {
        // Facing south (yaw 0, +z), west (-x) is to the right.
        let (l, r) = stereo_gains(
            &at_origin(0.0),
            [-10.0, 64.0, 0.0],
            &Falloff::with_max(48.0),
            AudioType::Normal,
        );
        assert!(r > 0.0 && l.abs() < EPS, "l {l} r {r}");
    }

    #[test]
    fn speaker_ahead_is_centred() {
        // Facing west (yaw 90), a speaker further west is straight ahead.
        let (l, r) = stereo_gains(
            &at_origin(90.0),
            [-10.0, 64.0, 0.0],
            &Falloff::with_max(48.0),
            AudioType::Normal,
        );
        assert!((l - r).abs() < EPS);
    }

    #[test]
    fn panning_keeps_loudness_constant() {
        let f = Falloff::with_max(48.0);
        for yaw in (0..360).step_by(15) {
            let (l, r) = stereo_gains(
                &at_origin(yaw as f32),
                [3.0, 64.0, 4.0],
                &f,
                AudioType::Normal,
            );
            assert!((l * l + r * r - 1.0).abs() < 1e-3, "yaw {yaw}: {l} {r}");
        }
    }

    #[test]
    fn speaker_right_above_is_centred() {
        let (l, r) = stereo_gains(
            &at_origin(37.0),
            [0.1, 70.0, 0.0],
            &Falloff::with_max(48.0),
            AudioType::Normal,
        );
        assert!((l - r).abs() < EPS);
    }

    #[test]
    fn out_of_range_is_silent() {
        assert_eq!(
            stereo_gains(
                &at_origin(0.0),
                [100.0, 64.0, 0.0],
                &Falloff::with_max(48.0),
                AudioType::Normal
            ),
            (0.0, 0.0)
        );
    }

    #[test]
    fn reduced_audio_never_silences_an_ear_and_keeps_the_volume_up() {
        let f = Falloff::with_max(48.0);
        // Straight right: the right ear 1.0, the left at the 0.3 base.
        let (l, r) = stereo_gains(&at_origin(180.0), [10.0, 64.0, 0.0], &f, AudioType::Reduced);
        let g = f.gain(10.0);
        assert!((r - g).abs() < EPS && (l - 0.3 * g).abs() < EPS, "{l} {r}");
        // Ahead: both ears the same, and the shortfall from full is shared out.
        let (l, r) = stereo_gains(&at_origin(0.0), [0.0, 64.0, 10.0], &f, AudioType::Reduced);
        assert!((l - r).abs() < EPS && (l - 1.0 * g).abs() < EPS, "{l} {r}");
        // Behind sounds like ahead.
        let (lb, rb) = stereo_gains(&at_origin(0.0), [0.0, 64.0, -10.0], &f, AudioType::Reduced);
        assert!((lb - l).abs() < EPS && (rb - r).abs() < EPS);
        // Far above: no side at all.
        let (l, r) = stereo_gains(
            &at_origin(180.0),
            [10.0, 100.0, 0.0],
            &f,
            AudioType::Reduced,
        );
        assert!((l - r).abs() < EPS, "{l} {r}");
    }

    #[test]
    fn reduced_pan_folds_the_angle_around_the_sides() {
        // 45 degrees right: side 0.25, so right 0.65, left 0.3, and 0.35 shared out.
        let (l, r) = reduced_pan(45.0, 0.0);
        assert!((r - 1.0).abs() < EPS && (l - 0.65).abs() < EPS, "{l} {r}");
        // 135 degrees right folds back to the same side value as 45.
        assert_eq!(reduced_pan(135.0, 0.0), reduced_pan(45.0, 0.0));
        // On the left the ears swap.
        let (l, r) = reduced_pan(-45.0, 0.0);
        assert!((l - 1.0).abs() < EPS && (r - 0.65).abs() < EPS, "{l} {r}");
    }

    #[test]
    fn audio_off_plays_the_same_in_both_ears_at_the_distance_volume() {
        let f = Falloff::with_max(48.0);
        let (l, r) = stereo_gains(&at_origin(180.0), [20.0, 64.0, 0.0], &f, AudioType::Off);
        assert_eq!(l, r);
        assert!((l - f.gain(20.0) * std::f32::consts::FRAC_1_SQRT_2).abs() < EPS);
        assert_eq!(
            stereo_gains(&at_origin(0.0), [100.0, 64.0, 0.0], &f, AudioType::Off),
            (0.0, 0.0)
        );
    }
}
