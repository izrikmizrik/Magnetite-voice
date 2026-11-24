//! The mvchat wire protocol over UDP (all numbers big-endian).
//!
//! Every packet starts with a 28-byte header:
//!
//! | offset | field                              | size |
//! |--------|------------------------------------|------|
//! | 0      | magic `b"MV"`                      | 2    |
//! | 2      | protocol version (u8, = 1)         | 1    |
//! | 3      | packet type (u8)                   | 1    |
//! | 4      | sender uuid (u128)                 | 16   |
//! | 20     | packet counter (u64, monotonic per sender) | 8 |
//! | 28     | body                               | ...  |
//!
//! - `VOICE`: body is the AEAD ciphertext (nonce and key in `crypto.rs`) over the
//!   plaintext `[pose 16 bytes: x, y, z, yaw as f32 BE][Opus frame]`. The header is the
//!   AEAD's AAD, so the pose inside is authentic and gives positional sound.
//! - `PING`/`PONG`/`ASK`/`ECHO`: NAT hole punching. Body is the 8-byte truncated Poly1305
//!   MAC over the header (plus the reported address for `ECHO`) under the pair key, so
//!   foreign punch traffic is dropped. A client sends `PING` to a candidate every 500 ms;
//!   the first valid `PONG`/`VOICE` opens the path. `ASK` to the server's punch socket asks
//!   "tell me my external address"; `ECHO` carries it back (and, with the peer's uuid in the
//!   header, that peer's last-seen external address).
//! - `BYE`: clean exit (level closed / server gone), so the peer stops waiting immediately
//!   instead of by timeout.

/// Magic bytes of every packet.
pub const MAGIC: [u8; 2] = *b"MV";
/// Protocol version; bump to evolve the format without breaking old clients.
pub const VERSION: u8 = 1;
/// Header size in bytes: magic(2) + version(1) + type(1) + uuid(16) + counter(8).
pub const HEADER_LEN: usize = 28;
/// Maximum body we accept from a peer (Opus frame + pose + tag fits well below this).
pub const MAX_BODY_LEN: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Ping = 1,
    Pong = 2,
    Ask = 3,
    Echo = 4,
    Voice = 5,
    By = 6,
}

impl PacketType {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Ping),
            2 => Some(Self::Pong),
            3 => Some(Self::Ask),
            4 => Some(Self::Echo),
            5 => Some(Self::Voice),
            6 => Some(Self::By),
            _ => None,
        }
    }
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// The 28-byte packet header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub sender: u128,
    pub counter: u64,
    pub kind: PacketType,
}

/// A parsed packet: the header plus the (still encrypted or MACed) body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    pub header: Header,
    pub body: Vec<u8>,
}

/// Serializes the header into `out` (exactly [`HEADER_LEN`] bytes).
pub fn write_header(out: &mut Vec<u8>, header: &Header) {
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(header.kind.as_u8());
    out.extend_from_slice(&header.sender.to_be_bytes());
    out.extend_from_slice(&header.counter.to_be_bytes());
}

/// Serializes a whole packet.
pub fn write_packet(header: &Header, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    write_header(&mut out, header);
    out.extend_from_slice(body);
    out
}

/// Parses one packet; `None` for anything malformed (bad magic, unknown version or type,
/// truncated header, oversized body).
pub fn parse_packet(bytes: &[u8]) -> Option<Packet> {
    if bytes.len() < HEADER_LEN || bytes.len() > HEADER_LEN + MAX_BODY_LEN {
        return None;
    }
    if bytes[0..2] != MAGIC {
        return None;
    }
    if bytes[2] != VERSION {
        return None;
    }
    let kind = PacketType::from_u8(bytes[3])?;
    let mut sender = [0u8; 16];
    sender.copy_from_slice(&bytes[4..20]);
    let mut counter = [0u8; 8];
    counter.copy_from_slice(&bytes[20..28]);
    Some(Packet {
        header: Header {
            sender: u128::from_be_bytes(sender),
            counter: u64::from_be_bytes(counter),
            kind,
        },
        body: bytes[HEADER_LEN..].to_vec(),
    })
}

/// A speaker's pose as carried (encrypted) inside a VOICE packet: x, y, z, yaw.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pose {
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub yaw: f32,
}

pub const POSE_LEN: usize = 16;

pub fn write_pose(out: &mut Vec<u8>, pose: &Pose) {
    out.extend_from_slice(&pose.x.to_be_bytes());
    out.extend_from_slice(&pose.y.to_be_bytes());
    out.extend_from_slice(&pose.z.to_be_bytes());
    out.extend_from_slice(&pose.yaw.to_be_bytes());
}

/// Parses a pose from the first [`POSE_LEN`] bytes of `data`, returning the rest.
pub fn read_pose(data: &[u8]) -> Option<(Pose, &[u8])> {
    if data.len() < POSE_LEN {
        return None;
    }
    let x = f32::from_be_bytes(data[0..4].try_into().ok()?);
    let y = f32::from_be_bytes(data[4..8].try_into().ok()?);
    let z = f32::from_be_bytes(data[8..12].try_into().ok()?);
    let yaw = f32::from_be_bytes(data[12..16].try_into().ok()?);
    Some((Pose { x, y, z, yaw }, &data[POSE_LEN..]))
}

/// Serializes a socket address for an `ECHO` body: 1 byte tag (4 = IPv4, 6 = IPv6),
/// then the address bytes (4 or 16), then the port as u16 BE.
pub fn write_addr(out: &mut Vec<u8>, addr: &std::net::SocketAddr) {
    match addr {
        std::net::SocketAddr::V4(v4) => {
            out.push(4);
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_be_bytes());
        }
        std::net::SocketAddr::V6(v6) => {
            out.push(6);
            out.extend_from_slice(&v6.ip().octets());
            out.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
}

/// Parses an address written by [`write_addr`].
pub fn read_addr(data: &[u8]) -> Option<(std::net::SocketAddr, &[u8])> {
    let (tag, rest) = data.split_first()?;
    match tag {
        4 => {
            if rest.len() < 6 {
                return None;
            }
            let ip = std::net::Ipv4Addr::new(rest[0], rest[1], rest[2], rest[3]);
            let port = u16::from_be_bytes([rest[4], rest[5]]);
            Some((
                std::net::SocketAddr::new(std::net::IpAddr::V4(ip), port),
                &rest[6..],
            ))
        }
        6 => {
            if rest.len() < 18 {
                return None;
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&rest[..16]);
            let port = u16::from_be_bytes([rest[16], rest[17]]);
            Some((
                std::net::SocketAddr::new(
                    std::net::IpAddr::V6(std::net::Ipv6Addr::from(octets)),
                    port,
                ),
                &rest[18..],
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packet_round_trip() {
        let header = Header {
            sender: 0x0123_4567_89ab_cdef_fedc_ba98_7654_3210,
            counter: 42,
            kind: PacketType::Voice,
        };
        let body = vec![1, 2, 3, 4, 5];
        let bytes = write_packet(&header, &body);
        assert_eq!(bytes.len(), HEADER_LEN + body.len());
        assert_eq!(&bytes[0..2], b"MV");
        let parsed = parse_packet(&bytes).expect("parses");
        assert_eq!(parsed.header, header);
        assert_eq!(parsed.body, body);
    }

    #[test]
    fn rejects_bad_magic_version_type_and_truncation() {
        let header = Header {
            sender: 7,
            counter: 1,
            kind: PacketType::Ping,
        };
        let mut bytes = write_packet(&header, &[]);
        assert!(parse_packet(&bytes).is_some());
        bytes[0] = b'X';
        assert!(parse_packet(&bytes).is_none());
        bytes[0] = b'M';
        bytes[2] = 9;
        assert!(parse_packet(&bytes).is_none());
        bytes[2] = VERSION;
        bytes[3] = 99;
        assert!(parse_packet(&bytes).is_none());
        bytes[3] = PacketType::Ping.as_u8();
        assert!(parse_packet(&bytes[..HEADER_LEN - 1]).is_none());
        let mut huge = vec![0u8; HEADER_LEN + MAX_BODY_LEN + 1];
        huge[0..2].copy_from_slice(&MAGIC);
        huge[2] = VERSION;
        huge[3] = PacketType::Ping.as_u8();
        assert!(parse_packet(&huge).is_none());
    }

    #[test]
    fn pose_round_trip() {
        let pose = Pose {
            x: 1.5,
            y: -64.25,
            z: 300.0,
            yaw: 90.0,
        };
        let mut buf = Vec::new();
        write_pose(&mut buf, &pose);
        assert_eq!(buf.len(), POSE_LEN);
        buf.extend_from_slice(b"opus");
        let (read, rest) = read_pose(&buf).expect("pose");
        assert_eq!(read, pose);
        assert_eq!(rest, b"opus");
        assert!(read_pose(&buf[..POSE_LEN - 1]).is_none());
    }

    #[test]
    fn addr_round_trip() {
        for addr in [
            "192.168.1.10:25567".parse().unwrap(),
            "[2001:db8::1]:40000".parse().unwrap(),
        ] {
            let mut buf = Vec::new();
            write_addr(&mut buf, &addr);
            let (read, rest) = read_addr(&buf).expect("addr");
            assert_eq!(read, addr);
            assert!(rest.is_empty());
        }
        assert!(read_addr(&[]).is_none());
        assert!(read_addr(&[7, 1, 2]).is_none());
        assert!(read_addr(&[4, 1, 2]).is_none());
    }
}
