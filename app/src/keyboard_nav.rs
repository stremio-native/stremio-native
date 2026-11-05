//! Keyboard navigation for the shell.
//!
//! When enabled in Settings, the arrow keys move a focus ring between the
//! clickable items on screen and Enter clicks the focused one. The heavy
//! lifting — finding reachable targets, choosing the nearest one in a
//! direction, scrolling it into view and emulating the pointer so the item's
//! own hover and click handling runs — lives in the vendored engine, see
//! `vendor/i-slint-core/spatial_navigation.rs`. This module owns the policy:
//! which keys navigate, when navigation is off (the player has its own arrow
//! bindings), the ring overlay, and yielding to the real mouse.
//!
//! Keys only reach this module when nothing focused in the UI consumed them,
//! so sliders, menus and text fields keep their own arrow handling. The one
//! exception is Up/Down in a single-line text field, which would otherwise
//! trap the focus in every search box; Escape also leaves a text field.

use std::{cell::Cell, rc::Rc, time::Duration};

use i_slint_core::{
    input::KeyEvent,
    spatial_navigation::{SpatialDirection, SpatialNavResult},
    window::WindowInner,
};
use slint::{
    ComponentHandle, Timer, TimerMode,
    platform::Key,
    winit_030::{EventResult, winit},
};

use crate::MainWindow;

/// How often the ring follows its target (hover scaling, scroll animations,
/// layout changes).
const RING_REFRESH: Duration = Duration::from_millis(33);
/// Re-aim the synthetic pointer once an animated scroll has settled.
const HOVER_SETTLE: Duration = Duration::from_millis(300);
/// After paging a virtualized list, give it a moment to instantiate rows.
const SCROLL_RETRY: Duration = Duration::from_millis(80);
/// Real pointer travel, in physical pixels, that hands control back to it.
const MOUSE_TAKEOVER_DISTANCE: f64 = 4.0;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static RING_TIMER: Timer = Timer::default();
    static LAST_CURSOR: Cell<Option<(f64, f64)>> = const { Cell::new(None) };
}

fn engine(ui: &MainWindow) -> &WindowInner {
    WindowInner::from_pub(ui.window())
}

fn direction_for(ch: char) -> Option<SpatialDirection> {
    match ch {
        c if c == char::from(Key::UpArrow) => Some(SpatialDirection::Up),
        c if c == char::from(Key::DownArrow) => Some(SpatialDirection::Down),
        c if c == char::from(Key::LeftArrow) => Some(SpatialDirection::Left),
        c if c == char::from(Key::RightArrow) => Some(SpatialDirection::Right),
        _ => None,
    }
}

fn active(ui: &MainWindow) -> bool {
    ENABLED.get() && !ui.get_show_player()
}

/// Installs the key hook and the mouse-takeover listener. Navigation stays
/// inactive until [`set_enabled`] turns it on.
pub fn install(ui: &MainWindow) {
    let weak = ui.as_weak();
    engine(ui).set_unhandled_key_hook(Some(Rc::new(move |event: &KeyEvent| {
        let Some(ui) = weak.upgrade() else {
            return false;
        };
        handle_key(&ui, event)
    })));

    let weak = ui.as_weak();
    crate::window_hooks::register(ui, move |_, event| {
        let pointer_took_over = match event {
            winit::event::WindowEvent::CursorMoved { position, .. } => {
                let now = (position.x, position.y);
                LAST_CURSOR.replace(Some(now)).is_some_and(|before| {
                    (now.0 - before.0).abs() + (now.1 - before.1).abs() > MOUSE_TAKEOVER_DISTANCE
                })
            }
            winit::event::WindowEvent::MouseInput { .. }
            | winit::event::WindowEvent::MouseWheel { .. } => true,
            _ => false,
        };
        if pointer_took_over
            && ENABLED.get()
            && let Some(ui) = weak.upgrade()
        {
            // The real pointer delivers its own hover; no synthetic exit.
            engine(&ui).spatial_clear(false);
            update_ring(&ui);
        }
        EventResult::Propagate
    });
}

fn handle_key(ui: &MainWindow, event: &KeyEvent) -> bool {
    if !active(ui) {
        return false;
    }
    let modifiers = &event.modifiers;
    if modifiers.control || modifiers.alt || modifiers.meta {
        return false;
    }
    let Some(ch) = event.text.chars().next() else {
        return false;
    };

    if let Some(direction) = direction_for(ch) {
        if modifiers.shift {
            return false;
        }
        let leaving_text_field = engine(ui).text_input_has_focus();
        // Run outside the engine's key dispatch: navigating sends synthetic
        // pointer events.
        let weak = ui.as_weak();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = weak.upgrade()
                && navigate(&ui, direction, true)
                && leaving_text_field
            {
                // Typing should no longer land in the field we left.
                ui.invoke_focus_app_shortcuts();
            }
        });
        return true;
    }

    if ch == char::from(Key::Return) {
        if engine(ui).spatial_focus_rect().is_none() {
            return false;
        }
        let weak = ui.as_weak();
        Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = weak.upgrade() {
                engine(&ui).spatial_activate();
                update_ring(&ui);
            }
        });
        return true;
    }

    if ch == char::from(Key::Escape) && engine(ui).text_input_has_focus() {
        ui.invoke_focus_app_shortcuts();
        return true;
    }

    false
}

/// Returns whether the focus moved.
fn navigate(ui: &MainWindow, direction: SpatialDirection, allow_scroll: bool) -> bool {
    match engine(ui).spatial_navigate(direction, allow_scroll) {
        SpatialNavResult::Moved => {
            update_ring(ui);
            let weak = ui.as_weak();
            Timer::single_shot(HOVER_SETTLE, move || {
                if let Some(ui) = weak.upgrade()
                    && active(&ui)
                {
                    engine(&ui).spatial_refresh_hover();
                }
            });
            true
        }
        SpatialNavResult::Scrolled => {
            let weak = ui.as_weak();
            Timer::single_shot(SCROLL_RETRY, move || {
                if let Some(ui) = weak.upgrade()
                    && active(&ui)
                {
                    navigate(&ui, direction, false);
                }
            });
            false
        }
        SpatialNavResult::Unchanged => false,
    }
}

fn update_ring(ui: &MainWindow) {
    match engine(ui).spatial_focus_rect() {
        Some((x, y, width, height)) => {
            ui.set_kbnav_ring_x(x);
            ui.set_kbnav_ring_y(y);
            ui.set_kbnav_ring_width(width);
            ui.set_kbnav_ring_height(height);
            ui.set_kbnav_ring_visible(true);
        }
        None => ui.set_kbnav_ring_visible(false),
    }
}

/// Turns keyboard navigation on or off and mirrors the state into the UI.
pub fn set_enabled(ui: &MainWindow, enabled: bool) {
    ENABLED.set(enabled);
    ui.set_settings_keyboard_navigation(enabled);
    if enabled {
        let weak = ui.as_weak();
        RING_TIMER.with(|timer| {
            timer.start(TimerMode::Repeated, RING_REFRESH, move || {
                if let Some(ui) = weak.upgrade() {
                    update_ring(&ui);
                }
            });
        });
    } else {
        RING_TIMER.with(Timer::stop);
        engine(ui).spatial_clear(true);
        ui.set_kbnav_ring_visible(false);
    }
}
