//! Property tests for the two guarantees the layout engine rests on:
//!
//! - the rectangles a tree produces are pairwise non-overlapping, contained in
//!   the area, and exactly cover it;
//! - the tree and the desktop model stay structurally valid under arbitrary
//!   sequences of operations.

use irontile_layout::{
    Axis, Config, Direction, InsertTarget, Layout, Output, OutputId, Params, Rect, Tree, WindowId,
    WorkspaceId, apply, apply_with, frame,
};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Tree operations
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum TreeOp {
    /// Focus an existing window, then insert a new one relative to it.
    Insert {
        anchor: usize,
        axis: Option<Axis>,
    },
    Remove {
        target: usize,
    },
    Move {
        target: usize,
        dir: Direction,
    },
    Swap {
        a: usize,
        b: usize,
    },
    Resize {
        target: usize,
        dir: Direction,
        delta: i32,
    },
    Focus {
        target: usize,
    },
    ToggleAxis {
        target: usize,
    },
    Equalize {
        target: usize,
    },
    DissolveParent {
        target: usize,
    },
}

fn any_axis() -> impl Strategy<Value = Option<Axis>> {
    prop_oneof![
        Just(None),
        Just(Some(Axis::Horizontal)),
        Just(Some(Axis::Vertical)),
    ]
}

fn any_dir() -> impl Strategy<Value = Direction> {
    prop_oneof![
        Just(Direction::Left),
        Just(Direction::Right),
        Just(Direction::Up),
        Just(Direction::Down),
    ]
}

fn any_tree_op() -> impl Strategy<Value = TreeOp> {
    prop_oneof![
        // Weighted towards insertion so sequences build trees of real depth
        // instead of thrashing around one or two windows.
        4 => (any::<u16>(), any_axis())
            .prop_map(|(anchor, axis)| TreeOp::Insert { anchor: anchor as usize, axis }),
        2 => any::<u16>().prop_map(|t| TreeOp::Remove { target: t as usize }),
        3 => (any::<u16>(), any_dir())
            .prop_map(|(t, dir)| TreeOp::Move { target: t as usize, dir }),
        1 => (any::<u16>(), any::<u16>())
            .prop_map(|(a, b)| TreeOp::Swap { a: a as usize, b: b as usize }),
        2 => (any::<u16>(), any_dir(), -400i32..400)
            .prop_map(|(t, dir, delta)| TreeOp::Resize { target: t as usize, dir, delta }),
        1 => any::<u16>().prop_map(|t| TreeOp::Focus { target: t as usize }),
        1 => any::<u16>().prop_map(|t| TreeOp::ToggleAxis { target: t as usize }),
        1 => any::<u16>().prop_map(|t| TreeOp::Equalize { target: t as usize }),
        1 => any::<u16>().prop_map(|t| TreeOp::DissolveParent { target: t as usize }),
    ]
}

fn any_rect() -> impl Strategy<Value = Rect> {
    (-2000i32..2000, -2000i32..2000, 1i32..4000, 1i32..4000)
        .prop_map(|(x, y, w, h)| Rect::new(x, y, w, h))
}

/// Replays a sequence against a tree, checking validity after every step.
///
/// Operations that cannot apply to the current shape are skipped rather than
/// asserted on: the point is that no reachable sequence breaks the tree, not
/// that every randomly generated step is meaningful.
fn replay_tree(ops: &[TreeOp], area: Rect) -> Tree {
    let mut tree = Tree::new();
    let mut next = 0u64;

    for op in ops {
        let windows: Vec<WindowId> = tree.leaves().into_iter().map(|(_, w)| w).collect();
        let pick = |i: usize| windows.get(i % windows.len().max(1)).copied();

        match *op {
            TreeOp::Insert { anchor, axis } => {
                if let Some(w) = pick(anchor) {
                    let node = tree.node_of(w).expect("listed window has a node");
                    tree.set_focus(node).expect("listed node is live");
                }
                tree.insert(WindowId(next), InsertTarget::Focused { axis })
                    .expect("a fresh window id is always insertable");
                next += 1;
            }
            TreeOp::Remove { target } => {
                if let Some(w) = pick(target) {
                    tree.remove(w).expect("listed window is removable");
                }
            }
            TreeOp::Move { target, dir } => {
                if let Some(node) = pick(target).and_then(|w| tree.node_of(w)) {
                    let _ = tree.move_node(node, dir);
                }
            }
            TreeOp::Swap { a, b } => {
                if let (Some(na), Some(nb)) = (
                    pick(a).and_then(|w| tree.node_of(w)),
                    pick(b).and_then(|w| tree.node_of(w)),
                ) {
                    let _ = tree.swap(na, nb);
                }
            }
            TreeOp::Resize { target, dir, delta } => {
                if let Some(node) = pick(target).and_then(|w| tree.node_of(w)) {
                    let _ = tree.resize(node, dir, delta, area, &Params::ZERO);
                }
            }
            TreeOp::Focus { target } => {
                if let Some(node) = pick(target).and_then(|w| tree.node_of(w)) {
                    tree.set_focus(node).expect("listed node is live");
                }
            }
            TreeOp::ToggleAxis { target } => {
                if let Some(parent) = pick(target)
                    .and_then(|w| tree.node_of(w))
                    .and_then(|n| tree.parent(n))
                {
                    let _ = tree.toggle_axis(parent);
                }
            }
            TreeOp::Equalize { target } => {
                if let Some(parent) = pick(target)
                    .and_then(|w| tree.node_of(w))
                    .and_then(|n| tree.parent(n))
                {
                    let _ = tree.equalize(parent);
                }
            }
            TreeOp::DissolveParent { target } => {
                if let Some(parent) = pick(target)
                    .and_then(|w| tree.node_of(w))
                    .and_then(|n| tree.parent(n))
                {
                    let _ = tree.dissolve(parent);
                }
            }
        }

        tree.validate()
            .expect("tree stays valid after every operation");
    }
    tree
}

/// Every window placed exactly once, no two rectangles overlapping, everything
/// inside the area, and the areas summing to the whole: together these say the
/// output tiles the input exactly.
fn assert_tiles_exactly(tree: &Tree, area: Rect) {
    let placed = apply(tree, area);
    assert_eq!(
        placed.len(),
        tree.len(),
        "every window is placed exactly once"
    );

    if tree.is_empty() {
        // Nothing to tile with; an empty desktop covers nothing by definition.
        assert!(placed.is_empty());
        return;
    }

    let mut windows: Vec<WindowId> = placed.iter().map(|(w, _)| *w).collect();
    windows.sort_unstable();
    windows.dedup();
    assert_eq!(windows.len(), placed.len(), "no window is placed twice");

    for (w, rect) in &placed {
        assert!(
            area.contains_rect(*rect),
            "{w} at {rect:?} escapes {area:?}"
        );
    }
    for (i, (wa, a)) in placed.iter().enumerate() {
        for (wb, b) in &placed[i + 1..] {
            assert!(!a.intersects(*b), "{wa} at {a:?} overlaps {wb} at {b:?}");
        }
    }
    let covered: i64 = placed.iter().map(|(_, r)| r.area()).sum();
    assert_eq!(covered, area.area(), "the placements leave no gap");
}

/// With gaps the cover is deliberately incomplete, but nothing may overlap or
/// escape.
fn assert_disjoint_within(tree: &Tree, area: Rect, params: &Params) {
    let placed = apply_with(tree, area, params);
    assert_eq!(placed.len(), tree.len());
    for (w, rect) in &placed {
        assert!(
            area.contains_rect(*rect),
            "{w} at {rect:?} escapes {area:?}"
        );
    }
    for (i, (wa, a)) in placed.iter().enumerate() {
        for (wb, b) in &placed[i + 1..] {
            assert!(!a.intersects(*b), "{wa} at {a:?} overlaps {wb} at {b:?}");
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    /// The headline guarantee, over arbitrary trees and arbitrary areas.
    #[test]
    fn placements_tile_the_area_exactly(
        ops in prop::collection::vec(any_tree_op(), 0..40),
        area in any_rect(),
        other in any_rect(),
    ) {
        let tree = replay_tree(&ops, area);
        assert_tiles_exactly(&tree, area);
        // The same tree laid out somewhere else entirely must hold up too;
        // nothing may depend on the area a tree was built against.
        assert_tiles_exactly(&tree, other);
    }

    #[test]
    fn gaps_never_cause_overlap_or_overflow(
        ops in prop::collection::vec(any_tree_op(), 0..30),
        area in any_rect(),
        outer in 0i32..40,
        inner in 0i32..40,
    ) {
        let tree = replay_tree(&ops, area);
        assert_disjoint_within(&tree, area, &Params::with_gaps(outer, inner));
    }

    /// `validate` is called inside the replay after every step; this also
    /// pins the window count and the depth-first ordering agreeing with `apply`.
    #[test]
    fn the_tree_survives_arbitrary_sequences(
        ops in prop::collection::vec(any_tree_op(), 0..60),
        area in any_rect(),
    ) {
        let tree = replay_tree(&ops, area);
        tree.validate().unwrap();

        let leaves: Vec<WindowId> = tree.leaves().into_iter().map(|(_, w)| w).collect();
        let placed: Vec<WindowId> = apply(&tree, area).into_iter().map(|(w, _)| w).collect();
        prop_assert_eq!(leaves, placed);
    }

    /// Serializing and reloading a tree is the identity, which is what the
    /// eventual component boundary depends on.
    #[test]
    fn trees_round_trip_through_serialization(
        ops in prop::collection::vec(any_tree_op(), 0..30),
        area in any_rect(),
    ) {
        let tree = replay_tree(&ops, area);
        let json = serde_json::to_string(&tree).unwrap();
        let decoded: Tree = serde_json::from_str(&json).unwrap();
        decoded.validate().unwrap();
        prop_assert_eq!(&decoded, &tree);
        prop_assert_eq!(apply(&decoded, area), apply(&tree, area));
    }

    /// A move within a container is exactly reversible.
    ///
    /// A move that escapes a container is not, and should not be: the window
    /// has left a group, and pushing it back puts it beside that group rather
    /// than inside it. So the round trip is only asserted when the window
    /// stayed under the same parent, and in the escaping case the weaker
    /// guarantees — the reverse move is available, and no window is lost —
    /// are checked instead.
    #[test]
    fn moving_a_window_and_back_restores_the_order(
        ops in prop::collection::vec(any_tree_op(), 1..25),
        area in any_rect(),
        target in any::<u16>(),
        dir in any_dir(),
    ) {
        let mut tree = replay_tree(&ops, area);
        let windows: Vec<WindowId> = tree.leaves().into_iter().map(|(_, w)| w).collect();
        prop_assume!(!windows.is_empty());
        let window = windows[target as usize % windows.len()];

        let before = apply(&tree, area);
        let node = tree.node_of(window).unwrap();
        let parent_before = tree.parent(node);
        if tree.move_node(node, dir).is_ok() {
            tree.validate().unwrap();
            let node = tree.node_of(window).unwrap();
            let reordered = tree.parent(node) == parent_before;

            prop_assert!(tree.move_node(node, dir.opposite()).is_ok());
            tree.validate().unwrap();

            if reordered {
                prop_assert_eq!(apply(&tree, area), before);
            } else {
                let after: Vec<WindowId> =
                    tree.leaves().into_iter().map(|(_, w)| w).collect();
                let mut sorted = after.clone();
                sorted.sort_unstable();
                let mut expected = windows.clone();
                expected.sort_unstable();
                prop_assert_eq!(sorted, expected, "escaping a container loses no window");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Desktop model
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum LayoutOp {
    AddWindow,
    RemoveWindow { target: usize },
    FocusDirection { dir: Direction },
    MoveDirection { target: usize, dir: Direction },
    MoveToWorkspace { target: usize, workspace: usize },
    ShowWorkspace { workspace: usize, output: usize },
    SwapOutputs,
    CreateWorkspace,
    DestroyWorkspace { workspace: usize },
    ToggleFloating { target: usize },
    ToggleFullscreen { target: usize },
    FocusOutput { output: usize },
    Reconfigure { count: usize },
}

fn any_layout_op() -> impl Strategy<Value = LayoutOp> {
    prop_oneof![
        5 => Just(LayoutOp::AddWindow),
        2 => any::<u16>().prop_map(|t| LayoutOp::RemoveWindow { target: t as usize }),
        2 => any_dir().prop_map(|dir| LayoutOp::FocusDirection { dir }),
        3 => (any::<u16>(), any_dir())
            .prop_map(|(t, dir)| LayoutOp::MoveDirection { target: t as usize, dir }),
        2 => (any::<u16>(), any::<u16>())
            .prop_map(|(t, w)| LayoutOp::MoveToWorkspace { target: t as usize, workspace: w as usize }),
        3 => (any::<u16>(), any::<u16>())
            .prop_map(|(w, o)| LayoutOp::ShowWorkspace { workspace: w as usize, output: o as usize }),
        1 => Just(LayoutOp::SwapOutputs),
        1 => Just(LayoutOp::CreateWorkspace),
        1 => any::<u16>().prop_map(|w| LayoutOp::DestroyWorkspace { workspace: w as usize }),
        1 => any::<u16>().prop_map(|t| LayoutOp::ToggleFloating { target: t as usize }),
        1 => any::<u16>().prop_map(|t| LayoutOp::ToggleFullscreen { target: t as usize }),
        1 => any::<u16>().prop_map(|o| LayoutOp::FocusOutput { output: o as usize }),
        2 => (0usize..4).prop_map(|count| LayoutOp::Reconfigure { count }),
    ]
}

/// A row of `count` displays, side by side.
fn displays(count: usize) -> Vec<Output> {
    (0..count)
        .map(|i| {
            let x = (i as i32) * 1920;
            Output::new(
                OutputId(i as u64 + 1),
                format!("DP-{i}"),
                Rect::new(x, 0, 1920, 1080),
            )
        })
        .collect()
}

fn replay_layout(ops: &[LayoutOp]) -> Layout {
    let mut layout = Layout::new(Config::default());
    layout.reconfigure_outputs(displays(2));
    let mut next = 0u64;

    for op in ops {
        let windows: Vec<WindowId> = layout.workspaces().flat_map(|w| w.windows()).collect();
        let workspaces: Vec<WorkspaceId> = layout.workspaces().map(|w| w.id).collect();
        let outputs: Vec<OutputId> = layout.outputs().iter().map(|o| o.id).collect();
        let pick_window = |i: usize| windows.get(i % windows.len().max(1)).copied();
        let pick_workspace = |i: usize| workspaces.get(i % workspaces.len().max(1)).copied();
        let pick_output = |i: usize| outputs.get(i % outputs.len().max(1)).copied();

        match *op {
            LayoutOp::AddWindow => {
                layout
                    .add_window(WindowId(next), None, InsertTarget::default())
                    .expect("a fresh window id is always addable");
                next += 1;
            }
            LayoutOp::RemoveWindow { target } => {
                if let Some(w) = pick_window(target) {
                    layout
                        .remove_window(w)
                        .expect("a managed window is removable");
                }
            }
            LayoutOp::FocusDirection { dir } => {
                layout.focus_direction(dir).expect("focus never fails");
            }
            LayoutOp::MoveDirection { target, dir } => {
                if let Some(w) = pick_window(target) {
                    layout
                        .move_window_direction(w, dir)
                        .expect("move never fails");
                }
            }
            LayoutOp::MoveToWorkspace { target, workspace } => {
                if let (Some(w), Some(ws)) = (pick_window(target), pick_workspace(workspace)) {
                    layout
                        .move_window_to_workspace(w, ws, false)
                        .expect("move never fails");
                }
            }
            LayoutOp::ShowWorkspace { workspace, output } => {
                if let (Some(ws), Some(o)) = (pick_workspace(workspace), pick_output(output)) {
                    layout.show_workspace(ws, o).expect("both exist");
                }
            }
            LayoutOp::SwapOutputs => {
                if let (Some(a), Some(b)) = (pick_output(0), pick_output(1)) {
                    layout.swap_output_workspaces(a, b).expect("both exist");
                }
            }
            LayoutOp::CreateWorkspace => {
                layout.create_workspace(None);
            }
            LayoutOp::DestroyWorkspace { workspace } => {
                if let Some(ws) = pick_workspace(workspace) {
                    // Refused for a populated desktop, which is the point.
                    let _ = layout.destroy_workspace(ws);
                }
            }
            LayoutOp::ToggleFloating { target } => {
                if let Some(w) = pick_window(target) {
                    let floating = layout
                        .workspace_of(w)
                        .and_then(|ws| layout.workspace(ws))
                        .is_some_and(|ws| ws.is_floating(w));
                    layout.set_floating(w, !floating).expect("managed window");
                }
            }
            LayoutOp::ToggleFullscreen { target } => {
                if let Some(w) = pick_window(target) {
                    let full = layout
                        .workspace_of(w)
                        .and_then(|ws| layout.workspace(ws))
                        .is_some_and(|ws| ws.fullscreen == Some(w));
                    layout.set_fullscreen(w, !full).expect("managed window");
                }
            }
            LayoutOp::FocusOutput { output } => {
                if let Some(o) = pick_output(output) {
                    layout.focus_output(o).expect("connected output");
                }
            }
            LayoutOp::Reconfigure { count } => {
                layout.reconfigure_outputs(displays(count));
            }
        }

        layout.validate().expect("the desktop model stays valid");
    }
    layout
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 300, ..ProptestConfig::default() })]

    #[test]
    fn the_desktop_model_survives_arbitrary_sequences(
        ops in prop::collection::vec(any_layout_op(), 0..50),
    ) {
        let layout = replay_layout(&ops);
        layout.validate().unwrap();

        // Every display is showing exactly one desktop, and no desktop is on
        // two displays at once.
        let mut shown: Vec<WorkspaceId> = layout
            .outputs()
            .iter()
            .map(|o| layout.active_workspace(o.id).expect("every display shows a desktop"))
            .collect();
        let count = shown.len();
        shown.sort_unstable();
        shown.dedup();
        prop_assert_eq!(shown.len(), count);
    }

    /// Whatever the desktop model has been through, the frame it produces is
    /// still coherent: every placed window is managed, placed once, and tiled
    /// windows on a display do not overlap.
    #[test]
    fn the_frame_stays_coherent(
        ops in prop::collection::vec(any_layout_op(), 0..50),
    ) {
        let layout = replay_layout(&ops);
        let f = frame(&layout);

        let mut placed: Vec<WindowId> = f.placements.iter().map(|p| p.window).collect();
        let total = placed.len();
        placed.sort_unstable();
        placed.dedup();
        prop_assert_eq!(placed.len(), total, "a window is placed at most once");

        for p in &f.placements {
            prop_assert!(layout.workspace_of(p.window).is_some(), "placed window is managed");
            prop_assert_eq!(layout.workspace_of(p.window), Some(p.workspace));
            let output = layout.output(p.output).expect("placed on a connected display");
            // Tiled and fullscreen rectangles are the engine's own work and must
            // stay on their display. A floating rectangle is the user's, and a
            // window dragged half off screen is legitimate.
            if p.kind != irontile_layout::PlacementKind::Floating {
                prop_assert!(output.logical.contains_rect(p.rect) || p.rect.is_empty());
            }
        }

        for output in layout.outputs() {
            let tiled: Vec<Rect> = f
                .on_output(output.id)
                .filter(|p| p.kind == irontile_layout::PlacementKind::Tiled)
                .map(|p| p.rect)
                .collect();
            for (i, a) in tiled.iter().enumerate() {
                for b in &tiled[i + 1..] {
                    prop_assert!(!a.intersects(*b), "{:?} overlaps {:?}", a, b);
                }
            }
        }

        if let Some(w) = f.focused {
            prop_assert!(layout.workspace_of(w).is_some());
        }
    }

    /// A desktop sent to another display arrives intact, with the same windows
    /// in the same tree order.
    #[test]
    fn transferring_a_desktop_preserves_its_contents(
        ops in prop::collection::vec(any_layout_op(), 0..30),
        workspace in any::<u16>(),
        output in any::<u16>(),
    ) {
        let mut layout = replay_layout(&ops);
        let workspaces: Vec<WorkspaceId> = layout.workspaces().map(|w| w.id).collect();
        let outputs: Vec<OutputId> = layout.outputs().iter().map(|o| o.id).collect();
        prop_assume!(!workspaces.is_empty() && !outputs.is_empty());

        let ws = workspaces[workspace as usize % workspaces.len()];
        let target = outputs[output as usize % outputs.len()];
        let before = layout.workspace(ws).unwrap().windows();

        layout.show_workspace(ws, target).unwrap();
        layout.validate().unwrap();

        prop_assert_eq!(layout.active_workspace(target), Some(ws));
        prop_assert_eq!(layout.workspace(ws).unwrap().windows(), before);
        // And every display still has something on it.
        for o in layout.outputs() {
            prop_assert!(layout.active_workspace(o.id).is_some());
        }
    }
}
