//! The signaling channel: typed payloads over the game connection (§4 of the ТЗ).
//!
//! | channel         | direction | type                          | purpose |
//! |-----------------|-----------|-------------------------------|---------|
//! | `mvchat:join`   | C -> S    | `{ version: u8 }`             | "I have voice chat" |
//! | `mvchat:roster` | S -> C    | `{ secret, udp_port, you, entries }` | secret, punch port, own uuid, participants |
//! | `mvchat:gone`   | S -> C    | `{ who: u128 }`               | a participant left |
//! | `mvchat:voice`  | both      | `{ from, counter, ciphertext }` | wasm-variant relay transport (§9) |
//!
//! All are declared `optional()`: a client without the mod gets nothing, and a send to a
//! server without the mod returns `UnsupportedPeer`, which we show honestly as
//! "server without voice chat".

use mod_api::{
    PayloadCodec, PayloadDecodeError, PayloadId, PayloadReader, PayloadWriter, Registrar, ToClient,
    ToServer, VarLong, payload_schema_leaf,
};
use std::sync::OnceLock;

/// Wire version of the mod's signaling protocol, sent in `mvchat:join`.
pub const SIGNALING_VERSION: u8 = 1;

/// A UUID on the wire (`u128` has no blanket codec; the `uuid` leaf is 16 bytes, MSB first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Uuid(pub u128);

impl PayloadCodec for Uuid {
    fn encode(&self, out: &mut PayloadWriter) {
        out.uuid(self.0);
    }
    fn decode(input: &mut PayloadReader<'_>) -> Result<Self, PayloadDecodeError> {
        input.uuid().map(Uuid)
    }
    fn schema(_visiting: &mut Vec<std::any::TypeId>) -> u64 {
        payload_schema_leaf("uuid")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PayloadCodec)]
pub struct Join {
    pub version: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, PayloadCodec)]
pub struct RosterEntry {
    pub uuid: Uuid,
    pub name: String,
}

/// The 32-byte group secret. Hand-written codec: exactly 32 bytes, length-prefixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Secret(pub [u8; 32]);

impl PayloadCodec for Secret {
    fn encode(&self, out: &mut PayloadWriter) {
        out.bytes(&self.0, 32);
    }
    fn decode(input: &mut PayloadReader<'_>) -> Result<Self, PayloadDecodeError> {
        let bytes = input.bytes(32)?;
        let mut secret = [0u8; 32];
        secret.copy_from_slice(bytes);
        Ok(Secret(secret))
    }
    fn schema(_visiting: &mut Vec<std::any::TypeId>) -> u64 {
        payload_schema_leaf("mvchat:secret/1")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PayloadCodec)]
pub struct Roster {
    pub secret: Secret,
    pub udp_port: u16,
    pub you: Uuid,
    pub entries: Vec<RosterEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PayloadCodec)]
pub struct Gone {
    pub who: Uuid,
}

/// The wasm variant's relay transport (§9): a whole encrypted wire packet (header+body,
/// from `engine::protocol`) carried through the game server. Only used when built without
/// `pc-audio`; the native build never sends voice through the server.
#[derive(Debug, Clone, PartialEq, Eq, PayloadCodec)]
pub struct VoiceRelay {
    pub from: Uuid,
    pub counter: VarLong,
    pub packet: Vec<u8>,
}

pub static JOIN: OnceLock<ToServer<Join>> = OnceLock::new();
pub static ROSTER: OnceLock<ToClient<Roster>> = OnceLock::new();
pub static GONE: OnceLock<ToClient<Gone>> = OnceLock::new();
pub static VOICE_UP: OnceLock<ToServer<VoiceRelay>> = OnceLock::new();
pub static VOICE_DOWN: OnceLock<ToClient<VoiceRelay>> = OnceLock::new();

/// Declares all channels in `Hooks::register`. Handlers are wired in `lib.rs`/`server.rs`
/// through the statics above.
pub fn register(r: &mut dyn Registrar) {
    let mut payloads = r.payload_handlers();
    let mut view = payloads.registrar("1").optional();

    let join = view
        .play_to_server_typed::<Join>(PayloadId("mvchat:join".into()), crate::server::handle_join);
    let _ = JOIN.set(join);

    let roster = view.play_to_client_typed::<Roster>(
        PayloadId("mvchat:roster".into()),
        crate::client_handle_roster,
    );
    let _ = ROSTER.set(roster);

    let gone = view
        .play_to_client_typed::<Gone>(PayloadId("mvchat:gone".into()), crate::client_handle_gone);
    let _ = GONE.set(gone);

    let (up, down) = view.play_bidirectional_typed::<VoiceRelay>(
        PayloadId("mvchat:voice".into()),
        crate::server::handle_voice_relay,
        crate::client_handle_voice_relay,
    );
    let _ = VOICE_UP.set(up);
    let _ = VOICE_DOWN.set(down);
}
