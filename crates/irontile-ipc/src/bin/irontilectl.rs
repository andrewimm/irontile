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
    layout              The whole layout engine state, as JSON
    watch               Stream events until interrupted
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

fn run(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let Some(first) = args.first() else {
        print!("{HELP}");
        return Err("no action given".into());
    };

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
