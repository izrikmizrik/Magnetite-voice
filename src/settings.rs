//! The mod's page in Options > Mods (§7.3 of the ТЗ). Keys, translation keys and defaults.

use mod_api::{Registrar, SettingError, SettingKind, SettingSpec};

pub const KEY_ENABLED: &str = "enabled";
pub const KEY_SHOW_DOCK: &str = "show_dock";
pub const KEY_MODE: &str = "mode";
pub const KEY_VA_THRESHOLD: &str = "va_threshold";
pub const KEY_OUTPUT_VOLUME: &str = "output_volume";
pub const KEY_MIC_GAIN: &str = "mic_gain";
pub const KEY_MAX_DISTANCE: &str = "max_distance";
pub const KEY_SPATIAL: &str = "spatial";

/// `mode` choice indices.
pub const MODE_RADIO: u32 = 0;
pub const MODE_PTT: u32 = 1;
pub const MODE_VA: u32 = 2;

pub fn register(r: &mut dyn Registrar) -> Result<(), SettingError> {
    r.register_setting(SettingSpec::toggle(
        KEY_ENABLED,
        "mvchat.setting.enabled",
        true,
    ))?;
    r.register_setting(SettingSpec::toggle(
        KEY_SHOW_DOCK,
        "mvchat.setting.show_dock",
        true,
    ))?;
    r.register_setting(SettingSpec::choice(
        KEY_MODE,
        "mvchat.setting.mode",
        ["mvchat.mode.radio", "mvchat.mode.ptt", "mvchat.mode.va"],
        0,
    ))?;
    r.register_setting(SettingSpec::range(
        KEY_VA_THRESHOLD,
        "mvchat.setting.va_threshold",
        0.0,
        0.2,
        0.005,
        0.05,
    ))?;
    r.register_setting(SettingSpec::range(
        KEY_OUTPUT_VOLUME,
        "mvchat.setting.output_volume",
        0.0,
        200.0,
        5.0,
        100.0,
    ))?;
    r.register_setting(SettingSpec::range(
        KEY_MIC_GAIN,
        "mvchat.setting.mic_gain",
        0.0,
        200.0,
        5.0,
        100.0,
    ))?;
    r.register_setting(SettingSpec::range(
        KEY_MAX_DISTANCE,
        "mvchat.setting.max_distance",
        8.0,
        128.0,
        1.0,
        32.0,
    ))?;
    r.register_setting(SettingSpec::toggle(
        KEY_SPATIAL,
        "mvchat.setting.spatial",
        true,
    ))?;
    Ok(())
}

/// Typed reads with the spec defaults as fallback (the game's own reads already fall back;
/// these helpers keep one place that knows the defaults).
pub fn read_bool(host: &dyn mod_api::ClientHost, key: &str, default: bool) -> bool {
    host.setting_bool(key).unwrap_or(default)
}

pub fn read_choice(host: &dyn mod_api::ClientHost, key: &str, default: u32) -> u32 {
    host.setting_choice(key).unwrap_or(default)
}

pub fn read_f64(host: &dyn mod_api::ClientHost, key: &str, default: f64) -> f64 {
    host.setting_f64(key).unwrap_or(default)
}

/// The settings snapshot the engine tick consumes.
pub fn snapshot(host: &dyn mod_api::ClientHost) -> crate::engine::Settings {
    use crate::engine::MicMode;
    let mode = match read_choice(host, KEY_MODE, MODE_RADIO) {
        MODE_PTT => MicMode::Ptt,
        MODE_VA => MicMode::Va,
        _ => MicMode::Radio,
    };
    crate::engine::Settings {
        enabled: read_bool(host, KEY_ENABLED, true),
        mode,
        va_threshold: read_f64(host, KEY_VA_THRESHOLD, 0.05) as f32,
        output_volume: read_f64(host, KEY_OUTPUT_VOLUME, 100.0) as f32 / 100.0,
        mic_gain: read_f64(host, KEY_MIC_GAIN, 100.0) as f32 / 100.0,
        max_distance: read_f64(host, KEY_MAX_DISTANCE, 32.0) as f32,
        spatial: read_bool(host, KEY_SPATIAL, true),
    }
}

/// Persists a setting from the menu (a raw write; does not fire `setting_changed`).
pub fn write(host: &dyn mod_api::ClientHost, key: &str, value: &str) {
    host.set_setting(crate::MOD_ID, key, value);
}

/// Convenience for range settings the menu steps with +/- buttons.
pub fn write_f64(host: &dyn mod_api::ClientHost, key: &str, value: f64) {
    write(host, key, &format!("{value}"));
}

/// Keep the `SettingKind` import used by `register` even if the spec helpers change.
#[allow(unused)]
fn _kind_marker(kind: SettingKind) -> SettingKind {
    kind
}
