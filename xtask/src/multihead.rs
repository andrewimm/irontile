//! The multi-display probe.
//!
//! Driven over the control socket rather than by pressing keys, so a test run
//! produces a log that says what happened instead of relying on someone
//! remembering what they pressed. Started as an irontile startup command, so it
//! inherits `IRONTILE_SOCKET` and runs inside the session it is testing.

use std::fmt::Write as _;
use std::io::Write as _;
use std::time::Duration;

use irontile_ipc::{
    Action, Client, Direction, Layout, Output, Query, ResponsePayload, WorkspaceSummary,
};

/// How long to watch for a monitor being unplugged and plugged back in.
const WATCH: Duration = Duration::from_secs(120);
const SETTLE: Duration = Duration::from_millis(800);

pub fn run(args: &[String]) -> Result<(), String> {
    let path = args
        .iter()
        .position(|a| a == "--log")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/irontile-multihead.log")
        });

    let mut report = Report::new(&path)?;
    let mut client =
        Client::connect_default().map_err(|e| format!("could not reach the compositor: {e}"))?;
    client
        .set_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("{e}"))?;

    // Let the compositor finish coming up before asking it anything.
    std::thread::sleep(Duration::from_secs(3));

    report.section("displays");
    let outputs = outputs(&mut client)?;
    report.displays(&outputs);
    report.section("desktops");
    report.desktops(&workspaces(&mut client)?);

    if outputs.len() < 2 {
        report.line("only one display; the external monitor was not detected");
        return Ok(());
    }

    report.section("send the focused desktop to the display on the right");
    act(&mut client, Action::SendToOutput(Direction::Right))?;
    report.desktops(&workspaces(&mut client)?);
    report.invariants(&layout(&mut client)?);

    // Sending a desktop away moves focus to the display it left behind, so
    // focus has to follow it before it can be sent back. Without this the
    // second step silently does nothing, which reads as a pass.
    report.section("follow it right, then send it back to the left");
    act(&mut client, Action::FocusOutput(Direction::Right))?;
    act(&mut client, Action::SendToOutput(Direction::Left))?;
    report.desktops(&workspaces(&mut client)?);
    report.invariants(&layout(&mut client)?);

    report.section("open a window on each display");
    act(&mut client, Action::Terminal)?;
    std::thread::sleep(Duration::from_secs(2));
    act(&mut client, Action::FocusOutput(Direction::Right))?;
    act(&mut client, Action::Terminal)?;
    std::thread::sleep(Duration::from_secs(2));
    report.windows(&mut client)?;

    report.section("move the focused window left, across the display boundary");
    act(&mut client, Action::MoveWindow(Direction::Left))?;
    report.windows(&mut client)?;
    report.invariants(&layout(&mut client)?);

    report.section("focus across the boundary and back");
    act(&mut client, Action::Focus(Direction::Right))?;
    report.windows(&mut client)?;
    act(&mut client, Action::Focus(Direction::Left))?;
    report.windows(&mut client)?;

    report.section("now unplug the external monitor, wait, and plug it back in");
    report.line("watching for two minutes; every change is recorded below");
    watch_hotplug(&mut client, &mut report)?;

    report.section("finished");
    Ok(())
}

/// Records the arrangement whenever it changes, which is what hotplug looks
/// like from the outside.
fn watch_hotplug(client: &mut Client, report: &mut Report) -> Result<(), String> {
    let started = std::time::Instant::now();
    let deadline = started + WATCH;
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        let outputs = outputs(client)?;
        let desktops = workspaces(client)?;
        let mut snapshot = String::new();
        for output in &outputs {
            let _ = writeln!(snapshot, "{} {:?}", output.name, output.logical);
        }
        for ws in &desktops {
            let _ = writeln!(snapshot, "ws {:?} on {:?}", ws.name, ws.output);
        }
        if snapshot != last {
            last = snapshot;
            report.line(&format!(
                "--- changed at t+{}s ---",
                started.elapsed().as_secs()
            ));
            report.displays(&outputs);
            report.desktops(&desktops);
            report.invariants(&layout(client)?);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}

fn act(client: &mut Client, action: Action) -> Result<(), String> {
    client
        .action(action)
        .map_err(|e| format!("action failed: {e}"))?;
    std::thread::sleep(SETTLE);
    Ok(())
}

fn outputs(client: &mut Client) -> Result<Vec<Output>, String> {
    match client.query(Query::Outputs).map_err(|e| e.to_string())? {
        ResponsePayload::Outputs(outputs) => Ok(outputs),
        other => Err(format!("expected outputs, got {other:?}")),
    }
}

fn workspaces(client: &mut Client) -> Result<Vec<WorkspaceSummary>, String> {
    match client.query(Query::Workspaces).map_err(|e| e.to_string())? {
        ResponsePayload::Workspaces(w) => Ok(w),
        other => Err(format!("expected workspaces, got {other:?}")),
    }
}

fn layout(client: &mut Client) -> Result<Layout, String> {
    client.layout().map_err(|e| e.to_string())
}

/// Writes the log, and echoes it so a terminal shows progress too.
struct Report {
    file: std::fs::File,
}

impl Report {
    fn new(path: &str) -> Result<Report, String> {
        let file =
            std::fs::File::create(path).map_err(|e| format!("could not write {path}: {e}"))?;
        println!("multihead: logging to {path}");
        Ok(Report { file })
    }

    fn line(&mut self, text: &str) {
        println!("{text}");
        let _ = writeln!(self.file, "{text}");
        let _ = self.file.flush();
    }

    fn section(&mut self, title: &str) {
        self.line("");
        self.line(&format!("=== {title} ==="));
    }

    fn displays(&mut self, outputs: &[Output]) {
        for output in outputs {
            let (l, w) = (output.logical, output.work_area);
            self.line(&format!(
                "  {:<12} id={} logical={}x{}+{}+{} work={}x{}+{}+{}",
                output.name, output.id.0, l.w, l.h, l.x, l.y, w.w, w.h, w.x, w.y
            ));
        }
    }

    fn desktops(&mut self, workspaces: &[WorkspaceSummary]) {
        for ws in workspaces {
            self.line(&format!(
                "  ws {} name={:?} output={:?} focused={} windows={}",
                ws.id.0,
                ws.name,
                ws.output.map(|o| o.0),
                ws.focused,
                ws.windows.len()
            ));
        }
    }

    fn windows(&mut self, client: &mut Client) -> Result<(), String> {
        let frame = client.frame().map_err(|e| e.to_string())?;
        if frame.placements.is_empty() {
            self.line("  (no windows)");
        }
        for p in &frame.placements {
            self.line(&format!(
                "  win {} output={} {}x{}+{}+{} focused={}",
                p.window.0, p.output.0, p.rect.w, p.rect.h, p.rect.x, p.rect.y, p.focused
            ));
        }
        Ok(())
    }

    /// The engine validates itself; this reports the result and the couple of
    /// cross-display properties a caller can check from outside.
    fn invariants(&mut self, layout: &Layout) {
        match layout.validate() {
            Ok(()) => {}
            Err(err) => {
                self.line(&format!("  INVARIANT VIOLATED: {err}"));
                return;
            }
        }
        let displays = layout.outputs().len();
        let shown: Vec<_> = layout
            .outputs()
            .iter()
            .filter_map(|o| layout.active_workspace(o.id))
            .collect();
        let mut unique = shown.clone();
        unique.sort_by_key(|w| w.0);
        unique.dedup();
        if shown.len() != displays {
            self.line("  INVARIANT VIOLATED: a display is showing nothing");
        } else if unique.len() != shown.len() {
            self.line("  INVARIANT VIOLATED: a desktop is on two displays");
        } else {
            self.line(&format!(
                "  invariants ok ({displays} displays, all occupied)"
            ));
        }
    }
}
