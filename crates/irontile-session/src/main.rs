//! Starts irontile and brings it back if it dies.
//!
//! A compositor is the one process in a graphical session nothing else can
//! stand in for. When it goes, every window goes with it and the seat is left
//! with no display server at all, which from the chair is indistinguishable
//! from the machine having hung. Supervising it turns that into a blink: the
//! windows are still lost, since this is a restart and not a resurrection, but
//! the session comes back on its own rather than needing a reboot.
//!
//! This is deliberately its own binary with almost no dependencies, because
//! sharing the compositor's would mean sharing whatever made it crash.

use std::os::unix::process::ExitStatusExt as _;
use std::process::{Command, ExitCode, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};

const HELP: &str = "\
start-irontile - run irontile under a supervisor that restarts it if it crashes

USAGE:
    start-irontile [OPTIONS] [-- ARGS...]

OPTIONS:
    --path PATH     The compositor to run. Default: irontile, found on PATH.
    --version       Show the version and exit.
    --help          Show this message.

Everything after -- is passed to irontile; run `irontile --help` for what it
takes. With nothing after it, irontile is started with --session, because a
supervised session is what this exists for.

A clean exit ends the session, and so does being asked to stop: a login manager
ending the session and Ctrl+C at a terminal both arrive as a signal, which is
passed on to irontile and then honoured rather than treated as a crash.
Anything else is a crash, and irontile is started again.
";

/// How many crashes inside [`LIMIT_WINDOW`] before the supervisor gives up.
///
/// Without a limit, a compositor that dies during startup -- a GPU it cannot
/// drive, a display it cannot modeset -- would be restarted forever, which
/// spins a core and leaves the seat flickering with no way back to a terminal.
/// Stopping hands the seat back to whatever started the session.
const LIMIT_BURST: usize = 5;
const LIMIT_WINDOW: Duration = Duration::from_secs(60);

/// A moment between attempts, so the DRM device, the seat and the Wayland
/// socket are let go before the next irontile asks for them.
const SETTLE: Duration = Duration::from_millis(500);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("start-irontile: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<ExitCode, String> {
    let mut bin = String::from("irontile");
    let mut forwarded: Vec<String> = Vec::new();

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(ExitCode::SUCCESS);
            }
            "--version" | "-V" => {
                println!(
                    "{}",
                    irontile_version::line("start-irontile", env!("CARGO_PKG_VERSION"))
                );
                return Ok(ExitCode::SUCCESS);
            }
            "--path" => {
                bin = iter
                    .next()
                    .ok_or_else(|| "--path needs a path".to_string())?
                    .clone();
            }
            "--" => {
                forwarded.extend(iter.cloned());
                break;
            }
            other => return Err(format!("unknown option {other:?}; try --help")),
        }
    }

    if forwarded.is_empty() {
        forwarded.push("--session".to_string());
    }
    supervise(&bin, &forwarded)
}

fn supervise(bin: &str, args: &[String]) -> Result<ExitCode, String> {
    // The compositor currently running, so the signal thread knows what to pass
    // a signal on to. Zero means there is none, which `Pid::from_raw` rejects --
    // worth relying on, because a zero pid would otherwise mean "the whole
    // process group", and this process is in it.
    let running = Arc::new(AtomicI32::new(0));
    forward_signals(Arc::clone(&running))?;

    let mut crashes: Vec<Instant> = Vec::new();
    loop {
        let mut child = Command::new(bin)
            .args(args)
            .spawn()
            .map_err(|err| format!("failed to start {bin}: {err}"))?;
        running.store(child.id() as i32, Ordering::SeqCst);

        let status = child
            .wait()
            .map_err(|err| format!("failed to wait for {bin}: {err}"))?;
        running.store(0, Ordering::SeqCst);

        let how = match outcome(&status) {
            Outcome::Over => return Ok(ExitCode::SUCCESS),
            Outcome::Crashed(how) => how,
        };
        eprintln!("start-irontile: irontile {how}");

        // A sliding window rather than a running total, so a session that stays
        // up for a week and then crashes once is not one crash closer to being
        // abandoned.
        let now = Instant::now();
        crashes.retain(|at| now.duration_since(*at) < LIMIT_WINDOW);
        crashes.push(now);
        if crashes.len() > LIMIT_BURST {
            return Err(format!(
                "irontile crashed {} times in {} seconds; not starting it again",
                crashes.len(),
                LIMIT_WINDOW.as_secs()
            ));
        }

        eprintln!(
            "start-irontile: restarting ({} of {LIMIT_BURST} within {}s)",
            crashes.len(),
            LIMIT_WINDOW.as_secs()
        );
        std::thread::sleep(SETTLE);
    }
}

/// What the compositor exiting means for the session.
#[derive(Debug)]
enum Outcome {
    /// The session is over and nothing should be started again.
    Over,
    /// irontile died on its own, which is what this supervisor is here for.
    Crashed(String),
}

fn outcome(status: &ExitStatus) -> Outcome {
    if status.success() {
        return Outcome::Over;
    }
    match status.signal() {
        // Not a crash: this is how being asked to stop arrives. A login manager
        // ending the session and Ctrl+C at a terminal both land here, and
        // starting the compositor again would fight whoever asked.
        Some(SIGTERM | SIGINT | SIGHUP) => Outcome::Over,
        Some(signal) => Outcome::Crashed(format!("was killed by signal {signal}")),
        None => match status.code() {
            Some(code) => Outcome::Crashed(format!("exited with status {code}")),
            None => Outcome::Crashed("exited without saying why".to_string()),
        },
    }
}

/// Passes a request to stop on to the compositor instead of dying and leaving
/// it orphaned.
///
/// A login manager ending a session signals the process it started, which is
/// this one. Without this, that would kill the supervisor and leave irontile
/// holding the DRM master with nothing watching it -- a black screen that
/// survives the session it belonged to.
fn forward_signals(running: Arc<AtomicI32>) -> Result<(), String> {
    let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGINT, SIGHUP])
        .map_err(|err| format!("failed to listen for signals: {err}"))?;

    std::thread::spawn(move || {
        for number in signals.forever() {
            let Some(pid) = rustix::process::Pid::from_raw(running.load(Ordering::SeqCst)) else {
                continue;
            };
            let signal = match number {
                SIGTERM => rustix::process::Signal::TERM,
                SIGINT => rustix::process::Signal::INT,
                SIGHUP => rustix::process::Signal::HUP,
                _ => continue,
            };
            // A failure here means the compositor is already gone, which is
            // what the signal was asking for.
            let _ = rustix::process::kill_process(pid, signal);
        }
    });
    Ok(())
}
