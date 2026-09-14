//! The desktop model: displays, desktops, and focus.
//!
//! [`Layout`] is the aggregate root. It owns an arrangement of displays, an
//! unbounded set of desktops, and the binding between them: each connected
//! display shows exactly one desktop, and every desktop remembers which display
//! it would rather be on so that unplugging and replugging a monitor restores
//! what was there.
//!
//! Moving a desktop to a display *steals* that display: the desktop that was
//! there becomes hidden rather than being pushed back the other way, so moving
//! several desktops onto one monitor in a row does what you meant each time. A
//! display left showing nothing is given a fresh desktop.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::command::Event;
use crate::error::{LayoutError, LayoutInvariant};
use crate::geom::{Axis, Direction, Point, Rect};
use crate::id::{OutputId, WindowId, WorkspaceId};
use crate::output::Output;
use crate::tiling::{Params, apply_with, geometry, pick_direction};
use crate::tree::InsertTarget;
use crate::workspace::{Floating, Workspace};

/// Policy knobs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub params: Params,
    /// Axis used when a desktop holds a single window and no axis was
    /// requested. Only consulted when `smart_split` is off.
    pub default_axis: Axis,
    /// Split a window along its longer edge instead of appending to whatever
    /// container the focus is already in. This is what produces a dwindling
    /// layout rather than an ever-growing row.
    pub smart_split: bool,
    /// Destroy a desktop once it is empty and no longer displayed.
    pub reap_empty_workspaces: bool,
    /// Follow a window that is moved to another desktop or display.
    pub focus_follows_move: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            params: Params::ZERO,
            default_axis: Axis::Horizontal,
            smart_split: true,
            reap_empty_workspaces: true,
            focus_follows_move: true,
        }
    }
}

/// Displays, desktops, and what is on what.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    config: Config,
    /// Connected displays, in the order the compositor reported them.
    outputs: Vec<Output>,
    workspaces: BTreeMap<WorkspaceId, Workspace>,
    /// Which desktop each display is showing. A desktop appears here at most
    /// once.
    shown: BTreeMap<OutputId, WorkspaceId>,
    focused_output: Option<OutputId>,
    next_workspace: u64,
    /// Reverse index from window to the desktop holding it.
    index: BTreeMap<WindowId, WorkspaceId>,
}

impl Layout {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    // ---- displays -------------------------------------------------------

    pub fn outputs(&self) -> &[Output] {
        &self.outputs
    }

    pub fn output(&self, id: OutputId) -> Option<&Output> {
        self.outputs.iter().find(|o| o.id == id)
    }

    pub fn output_named(&self, name: &str) -> Option<&Output> {
        self.outputs.iter().find(|o| o.name == name)
    }

    pub fn focused_output(&self) -> Option<OutputId> {
        self.focused_output
    }

    /// The display whose rectangle lies nearest in `dir`.
    pub fn output_in_direction(&self, from: OutputId, dir: Direction) -> Option<OutputId> {
        let origin = self.output(from)?.logical;
        let candidates: Vec<(OutputId, Rect)> = self
            .outputs
            .iter()
            .filter(|o| o.id != from)
            .map(|o| (o.id, o.logical))
            .collect();
        pick_direction(origin, &candidates, dir)
    }

    /// Pulls a point back onto the nearest display.
    ///
    /// The pointer moves by deltas, so nothing stops it walking off the side of
    /// the arrangement or into the gap between two displays that do not touch.
    /// Points already on a display are returned unchanged.
    pub fn clamp_to_outputs(&self, point: Point) -> Point {
        if self.output_at(point).is_some() {
            return point;
        }
        let nearest = self
            .outputs
            .iter()
            .min_by_key(|output| squared_distance(output.logical, point));
        match nearest {
            Some(output) => {
                let area = output.logical;
                // The far edges are exclusive, so the last point actually on
                // the display is one short of them.
                Point::new(
                    point.x.clamp(area.x, (area.right() - 1).max(area.x)),
                    point.y.clamp(area.y, (area.bottom() - 1).max(area.y)),
                )
            }
            None => point,
        }
    }

    pub fn output_at(&self, p: Point) -> Option<OutputId> {
        self.outputs
            .iter()
            .find(|o| o.logical.contains(p))
            .map(|o| o.id)
    }

    pub fn focus_output(&mut self, id: OutputId) -> Result<Vec<Event>, LayoutError> {
        if self.output(id).is_none() {
            return Err(LayoutError::UnknownOutput(id));
        }
        self.focused_output = Some(id);
        Ok(vec![Event::FocusChanged {
            window: self.focused_window(),
            output: Some(id),
        }])
    }

    pub fn focus_output_direction(&mut self, dir: Direction) -> Result<Vec<Event>, LayoutError> {
        let from = self.focused_output.ok_or(LayoutError::NoOutputs)?;
        match self.output_in_direction(from, dir) {
            Some(next) => self.focus_output(next),
            None => Ok(Vec::new()),
        }
    }

    /// Replaces the display arrangement wholesale.
    ///
    /// This is the single entry point for hotplug, rearrangement, and exclusive
    /// zone changes. Desktops on departing displays survive as hidden desktops
    /// and keep pointing at the display they came from, so reconnecting a
    /// monitor puts back what was on it.
    pub fn reconfigure_outputs(&mut self, outputs: Vec<Output>) -> Vec<Event> {
        let mut events = Vec::new();
        let areas = self.area_snapshot();

        let mut seen = BTreeSet::new();
        let before = std::mem::replace(
            &mut self.outputs,
            outputs.into_iter().filter(|o| seen.insert(o.id)).collect(),
        );
        let connected: BTreeSet<OutputId> = self.outputs.iter().map(|o| o.id).collect();

        let departed: Vec<OutputId> = self
            .shown
            .keys()
            .copied()
            .filter(|id| !connected.contains(id))
            .collect();
        for id in departed {
            if let Some(ws) = self.shown.remove(&id) {
                events.push(Event::WorkspaceHidden { workspace: ws });
            }
        }

        let ids: Vec<OutputId> = self.outputs.iter().map(|o| o.id).collect();
        for id in ids {
            if !self.shown.contains_key(&id) {
                self.backfill(id, &mut events);
            }
        }

        self.reanchor_floating(&areas);

        if self.focused_output.is_none_or(|f| !connected.contains(&f)) {
            self.focused_output = self.outputs.first().map(|o| o.id);
            events.push(Event::FocusChanged {
                window: self.focused_window(),
                output: self.focused_output,
            });
        }
        // Only when the arrangement is actually different. The compositor
        // republishes on anything that could have moved a work area -- every
        // commit from a bar among them -- and saying "the displays changed" to
        // a client that redraws when it hears it is a loop that never settles.
        if self.outputs != before {
            events.push(Event::OutputsChanged);
        }
        events
    }

    // ---- desktops -------------------------------------------------------

    pub fn workspaces(&self) -> impl Iterator<Item = &Workspace> + '_ {
        self.workspaces.values()
    }

    pub fn workspace(&self, id: WorkspaceId) -> Option<&Workspace> {
        self.workspaces.get(&id)
    }

    pub fn workspace_named(&self, name: &str) -> Option<&Workspace> {
        self.workspaces
            .values()
            .find(|w| w.name.as_deref() == Some(name))
    }

    /// The desktop `on` is currently showing.
    pub fn active_workspace(&self, on: OutputId) -> Option<WorkspaceId> {
        self.shown.get(&on).copied()
    }

    /// The display showing `ws`, if it is displayed at all.
    pub fn output_showing(&self, ws: WorkspaceId) -> Option<OutputId> {
        self.shown.iter().find(|&(_, &v)| v == ws).map(|(&k, _)| k)
    }

    pub fn focused_workspace(&self) -> Option<WorkspaceId> {
        self.focused_output.and_then(|o| self.active_workspace(o))
    }

    pub fn workspace_of(&self, window: WindowId) -> Option<WorkspaceId> {
        self.index.get(&window).copied()
    }

    pub fn create_workspace(&mut self, name: Option<String>) -> WorkspaceId {
        let id = WorkspaceId(self.next_workspace);
        self.next_workspace += 1;
        let mut ws = Workspace::new(id);
        ws.name = name;
        self.workspaces.insert(id, ws);
        id
    }

    pub fn rename_workspace(
        &mut self,
        ws: WorkspaceId,
        name: Option<String>,
    ) -> Result<Vec<Event>, LayoutError> {
        let workspace = self
            .workspaces
            .get_mut(&ws)
            .ok_or(LayoutError::UnknownWorkspace(ws))?;
        workspace.name = name;
        Ok(vec![Event::WorkspaceRenamed { workspace: ws }])
    }

    /// Destroys an empty desktop, backfilling its display if it had one.
    pub fn destroy_workspace(&mut self, ws: WorkspaceId) -> Result<Vec<Event>, LayoutError> {
        let workspace = self
            .workspaces
            .get(&ws)
            .ok_or(LayoutError::UnknownWorkspace(ws))?;
        if !workspace.is_empty() {
            return Err(LayoutError::WorkspaceNotEmpty(ws));
        }
        let mut events = Vec::new();
        let on = self.output_showing(ws);
        self.workspaces.remove(&ws);
        if let Some(o) = on {
            self.shown.remove(&o);
            self.backfill(o, &mut events);
        }
        events.push(Event::WorkspaceDestroyed { workspace: ws });
        Ok(events)
    }

    /// Displays `ws` on `on`, taking the display over.
    ///
    /// Whatever was on `on` becomes hidden. If `ws` was being shown somewhere
    /// else, that display is left blank and gets a fresh desktop rather than
    /// inheriting the displaced one — so pulling several desktops onto one
    /// monitor in sequence never shuffles the others around behind you.
    pub fn show_workspace(
        &mut self,
        ws: WorkspaceId,
        on: OutputId,
    ) -> Result<Vec<Event>, LayoutError> {
        if !self.workspaces.contains_key(&ws) {
            return Err(LayoutError::UnknownWorkspace(ws));
        }
        if self.output(on).is_none() {
            return Err(LayoutError::UnknownOutput(on));
        }
        if self.shown.get(&on) == Some(&ws) {
            self.prefer(ws, on);
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        let areas = self.area_snapshot();
        let vacated = self.output_showing(ws);
        if let Some(v) = vacated {
            self.shown.remove(&v);
        }
        let displaced = self.shown.insert(on, ws);
        self.prefer(ws, on);
        events.push(Event::WorkspaceShown {
            workspace: ws,
            output: on,
        });
        if let Some(d) = displaced {
            events.push(Event::WorkspaceHidden { workspace: d });
        }
        if let Some(v) = vacated {
            self.backfill(v, &mut events);
        }
        if let Some(d) = displaced {
            self.reap(d, &mut events);
        }
        self.reanchor_floating(&areas);
        events.push(Event::LayoutChanged);
        Ok(events)
    }

    /// Exchanges what two displays are showing.
    pub fn swap_output_workspaces(
        &mut self,
        a: OutputId,
        b: OutputId,
    ) -> Result<Vec<Event>, LayoutError> {
        if self.output(a).is_none() {
            return Err(LayoutError::UnknownOutput(a));
        }
        if self.output(b).is_none() {
            return Err(LayoutError::UnknownOutput(b));
        }
        if a == b {
            return Ok(Vec::new());
        }
        let areas = self.area_snapshot();
        let (wa, wb) = (self.shown.remove(&a), self.shown.remove(&b));
        let mut events = Vec::new();
        if let Some(w) = wb {
            self.shown.insert(a, w);
            self.prefer(w, a);
            events.push(Event::WorkspaceShown {
                workspace: w,
                output: a,
            });
        }
        if let Some(w) = wa {
            self.shown.insert(b, w);
            self.prefer(w, b);
            events.push(Event::WorkspaceShown {
                workspace: w,
                output: b,
            });
        }
        for id in [a, b] {
            if !self.shown.contains_key(&id) {
                self.backfill(id, &mut events);
            }
        }
        self.reanchor_floating(&areas);
        events.push(Event::LayoutChanged);
        Ok(events)
    }

    // ---- windows --------------------------------------------------------

    pub fn focused_window(&self) -> Option<WindowId> {
        self.focused_workspace()
            .and_then(|ws| self.workspaces.get(&ws))
            .and_then(Workspace::focused_window)
    }

    /// Adds a window to a desktop, or to the focused one when `to` is `None`.
    pub fn add_window(
        &mut self,
        window: WindowId,
        to: Option<WorkspaceId>,
        target: InsertTarget,
    ) -> Result<Vec<Event>, LayoutError> {
        if self.index.contains_key(&window) {
            return Err(LayoutError::WindowAlreadyManaged(window));
        }
        let ws = match to {
            Some(ws) => {
                if !self.workspaces.contains_key(&ws) {
                    return Err(LayoutError::UnknownWorkspace(ws));
                }
                ws
            }
            None => self.ensure_workspace(),
        };
        let target = self.resolve_target(ws, target);
        let workspace = self.workspaces.get_mut(&ws).expect("checked above");
        workspace.tree.insert(window, target)?;
        workspace.focused_floating = None;
        self.index.insert(window, ws);
        Ok(vec![
            Event::WindowAdded {
                window,
                workspace: ws,
            },
            Event::FocusChanged {
                window: Some(window),
                output: self.output_showing(ws),
            },
            Event::LayoutChanged,
        ])
    }

    pub fn remove_window(&mut self, window: WindowId) -> Result<Vec<Event>, LayoutError> {
        let ws = self
            .index
            .remove(&window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        let workspace = self
            .workspaces
            .get_mut(&ws)
            .expect("index points at a desktop");
        if workspace.tree.contains(window) {
            workspace.tree.remove(window)?;
        }
        workspace.forget(window);
        let mut events = vec![Event::WindowRemoved { window }, Event::LayoutChanged];
        self.reap(ws, &mut events);
        events.push(Event::FocusChanged {
            window: self.focused_window(),
            output: self.focused_output,
        });
        Ok(events)
    }

    /// Focuses a window, pulling its desktop into view if it is hidden.
    pub fn focus_window(&mut self, window: WindowId) -> Result<Vec<Event>, LayoutError> {
        let ws = self
            .workspace_of(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        let mut events = Vec::new();
        let on = match self.output_showing(ws) {
            Some(o) => o,
            None => {
                let target = self
                    .workspaces
                    .get(&ws)
                    .and_then(|w| w.preferred_output)
                    .filter(|&o| self.output(o).is_some())
                    .or(self.focused_output)
                    .or_else(|| self.outputs.first().map(|o| o.id))
                    .ok_or(LayoutError::NoOutputs)?;
                events.extend(self.show_workspace(ws, target)?);
                target
            }
        };
        let workspace = self.workspaces.get_mut(&ws).expect("checked above");
        if workspace.is_floating(window) {
            workspace.focused_floating = Some(window);
            workspace.raise(window);
        } else {
            let node = workspace
                .tree
                .node_of(window)
                .ok_or(LayoutError::UnknownWindow(window))?;
            workspace.tree.set_focus(node)?;
            workspace.focused_floating = None;
        }
        self.focused_output = Some(on);
        events.push(Event::FocusChanged {
            window: Some(window),
            output: Some(on),
        });
        Ok(events)
    }

    /// Moves focus one window in `dir`, crossing to the neighbouring display
    /// when there is nothing that way on the current one.
    pub fn focus_direction(&mut self, dir: Direction) -> Result<Vec<Event>, LayoutError> {
        let visible = self.visible_windows();
        let origin = self
            .focused_window()
            .and_then(|w| visible.iter().find(|v| v.window == w).map(|v| v.rect));
        let Some(origin) = origin else {
            // Nothing focused, or focus is on something not currently visible;
            // land on whatever is on the focused display.
            let first = self
                .focused_workspace()
                .and_then(|ws| self.workspaces.get(&ws))
                .and_then(Workspace::focused_window)
                .or_else(|| visible.first().map(|v| v.window));
            return match first {
                Some(w) => self.focus_window(w),
                None => Ok(Vec::new()),
            };
        };
        let focused = self.focused_window();
        let pool: Vec<(WindowId, Rect)> = visible
            .iter()
            .filter(|v| Some(v.window) != focused)
            .map(|v| (v.window, v.rect))
            .collect();
        match pick_direction(origin, &pool, dir) {
            Some(next) => self.focus_window(next),
            None => Ok(Vec::new()),
        }
    }

    /// Moves a window one step in `dir`, crossing to the neighbouring display
    /// when it is already at the edge of its own tree.
    pub fn move_window_direction(
        &mut self,
        window: WindowId,
        dir: Direction,
    ) -> Result<Vec<Event>, LayoutError> {
        let ws = self
            .workspace_of(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        let workspace = self
            .workspaces
            .get_mut(&ws)
            .expect("index points at a desktop");
        if workspace.is_floating(window) {
            // Floating windows have no place in the tiling order; they are
            // repositioned with `move_floating` instead.
            return Ok(Vec::new());
        }
        let node = workspace
            .tree
            .node_of(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        match workspace.tree.move_node(node, dir) {
            Ok(()) => Ok(vec![Event::LayoutChanged]),
            Err(LayoutError::AtEdge(_)) => self.move_across(window, ws, dir),
            Err(e) => Err(e),
        }
    }

    /// Sends a window to another desktop.
    pub fn move_window_to_workspace(
        &mut self,
        window: WindowId,
        to: WorkspaceId,
        follow: bool,
    ) -> Result<Vec<Event>, LayoutError> {
        let from = self
            .workspace_of(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        if !self.workspaces.contains_key(&to) {
            return Err(LayoutError::UnknownWorkspace(to));
        }
        if from == to {
            return Ok(Vec::new());
        }
        let target = self.resolve_target(to, InsertTarget::default());
        let mut events = self.relocate(window, from, to, target)?;
        if follow {
            events.extend(self.focus_window(window)?);
        }
        Ok(events)
    }

    pub fn swap_windows(&mut self, a: WindowId, b: WindowId) -> Result<Vec<Event>, LayoutError> {
        let wa = self.workspace_of(a).ok_or(LayoutError::UnknownWindow(a))?;
        let wb = self.workspace_of(b).ok_or(LayoutError::UnknownWindow(b))?;
        if wa != wb {
            // Across desktops a swap is two relocations; keeping it to the
            // same desktop keeps the weight semantics well defined.
            return Err(LayoutError::UnknownWindow(b));
        }
        let workspace = self
            .workspaces
            .get_mut(&wa)
            .expect("index points at a desktop");
        let (na, nb) = match (workspace.tree.node_of(a), workspace.tree.node_of(b)) {
            (Some(na), Some(nb)) => (na, nb),
            _ => return Ok(Vec::new()),
        };
        workspace.tree.swap(na, nb)?;
        Ok(vec![Event::LayoutChanged])
    }

    pub fn resize_window(
        &mut self,
        window: WindowId,
        dir: Direction,
        delta_px: i32,
    ) -> Result<Vec<Event>, LayoutError> {
        let ws = self
            .workspace_of(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        let area = self.area_of(ws);
        let params = self.config.params;
        let workspace = self
            .workspaces
            .get_mut(&ws)
            .expect("index points at a desktop");
        if let Some(f) = workspace.floating.iter_mut().find(|f| f.window == window) {
            f.rect = grow(f.rect, dir, delta_px);
            return Ok(vec![Event::LayoutChanged]);
        }
        let node = workspace
            .tree
            .node_of(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        match workspace.tree.resize(node, dir, delta_px, area, &params) {
            Ok(()) => Ok(vec![Event::LayoutChanged]),
            // Nothing to push against in that direction; not an error at this
            // level, just a keystroke with nothing to do.
            Err(LayoutError::AtEdge(_)) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Gives every sibling of the window's container an equal share.
    pub fn equalize(&mut self, window: WindowId) -> Result<Vec<Event>, LayoutError> {
        let ws = self
            .workspace_of(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        let workspace = self
            .workspaces
            .get_mut(&ws)
            .expect("index points at a desktop");
        let Some(node) = workspace.tree.node_of(window) else {
            return Ok(Vec::new());
        };
        let Some(parent) = workspace.tree.parent(node) else {
            return Ok(Vec::new());
        };
        workspace.tree.equalize(parent)?;
        Ok(vec![Event::LayoutChanged])
    }

    /// Changes the axis of the container holding the window.
    pub fn set_axis(
        &mut self,
        window: WindowId,
        axis: Option<Axis>,
    ) -> Result<Vec<Event>, LayoutError> {
        let ws = self
            .workspace_of(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        let workspace = self
            .workspaces
            .get_mut(&ws)
            .expect("index points at a desktop");
        let Some(parent) = workspace
            .tree
            .node_of(window)
            .and_then(|n| workspace.tree.parent(n))
        else {
            return Ok(Vec::new());
        };
        match axis {
            Some(a) => workspace.tree.set_axis(parent, a)?,
            None => workspace.tree.toggle_axis(parent)?,
        }
        Ok(vec![Event::LayoutChanged])
    }

    pub fn set_floating(
        &mut self,
        window: WindowId,
        floating: bool,
    ) -> Result<Vec<Event>, LayoutError> {
        let ws = self
            .workspace_of(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        let area = self.area_of(ws);
        let params = self.config.params;
        let is_floating = self.workspaces[&ws].is_floating(window);
        if is_floating == floating {
            return Ok(Vec::new());
        }

        if floating {
            // Hand the window the rectangle it already occupied, so detaching it
            // from the tree is visually a no-op until it is moved.
            let workspace = self.workspaces.get(&ws).expect("index points at a desktop");
            let rect = workspace
                .tree
                .node_of(window)
                .and_then(|node| {
                    geometry(&workspace.tree, area, &params)
                        .into_iter()
                        .find(|(id, _)| *id == node)
                        .map(|(_, r)| r)
                })
                .unwrap_or_else(|| centered(area));
            let workspace = self
                .workspaces
                .get_mut(&ws)
                .expect("index points at a desktop");
            if workspace.tree.contains(window) {
                workspace.tree.remove(window)?;
            }
            workspace.floating.push(Floating { window, rect });
            workspace.focused_floating = Some(window);
        } else {
            let target = self.resolve_target(ws, InsertTarget::default());
            let workspace = self
                .workspaces
                .get_mut(&ws)
                .expect("index points at a desktop");
            workspace.floating.retain(|f| f.window != window);
            if workspace.focused_floating == Some(window) {
                workspace.focused_floating = None;
            }
            workspace.tree.insert(window, target)?;
        }
        Ok(vec![Event::LayoutChanged])
    }

    /// Repositions a floating window. No-op for tiled windows.
    pub fn move_floating(
        &mut self,
        window: WindowId,
        rect: Rect,
    ) -> Result<Vec<Event>, LayoutError> {
        let ws = self
            .workspace_of(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        let workspace = self
            .workspaces
            .get_mut(&ws)
            .expect("index points at a desktop");
        match workspace.floating.iter_mut().find(|f| f.window == window) {
            Some(f) => {
                f.rect = rect;
                Ok(vec![Event::LayoutChanged])
            }
            None => Ok(Vec::new()),
        }
    }

    pub fn set_fullscreen(
        &mut self,
        window: WindowId,
        fullscreen: bool,
    ) -> Result<Vec<Event>, LayoutError> {
        let ws = self
            .workspace_of(window)
            .ok_or(LayoutError::UnknownWindow(window))?;
        let workspace = self
            .workspaces
            .get_mut(&ws)
            .expect("index points at a desktop");
        let next = fullscreen.then_some(window);
        if workspace.fullscreen == next {
            return Ok(Vec::new());
        }
        if !fullscreen && workspace.fullscreen != Some(window) {
            return Ok(Vec::new());
        }
        workspace.fullscreen = next;
        Ok(vec![Event::LayoutChanged])
    }

    // ---- placement ------------------------------------------------------

    /// Every window on a displayed desktop, with its rectangle in global
    /// coordinates. Tiled windows first, then floating ones bottom-to-top.
    pub(crate) fn visible_windows(&self) -> Vec<Visible> {
        let mut out = Vec::new();
        for output in &self.outputs {
            let Some(&ws) = self.shown.get(&output.id) else {
                continue;
            };
            let Some(workspace) = self.workspaces.get(&ws) else {
                continue;
            };
            for (window, rect) in apply_with(&workspace.tree, output.work_area, &self.config.params)
            {
                out.push(Visible {
                    window,
                    rect,
                    output: output.id,
                    workspace: ws,
                    floating: false,
                });
            }
            for f in &workspace.floating {
                out.push(Visible {
                    window: f.window,
                    rect: f.rect,
                    output: output.id,
                    workspace: ws,
                    floating: true,
                });
            }
        }
        out
    }

    /// The rectangle a desktop's tree is laid out in.
    ///
    /// A hidden desktop has no display of its own, so it borrows the geometry
    /// of the display it prefers, falling back to the first connected one. That
    /// keeps operations on hidden desktops behaving sensibly instead of
    /// collapsing to a zero rectangle.
    pub fn area_of(&self, ws: WorkspaceId) -> Rect {
        if let Some(o) = self.output_showing(ws).and_then(|id| self.output(id)) {
            return o.work_area;
        }
        if let Some(o) = self
            .workspaces
            .get(&ws)
            .and_then(|w| w.preferred_output)
            .and_then(|id| self.output(id))
        {
            return o.work_area;
        }
        self.outputs
            .first()
            .map(|o| o.work_area)
            .unwrap_or(Rect::ZERO)
    }

    // ---- validation -----------------------------------------------------

    pub fn validate(&self) -> Result<(), LayoutInvariant> {
        let mut seen_outputs = BTreeSet::new();
        for o in &self.outputs {
            if !seen_outputs.insert(o.id) {
                return Err(LayoutInvariant::DuplicateOutput(o.id));
            }
        }
        if self.outputs.is_empty() && !self.shown.is_empty() {
            return Err(LayoutInvariant::ShownWithoutOutputs);
        }

        let mut seen_workspaces = BTreeSet::new();
        for (&output, &ws) in &self.shown {
            if !seen_outputs.contains(&output) {
                return Err(LayoutInvariant::UnknownShownWorkspace(output, ws));
            }
            if !self.workspaces.contains_key(&ws) {
                return Err(LayoutInvariant::UnknownShownWorkspace(output, ws));
            }
            if !seen_workspaces.insert(ws) {
                return Err(LayoutInvariant::WorkspaceShownTwice(ws));
            }
        }
        for o in &self.outputs {
            if !self.shown.contains_key(&o.id) {
                return Err(LayoutInvariant::OutputWithoutWorkspace(o.id));
            }
        }
        if let Some(f) = self.focused_output
            && !seen_outputs.contains(&f)
        {
            return Err(LayoutInvariant::BadFocusedOutput(f));
        }

        let mut owner: BTreeMap<WindowId, WorkspaceId> = BTreeMap::new();
        for ws in self.workspaces.values() {
            ws.tree
                .validate()
                .map_err(|e| LayoutInvariant::Tree(ws.id, e))?;
            for window in ws.windows() {
                if owner.insert(window, ws.id).is_some() {
                    return Err(LayoutInvariant::DuplicateWindow(window));
                }
            }
            for f in &ws.floating {
                if ws.tree.contains(f.window) {
                    return Err(LayoutInvariant::DuplicateWindow(f.window));
                }
            }
            if let Some(w) = ws.fullscreen
                && !ws.contains(w)
            {
                return Err(LayoutInvariant::BadFullscreen(ws.id, w));
            }
            if let Some(w) = ws.focused_floating
                && !ws.is_floating(w)
            {
                return Err(LayoutInvariant::BadFloatingFocus(ws.id, w));
            }
        }
        if owner != self.index {
            let mismatch = owner
                .keys()
                .find(|w| self.index.get(w) != owner.get(w))
                .or_else(|| self.index.keys().find(|w| !owner.contains_key(w)))
                .copied()
                .expect("unequal maps differ somewhere");
            return Err(LayoutInvariant::WindowIndexMismatch(mismatch));
        }
        Ok(())
    }

    // ---- internals ------------------------------------------------------

    /// The rectangle every desktop is currently laid out in, taken before a
    /// change of display binding so floating windows can be carried across.
    fn area_snapshot(&self) -> BTreeMap<WorkspaceId, Rect> {
        self.workspaces
            .keys()
            .map(|&id| (id, self.area_of(id)))
            .collect()
    }

    /// Carries floating windows along when their desktop changes display.
    ///
    /// Tiled windows are re-placed from the tree every frame and need nothing,
    /// but a floating rectangle is absolute: without this it would stay behind
    /// on the display the desktop came from.
    fn reanchor_floating(&mut self, before: &BTreeMap<WorkspaceId, Rect>) {
        let moves: Vec<(WorkspaceId, Rect, Rect)> = self
            .workspaces
            .keys()
            .filter_map(|&id| {
                let from = *before.get(&id)?;
                let to = self.area_of(id);
                (from != to && !to.is_empty()).then_some((id, from, to))
            })
            .collect();
        for (id, from, to) in moves {
            if let Some(ws) = self.workspaces.get_mut(&id) {
                for f in &mut ws.floating {
                    f.rect = reanchor(f.rect, from, to);
                }
            }
        }
    }

    fn prefer(&mut self, ws: WorkspaceId, on: OutputId) {
        if let Some(w) = self.workspaces.get_mut(&ws) {
            w.preferred_output = Some(on);
        }
    }

    /// Gives a blank display something to show: the desktop that belongs to it
    /// if one is waiting, otherwise a fresh one.
    fn backfill(&mut self, output: OutputId, events: &mut Vec<Event>) {
        let displayed: BTreeSet<WorkspaceId> = self.shown.values().copied().collect();
        let returning = self
            .workspaces
            .values()
            .find(|w| w.preferred_output == Some(output) && !displayed.contains(&w.id))
            .map(|w| w.id)
            // Nothing belongs to this display, but a desktop that has windows
            // and has never been shown anywhere should be put on screen rather
            // than left stranded behind a brand new empty one.
            .or_else(|| {
                self.workspaces
                    .values()
                    .find(|w| {
                        w.preferred_output.is_none() && !w.is_empty() && !displayed.contains(&w.id)
                    })
                    .map(|w| w.id)
            });
        let ws = match returning {
            Some(ws) => ws,
            None => {
                let ws = self.create_workspace(None);
                events.push(Event::WorkspaceCreated { workspace: ws });
                ws
            }
        };
        self.shown.insert(output, ws);
        self.prefer(ws, output);
        events.push(Event::WorkspaceShown {
            workspace: ws,
            output,
        });
    }

    /// Discards a desktop that is empty and no longer on screen.
    fn reap(&mut self, ws: WorkspaceId, events: &mut Vec<Event>) {
        if !self.config.reap_empty_workspaces {
            return;
        }
        let empty = self.workspaces.get(&ws).is_some_and(Workspace::is_empty);
        if empty && self.output_showing(ws).is_none() {
            self.workspaces.remove(&ws);
            events.push(Event::WorkspaceDestroyed { workspace: ws });
        }
    }

    /// The desktop a new window belongs on when none was named.
    fn ensure_workspace(&mut self) -> WorkspaceId {
        if let Some(ws) = self.focused_workspace() {
            return ws;
        }
        if let Some(&ws) = self.shown.values().next() {
            return ws;
        }
        let ws = self.create_workspace(None);
        if let Some(output) = self
            .focused_output
            .or_else(|| self.outputs.first().map(|o| o.id))
        {
            self.shown.insert(output, ws);
            self.prefer(ws, output);
        }
        ws
    }

    /// Fills in the axis an insertion needs, which depends on geometry the tree
    /// itself cannot see.
    fn resolve_target(&self, ws: WorkspaceId, target: InsertTarget) -> InsertTarget {
        match target {
            InsertTarget::Focused { axis: None } => InsertTarget::Focused {
                axis: self.insert_axis(ws),
            },
            InsertTarget::Root { axis: None } => InsertTarget::Root {
                axis: self.insert_axis(ws),
            },
            other => other,
        }
    }

    fn insert_axis(&self, ws: WorkspaceId) -> Option<Axis> {
        let workspace = self.workspaces.get(&ws)?;
        let anchor = workspace.tree.focus().or_else(|| workspace.tree.root())?;
        if !self.config.smart_split {
            // Without smart splitting, `None` means "join the container the
            // focus is already in", which needs no axis. The one case that does
            // is a lone window, which has no container to join.
            return workspace
                .tree
                .parent(anchor)
                .is_none()
                .then_some(self.config.default_axis);
        }
        let area = self.area_of(ws);
        let rect = geometry(&workspace.tree, area, &self.config.params)
            .into_iter()
            .find(|(id, _)| *id == anchor)
            .map(|(_, r)| r)?;
        Some(if rect.w >= rect.h {
            Axis::Horizontal
        } else {
            Axis::Vertical
        })
    }

    /// Moves a window between desktops, preserving nothing but the window
    /// itself; its share of the destination is decided by the destination.
    fn relocate(
        &mut self,
        window: WindowId,
        from: WorkspaceId,
        to: WorkspaceId,
        target: InsertTarget,
    ) -> Result<Vec<Event>, LayoutError> {
        let mut was_floating = self.workspaces[&from].float_of(window).copied();
        if let Some(f) = was_floating.as_mut() {
            f.rect = reanchor(f.rect, self.area_of(from), self.area_of(to));
        }
        let source = self.workspaces.get_mut(&from).expect("caller checked");
        if source.tree.contains(window) {
            source.tree.remove(window)?;
        }
        source.forget(window);

        let dest = self.workspaces.get_mut(&to).expect("caller checked");
        match was_floating {
            Some(f) => {
                dest.floating.push(f);
                dest.focused_floating = Some(window);
            }
            None => {
                dest.tree.insert(window, target)?;
                dest.focused_floating = None;
            }
        }
        self.index.insert(window, to);

        let mut events = vec![
            Event::WindowMoved { window, from, to },
            Event::LayoutChanged,
        ];
        self.reap(from, &mut events);
        Ok(events)
    }

    /// Hands a window at the edge of its tree to the display in that direction.
    fn move_across(
        &mut self,
        window: WindowId,
        from: WorkspaceId,
        dir: Direction,
    ) -> Result<Vec<Event>, LayoutError> {
        let Some(from_output) = self.output_showing(from) else {
            return Ok(Vec::new());
        };
        let Some(to_output) = self.output_in_direction(from_output, dir) else {
            return Ok(Vec::new());
        };
        let Some(to) = self.active_workspace(to_output) else {
            return Ok(Vec::new());
        };
        // Enter from the edge we arrived at, so a window pushed right lands on
        // the left of the next display.
        let target = match self.workspaces[&to].tree.root() {
            Some(root) => InsertTarget::Beside {
                of: root,
                dir: dir.opposite(),
            },
            None => InsertTarget::Root { axis: None },
        };
        let target = match target {
            InsertTarget::Root { .. } => self.resolve_target(to, target),
            other => other,
        };
        let mut events = self.relocate(window, from, to, target)?;
        if self.config.focus_follows_move {
            events.extend(self.focus_window(window)?);
        }
        Ok(events)
    }
}

/// A window that is currently on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Visible {
    pub window: WindowId,
    pub rect: Rect,
    pub output: OutputId,
    pub workspace: WorkspaceId,
    pub floating: bool,
}

/// Grows a rectangle on one edge, as a floating counterpart to a weight resize.
fn grow(rect: Rect, dir: Direction, delta: i32) -> Rect {
    match dir {
        Direction::Right => Rect::new(rect.x, rect.y, (rect.w + delta).max(1), rect.h),
        Direction::Down => Rect::new(rect.x, rect.y, rect.w, (rect.h + delta).max(1)),
        Direction::Left => {
            let w = (rect.w + delta).max(1);
            Rect::new(rect.x - (w - rect.w), rect.y, w, rect.h)
        }
        Direction::Up => {
            let h = (rect.h + delta).max(1);
            Rect::new(rect.x, rect.y - (h - rect.h), rect.w, h)
        }
    }
}

/// How far a point is from a rectangle, squared. Squared because only the
/// ordering matters and a square root would add nothing but rounding.
fn squared_distance(rect: Rect, point: Point) -> i64 {
    let dx = i64::from((rect.x - point.x).max(point.x - (rect.right() - 1)).max(0));
    let dy = i64::from((rect.y - point.y).max(point.y - (rect.bottom() - 1)).max(0));
    dx * dx + dy * dy
}

/// Maps a floating rectangle from one display's work area onto another's,
/// keeping its offset from the top-left and clamping it back inside.
fn reanchor(rect: Rect, from: Rect, to: Rect) -> Rect {
    let w = rect.w.clamp(0, to.w.max(0));
    let h = rect.h.clamp(0, to.h.max(0));
    let x = to.x + (rect.x - from.x);
    let y = to.y + (rect.y - from.y);
    Rect::new(
        x.clamp(to.x, (to.x + to.w - w).max(to.x)),
        y.clamp(to.y, (to.y + to.h - h).max(to.y)),
        w,
        h,
    )
}

/// A half-size rectangle in the middle of `area`, for a floating window with no
/// previous geometry to inherit.
fn centered(area: Rect) -> Rect {
    let w = (area.w / 2).max(1);
    let h = (area.h / 2).max(1);
    Rect::new(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2, w, h)
}
