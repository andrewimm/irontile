//! Input handling: turning device events into layout commands.

use irontile_layout::Command;
use smithay::backend::input::{
    AbsolutePositionEvent, Axis as InputAxis, AxisSource, ButtonState, Event, InputBackend,
    InputEvent, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
};
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};

use crate::keymap::{Action, action_for};
use crate::state::Irontile;

/// Dispatches one input event.
///
/// `output_size` is the current logical size of the display, used to place
/// absolute pointer motion.
pub fn handle<B: InputBackend>(
    state: &mut Irontile,
    event: InputEvent<B>,
    output_size: Size<i32, Logical>,
) {
    match event {
        InputEvent::Keyboard { event } => keyboard::<B>(state, event),
        InputEvent::PointerMotionAbsolute { event } => {
            let location = event.position_transformed(output_size);
            pointer_motion(state, location, event.time_msec());
        }
        InputEvent::PointerButton { event } => pointer_button::<B>(state, &event),
        InputEvent::PointerAxis { event } => pointer_axis::<B>(state, &event),
        _ => {}
    }
}

fn keyboard<B: InputBackend>(state: &mut Irontile, event: B::KeyboardKeyEvent) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    let time = event.time_msec();
    let code = event.key_code();
    let key_state = event.state();

    let action = keyboard.input(
        state,
        code,
        key_state,
        serial,
        time,
        |_state, mods, handle| {
            if key_state != smithay::backend::input::KeyState::Pressed {
                return FilterResult::Forward;
            }
            // The unmodified symbol, so that a binding written as `Super+Shift+1`
            // matches the `1` key rather than whatever `Shift+1` produces on the
            // active layout.
            let sym = handle
                .raw_latin_sym_or_raw_current_sym()
                .unwrap_or_else(|| handle.modified_sym());
            match action_for(mods, sym) {
                // Intercepting means the client never sees the key, which is what
                // keeps a compositor binding from also typing into the window.
                Some(action) => FilterResult::Intercept(action),
                None => FilterResult::Forward,
            }
        },
    );

    if let Some(action) = action {
        perform(state, action);
    }
}

fn perform(state: &mut Irontile, action: Action) {
    match action {
        Action::Focus(dir) => state.apply(Command::FocusDirection { dir }),
        Action::Move(dir) => state.apply(Command::MoveWindow { window: None, dir }),
        Action::Resize(dir) => {
            let delta_px = state.theme.resize_step;
            state.apply(Command::Resize {
                window: None,
                dir,
                delta_px,
            });
        }
        Action::SetAxis(axis) => state.apply(Command::SetAxis { window: None, axis }),
        Action::Equalize => state.apply(Command::Equalize { window: None }),
        Action::ToggleFloating => state.apply(Command::SetFloating {
            window: None,
            floating: None,
        }),
        Action::ToggleFullscreen => state.apply(Command::SetFullscreen {
            window: None,
            fullscreen: None,
        }),
        Action::CloseWindow => state.close_focused(),
        Action::ShowWorkspace(n) => {
            let workspace = state.workspace_by_number(n);
            state.apply(Command::ShowWorkspace {
                workspace,
                output: None,
            });
        }
        Action::MoveToWorkspace(n) => {
            let workspace = state.workspace_by_number(n);
            state.apply(Command::MoveWindowToWorkspace {
                window: None,
                workspace,
                follow: false,
            });
        }
        Action::FocusOutput(dir) => state.apply(Command::FocusOutputDirection { dir }),
        Action::SendWorkspaceToOutput(dir) => state.send_workspace_to_output(dir),
        Action::SpawnTerminal => match state.theme.terminal.clone() {
            Some(terminal) => state.spawn(&terminal),
            None => tracing::warn!("no terminal found; set IRONTILE_TERMINAL"),
        },
        Action::Quit => state.running = false,
    }
}

fn pointer_motion(state: &mut Irontile, location: Point<f64, Logical>, time: u32) {
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let focus = state.surface_under(location);
    let serial = SERIAL_COUNTER.next_serial();
    pointer.motion(
        state,
        focus,
        &MotionEvent {
            location,
            serial,
            time,
        },
    );
    pointer.frame(state);
}

fn pointer_button<B: InputBackend>(state: &mut Irontile, event: &B::PointerButtonEvent) {
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    let button = event.button_code();
    let button_state = event.state();

    // Click to focus, before the button reaches the client, so the window that
    // receives the press is already the focused one.
    if button_state == ButtonState::Pressed
        && let Some(window) = state.window_at(pointer.current_location())
        && state.layout.focused_window() != Some(window)
    {
        state.apply(Command::FocusWindow { window });
        state.reflow();
    }

    pointer.button(
        state,
        &ButtonEvent {
            button,
            state: button_state,
            serial,
            time: event.time_msec(),
        },
    );
    pointer.frame(state);
}

fn pointer_axis<B: InputBackend>(state: &mut Irontile, event: &B::PointerAxisEvent) {
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let source = event.source();
    let mut frame = AxisFrame::new(event.time_msec()).source(source);

    for axis in [InputAxis::Horizontal, InputAxis::Vertical] {
        if let Some(discrete) = event.amount_v120(axis) {
            frame = frame.v120(axis, discrete as i32);
        }
        match event.amount(axis) {
            Some(amount) => {
                frame = frame.value(axis, amount);
                if amount == 0.0 && source == AxisSource::Finger {
                    frame = frame.stop(axis);
                }
            }
            None => {
                // Some backends report only discrete steps; synthesize a
                // continuous value so clients that ignore v120 still scroll.
                if let Some(discrete) = event.amount_v120(axis) {
                    frame = frame.value(axis, discrete / 120.0 * 15.0);
                }
            }
        }
    }

    pointer.axis(state, frame);
    pointer.frame(state);
}
