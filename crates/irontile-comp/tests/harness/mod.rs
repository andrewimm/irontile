//! Runs a real compositor and talks to it over its control socket.
//!
//! The headless backend needs no display server, so these run anywhere,
//! including in CI. Each test gets its own compositor process with its own
//! Wayland socket name, so they neither interfere with each other nor with a
//! session already running on the machine.

// This module is compiled into each test binary separately, so whatever one of
// them does not use looks dead from that binary's point of view.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub mod client;

pub use client::TestClient;
use irontile_ipc::Client;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

pub struct Compositor {
    child: Child,
    pub client: Client,
    control_socket: String,
    wayland_socket: String,
    runtime: std::path::PathBuf,
}

impl Compositor {
    /// Starts a compositor with the given display spec, such as `"1920x1080"`
    /// or `"1920x1080,1280x1024"`.
    pub fn start(displays: &str) -> Compositor {
        Compositor::with_config(displays, None)
    }

    /// Starts a compositor reading a specific configuration file.
    pub fn with_config(displays: &str, config: Option<&std::path::Path>) -> Compositor {
        // Once, and shared with the child: two calls would now be two different
        // directories, and the compositor would bind its sockets somewhere the
        // test is not looking.
        let runtime = runtime_dir();
        let mut command = Command::new(env!("CARGO_BIN_EXE_irontile"));
        command
            .arg("--headless")
            .arg(displays)
            .env("RUST_LOG", "irontile=info")
            // Both the Wayland socket and the control socket live under this,
            // and a bare CI runner has none.
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match config {
            Some(path) => {
                command.arg("--config").arg(path);
            }
            None => {
                // A configuration left over from the developer's own session
                // would make these tests depend on the machine they run on.
                command.arg("--config").arg("/nonexistent/irontile.toml");
            }
        }

        let mut child = command.spawn().expect("failed to start irontile");
        let Startup { control, wayland } = read_startup(&mut child);
        let socket = control;

        // The socket file appears a moment after the line reporting it.
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        let client = loop {
            match Client::connect(&socket) {
                Ok(client) => break client,
                Err(err) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        panic!("could not connect to {socket}: {err}");
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };

        let mut compositor = Compositor {
            child,
            client,
            control_socket: socket,
            wayland_socket: wayland,
            runtime,
        };
        compositor
            .client
            .set_timeout(Some(Duration::from_secs(10)))
            .expect("failed to set a timeout");
        compositor
    }
}

impl Compositor {
    /// The compositor process, for tests that need to look at it from outside.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Opens another control connection, for tests that need a second one.
    pub fn connect_control(&self) -> Client {
        Client::connect(&self.control_socket).expect("could not reach the control socket")
    }

    /// Connects a real Wayland client to this compositor.
    pub fn connect_client(&self) -> TestClient {
        TestClient::connect(&self.wayland_socket, &self.runtime)
    }

    /// Blocks until a condition holds, or fails the test.
    pub fn wait_for(&mut self, mut done: impl FnMut(&mut Compositor) -> bool) {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            if done(self) {
                return;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting on the compositor");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Blocks until the compositor reports exactly `count` placed windows.
    ///
    /// Mapping is asynchronous: the client commits a buffer, the compositor
    /// notices on its next dispatch. Polling the frame is what makes a test
    /// deterministic without sleeping for a guessed interval.
    pub fn wait_for_windows(&mut self, count: usize) -> irontile_ipc::Frame {
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        loop {
            let frame = self.client.frame().expect("failed to query the frame");
            if frame.placements.len() == count {
                return frame;
            }
            if Instant::now() >= deadline {
                panic!(
                    "expected {count} windows, saw {}: {:?}",
                    frame.placements.len(),
                    frame.placements
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        // Asked to quit before being killed, so the ordinary exit path runs and
        // the sockets the compositor made are removed by the code that made
        // them. A killed process runs no destructors, so tests that only ever
        // killed it were routing around the cleanup they should be covering.
        let _ = self.client.action(irontile_ipc::Action::Quit);
        let deadline = Instant::now() + SHUTDOWN;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.tidy();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
        // It did not go, so it is made to. A test that wedged the compositor
        // must not also wedge the test run.
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.tidy();
    }
}

impl Compositor {
    /// Removes what the compositor leaves behind.
    ///
    /// Its own control socket goes on the way out, but the Wayland socket is
    /// smithay's and is left on the floor -- harmless in a session, where the
    /// lock file beside it lets the next start reclaim the name, and so much
    /// litter across a test run that used to bury the one socket that mattered.
    ///
    /// The directory goes too, but only when it is empty: tests in one process
    /// share it, so this succeeds for whichever compositor is the last to go
    /// and fails harmlessly for the rest.
    fn tidy(&self) {
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

/// How long a compositor is given to exit on its own before it is killed.
const SHUTDOWN: Duration = Duration::from_millis(500);

/// A runtime directory for one compositor's sockets.
///
/// Always private, never the session's. A test compositor binds a real Wayland
/// display name, and doing that in `XDG_RUNTIME_DIR` puts throwaway compositors
/// in the namespace the desktop's own clients resolve names from -- shared
/// mutable state between the test suite and whatever is logged in.
///
/// One per compositor rather than one per test run, so each owns what it puts
/// there and can take the whole directory with it. Tests run in parallel in one
/// process, and a directory they shared could be removed by one of them while
/// another was still starting inside it.
///
/// Kept short on purpose: a unix socket path is limited to about a hundred
/// bytes, and a long directory here is spent before the socket is even named.
fn runtime_dir() -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "irt-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).expect("failed to create a runtime directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // A socket anyone can reach would let anyone drive the session.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    }
    path
}

struct Startup {
    control: String,
    wayland: String,
}

/// Keeps reading the compositor's output so it never blocks on a full pipe.
///
/// Set `IRONTILE_TEST_LOG` to see it; a passing test has nothing to say and
/// several running at once would interleave into nonsense.
fn drain(mut reader: BufReader<std::process::ChildStdout>) {
    let echo = std::env::var_os("IRONTILE_TEST_LOG").is_some();
    std::thread::spawn(move || {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {
                    if echo {
                        eprint!("compositor: {line}");
                    }
                }
            }
        }
    });
}

/// Reads the startup line and pulls both socket names out of it.
fn read_startup(child: &mut Child) -> Startup {
    let stdout = child.stdout.take().expect("stdout was piped");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let deadline = Instant::now() + STARTUP_TIMEOUT;

    while Instant::now() < deadline {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                let field = |key: &str| {
                    line.split(key)
                        .nth(1)
                        .and_then(|rest| rest.split_whitespace().next())
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                };
                if let (Some(control), Some(wayland)) = (field("control="), field("socket=")) {
                    // Everything the compositor says after this goes somewhere,
                    // because a pipe nobody reads fills up and then the
                    // compositor blocks trying to write to it -- which looks
                    // like a compositor that has stopped responding.
                    drain(reader);
                    return Startup { control, wayland };
                }
            }
            Err(err) => panic!("failed to read compositor output: {err}"),
        }
    }
    let _ = child.kill();
    panic!("compositor did not report its sockets");
}

/// A configuration file that cleans itself up.
pub struct TempConfig {
    path: std::path::PathBuf,
}

impl TempConfig {
    pub fn new(contents: &str) -> TempConfig {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "irontile-test-{}-{}.toml",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, contents).expect("failed to write a test config");
        TempConfig { path }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Rewrites the file, for testing a reload.
    pub fn rewrite(&self, contents: &str) {
        std::fs::write(&self.path, contents).expect("failed to rewrite a test config");
    }
}

impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
