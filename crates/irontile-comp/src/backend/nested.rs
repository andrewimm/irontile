//! The nested backend: irontile in a window, on top of another compositor.

use std::time::Duration;

use anyhow::Context;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Frame as _, Renderer as _};
use smithay::backend::winit::{self, WinitEvent};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::Display;
use smithay::reexports::winit::platform::pump_events::PumpStatus;
use smithay::utils::Rectangle;
use smithay::wayland::socket::ListeningSocketSource;

use crate::config::Config;
use crate::ipc;
use crate::render::{self, NESTED_TRANSFORM};
use crate::state::{Irontile, NESTED_OUTPUT, OutputSpec};

/// Roughly 60Hz. Winit is pumped from a timer rather than being a calloop
/// source of its own, so this is also the input latency ceiling for the nested
/// backend; a session backend will be driven by real vblank instead.
const TICK: Duration = Duration::from_millis(16);

pub fn run(config: Config, config_path: std::path::PathBuf) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<Irontile> = EventLoop::try_new()?;
    let display: Display<Irontile> = Display::new()?;
    let display_handle = display.handle();

    let (mut backend, mut winit_loop) =
        winit::init::<GlesRenderer>().map_err(|e| anyhow::anyhow!("{e}"))?;
    let window_size = backend.window_size();

    let socket = ListeningSocketSource::new_auto().context("failed to bind a wayland socket")?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();

    let mut state = Irontile::new(display_handle, socket_name.clone(), config, config_path);
    state
        .seat
        .add_keyboard(Default::default(), 200, 25)
        .context("failed to create a keyboard")?;
    state.seat.add_pointer();
    state.configure_outputs(&[nested_spec(window_size.to_logical(1))]);

    let handle = event_loop.handle();
    super::insert_wayland_sources(&handle, display, socket)?;
    let control =
        ipc::listen(&handle, &socket_name).context("failed to bind the control socket")?;

    handle
        .insert_source(Timer::immediate(), move |_, _, state: &mut Irontile| {
            let status = winit_loop.dispatch_new_events(|event| match event {
                WinitEvent::Resized { size, .. } => {
                    state.configure_outputs(&[nested_spec(size.to_logical(1))]);
                }
                WinitEvent::Input(event) => {
                    let size = state.output_size(NESTED_OUTPUT);
                    crate::input::handle(state, event, size);
                }
                WinitEvent::CloseRequested => state.running = false,
                _ => {}
            });

            if matches!(status, PumpStatus::Exit(_)) {
                state.running = false;
            }
            if !state.running {
                return TimeoutAction::Drop;
            }

            if state.dirty {
                state.reflow();
            }
            if let Err(err) = draw(state, &mut backend) {
                tracing::error!(%err, "failed to render");
            }

            TimeoutAction::ToDuration(TICK)
        })
        .map_err(|e| anyhow::anyhow!("failed to insert the tick source: {e}"))?;

    tracing::info!(
        socket = %socket_name,
        control = %control.path().display(),
        "irontile is running"
    );

    let signal = event_loop.get_signal();
    event_loop.run(Some(TICK), &mut state, move |state| {
        if !state.running {
            signal.stop();
            return;
        }
        state.popups.cleanup();
        if let Err(err) = state.display_handle.flush_clients() {
            tracing::warn!(%err, "failed to flush clients");
        }
    })?;

    Ok(())
}

/// The nested window is the whole display, and winit renders it flipped.
fn nested_spec(size: smithay::utils::Size<i32, smithay::utils::Logical>) -> OutputSpec {
    let mut spec = OutputSpec::new(
        NESTED_OUTPUT,
        "irontile-nested",
        irontile_layout::Rect::new(0, 0, size.w, size.h),
    );
    spec.transform = NESTED_TRANSFORM;
    spec
}

fn draw(
    state: &mut Irontile,
    backend: &mut winit::WinitGraphicsBackend<GlesRenderer>,
) -> anyhow::Result<()> {
    let size = backend.window_size();
    let scale = backend.scale_factor();
    let damage = Rectangle::from_size(size);

    let (renderer, mut framebuffer) = backend.bind().map_err(|e| anyhow::anyhow!("{e}"))?;
    let elements = render::elements(state, renderer, scale);

    let mut frame = renderer
        .render(&mut framebuffer, size, NESTED_TRANSFORM)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    frame
        .clear(state.config.theme.background.into(), &[damage])
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    smithay::backend::renderer::utils::draw_render_elements(
        &mut frame,
        scale,
        &elements,
        &[damage],
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let _sync = frame.finish().map_err(|e| anyhow::anyhow!("{e}"))?;
    drop(framebuffer);

    state.send_frame_callbacks();

    backend
        .submit(Some(&[damage]))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}
