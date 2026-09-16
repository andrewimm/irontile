//! The irontile compositor.
//!
//! The binary owns every Wayland and rendering concern and holds no layout
//! policy of its own: protocol events become [`irontile_layout`] commands, and
//! the frame that comes back becomes surface configures and render elements.

mod action;
mod backend;
mod config;
mod cursor;
mod device;
mod environment;
mod focus;
mod indicator;
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
    --version           Show the version and exit.
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
            "--version" | "-V" => {
                println!(
                    "{}",
                    irontile_version::line("irontile", env!("CARGO_PKG_VERSION"))
                );
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

    // Decided before the backend runs, because the backend is the reason: a
    // session drives real hardware from a virtual terminal nobody reads
    // afterwards, while nested and headless are run from a terminal that is
    // being watched right now. Writing every nested run to the same file would
    // also rotate away the log of the session it is nested inside, which is the
    // one worth keeping.
    let backend = chosen.get_or_insert_with(default_backend);
    init_tracing(matches!(backend, Chosen::Session));
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
    match chosen.expect("filled in above") {
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

fn init_tracing(to_file: bool) {
    use std::io::IsTerminal;
    use tracing_subscriber::EnvFilter;
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let terminal = tracing_subscriber::fmt::layer()
        // Escape codes in a redirected log make it unreadable and unparseable.
        .with_ansi(std::io::stdout().is_terminal());

    // A compositor started from a virtual terminal writes to that terminal, and
    // a virtual terminal is not somewhere anybody reads from afterwards: the
    // session that went wrong is precisely the session whose output is gone.
    // So it is written down as well, and the previous run is kept, because the
    // interesting run is usually the one before the machine came back up.
    let file = to_file.then(log_file).flatten().map(|file| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
    });
    let missing = to_file && file.is_none();

    tracing_subscriber::registry()
        .with(filter)
        .with(terminal)
        .with(file)
        .init();
    if missing {
        tracing::warn!("no log file; this session's output lives only in this terminal");
    }
}

/// How many previous runs to keep beside the current one.
///
/// More than one, because getting back into a wedged session takes more than
/// one attempt: a compositor that cannot take the display still starts, still
/// rotates the log, and the run worth reading is then two or three starts back
/// rather than one. Keeping a single previous run meant the first restart
/// destroyed the evidence of what it was restarting from, which is exactly
/// what happened the first time this was needed.
const KEPT_LOGS: usize = 5;

/// Opens the log, shuffling previous runs down to `irontile.log.5`.
///
/// `None` rather than a failure: a compositor that would not start because it
/// could not write a log would be a worse compositor than one that starts
/// without it.
fn log_file() -> Option<std::fs::File> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/state"))
        })?
        .join("irontile");
    std::fs::create_dir_all(&state).ok()?;
    open_log_in(&state)
}

/// The rotation itself, given somewhere to do it.
///
/// Separate from finding the directory so that what shuffles the files can be
/// tested on a directory of its own. This is the part that decides whether the
/// run worth reading is still there, and it was wrong the first time.
fn open_log_in(state: &std::path::Path) -> Option<std::fs::File> {
    let path = state.join("irontile.log");
    // Renamed rather than appended to, so a file is one run and its size is
    // that run's. An append would grow without bound across a year of logins.
    if path.exists() {
        // Oldest first, or each rename would overwrite the one it is about to
        // move.
        for n in (1..KEPT_LOGS).rev() {
            let older = state.join(format!("irontile.log.{n}"));
            if older.exists() {
                let _ = std::fs::rename(&older, state.join(format!("irontile.log.{}", n + 1)));
            }
        }
        let _ = std::fs::rename(&path, state.join("irontile.log.1"));
    }
    std::fs::File::create(&path).ok()
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

#[cfg(test)]
mod log_tests {
    use super::{KEPT_LOGS, open_log_in};
    use std::io::Write as _;

    /// A directory of its own, removed when the test ends.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("irontile-log-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory to write in");
        dir
    }

    /// Starts the compositor again, writing a line that says which run it was.
    fn run_once(dir: &std::path::Path, marker: &str) {
        let mut file = open_log_in(dir).expect("a log to write to");
        writeln!(file, "{marker}").expect("a line to land");
    }

    #[test]
    fn the_run_that_went_wrong_survives_the_restarts_it_takes_to_recover() {
        // The failure that made this necessary: a wedged session took two
        // restarts to get back into, and keeping one previous run meant the
        // first restart destroyed the log of the thing it was restarting from.
        let dir = scratch("recover");
        run_once(&dir, "the run that went wrong");
        run_once(&dir, "first attempt at getting back in");
        run_once(&dir, "second attempt, which worked");

        let two_back = std::fs::read_to_string(dir.join("irontile.log.2")).expect("two runs back");
        assert!(
            two_back.contains("the run that went wrong"),
            "the interesting run was rotated away by the recovery: {two_back:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_shuffle_keeps_them_in_order_and_stops_at_the_last() {
        let dir = scratch("order");
        for run in 0..KEPT_LOGS + 3 {
            run_once(&dir, &format!("run {run}"));
        }

        // The newest previous run is .1 and they get older as they go.
        let last = KEPT_LOGS + 2;
        for back in 1..=KEPT_LOGS {
            let body = std::fs::read_to_string(dir.join(format!("irontile.log.{back}")))
                .unwrap_or_else(|_| panic!("irontile.log.{back} should exist"));
            assert!(
                body.contains(&format!("run {}", last - back)),
                "irontile.log.{back} held {body:?}"
            );
        }
        assert!(
            !dir.join(format!("irontile.log.{}", KEPT_LOGS + 1)).exists(),
            "kept more runs than it promised to"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
