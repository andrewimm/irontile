//! The map between layout window ids and Wayland toplevels.
//!
//! The layout engine addresses windows by an opaque [`WindowId`]; everything
//! protocol-shaped lives behind that id here. Ids are allocated monotonically
//! and never reused, so an id held across an unmap resolves to nothing rather
//! than to whatever window was mapped next.

use std::collections::{HashMap, HashSet};

use irontile_layout::WindowId;
use smithay::backend::renderer::element::Id;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::get_parent;

#[derive(Debug)]
pub struct Entry {
    pub window: Window,
    /// Stable identity for this window's border quad, so the renderer sees one
    /// long-lived element rather than a new one every frame.
    pub border: Id,
}

#[derive(Debug, Default)]
pub struct Registry {
    next: u64,
    entries: HashMap<WindowId, Entry>,
    /// Toplevels that exist but have not committed a buffer yet. They are held
    /// out of the tiling tree until they have something to show, so a window
    /// never appears as an empty bordered cell that steals focus before it has
    /// painted anything.
    unmapped: HashSet<WindowId>,
}

impl Registry {
    /// Registers a new toplevel, initially unmapped.
    pub fn insert(&mut self, window: Window) -> WindowId {
        let id = WindowId(self.next);
        self.next += 1;
        self.entries.insert(
            id,
            Entry {
                window,
                border: Id::new(),
            },
        );
        self.unmapped.insert(id);
        id
    }

    pub fn remove(&mut self, id: WindowId) -> Option<Entry> {
        self.unmapped.remove(&id);
        self.entries.remove(&id)
    }

    pub fn is_unmapped(&self, id: WindowId) -> bool {
        self.unmapped.contains(&id)
    }

    /// Marks a window as having drawn. Returns whether this was the transition,
    /// so the caller only admits it to the tree once.
    pub fn mark_mapped(&mut self, id: WindowId) -> bool {
        self.unmapped.remove(&id)
    }

    pub fn get(&self, id: WindowId) -> Option<&Entry> {
        self.entries.get(&id)
    }

    pub fn window(&self, id: WindowId) -> Option<&Window> {
        self.entries.get(&id).map(|e| &e.window)
    }

    /// Finds the window owning a surface, following subsurface parents up to
    /// the toplevel so that a commit on any part of a window finds it.
    pub fn find(&self, surface: &WlSurface) -> Option<(WindowId, &Window)> {
        let mut current = surface.clone();
        loop {
            if let Some((&id, entry)) = self.entries.iter().find(|(_, e)| {
                e.window
                    .toplevel()
                    .is_some_and(|t| t.wl_surface() == &current)
            }) {
                return Some((id, &entry.window));
            }
            match get_parent(&current) {
                Some(parent) => current = parent,
                None => return None,
            }
        }
    }

    pub fn id_of(&self, surface: &WlSurface) -> Option<WindowId> {
        self.find(surface).map(|(id, _)| id)
    }
}
