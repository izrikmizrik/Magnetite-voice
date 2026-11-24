//! The native UDP transport: one socket for everything (punch + voice), a receive thread
//! that only parses framing and hands raw datagrams to the game tick, and the pacer's send
//! path. Exactly two mod-spawned threads exist (receive + pacer), both owned by
//! [`super::VoiceEngine`].
//!
//! On wasm (`target_arch = "wasm32"`) there are no sockets: the transport is a stub and voice
//! would go through the `mvchat:voice` relay payload (the wasm variant's transport).

use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Duration;

/// Binds the voice socket on an ephemeral port (all interfaces; punching needs the NAT's
/// mapping of whatever local port we get). Native only: wasm has no sockets, so the wasm
/// variant's transport is the `mvchat:voice` relay payload.
#[cfg(not(target_arch = "wasm32"))]
pub fn bind_voice_socket() -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, 0))?;
    socket.set_nonblocking(false)?;
    Ok(socket)
}

/// Wasm stub: no UDP in the sandbox.
#[cfg(target_arch = "wasm32")]
pub fn bind_voice_socket() -> io::Result<UdpSocket> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no sockets on wasm",
    ))
}

/// A received datagram: source address and raw bytes (framing checked by the tick).
pub type Datagram = (SocketAddr, Vec<u8>);

/// Spawns the receive thread. It ends when the sender side is dropped (engine stop).
/// Native only: the wasm variant has no threads; its inbox is fed by the relay handler.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_receiver(socket: Arc<UdpSocket>, tx: Sender<Datagram>) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("mvchat-rx".into())
        .spawn(move || {
            let mut buf = vec![0u8; 2048];
            loop {
                match socket.recv_from(&mut buf) {
                    Ok((n, from)) => {
                        if tx.send((from, buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        })
        .expect("spawn mvchat-rx")
}

/// Wasm stub: no receive thread.
#[cfg(target_arch = "wasm32")]
pub fn spawn_receiver(
    _socket: Arc<UdpSocket>,
    _tx: Sender<Datagram>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("mvchat-rx".into())
        .spawn(|| {})
        .expect("spawn stub")
}

/// Sends one datagram (never blocks meaningfully on UDP).
#[cfg(not(target_arch = "wasm32"))]
pub fn send(socket: &UdpSocket, addr: SocketAddr, bytes: &[u8]) -> io::Result<()> {
    socket.send_to(bytes, addr).map(|_| ())
}

/// Wasm stub.
#[cfg(target_arch = "wasm32")]
pub fn send(_socket: &UdpSocket, _addr: SocketAddr, _bytes: &[u8]) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no sockets on wasm",
    ))
}

/// Parses `host:port` or `host` (default game port 25565) into an address, resolving via DNS
/// is the caller's business — we only handle literal IPs and `host:port` with a numeric port.
pub fn parse_server_address(text: &str, default_port: u16) -> Option<SocketAddr> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Some(addr);
    }
    let (host, port) = match text.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (text, default_port),
    };
    let host = host.trim_matches(['[', ']']);
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    // Resolve hostnames through the OS resolver.
    text.to_socket_addrs().ok()?.next()
}

/// How often a PING goes to an unconfirmed candidate.
pub const PING_INTERVAL: Duration = Duration::from_millis(500);
/// How often an ASK goes to the server's punch socket.
pub const ASK_INTERVAL: Duration = Duration::from_secs(1);
/// The pacer's send quantum: one 20 ms voice frame.
pub const PACER_TICK: Duration = Duration::from_millis(20);
/// A peer with no valid packet for this long drops back to Connecting.
pub const PEER_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_addresses() {
        assert_eq!(
            parse_server_address("127.0.0.1:25565", 25565),
            Some("127.0.0.1:25565".parse().unwrap())
        );
        assert_eq!(
            parse_server_address("127.0.0.1", 25565),
            Some("127.0.0.1:25565".parse().unwrap())
        );
        assert_eq!(
            parse_server_address("[::1]:40000", 25565),
            Some("[::1]:40000".parse().unwrap())
        );
        assert_eq!(parse_server_address("", 25565), None);
        assert_eq!(parse_server_address("127.0.0.1:abc", 25565), None);
        assert_eq!(
            parse_server_address("localhost:25567", 25565),
            Some("127.0.0.1:25567".parse().unwrap())
        );
    }

    #[test]
    fn loopback_socket_round_trip() {
        let a = bind_voice_socket().unwrap();
        let b = bind_voice_socket().unwrap();
        send(&a, b.local_addr().unwrap(), b"MV").unwrap();
        b.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut buf = [0u8; 8];
        let (n, from) = b.recv_from(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"MV");
        // The bound socket reports 0.0.0.0; the datagram's source is the loopback address.
        assert_eq!(from.port(), a.local_addr().unwrap().port());
        assert!(from.ip().is_loopback());
    }
}
