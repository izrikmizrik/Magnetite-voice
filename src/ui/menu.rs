//! The full-screen menu (key `V` or the dock's ☰ button), a mod screen with three tabs:
//! Main (status, players, per-player mute/volume, the dock's buttons), Mode (radio/PTT/VA),
//! Settings (volumes, distance, spatial, voice server, devices). All tap targets >= 44 px.

use crate::engine::Shared;
use crate::ui::{colors, tr};
use mod_api::client::widgets::{Button, Outcome, TextField, WidgetId, Widgets};
use mod_api::{
    ClientHost, Flow, ScreenCharTypedPreEvent, ScreenKeyPressedPreEvent,
    ScreenMouseButtonPressedPreEvent, ScreenMouseButtonReleasedPreEvent,
    ScreenMouseDraggedPreEvent, ScreenRenderPostEvent, Text,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Main = 0,
    Mode = 1,
    Settings = 2,
}

const BTN_H: f32 = 44.0;
const ROW_H: f32 = 48.0;

struct Ids {
    tabs: [WidgetId; 3],
    mic: WidgetId,
    deaf: WidgetId,
    close: WidgetId,
    peer_uuids: Vec<u128>,
    peer_mute: Vec<WidgetId>,
    peer_vol_down: Vec<WidgetId>,
    peer_vol_up: Vec<WidgetId>,
    modes: [WidgetId; 3],
    va_down: WidgetId,
    va_up: WidgetId,
    out_down: WidgetId,
    out_up: WidgetId,
    gain_down: WidgetId,
    gain_up: WidgetId,
    dist_down: WidgetId,
    dist_up: WidgetId,
    spatial: WidgetId,
    voice_server: WidgetId,
}

struct Screen {
    widgets: Widgets,
    tab: Tab,
    built_key: Option<(Tab, usize, u32)>,
    ids: Ids,
}

/// The menu mod-screen state: the open screen's widgets, and a close request taken in the
/// next client tick (input hooks only read the host).
pub struct Menu {
    screen: Mutex<Option<Screen>>,
    close: AtomicBool,
    shared: Arc<Mutex<Shared>>,
}

impl Menu {
    pub fn new(shared: Arc<Mutex<Shared>>) -> Self {
        Self {
            screen: Mutex::new(None),
            close: AtomicBool::new(false),
            shared,
        }
    }

    /// Takes the close request (called from `client_tick_pre`).
    pub fn take_close(&self) -> bool {
        self.close.swap(false, Ordering::Relaxed)
    }

    fn panel(event_width: f32, event_height: f32) -> [f32; 4] {
        let w = (420.0f32).min(event_width - 40.0).max(280.0);
        let h = (400.0f32).min(event_height - 40.0).max(300.0);
        [(event_width - w) / 2.0, (event_height - h) / 2.0, w, h]
    }

    /// Builds (or rebuilds) the widgets for the current tab and roster.
    fn ensure_built(&self, width: f32, height: f32) -> std::sync::MutexGuard<'_, Option<Screen>> {
        let mut guard = self.screen.lock().unwrap();
        let peer_count = self.shared.lock().unwrap().peers.len();
        let tab = guard.as_ref().map(|s| s.tab).unwrap_or(Tab::Main);
        let key = (tab, peer_count, width.to_bits());
        if guard.as_ref().and_then(|s| s.built_key) == Some(key) {
            return guard;
        }
        let [px, py, pw, ph] = Self::panel(width, height);
        let mut widgets = Widgets::new();
        let tab_w = (pw - 16.0) / 3.0;
        // WidgetId has no public constructor; a placeholder button provides the initial ids,
        // replaced below as the real widgets are added.
        let placeholder = widgets.add_button(Button::new([0.0, 0.0, 1.0, 1.0], Text::literal("")));
        let mut tabs = [placeholder; 3];
        for (i, key) in ["mvchat.tab.main", "mvchat.tab.mode", "mvchat.tab.settings"]
            .iter()
            .enumerate()
        {
            tabs[i] = widgets.add_button(Button::new(
                [px + 8.0 + i as f32 * tab_w, py + 8.0, tab_w - 4.0, BTN_H],
                Text::translate(*key, vec![]),
            ));
        }
        let mut ids = Ids {
            tabs,
            mic: placeholder,
            deaf: placeholder,
            close: placeholder,
            peer_uuids: Vec::new(),
            peer_mute: Vec::new(),
            peer_vol_down: Vec::new(),
            peer_vol_up: Vec::new(),
            modes: [placeholder; 3],
            va_down: placeholder,
            va_up: placeholder,
            out_down: placeholder,
            out_up: placeholder,
            gain_down: placeholder,
            gain_up: placeholder,
            dist_down: placeholder,
            dist_up: placeholder,
            spatial: placeholder,
            voice_server: placeholder,
        };
        let bottom_y = py + ph - 8.0 - BTN_H;
        let bw = (pw - 16.0 - 16.0) / 3.0;
        ids.mic = widgets.add_button(Button::new(
            [px + 8.0, bottom_y, bw, BTN_H],
            Text::translate("mvchat.btn.mic", vec![]),
        ));
        ids.deaf = widgets.add_button(Button::new(
            [px + 12.0 + bw, bottom_y, bw, BTN_H],
            Text::translate("mvchat.btn.deafen", vec![]),
        ));
        ids.close = widgets.add_button(Button::new(
            [px + 16.0 + 2.0 * bw, bottom_y, bw, BTN_H],
            Text::translate("mvchat.btn.close", vec![]),
        ));

        let content_top = py + 8.0 + BTN_H + 8.0;
        match tab {
            Tab::Main => {
                let peers: Vec<u128> = self.shared.lock().unwrap().peers.keys().copied().collect();
                let max_rows =
                    (((bottom_y - 40.0) - content_top) / ROW_H).floor().max(1.0) as usize;
                for (row, uuid) in peers.iter().take(max_rows).enumerate() {
                    let ry = content_top + 36.0 + row as f32 * ROW_H;
                    let bx = px + pw - 8.0 - 3.0 * 46.0;
                    ids.peer_mute.push(widgets.add_button(Button::new(
                        [bx, ry + 2.0, 44.0, 40.0],
                        Text::translate("mvchat.btn.mute", vec![]),
                    )));
                    ids.peer_vol_down.push(widgets.add_button(Button::new(
                        [bx + 46.0, ry + 2.0, 44.0, 40.0],
                        Text::literal("-"),
                    )));
                    ids.peer_vol_up.push(widgets.add_button(Button::new(
                        [bx + 92.0, ry + 2.0, 44.0, 40.0],
                        Text::literal("+"),
                    )));
                    ids.peer_uuids.push(*uuid);
                }
            }
            Tab::Mode => {
                for (i, key) in ["mvchat.mode.radio", "mvchat.mode.ptt", "mvchat.mode.va"]
                    .iter()
                    .enumerate()
                {
                    let ry = content_top + 8.0 + i as f32 * (BTN_H + 8.0);
                    ids.modes[i] = widgets.add_button(Button::new(
                        [px + 8.0, ry, pw - 16.0, BTN_H],
                        Text::translate(*key, vec![]),
                    ));
                }
                let ry = content_top + 8.0 + 3.0 * (BTN_H + 8.0) + 4.0;
                ids.va_down = widgets
                    .add_button(Button::new([px + 8.0, ry, 44.0, BTN_H], Text::literal("-")));
                ids.va_up = widgets.add_button(Button::new(
                    [px + pw - 8.0 - 44.0, ry, 44.0, BTN_H],
                    Text::literal("+"),
                ));
            }
            Tab::Settings => {
                let rows = [
                    (
                        "mvchat.setting.output_volume",
                        crate::settings::KEY_OUTPUT_VOLUME,
                    ),
                    ("mvchat.setting.mic_gain", crate::settings::KEY_MIC_GAIN),
                    (
                        "mvchat.setting.max_distance",
                        crate::settings::KEY_MAX_DISTANCE,
                    ),
                ];
                let mut ry = content_top + 8.0;
                for (label, key) in rows {
                    let _ = label;
                    let _ = key;
                    let down = widgets.add_button(Button::new(
                        [px + pw - 8.0 - 92.0, ry, 44.0, 40.0],
                        Text::literal("-"),
                    ));
                    let up = widgets.add_button(Button::new(
                        [px + pw - 8.0 - 44.0, ry, 44.0, 40.0],
                        Text::literal("+"),
                    ));
                    match key {
                        crate::settings::KEY_OUTPUT_VOLUME => {
                            ids.out_down = down;
                            ids.out_up = up;
                        }
                        crate::settings::KEY_MIC_GAIN => {
                            ids.gain_down = down;
                            ids.gain_up = up;
                        }
                        _ => {
                            ids.dist_down = down;
                            ids.dist_up = up;
                        }
                    }
                    ry += ROW_H;
                }
                ids.spatial = widgets.add_button(Button::new(
                    [px + 8.0, ry, pw - 16.0, BTN_H],
                    Text::translate("mvchat.setting.spatial", vec![]),
                ));
                ry += BTN_H + 8.0;
                ids.voice_server =
                    widgets.add_text_field(TextField::new([px + 8.0, ry, pw - 16.0, 32.0], 128));
            }
        }
        *guard = Some(Screen {
            widgets,
            tab,
            built_key: Some(key),
            ids,
        });
        guard
    }

    /// Draws the menu in `screen_render_post`.
    pub fn draw(&self, host: &dyn ClientHost, event: &mut ScreenRenderPostEvent<'_>) {
        if host.mod_screen() != Some(crate::MOD_ID) {
            return;
        }
        let [width, height] = event.draw.size();
        let mut guard = self.ensure_built(width, height);
        let Some(screen) = guard.as_mut() else { return };
        let [px, py, pw, ph] = Self::panel(width, height);
        let draw = &mut *event.draw;
        draw.fill([0.0, 0.0, width, height], 0xA0_000000);
        draw.fill([px, py, pw, ph], 0xF0_1E1E24);
        draw.fill([px, py, pw, 1.0], colors::PANEL_EDGE);
        let _ = screen.widgets.draw(&mut *draw, host, event.screen.mouse);

        // Title + status.
        let title = tr(host, "mvchat.menu.title");
        draw.text(
            &title,
            [px + 8.0, py + 8.0 + BTN_H + 8.0],
            colors::TEXT,
            false,
        );
        let status = crate::ui::hud::status_line(host, &self.shared);
        draw.text(
            &status,
            [px + 8.0, py + 8.0 + BTN_H + 26.0],
            colors::NEUTRAL,
            false,
        );

        let content_top = py + 8.0 + BTN_H + 8.0 + 36.0 + 16.0;
        match screen.tab {
            Tab::Main => {
                let shared = self.shared.lock().unwrap();
                let now = std::time::Instant::now();
                for (row, uuid) in screen.ids.peer_uuids.iter().enumerate() {
                    let Some(peer) = shared.peers.get(uuid) else {
                        continue;
                    };
                    let ry = content_top + row as f32 * ROW_H;
                    let speaking = peer.speaking_until > now;
                    if speaking {
                        draw.fill([px + 8.0, ry + 16.0, 12.0, 12.0], colors::SPEAKING);
                    }
                    let name = if peer.muted {
                        format!("{} ({})", peer.name, tr(host, "mvchat.btn.muted"))
                    } else {
                        peer.name.clone()
                    };
                    draw.text(&name, [px + 28.0, ry + 14.0], colors::TEXT, false);
                    let vol = format!("{}%", (peer.volume * 100.0).round() as i32);
                    draw.text(
                        &vol,
                        [px + pw - 8.0 - 3.0 * 46.0 - 60.0, ry + 14.0],
                        colors::NEUTRAL,
                        false,
                    );
                }
            }
            Tab::Mode => {
                let ry = content_top + 3.0 * (BTN_H + 8.0) + 4.0 + 8.0;
                let label = tr(host, "mvchat.setting.va_threshold");
                draw.text(&label, [px + 8.0 + 52.0, ry + 12.0], colors::TEXT, false);
                let va = crate::settings::read_f64(host, crate::settings::KEY_VA_THRESHOLD, 0.05);
                draw.text(
                    &format!("{va:.3}"),
                    [px + pw / 2.0 - 20.0, ry + 12.0],
                    colors::NEUTRAL,
                    false,
                );
            }
            Tab::Settings => {
                let rows = [
                    (
                        tr(host, "mvchat.setting.output_volume"),
                        crate::settings::read_f64(host, crate::settings::KEY_OUTPUT_VOLUME, 100.0),
                    ),
                    (
                        tr(host, "mvchat.setting.mic_gain"),
                        crate::settings::read_f64(host, crate::settings::KEY_MIC_GAIN, 100.0),
                    ),
                    (
                        tr(host, "mvchat.setting.max_distance"),
                        crate::settings::read_f64(host, crate::settings::KEY_MAX_DISTANCE, 32.0),
                    ),
                ];
                for (i, (label, value)) in rows.iter().enumerate() {
                    let ry = content_top + 8.0 + i as f32 * ROW_H;
                    draw.text(label, [px + 8.0, ry + 12.0], colors::TEXT, false);
                    draw.text(
                        &format!("{value:.0}"),
                        [px + pw - 8.0 - 3.0 * 46.0 - 40.0, ry + 12.0],
                        colors::NEUTRAL,
                        false,
                    );
                }
                let ry = content_top + 8.0 + 3.0 * ROW_H;
                let ry2 = ry + BTN_H + 8.0;
                let label = tr(host, "mvchat.setting.voice_server");
                draw.text(&label, [px + 8.0, ry2 - 4.0], colors::TEXT, false);
            }
        }
    }

    /// Runs `input` on the widgets and acts on the outcome (the widget_screen.rs pattern).
    fn input(&self, host: &dyn ClientHost, input: impl FnOnce(&mut Widgets) -> Outcome) -> Flow {
        if host.mod_screen() != Some(crate::MOD_ID) {
            return Flow::Continue;
        }
        let mut guard = self.screen.lock().unwrap();
        let Some(screen) = guard.as_mut() else {
            return Flow::Continue;
        };
        let outcome = input(&mut screen.widgets);
        if let Outcome::Pressed(id) = outcome {
            self.on_pressed(host, screen, id);
        } else if let Outcome::Changed(id) | Outcome::Submitted(id) = outcome
            && id == screen.ids.voice_server
            && let Some(f) = screen.widgets.text_field(screen.ids.voice_server)
        {
            crate::client_config::set_voice_server(host, f.value());
        }
        if outcome.handled() {
            Flow::Cancel
        } else {
            Flow::Continue
        }
    }

    fn on_pressed(&self, host: &dyn ClientHost, screen: &mut Screen, id: WidgetId) {
        let ids = &screen.ids;
        if ids.tabs.contains(&id) {
            let i = ids.tabs.iter().position(|t| *t == id).unwrap();
            screen.tab = match i {
                1 => Tab::Mode,
                2 => Tab::Settings,
                _ => Tab::Main,
            };
            screen.built_key = None; // rebuild next frame
            return;
        }
        if id == ids.close {
            self.close.store(true, Ordering::Relaxed);
            return;
        }
        if id == ids.mic {
            let mut s = self.shared.lock().unwrap();
            if !s.muted && !s.deafened {
                s.mic_open = !s.mic_open;
            }
            return;
        }
        if id == ids.deaf {
            let mut s = self.shared.lock().unwrap();
            s.deafened = !s.deafened;
            if s.deafened {
                s.mic_open = false;
            }
            return;
        }
        if let Some(i) = ids.peer_mute.iter().position(|w| *w == id) {
            if let Some(uuid) = screen.ids.peer_uuids.get(i) {
                let mut s = self.shared.lock().unwrap();
                if let Some(p) = s.peers.get_mut(uuid) {
                    p.muted = !p.muted;
                }
            }
            return;
        }
        if let Some(i) = ids.peer_vol_down.iter().position(|w| *w == id) {
            self.bump_peer_volume(screen, i, -0.25);
            return;
        }
        if let Some(i) = ids.peer_vol_up.iter().position(|w| *w == id) {
            self.bump_peer_volume(screen, i, 0.25);
            return;
        }
        if ids.modes.contains(&id) {
            let i = ids.modes.iter().position(|m| *m == id).unwrap();
            crate::settings::write(host, crate::settings::KEY_MODE, &format!("{i}"));
            return;
        }
        if id == ids.va_down || id == ids.va_up {
            let cur = crate::settings::read_f64(host, crate::settings::KEY_VA_THRESHOLD, 0.05);
            let next = (cur + if id == ids.va_up { 0.005 } else { -0.005 }).clamp(0.0, 0.2);
            crate::settings::write_f64(host, crate::settings::KEY_VA_THRESHOLD, next);
            return;
        }
        let step = |key: &str, d: f64, min: f64, max: f64| {
            let cur = crate::settings::read_f64(host, key, min);
            crate::settings::write_f64(host, key, (cur + d).clamp(min, max));
        };
        if id == ids.out_down {
            step(crate::settings::KEY_OUTPUT_VOLUME, -5.0, 0.0, 200.0);
            return;
        }
        if id == ids.out_up {
            step(crate::settings::KEY_OUTPUT_VOLUME, 5.0, 0.0, 200.0);
            return;
        }
        if id == ids.gain_down {
            step(crate::settings::KEY_MIC_GAIN, -5.0, 0.0, 200.0);
            return;
        }
        if id == ids.gain_up {
            step(crate::settings::KEY_MIC_GAIN, 5.0, 0.0, 200.0);
            return;
        }
        if id == ids.dist_down {
            step(crate::settings::KEY_MAX_DISTANCE, -1.0, 8.0, 128.0);
            return;
        }
        if id == ids.dist_up {
            step(crate::settings::KEY_MAX_DISTANCE, 1.0, 8.0, 128.0);
            return;
        }
        if id == ids.spatial {
            let cur = crate::settings::read_bool(host, crate::settings::KEY_SPATIAL, true);
            crate::settings::write(
                host,
                crate::settings::KEY_SPATIAL,
                if cur { "false" } else { "true" },
            );
        }
    }

    fn bump_peer_volume(&self, screen: &Screen, i: usize, d: f32) {
        if let Some(uuid) = screen.ids.peer_uuids.get(i) {
            let mut s = self.shared.lock().unwrap();
            if let Some(p) = s.peers.get_mut(uuid) {
                p.volume = (p.volume + d).clamp(0.0, 2.0);
            }
        }
    }

    pub fn mouse_pressed(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenMouseButtonPressedPreEvent<'_>,
    ) -> Flow {
        let (at, button, modifiers) = (event.screen.mouse, event.button, event.modifiers);
        self.input(host, |w| w.mouse_pressed(host, at, button, modifiers))
    }

    pub fn mouse_released(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenMouseButtonReleasedPreEvent<'_>,
    ) -> Flow {
        self.input(host, |w| w.mouse_released(event.button))
    }

    pub fn mouse_dragged(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenMouseDraggedPreEvent<'_>,
    ) -> Flow {
        let (at, button) = (event.screen.mouse, event.button);
        self.input(host, |w| w.mouse_dragged(host, at, button))
    }

    pub fn key_pressed(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenKeyPressedPreEvent<'_>,
    ) -> Flow {
        let (key, modifiers) = (event.key, event.modifiers);
        self.input(host, |w| w.key_pressed(host, key, modifiers))
    }

    pub fn char_typed(
        &self,
        host: &dyn ClientHost,
        event: &mut ScreenCharTypedPreEvent<'_>,
    ) -> Flow {
        let c = event.c;
        self.input(host, |w| w.char_typed(host, c))
    }
}
