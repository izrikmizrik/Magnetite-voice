//! The shared voice engine: state, peer table, and the tick that the game's hooks drive.
//!
//! ```text
//! Off -> Waiting (level loaded, mod on)
//! Waiting -> Connecting (roster with secret arrived; punch-PINGs start)
//! Connecting -> Ready (every peer sent a valid PONG/VOICE)
//! Ready -> Connecting (5 s without packets from a peer)
//! any -> Off (level unloaded / client stopping)
//! ```
//!
//! Two mod-spawned threads exist (native build): the UDP receive thread and the pacer.
//! Everything else runs in the game tick. Codec, crypto, jitter and protocol are pure
//! modules with their own unit tests.

pub mod crypto;
pub mod jitter;
pub mod net;
pub mod protocol;

pub mod audio;
#[cfg(feature = "opus")]
pub mod codec;

use crate::engine::crypto::{KEY_LEN, ReplayWindow, pair_key, punch_key};
use crate::engine::jitter::FRAME_SAMPLES;
use crate::engine::jitter::JitterBuffer;
#[cfg(not(target_arch = "wasm32"))]
use crate::engine::net::{ASK_INTERVAL, PACER_TICK, PING_INTERVAL};
use crate::engine::net::{Datagram, PEER_TIMEOUT};
use crate::engine::protocol::Pose;
use crate::engine::protocol::{
    Header, Packet, PacketType, parse_packet, read_addr, write_addr, write_packet,
};
use std::collections::{BTreeMap, VecDeque};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineState {
    Off,
    Waiting,
    Connecting,
    Ready,
}

/// How the mic gate opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicMode {
    /// Tap-to-talk toggle ("рация"): a tap on the mic button flips `mic_open`.
    Radio,
    /// Push-to-talk: talk while the registered key is held.
    Ptt,
    /// Voice activation: RMS above `va_threshold`.
    Va,
}

/// Snapshot of the settings the tick needs (read from Options > Mods each tick).
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub enabled: bool,
    pub mode: MicMode,
    pub va_threshold: f32,
    pub output_volume: f32,
    pub mic_gain: f32,
    pub max_distance: f32,
    pub spatial: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: MicMode::Radio,
            va_threshold: 0.05,
            output_volume: 1.0,
            mic_gain: 1.0,
            max_distance: 32.0,
            spatial: true,
        }
    }
}

/// One remote participant.
pub struct Peer {
    pub uuid: u128,
    pub name: String,
    pub key: [u8; KEY_LEN],
    /// Candidate external addresses learned from ECHO packets.
    pub candidates: Vec<SocketAddr>,
    /// The confirmed path (first valid PONG/VOICE source).
    pub addr: Option<SocketAddr>,
    pub replay: ReplayWindow,
    pub jitter: JitterBuffer,
    #[cfg(feature = "opus")]
    pub decoder: Option<codec::Decoder>,
    pub pose: Pose,
    pub last_seen: Instant,
    pub last_ping: Instant,
    /// Set when a VOICE packet arrived; the HUD shows the speaker until this passes.
    pub speaking_until: Instant,
    pub muted: bool,
    pub volume: f32,
    pub rx_packets: u64,
    pub tx_packets: u64,
}

/// State shared between the game tick, the receive thread and the pacer.
pub struct Shared {
    pub state: EngineState,
    /// `Some(false)` once a send told us the server has no mvchat (UnsupportedPeer).
    pub server_has_mod: Option<bool>,
    pub you: u128,
    pub secret: Option<[u8; 32]>,
    /// The server's punch socket (voice server address + roster's udp_port).
    pub punch: Option<SocketAddr>,
    pub peers: BTreeMap<u128, Peer>,
    /// "Рация" toggle, flipped by the dock's mic button.
    pub mic_open: bool,
    pub muted: bool,
    pub deafened: bool,
    /// Camera pose written every tick; sent inside VOICE packets.
    pub pose: Pose,
    /// Outgoing packet counter (monotonic per sender).
    pub counter: u64,
    /// Raw packets for the pacer to send: (address, bytes). Native P2P only.
    pub outbox: VecDeque<(SocketAddr, Vec<u8>)>,
    /// On wasm (no sockets) the same wire packets queue here, wrapped into `mvchat:voice`
    /// payloads and sent through the game server in the client tick.
    pub relay_outbox: VecDeque<Vec<u8>>,
    /// Sender half of the inbox: the receive thread (and the wasm relay handler) push here.
    pub inbox_tx: Option<Sender<Datagram>>,
    /// Counters for the menu.
    pub rx_datagrams: u64,
    pub tx_datagrams: u64,
    /// Last error text for the menu status line.
    pub last_error: Option<String>,
}

impl Shared {
    pub fn new() -> Self {
        Self {
            state: EngineState::Off,
            server_has_mod: None,
            you: 0,
            secret: None,
            punch: None,
            peers: BTreeMap::new(),
            mic_open: false,
            muted: false,
            deafened: false,
            pose: Pose::default(),
            counter: 1,
            outbox: VecDeque::new(),
            relay_outbox: VecDeque::new(),
            inbox_tx: None,
            rx_datagrams: 0,
            tx_datagrams: 0,
            last_error: None,
        }
    }

    pub fn next_counter(&mut self) -> u64 {
        let c = self.counter;
        self.counter += 1;
        c
    }
}

/// Where a "confirmed path" points on wasm, where the relay replaces P2P addresses.
pub const RELAY_ADDR: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 9);

/// Queues one wire packet for sending: P2P address on native, relay payload on wasm.
fn queue_packet(shared: &mut Shared, addr: SocketAddr, bytes: Vec<u8>) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        shared.outbox.push_back((addr, bytes));
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = addr;
        shared.relay_outbox.push_back(bytes);
    }
}

impl Default for Shared {
    fn default() -> Self {
        Self::new()
    }
}

/// The running engine (client side). `None` when off.
pub struct VoiceEngine {
    pub shared: Arc<Mutex<Shared>>,
    /// `None` on wasm, where the transport is the `mvchat:voice` relay payload.
    socket: Option<Arc<UdpSocket>>,
    rx: Option<Receiver<Datagram>>,
    stop: Arc<AtomicBool>,
    pacer: Option<std::thread::JoinHandle<()>>,
    receiver: Option<std::thread::JoinHandle<()>>,
    #[cfg(feature = "opus")]
    encoder: Option<codec::Encoder>,
    audio: Option<Box<dyn audio::AudioBackend>>,
    resampler: jitter::Resampler,
    framer: jitter::Framer,
}

impl VoiceEngine {
    /// Binds the voice socket and spawns the receive and pacer threads (native). Called when
    /// the first roster arrives (state -> Connecting) or when the level loads (state ->
    /// Waiting). On wasm there is no socket: the engine only tracks state and the voice
    /// goes through the `mvchat:voice` relay payload.
    pub fn start(shared: Arc<Mutex<Shared>>, punch: Option<SocketAddr>) -> Result<Self, String> {
        if let Some(punch) = punch {
            shared.lock().unwrap().punch = Some(punch);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let socket = Arc::new(net::bind_voice_socket().map_err(|e| e.to_string())?);
            let (tx, rx) = channel::<Datagram>();
            shared.lock().unwrap().inbox_tx = Some(tx.clone());
            let receiver = net::spawn_receiver(socket.clone(), tx);
            let stop = Arc::new(AtomicBool::new(false));
            let pacer = {
                let socket = socket.clone();
                let shared = shared.clone();
                let stop = stop.clone();
                std::thread::Builder::new()
                    .name("mvchat-pacer".into())
                    .spawn(move || pacer_loop(socket, shared, stop))
                    .map_err(|e| e.to_string())?
            };
            #[cfg(feature = "opus")]
            let encoder = codec::Encoder::new().ok();
            let audio = Some(audio::open(None, None));
            Ok(Self {
                shared,
                socket: Some(socket),
                rx: Some(rx),
                stop,
                pacer: Some(pacer),
                receiver: Some(receiver),
                #[cfg(feature = "opus")]
                encoder,
                audio,
                resampler: jitter::Resampler::default(),
                framer: jitter::Framer::default(),
            })
        }
        #[cfg(target_arch = "wasm32")]
        {
            // No threads on wasm: the relay handler feeds the inbox, the tick drains it.
            let (tx, rx) = channel::<Datagram>();
            shared.lock().unwrap().inbox_tx = Some(tx);
            #[cfg(feature = "opus")]
            let encoder = codec::Encoder::new().ok();
            Ok(Self {
                shared,
                socket: None,
                rx: Some(rx),
                stop: Arc::new(AtomicBool::new(false)),
                pacer: None,
                receiver: None,
                #[cfg(feature = "opus")]
                encoder,
                audio: Some(audio::open(None, None)),
                resampler: jitter::Resampler::default(),
                framer: jitter::Framer::default(),
            })
        }
    }

    /// Sends BYE to every confirmed peer and stops both threads (native).
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        {
            let mut shared = self.shared.lock().unwrap();
            let counter_peers: Vec<(u128, SocketAddr, [u8; KEY_LEN])> = shared
                .peers
                .iter()
                .filter_map(|(uuid, p)| p.addr.map(|a| (*uuid, a, p.key)))
                .collect();
            let you = shared.you;
            for (uuid, addr, key) in &counter_peers {
                let counter = shared.next_counter();
                let header = Header {
                    sender: you,
                    counter,
                    kind: PacketType::By,
                };
                let mac = crypto::mac8(key, &header, b"");
                queue_packet(&mut shared, *addr, write_packet(&header, &mac));
                let _ = uuid;
            }
            if let Some(socket) = &self.socket {
                while let Some((addr, bytes)) = shared.outbox.pop_front() {
                    let _ = net::send(socket, addr, &bytes);
                }
            }
            shared.state = EngineState::Off;
            shared.outbox.clear();
            shared.relay_outbox.clear();
            shared.inbox_tx = None;
        }
        drop(self.rx.take());
        if let Some(h) = self.receiver.take() {
            let _ = h.join();
        }
        if let Some(h) = self.pacer.take() {
            let _ = h.join();
        }
    }

    /// One game tick: drain received datagrams, run the state machine, capture/encode/send
    /// voice, mix and queue playback.
    pub fn tick(&mut self, pose: Pose, ptt_down: bool, settings: &Settings) {
        let now = Instant::now();
        self.shared.lock().unwrap().pose = pose;
        self.drain_inbox(now);
        self.update_state(now);
        if settings.enabled {
            self.capture_and_send(pose, ptt_down, settings, now);
            self.mix_and_play(settings, pose);
        }
    }

    fn drain_inbox(&mut self, now: Instant) {
        let mut shared = self.shared.lock().unwrap();
        if let Some(rx) = &self.rx {
            while let Ok((from, bytes)) = rx.try_recv() {
                shared.rx_datagrams += 1;
                if let Some(packet) = parse_packet(&bytes) {
                    Self::handle_packet(&mut shared, from, packet, now);
                }
            }
        }
    }

    fn handle_packet(shared: &mut Shared, from: SocketAddr, packet: Packet, now: Instant) {
        let Some(secret) = shared.secret else { return };
        match packet.header.kind {
            PacketType::Ask => {
                // Clients never answer ASKs; only the server's punch socket does.
                let _ = (from, secret);
            }
            PacketType::Echo => {
                // body = mac8(header || addr) || addr
                if packet.body.len() < crypto::MAC_LEN {
                    return;
                }
                let (mac, addr_bytes) = packet.body.split_at(crypto::MAC_LEN);
                let pk = punch_key(&secret);
                if !crypto::verify_mac8(&pk, &packet.header, addr_bytes, mac) {
                    return;
                }
                let Some((addr, _)) = read_addr(addr_bytes) else {
                    return;
                };
                if packet.header.sender == shared.you {
                    // Our own external address as the punch socket sees it.
                    shared.last_error = Some(format!("external {addr}"));
                } else if let Some(peer) = shared.peers.get_mut(&packet.header.sender)
                    && !peer.candidates.contains(&addr)
                {
                    peer.candidates.push(addr);
                }
            }
            PacketType::Ping | PacketType::Pong => {
                let Some(key) = shared.peers.get(&packet.header.sender).map(|p| p.key) else {
                    return;
                };
                if packet.body.len() != crypto::MAC_LEN {
                    return;
                }
                if !crypto::verify_mac8(&key, &packet.header, b"", &packet.body) {
                    return;
                }
                let is_ping = packet.header.kind == PacketType::Ping;
                {
                    let Some(peer) = shared.peers.get_mut(&packet.header.sender) else {
                        return;
                    };
                    if !peer.replay.check_and_update(packet.header.counter) {
                        return;
                    }
                    peer.rx_packets += 1;
                    peer.last_seen = now;
                    if peer.addr.is_none() || !is_ping {
                        peer.addr = Some(from);
                    }
                }
                if is_ping {
                    let counter = shared.next_counter();
                    let reply = Header {
                        sender: shared.you,
                        counter,
                        kind: PacketType::Pong,
                    };
                    let mac = crypto::mac8(&key, &reply, b"");
                    queue_packet(shared, from, write_packet(&reply, &mac));
                }
            }
            PacketType::Voice => {
                let Some(key) = shared.peers.get(&packet.header.sender).map(|p| p.key) else {
                    return;
                };
                let Some(plain) = crypto::open_voice(&key, &packet.header, &packet.body) else {
                    return;
                };
                let Some((pose, opus)) = protocol::read_pose(&plain) else {
                    return;
                };
                let Some(peer) = shared.peers.get_mut(&packet.header.sender) else {
                    return;
                };
                if !peer.replay.check_and_update(packet.header.counter) {
                    return;
                }
                if !opus.is_empty() {
                    peer.pose = pose;
                    peer.jitter.push(packet.header.counter, opus.to_vec());
                    peer.speaking_until = now + Duration::from_millis(500);
                }
                peer.rx_packets += 1;
                peer.last_seen = now;
                if peer.addr.is_none() {
                    peer.addr = Some(from);
                }
            }
            PacketType::By => {
                shared.peers.remove(&packet.header.sender);
            }
        }
    }

    fn update_state(&mut self, now: Instant) {
        let mut shared = self.shared.lock().unwrap();
        match shared.state {
            EngineState::Connecting => {
                let all_ready =
                    !shared.peers.is_empty() && shared.peers.values().all(|p| p.addr.is_some());
                if all_ready {
                    shared.state = EngineState::Ready;
                }
            }
            EngineState::Ready => {
                let stale = shared
                    .peers
                    .values()
                    .any(|p| now.duration_since(p.last_seen) > PEER_TIMEOUT);
                if stale {
                    shared.state = EngineState::Connecting;
                }
            }
            _ => {}
        }
    }

    /// Queues a sealed VOICE packet to every confirmed peer.
    #[cfg(feature = "opus")]
    fn send_voice(shared: &mut Shared, pose: Pose, opus: Vec<u8>) {
        {
            let mut plain = Vec::with_capacity(protocol::POSE_LEN + opus.len());
            protocol::write_pose(&mut plain, &pose);
            plain.extend_from_slice(&opus);
            let targets: Vec<(u128, SocketAddr)> = shared
                .peers
                .iter()
                .filter(|(_, p)| p.addr.is_some())
                .map(|(u, p)| (*u, p.addr.unwrap()))
                .collect();
            for (uuid, addr) in targets {
                let key = pair_key(&shared.secret.unwrap(), shared.you, uuid);
                let counter = shared.next_counter();
                let header = Header {
                    sender: shared.you,
                    counter,
                    kind: PacketType::Voice,
                };
                let body = crypto::seal_voice(&key, &header, &plain);
                shared
                    .outbox
                    .push_back((addr, write_packet(&header, &body)));
                if let Some(peer) = shared.peers.get_mut(&uuid) {
                    peer.tx_packets += 1;
                }
            }
        }
    }

    fn capture_and_send(&mut self, pose: Pose, ptt_down: bool, settings: &Settings, _now: Instant) {
        let Some(audio) = &self.audio else { return };
        let samples = audio.drain_capture();
        if samples.is_empty() {
            return;
        }
        let input_rate = audio.input_rate();
        let resampled = self.resampler.resample(&samples, input_rate);
        let frames = self.framer.push(&resampled);
        // Encode first (the encoder borrows self), then queue the packets.
        let mut packets: Vec<Vec<u8>> = Vec::new();
        {
            let Some(encoder) = &mut self.encoder else {
                return;
            };
            for mut frame in frames {
                for s in frame.iter_mut() {
                    *s *= settings.mic_gain.clamp(0.0, 2.0);
                    *s = s.clamp(-1.0, 1.0);
                }
                let gate = match settings.mode {
                    MicMode::Radio => self.shared.lock().unwrap().mic_open,
                    MicMode::Ptt => ptt_down,
                    MicMode::Va => jitter::rms(&frame) > settings.va_threshold,
                };
                let muted = {
                    let s = self.shared.lock().unwrap();
                    s.muted || s.deafened
                };
                if !gate || muted {
                    continue;
                }
                match encoder.encode(&frame) {
                    Ok(opus) => packets.push(opus),
                    Err(e) => {
                        self.shared.lock().unwrap().last_error = Some(format!("opus: {e:?}"));
                    }
                }
            }
        }
        let mut shared = self.shared.lock().unwrap();
        for opus in packets {
            Self::send_voice(&mut shared, pose, opus);
        }
    }

    fn mix_and_play(&mut self, settings: &Settings, camera: Pose) {
        let Some(audio) = &self.audio else { return };
        let mut shared = self.shared.lock().unwrap();
        if shared.deafened || shared.peers.values().all(|p| p.addr.is_none()) {
            return;
        }
        let mut out = [[0.0f32; 2]; FRAME_SAMPLES];
        let mut any = false;
        for peer in shared.peers.values_mut() {
            if peer.addr.is_none() {
                continue;
            }
            let Some(dec) = peer.decoder.as_mut() else {
                continue;
            };
            let (distance, rel) = if settings.spatial {
                jitter::relative_angle(&camera, &peer.pose)
            } else {
                (0.0, 0.0)
            };
            let gain = jitter::distance_gain(distance, settings.max_distance)
                * peer.volume.clamp(0.0, 2.0)
                * settings.output_volume.clamp(0.0, 2.0);
            let frame = peer.jitter.pop_due();
            let pcm = match frame {
                Some(jitter::Frame::Packet(bytes)) => match dec.decode(&bytes) {
                    Ok(p) => p,
                    Err(_) => continue,
                },
                Some(jitter::Frame::Plc) => match dec.decode(&[]) {
                    Ok(p) => p,
                    Err(_) => continue,
                },
                None => continue,
            };
            any = true;
            let (pan_l, pan_r) = if settings.spatial {
                jitter::constant_power_pan(rel.to_radians())
            } else {
                (1.0, 1.0)
            };
            let gl = gain * pan_l;
            let gr = gain * pan_r;
            for (o, s) in out.iter_mut().zip(pcm.iter()) {
                o[0] += s * gl;
                o[1] += s * gr;
            }
        }
        drop(shared);
        if any {
            audio.push_playback(&out);
        }
    }
}

/// The pacer thread (native only): sends the outbox every 20 ms, PINGs unconfirmed
/// candidates every 500 ms, and ASKs the punch socket every second until answered.
/// On wasm there are no threads: the tick drains the relay outbox instead.
#[cfg(not(target_arch = "wasm32"))]
fn pacer_loop(socket: Arc<UdpSocket>, shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>) {
    let mut last_ping = Instant::now() - PING_INTERVAL;
    let mut last_ask = Instant::now() - ASK_INTERVAL;
    while !stop.load(Ordering::SeqCst) {
        let mut to_send: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        {
            let mut shared = shared.lock().unwrap();
            while let Some(item) = shared.outbox.pop_front() {
                to_send.push(item);
            }
            let now = Instant::now();
            if now.duration_since(last_ping) >= PING_INTERVAL {
                last_ping = now;
                let you = shared.you;
                let mut pings: Vec<(SocketAddr, [u8; KEY_LEN])> = Vec::new();
                for peer in shared.peers.values_mut() {
                    if peer.addr.is_none() {
                        let n = peer.candidates.len() as u64;
                        for addr in peer.candidates.iter().copied() {
                            pings.push((addr, peer.key));
                        }
                        peer.tx_packets += n;
                        peer.last_ping = now;
                    }
                }
                for (addr, key) in pings {
                    let counter = shared.next_counter();
                    let header = Header {
                        sender: you,
                        counter,
                        kind: PacketType::Ping,
                    };
                    let mac = crypto::mac8(&key, &header, b"");
                    to_send.push((addr, write_packet(&header, &mac)));
                }
            }
            if now.duration_since(last_ask) >= ASK_INTERVAL
                && let (Some(punch), Some(secret)) = (shared.punch, shared.secret)
            {
                last_ask = now;
                let pk = punch_key(&secret);
                let counter = shared.next_counter();
                let header = Header {
                    sender: shared.you,
                    counter,
                    kind: PacketType::Ask,
                };
                let mac = crypto::mac8(&pk, &header, b"");
                to_send.push((punch, write_packet(&header, &mac)));
            }
        }
        for (addr, bytes) in to_send {
            if net::send(&socket, addr, &bytes).is_ok() {
                shared.lock().unwrap().tx_datagrams += 1;
            }
        }
        std::thread::sleep(PACER_TICK);
    }
}

/// Builds the peer table from a roster (secret, own uuid, entries). Called from the
/// `mvchat:roster` handler.
pub fn apply_roster(
    shared: &Arc<Mutex<Shared>>,
    secret: [u8; 32],
    you: u128,
    entries: &[(u128, String)],
) {
    let mut s = shared.lock().unwrap();
    s.secret = Some(secret);
    s.you = you;
    s.state = EngineState::Connecting;
    for (uuid, name) in entries {
        if *uuid == you {
            continue;
        }
        let key = pair_key(&secret, you, *uuid);
        // On wasm there is no P2P path to open: every peer is reached through the relay,
        // so the roster itself confirms the path.
        #[cfg(target_arch = "wasm32")]
        let addr = Some(RELAY_ADDR);
        #[cfg(not(target_arch = "wasm32"))]
        let addr = None;
        s.peers
            .entry(*uuid)
            .and_modify(|p| {
                p.name = name.clone();
                p.key = key;
                if cfg!(target_arch = "wasm32") {
                    p.addr = Some(RELAY_ADDR);
                }
            })
            .or_insert_with(|| Peer {
                uuid: *uuid,
                name: name.clone(),
                key,
                candidates: Vec::new(),
                addr,
                replay: ReplayWindow::new(),
                jitter: JitterBuffer::new(),
                #[cfg(feature = "opus")]
                decoder: codec::Decoder::new().ok(),
                pose: Pose::default(),
                last_seen: Instant::now(),
                last_ping: Instant::now(),
                speaking_until: Instant::now(),
                muted: false,
                volume: 1.0,
                rx_packets: 0,
                tx_packets: 0,
            });
    }
    s.peers
        .retain(|uuid, _| *uuid == you || entries.iter().any(|(u, _)| u == uuid));
}

/// Drains the wasm relay outbox: raw wire packets to wrap into `mvchat:voice` payloads.
pub fn take_relay_outbox(shared: &Arc<Mutex<Shared>>) -> Vec<Vec<u8>> {
    std::mem::take(&mut shared.lock().unwrap().relay_outbox)
        .into_iter()
        .collect()
}

/// Removes a peer (`mvchat:gone`).
pub fn remove_peer(shared: &Arc<Mutex<Shared>>, who: u128) {
    shared.lock().unwrap().peers.remove(&who);
}

/// Convenience for the HUD: who is speaking right now.
pub fn speaking_now(shared: &Arc<Mutex<Shared>>) -> Vec<String> {
    let now = Instant::now();
    shared
        .lock()
        .unwrap()
        .peers
        .values()
        .filter(|p| p.speaking_until > now)
        .map(|p| p.name.clone())
        .collect()
}

/// Serializes an ECHO body for the server's punch socket: mac8(header||addr) || addr.
pub fn echo_body(pk: &[u8; 32], header: &Header, addr: &SocketAddr) -> Vec<u8> {
    let mut addr_bytes = Vec::new();
    write_addr(&mut addr_bytes, addr);
    let mac = crypto::mac8(pk, header, &addr_bytes);
    let mut body = mac.to_vec();
    body.extend_from_slice(&addr_bytes);
    body
}

/// Builds an ECHO packet (server side) reporting `addr` for participant `about`.
pub fn build_echo(
    pk: &[u8; 32],
    sender_uuid: u128,
    counter: u64,
    about: u128,
    addr: &SocketAddr,
) -> Vec<u8> {
    let header = Header {
        sender: about,
        counter,
        kind: PacketType::Echo,
    };
    let _ = sender_uuid;
    write_packet(&header, &echo_body(pk, &header, addr))
}

/// Builds an ASK packet (client side).
pub fn build_ask(pk: &[u8; 32], you: u128, counter: u64) -> Vec<u8> {
    let header = Header {
        sender: you,
        counter,
        kind: PacketType::Ask,
    };
    let mac = crypto::mac8(pk, &header, b"");
    write_packet(&header, &mac)
}

/// Parses an ASK from the punch socket: valid when body == mac8(punch_key, header).
pub fn verify_ask(pk: &[u8; 32], packet: &Packet) -> bool {
    packet.header.kind == PacketType::Ask
        && packet.body.len() == crypto::MAC_LEN
        && crypto::verify_mac8(pk, &packet.header, b"", &packet.body)
}
