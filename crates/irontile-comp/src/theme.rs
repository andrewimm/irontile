//! Appearance and behaviour knobs that belong to the compositor rather than to
//! the layout engine.

use irontile_layout::{Params, Size};

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Drawn as a solid quad behind each window; the window itself is inset by
    /// this much. That is the whole of the decoration.
    pub border_width: i32,
    pub border_focused: [f32; 4],
    pub border_unfocused: [f32; 4],
    pub background: [f32; 4],
    /// Space between adjacent windows.
    pub inner_gap: i32,
    /// Space between the work area edge and the outermost windows.
    pub outer_gap: i32,
    /// Floor a directional resize will not shrink a window past.
    pub min_window: Size,
    /// Pixels per keypress for a directional resize.
    pub resize_step: i32,
    /// Launched by a `spawn` binding with no program of its own.
    pub terminal: Option<String>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            border_width: 2,
            border_focused: [0.36, 0.60, 0.84, 1.0],
            border_unfocused: [0.16, 0.17, 0.20, 1.0],
            background: [0.07, 0.07, 0.09, 1.0],
            inner_gap: 4,
            outer_gap: 4,
            min_window: Size::new(48, 48),
            resize_step: 40,
            terminal: detect_terminal(),
        }
    }
}

impl Theme {
    /// Gaps the layout engine should leave. The border is drawn inside a
    /// window's own cell, so it costs no gap of its own.
    pub fn layout_params(&self) -> Params {
        Params {
            outer_gap: self.outer_gap,
            inner_gap: self.inner_gap,
            min_window: self.min_window,
        }
    }
}

/// Picks the terminal a bare spawn binding launches.
///
/// `IRONTILE_TERMINAL` wins if it is set. Otherwise the first of a few common
/// emulators that is actually on `PATH`, so the binding does something useful
/// before any configuration exists.
fn detect_terminal() -> Option<String> {
    if let Ok(explicit) = std::env::var("IRONTILE_TERMINAL")
        && !explicit.is_empty()
    {
        return Some(explicit);
    }
    ["foot", "alacritty", "kitty", "ghostty", "wezterm", "xterm"]
        .into_iter()
        .find(|program| on_path(program))
        .map(str::to_owned)
}

fn on_path(program: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}
