//! Command line control for a running irontile.

use std::process::ExitCode;

use irontile_ipc::{Action, Client, ClientError, Query, ResponsePayload, VERBS, parse_action};

const HELP: &str = "\
irontilectl - control a running irontile

USAGE:
    irontilectl <ACTION>...
    irontilectl <SUBCOMMAND>

SUBCOMMANDS:
    frame               Where every window is right now
    outputs             Connected displays and their arrangement
    workspaces          Every desktop, and what is on it
    windows             Every window, with its title and application id
    layers              Panels and overlays on screen, and what they reserve
    layout              The whole layout engine state, as JSON
    watch               Stream events until interrupted
    version             Show the version and exit
    help                Show this message

ACTIONS:
    Anything the config file accepts as a binding. Words are joined, so both
    `irontilectl focus left` and `irontilectl \"focus left\"` work.

ENVIRONMENT:
    IRONTILE_SOCKET     Socket to connect to; otherwise derived from
                        WAYLAND_DISPLAY.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("irontilectl: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Asks the compositor on the other end of the socket what build it is.
///
/// Separate from this binary's own version on purpose: the number printed
/// above comes from the file on disk, and the session was started from
/// whatever was on disk at the time, which an upgrade since has replaced
/// without touching the running process.
///
/// Never an error, because "what is running" still has an answer when the
/// answer is "nothing" or "something too old to say" -- and a version command
/// that fails is a version command nobody can put in a bug report.
fn running_compositor() -> String {
    let mut client = match Client::connect_default() {
        Ok(client) => client,
        Err(why) => return format!("irontile    nothing to ask: {why}"),
    };
    match client.query(Query::Version) {
        Ok(ResponsePayload::Version {
            name,
            version,
            commit,
        }) => match commit {
            Some(commit) => format!("{name}    {version} ({commit})"),
            None => format!("{name}    {version}"),
        },
        Ok(ResponsePayload::Error { message }) => {
            format!("irontile    would not say: {message}")
        }
        Ok(other) => format!("irontile    answered something else: {other:?}"),
        // A compositor from before this question existed cannot parse it and
        // drops the connection rather than replying, so a closed connection
        // here means an older session rather than a broken one. Saying which
        // is the whole point: an old compositor is exactly what somebody
        // checking versions is trying to find out about.
        Err(why) => format!(
            "irontile    did not answer ({why}); \
             a session older than this question cannot be asked"
        ),
    }
}

fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let Some(first) = args.first() else {
        print!("{HELP}");
        return Err("no action given".into());
    };

    if matches!(first.as_str(), "version" | "-V" | "--version") {
        // This binary first, because it answers even with nothing running --
        // and then the compositor, which is the one somebody actually wanted.
        // They are separate binaries and routinely not from the same build:
        // installing a package replaces both on disk and neither in memory,
        // so the session keeps running whatever it started as.
        println!(
            "{}",
            irontile_version::line("irontilectl", env!("CARGO_PKG_VERSION"))
        );
        println!("{}", running_compositor());
        return Ok(());
    }

    if matches!(first.as_str(), "help" | "-h" | "--help") {
        print!("{HELP}");
        println!("Actions:");
        for verb in VERBS {
            println!("    {verb}");
        }
        return Ok(());
    }

    let mut client = Client::connect_default()?;

    match first.as_str() {
        "frame" => show(client.query(Query::Frame)?),
        "outputs" => show(client.query(Query::Outputs)?),
        "workspaces" => show(client.query(Query::Workspaces)?),
        "windows" => show(client.query(Query::Windows)?),
        "layers" => show(client.query(Query::Layers)?),
        "layout" => show(client.query(Query::Layout)?),
        "watch" => watch(&mut client)?,
        _ => {
            // Join so that both a quoted action and loose words work.
            let text = args.join(" ");
            let action: Action = parse_action(&text)?;
            let events = client.action(action)?;
            for event in events {
                println!("{}", serde_json::to_string(&event)?);
            }
        }
    }
    Ok(())
}

fn show(payload: ResponsePayload) {
    // Pretty JSON: the output of these is meant to be read, and piped into jq.
    match serde_json::to_string_pretty(&payload) {
        Ok(text) => println!("{text}"),
        Err(err) => eprintln!("irontilectl: {err}"),
    }
}

fn watch(client: &mut Client) -> Result<(), Box<dyn std::error::Error>> {
    client.subscribe()?;
    loop {
        match client.next_event() {
            Ok(event) => println!("{}", serde_json::to_string(&event)?),
            // A clean shutdown of the compositor is not a failure of `watch`.
            Err(ClientError::Closed) => return Ok(()),
            Err(err) => return Err(err.into()),
        }
    }
}
