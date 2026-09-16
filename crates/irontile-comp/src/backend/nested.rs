//! The nested backend: irontile in a window, on top of another compositor.

use std::time::Duration;

use anyhow::Context as _;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Frame as _, Renderer as _};
use smithay::backend::winit::{self, WinitEvent};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::Display;
use smithay::reexports::winit::platform::pump_events::PumpStatus;
use smithay::utils::Rectangle;

use crate::backend::{Backend, Options};
use crate::ipc;
use crate::render::{self, NESTED_TRANSFORM, Scene};
use crate::state::{Irontile, NESTED_OUTPUT, OutputSpec};

/// How often winit's event queue is drained. Winit is not a calloop source of
/// its own, so this is the input latency ceiling for the nested backend.
///
/// Drawing is *not* on this clock. Submitting a frame blocks until the host
/// compositor releases a buffer, and a host that is not showing the window --
/// because it is on another workspace, or occluded -- releases none. Drawing on
/// a timer would then block inside the event loop and starve everything else,
/// including the control socket and every client. So frames are drawn when the
/// host asks for one, and never otherwise.
const TICK: Duration = Duration::from_millis(16);

pub fn run(options: Options) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<Irontile> = EventLoop::try_new()?;
    let display: Display<Irontile> = Display::new()?;
    let display_handle = display.handle();

    let (graphics, mut winit_loop) =
        winit::init::<GlesRenderer>().map_err(|e| anyhow::anyhow!("{e}"))?;
    let window_size = graphics.window_size();
    let window_scale = graphics.scale_factor();

    let socket = super::bind_socket(options.wayland_display.as_deref())?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();

    let mut state = Irontile::new(
        display_handle,
        socket_name.clone(),
        options.config,
        options.config_path,
        event_loop.handle(),
    );
    state.backend = Backend::Nested(Box::new(graphics));
    state.advertise_dmabuf();
    state.add_keyboard()?;
    state.seat.add_pointer();
    state.configure_outputs(&[nested_spec(window_size, window_scale)]);

    let handle = event_loop.handle();
    super::insert_wayland_sources(&handle, display, socket)?;
    let control =
        ipc::listen(&handle, &socket_name).context("failed to bind the control socket")?;

    handle
        .insert_source(Timer::immediate(), move |_, _, state: &mut Irontile| {
            let mut wants_redraw = false;
            let status = winit_loop.dispatch_new_events(|event| match event {
                WinitEvent::Resized { size, scale_factor } => {
                    state.configure_outputs(&[nested_spec(size, scale_factor)]);
                }
                WinitEvent::Input(event) => {
                    let size = state.output_size(NESTED_OUTPUT);
                    crate::input::handle(state, event, size);
                }
                WinitEvent::Redraw => wants_redraw = true,
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
                // Something moved, so ask the host for a frame to show it in.
                state.backend.request_redraw();
            }
            if wants_redraw && let Err(err) = draw(state) {
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
    // The first frame has to be asked for; after that each one is requested
    // when something changes.
    state.backend.request_redraw();
    state.run_startup_commands();

    let signal = event_loop.get_signal();
    event_loop.run(Some(TICK), &mut state, move |state| {
        if !state.running {
            signal.stop();
            return;
        }
        state.popups.cleanup();
        state.notice_a_dead_lock();
        if let Err(err) = state.display_handle.flush_clients() {
            tracing::warn!(%err, "failed to flush clients");
        }
    })?;

    Ok(())
}

/// The nested window is the whole display, and winit renders it flipped.
///
/// The size is the window's real pixels and the scale is the host's, and both
/// have to be stated: a display left at a scale of one has a logical size a
/// third too large on a 1.3333 host, so `draw` lays the scene out across more
/// pixels than the framebuffer holds and the right and bottom of it fall off
/// the edge. On screen that reads as a window that is merely cropped; in a
/// screenshot it is unmistakable, which is how it was found.
fn nested_spec(
    size: smithay::utils::Size<i32, smithay::utils::Physical>,
    scale: f64,
) -> OutputSpec {
    let mut spec = OutputSpec::new(
        NESTED_OUTPUT,
        "irontile-nested",
        irontile_layout::Size::new(size.w.max(1), size.h.max(1)),
    );
    spec.transform = NESTED_TRANSFORM;
    // A host that reports a nonsense scale would otherwise divide the display
    // down to nothing.
    spec.scale = if scale > 0.0 { scale } else { 1.0 };
    spec
}

fn draw(state: &mut Irontile) -> anyhow::Result<()> {
    // Split the compositor into disjoint borrows: the renderer and the state it
    // is drawing now live in the same struct.
    let Irontile {
        backend,
        placements,
        windows,
        outputs,
        layout,
        config,
        session_lock: lock,
        screencopy,
        indicator,
        start_time,
        ..
    } = state;
    let Backend::Nested(graphics) = backend else {
        return Ok(());
    };

    let size = graphics.window_size();
    let scale = graphics.scale_factor();
    let damage = Rectangle::from_size(size);

    let (renderer, mut framebuffer) = graphics.bind().map_err(|e| anyhow::anyhow!("{e}"))?;
    let scene = Scene {
        frame: placements,
        windows,
        outputs,
        layout,
        theme: &config.theme,
        // The compositor irontile is nested inside draws the pointer already.
        cursor: None,
        lock: lock.as_ref(),
        // Asked before the copy is served, so the mark is in the element list
        // that screencopy renders from: a recording carries the light that
        // says it is a recording.
        capture: screencopy.capturing().then(|| indicator.buffer(scale)),
    };
    let elements = render::elements(&scene, renderer, NESTED_OUTPUT, scale);

    // The same list that is about to be shown, so a copy is what is on screen.
    crate::screencopy::serve(
        screencopy,
        renderer,
        &elements,
        crate::screencopy::Display {
            id: NESTED_OUTPUT,
            size: (size.w, size.h).into(),
            scale,
            transform: NESTED_TRANSFORM,
            clear: config.theme.background,
        },
        start_time.elapsed(),
    );

    let mut frame = renderer
        .render(&mut framebuffer, size, NESTED_TRANSFORM)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    frame
        .clear(config.theme.background.into(), &[damage])
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

    // Submitting must happen before the frame callbacks, or a client could
    // draw into the buffer still being scanned out.
    graphics
        .submit(Some(&[damage]))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    state.send_frame_callbacks();
    Ok(())
}
