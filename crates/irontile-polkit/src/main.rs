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
    --version       Show the version and exit.
    --help          Show this message.
";

mod agent;
mod helper;
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

    let listener = agent::Listener {
        ask: Box::new(ask_on_the_terminal),
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
