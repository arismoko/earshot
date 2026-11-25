//! Earshot's client: the voice client's state between hooks, the settings it keeps, the
//! HUD, the name tag icons and the voice menu (V). Everything that doesn't need the game
//! is in `earshot_core` (the menu's layout and behaviour and the icons included:
//! `earshot_core::ui`).
//!
//! Earshot is an independent project, not affiliated with Simple Voice Chat.
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use earshot_core::activation::{Mode, PttKey};
use earshot_core::engine::{GroupInfo, Settings, Status, VoiceClient};
use earshot_core::menu::{Menu, MenuAction, MenuKey, MenuModel, Page};
use earshot_core::options::{self, OptKind, Options, Orientation};
use earshot_core::plugin;
use earshot_core::spatial::Listener;
use earshot_core::ui::{self, Draw, MicState};
use earshot_core::wire::Uuid;
use mod_api::{ClientHost, DrawContext, Host, Identifier, ServerboundPlayPacket, SettingKey};

pub const ID: &str = "earshot";

/// The keys: id, English label, default.
pub const KEY_MENU: (&str, &str, &str) = ("menu", "Voice menu", "key.keyboard.v");
pub const KEY_PTT: (&str, &str, &str) = ("push_to_talk", "Push to talk", "key.mouse.4");
/// Held: talk as a whisper, heard only nearby (capture 07). The other side button by default.
pub const KEY_WHISPER: (&str, &str, &str) = ("whisper", "Whisper", "key.mouse.5");
pub const KEY_MUTE: (&str, &str, &str) = ("mute", "Mute microphone", "key.keyboard.m");
/// Earshot's own, not in the original: deafen. Unbound until the player picks a key.
pub const KEY_DEAFEN: (&str, &str, &str) = ("deafen", "Deafen", UNBOUND);
/// The original's other keys: turn voice chat off, hide the icons, open the settings, the
/// groups and the volume list directly.
pub const KEY_DISABLE: (&str, &str, &str) = ("disable", "Disable voice chat", "key.keyboard.n");
pub const KEY_HIDE_ICONS: (&str, &str, &str) =
    ("hide_icons", "Hide voice chat icons", "key.keyboard.h");
pub const KEY_SETTINGS: (&str, &str, &str) = ("settings", "Voice chat settings", UNBOUND);
pub const KEY_GROUP: (&str, &str, &str) = ("group", "Group management", "key.keyboard.g");
pub const KEY_VOLUMES: (&str, &str, &str) = ("adjust_volumes", "Adjust volumes", UNBOUND);
pub const KEYS: [(&str, &str, &str); 10] = [
    KEY_MENU,
    KEY_PTT,
    KEY_WHISPER,
    KEY_MUTE,
    KEY_DEAFEN,
    KEY_DISABLE,
    KEY_HIDE_ICONS,
    KEY_SETTINGS,
    KEY_GROUP,
    KEY_VOLUMES,
];
const UNBOUND: &str = "key.keyboard.unknown";

/// The settings page's two switches (the menu holds the rest).
pub const SETTING_VOICE: &str = "voice_chat";
pub const SETTING_DEBUG: &str = "debug";

/// The keys `register_setting` gave the two switches, kept for reading their live values.
/// `None` for a switch the host refused to register: it reads as its default.
#[derive(Clone, Copy, Default)]
pub struct SettingKeys {
    pub voice: Option<SettingKey>,
    pub debug: Option<SettingKey>,
}

static SETTING_KEYS: Mutex<SettingKeys> = Mutex::new(SettingKeys {
    voice: None,
    debug: None,
});

/// Keeps the keys issued by this registration pass (a reinstall issues new ones).
pub fn set_setting_keys(keys: SettingKeys) {
    *SETTING_KEYS.lock().unwrap_or_else(|e| e.into_inner()) = keys;
}

/// The settings page's two switches: whether voice is off, and whether debug info is on.
fn switches(host: &dyn Host) -> (bool, bool) {
    let keys = *SETTING_KEYS.lock().unwrap_or_else(|e| e.into_inner());
    let read = |key: Option<SettingKey>, default: bool| {
        key.and_then(|key| host.setting_bool(key).ok())
            .unwrap_or(default)
    };
    (!read(keys.voice, true), read(keys.debug, false))
}

/// Earshot's data folder: `sunrise-mod-data/earshot` in the instance folder.
fn data_dir(host: &dyn Host) -> PathBuf {
    host.instance_dir().join("sunrise-mod-data").join(ID)
}

/// Players farther than this aren't listed in the menu (SVC's default voice distance).
const MENU_PLAYER_RANGE: f64 = 48.0;
/// The connecting line's distance from the screen's top left corner, in GUI pixels.
const HUD_MARGIN: f32 = 2.0;
/// The file the menu's settings and the players' volumes are kept in (the mod's data folder).
const SETTINGS_FILE: &str = "client.txt";

struct State {
    client: VoiceClient,
    settings: Settings,
    threshold: f32,
    /// The push-to-talk key (hold or toggle), and whether it says talk right now.
    ptt_key: PttKey,
    ptt: bool,
    /// The push-to-talk key's state last tick, to see presses and releases.
    ptt_down: bool,
    /// The whisper key is held.
    whisper: bool,
    /// The mic test is on (only while the menu is open).
    mic_test: bool,
    muted: bool,
    deafened: bool,
    disabled: bool,
    menu: Menu,
    /// Per-player volumes, kept in the settings file.
    volumes: HashMap<Uuid, f32>,
    /// Show the diagnostics (frame counters, UDP, devices). Off unless switched on.
    debug: bool,
    /// The settings file's other lines, as read.
    file: HashMap<String, String>,
    /// The players the voice server told of, by entity id, for the name tag icons.
    entities: HashMap<u32, Uuid>,
    /// The players' names, kept beside their volumes so offline players can be listed.
    names: HashMap<Uuid, String>,
    /// The world was being played last tick (to see joining it).
    was_playing: bool,
    /// The menu key was pressed on the menu: it closes on the next tick (closing a screen
    /// needs the tick's host).
    close_menu: bool,
}

thread_local! {
    // The window's game runs on one thread; the server's hooks never get here.
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn with_state<R>(host: &dyn Host, f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|cell| {
        let mut slot = cell.borrow_mut();
        let state = slot.get_or_insert_with(|| load(host));
        f(state)
    })
}

/// Only if the client already exists: drawing shouldn't start threads.
fn with_existing<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE.with(|cell| cell.borrow_mut().as_mut().map(f))
}

/// `key=value` lines.
fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

fn read_file(host: &dyn Host) -> HashMap<String, String> {
    std::fs::read_to_string(data_dir(host).join(SETTINGS_FILE))
        .ok()
        .map(|t| parse(&t))
        .unwrap_or_default()
}

fn load(host: &dyn Host) -> State {
    let file = read_file(host);
    let get = |k: &str| file.get(k).cloned();
    let num = |k: &str, d: f32| get(k).and_then(|v| v.parse::<f32>().ok()).unwrap_or(d);
    let flag = |k: &str| get(k).as_deref() == Some("true");
    let threshold = num("threshold", -45.0);
    let settings = Settings {
        input_device: get("input_device").filter(|s| !s.is_empty()),
        output_device: get("output_device").filter(|s| !s.is_empty()),
        mic_volume: num("mic_volume", 1.0),
        output_volume: num("output_volume", 1.0),
        mode: if get("mode").as_deref() == Some("voice") {
            Mode::Voice {
                threshold_dbfs: threshold,
            }
        } else {
            Mode::PushToTalk
        },
        noise_suppression: get("noise").as_deref() != Some("false"),
        group_volume: num("group_volume", 1.0),
        group_sounds: get("group_sounds").as_deref() != Some("false"),
        use_audio_devices: true,
        options: Options::from_file(&file),
    };
    let volumes = file
        .iter()
        .filter_map(|(k, v)| {
            Some((
                Uuid::parse(k.strip_prefix("volume.")?)?,
                v.parse::<f32>().ok()?,
            ))
        })
        .collect::<HashMap<_, _>>();
    let names = file
        .iter()
        .filter_map(|(k, v)| Some((Uuid::parse(k.strip_prefix("name.")?)?, v.clone())))
        .collect::<HashMap<_, _>>();
    let client = VoiceClient::new(settings.clone());
    client.apply_settings(settings.clone());
    let (muted, deafened) = (flag("muted"), flag("deafened"));
    let (disabled, debug) = switches(host);
    client.set_muted(muted);
    client.set_deafened(deafened);
    client.set_disabled(disabled);
    for (id, v) in &volumes {
        client.set_player_volume(*id, *v);
    }
    let ptt_key = PttKey::new(flag("ptt_toggle"));
    State {
        client,
        settings,
        threshold,
        ptt_key,
        ptt: false,
        ptt_down: false,
        whisper: false,
        mic_test: false,
        muted,
        deafened,
        disabled,
        menu: Menu::new(),
        volumes,
        debug,
        file,
        entities: HashMap::new(),
        names,
        was_playing: false,
        close_menu: false,
    }
}

/// Keeps the menu's settings and the players' volumes in the settings file.
fn save(host: &dyn Host, s: &mut State) {
    let mut set = |k: &str, v: String| {
        s.file.insert(k.to_owned(), v);
    };
    set(
        "mode",
        if matches!(s.settings.mode, Mode::Voice { .. }) {
            "voice"
        } else {
            "ptt"
        }
        .into(),
    );
    set("threshold", s.threshold.to_string());
    set("mic_volume", s.settings.mic_volume.to_string());
    set("output_volume", s.settings.output_volume.to_string());
    set(
        "input_device",
        s.settings.input_device.clone().unwrap_or_default(),
    );
    set(
        "output_device",
        s.settings.output_device.clone().unwrap_or_default(),
    );
    set("noise", s.settings.noise_suppression.to_string());
    set("ptt_toggle", s.ptt_key.toggle.to_string());
    set("group_volume", s.settings.group_volume.to_string());
    set("group_sounds", s.settings.group_sounds.to_string());
    set("muted", s.muted.to_string());
    set("deafened", s.deafened.to_string());
    s.settings.options.write_to(&mut s.file);
    for (id, v) in &s.volumes {
        s.file.insert(format!("volume.{id}"), v.to_string());
        if let Some(name) = s.names.get(id) {
            s.file.insert(format!("name.{id}"), name.clone());
        }
    }
    let mut lines: Vec<String> = s.file.iter().map(|(k, v)| format!("{k}={v}")).collect();
    lines.sort();
    let dir = data_dir(host);
    if let Err(e) = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(dir.join(SETTINGS_FILE), lines.join("\n") + "\n"))
    {
        eprintln!("[earshot] couldn't keep the settings: {e}");
    }
}

/// The host part of `host:port` (or `[v6]:port`), as the player typed it.
pub fn host_of(address: &str) -> String {
    if let Some(rest) = address.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest).to_string();
    }
    match address.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.parse::<u16>().is_ok() => {
            host.to_string()
        }
        _ => address.to_string(),
    }
}

/// `key.keyboard.v` -> `V`, `key.mouse.4` -> `Mouse 4`.
pub fn key_label(key: &str) -> String {
    if let Some(n) = key.strip_prefix("key.mouse.") {
        return format!("Mouse {}", n.replace('.', " "));
    }
    let k = key.strip_prefix("key.keyboard.").unwrap_or(key);
    k.split('.')
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A custom payload's packet body: the channel as a string (VarInt length, UTF-8), then
/// the bytes.
pub fn payload_body(channel: &str, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(channel.len() + data.len() + 5);
    let mut n = channel.len() as u32;
    loop {
        let byte = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out.extend_from_slice(channel.as_bytes());
    out.extend_from_slice(data);
    out
}

/// Sends what the voice client wants to say over the game connection.
pub fn flush(host: &mut dyn ClientHost) {
    let Some(out) = with_existing(|s| s.client.take_outgoing()) else {
        return;
    };
    let Ok(id) = Identifier::parse("minecraft:custom_payload") else {
        return;
    };
    for (channel, data) in out {
        let packet = ServerboundPlayPacket::new(id.clone(), payload_body(channel, &data));
        if let Err(e) = host.send_packet(packet) {
            eprintln!("[earshot] couldn't send {channel}: {e:?}");
        }
    }
}

// --- connection -------------------------------------------------------------------------------

/// A custom payload from the server.
pub fn plugin_message(host: &mut dyn ClientHost, channel: &str, data: &[u8], server: &str) {
    if channel != plugin::MINECRAFT_REGISTER && !channel.starts_with("voicechat:") {
        return;
    }
    let host_name = host_of(server);
    with_state(&*host, |s| s.client.on_plugin(channel, data, &host_name));
    flush(host);
}

/// A chat line from the server, before it shows: voice chat lines worded as
/// `message.voicechat.` keys only its own client has text for get Earshot's own wording
/// (`earshot_core::chat`), with the server's arguments and click actions kept.
pub fn chat_line(host: &dyn ClientHost, message: &mut mod_api::Text) {
    let component = crate::text_json::to_json(message);
    let translated = |key: &str| host.translate(key).is_some();
    let Some(rewritten) = earshot_core::chat::rewrite(&component, &translated) else {
        return;
    };
    if with_state(host, |s| s.debug) {
        eprintln!("[earshot] voice chat line from the server: {component}");
    }
    *message = crate::text_json::from_json(&rewritten);
}

/// The players the voice client knows of: their entity, and where their voice comes from.
fn players(host: &dyn ClientHost, status: &Status) -> Vec<(Uuid, u32, [f64; 3])> {
    let mut ids: Vec<Uuid> = status
        .has_voice
        .iter()
        .chain(&status.voice_off)
        .chain(&status.talking)
        .copied()
        .collect();
    if let Some(g) = &status.my_group {
        ids.extend(g.members.iter().map(|m| m.0));
    }
    ids.sort();
    ids.dedup();
    let reads: &dyn Host = host;
    ids.into_iter()
        .filter(|id| Some(*id) != status.me)
        .filter_map(|id| {
            let entity = reads.player(u128::from_be_bytes(id.0))?.entity();
            let pos = entity.position(reads)?;
            let height = entity.dimensions(reads).map_or(1.8, |d| d[1]);
            // Voices come from the head, not the feet.
            Some((id, entity.id(), [pos[0], pos[1] + height * 0.9, pos[2]]))
        })
        .collect()
}

/// Whether Earshot's key `id` is held (`KeyMapping.isDown`).
fn key_down(host: &dyn ClientHost, id: &str) -> bool {
    host.mod_key_mapping(ID, id)
        .is_ok_and(|key| key.is_down(host).unwrap_or(false))
}

/// Takes one press of Earshot's key `id` (`KeyMapping.consumeClick`).
fn key_clicked(host: &mut dyn ClientHost, id: &str) -> bool {
    host.mod_key_mapping(ID, id)
        .is_ok_and(|key| key.consume_click(host).unwrap_or(false))
}

/// The binding name of Earshot's key, or its default when it can't be read.
fn key_binding(host: &dyn ClientHost, key: (&str, &str, &str)) -> String {
    host.mod_key_mapping(ID, key.0)
        .ok()
        .and_then(|mapping| mapping.binding(host).ok().map(|(name, _)| name.to_owned()))
        .unwrap_or_else(|| key.2.to_owned())
}

/// Once a tick: the keys, positions, push-to-talk, and anything to send.
pub fn tick(host: &mut dyn ClientHost) {
    let reads: &dyn ClientHost = &*host;
    let playing = reads.playing();
    let menu_open = reads.mod_screen() == Some(ID);
    let listener = reads.camera().map(|c| Listener {
        pos: c.position,
        yaw_degrees: c.yaw,
    });
    let ptt_down = key_down(reads, KEY_PTT.0);
    let whisper_down = key_down(reads, KEY_WHISPER.0);
    let status = with_state(reads, |s| s.client.status());
    let players = players(reads, &status);
    let mut joined_muted = false;
    let me = status.me;
    with_state(reads, |s| {
        // Remember the names of players whose volumes we keep.
        for (id, name) in &status.player_names {
            if s.volumes.contains_key(id) && s.names.get(id) != Some(name) && Some(*id) != me {
                s.names.insert(*id, name.clone());
            }
        }
        // Joining a world: start muted if asked to.
        if playing && !s.was_playing && s.settings.options.mute_on_join && !s.muted {
            s.muted = true;
            s.client.set_muted(true);
            joined_muted = true;
        }
        s.was_playing = playing;
        // The mic test lasts only while the menu is open.
        if s.mic_test && !menu_open {
            s.mic_test = false;
            s.client.set_mic_test(false);
        }
        // Typing in chat or a sign mustn't key the microphone: presses count only while playing.
        if ptt_down != s.ptt_down {
            s.ptt_key.key(ptt_down, playing);
            s.ptt_down = ptt_down;
        }
        s.ptt = s.ptt_key.is_on();
        s.whisper = whisper_down && playing;
        s.entities = players.iter().map(|p| (p.1, p.0)).collect();
        let positions = players.iter().map(|p| (p.0, p.2)).collect();
        s.client.tick(listener, positions, s.ptt, s.whisper);
    });
    if with_existing(|s| std::mem::take(&mut s.close_menu)) == Some(true) {
        host.mod_screen_close(ID);
    }
    // The keys pressed while playing: the menu, mute and deafen.
    if key_clicked(host, KEY_MENU.0) {
        // Start on the main screen each time.
        open_page(host, Page::Main);
    }
    if key_clicked(host, KEY_SETTINGS.0) {
        open_page(host, Page::Settings);
    }
    if key_clicked(host, KEY_GROUP.0) {
        open_page(host, Page::Groups);
    }
    if key_clicked(host, KEY_VOLUMES.0) {
        open_page(host, Page::Players);
    }
    let mut changed = joined_muted;
    while key_clicked(host, KEY_HIDE_ICONS.0) {
        let hidden = with_state(&*host, |s| {
            let o = &mut s.settings.options;
            o.hide_icons = !o.hide_icons;
            let hidden = o.hide_icons;
            s.client.apply_settings(s.settings.clone());
            hidden
        });
        let key = if hidden {
            "earshot.message.icons_hidden"
        } else {
            "earshot.message.icons_visible"
        };
        let _ = host.set_overlay_message(mod_api::Text::translate(key, vec![]), false);
        changed = true;
    }
    while key_clicked(host, KEY_DISABLE.0) {
        with_state(&*host, |s| {
            s.disabled = !s.disabled;
            s.client.set_disabled(s.disabled);
        });
    }
    while key_clicked(host, KEY_MUTE.0) {
        with_state(&*host, |s| {
            s.muted = !s.muted;
            s.client.set_muted(s.muted);
        });
        changed = true;
    }
    while key_clicked(host, KEY_DEAFEN.0) {
        with_state(&*host, |s| {
            s.deafened = !s.deafened;
            s.client.set_deafened(s.deafened);
        });
        changed = true;
    }
    if changed {
        let reads: &dyn Host = &*host;
        let _ = with_existing(|s| save(reads, s));
    }
    flush(host);
}

/// Opens the voice menu on `page` (the keys that go straight to a screen).
fn open_page(host: &mut dyn ClientHost, page: Page) {
    with_state(&*host, |s| {
        s.ptt_key.release();
        s.ptt = false;
        s.whisper = false;
        s.menu.open_at(page);
    });
    host.mod_screen_open(ID);
}

/// The settings page changed: voice on or off, debug info.
pub fn page_changed(host: &dyn Host) {
    let (disabled, debug) = switches(host);
    with_state(host, |s| {
        s.disabled = disabled;
        s.client.set_disabled(disabled);
        s.debug = debug;
    });
}

/// Before the connection closes.
pub fn disconnecting() {
    let _ = with_existing(|s| {
        s.ptt_key.release();
        s.ptt = false;
        s.whisper = false;
        let _ = s.menu.release();
        s.client.disconnect();
        s.entities.clear();
    });
}

// --- HUD and name tags --------------------------------------------------------------------------

fn width(host: &dyn ClientHost, text: &str) -> f32 {
    host.text_width(&mod_api::Text::literal(text)) as f32
}

fn paint(draw: &mut dyn DrawContext, draws: &[Draw]) {
    for d in draws {
        match d {
            Draw::Fill { rect, argb } => draw.fill(*rect, *argb),
            Draw::Quad { points, argb } => draw.quad(*points, *argb),
            Draw::Label { text, at, rgb } => draw.text(text, *at, 0xFF00_0000 | rgb, true),
            Draw::Scaled {
                text,
                at,
                rgb,
                scale,
            } => draw.text_scaled(text, *at, *scale, 0xFF00_0000 | rgb, true),
            // A refused face leaves the rest drawn.
            Draw::Face { id, at, size } => {
                let _ = draw.draw_gui(&mut |canvas| {
                    canvas.player_face(
                        u128::from_be_bytes(id.0),
                        *at,
                        *size,
                        true,
                        false,
                        0xFFFF_FFFF,
                    )
                });
            }
        }
    }
}

fn mic_state(s: &Status) -> MicState {
    if s.disabled {
        MicState::Off
    } else if s.muted {
        MicState::Muted
    } else if s.whispering {
        MicState::Whispering
    } else if s.transmitting {
        MicState::Talking
    } else {
        MicState::Idle
    }
}

/// The HUD: the microphone icon, a line while connecting, and in a voice group the group
/// panel (its members, who's talking lit up). Where they go, their size and whether they
/// show come from the icon options.
pub fn hud(host: &dyn ClientHost, draw: &mut dyn DrawContext) {
    let Some((status, o)) = with_existing(|s| (s.client.status(), s.settings.options.clone()))
    else {
        return;
    };
    if status.connection == "no voice server" || o.hide_icons {
        return;
    }
    if !status.connected {
        if o.show_hud_icons {
            let text = format!("Voice: {}", status.connection);
            let w = width(host, &text) + 8.0;
            let (x, y) = (HUD_MARGIN, HUD_MARGIN);
            draw.fill([x, y, w, 13.0], 0x7800_0000);
            draw.text(&text, [x + 4.0, y + 3.0], 0xFFFF_AA55, true);
        }
        return;
    }
    let gui = draw.size();
    if o.show_hud_icons {
        let (size, draws) = mic_cluster(host, &status);
        let scale = o.hud_icon_scale;
        let at = [
            options::anchored(o.hud_icon_pos_x, size[0] * scale, gui[0]),
            options::anchored(o.hud_icon_pos_y, size[1] * scale, gui[1]),
        ];
        paint(draw, &ui::translate(ui::scale(draws, scale), at));
    }
    if o.show_group_hud
        && let Some((size, draws)) = group_panel(host, &status, &o)
    {
        let scale = o.group_hud_icon_scale / 2.0;
        let at = [
            options::anchored(o.group_player_icon_pos_x, size[0] * scale, gui[0]),
            options::anchored(o.group_player_icon_pos_y, size[1] * scale, gui[1]),
        ];
        paint(draw, &ui::translate(ui::scale(draws, scale), at));
    }
}

/// The microphone icon (and the deafened one, and the group's name): its size and draws with
/// its top-left at (0, 0).
fn mic_cluster(host: &dyn ClientHost, status: &Status) -> ([f32; 2], Vec<Draw>) {
    let icons = if status.deafened { 2.0 } else { 1.0 };
    let group = status
        .my_group
        .as_ref()
        .map(|g| format!("Group: {}", g.name));
    let text_w = group.as_ref().map(|t| width(host, t) + 8.0).unwrap_or(0.0);
    let mut draws = ui::mic_icon([2.0, 2.0], mic_state(status));
    if status.deafened {
        draws.extend(ui::deafened_icon([24.0, 2.0]));
    }
    if let Some(text) = group {
        let tx = 22.0 * icons + 2.0;
        draws.push(Draw::Fill {
            rect: [tx - 2.0, 0.0, text_w, 20.0],
            argb: 0x7800_0000,
        });
        draws.push(Draw::Label {
            text,
            at: [tx + 2.0, 6.0],
            rgb: 0x7CE38B,
        });
    }
    ([22.0 * icons + text_w, 20.0], draws)
}

/// In a voice group: its members, those talking now lit up with a speaker. Its size and
/// draws with its top-left at (0, 0).
fn group_panel(
    host: &dyn ClientHost,
    status: &Status,
    o: &Options,
) -> Option<([f32; 2], Vec<Draw>)> {
    let group = status.my_group.as_ref()?;
    let members: Vec<ui::PanelMember> = group
        .members
        .iter()
        .filter(|(id, _)| o.show_own_group_icon || status.me != Some(*id))
        .map(|(id, name)| {
            let me = status.me == Some(*id);
            // Our own voice isn't played back to us: we're talking while the microphone sends.
            let talking = if me {
                status.transmitting
            } else {
                status.talking.contains(id)
            };
            ui::PanelMember {
                id: *id,
                name: name.clone(),
                talking,
                me,
            }
        })
        .collect();
    Some(ui::group_panel(
        &format!("Group: {}", group.name),
        &members,
        o.group_player_icon_orientation == Orientation::Horizontal,
        &|t| width(host, t),
    ))
}

/// A name tag icon's lines (each from one point to another) and their colour.
pub type TagIcon = (Vec<([f32; 2], [f32; 2])>, u32);

/// The icon beside player `entity`'s name tag: a green speaker while they talk, a red one
/// if they turned voice chat off, a grey crossed-out one if they aren't connected to voice
/// (on a server that runs it) (`ui::tag_icon_for`). Its lines, in tag font pixels from the
/// icon's centre, and their colour.
pub fn tag_icon(entity: u32) -> Option<TagIcon> {
    with_existing(|s| {
        let o = &s.settings.options;
        if o.hide_icons || !o.show_nametag_icons {
            return None;
        }
        let status = s.client.status();
        let on_voice_server = status.connected;
        let id = s.entities.get(&entity).copied();
        let icon = match id {
            Some(id) => ui::tag_icon_for(
                status.talking.contains(&id),
                status.voice_off.contains(&id),
                status.has_voice.contains(&id),
                on_voice_server,
            ),
            // A player the server's states don't list (an NPC): marked only if asked to.
            None if on_voice_server && o.show_fake_players_disconnected => {
                ui::tag_icon_for(false, false, false, true)
            }
            None => None,
        }?;
        let (lines, argb) = ui::tag_icon(icon);
        Some((lines, argb))
    })
    .flatten()
}

// --- the voice menu (V) ----------------------------------------------------------------------

pub fn ours(host: &dyn ClientHost) -> bool {
    host.mod_screen() == Some(ID)
}

/// Nearby players for the menu, nearest first: the players the voice server told of.
fn nearby_players(host: &dyn ClientHost, status: &Status) -> Vec<(Uuid, String, f64)> {
    let Some(camera) = host.camera() else {
        return vec![];
    };
    let eye = camera.position;
    let reads: &dyn Host = host;
    let mut out: Vec<(Uuid, String, f64)> = players(host, status)
        .into_iter()
        .filter_map(|(id, _, at)| {
            let d =
                ((at[0] - eye[0]).powi(2) + (at[1] - eye[1]).powi(2) + (at[2] - eye[2]).powi(2))
                    .sqrt();
            let name = reads
                .player(u128::from_be_bytes(id.0))
                .and_then(|p| p.entity().name(reads))
                .unwrap_or_else(|| id.to_string());
            (d <= MENU_PLAYER_RANGE).then_some((id, name, d))
        })
        .collect();
    out.sort_by(|a, b| a.2.total_cmp(&b.2));
    out
}

fn device_label(chosen: &Option<String>, in_use: &str) -> String {
    match chosen {
        None => format!(
            "system default{}",
            if in_use.is_empty() {
                String::new()
            } else {
                format!(" ({in_use})")
            }
        ),
        Some(name) if in_use.contains("failed") || in_use.starts_with("none") => {
            format!("{name} - {in_use}")
        }
        Some(name) => name.clone(),
    }
}

/// A group's name for the list, with its type when it isn't normal.
fn group_label(g: &GroupInfo) -> String {
    match g.kind {
        plugin::GroupType::Normal => g.name.clone(),
        kind => format!("{} ({})", g.name, kind.label().to_lowercase()),
    }
}

/// Players with a saved volume who aren't on the server now, when their volumes may be adjusted.
fn offline_players(s: &State, status: &Status) -> Vec<(Uuid, String, f32)> {
    if !s.settings.options.offline_player_volume_adjustment {
        return vec![];
    }
    let mut out: Vec<(Uuid, String, f32)> = s
        .volumes
        .iter()
        .filter(|(id, _)| {
            !status.player_names.iter().any(|p| p.0 == **id) && status.me != Some(**id)
        })
        .map(|(id, v)| {
            (
                *id,
                s.names.get(id).cloned().unwrap_or_else(|| id.to_string()),
                *v,
            )
        })
        .collect();
    out.sort_by(|a, b| {
        a.1.to_lowercase()
            .cmp(&b.1.to_lowercase())
            .then(a.0.cmp(&b.0))
    });
    out
}

fn model(host: &dyn ClientHost, s: &State) -> MenuModel {
    let status = s.client.status();
    let players = nearby_players(host, &status)
        .into_iter()
        .map(|(id, name, _)| {
            let v = s.volumes.get(&id).copied().unwrap_or(1.0);
            (id, name, v, status.talking.contains(&id))
        })
        .collect();
    let connection = if status.connected && s.debug {
        format!(
            "Connected - sent {} / heard {} voice frames",
            status.mic_frames_sent, status.voice_frames_received
        )
    } else if status.connected {
        "Connected".to_string()
    } else {
        format!("Voice: {}", status.connection)
    };
    let ptt_key = key_binding(host, KEY_PTT);
    MenuModel {
        status: connection,
        connected: status.connected,
        muted: s.muted,
        deafened: s.deafened,
        disabled: s.disabled,
        push_to_talk: matches!(s.settings.mode, Mode::PushToTalk),
        ptt_toggle: s.ptt_key.toggle,
        mic_test: s.mic_test,
        group_volume: s.settings.group_volume,
        group_sounds: s.settings.group_sounds,
        ptt_key: key_label(&ptt_key),
        threshold_db: s.threshold,
        level_db: status.input_level_dbfs,
        noise_suppression: s.settings.noise_suppression,
        mic_volume: s.settings.mic_volume,
        voice_volume: s.settings.output_volume,
        microphone: device_label(&s.settings.input_device, &status.input_device),
        speakers: device_label(&s.settings.output_device, &status.output_device),
        players,
        offline: offline_players(s, &status),
        options: s.settings.options.clone(),
        group: status.my_group.as_ref().map(|g| g.name.clone()),
        groups: status
            .groups
            .iter()
            // A group that isn't normal says its type: "Raid (open)".
            .map(|g| {
                (
                    g.id,
                    group_label(g),
                    g.has_password,
                    g.members.len(),
                    status.my_group.as_ref().is_some_and(|m| m.id == g.id),
                )
            })
            .collect(),
        group_members: status
            .my_group
            .as_ref()
            .map(|g| {
                g.members
                    .iter()
                    .map(|(id, name)| (*id, name.clone(), status.talking.contains(id)))
                    .collect()
            })
            .unwrap_or_default(),
        group_problem: status.group_problem.clone(),
    }
}

fn next_device(devices: Vec<String>, current: &Option<String>) -> Option<String> {
    // System default, then each device in turn.
    let mut all: Vec<Option<String>> = vec![None];
    all.extend(devices.into_iter().map(Some));
    let i = all
        .iter()
        .position(|d| d == current)
        .map(|i| i + 1)
        .unwrap_or(0);
    all[i % all.len()].clone()
}

/// Applies a menu action. Returns whether it's a change worth saving now (sliders save when
/// let go instead, so a drag doesn't write the settings dozens of times).
fn apply(s: &mut State, action: MenuAction) -> bool {
    let mut settings_changed = true;
    let save_now = match action {
        MenuAction::ToggleMute => {
            s.muted = !s.muted;
            s.client.set_muted(s.muted);
            settings_changed = false;
            true
        }
        MenuAction::ToggleDeafen => {
            s.deafened = !s.deafened;
            s.client.set_deafened(s.deafened);
            settings_changed = false;
            true
        }
        MenuAction::ToggleVoiceChat => {
            s.disabled = !s.disabled;
            s.client.set_disabled(s.disabled);
            settings_changed = false;
            true
        }
        MenuAction::ToggleMode => {
            s.settings.mode = match s.settings.mode {
                Mode::PushToTalk => Mode::Voice {
                    threshold_dbfs: s.threshold,
                },
                Mode::Voice { .. } => Mode::PushToTalk,
            };
            true
        }
        MenuAction::ToggleNoiseSuppression => {
            s.settings.noise_suppression = !s.settings.noise_suppression;
            true
        }
        MenuAction::TogglePttStyle => {
            s.ptt_key = PttKey::new(!s.ptt_key.toggle);
            s.ptt = false;
            settings_changed = false;
            true
        }
        MenuAction::ToggleMicTest => {
            s.mic_test = !s.mic_test;
            s.client.set_mic_test(s.mic_test);
            settings_changed = false;
            false
        }
        MenuAction::ToggleGroupSounds => {
            s.settings.group_sounds = !s.settings.group_sounds;
            true
        }
        MenuAction::SetGroupVolume(v) => {
            s.settings.group_volume = v;
            false
        }
        MenuAction::SetThreshold(v) => {
            s.threshold = v;
            if let Mode::Voice { .. } = s.settings.mode {
                s.settings.mode = Mode::Voice { threshold_dbfs: v };
            }
            false
        }
        MenuAction::SetMicVolume(v) => {
            s.settings.mic_volume = v;
            false
        }
        MenuAction::SetVoiceVolume(v) => {
            s.settings.output_volume = v;
            false
        }
        MenuAction::NextMicrophone => {
            s.settings.input_device = next_device(
                earshot_core::audio::input_devices(),
                &s.settings.input_device,
            );
            true
        }
        MenuAction::NextSpeakers => {
            s.settings.output_device = next_device(
                earshot_core::audio::output_devices(),
                &s.settings.output_device,
            );
            true
        }
        MenuAction::JoinGroup(id, password) => {
            s.client.join_group(id, password.as_deref());
            settings_changed = false;
            false
        }
        MenuAction::CreateGroup {
            name,
            password,
            kind,
        } => {
            s.client.create_group(&name, password.as_deref(), kind);
            settings_changed = false;
            false
        }
        MenuAction::LeaveGroup => {
            s.client.leave_group();
            settings_changed = false;
            false
        }
        MenuAction::Set(opt, v) => {
            s.settings.options.set(opt, v);
            // Sliders save when let go; toggles and choices at once.
            !matches!(opt.kind(), OptKind::Int { .. } | OptKind::Scale { .. })
        }
        MenuAction::SetPlayerVolume(id, v) => {
            s.volumes.insert(id, v);
            s.client.set_player_volume(id, v);
            settings_changed = false;
            false
        }
    };
    if settings_changed {
        s.client.apply_settings(s.settings.clone());
    }
    save_now
}

/// The GUI's size in GUI pixels.
fn gui(draw_size: Option<[f32; 2]>, screen_rect: [f32; 4]) -> [f32; 2] {
    draw_size.unwrap_or([
        screen_rect[0] + screen_rect[2],
        screen_rect[1] + screen_rect[3],
    ])
}

/// Draws the menu.
pub fn screen_render(host: &dyn ClientHost, draw: &mut dyn DrawContext, mouse: [f32; 2]) {
    let size = draw.size();
    let draws = with_state(host, |s| {
        let m = model(host, s);
        s.menu.draw(&m, size, mouse, &|t: &str| width(host, t))
    });
    paint(draw, &draws);
}

/// A click in the menu: whether it was the menu's.
pub fn screen_press(host: &dyn ClientHost, rect: [f32; 4], mouse: [f32; 2]) -> bool {
    let size = gui(None, rect);
    with_state(host, |s| {
        let m = model(host, s);
        let inside = s.menu.contains(&m, size, mouse);
        if let Some(action) = s.menu.press(&m, size, mouse)
            && apply(s, action)
        {
            save(host, s);
        }
        inside
    })
}

/// A drag in the menu: sliders follow it. Whether a slider took it.
pub fn screen_drag(host: &dyn ClientHost, rect: [f32; 4], mouse: [f32; 2]) -> bool {
    let size = gui(None, rect);
    with_state(host, |s| {
        let m = model(host, s);
        match s.menu.drag(&m, size, mouse) {
            Some(action) => {
                apply(s, action);
                true
            }
            None => false,
        }
    })
}

/// The button came up: a slider was let go (whether one was).
pub fn screen_release(host: &dyn ClientHost) -> bool {
    with_existing(|s| {
        let dragging = s.menu.release();
        if dragging {
            save(host, s);
        }
        dragging
    })
    .unwrap_or(false)
}

/// A key on the menu: whether it is the menu's. While a text box has focus the keys are the
/// box's: Backspace, Enter and Tab work it, every other key is held back from the game (and
/// from Earshot's own keys), so typing a group name doesn't act on anything. Escape still
/// closes the menu, and the menu key closes it when nothing is being typed.
pub fn screen_key(host: &dyn ClientHost, key: &str) -> bool {
    let typing = with_state(host, |s| s.menu.typing());
    if !typing {
        let menu_key = key_binding(host, KEY_MENU);
        if key == menu_key {
            with_state(host, |s| s.close_menu = true);
            return true;
        }
        return false;
    }
    with_state(host, |s| {
        let menu_key = match key {
            "key.keyboard.backspace" => Some(MenuKey::Backspace),
            "key.keyboard.enter" | "key.keyboard.keypad.enter" => Some(MenuKey::Enter),
            "key.keyboard.tab" => Some(MenuKey::Tab),
            "key.keyboard.escape" => {
                let _ = s.menu.key(MenuKey::Escape);
                return false;
            }
            _ => None,
        };
        if let Some(k) = menu_key
            && let Some(action) = s.menu.key(k)
            && apply(s, action)
        {
            save(host, s);
        }
        true
    })
}

/// A character typed on the menu: into the focused text box (whether there was one).
pub fn screen_char(c: char) -> bool {
    with_existing(|s| {
        if s.menu.typing() {
            s.menu.type_char(c);
            true
        } else {
            false
        }
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_keys_and_payloads() {
        assert_eq!(host_of("play.example.org:25565"), "play.example.org");
        assert_eq!(host_of("[::1]:25565"), "::1");
        assert_eq!(host_of("example.org"), "example.org");
        assert_eq!(key_label("key.keyboard.v"), "V");
        assert_eq!(key_label("key.mouse.4"), "Mouse 4");
        assert_eq!(key_label("key.keyboard.left.shift"), "Left Shift");
        let body = payload_body("voicechat:ping", &[1, 2]);
        assert_eq!(body[0] as usize, "voicechat:ping".len());
        assert_eq!(&body[1..15], b"voicechat:ping");
        assert_eq!(&body[15..], &[1, 2]);
        // A channel past 127 bytes takes two VarInt bytes.
        let long = "a".repeat(200);
        assert_eq!(&payload_body(&long, &[])[..2], &[0xC8, 0x01]);
    }

    #[test]
    fn the_settings_file_is_key_value_lines() {
        let f = parse("mode=voice\nthreshold = -40\nbroken\nvolume.0=0.5\n");
        assert_eq!(f.get("mode").map(String::as_str), Some("voice"));
        assert_eq!(f.get("threshold").map(String::as_str), Some("-40"));
        assert_eq!(f.len(), 3);
    }

    #[test]
    fn devices_cycle_through_the_default() {
        let devices = || vec!["A".to_owned(), "B".to_owned()];
        assert_eq!(next_device(devices(), &None), Some("A".to_owned()));
        assert_eq!(next_device(devices(), &Some("B".to_owned())), None);
        assert_eq!(next_device(devices(), &Some("gone".to_owned())), None);
    }

    #[test]
    fn the_keys_are_the_originals_with_their_defaults() {
        let default = |id: &str| KEYS.iter().find(|k| k.0 == id).map(|k| k.2);
        assert_eq!(default("menu"), Some("key.keyboard.v"));
        assert_eq!(default("group"), Some("key.keyboard.g"));
        assert_eq!(default("hide_icons"), Some("key.keyboard.h"));
        assert_eq!(default("mute"), Some("key.keyboard.m"));
        assert_eq!(default("disable"), Some("key.keyboard.n"));
        assert_eq!(default("settings"), Some(UNBOUND));
        assert_eq!(default("adjust_volumes"), Some(UNBOUND));
        // Earshot's own deafen key mustn't take the original's N.
        assert_eq!(default("deafen"), Some(UNBOUND));
        let mut ids: Vec<&str> = KEYS.iter().map(|k| k.0).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), KEYS.len());
    }
}
