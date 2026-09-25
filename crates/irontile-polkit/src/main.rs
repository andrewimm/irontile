//! The authentication agent irontile's session registers with polkit.
//!
//! polkit decides whether something may be done; an agent is what asks the
//! person at the keyboard. Without one registered, every action that needs an
//! administrator fails with nobody to ask -- which from the chair looks like a
//! button that does nothing.
//!
//! Nothing here is privileged. The agent shows what is being asked for and
//! collects an answer; polkit's own helper does the checking and tells the
//! authority. See `helper`.

const HELP: &str = "\
irontile-polkit - the authentication agent irontile's session registers with polkit

USAGE:
    irontile-polkit

Started with the session, and left running: an agent that exits is an agent
that unregisters, and the next action needing an administrator then has nobody
to ask. Nothing here is privileged -- polkit's own helper does the checking.

OPTIONS:
    --dump PATH     Render the dialog to a PNG and exit, without waiting for
                    anything to ask. A prompt that only appears when something
                    wants an administrator is a prompt nobody can look at.
    --try           Show the dialog with a made-up request and print what was
                    typed. For looking at it on a real display without waiting
                    for something to want an administrator.
    --version       Show the version and exit.
    --help          Show this message.
";

mod agent;
mod dialog;
mod helper;
mod paint;
mod users;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("irontile-polkit: {err}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    // Every option this takes ends the program, so there is nothing to collect
    // and nothing to loop over.
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("--version" | "-V") => {
            println!(
                "{}",
                irontile_version::line("irontile-polkit", env!("CARGO_PKG_VERSION"))
            );
            return Ok(());
        }
        Some("--dump") => {
            let path = std::env::args()
                .nth(2)
                .ok_or("--dump needs a path".to_string())?;
            return dump(&path);
        }
        Some("--try") => return try_the_dialog(),
        Some("--help" | "-h") => {
            print!("{HELP}");
            return Ok(());
        }
        Some(other) => return Err(format!("unknown option {other:?}; try --help")),
    }

    if !helper::available() {
        return Err("polkit's authentication helper is not listening; is polkit running?".into());
    }

    let session = agent::session_id()?;
    let connection = zbus::blocking::Connection::system()
        .map_err(|err| format!("could not reach the system bus: {err}"))?;

    // A window when there is a compositor to put one on, and the terminal
    // otherwise. The fallback is not a nicety: an agent that can only ask
    // through a window cannot be used to fix a broken window.
    let windowed = std::env::var_os("WAYLAND_DISPLAY").is_some();
    let listener = agent::Listener {
        ask: if windowed {
            Box::new(ask_in_a_window)
        } else {
            Box::new(ask_on_the_terminal)
        },
    };
    connection
        .object_server()
        .at(agent::AGENT_PATH, listener)
        .map_err(|err| format!("could not put the agent on the bus: {err}"))?;
    agent::register(&connection, &session)?;
    eprintln!("irontile-polkit: answering for session {session}");

    // Nothing else to do on this thread: the object server answers on its own,
    // and an agent that exited would be an agent that unregistered.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

/// Asks in a window, which is what this is for.
///
/// One attempt per call. polkit's own agents loop here and offer another go
/// after a wrong password; this does not yet, so a mistyped password ends the
/// attempt and whatever asked has to ask again.
fn ask_in_a_window(request: &agent::Request) -> agent::Answer {
    let who = request
        .identities
        .iter()
        .find_map(|identity| identity.name())?;
    let prompt = paint::Prompt {
        message: request.message.clone(),
        action: request.action_id.clone(),
        user: who.clone(),
        typed: 0,
        refused: false,
    };
    let families = vec!["Anonymous Pro".to_string(), "Noto Sans".to_string()];
    match dialog::ask(prompt, &families) {
        Ok(dialog::Outcome::Entered(secret)) => Some((who, secret)),
        Ok(dialog::Outcome::Dismissed) => None,
        Err(err) => {
            // Saying so and falling back is better than a prompt that never
            // appears: something asked for an administrator and nobody was
            // told.
            eprintln!("irontile-polkit: could not open a dialog: {err}");
            ask_on_the_terminal(request)
        }
    }
}

/// Asks on the terminal this was started from.
///
/// The first thing to work, and the thing to fall back to: an agent that can
/// only ask through a window is one that cannot be used to fix a broken
/// window.
fn ask_on_the_terminal(request: &agent::Request) -> Option<(String, String)> {
    let who = request
        .identities
        .iter()
        .find_map(|identity| identity.name())?;

    eprintln!("\nirontile-polkit: {}", request.message);
    eprintln!("irontile-polkit: action {}", request.action_id);
    eprint!("irontile-polkit: password for {who}: ");

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).ok()? == 0 {
        return None;
    }
    Some((who, line.trim_end_matches('\n').to_string()))
}

/// Renders the dialog to a file, in the states it has.
///
/// Three of them stacked: nothing typed, something typed, and a refusal. The
/// states are the design, and a picture of an empty field says nothing about
/// whether being turned away reads as being turned away.
fn dump(path: &str) -> Result<(), String> {
    let states = [(0usize, false), (5, false), (0, true)];
    let (w, h) = (460u32, 200u32);
    let mut sheet = tiny_skia::Pixmap::new(w, h * states.len() as u32)
        .ok_or("that size is too large to render")?;
    let mut text = paint::Text::new(&["Anonymous Pro".to_string()]);
    let palette = paint::Palette::default();

    for (index, (typed, refused)) in states.into_iter().enumerate() {
        let mut tile = tiny_skia::Pixmap::new(w, h).ok_or("that size is too large to render")?;
        let prompt = paint::Prompt {
            message: "Authentication is required to manage system services or other units."
                .to_string(),
            action: "org.freedesktop.systemd1.manage-units".to_string(),
            user: "andrew".to_string(),
            typed,
            refused,
        };
        paint::draw(&mut tile.as_mut(), &prompt, &mut text, &palette, 1.0);
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
    std::fs::write(path, png).map_err(|err| format!("could not write {path}: {err}"))?;
    println!(
        "wrote {path} ({w}x{}, {} states)",
        h * states.len() as u32,
        states.len()
    );
    Ok(())
}

/// Shows the dialog once, against whatever compositor is running.
///
/// Prints how long the answer was rather than the answer: this is a
/// development aid, and one that echoed passwords would be a development aid
/// nobody should run twice.
fn try_the_dialog() -> Result<(), String> {
    let prompt = paint::Prompt {
        message: "Authentication is required to manage system services or other units.".to_string(),
        action: "org.freedesktop.systemd1.manage-units".to_string(),
        user: std::env::var("USER").unwrap_or_else(|_| "somebody".to_string()),
        typed: 0,
        refused: false,
    };
    let families = vec!["Anonymous Pro".to_string(), "Noto Sans".to_string()];
    match dialog::ask(prompt, &families)? {
        dialog::Outcome::Entered(secret) => {
            println!("entered {} characters", secret.chars().count());
        }
        dialog::Outcome::Dismissed => println!("dismissed"),
    }
    Ok(())
}
