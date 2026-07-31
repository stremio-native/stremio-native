//! Multiplexes Slint's single native window-event filter.
//!
//! `Window::on_winit_window_event` stores the callback in a `Cell`, so every
//! call silently replaces the previously installed filter. Registering the
//! hotkey dispatcher and the shell's focus/drop handling separately therefore
//! left whichever one ran last as the only live consumer. Everything that needs
//! raw winit window events goes through [`register`] instead, which keeps one
//! filter installed and fans the events out in registration order.

use std::cell::{Cell, RefCell};

use slint::{
    ComponentHandle,
    winit_030::{EventResult, WinitWindowAccessor, winit},
};

use crate::MainWindow;

type Hook = Box<dyn FnMut(&slint::Window, &winit::event::WindowEvent) -> EventResult>;

thread_local! {
    static HOOKS: RefCell<Vec<Hook>> = const { RefCell::new(Vec::new()) };
    static INSTALLED: Cell<bool> = const { Cell::new(false) };
}

/// Adds a native window-event consumer. Hooks see each event in the order they
/// were registered; the first one to answer [`EventResult::PreventDefault`]
/// ends the dispatch and keeps the event away from Slint.
pub fn register(
    ui: &MainWindow,
    hook: impl FnMut(&slint::Window, &winit::event::WindowEvent) -> EventResult + 'static,
) {
    HOOKS.with_borrow_mut(|hooks| hooks.push(Box::new(hook)));
    if !INSTALLED.replace(true) {
        ui.window().on_winit_window_event(dispatch);
    }
}

fn dispatch(window: &slint::Window, event: &winit::event::WindowEvent) -> EventResult {
    // Hooks are moved out for the duration of the dispatch: a hook is free to
    // invoke UI callbacks that register another hook without tripping over the
    // borrow, and any late arrival is appended afterwards.
    let mut hooks = HOOKS.with_borrow_mut(std::mem::take);
    let result = fan_out(&mut hooks, |hook| hook(window, event));
    HOOKS.with_borrow_mut(|pending| {
        hooks.append(pending);
        *pending = hooks;
    });
    result
}

fn fan_out<H>(hooks: &mut [H], mut call: impl FnMut(&mut H) -> EventResult) -> EventResult {
    for hook in hooks {
        if matches!(call(hook), EventResult::PreventDefault) {
            return EventResult::PreventDefault;
        }
    }
    EventResult::Propagate
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each hook is modelled by whether it claims the event.
    fn answer(claims: &mut bool) -> EventResult {
        if *claims {
            EventResult::PreventDefault
        } else {
            EventResult::Propagate
        }
    }

    #[test]
    fn every_hook_sees_an_unclaimed_event() {
        let mut hooks = [false, false, false];
        let mut calls = 0;

        let result = fan_out(&mut hooks, |hook| {
            calls += 1;
            answer(hook)
        });

        assert_eq!(calls, 3);
        assert!(matches!(result, EventResult::Propagate));
    }

    #[test]
    fn a_claimed_event_stops_at_the_first_consumer() {
        let mut hooks = [false, true, false];
        let mut calls = 0;

        let result = fan_out(&mut hooks, |hook| {
            calls += 1;
            answer(hook)
        });

        assert_eq!(calls, 2);
        assert!(matches!(result, EventResult::PreventDefault));
    }
}
