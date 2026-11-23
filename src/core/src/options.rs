//! The client options Simple Voice Chat's client configuration documents, with its names,
//! defaults and ranges: delays, buffering, 3D audio, the icons and where the HUD draws them.
//! Kept in the settings file under the same names. Independent of the game.

use std::collections::HashMap;

/// The 3D audio type (`audio_type`): how a voice is placed around you.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioType {
    /// Full left/right placement by where the speaker is.
    #[default]
    Normal,
    /// A softer placement: the far ear is never silent.
    Reduced,
    /// No placement: the same in both ears (the volume still follows distance).
    Off,
}

impl AudioType {
    pub fn name(self) -> &'static str {
        match self {
            AudioType::Normal => "NORMAL",
            AudioType::Reduced => "REDUCED",
            AudioType::Off => "OFF",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "NORMAL" => Some(AudioType::Normal),
            "REDUCED" => Some(AudioType::Reduced),
            "OFF" => Some(AudioType::Off),
            _ => None,
        }
    }

    fn next(self) -> Self {
        match self {
            AudioType::Normal => AudioType::Reduced,
            AudioType::Reduced => AudioType::Off,
            AudioType::Off => AudioType::Normal,
        }
    }
}

/// The group HUD's layout (`group_player_icon_orientation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Orientation {
    #[default]
    Vertical,
    Horizontal,
}

impl Orientation {
    pub fn name(self) -> &'static str {
        match self {
            Orientation::Vertical => "VERTICAL",
            Orientation::Horizontal => "HORIZONTAL",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "VERTICAL" => Some(Orientation::Vertical),
            "HORIZONTAL" => Some(Orientation::Horizontal),
            _ => None,
        }
    }

    fn next(self) -> Self {
        match self {
            Orientation::Vertical => Orientation::Horizontal,
            Orientation::Horizontal => Orientation::Vertical,
        }
    }
}

/// Every option the menu edits through [`Options`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opt {
    VoiceDeactivationDelay,
    PttDeactivationDelay,
    OutputBufferSize,
    AudioPacketThreshold,
    AudioType,
    MuteOnJoin,
    OfflinePlayerVolumeAdjustment,
    ShowFakePlayersDisconnected,
    HideIcons,
    ShowNametagIcons,
    ShowHudIcons,
    ShowGroupHud,
    ShowOwnGroupIcon,
    GroupPlayerIconOrientation,
    HudIconScale,
    GroupHudIconScale,
    HudIconPosX,
    HudIconPosY,
    GroupPlayerIconPosX,
    GroupPlayerIconPosY,
}

/// What kind of control an option is, and its range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum OptKind {
    Toggle,
    /// A whole number from `min` to `max`.
    Int {
        min: i32,
        max: i32,
    },
    /// A number from `min` to `max`, shown with two decimals.
    Scale {
        min: f32,
        max: f32,
    },
    /// One of a few named values, advanced by clicking.
    Choice,
}

impl Opt {
    /// The option's name in the settings file: Simple Voice Chat's own.
    pub fn key(self) -> &'static str {
        match self {
            Opt::VoiceDeactivationDelay => "voice_deactivation_delay",
            Opt::PttDeactivationDelay => "ptt_deactivation_delay",
            Opt::OutputBufferSize => "output_buffer_size",
            Opt::AudioPacketThreshold => "audio_packet_threshold",
            Opt::AudioType => "audio_type",
            Opt::MuteOnJoin => "mute_on_join",
            Opt::OfflinePlayerVolumeAdjustment => "offline_player_volume_adjustment",
            Opt::ShowFakePlayersDisconnected => "show_fake_players_disconnected",
            Opt::HideIcons => "hide_icons",
            Opt::ShowNametagIcons => "show_nametag_icons",
            Opt::ShowHudIcons => "show_hud_icons",
            Opt::ShowGroupHud => "show_group_hud",
            Opt::ShowOwnGroupIcon => "show_own_group_icon",
            Opt::GroupPlayerIconOrientation => "group_player_icon_orientation",
            Opt::HudIconScale => "hud_icon_scale",
            Opt::GroupHudIconScale => "group_hud_icon_scale",
            Opt::HudIconPosX => "hud_icon_pos_x",
            Opt::HudIconPosY => "hud_icon_pos_y",
            Opt::GroupPlayerIconPosX => "group_player_icon_pos_x",
            Opt::GroupPlayerIconPosY => "group_player_icon_pos_y",
        }
    }

    pub fn kind(self) -> OptKind {
        match self {
            Opt::VoiceDeactivationDelay | Opt::PttDeactivationDelay => {
                OptKind::Int { min: 0, max: 100 }
            }
            Opt::OutputBufferSize => OptKind::Int { min: 1, max: 16 },
            Opt::AudioPacketThreshold => OptKind::Int { min: 0, max: 16 },
            Opt::HudIconPosX
            | Opt::HudIconPosY
            | Opt::GroupPlayerIconPosX
            | Opt::GroupPlayerIconPosY => OptKind::Int {
                min: i32::MIN,
                max: i32::MAX,
            },
            Opt::HudIconScale | Opt::GroupHudIconScale => OptKind::Scale {
                min: 0.01,
                max: 10.0,
            },
            Opt::AudioType | Opt::GroupPlayerIconOrientation => OptKind::Choice,
            _ => OptKind::Toggle,
        }
    }

    /// The range a slider offers. Positions are wider than a screen needs but not the whole
    /// range of a number; the file can still hold any.
    pub fn slider(self) -> (f32, f32) {
        match self.kind() {
            OptKind::Int { min, .. } if min == i32::MIN => (-300.0, 300.0),
            OptKind::Int { min, max } => (min as f32, max as f32),
            OptKind::Scale { .. } => (0.25, 4.0),
            _ => (0.0, 1.0),
        }
    }
}

/// The values, with SVC's defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Frames (20 ms each) the microphone stays on after the voice falls quiet.
    pub voice_deactivation_delay: i32,
    /// Frames the microphone stays on after the push-to-talk key is let go.
    pub ptt_deactivation_delay: i32,
    /// Voice frames collected per speaker before playing them.
    pub output_buffer_size: i32,
    /// Frames held back when one arrives out of order or is missing.
    pub audio_packet_threshold: i32,
    pub audio_type: AudioType,
    pub mute_on_join: bool,
    pub offline_player_volume_adjustment: bool,
    pub show_fake_players_disconnected: bool,
    pub hide_icons: bool,
    pub show_nametag_icons: bool,
    pub show_hud_icons: bool,
    pub show_group_hud: bool,
    pub show_own_group_icon: bool,
    pub group_player_icon_orientation: Orientation,
    pub hud_icon_scale: f32,
    pub group_hud_icon_scale: f32,
    pub hud_icon_pos_x: i32,
    pub hud_icon_pos_y: i32,
    pub group_player_icon_pos_x: i32,
    pub group_player_icon_pos_y: i32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            voice_deactivation_delay: 25,
            ptt_deactivation_delay: 5,
            output_buffer_size: 5,
            audio_packet_threshold: 3,
            audio_type: AudioType::Normal,
            mute_on_join: false,
            offline_player_volume_adjustment: false,
            show_fake_players_disconnected: false,
            hide_icons: false,
            show_nametag_icons: true,
            show_hud_icons: true,
            show_group_hud: true,
            show_own_group_icon: true,
            group_player_icon_orientation: Orientation::Vertical,
            hud_icon_scale: 1.0,
            group_hud_icon_scale: 2.0,
            hud_icon_pos_x: 16,
            hud_icon_pos_y: -16,
            group_player_icon_pos_x: 4,
            group_player_icon_pos_y: 4,
        }
    }
}

impl Options {
    /// Reads the options from the settings file's lines; anything missing, unreadable or out
    /// of range is the default or the nearest value in range.
    pub fn from_file(file: &HashMap<String, String>) -> Self {
        let mut o = Options::default();
        for opt in ALL {
            if let Some(v) = file.get(opt.key()) {
                o.set_text(opt, v);
            }
        }
        o
    }

    /// The options as settings file lines.
    pub fn write_to(&self, file: &mut HashMap<String, String>) {
        for opt in ALL {
            file.insert(opt.key().to_owned(), self.text(opt));
        }
    }

    fn text(&self, opt: Opt) -> String {
        match opt {
            Opt::AudioType => self.audio_type.name().to_owned(),
            Opt::GroupPlayerIconOrientation => self.group_player_icon_orientation.name().to_owned(),
            Opt::HudIconScale => self.hud_icon_scale.to_string(),
            Opt::GroupHudIconScale => self.group_hud_icon_scale.to_string(),
            _ => match opt.kind() {
                OptKind::Toggle => self.flag(opt).to_string(),
                _ => self.int(opt).to_string(),
            },
        }
    }

    fn set_text(&mut self, opt: Opt, text: &str) {
        match opt {
            Opt::AudioType => {
                if let Some(t) = AudioType::parse(text) {
                    self.audio_type = t;
                }
            }
            Opt::GroupPlayerIconOrientation => {
                if let Some(t) = Orientation::parse(text) {
                    self.group_player_icon_orientation = t;
                }
            }
            _ => match opt.kind() {
                OptKind::Toggle => {
                    if let Ok(v) = text.parse::<bool>() {
                        self.set_flag(opt, v);
                    }
                }
                OptKind::Int { .. } => {
                    if let Ok(v) = text.parse::<i32>() {
                        self.set_number(opt, v as f32, Some(v));
                    }
                }
                _ => {
                    if let Ok(v) = text.parse::<f32>() {
                        self.set_number(opt, v, None);
                    }
                }
            },
        }
    }

    fn flag(&self, opt: Opt) -> bool {
        match opt {
            Opt::MuteOnJoin => self.mute_on_join,
            Opt::OfflinePlayerVolumeAdjustment => self.offline_player_volume_adjustment,
            Opt::ShowFakePlayersDisconnected => self.show_fake_players_disconnected,
            Opt::HideIcons => self.hide_icons,
            Opt::ShowNametagIcons => self.show_nametag_icons,
            Opt::ShowHudIcons => self.show_hud_icons,
            Opt::ShowGroupHud => self.show_group_hud,
            Opt::ShowOwnGroupIcon => self.show_own_group_icon,
            _ => false,
        }
    }

    fn set_flag(&mut self, opt: Opt, v: bool) {
        match opt {
            Opt::MuteOnJoin => self.mute_on_join = v,
            Opt::OfflinePlayerVolumeAdjustment => self.offline_player_volume_adjustment = v,
            Opt::ShowFakePlayersDisconnected => self.show_fake_players_disconnected = v,
            Opt::HideIcons => self.hide_icons = v,
            Opt::ShowNametagIcons => self.show_nametag_icons = v,
            Opt::ShowHudIcons => self.show_hud_icons = v,
            Opt::ShowGroupHud => self.show_group_hud = v,
            Opt::ShowOwnGroupIcon => self.show_own_group_icon = v,
            _ => {}
        }
    }

    fn int(&self, opt: Opt) -> i32 {
        match opt {
            Opt::VoiceDeactivationDelay => self.voice_deactivation_delay,
            Opt::PttDeactivationDelay => self.ptt_deactivation_delay,
            Opt::OutputBufferSize => self.output_buffer_size,
            Opt::AudioPacketThreshold => self.audio_packet_threshold,
            Opt::HudIconPosX => self.hud_icon_pos_x,
            Opt::HudIconPosY => self.hud_icon_pos_y,
            Opt::GroupPlayerIconPosX => self.group_player_icon_pos_x,
            Opt::GroupPlayerIconPosY => self.group_player_icon_pos_y,
            _ => 0,
        }
    }

    /// Sets a number option, clamped to its range; `whole` is the exact value of a file's
    /// integer so positions past what an `f32` holds survive.
    fn set_number(&mut self, opt: Opt, v: f32, whole: Option<i32>) {
        match opt.kind() {
            OptKind::Int { min, max } => {
                let n = whole.unwrap_or(v.round() as i32).clamp(min, max);
                match opt {
                    Opt::VoiceDeactivationDelay => self.voice_deactivation_delay = n,
                    Opt::PttDeactivationDelay => self.ptt_deactivation_delay = n,
                    Opt::OutputBufferSize => self.output_buffer_size = n,
                    Opt::AudioPacketThreshold => self.audio_packet_threshold = n,
                    Opt::HudIconPosX => self.hud_icon_pos_x = n,
                    Opt::HudIconPosY => self.hud_icon_pos_y = n,
                    Opt::GroupPlayerIconPosX => self.group_player_icon_pos_x = n,
                    Opt::GroupPlayerIconPosY => self.group_player_icon_pos_y = n,
                    _ => {}
                }
            }
            OptKind::Scale { min, max } if v.is_finite() => {
                let v = v.clamp(min, max);
                match opt {
                    Opt::HudIconScale => self.hud_icon_scale = v,
                    Opt::GroupHudIconScale => self.group_hud_icon_scale = v,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// A slider moved to `v` (a number option), or a toggle or choice is clicked (`v` ignored).
    pub fn set(&mut self, opt: Opt, v: f32) {
        match opt.kind() {
            OptKind::Toggle => self.set_flag(opt, !self.flag(opt)),
            OptKind::Choice => match opt {
                Opt::AudioType => self.audio_type = self.audio_type.next(),
                _ => self.group_player_icon_orientation = self.group_player_icon_orientation.next(),
            },
            _ => self.set_number(opt, v, None),
        }
    }

    /// The value a slider shows.
    pub fn number(&self, opt: Opt) -> f32 {
        match opt {
            Opt::HudIconScale => self.hud_icon_scale,
            Opt::GroupHudIconScale => self.group_hud_icon_scale,
            _ => self.int(opt) as f32,
        }
    }

    /// A toggle's state.
    pub fn on(&self, opt: Opt) -> bool {
        self.flag(opt)
    }

    /// A choice's current value, as the name its label is looked up by (lower case).
    pub fn choice(&self, opt: Opt) -> &'static str {
        match opt {
            Opt::AudioType => match self.audio_type {
                AudioType::Normal => "normal",
                AudioType::Reduced => "reduced",
                AudioType::Off => "off",
            },
            _ => match self.group_player_icon_orientation {
                Orientation::Vertical => "vertical",
                Orientation::Horizontal => "horizontal",
            },
        }
    }
}

/// Where an icon group of `size` goes along one axis of a screen `extent` long: from the
/// near edge when `pos` is not negative, else that far from the far edge (SVC: "negative
/// values mean anchoring to the right/bottom instead").
pub fn anchored(pos: i32, size: f32, extent: f32) -> f32 {
    if pos >= 0 {
        pos as f32
    } else {
        extent + pos as f32 - size
    }
}

/// Every option, in the order the settings file lists them.
pub const ALL: [Opt; 20] = [
    Opt::VoiceDeactivationDelay,
    Opt::PttDeactivationDelay,
    Opt::OutputBufferSize,
    Opt::AudioPacketThreshold,
    Opt::AudioType,
    Opt::MuteOnJoin,
    Opt::OfflinePlayerVolumeAdjustment,
    Opt::ShowFakePlayersDisconnected,
    Opt::HideIcons,
    Opt::ShowNametagIcons,
    Opt::ShowHudIcons,
    Opt::ShowGroupHud,
    Opt::ShowOwnGroupIcon,
    Opt::GroupPlayerIconOrientation,
    Opt::HudIconScale,
    Opt::GroupHudIconScale,
    Opt::HudIconPosX,
    Opt::HudIconPosY,
    Opt::GroupPlayerIconPosX,
    Opt::GroupPlayerIconPosY,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn file(lines: &[(&str, &str)]) -> HashMap<String, String> {
        lines
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_are_the_originals() {
        let o = Options::from_file(&HashMap::new());
        assert_eq!(o, Options::default());
        assert_eq!(
            (o.voice_deactivation_delay, o.ptt_deactivation_delay),
            (25, 5)
        );
        assert_eq!((o.output_buffer_size, o.audio_packet_threshold), (5, 3));
        assert!(
            !o.hide_icons
                && o.show_nametag_icons
                && o.show_hud_icons
                && o.show_group_hud
                && o.show_own_group_icon
        );
        assert!(
            !o.mute_on_join
                && !o.offline_player_volume_adjustment
                && !o.show_fake_players_disconnected
        );
        assert_eq!(
            (o.hud_icon_pos_x, o.hud_icon_pos_y, o.hud_icon_scale),
            (16, -16, 1.0)
        );
        assert_eq!(
            (
                o.group_player_icon_pos_x,
                o.group_player_icon_pos_y,
                o.group_hud_icon_scale
            ),
            (4, 4, 2.0)
        );
    }

    #[test]
    fn the_file_round_trips_and_out_of_range_values_are_pulled_in() {
        let mut o = Options::default();
        o.set(Opt::AudioType, 0.0);
        o.set(Opt::HideIcons, 0.0);
        o.set(Opt::HudIconPosX, -40.0);
        let mut f = HashMap::new();
        o.write_to(&mut f);
        assert_eq!(f["audio_type"], "REDUCED");
        assert_eq!(Options::from_file(&f), o);

        let bad = Options::from_file(&file(&[
            ("voice_deactivation_delay", "500"),
            ("output_buffer_size", "0"),
            ("audio_packet_threshold", "-3"),
            ("hud_icon_scale", "99"),
            ("group_hud_icon_scale", "nan"),
            ("audio_type", "loud"),
            ("hide_icons", "yes"),
            ("hud_icon_pos_x", "-2000000000"),
        ]));
        assert_eq!(bad.voice_deactivation_delay, 100);
        assert_eq!(bad.output_buffer_size, 1);
        assert_eq!(bad.audio_packet_threshold, 0);
        assert_eq!(bad.hud_icon_scale, 10.0);
        assert_eq!(
            bad.group_hud_icon_scale, 2.0,
            "unreadable keeps the default"
        );
        assert_eq!(bad.audio_type, AudioType::Normal);
        assert!(!bad.hide_icons);
        assert_eq!(
            bad.hud_icon_pos_x, -2_000_000_000,
            "positions are whole numbers of any size"
        );
    }

    #[test]
    fn controls_edit_their_option() {
        let mut o = Options::default();
        o.set(Opt::VoiceDeactivationDelay, 40.4);
        assert_eq!(o.voice_deactivation_delay, 40);
        o.set(Opt::OutputBufferSize, 99.0);
        assert_eq!(o.output_buffer_size, 16);
        o.set(Opt::MuteOnJoin, 0.0);
        assert!(o.mute_on_join);
        o.set(Opt::MuteOnJoin, 0.0);
        assert!(!o.mute_on_join);
        for expected in [AudioType::Reduced, AudioType::Off, AudioType::Normal] {
            o.set(Opt::AudioType, 0.0);
            assert_eq!(o.audio_type, expected);
        }
        o.set(Opt::GroupPlayerIconOrientation, 0.0);
        assert_eq!(o.choice(Opt::GroupPlayerIconOrientation), "horizontal");
        o.set(Opt::HudIconScale, 0.0);
        assert_eq!(o.hud_icon_scale, 0.01);
    }

    #[test]
    fn negative_positions_anchor_to_the_far_edge() {
        assert_eq!(anchored(16, 20.0, 400.0), 16.0);
        assert_eq!(anchored(0, 20.0, 400.0), 0.0);
        assert_eq!(
            anchored(-16, 20.0, 400.0),
            364.0,
            "16 from the far edge, the icons' far side"
        );
    }
}
