//! The irontile compositor.
//!
//! The binary owns every Wayland and rendering concern and holds no layout
//! policy of its own: protocol events become [`irontile_layout`] commands, and
//! the frame that comes back becomes surface configures and render elements.

mod action;
mod backend;
mod config;
mod cursor;
mod environment;
mod focus;
mod input;
mod ipc;
mod keymap;
mod layer;
mod lock;
mod registry;
mod render;
mod screencopy;
mod shell;
mod state;
mod theme;

use std::process::ExitCode;

use crate::state::OutputSpec;

const HELP: &str = "\
irontile - a Wayland tiling compositor

USAGE:
    irontile [OPTIONS]

OPTIONS:
    --session           Drive real hardware through DRM. This is the default
                        when there is no compositor already running to nest in.
    --nested            Run as a window inside the compositor already running.
                        The default when there is one.
    --headless [SPEC]   Run without a renderer, with displays described by
                        SPEC. Used for tests and for driving irontile purely
                        over its control socket. SPEC is a comma-separated list
                        of WxH or WxH+X+Y; the default is one 1920x1080 display.
    --wayland-display NAME
                        Bind this Wayland socket name instead of picking the
                        first free one. Useful when you need to know the name
                        in advance, such as on a machine where another
                        compositor already holds wayland-1.
    --config PATH       Read configuration from PATH instead of the usual place.
    --print-config      Write the default configuration to stdout and exit.
    --help              Show this message.

With no backend named, irontile nests if WAYLAND_DISPLAY or DISPLAY is set and
takes the session otherwise, which is what each of those situations means.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("irontile: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Which backend to run.
enum Chosen {
    Nested,
    Session,
    Headless(Vec<OutputSpec>),
}

fn run(args: &[String]) -> anyhow::Result<()> {
    let mut chosen: Option<Chosen> = None;
    let mut config_path: Option<std::path::PathBuf> = None;
    let mut wayland_display: Option<String> = None;
    let mut iter = args.iter().peekable();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--print-config" => {
                print!("{}", config::default_config_text());
                return Ok(());
            }
            "--wayland-display" => {
                let name = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--wayland-display needs a name"))?;
                wayland_display = Some(name.clone());
            }
            "--config" => {
                let path = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--config needs a path"))?;
                config_path = Some(path.into());
            }
            "--session" => chosen = Some(Chosen::Session),
            "--nested" => chosen = Some(Chosen::Nested),
            "--headless" => {
                let spec = match iter.peek() {
                    Some(next) if !next.starts_with("--") => Some(iter.next().expect("peeked")),
                    _ => None,
                };
                chosen = Some(Chosen::Headless(parse_outputs(spec.map(String::as_str))?));
            }
            other => anyhow::bail!("unknown option {other:?}; try --help"),
        }
    }

    init_tracing();
    let path = config_path.unwrap_or_else(config::config_path);
    let config = match config::Config::load_from(&path) {
        Ok(config) => config,
        Err(err) => {
            // Starting with defaults beats refusing to start at all.
            tracing::error!(%err, "using the default configuration");
            config::Config::default()
        }
    };

    let options = backend::Options {
        config,
        config_path: path,
        wayland_display,
    };
    match chosen.unwrap_or_else(default_backend) {
        Chosen::Headless(outputs) => backend::headless::run(outputs, options),
        Chosen::Nested => backend::nested::run(options),
        Chosen::Session => backend::session::run(options),
    }
}

/// Nesting needs something to nest in; taking the session needs nothing to be
/// there already. Which of those is true is the only sensible default.
fn default_backend() -> Chosen {
    let nested = std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty());
    if nested {
        Chosen::Nested
    } else {
        Chosen::Session
    }
}

/// Parses `1920x1080,1280x1024+1920+0`.
///
/// Displays with no explicit position are laid end to end, left to right, which
/// is the arrangement anyone writing a short spec means.
fn parse_outputs(spec: Option<&str>) -> anyhow::Result<Vec<OutputSpec>> {
    let spec = spec.unwrap_or("1920x1080");
    let mut outputs = Vec::new();
    let mut cursor = 0;

    for (index, part) in spec.split(',').map(str::trim).enumerate() {
        let (size, offset) = match part.split_once('+') {
            Some((size, rest)) => {
                let (x, y) = rest
                    .split_once('+')
                    .ok_or_else(|| anyhow::anyhow!("{part:?} needs both +X and +Y"))?;
                (size, Some((number(x, part)?, number(y, part)?)))
            }
            None => (part, None),
        };
        let (w, h) = size
            .split_once('x')
            .ok_or_else(|| anyhow::anyhow!("{part:?} is not WxH"))?;
        let (w, h) = (number(w, part)?, number(h, part)?);
        if w <= 0 || h <= 0 {
            anyhow::bail!("{part:?} has a non-positive size");
        }
        let (x, y) = offset.unwrap_or((cursor, 0));
        cursor = x + w;
        outputs.push(
            OutputSpec::new(
                irontile_layout::OutputId(index as u64 + 1),
                format!("HEADLESS-{}", index + 1),
                irontile_layout::Size::new(w, h),
            )
            .at(irontile_layout::Point::new(x, y)),
        );
    }
    if outputs.is_empty() {
        anyhow::bail!("no displays in the spec");
    }
    Ok(outputs)
}

fn number(text: &str, part: &str) -> anyhow::Result<i32> {
    text.parse()
        .map_err(|_| anyhow::anyhow!("{text:?} in {part:?} is not a number"))
}

fn init_tracing() {
    use std::io::IsTerminal;
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // Escape codes in a redirected log make it unreadable and unparseable.
        .with_ansi(std::io::stdout().is_terminal())
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    use irontile_layout::Rect;

    #[test]
    fn a_bare_size_becomes_one_display() {
        let outputs = parse_outputs(Some("800x600")).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].logical(), Rect::new(0, 0, 800, 600));
    }

    #[test]
    fn displays_without_positions_are_laid_end_to_end() {
        let outputs = parse_outputs(Some("1920x1080,1280x1024")).unwrap();
        assert_eq!(outputs[0].logical(), Rect::new(0, 0, 1920, 1080));
        assert_eq!(outputs[1].logical(), Rect::new(1920, 0, 1280, 1024));
    }

    #[test]
    fn explicit_positions_are_honoured() {
        let outputs = parse_outputs(Some("1920x1080+100+50")).unwrap();
        assert_eq!(outputs[0].logical(), Rect::new(100, 50, 1920, 1080));
    }

    #[test]
    fn malformed_specs_are_rejected() {
        for spec in ["", "1920", "1920x", "axb", "1920x1080+1", "0x100"] {
            assert!(
                parse_outputs(Some(spec)).is_err(),
                "{spec:?} should not parse"
            );
        }
    }
}
