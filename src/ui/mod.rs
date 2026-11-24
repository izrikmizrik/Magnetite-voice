//! The Discord-like UI: the always-on dock (hud.rs) and the full-screen menu (menu.rs).

pub mod hud;
pub mod menu;

use mod_api::ClientHost;

/// Resolves a translation key through the client's language, falling back to the key itself.
pub fn tr(host: &dyn ClientHost, key: &str) -> String {
    host.translate(key).unwrap_or_else(|| key.to_owned())
}

/// ARGB colours of the dock's states.
pub mod colors {
    pub const BG: u32 = 0xC0_101014;
    pub const PANEL_EDGE: u32 = 0x40_000000;
    pub const ON: u32 = 0xFF_3BA55D; // green: talking / hearing
    pub const OFF: u32 = 0xFF_808080; // grey: muted
    pub const DEAF: u32 = 0xFF_ED4245; // red: deafened
    pub const NEUTRAL: u32 = 0xFF_B0B0B0;
    pub const TEXT: u32 = 0xFF_FFFFFF;
    pub const SPEAKING: u32 = 0xFF_3BA55D;
}
