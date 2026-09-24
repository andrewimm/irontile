//! The lock screen: cover every display, and give them back to whoever can
//! prove the session is theirs.
//!
//! Three pieces, deliberately separable. `auth` asks PAM and knows nothing
//! about drawing; `paint` draws and knows nothing about Wayland, so a lock
//! screen can be looked at with `--dump` rather than by locking the machine
//! and hoping; `session` holds the displays and drives the two of them.

mod auth;
mod paint;
mod session;

use std::io::Write as _;

use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};

const HELP: &str = "\
irontile-lock - lock the session until a password says otherwise

USAGE:
    irontile-lock [--service NAME] [--user NAME]

OPTIONS:
    --service NAME  The file in /etc/pam.d to authenticate against.
                    Default: irontile-lock.
    --user NAME     Who to authenticate as. Default: $USER.
    -f, --daemonize Fork once the screen is actually locked, so that whatever
                    started this can carry on. An idle daemon told to lock
                    before suspending waits for its lock command to finish,
                    and without this it would wait until somebody unlocked.
    --verify        Ask for a password on the terminal instead of locking
                    anything, and say how long PAM took to answer.
    --dump PATH     Render one lock screen to a PNG and exit, without locking
                    anything. The only way to look at a lock screen while
                    working on it.
    --size WxH      What to render for --dump. Default 1692x1128.
    --scale N       Display scale for --dump. Default 1.
    --version       Show the version and exit.
    --help          Show this message.

The service file is this program's own, so that what unlocking the screen
requires stays the administrator's to decide. It is installed as
/etc/pam.d/irontile-lock; without it nothing here will authenticate, which is
the safe way round.
";

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("irontile-lock: {err}");
            record(&err);
            std::process::ExitCode::FAILURE
        }
    }
}

/// Appends why this stopped to `~/.local/state/irontile/irontile-lock.log`.
///
/// Standard error goes wherever whatever started this was pointed, and what
/// starts a lock screen is an idle daemon on a machine whose owner has walked
/// away -- so on the one occasion the reason matters, it has been written to a
/// virtual terminal nobody will ever read. A locker that dies leaves every
/// display blank until somebody runs another one, and "why" is then the only
/// question worth answering.
///
/// Appended rather than rotated: these are one line each and rare, and losing
/// the line before is how the last one got away.
fn record(why: &str) {
    let Some(state) = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/state"))
        })
    else {
        return;
    };
    let state = state.join("irontile");
    if std::fs::create_dir_all(&state).is_err() {
        return;
    }
    let when = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%z");
    let line = format!("{when} irontile-lock: {why}\n");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state.join("irontile-lock.log"))
    {
        use std::io::Write as _;
        let _ = file.write_all(line.as_bytes());
    }
}

fn run() -> Result<(), String> {
    let mut service = String::from("irontile-lock");
    let mut user = std::env::var("USER").unwrap_or_default();
    let mut dump: Option<std::path::PathBuf> = None;
    let mut verify = false;
    let mut daemonize = false;
    let mut size = (1692u32, 1128u32);
    let mut scale = 1.0f32;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            "--version" | "-V" => {
                println!(
                    "{}",
                    irontile_version::line("irontile-lock", env!("CARGO_PKG_VERSION"))
                );
                return Ok(());
            }
            "--service" => {
                service = iter.next().ok_or("--service needs a name")?.clone();
            }
            "--user" => user = iter.next().ok_or("--user needs a name")?.clone(),
            "--verify" => verify = true,
            "--daemonize" | "-f" => daemonize = true,
            "--dump" => dump = Some(iter.next().ok_or("--dump needs a path")?.into()),
            "--size" => {
                let spec = iter.next().ok_or("--size needs WxH")?;
                let (w, h) = spec.split_once('x').ok_or("--size looks like 1920x1080")?;
                size = (
                    w.parse().map_err(|_| "--size width is not a number")?,
                    h.parse().map_err(|_| "--size height is not a number")?,
                );
            }
            "--scale" => {
                scale = iter
                    .next()
                    .ok_or("--scale needs a number")?
                    .parse()
                    .map_err(|_| "--scale is not a number")?;
            }
            other => return Err(format!("unknown option {other:?}; try --help")),
        }
    }
    if user.is_empty() {
        return Err("no user to authenticate as; try --user".to_string());
    }

    if let Some(path) = dump {
        return render_to_png(&path, size, scale, &user);
    }
    if !verify {
        let ready = if daemonize { fork_once_locked()? } else { None };
        return session::run(&service, &user, ready);
    }

    println!("service: {service}\nuser:    {user}");
    let password = read_password("password: ")?;

    let started = std::time::Instant::now();
    let outcome = auth::verify(&service, &user, &password);
    // How long PAM took, because the answer decides whether the real locker can
    // call it on the main thread or has to hand it to another one.
    let took = started.elapsed();

    match outcome {
        Ok(()) => println!("\nauthenticated in {took:?}"),
        Err(why) => println!("\ndenied in {took:?}: {why}"),
    }
    Ok(())
}

/// Renders one lock screen to a file, so it can be looked at.
///
/// Four of them side by side rather than one: the states are the design, and a
/// picture of the resting state alone says nothing about whether the failure
/// reads as a failure.
fn render_to_png(
    path: &std::path::Path,
    size: (u32, u32),
    scale: f32,
    user: &str,
) -> Result<(), String> {
    use paint::{Palette, Screen, Status, Text};

    let (w, h) = size;
    let states = [
        Status::Typing(0),
        Status::Typing(7),
        Status::Checking,
        Status::Denied("wrong password".to_string()),
        Status::Accepted,
    ];
    let mut sheet = tiny_skia::Pixmap::new(w, h * states.len() as u32)
        .ok_or("that size is too large to render")?;
    let mut text = Text::new(&["Anonymous Pro".to_string()]);
    let palette = Palette::default();
    let now = chrono::Local::now();
    let host = hostname();
    // The real one, like the clock and the hostname above it: this renders
    // what a display would show, and a made-up battery would make the one
    // picture anybody checks the design against a picture of nothing real.
    let battery = irontile_power::battery();

    let states_drawn = states.len();
    for (index, status) in states.into_iter().enumerate() {
        let mut tile = tiny_skia::Pixmap::new(w, h).ok_or("that size is too large to render")?;
        let screen = Screen {
            time: now.format("%H:%M").to_string(),
            seconds: now.format(":%S").to_string(),
            date: now.format("%A, %-d %B").to_string().to_lowercase(),
            user: user.to_string(),
            host: host.clone(),
            caps: matches!(status, Status::Denied(_)),
            status,
            battery,
            // The real answer, like everything else on this sheet: whether a
            // finger opens this machine is a fact about the machine.
            finger: auth::service_exists("irontile-lock-fprint"),
        };
        paint::draw(&mut tile.as_mut(), &screen, &mut text, &palette, scale);
        sheet.draw_pixmap(
            0,
            (h * index as u32) as i32,
            tile.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            tiny_skia::Transform::identity(),
            None,
        );
    }

    let png = sheet.encode_png().map_err(|err| format!("{err}"))?;
    std::fs::write(path, png)
        .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    let count = states_drawn;
    println!(
        "wrote {} ({}x{}, {count} states)",
        path.display(),
        w,
        h * count as u32
    );
    Ok(())
}

/// Splits in two, and lets the first half go as soon as the screen is covered.
///
/// swayidle and anything like it waits for its lock command to finish before
/// letting the machine sleep, which is the whole point of doing it before
/// sleep: a locker that stays in the foreground until somebody unlocks it holds
/// that wait open forever, and the machine sits awake on a lock screen with the
/// lid shut. Reporting "locked" and leaving is what makes the wait end at the
/// right moment rather than never.
///
/// The split happens before anything is connected or any thread is started, so
/// the two halves share nothing but a pipe. Forking a live Wayland connection
/// would give two processes one socket and one sequence of object ids between
/// them.
///
/// Returns the writing end for the half that carries on. The other half does
/// not return at all.
fn fork_once_locked() -> Result<Option<std::os::fd::OwnedFd>, String> {
    let (reader, writer) =
        rustix::pipe::pipe().map_err(|err| format!("could not make a pipe: {err}"))?;

    // SAFETY: nothing has been connected and no thread has been started, so
    // there is no lock, buffer or connection for a fork to leave half-owned.
    #[allow(unsafe_code)]
    let child = unsafe { libc::fork() };

    match child {
        -1 => Err(format!(
            "could not fork: {}",
            std::io::Error::last_os_error()
        )),
        0 => {
            drop(reader);
            // A session of its own, so that whatever started the half that is
            // about to exit cannot take the lock screen down with it.
            //
            // SAFETY: a just-forked child is never a process group leader,
            // which is the one thing this call asks for.
            #[allow(unsafe_code)]
            unsafe {
                libc::setsid();
            }
            Ok(Some(writer))
        }
        _ => {
            drop(writer);
            let mut byte = [0u8; 1];
            loop {
                match rustix::io::read(&reader, &mut byte) {
                    // The screens are covered. Whatever was waiting may go.
                    Ok(1) => std::process::exit(0),
                    // End of file: the other half stopped without ever getting
                    // there. Saying so is the difference between a machine that
                    // suspends locked and one that suspends showing the
                    // desktop, so this must not be reported as success.
                    Ok(_) => {
                        eprintln!("irontile-lock: the session was never locked");
                        std::process::exit(1);
                    }
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(err) => {
                        eprintln!("irontile-lock: lost the locking half: {err}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|name| name.trim().to_string())
        .unwrap_or_else(|_| "localhost".to_string())
}

/// Reads a line from the terminal without showing it.
///
/// The echo flag is restored by a guard rather than at the end of the
/// function, so that an error on the way out still leaves a usable terminal
/// behind -- a locker prototype that quits leaving the shell unable to show
/// what is typed is its own small betrayal.
fn read_password(prompt: &str) -> Result<String, String> {
    print!("{prompt}");
    std::io::stdout().flush().map_err(|err| err.to_string())?;

    let _hidden = Hidden::new()?;
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|err| format!("could not read a password: {err}"))?;
    Ok(line.trim_end_matches(['\n', '\r']).to_string())
}

/// Turns terminal echo off for as long as it is held.
struct Hidden {
    restore: Option<rustix::termios::Termios>,
}

impl Hidden {
    fn new() -> Result<Hidden, String> {
        let stdin = std::io::stdin();
        // Not a terminal -- a pipe, or a test -- so there is no echo to turn
        // off and nothing to restore.
        let Ok(current) = tcgetattr(&stdin) else {
            return Ok(Hidden { restore: None });
        };
        let mut quiet = current.clone();
        quiet.local_modes -= LocalModes::ECHO;
        tcsetattr(&stdin, OptionalActions::Flush, &quiet)
            .map_err(|err| format!("could not turn off echo: {err}"))?;
        Ok(Hidden {
            restore: Some(current),
        })
    }
}

impl Drop for Hidden {
    fn drop(&mut self) {
        if let Some(previous) = &self.restore {
            let _ = tcsetattr(std::io::stdin(), OptionalActions::Now, previous);
        }
    }
}
