//! The irontile bar.
//!
//! An ordinary layer-shell client with no privileged access: everything it
//! knows about the compositor arrives over the control socket, the same one the
//! tests drive.

use irontile_ui::bar;
use irontile_ui::config::{Config, config_path};
use irontile_ui::draw::TextRenderer;
use irontile_ui::icon::IconSet;
use irontile_ui::module::Snapshot;
use irontile_ui::wayland;
use irontile_ui::world::System;

const HELP: &str = "\
irontile-bar - a status bar for irontile

USAGE:
    irontile-bar [OPTIONS]

OPTIONS:
    --config PATH   Read configuration from PATH instead of the usual place.
    --dump PATH     Render one bar to a PNG and exit, without a compositor.
                    The quickest way to see what a configuration looks like.
    --width N       Width to render for --dump. Default 1200.
    --scale N       Display scale to render for --dump. Default 1.
    --tooltip TEXT  Render a tooltip saying TEXT rather than a bar, so the
                    [tooltip] settings can be seen. Newlines are written \\n.
    --help          Show this message.
";

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("irontile-bar: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config_file = None;
    let mut dump = None;
    let mut tip = None;
    let mut width = 1200u32;
    let mut scale = 1.0f32;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--config" => config_file = Some(std::path::PathBuf::from(need(iter.next(), arg)?)),
            "--dump" => dump = Some(std::path::PathBuf::from(need(iter.next(), arg)?)),
            "--width" => width = need(iter.next(), arg)?.parse()?,
            "--scale" => scale = need(iter.next(), arg)?.parse()?,
            "--tooltip" => tip = Some(need(iter.next(), arg)?.replace("\\n", "\n")),
            other => return Err(format!("unknown option {other:?}; try --help").into()),
        }
    }

    let path = config_file.unwrap_or_else(config_path);
    let config = Config::load(&path)?;

    match (dump, tip) {
        (Some(target), Some(text)) => tooltip_to_png(&config, &target, &text, scale),
        (Some(target), None) => render_to_png(&config, &target, width, scale),
        (None, Some(_)) => Err("--tooltip needs a --dump to write to".into()),
        (None, None) => wayland::run(config),
    }
}

fn need<'a>(value: Option<&'a String>, flag: &str) -> Result<&'a String, String> {
    value.ok_or_else(|| format!("{flag} needs a value"))
}

/// Renders one bar and writes it out, so a configuration can be looked at
/// without a compositor being involved.
fn render_to_png(
    config: &Config,
    target: &std::path::Path,
    width: u32,
    scale: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut text = TextRenderer::new(&config.font, config.font_size * scale);
    let mut icons = IconSet::new(&config.icon_theme, config.icon_path.clone());
    // One frame is all there is, so it is worth waiting a moment for the
    // readings that arrive on their own rather than drawing a bar with a hole
    // in it where the volume goes.
    let world = System::new();
    world.settle(std::time::Duration::from_millis(1500));
    // Nothing has been clicked, so every module shows its first format.
    let frame = bar::draw(
        config,
        &mut text,
        &mut icons,
        &sample(),
        &world,
        &bar::Target {
            width,
            scale,
            alt: &|_| false,
        },
    );
    frame.pixmap.save_png(target)?;
    println!(
        "irontile-bar: wrote {} ({}x{}, {} clickable)",
        target.display(),
        frame.pixmap.width(),
        frame.pixmap.height(),
        frame.hits.len()
    );
    Ok(())
}

/// Stand-in state for `--dump`, so the picture shows a populated bar rather
/// than an empty one.
fn sample() -> Snapshot {
    use irontile_ipc::{OutputId, WindowId, WindowInfo, WorkspaceId, WorkspaceSummary};
    let workspace = |id: u64, name: &str, focused: bool, windows: usize| WorkspaceSummary {
        id: WorkspaceId(id),
        name: Some(name.into()),
        output: Some(OutputId(1)),
        focused,
        windows: (0..windows as u64).map(WindowId).collect(),
    };
    Snapshot {
        workspaces: vec![
            workspace(0, "1", false, 2),
            workspace(1, "2", true, 1),
            workspace(2, "3", false, 1),
        ],
        windows: vec![WindowInfo {
            id: WindowId(0),
            title: Some("irontile — the bar, rendered".into()),
            app_id: Some("org.irontile.demo".into()),
            workspace: WorkspaceId(1),
            output: Some(OutputId(1)),
            focused: true,
        }],
        output: Some(OutputId(1)),
    }
}

/// Renders one tooltip and writes it out, the same way `--dump` does a bar.
fn tooltip_to_png(
    config: &Config,
    target: &std::path::Path,
    text: &str,
    scale: f32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut renderer = TextRenderer::new(&config.font, config.font_size);
    let frame = bar::tooltip(config, &mut renderer, text, scale)
        .ok_or("a tooltip with nothing in it is not drawn")?;
    frame.pixmap.save_png(target)?;
    println!(
        "irontile-bar: wrote {} ({}x{})",
        target.display(),
        frame.pixmap.width(),
        frame.pixmap.height()
    );
    Ok(())
}
