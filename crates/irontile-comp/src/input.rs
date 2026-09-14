//! Input handling: turning device events into layout commands.

use irontile_ipc::Action;
use irontile_layout::{Command, Direction};
use smithay::backend::input::{
    AbsolutePositionEvent, Axis as InputAxis, AxisSource, ButtonState, Event, InputBackend,
    InputEvent, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::input::keyboard::{FilterResult, xkb};
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
        // Mice and touchpads report how far they moved, not where they are.
        // Only a tablet or a nested window reports a position, so without this
        // the pointer never moves on real hardware at all.
        InputEvent::PointerMotion { event } => {
            let Some(pointer) = state.seat.get_pointer() else {
                return;
            };
            let current = pointer.current_location();
            let moved = Point::<f64, Logical>::from((
                current.x + event.delta_x(),
                current.y + event.delta_y(),
            ));
            pointer_motion(state, clamp_to_displays(state, moved), event.time_msec());
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
            let matched = state.config.keymap.action_for(mods, sym).cloned();

            // Only keys that could be a binding are logged, and never plain
            // text. Logging every keysym would write everything the user types
            // -- passwords included -- into a file as soon as anyone turned on
            // debug logging, which is not a trade worth making for a
            // diagnostic. A held Super, Control or Alt means the press was
            // aimed at the compositor rather than at a document, and that is
            // exactly the case worth being able to debug: shift alone is
            // typing, so it does not count.
            if should_log(matched.is_some(), mods) {
                tracing::debug!(
                    key = %xkb::keysym_get_name(sym),
                    logo = mods.logo,
                    ctrl = mods.ctrl,
                    alt = mods.alt,
                    shift = mods.shift,
                    action = ?matched,
                    "binding"
                );
            }

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

/// Whether a keypress may be written to the log.
///
/// A held Super, Control or Alt means the press was aimed at the compositor
/// rather than at a document, and those are the ones worth being able to debug.
/// Shift alone is typing. Anything else is the user's text and must not be
/// recorded anywhere.
fn should_log(matched: bool, mods: &smithay::input::keyboard::ModifiersState) -> bool {
    matched || mods.logo || mods.ctrl || mods.alt
}

/// Keeps the pointer on a display, since a delta on its own respects no edges.
fn clamp_to_displays(state: &Irontile, point: Point<f64, Logical>) -> Point<f64, Logical> {
    let whole = irontile_layout::Point::new(point.x.floor() as i32, point.y.floor() as i32);
    let clamped = state.layout.clamp_to_outputs(whole);
    if clamped == whole {
        // Already on a display; keep the sub-pixel part, which is what makes
        // slow pointer movement smooth rather than stepped.
        point
    } else {
        Point::from((f64::from(clamped.x), f64::from(clamped.y)))
    }
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

#[cfg(test)]
mod tests {
    use super::should_log;
    use smithay::input::keyboard::ModifiersState;

    fn mods(logo: bool, shift: bool, ctrl: bool, alt: bool) -> ModifiersState {
        ModifiersState {
            logo,
            shift,
            ctrl,
            alt,
            ..Default::default()
        }
    }

    #[test]
    fn typing_is_never_logged() {
        // The whole point: turning on debug logging must not turn the
        // compositor into a keylogger. Passwords are typed with no modifier, or
        // with shift, and neither may be recorded.
        assert!(!should_log(false, &mods(false, false, false, false)));
        assert!(!should_log(false, &mods(false, true, false, false)));
    }

    #[test]
    fn presses_aimed_at_the_compositor_are_logged() {
        // These are the ones worth debugging, and none of them are text.
        assert!(should_log(false, &mods(true, false, false, false)));
        assert!(should_log(false, &mods(false, false, true, true)));
        assert!(should_log(false, &mods(false, false, false, true)));
    }

    #[test]
    fn a_binding_that_fired_is_always_logged() {
        // If a key did something, saying so is not a leak: the action is
        // already visible in its effect.
        assert!(should_log(true, &mods(false, false, false, false)));
    }
}
