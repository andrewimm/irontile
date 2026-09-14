//! Input handling: turning device events into layout commands.

use irontile_ipc::Action;
use irontile_layout::{Command, Direction};
use smithay::backend::input::{
    AbsolutePositionEvent, Axis as InputAxis, AxisSource, ButtonState, Event, InputBackend,
    InputEvent, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
};
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent};
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};

use crate::action;
use crate::state::{Irontile, ResizeDrag};

/// Linux input event code for the right mouse button.
const BTN_RIGHT: u32 = 0x111;

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
        |state, mods, handle| {
            if key_state != smithay::backend::input::KeyState::Pressed {
                return FilterResult::Forward;
            }
            // The unmodified symbol, so that a binding written as `Super+Shift+1`
            // matches the `1` key rather than whatever `Shift+1` produces on the
            // active layout.
            let sym = handle
                .raw_latin_sym_or_raw_current_sym()
                .unwrap_or_else(|| handle.modified_sym());
            match state.config.keymap.action_for(mods, sym) {
                // Intercepting means the client never sees the key, which is what
                // keeps a compositor binding from also typing into the window.
                Some(action) => FilterResult::Intercept(action.clone()),
                None => FilterResult::Forward,
            }
        },
    );

    if let Some(action) = action {
        perform(state, &action);
    }
}

fn perform(state: &mut Irontile, action: &Action) {
    action::perform(state, action);
}

fn pointer_motion(state: &mut Irontile, location: Point<f64, Logical>, time: u32) {
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();

    if let Some(drag) = state.drag {
        // The pointer still has to move, or the next delta would be measured
        // from a stale position, but the client sees nothing while it lasts.
        pointer.motion(
            state,
            None,
            &MotionEvent {
                location,
                serial,
                time,
            },
        );
        pointer.frame(state);
        apply_drag(state, drag, location);
        return;
    }

    let focus = state.surface_under(location);
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

/// Turns one step of a drag into resize commands.
fn apply_drag(state: &mut Irontile, drag: ResizeDrag, location: Point<f64, Logical>) {
    let dx = (location.x - drag.last.x) as i32;
    let dy = (location.y - drag.last.y) as i32;
    // Below a whole pixel there is nothing to do, and the anchor stays put so
    // the remainder is not lost to rounding.
    if dx == 0 && dy == 0 {
        return;
    }

    let window = Some(drag.window);
    if dx != 0 {
        // Growing a left edge means moving the pointer left, so the sign flips
        // for the edges that run backwards.
        let delta_px = if drag.horizontal.is_forward() {
            dx
        } else {
            -dx
        };
        state.apply(Command::Resize {
            window,
            dir: drag.horizontal,
            delta_px,
        });
    }
    if dy != 0 {
        let delta_px = if drag.vertical.is_forward() { dy } else { -dy };
        state.apply(Command::Resize {
            window,
            dir: drag.vertical,
            delta_px,
        });
    }

    state.drag = Some(ResizeDrag {
        last: location,
        ..drag
    });
    state.reflow();
}

/// Which edges a drag moves, decided by where in the window it started.
fn drag_edges(cell: irontile_layout::Rect, at: Point<f64, Logical>) -> (Direction, Direction) {
    let centre = cell.center();
    let horizontal = if (at.x as i32) < centre.x {
        Direction::Left
    } else {
        Direction::Right
    };
    let vertical = if (at.y as i32) < centre.y {
        Direction::Up
    } else {
        Direction::Down
    };
    (horizontal, vertical)
}

fn pointer_button<B: InputBackend>(state: &mut Irontile, event: &B::PointerButtonEvent) {
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    let button = event.button_code();
    let button_state = event.state();

    let location = pointer.current_location();

    // A release always ends a drag, whichever button caused it.
    if button_state == ButtonState::Released && state.drag.take().is_some() {
        pointer.frame(state);
        return;
    }

    // Super plus the right button resizes, standing in for the titlebar drag
    // that a compositor drawing no titlebars cannot offer.
    let logo = state
        .seat
        .get_keyboard()
        .is_some_and(|k| k.modifier_state().logo);
    if button_state == ButtonState::Pressed
        && button == BTN_RIGHT
        && logo
        && !pointer.is_grabbed()
        && let Some(window) = state.window_at(location)
        && let Some(cell) = state.cell_of(window)
    {
        let (horizontal, vertical) = drag_edges(cell, location);
        state.drag = Some(ResizeDrag {
            window,
            horizontal,
            vertical,
            last: location,
        });
        // Tell the client the pointer left, so it stops drawing hover states
        // for a pointer it will not hear from again until the drag ends.
        let serial = SERIAL_COUNTER.next_serial();
        pointer.motion(
            state,
            None,
            &MotionEvent {
                location,
                serial,
                time: event.time_msec(),
            },
        );
        pointer.frame(state);
        return;
    }

    // Click to focus, before the button reaches the client, so the window that
    // receives the press is already the focused one.
    if button_state == ButtonState::Pressed
        && let Some(window) = state.window_at(location)
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
