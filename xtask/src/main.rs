//! Task runner for the irontile workspace.
//!
//! Invoked as `cargo xtask <subcommand>` via the alias in `.cargo/config.toml`.
//! Deliberately dependency-free: argument parsing is hand-rolled so the build
//! graph for `cargo xtask` stays as small as the workspace itself.

use std::env;
use std::process::{Command, ExitCode};

const HELP: &str = "\
cargo xtask - irontile task runner

USAGE:
    cargo xtask <SUBCOMMAND> [OPTIONS] [-- <ARGS>...]

SUBCOMMANDS:
    run     Build and launch the compositor. Everything after `--` is forwarded
            to the compositor binary.
    test    Verify the workspace: rustfmt check, clippy (warnings denied), then
            the full test suite.

OPTIONS (test):
    --skip-lints    Run only the test suite, skipping rustfmt and clippy.
    --skip-tests    Run only rustfmt and clippy.

OPTIONS (run):
    --release       Build with optimizations.
";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let (subcommand, rest) = match args.split_first() {
        Some((first, rest)) => (first.as_str(), rest),
        None => {
            print!("{HELP}");
            return Err("missing subcommand".into());
        }
    };

    match subcommand {
        "run" => cmd_run(rest),
        "test" => cmd_test(rest),
        "help" | "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        other => {
            print!("{HELP}");
            Err(format!("unknown subcommand `{other}`"))
        }
    }
}

fn cmd_run(args: &[String]) -> Result<(), String> {
    // Everything after a bare `--` belongs to the compositor, not to us.
    let (ours, theirs) = split_forwarded(args);

    let mut cargo = cargo();
    cargo.args(["run", "--package", "irontile-comp"]);
    for flag in ours {
        match flag.as_str() {
            "--release" => {
                cargo.arg("--release");
            }
            other => return Err(format!("unknown flag for `run`: {other}")),
        }
    }
    if !theirs.is_empty() {
        cargo.arg("--");
        cargo.args(theirs);
    }
    exec(cargo)
}

fn cmd_test(args: &[String]) -> Result<(), String> {
    let mut lints = true;
    let mut tests = true;
    for flag in args {
        match flag.as_str() {
            "--skip-lints" => lints = false,
            "--skip-tests" => tests = false,
            other => return Err(format!("unknown flag for `test`: {other}")),
        }
    }

    if lints {
        let mut fmt = cargo();
        fmt.args(["fmt", "--all", "--check"]);
        exec(fmt)?;

        let mut clippy = cargo();
        clippy.args([
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ]);
        exec(clippy)?;
    }

    if tests {
        let mut test = cargo();
        test.args(["test", "--workspace", "--all-features"]);
        exec(test)?;
    }

    Ok(())
}

/// Splits `args` at the first bare `--`, returning (before, after).
fn split_forwarded(args: &[String]) -> (&[String], &[String]) {
    match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => (args, &[]),
    }
}

fn cargo() -> Command {
    let mut cmd = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(workspace_root());
    cmd
}

fn workspace_root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR is `<root>/xtask`.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask manifest always has a parent")
        .to_path_buf()
}

fn exec(mut cmd: Command) -> Result<(), String> {
    eprintln!("+ {}", render(&cmd));
    let status = cmd
        .status()
        .map_err(|e| format!("failed to spawn `{}`: {e}", render(&cmd)))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{}` exited with {status}", render(&cmd)))
    }
}

fn render(cmd: &Command) -> String {
    let mut out = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        out.push(' ');
        out.push_str(&arg.to_string_lossy());
    }
    out
}
