//! The dock: a horizontal bar with three large tap buttons (mic, headphones, menu) in a
//! screen corner, plus "who is speaking" indicators. Drawn in `render_gui_post`, clicked in
//! `mouse_button_pre` (the cursor read through `ClientHost::cursor`).
//!
//! Buttons are 48 px (phone tap target). State is shown by colour/icon, not text: green
//! outline = active, grey with a slash = muted, red with a slash = deafened.

use crate::engine::{EngineState, Shared};
use crate::ui::{colors, tr};
use mod_api::{ClientHost, Corner, Flow, MouseButtonPreEvent, RenderGuiPostEvent};
use std::sync::{Arc, Mutex, OnceLock};

/// Dock geometry.
pub const BTN: f32 = 48.0;
pub const GAP: f32 = 6.0;
pub const PAD: f32 = 6.0;

/// The dock's rect from the last rendered frame, for click handling.
pub static DOCK_RECT: OnceLock<Mutex<Option<[f32; 4]>>> = OnceLock::new();

fn dock_rect_store() -> &'static Mutex<Option<[f32; 4]>> {
    DOCK_RECT.get_or_init(|| Mutex::new(None))
}

fn dock_size() -> (f32, f32) {
    (3.0 * BTN + 2.0 * GAP + 2.0 * PAD, BTN + 2.0 * PAD)
}

fn button_rects(dock: [f32; 4]) -> [[f32; 4]; 3] {
    let [x, y, _, _] = dock;
    let mut out = [[0.0; 4]; 3];
    for (i, r) in out.iter_mut().enumerate() {
        *r = [x + PAD + i as f32 * (BTN + GAP), y + PAD, BTN, BTN];
    }
    out
}

/// Which dock button a point is in: 0 = mic, 1 = headphones, 2 = menu.
pub fn button_at(point: [f32; 2]) -> Option<usize> {
    let guard = dock_rect_store().lock().unwrap();
    let rect = (*guard)?;
    button_rects(rect).iter().position(|[x, y, w, h]| {
        point[0] >= *x && point[0] < *x + *w && point[1] >= *y && point[1] < *y + *h
    })
}

/// Draws the dock (and the speaking indicators) in `render_gui_post`.
pub fn draw(
    host: &dyn ClientHost,
    event: &mut RenderGuiPostEvent<'_>,
    shared: &Arc<Mutex<Shared>>,
) {
    if !host.playing() && host.mod_screen() != Some(crate::MOD_ID) {
        *dock_rect_store().lock().unwrap() = None;
        return;
    }
    let show_dock = crate::settings::read_bool(host, crate::settings::KEY_SHOW_DOCK, true);
    if !show_dock {
        *dock_rect_store().lock().unwrap() = None;
        return;
    }
    let (w, h) = dock_size();
    // Prefer the bottom-right corner; skip corners F3's debug screen takes.
    let corner = [
        Corner::BottomRight,
        Corner::BottomLeft,
        Corner::TopRight,
        Corner::TopLeft,
    ]
    .into_iter()
    .find(|c| host.native_corner_shown(*c))
    .unwrap_or(Corner::BottomRight);
    let (x, y) = event.corners.place(corner, w, h);
    let dock = [x, y, w, h];
    *dock_rect_store().lock().unwrap() = Some(dock);

    let draw = &mut *event.draw;
    draw.fill(dock, colors::BG);
    draw.fill([dock[0], dock[1], dock[2], 1.0], colors::PANEL_EDGE);

    let shared = shared.lock().unwrap();
    let rects = button_rects(dock);
    // 0: mic — green when the gate is open, grey + slash when muted.
    let mic_on = shared.mic_open && !shared.muted && !shared.deafened;
    let mic_color = if shared.muted || shared.deafened {
        colors::OFF
    } else if mic_on {
        colors::ON
    } else {
        colors::OFF
    };
    draw_button(
        draw,
        rects[0],
        mic_color,
        Icon::Mic,
        shared.muted || shared.deafened,
    );
    // 1: headphones — green when hearing, red + slash when deafened.
    let deaf = shared.deafened;
    let hp_color = if deaf { colors::DEAF } else { colors::ON };
    draw_button(draw, rects[1], hp_color, Icon::Headphones, deaf);
    // 2: menu — neutral.
    draw_button(draw, rects[2], colors::NEUTRAL, Icon::Menu, false);

    // Speaking indicators to the left of the dock (or above it on the right edge).
    let now_speaking = speaking_now_from(&shared);
    let mut iy = dock[1] + dock[3] - 18.0;
    for name in now_speaking.iter().rev() {
        let label = tr(host, "mvchat.hud.speaking");
        let text = format!("{name} {label}");
        let tw = draw.text_width(&text);
        let tx = if corner.left() {
            dock[0] + dock[2] + 6.0
        } else {
            dock[0] - 6.0 - tw
        };
        draw.fill([tx - 4.0, iy, tw + 8.0, 16.0], colors::BG);
        draw.fill([tx, iy + 4.0, 8.0, 8.0], colors::SPEAKING);
        draw.text(&text, [tx + 12.0, iy + 4.0], colors::TEXT, false);
        iy -= 18.0;
    }
    if shared.deafened {
        let text = tr(host, "mvchat.hud.deafened");
        let tw = draw.text_width(&text);
        let tx = if corner.left() {
            dock[0] + dock[2] + 6.0
        } else {
            dock[0] - 6.0 - tw
        };
        draw.text(&text, [tx, dock[1] + dock[3] - 18.0], colors::DEAF, false);
    }
}

fn speaking_now_from(shared: &Shared) -> Vec<String> {
    let now = std::time::Instant::now();
    shared
        .peers
        .values()
        .filter(|p| p.speaking_until > now)
        .map(|p| p.name.clone())
        .collect()
}

#[derive(Clone, Copy)]
enum Icon {
    Mic,
    Headphones,
    Menu,
}

/// Draws one dock button: an outline in `color` and a simple icon; `slashed` adds a red
/// diagonal cross-out.
fn draw_button(
    draw: &mut dyn mod_api::DrawContext,
    rect: [f32; 4],
    color: u32,
    icon: Icon,
    slashed: bool,
) {
    let [x, y, w, h] = rect;
    // Outline (1 px frame).
    draw.fill([x, y, w, 1.0], color);
    draw.fill([x, y + h - 1.0, w, 1.0], color);
    draw.fill([x, y, 1.0, h], color);
    draw.fill([x + w - 1.0, y, 1.0, h], color);
    let cx = x + w / 2.0;
    let cy = y + h / 2.0;
    match icon {
        Icon::Mic => {
            // capsule + stand + base
            draw.fill([cx - 3.0, cy - 10.0, 6.0, 12.0], color);
            draw.fill([cx - 6.0, cy + 2.0, 12.0, 2.0], color);
            draw.fill([cx - 1.0, cy - 2.0, 2.0, 4.0], color);
        }
        Icon::Headphones => {
            // band + two earcups
            draw.fill([cx - 10.0, cy - 10.0, 20.0, 3.0], color);
            draw.fill([cx - 10.0, cy - 10.0, 3.0, 14.0], color);
            draw.fill([cx + 7.0, cy - 10.0, 3.0, 14.0], color);
            draw.fill([cx - 10.0, cy + 4.0, 6.0, 6.0], color);
            draw.fill([cx + 4.0, cy + 4.0, 6.0, 6.0], color);
        }
        Icon::Menu => {
            // three bars
            draw.fill([cx - 10.0, cy - 8.0, 20.0, 3.0], color);
            draw.fill([cx - 10.0, cy - 1.5, 20.0, 3.0], color);
            draw.fill([cx - 10.0, cy + 5.0, 20.0, 3.0], color);
        }
    }
    if slashed {
        draw.quad(
            [
                [x + 6.0, y + h - 6.0],
                [x + 10.0, y + h - 6.0],
                [x + w - 6.0, y + 6.0],
                [x + w - 10.0, y + 6.0],
            ],
            colors::DEAF,
        );
    }
}

/// Handles a click: toggles mic / deafen / opens the menu. `Flow::Cancel` when the dock took it.
pub fn click(
    host: &mut dyn ClientHost,
    event: &mut MouseButtonPreEvent,
    shared: &Arc<Mutex<Shared>>,
) -> Flow {
    if event.button != 0 || !event.pressed {
        return Flow::Continue;
    }
    if !host.playing() {
        return Flow::Continue;
    }
    let Some(which) = button_at(host.cursor()) else {
        return Flow::Continue;
    };
    match which {
        0 => {
            let mut s = shared.lock().unwrap();
            if !s.muted && !s.deafened {
                s.mic_open = !s.mic_open;
            }
        }
        1 => {
            let mut s = shared.lock().unwrap();
            s.deafened = !s.deafened;
            if s.deafened {
                s.mic_open = false;
            }
        }
        2 => {
            if host.mod_screen() == Some(crate::MOD_ID) {
                host.mod_screen_close(crate::MOD_ID);
            } else {
                host.mod_screen_open(crate::MOD_ID);
            }
        }
        _ => return Flow::Continue,
    }
    Flow::Cancel
}

/// The dock's status line for the menu: state + peers.
pub fn status_line(host: &dyn ClientHost, shared: &Arc<Mutex<Shared>>) -> String {
    let s = shared.lock().unwrap();
    let state_key = match s.state {
        EngineState::Off => "mvchat.state.off",
        EngineState::Waiting => "mvchat.state.waiting",
        EngineState::Connecting => "mvchat.state.connecting",
        EngineState::Ready => "mvchat.state.ready",
    };
    let state = tr(host, state_key);
    let peers = s.peers.len();
    let server = match s.server_has_mod {
        Some(false) => tr(host, "mvchat.state.no_server_mod"),
        _ => String::new(),
    };
    format!(
        "{state} · {} {} {}",
        peers,
        tr(host, "mvchat.menu.peers"),
        server
    )
}
