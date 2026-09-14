//! The irontile compositor.
//!
//! The binary owns every Wayland and rendering concern and holds no layout
//! policy of its own: protocol events become [`irontile_layout`] commands, and
//! the frame that comes back becomes surface configures and render elements.

mod action;
mod backend;
mod config;
mod focus;
mod input;
mod ipc;
mod keymap;
mod layer;
mod registry;
mod render;
mod shell;
mod state;
mod theme;

use std::process::ExitCode;

use irontile_layout::Rect;

use crate::state::OutputSpec;

const HELP: &str = "\
irontile - a Wayland tiling compositor

USAGE:
    irontile [OPTIONS]

OPTIONS:
    --headless [SPEC]   Run without a renderer, with displays described by
                        SPEC. Used for tests and for driving irontile purely
                        over its control socket. SPEC is a comma-separated list
                        of WxH or WxH+X+Y; the default is one 1920x1080 display.
    --config PATH       Read configuration from PATH instead of the usual place.
    --print-config      Write the default configuration to stdout and exit.
    --help              Show this message.

Without --headless, irontile runs nested: it opens as a window inside the
compositor already running, which is the development loop.
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

fn run(args: &[String]) -> anyhow::Result<()> {
    let mut headless: Option<Vec<OutputSpec>> = None;
    let mut config_path: Option<std::path::PathBuf> = None;
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
            "--config" => {
                let path = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--config needs a path"))?;
                config_path = Some(path.into());
            }
            "--headless" => {
                let spec = match iter.peek() {
                    Some(next) if !next.starts_with("--") => Some(iter.next().expect("peeked")),
                    _ => None,
                };
                headless = Some(parse_outputs(spec.map(String::as_str))?);
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

    match headless {
        Some(outputs) => backend::headless::run(outputs, config, path),
        None => backend::nested::run(config, path),
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
        outputs.push(OutputSpec::new(
            irontile_layout::OutputId(index as u64 + 1),
            format!("HEADLESS-{}", index + 1),
            Rect::new(x, y, w, h),
        ));
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

    #[test]
    fn a_bare_size_becomes_one_display() {
        let outputs = parse_outputs(Some("800x600")).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].logical, Rect::new(0, 0, 800, 600));
    }

    #[test]
    fn displays_without_positions_are_laid_end_to_end() {
        let outputs = parse_outputs(Some("1920x1080,1280x1024")).unwrap();
        assert_eq!(outputs[0].logical, Rect::new(0, 0, 1920, 1080));
        assert_eq!(outputs[1].logical, Rect::new(1920, 0, 1280, 1024));
    }

    #[test]
    fn explicit_positions_are_honoured() {
        let outputs = parse_outputs(Some("1920x1080+100+50")).unwrap();
        assert_eq!(outputs[0].logical, Rect::new(100, 50, 1920, 1080));
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
