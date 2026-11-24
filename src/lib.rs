//! mvchat — proximity voice chat for Magnetite (Simple Voice Chat style).
//!
//! One native Rust mod, `side = "both"`: the server part hands out the group secret and the
//! roster and runs the UDP punch socket; the client part runs the voice engine (P2P UDP,
//! Opus, ChaCha20-Poly1305) and the Discord-like UI. Voice never passes through the game
//! server (except the wasm variant's relay, §9 of the ТЗ).
//!
//! Hooks (client): `client_level_load` starts and announces, `client_tick_pre` drives the
//! engine, `client_level_unload`/`client_stopping` stop it (+BYE), `render_gui_post` +
//! `mouse_button_pre` are the dock, the `screen_*` hooks are the menu.
//! Hooks (server): `server_about_to_start`/`server_stopping`/`server_tick` run the punch
//! socket, `player_logged_in`/`player_logged_out` maintain and broadcast the roster.

pub mod client_config;
pub mod engine;
pub mod payloads;
pub mod server;
pub mod settings;
pub mod ui;

use engine::protocol::Pose;
use engine::{EngineState, Shared, VoiceEngine};
use mod_api::{
    ClientHost, ClientLevelLoadEvent, ClientLevelUnloadEvent, ClientStoppingEvent,
    ClientTickPreEvent, Config, ConfigKind, ConfigLoadingEvent, Flow, Hooks, HostSide,
    IPayloadContext, InputKeyEvent, KeyAction, KeyConflictContext, KeyRegistration,
    MouseButtonPreEvent, PayloadSendError, PlayerLoggedInEvent, PlayerLoggedOutEvent,
    RegisterConfigsEvent, Registrar, RegistrationContext, RenderGuiPostEvent,
    ScreenCharTypedPreEvent, ScreenKeyPressedPreEvent, ScreenMouseButtonPressedPreEvent,
    ScreenMouseButtonReleasedPreEvent, ScreenMouseDraggedPreEvent, ScreenRenderPostEvent,
    ServerAboutToStartEvent, ServerHost, ServerStoppingEvent, ServerTickEvent,
};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};

/// The mod's id: manifest, screen, settings owner, key owner.
pub const MOD_ID: &str = "mvchat";

static SHARED: OnceLock<Arc<Mutex<Shared>>> = OnceLock::new();
static ENGINE: OnceLock<Mutex<Option<VoiceEngine>>> = OnceLock::new();

fn shared() -> Arc<Mutex<Shared>> {
    SHARED
        .get_or_init(|| Arc::new(Mutex::new(Shared::new())))
        .clone()
}

fn engine_slot() -> &'static Mutex<Option<VoiceEngine>> {
    ENGINE.get_or_init(|| Mutex::new(None))
}

pub struct MvChat {
    menu: ui::menu::Menu,
}

impl Default for MvChat {
    fn default() -> Self {
        Self {
            menu: ui::menu::Menu::new(shared()),
        }
    }
}

impl MvChat {
    fn stop_engine(&self) {
        if let Some(mut engine) = engine_slot().lock().unwrap().take() {
            engine.stop();
        }
        let shared = shared();
        shared.lock().unwrap().state = EngineState::Off;
    }

    fn toggle_menu(&self, host: &mut dyn ClientHost) {
        if host.mod_screen() == Some(MOD_ID) {
            host.mod_screen_close(MOD_ID);
        } else {
            host.mod_screen_open(MOD_ID);
        }
    }
}

/// Whether the joined server is a real multiplayer server (not the integrated one).
fn multiplayer(host: &dyn ClientHost) -> bool {
    host.client_session()
        .client_server()
        .map(|s| !s.integrated_local)
        .unwrap_or(false)
}

/// The punch socket's address: the `voice_server` client config override, else the joined
/// server's address with the roster's `udp_port`.
fn resolve_punch(host: &dyn ClientHost, udp_port: u16) -> Option<SocketAddr> {
    let cfg = client_config::get();
    if !cfg.voice_server.is_empty() {
        return engine::net::parse_server_address(&cfg.voice_server, udp_port);
    }
    let address = host.client_session().client_server()?.address?;
    let host_part = address.rsplit_once(':').map(|(h, _)| h).unwrap_or(&address);
    let host_part = host_part.trim_matches(['[', ']']);
    let ip: std::net::IpAddr = host_part.parse().ok()?;
    Some(SocketAddr::new(ip, udp_port))
}

/// `mvchat:roster` handler (client): store the secret and peers, point the engine at the
/// punch socket, start it if needed.
pub fn client_handle_roster(
    value: payloads::Roster,
    cx: &mut dyn IPayloadContext,
) -> Result<(), String> {
    let host = cx.client_host().map_err(|e| format!("{e:?}"))?;
    if !multiplayer(host) {
        shared().lock().unwrap().state = EngineState::Off;
        return Ok(());
    }
    let entries: Vec<(u128, String)> = value
        .entries
        .iter()
        .map(|e| (e.uuid.0, e.name.clone()))
        .collect();
    let you = value.you.0;
    let udp_port = value.udp_port;
    let shared = shared();
    engine::apply_roster(&shared, value.secret.0, you, &entries);
    let punch = resolve_punch(host, udp_port);
    {
        let mut s = shared.lock().unwrap();
        s.punch = punch;
        if s.state == EngineState::Waiting {
            s.state = EngineState::Connecting;
        }
    }
    let mut slot = engine_slot().lock().unwrap();
    if slot.is_none() {
        match VoiceEngine::start(shared.clone(), punch) {
            Ok(e) => *slot = Some(e),
            Err(e) => {
                shared.lock().unwrap().last_error = Some(e);
            }
        }
    }
    Ok(())
}

/// `mvchat:gone` handler (client): drop the peer.
pub fn client_handle_gone(
    value: payloads::Gone,
    _cx: &mut dyn IPayloadContext,
) -> Result<(), String> {
    engine::remove_peer(&shared(), value.who.0);
    Ok(())
}

/// `mvchat:voice` handler (client, wasm relay only): feed the packet to the engine's inbox.
pub fn client_handle_voice_relay(
    value: payloads::VoiceRelay,
    _cx: &mut dyn IPayloadContext,
) -> Result<(), String> {
    let shared = shared();
    let tx = shared.lock().unwrap().inbox_tx.clone();
    if let Some(tx) = tx {
        let dummy: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let _ = tx.send((dummy, value.packet));
    }
    Ok(())
}

#[mod_api::horizon_mod]
impl Hooks for MvChat {
    fn register(&self, r: &mut dyn Registrar, _cx: &RegistrationContext) {
        if let Err(e) = settings::register(r) {
            eprintln!("mvchat: setting refused: {e:?}");
        }
        r.register_key_mapping(
            KeyRegistration::new("open_menu", "key.keyboard.v")
                .label("key.mvchat.open_menu")
                .category("key.categories.misc"),
        );
        r.register_key_mapping(
            KeyRegistration::new("ptt", "key.keyboard.c")
                .label("key.mvchat.ptt")
                .category("key.categories.misc")
                .context(KeyConflictContext::InGame),
        );
        payloads::register(r);
    }

    fn register_configs(&self, _host: HostSide, event: &mut RegisterConfigsEvent<'_>) {
        event.configs.push(Config::new(
            MOD_ID,
            ConfigKind::Client,
            client_config::DEFAULT_CLIENT,
        ));
        event.configs.push(Config::new(
            MOD_ID,
            ConfigKind::Server,
            client_config::DEFAULT_SERVER,
        ));
    }

    fn config_loading(&self, _host: HostSide, event: &mut ConfigLoadingEvent<'_>) {
        if event.config.mod_id != MOD_ID {
            return;
        }
        match event.config.kind {
            ConfigKind::Client => client_config::load_client(&event.config.text),
            ConfigKind::Server => {
                let port = client_config::load_server(&event.config.text);
                server::server_state().lock().unwrap().punch_port = port;
            }
            _ => {}
        }
    }

    // ---- client lifecycle -------------------------------------------------

    fn client_level_load(&self, host: &mut dyn ClientHost, _event: &mut ClientLevelLoadEvent<'_>) {
        let shared = shared();
        {
            let mut s = shared.lock().unwrap();
            s.state = EngineState::Waiting;
            s.server_has_mod = None;
            s.peers.clear();
            s.secret = None;
            s.outbox.clear();
        }
        if !settings::read_bool(host, settings::KEY_ENABLED, true) {
            return;
        }
        if multiplayer(host) {
            let mut slot = engine_slot().lock().unwrap();
            if slot.is_none() {
                match VoiceEngine::start(shared.clone(), None) {
                    Ok(e) => *slot = Some(e),
                    Err(e) => shared.lock().unwrap().last_error = Some(e),
                }
            }
        }
        if let Some(join) = payloads::JOIN.get() {
            match join.send(
                host,
                &payloads::Join {
                    version: payloads::SIGNALING_VERSION,
                },
            ) {
                Ok(_) => shared.lock().unwrap().server_has_mod = Some(true),
                Err(PayloadSendError::UnsupportedPeer(_)) => {
                    // Honest "server without voice chat".
                    shared.lock().unwrap().server_has_mod = Some(false);
                }
                Err(e) => eprintln!("mvchat: join send: {e:?}"),
            }
        }
    }

    fn client_level_unload(
        &self,
        _host: &mut dyn ClientHost,
        _event: &mut ClientLevelUnloadEvent<'_>,
    ) {
        self.stop_engine();
    }

    fn client_stopping(&self, _host: &mut dyn ClientHost, _event: &mut ClientStoppingEvent) {
        self.stop_engine();
    }

    fn client_tick_pre(&self, host: &mut dyn ClientHost, _event: &mut ClientTickPreEvent) {
        if self.menu.take_close() {
            host.mod_screen_close(MOD_ID);
        }
        if host.consume_click(MOD_ID, "open_menu") && host.playing() {
            self.toggle_menu(host);
        }
        if !settings::read_bool(host, settings::KEY_ENABLED, true) {
            return;
        }
        let pose = host
            .camera()
            .map(|c| Pose {
                x: c.position[0] as f32,
                y: c.position[1] as f32,
                z: c.position[2] as f32,
                yaw: c.yaw,
            })
            .unwrap_or_default();
        let ptt_down = host.is_down(MOD_ID, "ptt");
        let settings = settings::snapshot(host);
        let mut slot = engine_slot().lock().unwrap();
        if let Some(engine) = slot.as_mut() {
            engine.tick(pose, ptt_down, &settings);
        }
    }

    fn input_key(&self, host: &mut dyn ClientHost, event: &mut InputKeyEvent<'_>) {
        if event.action == KeyAction::Press && event.key == "key.keyboard.v" && host.playing() {
            self.toggle_menu(host);
        }
    }

    // ---- client UI ----------------------------------------------------------

    fn render_gui_post(&self, host: &dyn ClientHost, event: &mut RenderGuiPostEvent<'_>) {
        if !settings::read_bool(host, settings::KEY_ENABLED, true) {
            return;
        }
        ui::hud::draw(host, event, &shared());
    }

    fn mouse_button_pre(&self, host: &mut dyn ClientHost, event: &mut MouseButtonPreEvent) -> Flow {
        if !settings::read_bool(host, settings::KEY_ENABLED, true) {
            return Flow::Continue;
        }
        ui::hud::click(host, event, &shared())
    }

    fn screen_render_post(&self, host: &dyn ClientHost, event: &mut ScreenRenderPostEvent<'_>) {
        self.menu.draw(host, event);
    }

    fn screen_mouse_button_pressed_pre(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenMouseButtonPressedPreEvent<'_>,
    ) -> Flow {
        self.menu.mouse_pressed(host, event)
    }

    fn screen_mouse_button_released_pre(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenMouseButtonReleasedPreEvent<'_>,
    ) -> Flow {
        self.menu.mouse_released(host, event)
    }

    fn screen_mouse_dragged_pre(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenMouseDraggedPreEvent<'_>,
    ) -> Flow {
        self.menu.mouse_dragged(host, event)
    }

    fn screen_key_pressed_pre(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenKeyPressedPreEvent<'_>,
    ) -> Flow {
        self.menu.key_pressed(host, event)
    }

    fn screen_char_typed_pre(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenCharTypedPreEvent<'_>,
    ) -> Flow {
        self.menu.char_typed(host, event)
    }

    // ---- server -------------------------------------------------------------

    fn server_about_to_start(
        &self,
        host: &mut dyn ServerHost,
        _event: &mut ServerAboutToStartEvent,
    ) {
        server::on_about_to_start(host);
    }

    fn server_stopping(&self, host: &mut dyn ServerHost, _event: &mut ServerStoppingEvent) {
        server::on_stopping(host);
    }

    fn server_tick(&self, host: &mut dyn ServerHost, _event: &mut ServerTickEvent) {
        server::on_tick(host);
    }

    fn player_logged_in(&self, host: &mut dyn ServerHost, event: &mut PlayerLoggedInEvent<'_>) {
        server::on_player_in(host, event.player, event.name);
    }

    fn player_logged_out(&self, host: &mut dyn ServerHost, event: &mut PlayerLoggedOutEvent<'_>) {
        server::on_player_out(host, event.player);
    }
}
