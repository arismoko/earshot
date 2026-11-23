//! Earshot on a dedicated server (only: not single player's server thread, nor a
//! world opened to LAN): an SVC-compatible voice server ([`earshot_core::server`]), so
//! Earshot players and Java players with Simple Voice Chat can talk there. Its settings are
//! `key=value` lines in `server.txt` in the server's mod data folder
//! (the mod data folder for `earshot`): `server_port` (24454), `voice_host` (what clients connect
//! to; empty = the address they joined), `max_distance` (48), `whisper_distance` (half of
//! `max_distance`) and `server_voice` (`false` turns it off).
//!
//! The players' packets reach it through the game patch in `mixins/server_packet.rs`
//! (the mod API has no hook for a player's custom payloads on the server); where the
//! players are comes from the `server_tick` hook.
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use earshot_core::server::{ServerConfig, VoiceServer};
use earshot_core::wire::Uuid;
use mod_api::{ConfigPath, Host, ServerHost};

const SETTINGS_FILE: &str = "server.txt";

#[derive(Default)]
struct ServerSide {
    /// The settings file, read by the first server tick.
    settings: Option<HashMap<String, String>>,
    voice: Option<VoiceServer>,
    /// Connections already told this server has voice.
    announced: HashSet<u32>,
    /// Starting failed (port taken…) or voice is off: don't retry every packet.
    failed: bool,
}

static SERVER: Mutex<Option<ServerSide>> = Mutex::new(None);

fn with_server<R>(f: impl FnOnce(&mut ServerSide) -> R) -> R {
    let mut guard = SERVER.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(ServerSide::default))
}

/// `key=value` lines.
fn parse(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

/// The voice server's settings from `lines`; None when voice is turned off.
pub fn config(lines: &HashMap<String, String>) -> Option<ServerConfig> {
    let get = |k: &str| {
        lines
            .get(k)
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    };
    if get("server_voice").as_deref() == Some("false") {
        return None;
    }
    let port: u16 = get("server_port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(24454);
    let max_distance: f64 = get("max_distance")
        .and_then(|v| v.parse().ok())
        .unwrap_or(48.0);
    Some(ServerConfig {
        bind: std::net::SocketAddr::from(([0, 0, 0, 0], port)),
        max_distance,
        // Half the voice distance unless set (capture 07: 24 of 48 on SVC's defaults).
        whisper_distance: get("whisper_distance")
            .and_then(|v| v.parse().ok())
            .unwrap_or(max_distance / 2.0),
        voice_host: get("voice_host").unwrap_or_default(),
        ..Default::default()
    })
}

fn start(side: &mut ServerSide) {
    if side.voice.is_some() || side.failed {
        return;
    }
    // Before the first tick read the settings, nothing starts.
    let Some(settings) = side.settings.as_ref() else {
        return;
    };
    let Some(config) = config(settings) else {
        side.failed = true;
        return;
    };
    let port = config.bind.port();
    match VoiceServer::start(config) {
        Ok(v) => {
            eprintln!("[earshot] voice server on UDP port {}", v.port());
            side.voice = Some(v);
        }
        Err(e) => {
            eprintln!("[earshot] couldn't start the voice server on UDP port {port}: {e}");
            side.failed = true;
        }
    }
}

/// The server's tick: the settings (once), and where everyone is, for proximity.
pub fn tick(host: &mut dyn ServerHost) {
    let online = host.players().server_players();
    let reads: &dyn Host = &*host;
    with_server(|side| {
        if side.settings.is_none() {
            let lines = ConfigPath::new(SETTINGS_FILE)
                .ok()
                .and_then(|p| reads.data_files().read(&p).ok())
                .and_then(|b| String::from_utf8(b).ok())
                .map(|t| parse(&t))
                .unwrap_or_default();
            side.settings = Some(lines);
        }
    });
    let players: Vec<(Uuid, String, [f64; 3])> = online
        .into_iter()
        .filter_map(|p| {
            let entity = p.entity();
            let pos = entity.position(reads)?;
            let dimension = entity
                .level(reads)
                .and_then(|l| l.dimension().id(reads))
                .unwrap_or_default();
            Some((Uuid::from_u128(p.uuid()), dimension.to_owned(), pos))
        })
        .collect();
    with_server(|side| {
        let Some(voice) = side.voice.as_ref() else {
            return;
        };
        let empty = players.is_empty();
        voice.set_players(players);
        if empty {
            side.announced.clear();
        }
    });
}

/// What the game patch sends for a player's packet: to that connection first (a
/// `minecraft:register` once, so the client asks for a key), then to players by UUID.
pub struct Replies {
    pub announce: Option<Vec<u8>>,
    pub outgoing: Vec<(u128, &'static str, Vec<u8>)>,
}

/// A play packet from player `uuid` (`name`) on connection `conn`, with its custom
/// payload's channel and bytes if it is one, on a dedicated server.
pub fn on_packet(
    conn: u32,
    uuid: u128,
    name: &str,
    payload: Option<(&str, &[u8])>,
) -> Option<Replies> {
    with_server(|side| {
        start(side);
        let voice = side.voice.as_ref()?;
        let announce = side
            .announced
            .insert(conn)
            .then(VoiceServer::register_payload);
        if let Some((channel, data)) = payload
            && channel.starts_with("voicechat:")
        {
            voice.on_plugin(Uuid::from_u128(uuid), name, channel, data);
        }
        let outgoing = voice
            .take_outgoing()
            .into_iter()
            .map(|(to, channel, data)| (u128::from_be_bytes(to.0), channel, data))
            .collect();
        Some(Replies { announce, outgoing })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_settings_file_sets_the_voice_server() {
        let c = config(&parse("")).unwrap();
        assert_eq!(
            (c.bind.port(), c.max_distance, c.whisper_distance),
            (24454, 48.0, 24.0)
        );
        let c = config(&parse(
            "server_port=25570\nmax_distance = 30\nvoice_host= voice.example \n",
        ))
        .unwrap();
        assert_eq!(
            (c.bind.port(), c.max_distance, c.whisper_distance),
            (25570, 30.0, 15.0)
        );
        assert_eq!(c.voice_host, "voice.example");
        assert!(config(&parse("server_voice=false")).is_none());
    }

    #[test]
    fn nothing_starts_before_the_settings_are_read() {
        assert!(on_packet(1, 7, "a", None).is_none());
    }
}
