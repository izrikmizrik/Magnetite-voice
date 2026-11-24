//! The mod's own config files (registered in `register_configs`, parsed in `config_loading`):
//!
//! - `config/mvchat-client.toml` (client): `voice_server`, `input_device`, `output_device`.
//! - `config/mvchat-server.toml` (server, per world): `punch_port`.
//!
//! Plain `key = "value"` lines, parsed by hand (no extra crates).

use mod_api::{ClientHost, ConfigPath};
use std::sync::{Mutex, OnceLock};

pub const CLIENT_FILE: &str = "mvchat-client.toml";
pub const SERVER_FILE: &str = "mvchat-server.toml";

pub const DEFAULT_CLIENT: &str =
    "# mvchat client config\nvoice_server = \"\"\ninput_device = \"\"\noutput_device = \"\"\n";
pub const DEFAULT_SERVER: &str = "# mvchat server config\n# UDP port of the punch socket; default 25567 = game port 25565 + 2\npunch_port = 25567\n";

#[derive(Clone, Debug, Default)]
pub struct ClientConfig {
    pub voice_server: String,
    pub input_device: String,
    pub output_device: String,
}

static CLIENT: OnceLock<Mutex<ClientConfig>> = OnceLock::new();

fn client() -> &'static Mutex<ClientConfig> {
    CLIENT.get_or_init(|| Mutex::new(ClientConfig::default()))
}

/// Parses a config file's text (`key = "value"` lines; `#` comments).
pub fn load_client(text: &str) {
    let mut cfg = ClientConfig::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match key.trim() {
            "voice_server" => cfg.voice_server = value.to_owned(),
            "input_device" => cfg.input_device = value.to_owned(),
            "output_device" => cfg.output_device = value.to_owned(),
            _ => {}
        }
    }
    *client().lock().unwrap() = cfg;
}

/// The punch port from the server config (0 = default).
pub fn load_server(text: &str) -> u16 {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim() == "punch_port"
            && let Ok(port) = value.trim().parse::<u16>()
        {
            return port;
        }
    }
    crate::server::DEFAULT_PUNCH_PORT
}

pub fn get() -> ClientConfig {
    client().lock().unwrap().clone()
}

/// Sets `voice_server` and rewrites the client config file (best effort).
pub fn set_voice_server(host: &dyn ClientHost, value: &str) {
    {
        let mut cfg = client().lock().unwrap();
        cfg.voice_server = value.to_owned();
    }
    let cfg = client().lock().unwrap().clone();
    let text = format!(
        "# mvchat client config\nvoice_server = \"{}\"\ninput_device = \"{}\"\noutput_device = \"{}\"\n",
        cfg.voice_server, cfg.input_device, cfg.output_device
    );
    if let Ok(path) = ConfigPath::try_from(CLIENT_FILE.to_owned()) {
        let _ = host.config_files().write(&path, text.as_bytes());
    }
}
