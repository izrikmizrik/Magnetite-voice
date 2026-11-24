# mvchat — proximity voice chat for Magnetite

A native Rust mod (`mvchat.hmod`, `side = "both"`) for [Magnetite](https://github.com/izrikmizrik/Magnetite):
Simple Voice Chat style proximity voice, with a Discord-like tap menu instead of key holds.

- **Voice goes P2P over UDP** between clients. The game server is signaling only: it hands out
  the group secret and the roster and runs the hole-punching socket. Voice never passes through
  the game server in the native build (the wasm variant uses a relay payload, see §9 of the ТЗ).
- **Codec:** Opus 48 kHz mono, 20 ms frames, VoIP ~32 kbps, complexity 4, DTX on (`rusty-opus 1`).
- **Encryption:** ChaCha20-Poly1305 (`chacha20poly1305 0.10`), per-pair keys derived from the
  group secret (`sha2 0.10`). Voice packets carry an encrypted pose (position + yaw) for
  positional sound; control packets carry a truncated Poly1305 MAC.
- **Audio I/O:** `cpal 0.16` directly (the `pc-audio` feature; off only in the wasm build).
- **Dependencies:** only `rusty-opus`, `chacha20poly1305`, `sha2`, `cpal`, pinned to the exact
  versions in the game's `Cargo.lock`. No other crates.

## Layout

```text
horizon-mod.toml          manifest (id = "mvchat"); [native] pins are filled by cargo horizon-mod
Cargo.toml                deps + features (default = ["pc-audio"])
assets/mvchat/lang/       en_us.json, ru_ru.json
src/
├── lib.rs                hooks: register, configs, client lifecycle/tick/input, UI dispatch, server hooks
├── payloads.rs           typed payloads mvchat:join / roster / gone / voice (optional, PayloadCodec)
├── settings.rs           Options > Mods settings (register_setting)
├── client_config.rs      config/mvchat-client.toml (voice_server, devices), -server.toml (punch_port)
├── server.rs             roster, group secret, punch socket (polled in server_tick), relay forward
├── ui/hud.rs             the dock: 3 tap buttons (mic / headphones / ☰) + speaking indicators
├── ui/menu.rs            full-screen menu: Main / Mode / Settings tabs (widgets)
└── engine/
    ├── mod.rs            Shared state, peer table, VoiceEngine state machine, tick
    ├── protocol.rs       wire format: 28-byte header, PING/PONG/ASK/ECHO/VOICE/BYE
    ├── crypto.rs         pair keys, AEAD, MAC8, replay window, CSPRNG secret
    ├── codec.rs          Opus encoder/decoder wrapper (feature opus)
    ├── jitter.rs         jitter buffers, mixer, distance falloff + constant-power pan, resampler
    ├── net.rs            UDP socket, receive thread, pacer constants (native only)
    └── audio.rs          cpal capture/playback rings (feature pc-audio)
```

## Signaling (typed payloads, all `optional()`)

| channel | dir | type | purpose |
| --- | --- | --- | --- |
| `mvchat:join` | C→S | `{ version: u8 }` | "I have voice chat" |
| `mvchat:roster` | S→C | `{ secret: [u8;32], udp_port: u16, you: u128, entries: [{uuid, name}] }` | secret, punch port, own uuid, participants |
| `mvchat:gone` | S→C | `{ who: u128 }` | a participant left |
| `mvchat:voice` | both | `{ from: u128, counter: i64, packet: [u8] }` | wasm-variant relay only |

A client without the mod receives nothing; a send to a server without the mod returns
`UnsupportedPeer`, shown honestly as "server has no voice chat".

## Wire protocol (UDP, big-endian)

28-byte header: `b"MV"` ‖ version u8 (=1) ‖ type u8 ‖ sender uuid (16) ‖ counter u64.
Types: `PING=1 PONG=2 ASK=3 ECHO=4 VOICE=5 BYE=6`.

- `VOICE` body = ChaCha20-Poly1305 ciphertext (nonce = `0x00×4 ‖ counter_be`, header as AAD)
  over `[pose: x,y,z,yaw as f32 BE][Opus frame]`.
- `PING/PONG` between peers: body = 8-byte truncated Poly1305 MAC over the header under the
  pair key. `ASK`/`ECHO` to the punch socket use a punch key derived from the secret.
- Hole punching: the client sends `ASK` to the server's punch socket (default
  `server_port + 2`, i.e. 25567) every 1 s; the server answers `ECHO` with the sender's
  external address and one `ECHO` per other known participant (header sender = that peer's
  uuid, body = MAC ‖ that peer's last-seen external address; the socket keeps only each uuid's
  last-seen address+time, for cleanup). The client then `PING`s each candidate every 500 ms;
  the first valid `PONG`/`VOICE` opens the path. `BYE` on clean exit.
- Anti-replay: sliding 64-packet window per peer.

## Engine (client)

State machine: `Off → Waiting → Connecting → Ready → (back to Connecting after 5 s of silence
per peer) → Off`. Exactly two mod-spawned threads on native: the UDP receive thread and the
pacer (20 ms send quantum, PING/ASK scheduling). Everything else runs in `client_tick_pre`:
capture → resample to 48 kHz → 20 ms frames → gate (radio tap-toggle / PTT key / VA RMS) →
Opus encode → seal → outbox; inbox → verify → jitter buffers → mix (distance smoothstep +
constant-power pan) → playback ring. The cpal callbacks only move samples into/out of rings.
Deafen mutes mic and sound together (like Discord) and sends no BYE — we just go quiet.

## UI

- **Dock** (`render_gui_post` + `mouse_button_pre`): three 48 px buttons — mic (green = open,
  grey+slash = muted), headphones (green = hearing, red+slash = deafened), ☰ (menu) — plus
  "who is speaking" indicators. Hidden by the `show_dock` setting.
- **Menu** (key `V`, the `open_menu` mapping, or ☰): a mod screen with tabs Main (status,
  players with per-player mute/volume, the dock's buttons), Mode (radio/PTT/VA + threshold),
  Settings (volumes, mic gain, distance, 3D, voice server address, device lists).
- Settings also live in Options → Mods (`register_setting`). Keys are rebindable in Controls.

## Build

```sh
cargo horizon-mod new . --template native   # once: pins [native] to the current build, fetches .horizon/sdk
horizon-launcher native-lock .              # pins the registry deps (rusty-opus, chacha20poly1305, sha2, cpal) in Cargo.lock
horizon-launcher native-build . --instance "My Instance" --trust-native-code
cargo horizon-mod build --wasm --no-default-features   # the wasm variant (relay transport, stub audio)
```

Local checks (this repo, without the launcher):

```sh
cargo test --no-default-features            # pure modules: protocol, crypto, jitter, net
cargo test --no-default-features --features opus   # + codec round-trip / PLC
cargo clippy --no-default-features --features opus --all-targets
cargo check                                 # full crate against the SDK in .horizon/sdk (fetched by cargo horizon-mod)
cargo check --features pc-audio             # + the cpal audio backend (needs ALSA dev headers on Linux)
```

The `.horizon/` SDK folder is fetched by `cargo horizon-mod` and is gitignored.

## Notes / decisions

- **Punch address distribution:** the fixed wire format has no field for peer addresses in the
  roster, and the punch socket only keeps last-seen addresses — so `ECHO` is what carries them:
  on a valid `ASK` the server sends the requester one `ECHO` about itself and one per known
  peer (peer uuid in the header, peer's external address in the body). This uses only the
  packet types of the ТЗ and is what makes symmetric-NAT punching work.
- **Server punch port:** read from `config/mvchat-server.toml` (`punch_port`, default 25567 =
  25565+2); the mod API does not expose the bound port.
- **Voice server address on the client:** `config/mvchat-client.toml` `voice_server`, else the
  joined server's address (`ClientSession::client_server`) with the roster's `udp_port`.
- **CSPRNG without extra crates:** `/dev/urandom` on unix, `BCryptGenRandom` on Windows; the
  wasm sandbox has no entropy source, so there the secret is a SHA-256 KDF over time/addresses
  (acceptable only because the wasm variant is a relay skeleton).
- **Cargo features:** `default = ["pc-audio"]`; the prebuilt (phone) native route refuses mods
  that declare features, which is exactly why phones use the wasm variant.
- **Evolution:** the protocol version (u8 = 1) is in every packet header from day one.

## Milestones (ТЗ §10)

M0 scaffold/signaling, M1 crypto+wire+punch, M2 audio pipeline, M3 UI/stability are implemented
in this tree; M4 (publish to magnetite-mods `index.json`), M5 (wasm build) and M6 (Android
RECORD_AUDIO) are platform follow-ups.
