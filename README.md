# mvchat — proximity voice chat for Magnetite

A **WebAssembly mod** (`mvchat.hmod` with `mod.wasm`, `side = "both"`, no `[native]` table) for
[Magnetite](https://github.com/izrikmizrik/Magnetite), so it runs on PC, Android and iOS from
one build. Voice chat in the style of Simple Voice Chat, with a Discord-like tap menu instead
of key holds.

- **Transport (wasm):** the sandbox has no sockets or threads, so voice travels as encrypted
  `mvchat:voice` payloads C→S→C through the game server (`{ from, counter, packet }`, where
  `packet` is a whole wire packet of §wire). The server only forwards; it never sees plain
  voice. Encryption is unchanged (ChaCha20-Poly1305, per-pair keys from the group secret), so
  the relay learns nothing. The same code still builds as a native mod with direct P2P UDP
  (`cargo check --features pc-audio` / the `pc-audio` feature) for PC, where punch PING/ASK/
  ECHO replace the relay.
- **Microphone:** mod-API has no microphone hook yet. When the platform adds it (with its
  permission — `NSMicrophoneUsageDescription` is already in the macOS launcher's Info.plist,
  Android's `RECORD_AUDIO` comes with it), the hook's frames plug into the marked spot in
  `src/engine/audio.rs` (`TODO(mic-hook)`); until then the wasm audio backend is `MockAudio`
  and the mod runs as the full signaling/roster/UI/relay skeleton with silent audio.
- **Codec:** Opus 48 kHz mono, 20 ms frames, VoIP ~32 kbps, complexity 4, DTX on
  (`rusty-opus 1`, pure Rust, so it builds to wasm).
- **Dependencies:** only `rusty-opus`, `chacha20poly1305`, `sha2` (all pure Rust, wasm-safe),
  pinned to the exact versions in the game's `Cargo.lock`; `cpal` is a native-only optional
  dependency that never enters the wasm graph. No other crates.

## Layout

```text
horizon-mod.toml          manifest (id = "mvchat"); the [wasm] section is written by the build tool
Cargo.toml                [package.metadata.horizon] sdk pin + deps + features (default = ["opus"])
assets/mvchat/lang/       en_us.json, ru_ru.json
src/
├── lib.rs                hooks: register, configs, client lifecycle/tick/input, UI dispatch, server hooks;
│                         on wasm the tick drains the relay outbox into mvchat:voice payloads
├── payloads.rs           typed payloads mvchat:join / roster / gone / voice (all optional, PayloadCodec)
├── settings.rs           Options > Mods settings (register_setting)
├── client_config.rs      config/mvchat-client.toml (voice_server, devices), -server.toml (punch_port)
├── server.rs             roster, group secret, relay forward; punch socket on native only
├── ui/hud.rs             the dock: 3 tap buttons (mic / headphones / ☰) + speaking indicators
├── ui/menu.rs            full-screen menu: Main / Mode / Settings tabs (widgets)
└── engine/
    ├── mod.rs            Shared state, peer table, VoiceEngine state machine, tick (no threads on wasm)
    ├── protocol.rs       wire format: 28-byte header, PING/PONG/ASK/ECHO/VOICE/BYE
    ├── crypto.rs         pair keys, AEAD, MAC8, replay window, CSPRNG secret
    ├── codec.rs          Opus encoder/decoder wrapper (feature opus)
    ├── jitter.rs         jitter buffers, mixer, distance falloff + constant-power pan, resampler
    ├── net.rs            UDP socket + receive thread + pacer (native only, cfg-gated)
    └── audio.rs          AudioBackend trait: MockAudio (wasm) / CpalAudio (native pc-audio)
```

## Signaling (typed payloads, all `optional()`)

| channel | dir | type | purpose |
| --- | --- | --- | --- |
| `mvchat:join` | C→S | `{ version: u8 }` | "I have voice chat" |
| `mvchat:roster` | S→C | `{ secret: [u8;32], udp_port: u16, you: u128, entries: [{uuid, name}] }` | secret, punch port, own uuid, participants |
| `mvchat:gone` | S→C | `{ who: u128 }` | a participant left |
| `mvchat:voice` | both | `{ from: u128, counter: i64, packet: [u8] }` | the relayed (encrypted) wire packet |

A client without the mod receives nothing; a send to a server without the mod returns
`UnsupportedPeer`, shown honestly as "server has no voice chat".

## Wire format (inside `mvchat:voice` on wasm; raw UDP datagrams on native)

28-byte header: `b"MV"` ‖ version u8 (=1) ‖ type u8 ‖ sender uuid (16) ‖ counter u64.
Types: `PING=1 PONG=2 ASK=3 ECHO=4 VOICE=5 BYE=6`. All numbers big-endian.

- `VOICE` body = ChaCha20-Poly1305 ciphertext (nonce = `0x00×4 ‖ counter_be`, header as AAD)
  over `[pose: x,y,z,yaw as f32 BE][Opus frame]` — the pose is authentic and gives positional
  sound even through the relay.
- `PING/PONG` (native) = 8-byte truncated Poly1305 MAC over the header under the pair key;
  `ASK`/`ECHO` to the punch socket use a punch key derived from the secret. On a valid `ASK`
  the server answers `ECHO` with the sender's external address plus one `ECHO` per other known
  participant (peer uuid in the header, peer's last-seen external address in the body) — that
  is how peer addresses ride the fixed packet types. The client then `PING`s each candidate
  every 500 ms; the first valid `PONG`/`VOICE` opens the path. `BYE` on clean exit.
- Anti-replay: sliding 64-packet window per peer. Protocol version (u8 = 1) is in every header
  from day one.

## Engine (client)

State machine: `Off → Waiting → Connecting → Ready → (back to Connecting after 5 s of silence
per peer) → Off`. On native exactly two mod-spawned threads exist (UDP receiver + pacer); on
wasm **no threads at all** — the relay outbox is drained and sent in `client_tick_pre`, and
the inbox is fed by the `mvchat:voice` handler. Everything else runs in the tick: inbox →
verify (MAC/AEAD/replay) → jitter buffers → mix (distance smoothstep + constant-power pan) →
playback; capture → resample to 48 kHz → 20 ms frames → gate (radio tap-toggle / PTT key /
VA RMS) → Opus → seal → outbox/relay. Deafen mutes mic and sound together (like Discord) and
sends no BYE — we just go quiet.

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
cargo horizon-mod new . --template wasm   # once: writes the sdk pin, fetches .horizon/sdk
cargo horizon-mod build --wasm            # builds mod.wasm with the pinned toolchain, writes [wasm], packs mvchat.hmod
cargo horizon-mod update                  # move the sdk pin to a newer build
```

The `.horizon/` SDK folder is fetched by `cargo horizon-mod` and is gitignored. A mod carries
wasm code only — no `[native]` table; the native P2P path stays in the source behind
`--features pc-audio` for a future PC build.

Local checks (this repo):

```sh
cargo test                                  # 26 unit tests: protocol, crypto, jitter, net, codec
cargo clippy --all-targets                  # clean
cargo check                                 # full crate against the SDK in .horizon/sdk
cargo check --target wasm32-unknown-unknown # the wasm build path (default features)
cargo check --features pc-audio             # native cpal backend (needs ALSA dev headers on Linux)
```

## Notes / decisions

- **Permission:** there is no microphone capability in the manifest yet (unknown fields are
  refused), so nothing is declared today. When the platform adds the mic hook, its permission
  (capability or launcher prompt) is declared in `horizon-mod.toml` then; the launcher's
  macOS `NSMicrophoneUsageDescription` ("Native mods can use your microphone for voice chat")
  is already in place.
- **CSPRNG without extra crates:** `/dev/urandom` on unix, `BCryptGenRandom` on Windows; on
  wasm (no entropy source in the sandbox) the secret is a SHA-256 KDF over time/addresses —
  acceptable because the relayed ciphertext is still AEAD-protected and the wasm variant's
  secret is only a fallback until the server side runs natively.
- **Server punch port:** `config/mvchat-server.toml` `punch_port`, default 25567 (25565+2);
  native only, unused on wasm.
- **Voice server address (native):** `config/mvchat-client.toml` `voice_server`, else the
  joined server's address (`ClientSession::client_server`) with the roster's `udp_port`.

## Milestones (ТЗ §10)

M0–M3 implemented; **M5 (wasm build) is this tree** — `cargo horizon-mod build --wasm` packs the
`.hmod`; signaling, roster, UI, relay and state machine all run in the sandbox.
M4 (publish to magnetite-mods `index.json`, `kind: "mod"`, `side: "both"`) and M6 (Android
`RECORD_AUDIO` + the microphone hook, then real voice on phones) are the remaining steps.
