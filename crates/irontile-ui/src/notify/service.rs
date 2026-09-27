//! The bus side: taking notifications in, and saying what became of them.
//!
//! `org.freedesktop.Notifications` is how every program on the desktop asks
//! for a word to appear on screen. It is a small interface and an old one, and
//! the parts that matter are the parts that are easy to get subtly wrong: an
//! id that must be unique and must be reused when a sender says so, a close
//! that must be reported with a reason, and an action that must go back to the
//! sender rather than being acted on here.
//!
//! The bus lives on a thread of its own. Everything it learns goes into shared
//! state and a byte down a pipe; the surface loop polls that pipe alongside
//! Wayland's socket, so neither side ever waits on the other.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use zbus::zvariant::Value;

use crate::module::Pixels;
use crate::notify::model::{Action, Notification, Urgency};

/// Why a notification stopped being shown. The numbers are the
/// specification's, and senders do read them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    Expired = 1,
    Dismissed = 2,
    ByRequest = 3,
    Undefined = 4,
}

/// Something the surface decided, on its way back to the sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Closed(u32, Closed),
    Invoked(u32, String),
}

/// What both sides can see.
#[derive(Debug, Default)]
pub struct Shared {
    /// Everything not yet closed, oldest first.
    pub live: Vec<Notification>,
    /// Ids that arrived while quiet hours were on, so they go to the panel
    /// without ever appearing as a popup.
    pub silent: Vec<u32>,
    pub quiet: bool,
    /// Set when the panel should be open. The binding toggles it through the
    /// bus, because a key press arrives at the compositor rather than here.
    pub panel: bool,
}

/// The half of the daemon that talks to the bus.
#[derive(Debug)]
pub struct Service {
    pub shared: Arc<Mutex<Shared>>,
    /// Readable whenever something changed, so a poll loop can wait on it.
    pub wake: OwnedFd,
    replies: Sender<Reply>,
}

impl Service {
    /// Tells the sender what became of one of its notifications.
    pub fn reply(&self, reply: Reply) {
        let _ = self.replies.send(reply);
    }

    pub fn state(&self) -> Shared {
        match self.shared.lock() {
            Ok(guard) => Shared {
                live: guard.live.clone(),
                silent: guard.silent.clone(),
                quiet: guard.quiet,
                panel: guard.panel,
            },
            Err(_) => Shared::default(),
        }
    }

    /// Drains the wake pipe. Whatever was in it, the answer is the same: look
    /// at the shared state again.
    pub fn drain(&self) {
        let mut buffer = [0_u8; 64];
        while rustix::io::read(&self.wake, &mut buffer).is_ok_and(|n| n > 0) {}
    }
}

/// The object on the bus.
struct Notifications {
    shared: Arc<Mutex<Shared>>,
    wake: Arc<OwnedFd>,
    next: u32,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Notifications {
    /// What this server can do, which senders check before using it.
    ///
    /// Said honestly: claiming `body-markup` and then showing the tags would
    /// be worse than not claiming it, and `persistence` is a promise that
    /// notifications survive in a panel, which these do.
    fn get_capabilities(&self) -> Vec<String> {
        ["actions", "body", "icon-static", "persistence"]
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "irontile-notify".to_string(),
            "irontile".to_string(),
            env!("CARGO_PKG_VERSION").to_string(),
            // The version of the specification, not of anything here.
            "1.2".to_string(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        app_name: String,
        replaces_id: u32,
        app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        hints: HashMap<String, Value<'_>>,
        _expire_timeout: i32,
    ) -> u32 {
        // Zero means "give me a new one"; anything else names a notification
        // to replace in place, which is how a music player updates one line
        // rather than filling the screen with every track it plays.
        let id = if replaces_id == 0 {
            self.next = self.next.wrapping_add(1).max(1);
            self.next
        } else {
            replaces_id
        };

        let urgency = hints
            .get("urgency")
            .and_then(|value| u8::try_from(value.try_clone().ok()?).ok())
            .map(Urgency::from_hint)
            .unwrap_or_default();

        // A picture beats a name, and a name in `image-path` beats the icon the
        // sender gave as an argument: the hint is the more specific of the two
        // and is what a sender uses when it has something particular to show.
        let (drawn, named) = image_by_path(&hints);
        let image = image_from_hints(&hints).or(drawn);

        let note = Notification {
            id,
            app: app_name,
            icon: named.or((!app_icon.is_empty()).then_some(app_icon)),
            summary,
            body,
            urgency,
            actions: pairs(&actions),
            has_default: has_default(&actions),
            image,
            age: std::time::Duration::ZERO,
        };

        if let Ok(mut shared) = self.shared.lock() {
            if shared.quiet && urgency != Urgency::Critical {
                // Held back rather than dropped: quiet hours is about not
                // being interrupted, not about not being told.
                shared.silent.push(id);
            }
            match shared.live.iter_mut().find(|held| held.id == id) {
                Some(held) => *held = note,
                None => shared.live.push(note),
            }
        }
        nudge(&self.wake);
        id
    }

    fn close_notification(&self, id: u32) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.live.retain(|note| note.id != id);
        }
        nudge(&self.wake);
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        id: u32,
        action_key: &str,
    ) -> zbus::Result<()>;
}

/// The panel's own switch, which is not part of anybody's specification.
///
/// A key binding reaches the compositor, not this process, so the binding runs
/// `irontile-notify --toggle` and that call arrives here.
struct Panel {
    shared: Arc<Mutex<Shared>>,
    wake: Arc<OwnedFd>,
}

#[zbus::interface(name = "org.irontile.Notify1")]
impl Panel {
    fn toggle(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.panel = !shared.panel;
        }
        nudge(&self.wake);
    }

    fn open(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.panel = true;
        }
        nudge(&self.wake);
    }

    fn close(&self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.panel = false;
        }
        nudge(&self.wake);
    }

    /// Holds notifications back, or stops holding them.
    fn quiet(&self, on: bool) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.quiet = on;
        }
        nudge(&self.wake);
    }

    /// How many notifications are being held, for a bar to show.
    #[zbus(property)]
    fn count(&self) -> u32 {
        self.shared
            .lock()
            .map(|shared| shared.live.len() as u32)
            .unwrap_or(0)
    }

    /// Whether notifications are being held back rather than shown.
    #[zbus(property(emits_changed_signal = "false"), name = "Quiet")]
    fn quiet_now(&self) -> bool {
        self.shared
            .lock()
            .map(|shared| shared.quiet)
            .unwrap_or(false)
    }
}

/// The pixels a sender sent instead of naming an icon.
///
/// `image-data` is a struct of width, height, row stride, whether there is an
/// alpha channel, bits per sample, channels, and the bytes. Only eight bits a
/// sample is worth handling: nothing has sent anything else this century, and
/// guessing at a format nobody uses would be code nobody could test.
///
/// The rows are strided and the channels are RGB or RGBA, where what draws
/// them wants tightly packed ARGB, so this is the conversion as well as the
/// parsing. `image_data` and `icon_data` are the same thing under the names
/// older versions of the specification used.
pub fn image_from_hints(hints: &HashMap<String, Value<'_>>) -> Option<Pixels> {
    let value = ["image-data", "image_data", "icon_data"]
        .into_iter()
        .find_map(|name| hints.get(name))?;
    let (width, height, stride, has_alpha, bits, channels, data): (
        i32,
        i32,
        i32,
        bool,
        i32,
        i32,
        Vec<u8>,
    ) = value.try_clone().ok()?.try_into().ok()?;

    if bits != 8 || !(3..=4).contains(&channels) {
        return None;
    }
    let (w, h, stride) = (
        usize::try_from(width).ok()?,
        usize::try_from(height).ok()?,
        usize::try_from(stride).ok()?,
    );
    let channels = channels as usize;
    if w == 0 || h == 0 || stride < w * channels || data.len() < stride * (h - 1) + w * channels {
        return None;
    }

    let mut argb = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        let row = &data[y * stride..];
        for x in 0..w {
            let pixel = &row[x * channels..];
            let alpha = if has_alpha && channels == 4 {
                pixel[3]
            } else {
                255
            };
            argb.extend_from_slice(&[alpha, pixel[0], pixel[1], pixel[2]]);
        }
    }
    Some(Pixels {
        width: w as u32,
        height: h as u32,
        argb: argb.into(),
    })
}

/// The picture a sender named by path, rather than sending its pixels.
///
/// `image-path` holds either a file to load or an icon name to look up, and
/// the specification lets a sender use it for both. A leading slash or a
/// `file://` is the difference; anything else goes back to the caller to be
/// resolved against the icon theme like any other name.
///
/// Only PNG is loaded. It is what senders use, and reaching for a general
/// image library to cover the ones that do not would be a great deal of
/// machinery for a case nobody has.
pub fn image_by_path(hints: &HashMap<String, Value<'_>>) -> (Option<Pixels>, Option<String>) {
    let Some(text) = ["image-path", "image_path"]
        .into_iter()
        .find_map(|name| hints.get(name))
        .and_then(|value| String::try_from(value.try_clone().ok()?).ok())
    else {
        return (None, None);
    };
    let path = match text.strip_prefix("file://") {
        Some(rest) => rest.to_string(),
        None if text.starts_with('/') => text.clone(),
        // Not a path at all: a name for the theme.
        None => return (None, Some(text)),
    };
    let Ok(pixmap) = tiny_skia::Pixmap::load_png(&path) else {
        return (None, None);
    };
    let mut argb = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.pixels() {
        // tiny-skia holds premultiplied colour; the tray's format is straight
        // ARGB, so this undoes the multiplication rather than showing a dark
        // halo wherever an icon is translucent.
        let colour = pixel.demultiply();
        argb.extend_from_slice(&[colour.alpha(), colour.red(), colour.green(), colour.blue()]);
    }
    (
        Some(Pixels {
            width: pixmap.width(),
            height: pixmap.height(),
            argb: argb.into(),
        }),
        None,
    )
}

/// Splits the flat list the specification uses into pairs.
///
/// Actions arrive as key, label, key, label. A trailing key with no label is a
/// sender's mistake and is dropped rather than shown as a button with no word
/// on it. `default` is the action a click on the body invokes, and it is never
/// drawn as a button of its own.
pub fn pairs(flat: &[String]) -> Vec<Action> {
    flat.as_chunks::<2>()
        .0
        .iter()
        .filter(|pair| pair[0] != "default")
        .map(|pair| Action {
            key: pair[0].clone(),
            label: pair[1].clone(),
        })
        .collect()
}

/// Whether a notification carries a default action, which a click invokes.
pub fn has_default(flat: &[String]) -> bool {
    flat.as_chunks::<2>()
        .0
        .iter()
        .any(|pair| pair[0] == "default")
}

fn nudge(wake: &OwnedFd) {
    let _ = rustix::io::write(wake, &[1]);
}

/// Starts the bus half on a thread of its own.
///
/// Fails when the name is already taken, which means another notification
/// daemon is running: two of them would each show half the notifications, and
/// the one to fix that is whoever started the second.
pub fn start() -> Result<Service, String> {
    let (read, write) = rustix::pipe::pipe_with(
        // Never blocking. A drain happens when poll says the pipe is readable,
        // and a blocking read on an empty one would stop the surface loop
        // dead -- no redraws, no pointer, with the process still running.
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    )
    .map_err(|err| format!("could not make a wake pipe: {err}"))?;

    let shared = Arc::new(Mutex::new(Shared::default()));
    let (replies, inbox) = std::sync::mpsc::channel();
    let write = Arc::new(write);

    let ready = std::sync::mpsc::channel();
    let thread_shared = Arc::clone(&shared);
    let thread_wake = Arc::clone(&write);
    std::thread::Builder::new()
        .name("irontile-notify bus".into())
        .spawn(move || {
            let outcome = serve(thread_shared, thread_wake, &inbox, &ready.0);
            if let Err(err) = outcome {
                eprintln!("irontile-notify: {err}");
            }
        })
        .map_err(|err| format!("could not start the bus thread: {err}"))?;

    // The name is taken or it is not, and the answer decides whether this
    // process has any business continuing. Waiting for it here means the
    // failure is a message on a terminal rather than a daemon that is running
    // and silently doing nothing.
    match ready.1.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(Ok(())) => Ok(Service {
            shared,
            wake: read,
            replies,
        }),
        Ok(Err(err)) => Err(err),
        Err(_) => Err("the session bus did not answer".into()),
    }
}

fn serve(
    shared: Arc<Mutex<Shared>>,
    wake: Arc<OwnedFd>,
    inbox: &Receiver<Reply>,
    ready: &Sender<Result<(), String>>,
) -> Result<(), String> {
    let notifications = Notifications {
        shared: Arc::clone(&shared),
        wake: Arc::clone(&wake),
        next: 0,
    };
    let panel = Panel {
        shared: Arc::clone(&shared),
        wake,
    };

    let connection = zbus::blocking::connection::Builder::session()
        .and_then(|builder| builder.serve_at("/org/freedesktop/Notifications", notifications))
        .and_then(|builder| builder.serve_at("/org/irontile/Notify", panel))
        .and_then(|builder| builder.name("org.freedesktop.Notifications"))
        .and_then(|builder| builder.name("org.irontile.Notify"))
        .and_then(zbus::blocking::connection::Builder::build);

    let connection = match connection {
        Ok(connection) => {
            let _ = ready.send(Ok(()));
            connection
        }
        Err(err) => {
            let message = format!(
                "could not take org.freedesktop.Notifications: {err}\n\
                 Something else is already the notification daemon."
            );
            let _ = ready.send(Err(message.clone()));
            return Err(message);
        }
    };

    // Everything the surface decided, sent back out as the signals the
    // specification says a sender may wait for. Emitted by hand rather than
    // through the generated helpers, because those are async and this thread
    // is the blocking one: the signal is four values on a path, and saying so
    // outright is shorter than bridging two worlds to say it.
    for reply in inbox {
        let result = match reply {
            Reply::Closed(id, reason) => connection.emit_signal(
                None::<&str>,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "NotificationClosed",
                &(id, reason as u32),
            ),
            Reply::Invoked(id, key) => connection.emit_signal(
                None::<&str>,
                "/org/freedesktop/Notifications",
                "org.freedesktop.Notifications",
                "ActionInvoked",
                &(id, key.as_str()),
            ),
        };
        if let Err(err) = result {
            eprintln!("irontile-notify: could not answer the sender: {err}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_arrive_flat_and_come_out_in_pairs() {
        let flat = ["open".to_string(), "Open folder".to_string()];
        let actions = pairs(&flat);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].key, "open");
        assert_eq!(actions[0].label, "Open folder");
    }

    #[test]
    fn the_default_action_is_not_a_button() {
        // It is what a click on the notification itself invokes, and drawing
        // it beside the others would offer the same thing twice.
        let flat = [
            "default".to_string(),
            "Open".to_string(),
            "reply".to_string(),
            "Reply".to_string(),
        ];
        let actions = pairs(&flat);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].key, "reply");
        assert!(has_default(&flat));
    }

    #[test]
    fn a_key_with_no_label_is_dropped_rather_than_drawn_empty() {
        let flat = ["open".to_string(), "Open".to_string(), "orphan".to_string()];
        assert_eq!(pairs(&flat).len(), 1);
        assert!(!has_default(&flat));
    }
}

#[cfg(test)]
mod images {
    use super::*;

    fn hint(
        width: i32,
        height: i32,
        stride: i32,
        alpha: bool,
        channels: i32,
        data: Vec<u8>,
    ) -> HashMap<String, Value<'static>> {
        let mut hints = HashMap::new();
        hints.insert(
            "image-data".to_string(),
            Value::from((width, height, stride, alpha, 8_i32, channels, data)),
        );
        hints
    }

    #[test]
    fn rgb_pixels_become_opaque_argb_ones() {
        // Two pixels: red, then green.
        let pixels = image_from_hints(&hint(2, 1, 6, false, 3, vec![255, 0, 0, 0, 255, 0]))
            .expect("two pixels of red and green");
        assert_eq!((pixels.width, pixels.height), (2, 1));
        assert_eq!(&pixels.argb[..], &[255, 255, 0, 0, 255, 0, 255, 0]);
    }

    #[test]
    fn the_padding_at_the_end_of_a_row_is_not_part_of_the_picture() {
        // Two rows of one pixel each, in a buffer whose rows are four bytes
        // apart. Reading straight through would take the padding as pixels.
        let data = vec![1, 2, 3, 99, 4, 5, 6, 99];
        let pixels = image_from_hints(&hint(1, 2, 4, false, 3, data))
            .expect("one pixel on each of two rows");
        assert_eq!(&pixels.argb[..], &[255, 1, 2, 3, 255, 4, 5, 6]);
    }

    #[test]
    fn an_alpha_channel_is_kept() {
        let pixels = image_from_hints(&hint(1, 1, 4, true, 4, vec![10, 20, 30, 40]))
            .expect("one translucent pixel");
        assert_eq!(&pixels.argb[..], &[40, 10, 20, 30]);
    }

    #[test]
    fn nonsense_is_refused_rather_than_drawn() {
        // A sample size nothing uses, a channel count that means nothing, and
        // a buffer too short for what it claims: each would be a panic or a
        // smear if believed.
        let mut odd = hint(1, 1, 4, true, 4, vec![1, 2, 3, 4]);
        odd.insert(
            "image-data".to_string(),
            Value::from((
                1_i32,
                1_i32,
                4_i32,
                true,
                16_i32,
                4_i32,
                vec![1_u8, 2, 3, 4],
            )),
        );
        assert!(image_from_hints(&odd).is_none());
        assert!(image_from_hints(&hint(1, 1, 4, true, 2, vec![1, 2, 3, 4])).is_none());
        assert!(image_from_hints(&hint(4, 4, 16, true, 4, vec![1, 2, 3, 4])).is_none());
    }

    #[test]
    fn no_hint_at_all_is_not_an_error() {
        assert!(image_from_hints(&HashMap::new()).is_none());
    }
}
