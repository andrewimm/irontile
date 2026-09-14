//! Multi-display behaviour: arrangement, desktop transfer, and hotplug.

use irontile_layout::{
    Command, Config, Direction, InsertTarget, Layout, Output, OutputId, Rect, WindowId,
    WorkspaceId, dispatch, frame,
};

const LEFT: OutputId = OutputId(1);
const RIGHT: OutputId = OutputId(2);

fn left() -> Output {
    Output::new(LEFT, "DP-1", Rect::new(0, 0, 1920, 1080))
}

fn right() -> Output {
    Output::new(RIGHT, "DP-2", Rect::new(1920, 0, 1920, 1080))
}

fn two_displays() -> Layout {
    let mut layout = Layout::new(Config::default());
    layout.reconfigure_outputs(vec![left(), right()]);
    layout.validate().unwrap();
    layout
}

/// Adds a window to whichever desktop is on `output`.
fn add_on(layout: &mut Layout, output: OutputId, window: u64) -> WorkspaceId {
    layout.focus_output(output).unwrap();
    layout
        .add_window(WindowId(window), None, InsertTarget::default())
        .unwrap();
    layout.active_workspace(output).unwrap()
}

#[test]
fn every_connected_display_gets_a_desktop() {
    let layout = two_displays();
    assert!(layout.active_workspace(LEFT).is_some());
    assert!(layout.active_workspace(RIGHT).is_some());
    assert_ne!(
        layout.active_workspace(LEFT),
        layout.active_workspace(RIGHT)
    );
    assert_eq!(layout.workspaces().count(), 2);
}

#[test]
fn arrangement_determines_neighbours() {
    let layout = two_displays();
    assert_eq!(
        layout.output_in_direction(LEFT, Direction::Right),
        Some(RIGHT)
    );
    assert_eq!(
        layout.output_in_direction(RIGHT, Direction::Left),
        Some(LEFT)
    );
    assert_eq!(layout.output_in_direction(LEFT, Direction::Left), None);
    assert_eq!(layout.output_in_direction(LEFT, Direction::Up), None);
}

#[test]
fn a_desktop_moved_to_another_display_takes_its_windows_along() {
    let mut layout = two_displays();
    let ws = add_on(&mut layout, RIGHT, 1);

    layout.show_workspace(ws, LEFT).unwrap();
    layout.validate().unwrap();

    assert_eq!(layout.active_workspace(LEFT), Some(ws));
    assert_eq!(frame(&layout).placement(WindowId(1)).unwrap().output, LEFT);
}

#[test]
fn stealing_a_display_leaves_a_fresh_desktop_behind() {
    let mut layout = two_displays();
    let occupied = add_on(&mut layout, LEFT, 1);
    let ws = add_on(&mut layout, RIGHT, 2);

    layout.show_workspace(ws, LEFT).unwrap();
    layout.validate().unwrap();

    // The display we took it from is not handed the displaced desktop; it gets
    // a new empty one, so a run of moves never shuffles things behind you.
    let backfilled = layout.active_workspace(RIGHT).unwrap();
    assert_ne!(backfilled, ws);
    assert_ne!(backfilled, occupied);
    assert!(layout.workspace(backfilled).unwrap().is_empty());

    // The desktop that was displaced still exists, just off screen.
    assert_eq!(layout.output_showing(occupied), None);
    assert!(layout.workspace(occupied).is_some());
}

#[test]
fn consecutive_moves_onto_one_display_do_not_shuffle_the_others() {
    let mut layout = two_displays();
    let a = add_on(&mut layout, LEFT, 1);
    let b = add_on(&mut layout, RIGHT, 2);
    layout.focus_output(RIGHT).unwrap();
    let c = layout.create_workspace(Some("third".into()));
    layout
        .move_window_to_workspace(WindowId(2), c, false)
        .unwrap();

    // Pull b, then c, onto the left display in turn.
    layout.show_workspace(b, LEFT).unwrap();
    layout.show_workspace(c, LEFT).unwrap();
    layout.validate().unwrap();

    assert_eq!(layout.active_workspace(LEFT), Some(c));
    // `a` and `b` are both parked off screen rather than having been bounced
    // onto the right display by the second move.
    assert_eq!(layout.output_showing(a), None);
    assert_eq!(layout.output_showing(b), None);
}

#[test]
fn an_empty_displaced_desktop_is_reaped() {
    let mut layout = two_displays();
    let empty = layout.active_workspace(LEFT).unwrap();
    let ws = add_on(&mut layout, RIGHT, 1);

    layout.show_workspace(ws, LEFT).unwrap();
    assert!(layout.workspace(empty).is_none());
    layout.validate().unwrap();
}

#[test]
fn swapping_two_displays_exchanges_their_desktops() {
    let mut layout = two_displays();
    let a = add_on(&mut layout, LEFT, 1);
    let b = add_on(&mut layout, RIGHT, 2);

    layout.swap_output_workspaces(LEFT, RIGHT).unwrap();
    layout.validate().unwrap();

    assert_eq!(layout.active_workspace(LEFT), Some(b));
    assert_eq!(layout.active_workspace(RIGHT), Some(a));
}

#[test]
fn unplugging_parks_a_desktop_and_replugging_restores_it() {
    let mut layout = two_displays();
    let ws = add_on(&mut layout, RIGHT, 1);

    layout.reconfigure_outputs(vec![left()]);
    layout.validate().unwrap();
    assert_eq!(layout.output_showing(ws), None);
    // The desktop and its window survive the display going away.
    assert!(layout.workspace(ws).unwrap().contains(WindowId(1)));
    assert_eq!(layout.focused_output(), Some(LEFT));

    layout.reconfigure_outputs(vec![left(), right()]);
    layout.validate().unwrap();
    assert_eq!(layout.active_workspace(RIGHT), Some(ws));
}

#[test]
fn rearranging_displays_moves_the_windows_with_them() {
    let mut layout = two_displays();
    add_on(&mut layout, RIGHT, 1);
    assert_eq!(frame(&layout).placement(WindowId(1)).unwrap().rect.x, 1920);

    // Same displays, mirrored arrangement.
    layout.reconfigure_outputs(vec![
        Output::new(LEFT, "DP-1", Rect::new(1920, 0, 1920, 1080)),
        Output::new(RIGHT, "DP-2", Rect::new(0, 0, 1920, 1080)),
    ]);
    layout.validate().unwrap();
    assert_eq!(frame(&layout).placement(WindowId(1)).unwrap().rect.x, 0);
    assert_eq!(
        layout.output_in_direction(RIGHT, Direction::Right),
        Some(LEFT)
    );
}

#[test]
fn exclusive_zones_shrink_the_tiling_area_but_not_fullscreen() {
    let mut layout = two_displays();
    layout.reconfigure_outputs(vec![
        left().with_work_area(Rect::new(0, 30, 1920, 1050)),
        right(),
    ]);
    add_on(&mut layout, LEFT, 1);

    assert_eq!(
        frame(&layout).placement(WindowId(1)).unwrap().rect,
        Rect::new(0, 30, 1920, 1050)
    );

    layout.set_fullscreen(WindowId(1), true).unwrap();
    assert_eq!(
        frame(&layout).placement(WindowId(1)).unwrap().rect,
        Rect::new(0, 0, 1920, 1080)
    );
}

#[test]
fn floating_windows_follow_their_desktop_across_displays() {
    let mut layout = two_displays();
    let ws = add_on(&mut layout, LEFT, 1);
    layout.set_floating(WindowId(1), true).unwrap();
    let before = frame(&layout).placement(WindowId(1)).unwrap().rect;
    assert!(left().logical.contains_rect(before));

    layout.show_workspace(ws, RIGHT).unwrap();
    layout.validate().unwrap();

    // The rectangle is absolute, so moving the desktop has to carry it: the
    // offset within the display is preserved and it lands on the new one.
    let after = frame(&layout).placement(WindowId(1)).unwrap().rect;
    assert!(
        right().logical.contains_rect(after),
        "{after:?} is not on DP-2"
    );
    assert_eq!(after.size(), before.size());
    assert_eq!(after.x - 1920, before.x);
}

#[test]
fn a_floating_window_sent_to_another_display_arrives_on_it() {
    let mut layout = two_displays();
    add_on(&mut layout, LEFT, 1);
    layout.set_floating(WindowId(1), true).unwrap();
    let elsewhere = layout.active_workspace(RIGHT).unwrap();

    layout
        .move_window_to_workspace(WindowId(1), elsewhere, false)
        .unwrap();
    layout.validate().unwrap();

    let rect = frame(&layout).placement(WindowId(1)).unwrap().rect;
    assert!(
        right().logical.contains_rect(rect),
        "{rect:?} is not on DP-2"
    );
}

#[test]
fn focus_crosses_displays() {
    let mut layout = two_displays();
    add_on(&mut layout, LEFT, 1);
    add_on(&mut layout, RIGHT, 2);
    layout.focus_window(WindowId(1)).unwrap();

    layout.focus_direction(Direction::Right).unwrap();
    assert_eq!(layout.focused_window(), Some(WindowId(2)));
    assert_eq!(layout.focused_output(), Some(RIGHT));

    layout.focus_direction(Direction::Left).unwrap();
    assert_eq!(layout.focused_window(), Some(WindowId(1)));
    assert_eq!(layout.focused_output(), Some(LEFT));

    // Nothing further left; focus stays put rather than wrapping.
    layout.focus_direction(Direction::Left).unwrap();
    assert_eq!(layout.focused_window(), Some(WindowId(1)));
}

#[test]
fn a_window_pushed_off_one_display_enters_the_next_at_the_near_edge() {
    let mut layout = two_displays();
    add_on(&mut layout, LEFT, 1);
    let dest = add_on(&mut layout, RIGHT, 2);

    layout
        .move_window_direction(WindowId(1), Direction::Right)
        .unwrap();
    layout.validate().unwrap();

    assert_eq!(layout.workspace_of(WindowId(1)), Some(dest));
    let f = frame(&layout);
    let moved = f.placement(WindowId(1)).unwrap();
    let resident = f.placement(WindowId(2)).unwrap();
    assert_eq!(moved.output, RIGHT);
    // It arrived from the left, so it sits to the left of what was there.
    assert!(moved.rect.x < resident.rect.x);
    assert_eq!(moved.rect.x, 1920);
    // Focus followed it across.
    assert_eq!(layout.focused_output(), Some(RIGHT));
    assert_eq!(layout.focused_window(), Some(WindowId(1)));
}

#[test]
fn a_window_at_the_outer_edge_stays_put() {
    let mut layout = two_displays();
    add_on(&mut layout, LEFT, 1);
    let events = layout
        .move_window_direction(WindowId(1), Direction::Left)
        .unwrap();
    assert!(events.is_empty());
    assert_eq!(layout.frame_output_of(WindowId(1)), Some(LEFT));
}

#[test]
fn a_displayed_desktop_survives_losing_its_last_window() {
    let mut layout = two_displays();
    let source = add_on(&mut layout, LEFT, 1);
    add_on(&mut layout, RIGHT, 2);

    layout
        .move_window_direction(WindowId(1), Direction::Right)
        .unwrap();
    layout.validate().unwrap();

    // Emptying a desktop that is on screen leaves it on screen. Only desktops
    // that are both empty and hidden are reaped, so a display never flickers
    // through a new workspace id just because you moved the last window off it.
    assert_eq!(layout.active_workspace(LEFT), Some(source));
    assert!(layout.workspace(source).unwrap().is_empty());
}

#[test]
fn windows_can_be_sent_to_a_hidden_desktop_and_followed() {
    let mut layout = two_displays();
    add_on(&mut layout, LEFT, 1);
    add_on(&mut layout, LEFT, 2);
    let elsewhere = layout.create_workspace(Some("scratch".into()));

    layout
        .move_window_to_workspace(WindowId(2), elsewhere, false)
        .unwrap();
    layout.validate().unwrap();
    assert_eq!(layout.workspace_of(WindowId(2)), Some(elsewhere));
    // Not displayed anywhere yet, so it is absent from the frame.
    assert!(frame(&layout).placement(WindowId(2)).is_none());

    // Focusing it pulls its desktop onto the focused display.
    layout.focus_window(WindowId(2)).unwrap();
    layout.validate().unwrap();
    assert_eq!(layout.active_workspace(LEFT), Some(elsewhere));
    assert!(frame(&layout).placement(WindowId(2)).is_some());
}

#[test]
fn a_headless_session_puts_its_windows_up_when_a_display_appears() {
    let mut layout = Layout::new(Config::default());
    layout
        .add_window(WindowId(1), None, InsertTarget::default())
        .unwrap();
    layout.validate().unwrap();
    assert!(frame(&layout).placements.is_empty());

    layout.reconfigure_outputs(vec![left()]);
    layout.validate().unwrap();
    assert_eq!(frame(&layout).placement(WindowId(1)).unwrap().output, LEFT);
}

#[test]
fn commands_round_trip_through_json_and_apply() {
    let mut layout = two_displays();
    let command = Command::AddWindow {
        window: WindowId(7),
        workspace: None,
        target: InsertTarget::default(),
    };
    let wire = serde_json::to_string(&command).unwrap();
    let decoded: Command = serde_json::from_str(&wire).unwrap();
    assert_eq!(decoded, command);
    dispatch(&mut layout, decoded).unwrap();
    assert!(frame(&layout).placement(WindowId(7)).is_some());

    let snapshot = serde_json::to_string(&layout).unwrap();
    let restored: Layout = serde_json::from_str(&snapshot).unwrap();
    restored.validate().unwrap();
    assert_eq!(frame(&restored), frame(&layout));
}

/// Test-local convenience: which display a window is currently drawn on.
trait FrameOutput {
    fn frame_output_of(&self, window: WindowId) -> Option<OutputId>;
}

impl FrameOutput for Layout {
    fn frame_output_of(&self, window: WindowId) -> Option<OutputId> {
        frame(self).placement(window).map(|p| p.output)
    }
}

#[test]
fn a_point_on_a_display_is_left_alone() {
    let layout = two_displays();
    for point in [
        irontile_layout::Point::new(0, 0),
        irontile_layout::Point::new(1919, 1079),
        irontile_layout::Point::new(2000, 500),
    ] {
        assert_eq!(layout.clamp_to_outputs(point), point, "{point:?}");
    }
}

#[test]
fn a_point_off_the_arrangement_is_pulled_back() {
    let layout = two_displays();
    // The pointer moves by deltas, so nothing but this stops it walking off
    // the side of the desk entirely.
    let far_right = layout.clamp_to_outputs(irontile_layout::Point::new(99_999, 500));
    assert_eq!(far_right, irontile_layout::Point::new(3839, 500));

    let above = layout.clamp_to_outputs(irontile_layout::Point::new(400, -50));
    assert_eq!(above, irontile_layout::Point::new(400, 0));

    let below = layout.clamp_to_outputs(irontile_layout::Point::new(400, 99_999));
    assert_eq!(below, irontile_layout::Point::new(400, 1079));
}

#[test]
fn a_point_in_the_gap_between_displays_goes_to_the_nearer_one() {
    let mut layout = Layout::new(Config::default());
    // Two displays that do not touch, which a real desk often has.
    layout.reconfigure_outputs(vec![
        Output::new(LEFT, "DP-1", Rect::new(0, 0, 800, 600)),
        Output::new(RIGHT, "DP-2", Rect::new(1200, 0, 800, 600)),
    ]);

    let near_left = layout.clamp_to_outputs(irontile_layout::Point::new(850, 300));
    assert_eq!(near_left, irontile_layout::Point::new(799, 300));

    let near_right = layout.clamp_to_outputs(irontile_layout::Point::new(1150, 300));
    assert_eq!(near_right, irontile_layout::Point::new(1200, 300));
}

#[test]
fn clamping_with_no_displays_is_a_no_op() {
    let layout = Layout::new(Config::default());
    let point = irontile_layout::Point::new(10, 20);
    assert_eq!(layout.clamp_to_outputs(point), point);
}
