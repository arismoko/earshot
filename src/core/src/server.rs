//! A voice server speaking SVC's protocol, for a Horizon dedicated server (or a LAN host), so
//! Earshot players, and Java players running Simple Voice Chat, can talk on a Horizon server.
//!
//! It is the server side of what PROTOCOL.md describes, built from the same observations:
//! - over the game connection, a player's `request_secret` gets a fresh random `secret` (and
//!   the list of everyone's voice state); `update_state` switches a player's voice on or off;
//! - over UDP: `Authenticate` is acknowledged when its secret matches, `ConnectionCheck` makes
//!   the player connected, keep-alives go out every `keep_alive_ms`, and each microphone frame
//!   is forwarded, re-sealed with each listener's own key, to the connected players in the
//!   same dimension within the voice distance (as the SVC server was seen to do, capture 05);
//!   a whispered frame only within the whisper distance (capture 07);
//! - voice groups (captures 06 and 07): `create_group`, `set_group` (with its password) and
//!   `leave_group`; every player hears of groups made and gone (`add_group`, `remove_group`) and
//!   of who is in which (their state); members hear each other at any distance as group voice,
//!   and the group's type decides who else hears whom ([`route`]).
//!
//! The game feeds it player positions each tick and carries its plugin messages; this type
//! owns the UDP socket and its thread.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::crypto::VoiceKey;
use crate::plugin::{
    self, Codec, CreateGroup, Group, GroupType, JoinedGroup, PlayerState, RequestSecret, Secret,
    SetGroup, UpdateState,
};
use crate::udp::{self, GroupSound, Message, PlayerSound};
use crate::wire::Uuid;

/// A player who hasn't been heard from for this long is no longer connected.
const SILENCE_TIMEOUT_MS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq)]
pub struct ServerConfig {
    /// UDP address to bind (`0.0.0.0:24454` by default; port 0 picks a free one).
    pub bind: SocketAddr,
    pub max_distance: f64,
    /// How far a whisper carries (capture 07: 24, half SVC's default voice distance).
    pub whisper_distance: f64,
    pub keep_alive_ms: u32,
    /// What clients are told to connect to (`voice_host`); empty = the game server's host.
    pub voice_host: String,
    pub mtu: i32,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:24454".parse().unwrap(),
            max_distance: 48.0,
            whisper_distance: 24.0,
            keep_alive_ms: 1000,
            voice_host: String::new(),
            mtu: 1275,
        }
    }
}

struct Peer {
    name: String,
    secret: [u8; 16],
    key: VoiceKey,
    addr: Option<SocketAddr>,
    connected: bool,
    disabled: bool,
    last_heard: u64,
    last_keep_alive: u64,
    dimension: String,
    position: Option<[f64; 3]>,
    /// The voice group they're in.
    group: Option<Uuid>,
}

impl Peer {
    fn state(&self, id: Uuid) -> PlayerState {
        PlayerState {
            disabled: self.disabled,
            disconnected: !self.connected,
            player: id,
            name: self.name.clone(),
            group: self.group,
        }
    }
}

/// A voice group on this server.
struct ServerGroup {
    name: String,
    password: Option<String>,
    kind: GroupType,
}

impl ServerGroup {
    /// How `add_group` announces it.
    fn announce(&self, id: Uuid) -> Vec<u8> {
        Group {
            id,
            name: self.name.clone(),
            has_password: self.password.is_some(),
            flags: [0, 0],
            kind: self.kind,
        }
        .encode()
    }
}

/// How a listener hears a speaker, from their groups and those groups' types (captures 06 and 07).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Same group: group voice, at any distance.
    Group,
    /// Proximity voice, if they're near enough.
    Nearby,
    /// Not at all.
    Never,
}

fn route(speaker: Option<(Uuid, GroupType)>, listener: Option<(Uuid, GroupType)>) -> Route {
    match (speaker, listener) {
        (Some((a, _)), Some((b, _))) if a == b => Route::Group,
        // Only an open group's members are heard outside it (capture 07 at 892 s; a normal
        // group's weren't in capture 06 at 193 s, nor an isolated one's in 07 at 958 s).
        (Some((_, kind)), _) if kind != GroupType::Open => Route::Never,
        // An isolated group's members hear no one outside it (capture 07 at 966 s).
        (_, Some((_, GroupType::Isolated))) => Route::Never,
        _ => Route::Nearby,
    }
}

struct Inner {
    peers: HashMap<Uuid, Peer>,
    groups: HashMap<Uuid, ServerGroup>,
    /// Plugin messages for the game to send: (player, channel, data).
    outbox: Vec<(Uuid, &'static str, Vec<u8>)>,
    config: ServerConfig,
    port: u16,
}

impl Inner {
    /// Sends one message to every player with a secret.
    fn broadcast(&mut self, channel: &'static str, data: Vec<u8>) {
        let to: Vec<Uuid> = self.peers.keys().copied().collect();
        for t in to {
            self.outbox.push((t, channel, data.clone()));
        }
    }

    /// Tells every player with a secret about `id`'s voice state.
    fn broadcast_state(&mut self, id: Uuid) {
        let Some(p) = self.peers.get(&id) else { return };
        let data = p.state(id).encode();
        self.broadcast(plugin::STATE, data);
    }

    /// A speaker's group and its type, for [`route`].
    fn group_of(&self, id: &Uuid) -> Option<(Uuid, GroupType)> {
        let g = self.peers.get(id)?.group?;
        Some((g, self.groups.get(&g)?.kind))
    }

    /// Takes `id` out of their group, if any; a group left empty is gone (`remove_group`, as the
    /// SVC server did when its last member left, capture 06). Doesn't announce `id`'s new state.
    fn leave_group(&mut self, id: Uuid) {
        let Some(g) = self.peers.get_mut(&id).and_then(|p| p.group.take()) else {
            return;
        };
        if !self.peers.values().any(|p| p.group == Some(g)) {
            self.groups.remove(&g);
            self.broadcast(plugin::REMOVE_GROUP, g.0.to_vec());
        }
    }

    /// Puts `id` in group `g`: out of the old one, then `joined_group` for them and their state for
    /// everyone (the SVC server's order in capture 07).
    fn join_group(&mut self, id: Uuid, g: Uuid) {
        self.leave_group(id);
        let Some(p) = self.peers.get_mut(&id) else {
            return;
        };
        p.group = Some(g);
        self.broadcast_state(id);
        self.outbox.push((
            id,
            plugin::JOINED_GROUP,
            JoinedGroup {
                group: Some(g),
                wrong_password: false,
            }
            .encode(),
        ));
    }
}

struct Shared {
    inner: Mutex<Inner>,
    stop: AtomicBool,
    start: Instant,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

pub struct VoiceServer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl VoiceServer {
    /// Binds the UDP socket and starts serving.
    pub fn start(config: ServerConfig) -> io::Result<VoiceServer> {
        let socket = UdpSocket::bind(config.bind)?;
        socket.set_read_timeout(Some(Duration::from_millis(20)))?;
        let port = socket.local_addr()?.port();
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                peers: HashMap::new(),
                groups: HashMap::new(),
                outbox: Vec::new(),
                config,
                port,
            }),
            stop: AtomicBool::new(false),
            start: Instant::now(),
        });
        let s = shared.clone();
        let thread = std::thread::Builder::new()
            .name("earshot-voice-server".into())
            .spawn(move || serve(s, socket))?;
        Ok(VoiceServer {
            shared,
            thread: Some(thread),
        })
    }

    /// The UDP port actually in use.
    pub fn port(&self) -> u16 {
        self.shared.lock().port
    }

    /// The `minecraft:register` payload a server sends a joining player, naming the channels
    /// it takes (as SVC's server does, capture 02): it tells clients this server has voice.
    pub fn register_payload() -> Vec<u8> {
        plugin::encode_register(plugin::SERVER_RECEIVES)
    }

    /// A `voicechat:` plugin message from a player.
    pub fn on_plugin(&self, player: Uuid, name: &str, channel: &str, data: &[u8]) {
        let mut inner = self.shared.lock();
        match channel {
            plugin::REQUEST_SECRET => {
                match RequestSecret::decode(data) {
                    Ok(r) if r.compatibility_version == plugin::COMPATIBILITY_VERSION => {}
                    Ok(r) => {
                        eprintln!(
                            "[earshot] {name} has voice chat compatibility {} (this server speaks {}): not sending a key",
                            r.compatibility_version,
                            plugin::COMPATIBILITY_VERSION
                        );
                        return;
                    }
                    Err(_) => return,
                }
                let mut secret = [0u8; 16];
                if getrandom::fill(&mut secret).is_err() {
                    eprintln!("[earshot] no random bytes for {name}'s voice key");
                    return;
                }
                let disabled = inner.peers.get(&player).is_some_and(|p| p.disabled);
                let group = inner.peers.get(&player).and_then(|p| p.group);
                let (dimension, position) = inner
                    .peers
                    .get(&player)
                    .map(|p| (p.dimension.clone(), p.position))
                    .unwrap_or_default();
                inner.peers.insert(
                    player,
                    Peer {
                        name: name.to_string(),
                        secret,
                        key: VoiceKey::new(&secret),
                        addr: None,
                        connected: false,
                        disabled,
                        last_heard: 0,
                        last_keep_alive: 0,
                        dimension,
                        position,
                        group,
                    },
                );
                let cfg = &inner.config;
                let msg = Secret {
                    secret,
                    server_port: inner.port as i32,
                    player,
                    codec: Codec::Voip,
                    mtu: cfg.mtu,
                    voice_distance: cfg.max_distance,
                    keep_alive_ms: cfg.keep_alive_ms as i32,
                    groups_enabled: true,
                    voice_host: cfg.voice_host.clone(),
                    recording_allowed: false,
                };
                inner.outbox.push((player, plugin::SECRET, msg.encode()));
                // Everyone's state for the newcomer (the SVC server sends `states` with the secret,
                // capture 05), then the newcomer's for everyone.
                let states: Vec<PlayerState> =
                    inner.peers.iter().map(|(id, p)| p.state(*id)).collect();
                inner
                    .outbox
                    .push((player, plugin::STATES, PlayerState::encode_list(&states)));
                // The groups already there (the SVC server sent them with `states`, capture 07 at 825.65 s).
                let groups: Vec<Vec<u8>> =
                    inner.groups.iter().map(|(id, g)| g.announce(*id)).collect();
                for g in groups {
                    inner.outbox.push((player, plugin::ADD_GROUP, g));
                }
                inner.broadcast_state(player);
            }
            plugin::UPDATE_STATE => {
                let Ok(u) = UpdateState::decode(data) else {
                    return;
                };
                if let Some(p) = inner.peers.get_mut(&player) {
                    p.disabled = u.disabled;
                    inner.broadcast_state(player);
                }
            }
            plugin::CREATE_GROUP => {
                let Ok(c) = CreateGroup::decode(data) else {
                    return;
                };
                let group_name = c.name.trim().to_string();
                if !inner.peers.contains_key(&player) || group_name.is_empty() {
                    return;
                }
                let mut id = [0u8; 16];
                if getrandom::fill(&mut id).is_err() {
                    return;
                }
                let id = Uuid(id);
                let group = ServerGroup {
                    name: group_name,
                    password: c.password.filter(|p| !p.is_empty()),
                    kind: c.kind,
                };
                eprintln!(
                    "[earshot] {name} made the voice group {} ({})",
                    group.name,
                    group.kind.label().to_lowercase()
                );
                let announce = group.announce(id);
                inner.groups.insert(id, group);
                inner.broadcast(plugin::ADD_GROUP, announce);
                inner.join_group(player, id);
            }
            plugin::SET_GROUP => {
                let Ok(s) = SetGroup::decode(data) else {
                    return;
                };
                let Some(g) = inner.groups.get(&s.group) else {
                    return;
                };
                if !inner.peers.contains_key(&player) {
                    return;
                }
                let right = match &g.password {
                    None => true,
                    Some(p) => s.password.as_deref() == Some(p.as_str()),
                };
                if right {
                    inner.join_group(player, s.group);
                } else {
                    // Not in a group, wrong password (capture 07 at 1025.96 s).
                    inner.outbox.push((
                        player,
                        plugin::JOINED_GROUP,
                        JoinedGroup {
                            group: None,
                            wrong_password: true,
                        }
                        .encode(),
                    ));
                }
            }
            plugin::LEAVE_GROUP => {
                if inner.peers.get(&player).is_some_and(|p| p.group.is_some()) {
                    inner.leave_group(player);
                    inner.broadcast_state(player);
                    inner.outbox.push((
                        player,
                        plugin::JOINED_GROUP,
                        JoinedGroup {
                            group: None,
                            wrong_password: false,
                        }
                        .encode(),
                    ));
                }
            }
            _ => {}
        }
    }

    /// The players online, with where they are. Players no longer listed have left: out of their
    /// group, forgotten, and everyone told (`remove_state`, as the SVC server did, capture 06).
    pub fn set_players(&self, players: impl IntoIterator<Item = (Uuid, String, [f64; 3])>) {
        let mut inner = self.shared.lock();
        let mut seen = Vec::new();
        for (id, dimension, position) in players {
            seen.push(id);
            if let Some(p) = inner.peers.get_mut(&id) {
                p.dimension = dimension;
                p.position = Some(position);
            }
        }
        let gone: Vec<Uuid> = inner
            .peers
            .keys()
            .filter(|id| !seen.contains(id))
            .copied()
            .collect();
        for id in gone {
            inner.leave_group(id);
            inner.peers.remove(&id);
            inner.broadcast(plugin::REMOVE_STATE, id.0.to_vec());
        }
    }

    /// Plugin messages to send: (player, channel, data).
    pub fn take_outgoing(&self) -> Vec<(Uuid, &'static str, Vec<u8>)> {
        std::mem::take(&mut self.shared.lock().outbox)
    }

    /// Who's connected to voice (for logs and tests).
    pub fn connected(&self) -> Vec<Uuid> {
        self.shared
            .lock()
            .peers
            .iter()
            .filter(|(_, p)| p.connected)
            .map(|(id, _)| *id)
            .collect()
    }
}

impl Drop for VoiceServer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn send(socket: &UdpSocket, key: &VoiceKey, addr: SocketAddr, msg: &Message) {
    if let Ok(packet) = udp::seal_to_client(key, msg) {
        let _ = socket.send_to(&packet, addr);
    }
}

fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn serve(shared: Arc<Shared>, socket: UdpSocket) {
    let mut buf = vec![0u8; 4096];
    while !shared.stop.load(Ordering::Relaxed) {
        if let Ok((n, from)) = socket.recv_from(&mut buf) {
            handle(&shared, &socket, &buf[..n], from);
        }

        // Keep-alives, and players who went quiet.
        let now = shared.now();
        let mut inner = shared.lock();
        let interval = inner.config.keep_alive_ms as u64;
        let mut lost = Vec::new();
        for (id, p) in inner.peers.iter_mut() {
            if !p.connected {
                continue;
            }
            if now.saturating_sub(p.last_heard) >= SILENCE_TIMEOUT_MS {
                p.connected = false;
                lost.push(*id);
                continue;
            }
            if now.saturating_sub(p.last_keep_alive) >= interval {
                p.last_keep_alive = now;
                if let Some(addr) = p.addr {
                    send(&socket, &p.key, addr, &Message::KeepAlive);
                }
            }
        }
        for id in lost {
            inner.broadcast_state(id);
        }
    }
}

fn handle(shared: &Shared, socket: &UdpSocket, packet: &[u8], from: SocketAddr) {
    let Ok((sender, sealed)) = udp::split_from_client(packet) else {
        return;
    };
    let now = shared.now();
    let mut inner = shared.lock();
    let Some(peer) = inner.peers.get(&sender) else {
        return;
    };
    let Ok(plain) = peer.key.open(sealed) else {
        return;
    };
    let Ok(msg) = Message::decode(&plain) else {
        return;
    };

    match msg {
        Message::Authenticate { player, secret } => {
            let p = inner.peers.get_mut(&sender).unwrap();
            if player == sender && secret == p.secret {
                p.addr = Some(from);
                p.last_heard = now;
                send(socket, &p.key, from, &Message::AuthenticateAck);
            }
        }
        Message::ConnectionCheck => {
            let p = inner.peers.get_mut(&sender).unwrap();
            if p.addr == Some(from) {
                let newly = !p.connected;
                p.connected = true;
                p.last_heard = now;
                p.last_keep_alive = now;
                send(socket, &p.key, from, &Message::ConnectionCheckAck);
                if newly {
                    eprintln!("[earshot] {} connected to voice chat", p.name);
                    inner.broadcast_state(sender);
                }
            }
        }
        Message::KeepAlive => {
            let p = inner.peers.get_mut(&sender).unwrap();
            if p.addr == Some(from) {
                p.last_heard = now;
            }
        }
        Message::Mic(frame) => {
            let p = inner.peers.get_mut(&sender).unwrap();
            if p.addr != Some(from) || !p.connected || p.disabled {
                return;
            }
            p.last_heard = now;
            let (pos, dim) = (p.position, p.dimension.clone());
            // A whisper (last byte 1, capture 07) carries only the whisper distance, and says so.
            let whisper = frame.flag == 1;
            let range = if whisper {
                inner.config.whisper_distance
            } else {
                inner.config.max_distance
            };
            let nearby = Message::PlayerSound(PlayerSound {
                channel: sender,
                sender,
                opus: frame.opus.clone(),
                sequence: frame.sequence,
                distance: range as f32,
                flag: frame.flag,
            });
            // Group voice: the channel was the sender in every captured packet (capture 06).
            let group = Message::GroupSound(GroupSound {
                channel: sender,
                sender,
                opus: frame.opus,
                sequence: frame.sequence,
                flag: frame.flag,
            });
            let from_group = inner.group_of(&sender);
            for (id, l) in inner.peers.iter() {
                if *id == sender || !l.connected || l.disabled {
                    continue;
                }
                let Some(addr) = l.addr else { continue };
                match route(from_group, inner.group_of(id)) {
                    Route::Group => send(socket, &l.key, addr, &group),
                    Route::Nearby => {
                        let near = matches!((pos, l.position), (Some(a), Some(b)) if l.dimension == dim && distance(a, b) <= range);
                        if near {
                            send(socket, &l.key, addr, &nearby);
                        }
                    }
                    Route::Never => {}
                }
            }
        }
        // Server-to-client messages coming the wrong way: ignore.
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{Settings, VoiceClient};
    use crate::udp::MicFrame;

    fn server() -> VoiceServer {
        VoiceServer::start(ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..ServerConfig::default()
        })
        .unwrap()
    }

    /// A hand-driven client: its socket and key.
    struct Raw {
        id: Uuid,
        key: VoiceKey,
        socket: UdpSocket,
    }

    impl Raw {
        fn send(&self, msg: &Message) {
            self.socket
                .send(&udp::seal_to_server(&self.key, &self.id, msg).unwrap())
                .unwrap();
        }

        fn recv(&self) -> Option<Message> {
            let mut buf = [0u8; 4096];
            let n = self.socket.recv(&mut buf).ok()?;
            udp::open_from_server(&self.key, &buf[..n]).ok()
        }

        /// The next message that isn't a keep-alive.
        fn recv_not_keep_alive(&self) -> Option<Message> {
            for _ in 0..50 {
                match self.recv()? {
                    Message::KeepAlive => continue,
                    m => return Some(m),
                }
            }
            None
        }
    }

    /// Joins `name` at `pos` and runs the whole handshake by hand.
    fn join(
        srv: &VoiceServer,
        n: u8,
        dim: &str,
        pos: [f64; 3],
        everyone: &mut Vec<(Uuid, String, [f64; 3])>,
    ) -> Raw {
        let id = Uuid([n; 16]);
        everyone.push((id, dim.to_string(), pos));
        srv.on_plugin(
            id,
            &format!("P{n}"),
            plugin::REQUEST_SECRET,
            &RequestSecret {
                compatibility_version: 20,
            }
            .encode(),
        );
        srv.set_players(everyone.clone());
        let out = srv.take_outgoing();
        let secret = out
            .iter()
            .find(|(to, ch, _)| *to == id && *ch == plugin::SECRET)
            .expect("a secret");
        let secret = Secret::decode(&secret.2).unwrap();
        assert_eq!(secret.player, id);
        assert_eq!(secret.server_port, srv.port() as i32);

        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.connect(("127.0.0.1", srv.port())).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let raw = Raw {
            id,
            key: VoiceKey::new(&secret.secret),
            socket,
        };
        raw.send(&Message::Authenticate {
            player: id,
            secret: secret.secret,
        });
        assert_eq!(raw.recv_not_keep_alive(), Some(Message::AuthenticateAck));
        raw.send(&Message::ConnectionCheck);
        assert_eq!(raw.recv_not_keep_alive(), Some(Message::ConnectionCheckAck));
        raw
    }

    fn mic(seq: u64) -> Message {
        Message::Mic(MicFrame {
            opus: vec![0x78, seq as u8, 1, 2],
            sequence: seq,
            flag: 0,
        })
    }

    #[test]
    fn hands_out_a_secret_and_states_and_announces_itself() {
        let srv = server();
        let id = Uuid([1; 16]);
        srv.set_players([(id, "overworld".into(), [0.0; 3])]);
        srv.on_plugin(
            id,
            "Alice",
            plugin::REQUEST_SECRET,
            &RequestSecret {
                compatibility_version: 20,
            }
            .encode(),
        );
        let out = srv.take_outgoing();
        let channels: Vec<&str> = out.iter().map(|(_, c, _)| *c).collect();
        assert_eq!(channels, [plugin::SECRET, plugin::STATES, plugin::STATE]);
        let states = PlayerState::decode_list(&out[1].2).unwrap();
        assert_eq!(states[0].name, "Alice");
        assert!(states[0].disconnected);
        assert!(plugin::server_has_voice(&plugin::decode_register(
            &VoiceServer::register_payload()
        )));
    }

    #[test]
    fn refuses_another_compatibility_version() {
        let srv = server();
        srv.on_plugin(
            Uuid([1; 16]),
            "Old",
            plugin::REQUEST_SECRET,
            &RequestSecret {
                compatibility_version: 19,
            }
            .encode(),
        );
        assert!(srv.take_outgoing().is_empty());
    }

    #[test]
    fn a_wrong_secret_gets_no_ack() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0; 3], &mut all);
        let impostor = Raw {
            id: a.id,
            key: VoiceKey::new(&[0; 16]),
            socket: UdpSocket::bind("127.0.0.1:0").unwrap(),
        };
        impostor.socket.connect(("127.0.0.1", srv.port())).unwrap();
        impostor
            .socket
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        impostor.send(&Message::Authenticate {
            player: a.id,
            secret: [0; 16],
        });
        assert_eq!(impostor.recv(), None);
    }

    #[test]
    fn forwards_voice_to_players_in_range_only() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0, 64.0, 0.0], &mut all);
        let near = join(&srv, 2, "overworld", [10.0, 64.0, 0.0], &mut all);
        let far = join(&srv, 3, "overworld", [100.0, 64.0, 0.0], &mut all);
        let nether = join(&srv, 4, "the_nether", [1.0, 64.0, 0.0], &mut all);
        srv.set_players(all.clone());
        assert_eq!(srv.connected().len(), 4);

        a.send(&mic(7));
        let Some(Message::PlayerSound(s)) = near.recv_not_keep_alive() else {
            panic!("near player heard nothing")
        };
        assert_eq!(
            (s.sender, s.channel, s.sequence, s.distance),
            (a.id, a.id, 7, 48.0)
        );
        assert_eq!(s.opus, [0x78, 7, 1, 2], "forwarded as sent");
        assert_eq!(far.recv_not_keep_alive(), None, "beyond 48 blocks");
        assert_eq!(nether.recv_not_keep_alive(), None, "another dimension");
        assert_eq!(a.recv_not_keep_alive(), None, "not echoed back");
    }

    #[test]
    fn a_disabled_player_is_neither_heard_nor_sent_to() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0; 3], &mut all);
        let b = join(&srv, 2, "overworld", [1.0, 0.0, 0.0], &mut all);
        srv.on_plugin(
            a.id,
            "P1",
            plugin::UPDATE_STATE,
            &UpdateState { disabled: true }.encode(),
        );
        a.send(&mic(1));
        assert_eq!(b.recv_not_keep_alive(), None);
        b.send(&mic(2));
        assert_eq!(a.recv_not_keep_alive(), None);
        // And everyone heard that A switched off.
        let states: Vec<PlayerState> = srv
            .take_outgoing()
            .into_iter()
            .filter(|(_, c, _)| *c == plugin::STATE)
            .map(|(_, _, d)| PlayerState::decode(&d).unwrap())
            .collect();
        assert!(states.iter().any(|s| s.player == a.id && s.disabled));
    }

    #[test]
    fn players_who_leave_are_forgotten() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0; 3], &mut all);
        let b = join(&srv, 2, "overworld", [1.0, 0.0, 0.0], &mut all);
        srv.set_players([all[1].clone()]); // A left
        assert_eq!(srv.connected(), [b.id]);
        a.send(&mic(1));
        assert_eq!(b.recv_not_keep_alive(), None);
    }

    fn whisper(seq: u64) -> Message {
        Message::Mic(MicFrame {
            opus: vec![0x78, seq as u8, 1, 2],
            sequence: seq,
            flag: 1,
        })
    }

    /// Makes `kind` group "G" with `creator` in it; returns its id.
    fn make_group(
        srv: &VoiceServer,
        creator: &Raw,
        kind: GroupType,
        password: Option<&str>,
    ) -> Uuid {
        srv.on_plugin(
            creator.id,
            "P",
            plugin::CREATE_GROUP,
            &CreateGroup {
                name: "G".into(),
                password: password.map(Into::into),
                kind,
            }
            .encode(),
        );
        let out = srv.take_outgoing();
        let add = out
            .iter()
            .find(|(_, c, _)| *c == plugin::ADD_GROUP)
            .expect("add_group");
        let g = Group::decode(&add.2).unwrap();
        assert_eq!((g.kind, g.has_password), (kind, password.is_some()));
        g.id
    }

    fn join_group(srv: &VoiceServer, who: &Raw, g: Uuid, password: Option<&str>) -> JoinedGroup {
        srv.on_plugin(
            who.id,
            "P",
            plugin::SET_GROUP,
            &SetGroup {
                group: g,
                password: password.map(Into::into),
            }
            .encode(),
        );
        let out = srv.take_outgoing();
        let j = out
            .iter()
            .find(|(to, c, _)| *to == who.id && *c == plugin::JOINED_GROUP)
            .expect("joined_group");
        JoinedGroup::decode(&j.2).unwrap()
    }

    /// What `listener` gets after `speaker` talks: group voice, nearby voice or nothing.
    fn heard(speaker: &Raw, listener: &Raw, seq: u64) -> &'static str {
        speaker.send(&mic(seq));
        match listener.recv_not_keep_alive() {
            Some(Message::GroupSound(_)) => "group",
            Some(Message::PlayerSound(_)) => "nearby",
            None => "nothing",
            Some(other) => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn routes_follow_the_captured_group_types() {
        let (g, h) = (Uuid([1; 16]), Uuid([2; 16]));
        let (normal, open, isolated) = (GroupType::Normal, GroupType::Open, GroupType::Isolated);
        assert_eq!(route(None, None), Route::Nearby);
        for kind in [normal, open, isolated] {
            assert_eq!(
                route(Some((g, kind)), Some((g, kind))),
                Route::Group,
                "{kind:?}: same group"
            );
        }
        assert_eq!(
            route(Some((g, normal)), None),
            Route::Never,
            "a normal group isn't heard outside"
        );
        assert_eq!(
            route(Some((g, open)), None),
            Route::Nearby,
            "an open group is"
        );
        assert_eq!(route(Some((g, isolated)), None), Route::Never);
        assert_eq!(
            route(None, Some((g, normal))),
            Route::Nearby,
            "a normal group hears outsiders"
        );
        assert_eq!(route(None, Some((g, open))), Route::Nearby);
        assert_eq!(
            route(None, Some((g, isolated))),
            Route::Never,
            "an isolated group doesn't"
        );
        assert_eq!(
            route(Some((g, open)), Some((h, normal))),
            Route::Nearby,
            "open to another group's member"
        );
        assert_eq!(route(Some((g, open)), Some((h, isolated))), Route::Never);
    }

    #[test]
    fn groups_end_to_end() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0, 64.0, 0.0], &mut all);
        let b = join(&srv, 2, "overworld", [5.0, 64.0, 0.0], &mut all);
        srv.take_outgoing();

        // Normal: B outside hears nothing of A; A hears B nearby.
        let g = make_group(&srv, &a, GroupType::Normal, None);
        assert_eq!(heard(&a, &b, 1), "nothing");
        assert_eq!(heard(&b, &a, 2), "nearby");
        // Once B joins, group voice both ways.
        assert_eq!(
            join_group(&srv, &b, g, None),
            JoinedGroup {
                group: Some(g),
                wrong_password: false
            }
        );
        assert_eq!(heard(&a, &b, 3), "group");
        assert_eq!(heard(&b, &a, 4), "group");
        // Both leave: the group is gone.
        srv.on_plugin(a.id, "P1", plugin::LEAVE_GROUP, &[]);
        srv.on_plugin(b.id, "P2", plugin::LEAVE_GROUP, &[]);
        let out = srv.take_outgoing();
        assert!(
            out.iter()
                .any(|(_, c, d)| *c == plugin::REMOVE_GROUP && d == &g.0.to_vec()),
            "removed when empty"
        );
        assert_eq!(heard(&a, &b, 5), "nearby", "no groups: proximity again");

        // Open: outsiders nearby hear the member.
        make_group(&srv, &a, GroupType::Open, None);
        assert_eq!(heard(&a, &b, 6), "nearby");
        assert_eq!(heard(&b, &a, 7), "nearby");
        srv.on_plugin(a.id, "P1", plugin::LEAVE_GROUP, &[]);

        // Isolated: nobody across the line.
        make_group(&srv, &a, GroupType::Isolated, None);
        assert_eq!(heard(&a, &b, 8), "nothing");
        assert_eq!(heard(&b, &a, 9), "nothing");
    }

    #[test]
    fn a_group_password_is_checked() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0; 3], &mut all);
        let b = join(&srv, 2, "overworld", [1.0, 0.0, 0.0], &mut all);
        let g = make_group(&srv, &a, GroupType::Normal, Some("1234"));
        assert_eq!(
            join_group(&srv, &b, g, Some("9999")),
            JoinedGroup {
                group: None,
                wrong_password: true
            }
        );
        assert_eq!(
            join_group(&srv, &b, g, None),
            JoinedGroup {
                group: None,
                wrong_password: true
            }
        );
        assert_eq!(
            join_group(&srv, &b, g, Some("1234")),
            JoinedGroup {
                group: Some(g),
                wrong_password: false
            }
        );
    }

    #[test]
    fn a_newcomer_hears_of_the_groups_and_leavers_leave_them() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0; 3], &mut all);
        let g = make_group(&srv, &a, GroupType::Open, None);
        let b = join(&srv, 2, "overworld", [1.0, 0.0, 0.0], &mut all);
        // join() already took B's messages; ask again to see what a newcomer is sent.
        srv.on_plugin(
            b.id,
            "P2",
            plugin::REQUEST_SECRET,
            &RequestSecret {
                compatibility_version: 20,
            }
            .encode(),
        );
        let out = srv.take_outgoing();
        let add = out
            .iter()
            .find(|(to, c, _)| *to == b.id && *c == plugin::ADD_GROUP)
            .expect("the existing group");
        assert_eq!(Group::decode(&add.2).unwrap().id, g);
        let states = out
            .iter()
            .find(|(to, c, _)| *to == b.id && *c == plugin::STATES)
            .unwrap();
        let states = PlayerState::decode_list(&states.2).unwrap();
        assert_eq!(
            states.iter().find(|s| s.player == a.id).unwrap().group,
            Some(g),
            "A's state names the group"
        );
        // A leaves the server: out of the group, which is now empty, and everyone told.
        srv.set_players([all[1].clone()]);
        let out = srv.take_outgoing();
        assert!(
            out.iter()
                .any(|(_, c, d)| *c == plugin::REMOVE_GROUP && d == &g.0.to_vec())
        );
        assert!(
            out.iter()
                .any(|(_, c, d)| *c == plugin::REMOVE_STATE && d == &a.id.0.to_vec())
        );
    }

    #[test]
    fn a_whisper_carries_half_as_far_and_says_so() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0, 64.0, 0.0], &mut all);
        let close = join(&srv, 2, "overworld", [10.0, 64.0, 0.0], &mut all);
        let mid = join(&srv, 3, "overworld", [30.0, 64.0, 0.0], &mut all);
        a.send(&whisper(1));
        let Some(Message::PlayerSound(s)) = close.recv_not_keep_alive() else {
            panic!("10 blocks: heard")
        };
        assert_eq!(
            (s.distance, s.flag),
            (24.0, 1),
            "the whisper's range and flag, as capture 07"
        );
        assert_eq!(mid.recv_not_keep_alive(), None, "30 blocks: past a whisper");
        a.send(&mic(2));
        let Some(Message::PlayerSound(s)) = mid.recv_not_keep_alive() else {
            panic!("30 blocks: normal talk heard")
        };
        assert_eq!((s.distance, s.flag), (48.0, 0));
        close.recv_not_keep_alive();
    }

    #[test]
    fn sends_keep_alives() {
        let srv = server();
        let mut all = vec![];
        let a = join(&srv, 1, "overworld", [0.0; 3], &mut all);
        let mut got = false;
        for _ in 0..8 {
            if a.recv() == Some(Message::KeepAlive) {
                got = true;
                break;
            }
        }
        assert!(got, "a keep-alive within about 2 s");
    }

    #[test]
    fn earshots_own_client_connects_to_it() {
        // The real client engine against this server over localhost UDP, with the test carrying
        // the plugin messages both ways as the game would.
        let srv = server();
        let me = Uuid([9; 16]);
        srv.set_players([(me, "overworld".into(), [0.0; 3])]);
        let client = VoiceClient::new(Settings {
            use_audio_devices: false,
            ..Settings::default()
        });
        client.on_plugin(
            plugin::MINECRAFT_REGISTER,
            &VoiceServer::register_payload(),
            "127.0.0.1",
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !client.status().connected {
            assert!(
                Instant::now() < deadline,
                "client didn't connect: {}",
                client.status().connection
            );
            for (ch, data) in client.take_outgoing() {
                srv.on_plugin(me, "Me", ch, &data);
            }
            for (_, ch, data) in srv.take_outgoing() {
                client.on_plugin(ch, &data, "127.0.0.1");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(srv.connected(), [me]);
    }
}
