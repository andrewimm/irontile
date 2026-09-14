//! The nested backend: irontile in a window, on top of another compositor.

use std::time::Duration;

use anyhow::Context;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::{Frame as _, Renderer as _};
use smithay::backend::winit::{self, WinitEvent};
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::EventLoop;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::wayland_server::Display;
use smithay::reexports::winit::platform::pump_events::PumpStatus;
use smithay::utils::{Rectangle, Transform};
use smithay::wayland::socket::ListeningSocketSource;

use crate::render::{self, NESTED_TRANSFORM};
use crate::state::{ClientState, Irontile};

/// Roughly 60Hz. Winit is pumped from a timer rather than being a calloop
/// source of its own, so this is also the input latency ceiling for the nested
/// backend; a session backend will be driven by real vblank instead.
const TICK: Duration = Duration::from_millis(16);

pub fn run() -> anyhow::Result<()> {
    let mut event_loop: EventLoop<Irontile> = EventLoop::try_new()?;
    let display: Display<Irontile> = Display::new()?;
    let display_handle = display.handle();

    let (mut backend, mut winit_loop) =
        winit::init::<GlesRenderer>().map_err(|e| anyhow::anyhow!("{e}"))?;

    let window_size = backend.window_size();
    let mode = Mode {
        size: window_size,
        refresh: 60_000,
    };
    let output = Output::new(
        "irontile-nested".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "irontile".into(),
            model: "nested".into(),
        },
    );
    let _output_global = output.create_global::<Irontile>(&display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(mode);

    let socket = ListeningSocketSource::new_auto().context("failed to bind a wayland socket")?;
    let socket_name = socket.socket_name().to_string_lossy().into_owned();

    let mut state = Irontile::new(display_handle.clone(), output.clone(), socket_name.clone());
    state
        .seat
        .add_keyboard(Default::default(), 200, 25)
        .context("failed to create a keyboard")?;
    state.seat.add_pointer();
    state.set_output_size(window_size.to_logical(1));

    let handle = event_loop.handle();
    handle
        .insert_source(socket, move |stream, _, state: &mut Irontile| {
            if let Err(err) = state
                .display_handle
                .insert_client(stream, std::sync::Arc::new(ClientState::default()))
            {
                tracing::warn!(%err, "failed to accept a client");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert the socket source: {e}"))?;

    handle
        .insert_source(
            smithay::reexports::calloop::generic::Generic::new(
                display,
                smithay::reexports::calloop::Interest::READ,
                smithay::reexports::calloop::Mode::Level,
            ),
            |_, display, state: &mut Irontile| {
                // SAFETY: the display is owned by this source for the lifetime
                // of the event loop and is never moved out of it, so the
                // reference handed back is valid for the duration of the call.
                #[allow(unsafe_code)]
                let display = unsafe { display.get_mut() };
                display.dispatch_clients(state)?;
                Ok(smithay::reexports::calloop::PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert the display source: {e}"))?;

    handle
        .insert_source(Timer::immediate(), move |_, _, state: &mut Irontile| {
            let status = winit_loop.dispatch_new_events(|event| match event {
                WinitEvent::Resized { size, .. } => {
                    let mode = Mode {
                        size,
                        refresh: 60_000,
                    };
                    state
                        .output
                        .change_current_state(Some(mode), None, None, None);
                    state.output.set_preferred(mode);
                    state.set_output_size(size.to_logical(1));
                }
                WinitEvent::Input(event) => {
                    let size = state.output_size();
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

    tracing::info!(socket = %socket_name, "irontile is running");

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
        .clear(state.theme.background.into(), &[damage])
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
