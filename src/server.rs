//! The server part of mvchat (same mod, `side = "both"`).
//!
//! - `server_about_to_start`: generate a fresh 32-byte group secret (CSPRNG; never logged,
//!   never kept past the session) and open the punch UDP socket (port from the server config
//!   `punch_port`, default 25567 = 25565 + 2).
//! - `player_logged_in` / `player_logged_out`: maintain the roster and broadcast
//!   `mvchat:roster` (whole roster on every change) and `mvchat:gone`.
//! - The punch socket is polled (non-blocking) in `server_tick` — no extra thread: a valid
//!   ASK gets an ECHO with the sender's external address, plus an ECHO about every other
//!   known participant (the socket only keeps each uuid's last-seen address and time, for
//!   cleanup of stale entries).
//! - `server_stopping`: close the socket, drop the secret. A restart issues a new one.

use crate::engine::crypto;
use crate::engine::net;
use crate::engine::protocol::{Packet, PacketType, parse_packet};
use crate::payloads::{Roster, RosterEntry, Secret, VoiceRelay};
use mod_api::{IPayloadContext, PayloadSendError, ServerHost};
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Default punch port: the game port 25565 + 2 (like SVC).
pub const DEFAULT_PUNCH_PORT: u16 = 25567;
/// Stale punch entries are dropped after this long without an ASK.
const PUNCH_ENTRY_TTL: Duration = Duration::from_secs(60);

pub struct ServerState {
    pub secret: Option<[u8; 32]>,
    pub punch_port: u16,
    /// uuid -> (name, last-seen external address, last activity).
    pub roster: Vec<(u128, String)>,
    pub addrs: HashMap<u128, (SocketAddr, Instant)>,
    pub socket: Option<UdpSocket>,
    /// monotonic counter for the server-originated ECHO packets
    pub counter: u64,
}

impl ServerState {
    fn new() -> Self {
        Self {
            secret: None,
            punch_port: DEFAULT_PUNCH_PORT,
            roster: Vec::new(),
            addrs: HashMap::new(),
            socket: None,
            counter: 1,
        }
    }
}

pub static SERVER: OnceLock<Mutex<ServerState>> = OnceLock::new();

pub fn server_state() -> &'static Mutex<ServerState> {
    SERVER.get_or_init(|| Mutex::new(ServerState::new()))
}

/// `server_about_to_start`: fresh secret + punch socket.
pub fn on_about_to_start(_host: &mut dyn ServerHost) {
    let mut st = server_state().lock().unwrap();
    st.secret = Some(crypto::generate_secret());
    st.roster.clear();
    st.addrs.clear();
    st.counter = 1;
    let port = st.punch_port;
    match UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, port)) {
        Ok(socket) => {
            let _ = socket.set_nonblocking(true);
            st.socket = Some(socket);
        }
        Err(e) => {
            eprintln!("mvchat: cannot bind punch socket on port {port}: {e}");
            st.socket = None;
        }
    }
}

/// `server_stopping`: close the socket, drop the secret.
pub fn on_stopping(_host: &mut dyn ServerHost) {
    let mut st = server_state().lock().unwrap();
    st.socket = None;
    st.secret = None;
    st.roster.clear();
    st.addrs.clear();
}

/// `server_tick`: poll the punch socket (non-blocking).
pub fn on_tick(host: &mut dyn ServerHost) {
    let (socket, secret) = {
        let st = server_state().lock().unwrap();
        (
            st.socket.as_ref().and_then(|s| s.try_clone().ok()),
            st.secret,
        )
    };
    let (Some(socket), Some(secret)) = (socket, secret) else {
        return;
    };
    let pk = crypto::punch_key(&secret);
    let mut buf = [0u8; 2048];
    loop {
        match socket.recv_from(&mut buf) {
            Ok((n, from)) => {
                if let Some(packet) = parse_packet(&buf[..n]) {
                    handle_punch_packet(host, &pk, &secret, from, &packet);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
    // Drop stale address entries.
    let now = Instant::now();
    server_state()
        .lock()
        .unwrap()
        .addrs
        .retain(|_, (_, t)| now.duration_since(*t) < PUNCH_ENTRY_TTL);
}

fn handle_punch_packet(
    _host: &mut dyn ServerHost,
    pk: &[u8; 32],
    _secret: &[u8; 32],
    from: SocketAddr,
    packet: &Packet,
) {
    if packet.header.kind != PacketType::Ask {
        return;
    }
    if !crate::engine::verify_ask(pk, packet) {
        return;
    }
    let who = packet.header.sender;
    let mut st = server_state().lock().unwrap();
    let Some(secret) = st.secret else { return };
    let pk = crypto::punch_key(&secret);
    // Remember this participant's external address (last activity, for cleanup).
    st.addrs.insert(who, (from, Instant::now()));
    let others: Vec<(u128, SocketAddr)> = st
        .addrs
        .iter()
        .filter(|(uuid, _)| **uuid != who)
        .map(|(uuid, (addr, _))| (*uuid, *addr))
        .collect();
    let mut replies: Vec<(u128, SocketAddr, Vec<u8>)> = Vec::new();
    // ECHO about ourselves first (header.sender == who).
    let counter = st.next_counter();
    replies.push((
        who,
        from,
        crate::engine::build_echo(&pk, who, counter, who, &from),
    ));
    // Then an ECHO about every other known participant.
    for (peer, addr) in others {
        let counter = st.next_counter();
        replies.push((
            who,
            from,
            crate::engine::build_echo(&pk, who, counter, peer, &addr),
        ));
    }
    let _ = secret;
    drop(st);
    // Send the ECHOes through the punch socket.
    let socket = server_state()
        .lock()
        .unwrap()
        .socket
        .as_ref()
        .and_then(|s| s.try_clone().ok());
    if let Some(socket) = socket {
        for (_, to, bytes) in replies {
            let _ = net::send(&socket, to, &bytes);
        }
    }
}

impl ServerState {
    fn next_counter(&mut self) -> u64 {
        let c = self.counter;
        self.counter += 1;
        c
    }
}

/// Broadcasts the whole roster to every connected player (`mvchat:roster`).
pub fn broadcast_roster(host: &mut dyn ServerHost) {
    let Some(token) = crate::payloads::ROSTER.get() else {
        return;
    };
    let st = server_state().lock().unwrap();
    let Some(secret) = st.secret else { return };
    let roster = Roster {
        secret: Secret(secret),
        udp_port: st.punch_port,
        you: crate::payloads::Uuid(0), // per-recipient below
        entries: st
            .roster
            .iter()
            .map(|(uuid, name)| RosterEntry {
                uuid: crate::payloads::Uuid(*uuid),
                name: name.clone(),
            })
            .collect(),
    };
    drop(st);
    let players = host.players().server_players();
    for player in players {
        let uuid = player.uuid();
        let mut per_player = roster.clone();
        per_player.you = crate::payloads::Uuid(uuid);
        if let Err(e) = token.send(host, player, &per_player) {
            if matches!(e, PayloadSendError::UnsupportedPeer(_)) {
                // A client without the mod: it simply gets nothing.
            } else {
                eprintln!("mvchat: roster send failed: {e:?}");
            }
        }
    }
}

/// `player_logged_in`: add to the roster and broadcast.
pub fn on_player_in(host: &mut dyn ServerHost, uuid: u128, name: &str) {
    let mut st = server_state().lock().unwrap();
    if !st.roster.iter().any(|(u, _)| *u == uuid) {
        st.roster.push((uuid, name.to_owned()));
    }
    drop(st);
    broadcast_roster(host);
}

/// `player_logged_out`: remove, tell everyone (`mvchat:gone`), rebroadcast the roster.
pub fn on_player_out(host: &mut dyn ServerHost, uuid: u128) {
    {
        let mut st = server_state().lock().unwrap();
        st.roster.retain(|(u, _)| *u != uuid);
        st.addrs.remove(&uuid);
    }
    if let Some(token) = crate::payloads::GONE.get() {
        let players = host.players().server_players();
        for player in players {
            let _ = token.send(
                host,
                player,
                &crate::payloads::Gone {
                    who: crate::payloads::Uuid(uuid),
                },
            );
        }
    }
    broadcast_roster(host);
}

/// `mvchat:join` handler (server side): the player has the mod — (re)send them the roster.
pub fn handle_join(
    _value: crate::payloads::Join,
    cx: &mut dyn IPayloadContext,
) -> Result<(), String> {
    let host = cx.server_host().map_err(|e| format!("{e:?}"))?;
    broadcast_roster(host);
    Ok(())
}

/// `mvchat:voice` handler (server side, wasm relay only): forward to every other participant.
pub fn handle_voice_relay(value: VoiceRelay, cx: &mut dyn IPayloadContext) -> Result<(), String> {
    let host = cx.server_host().map_err(|e| format!("{e:?}"))?;
    let Some(down) = crate::payloads::VOICE_DOWN.get() else {
        return Ok(());
    };
    let players = host.players().server_players();
    for player in players {
        if player.uuid() != value.from.0 {
            let _ = down.send(host, player, &value);
        }
    }
    Ok(())
}
