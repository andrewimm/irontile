//! Layer-shell surfaces: bars, panels, wallpapers and lock screens.
//!
//! These sit outside the tiling tree entirely. What connects them to it is the
//! exclusive zone: a bar that reserves space at the top of a display shrinks
//! that display's work area, and the layout engine tiles into what is left.
//! Until now [`Output::work_area`] was always the whole display, because
//! nothing existed that could reserve anything.
//!
//! [`Output::work_area`]: irontile_layout::Output

use smithay::delegate_layer_shell;
use smithay::desktop::{LayerSurface, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState,
};

use crate::state::Irontile;

impl WlrLayerShellHandler for Irontile {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        // A client may name an output or leave the choice to us; the focused
        // display is the sensible default for a bar or a launcher.
        let output = output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.focused_smithay_output().cloned());
        let Some(output) = output else {
            tracing::warn!(namespace, "no display for a layer surface");
            return;
        };

        let layer = LayerSurface::new(surface, namespace);
        if let Err(err) = layer_map_for_output(&output).map_layer(&layer) {
            tracing::warn!(%err, "failed to map a layer surface");
            return;
        }
        self.refresh_layers();
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        let Some((output, layer)) = self.outputs.iter().find_map(|entry| {
            let map = layer_map_for_output(&entry.output);
            let layer = map
                .layers()
                .find(|l| l.layer_surface() == &surface)
                .cloned()?;
            Some((entry.output.clone(), layer))
        }) else {
            return;
        };
        layer_map_for_output(&output).unmap_layer(&layer);
        self.refresh_layers();
    }
}

impl Irontile {
    /// Re-arranges every display's layer surfaces and republishes the work
    /// areas that come out of it.
    ///
    /// Arranging is what assigns each layer surface its rectangle and computes
    /// what is left over; the layout engine then tiles into that.
    pub fn refresh_layers(&mut self) {
        // `publish_outputs` arranges every map before reading its zone, so this
        // is only the name the layer-shell side calls it by.
        self.publish_outputs();
        // A panel appearing or going away can change who owns the keyboard, and
        // that has to settle now rather than on the next reflow: a launcher
        // that has to wait a frame for focus loses the first thing typed.
        self.reflow();
    }

    /// Whether a surface belongs to a layer surface on any display.
    pub fn layer_for_surface(&self, surface: &WlSurface) -> Option<LayerSurface> {
        self.outputs.iter().find_map(|entry| {
            layer_map_for_output(&entry.output)
                .layer_for_surface(surface, smithay::desktop::WindowSurfaceType::ALL)
                .cloned()
        })
    }
}

delegate_layer_shell!(Irontile);
