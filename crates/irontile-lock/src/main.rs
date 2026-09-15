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
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut service = String::from("irontile-lock");
    let mut user = std::env::var("USER").unwrap_or_default();
    let mut dump: Option<std::path::PathBuf> = None;
    let mut verify = false;
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
                println!("irontile-lock {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--service" => {
                service = iter.next().ok_or("--service needs a name")?.clone();
            }
            "--user" => user = iter.next().ok_or("--user needs a name")?.clone(),
            "--verify" => verify = true,
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
        return session::run(&service, &user);
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
