//! Input handling: turning device events into layout commands.

use irontile_ipc::Action;
use irontile_layout::{Command, Direction};
use smithay::backend::input::{
    AbsolutePositionEvent, Axis as InputAxis, AxisSource, ButtonState, Event, GestureBeginEvent,
    GestureEndEvent, GestureSwipeUpdateEvent, InputBackend, InputEvent, KeyboardKeyEvent,
    PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
};
use smithay::input::keyboard::{FilterResult, Keycode, xkb};
use smithay::input::pointer::{AxisFrame, ButtonEvent, CursorIcon, MotionEvent};
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::utils::{Logical, Point, SERIAL_COUNTER, Size};
use std::time::Duration;

use crate::action;
use crate::state::{Drag, DragKind, Irontile, KeyRepeat, REPEAT_DELAY_MS, REPEAT_RATE_HZ};

/// Linux input event code for the right mouse button.
const BTN_RIGHT: u32 = 0x111;
/// ...and the left one.
const BTN_LEFT: u32 = 0x110;

/// How far from a window's edge a press still counts as grabbing that edge.
///
/// The strip this covers is the window's border and the gap beside it, which is
/// the one part of the screen no client is drawing on. Reaching further, into
/// the window itself, would be an easier target and a worse trade: the last few
/// pixels of a window are where scrollbars live.
const EDGE_GRIP: i32 = 8;

/// Dispatches one input event.
///
/// `output_size` is the current logical size of the display, used to place
/// absolute pointer motion.
pub fn handle<B: InputBackend>(
    state: &mut Irontile,
    event: InputEvent<B>,
    output_size: Size<i32, Logical>,
) {
    // Somebody is there. Devices appearing and disappearing are not somebody
    // being there, and neither is the control socket moving the pointer -- that
    // arrives further in, so a script driving the compositor cannot hold the
    // screen awake by pretending to be a hand.
    if !matches!(
        event,
        InputEvent::DeviceAdded { .. } | InputEvent::DeviceRemoved { .. }
    ) {
        let seat = state.seat.clone();
        state.idle_notifier.notify_activity(&seat);
    }

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
        InputEvent::GestureSwipeBegin { event } => {
            state.swipe = (event.fingers() == SWIPE_FINGERS).then_some((0.0, 0.0));
        }
        InputEvent::GestureSwipeUpdate { event } => {
            if let Some((x, y)) = &mut state.swipe {
                *x += event.delta_x();
                *y += event.delta_y();
            }
        }
        InputEvent::GestureSwipeEnd { event } => {
            // Taken either way: a cancelled gesture is over, and leaving the
            // total behind would add the next swipe to this one.
            let swipe = state.swipe.take();
            if let Some((x, y)) = swipe
                && !event.cancelled()
                && let Some(step) = swipe_step(x, y)
            {
                action::perform(state, &Action::WorkspaceStep(step));
            }
        }
        _ => {}
    }
}

/// How many fingers a desktop swipe takes.
const SWIPE_FINGERS: u32 = 3;

/// How far a swipe has to travel before it counts, in logical pixels.
///
/// Low enough that a deliberate flick registers, high enough that resting three
/// fingers on the pad and shifting slightly does not move the desktop out from
/// under you.
const SWIPE_THRESHOLD: f64 = 100.0;

/// Which way a finished swipe went, if it went anywhere.
///
/// Measured from the total travel rather than the last movement, so a swipe
/// that wanders and comes back is correctly no swipe at all.
fn swipe_step(x: f64, y: f64) -> Option<i32> {
    // Mostly sideways, or it was a scroll that drifted.
    if x.abs() < SWIPE_THRESHOLD || x.abs() < y.abs() {
        return None;
    }
    // Swiping left moves the desktops left, which brings the next one in from
    // the right -- the direction the content moves, as on a touchscreen.
    Some(if x < 0.0 { 1 } else { -1 })
}

fn keyboard<B: InputBackend>(state: &mut Irontile, event: B::KeyboardKeyEvent) {
    let Some(keyboard) = state.seat.get_keyboard() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    let time = event.time_msec();
    let code = event.key_code();
    let key_state = event.state();

    let bind = keyboard.input(
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
            let matched = state.config.keymap.bind_for(mods, sym).cloned();

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
                    action = ?matched.as_ref().map(|bind| &bind.action),
                    "binding"
                );
            }

            match matched {
                // While the session is locked the keyboard is the lock
                // screen's. A binding that fires anyway is a hole straight
                // through it -- most of them merely rearrange a desktop
                // nobody can see, but `spawn` runs a program behind it and
                // quitting hands back the terminal the session was started
                // from. Forwarding rather than swallowing, so the keystroke
                // reaches the lock screen and counts towards the password
                // somebody is typing.
                Some(bind)
                    if state.session_lock.is_some()
                        && !crate::action::fires_while_locked(&bind.action, bind.locked) =>
                {
                    FilterResult::Forward
                }
                // Intercepting means the client never sees the key, which is what
                // keeps a compositor binding from also typing into the window.
                Some(bind) => FilterResult::Intercept(bind),
                None => FilterResult::Forward,
            }
        },
    );

    // A release ends whatever it was holding open; a press of anything else
    // leaves it alone, so rolling onto another key does not stop the ramp.
    if key_state != smithay::backend::input::KeyState::Pressed {
        stop_repeat(state, Some(code));
    }

    if let Some(bind) = bind {
        // A new binding takes over from whatever was repeating. Two ramps at
        // once is never what was meant, and the key holding the old one open
        // may be one whose release is never seen.
        stop_repeat(state, None);
        perform(state, &bind.action);
        if bind.repeat {
            start_repeat(state, code, bind.action);
        }
    }
}

/// Starts a held binding firing on its own.
///
/// Key repeat for a binding has to happen here: the client that would normally
/// do its own repeating never sees an intercepted key, and the input backend
/// reports a press and a release and nothing in between.
fn start_repeat(state: &mut Irontile, code: Keycode, action: Action) {
    let rate = Duration::from_secs_f64(1.0 / f64::from(REPEAT_RATE_HZ.max(1)));
    let timer = Timer::from_duration(Duration::from_millis(REPEAT_DELAY_MS.max(0) as u64));
    let repeated = action.clone();
    let token = state.loop_handle.insert_source(timer, move |_, _, state| {
        action::perform(state, &repeated);
        TimeoutAction::ToDuration(rate)
    });
    match token {
        Ok(token) => state.repeat = Some(KeyRepeat { code, token }),
        Err(err) => tracing::warn!(?err, "could not start a repeating binding"),
    }
}

/// Stops the repeating binding, if `code` is the key holding it open -- or
/// unconditionally when no key is named.
fn stop_repeat(state: &mut Irontile, code: Option<Keycode>) {
    let Some(repeat) = &state.repeat else {
        return;
    };
    if code.is_some_and(|code| code != repeat.code) {
        return;
    }
    let token = repeat.token;
    state.repeat = None;
    state.loop_handle.remove(token);
}

fn perform(state: &mut Irontile, action: &Action) {
    action::perform(state, action);
}

/// Puts the pointer somewhere, as though it had been moved there.
///
/// The compositor draws the pointer, so it is the only thing that can move it.
/// Everything a pointer reaches -- a panel's buttons, the pointer resting on
/// one -- is otherwise only reachable by hand, which is how a bar that received
/// no pointer events at all went unnoticed.
pub fn warp(state: &mut Irontile, x: f64, y: f64) {
    let at = clamp_to_displays(state, Point::from((x, y)));
    pointer_motion(state, at, state.start_time.elapsed().as_millis() as u32);
}

/// Presses and releases a button where the pointer is.
pub fn click(state: &mut Irontile, button: u32) {
    press(state, button, true);
    press(state, button, false);
}

/// Holds a button down, or lets it go.
pub fn press(state: &mut Irontile, button: u32, down: bool) {
    // The numbering people use for mouse buttons, mapped onto the kernel's.
    let code = match button {
        2 => 0x112,
        3 => BTN_RIGHT,
        _ => BTN_LEFT,
    };
    let time = state.start_time.elapsed().as_millis() as u32;
    let button_state = if down {
        ButtonState::Pressed
    } else {
        ButtonState::Released
    };
    button_event(state, code, button_state, time);
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
    // The compositor draws the pointer, so moving it is a change to the screen
    // that no client will commit for.
    state.redraw = true;
    let serial = SERIAL_COUNTER.next_serial();

    if let Some(drag) = state.drag {
        // The arrow stays as it was when the drag began, so it does not flicker
        // as what is under the pointer changes.
        state.cursor_hint = drag_cursor(drag.kind);
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
    // Only where no client would have received the press, which is the same
    // test the press itself makes: an arrow promising a resize that would not
    // happen is worse than no arrow at all.
    state.cursor_hint = match &focus {
        Some(_) => None,
        None => state
            .edges_near(location, EDGE_GRIP)
            .and_then(|(_, horizontal, vertical)| edge_cursor(horizontal, vertical)),
    };

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
fn apply_drag(state: &mut Irontile, drag: Drag, location: Point<f64, Logical>) {
    let dx = (location.x - drag.last.x) as i32;
    let dy = (location.y - drag.last.y) as i32;
    // Below a whole pixel there is nothing to do, and the anchor stays put so
    // the remainder is not lost to rounding.
    if dx == 0 && dy == 0 {
        return;
    }

    let window = Some(drag.window);
    match drag.kind {
        DragKind::Resize {
            horizontal,
            vertical,
        } => {
            if let Some(dir) = horizontal
                && dx != 0
            {
                // Growing a left edge means moving the pointer left, so the
                // sign flips for the edges that run backwards.
                let delta_px = if dir.is_forward() { dx } else { -dx };
                state.apply(Command::Resize {
                    window,
                    dir,
                    delta_px,
                });
            }
            if let Some(dir) = vertical
                && dy != 0
            {
                let delta_px = if dir.is_forward() { dy } else { -dy };
                state.apply(Command::Resize {
                    window,
                    dir,
                    delta_px,
                });
            }
        }
        // A whole-rectangle move, because the floating position is stored
        // outright rather than as an offset from anything.
        DragKind::Move => {
            if let Some(rect) = state.cell_of(drag.window) {
                state.apply(Command::MoveFloating {
                    window,
                    rect: irontile_layout::Rect::new(rect.x + dx, rect.y + dy, rect.w, rect.h),
                });
            }
        }
    }

    state.drag = Some(Drag {
        last: location,
        ..drag
    });
    state.reflow();
}

/// The pointer image for a drag in progress.
fn drag_cursor(kind: DragKind) -> Option<CursorIcon> {
    match kind {
        DragKind::Resize {
            horizontal,
            vertical,
        } => edge_cursor(horizontal, vertical),
        DragKind::Move => Some(CursorIcon::Grabbing),
    }
}

/// The arrow for an edge, pointing the way that edge will move.
fn edge_cursor(horizontal: Option<Direction>, vertical: Option<Direction>) -> Option<CursorIcon> {
    match (horizontal, vertical) {
        (Some(_), None) => Some(CursorIcon::EwResize),
        (None, Some(_)) => Some(CursorIcon::NsResize),
        // Corners, named for the diagonal they lie on rather than for the
        // corner itself: top-left and bottom-right share one arrow.
        (Some(Direction::Left), Some(Direction::Up))
        | (Some(Direction::Right), Some(Direction::Down)) => Some(CursorIcon::NwseResize),
        (Some(Direction::Right), Some(Direction::Up))
        | (Some(Direction::Left), Some(Direction::Down)) => Some(CursorIcon::NeswResize),
        // Nothing hands a vertical direction to the horizontal axis, and the
        // cursor is the wrong place to find out that something did.
        _ => None,
    }
}

/// Which edges a drag moves, decided by where in the window it started.
fn drag_edges(
    cell: irontile_layout::Rect,
    at: Point<f64, Logical>,
) -> (Option<Direction>, Option<Direction>) {
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
    (Some(horizontal), Some(vertical))
}

fn pointer_button<B: InputBackend>(state: &mut Irontile, event: &B::PointerButtonEvent) {
    button_event(state, event.button_code(), event.state(), event.time_msec());
}

/// One button press or release, wherever it came from.
///
/// Real hardware and the control socket both arrive here, so a synthetic press
/// does everything a real one does -- focuses the window under it, starts a
/// drag, reaches the client -- rather than only the last of those. Anything
/// that skipped this would be testing a path nobody uses.
pub fn button_event(state: &mut Irontile, button: u32, button_state: ButtonState, time: u32) {
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();

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
        state.drag = Some(Drag {
            window,
            last: location,
            kind: DragKind::Resize {
                horizontal,
                vertical,
            },
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
                time,
            },
        );
        pointer.frame(state);
        return;
    }

    // Super and the left button moves a floating window, the mirror of Super
    // and the right button resizing one. A tiled window is deliberately not
    // draggable: where it sits is the layout's to decide, and the next reflow
    // would undo the move anyway.
    if button_state == ButtonState::Pressed
        && button == BTN_LEFT
        && logo
        && !pointer.is_grabbed()
        && let Some(window) = state.floating_at(location)
    {
        state.drag = Some(Drag {
            window,
            last: location,
            kind: DragKind::Move,
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
                time,
            },
        );
        pointer.frame(state);
        return;
    }

    // Dragging an edge resizes, the way a floating compositor's window frame
    // does. The press has to land somewhere no client would have received it
    // anyway -- checked here rather than assumed from the geometry -- so this
    // can never swallow a click meant for a window.
    if button_state == ButtonState::Pressed
        && button == BTN_LEFT
        && !pointer.is_grabbed()
        && state.surface_under(location).is_none()
        && let Some((window, horizontal, vertical)) = state.edges_near(location, EDGE_GRIP)
    {
        state.drag = Some(Drag {
            window,
            last: location,
            kind: DragKind::Resize {
                horizontal,
                vertical,
            },
        });
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
            time,
        },
    );
    pointer.frame(state);
}

fn pointer_axis<B: InputBackend>(state: &mut Irontile, event: &B::PointerAxisEvent) {
    let Some(pointer) = state.seat.get_pointer() else {
        return;
    };
    let source = event.source();
    // Scroll direction is not reversed here. libinput applies it per device,
    // which is the only place that can tell a touchpad from a trackpoint from a
    // wheel; inferring it from the axis source would guess, and guessing again
    // on top of a device that has already been configured would undo it.
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
    fn an_edge_points_along_the_axis_it_moves() {
        use super::edge_cursor;
        use irontile_layout::Direction;
        use smithay::input::pointer::CursorIcon;

        assert_eq!(
            edge_cursor(Some(Direction::Left), None),
            Some(CursorIcon::EwResize)
        );
        assert_eq!(
            edge_cursor(None, Some(Direction::Down)),
            Some(CursorIcon::NsResize)
        );
        // Opposite corners of a rectangle lie on the same diagonal, so they
        // share an arrow.
        assert_eq!(
            edge_cursor(Some(Direction::Left), Some(Direction::Up)),
            edge_cursor(Some(Direction::Right), Some(Direction::Down)),
        );
        assert_eq!(
            edge_cursor(Some(Direction::Right), Some(Direction::Up)),
            edge_cursor(Some(Direction::Left), Some(Direction::Down)),
        );
        assert_ne!(
            edge_cursor(Some(Direction::Left), Some(Direction::Up)),
            edge_cursor(Some(Direction::Right), Some(Direction::Up)),
            "the two diagonals are not the same arrow"
        );
        assert_eq!(edge_cursor(None, None), None, "nothing to grab, no arrow");
    }

    #[test]
    fn a_move_grabs_rather_than_pointing_anywhere() {
        use super::drag_cursor;
        use crate::state::DragKind;
        use smithay::input::pointer::CursorIcon;

        // A move has no edge and no axis, so none of the resize arrows would
        // say anything true about it.
        assert_eq!(drag_cursor(DragKind::Move), Some(CursorIcon::Grabbing));
        assert_eq!(
            drag_cursor(DragKind::Resize {
                horizontal: Some(irontile_layout::Direction::Left),
                vertical: None,
            }),
            Some(CursorIcon::EwResize)
        );
    }

    #[test]
    fn a_swipe_counts_only_when_it_went_far_enough_and_sideways() {
        use super::{SWIPE_THRESHOLD, swipe_step};

        let far = SWIPE_THRESHOLD + 1.0;
        // Left brings the next desktop in from the right, the way content
        // follows the fingers on a touchscreen.
        assert_eq!(swipe_step(-far, 0.0), Some(1));
        assert_eq!(swipe_step(far, 0.0), Some(-1));

        // A nudge is not a swipe.
        assert_eq!(swipe_step(SWIPE_THRESHOLD - 1.0, 0.0), None);
        // Nor is a scroll that drifted sideways.
        assert_eq!(swipe_step(far, far * 2.0), None);
        // Nor is wandering out and back, because the total is what is measured.
        assert_eq!(swipe_step(0.0, 0.0), None);
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
