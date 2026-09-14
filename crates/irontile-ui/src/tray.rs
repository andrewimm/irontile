//! The system tray, as StatusNotifierItem over D-Bus.
//!
//! Three parties: an application publishes a *item* on its own connection, a
//! *watcher* keeps the list of them, and a *host* -- this -- shows them. The
//! watcher is a well-known name, so whoever claims it first owns the list;
//! irontile's bar claims it when nothing else has, which is what makes a tray
//! appear at all on a desktop with no panel from a full environment.
//!
//! On a thread of its own for the same reason the volume is: there is no file
//! to read, only a connection to hold open. What crosses back is a list of
//! items and a pipe saying the list changed.

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::blocking::{Connection, fdo::PropertiesProxy};
use zbus::names::{BusName, InterfaceName};
use zbus::zvariant::{self, ObjectPath, OwnedValue};

/// How often the items are asked what they look like.
///
/// Polled rather than driven by their change signals: a tray holds a handful of
/// items whose properties are a few hundred bytes each, and the signals are the
/// part of this protocol that applications get wrong most often -- an item that
/// forgets to emit one is far more common than a slow tray.
const POLL: Duration = Duration::from_millis(500);

/// The interface an item publishes, and the one the watcher lives on.
const ITEM: &str = "org.kde.StatusNotifierItem";
const MENU: &str = "com.canonical.dbusmenu";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

/// One thing in the tray.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// The bus name and object path that together address it.
    pub service: String,
    pub path: String,
    /// What the application calls itself, used when it has nothing better.
    pub id: String,
    pub title: String,
    pub status: Status,
    pub icon: Icon,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    #[default]
    Active,
    /// The item would rather not be shown. Hidden, as a tray with a panel's
    /// worth of dormant icons in it is no better than no tray.
    Passive,
    NeedsAttention,
}

/// What an item looks like.
///
/// A well-behaved item names a themed icon, which costs nothing and follows the
/// icon theme. A Chromium-based one hands over pixels instead, so both have to
/// work -- 1Password and Slack are the second kind.
#[derive(Clone, Debug, PartialEq)]
pub enum Icon {
    Named(String),
    /// ARGB32, most significant byte first, as the specification says.
    Pixels {
        width: u32,
        height: u32,
        argb: Arc<[u8]>,
    },
    None,
}

/// A menu an item publishes for the host to draw.
///
/// The whole tree is fetched at once. Asking level by level is what the
/// protocol expects, but a tray menu is a handful of rows and the round trip
/// per level costs more than the rows do.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Menu {
    /// Which item it belongs to, so a click can be sent back to it.
    pub service: String,
    pub path: String,
    pub items: Vec<Entry>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: i32,
    pub label: String,
    pub enabled: bool,
    /// A line rather than a row: no label, nothing to click.
    pub separator: bool,
    /// Present when the entry shows a tick, and whether it is ticked.
    pub toggle: Option<bool>,
    pub children: Vec<Entry>,
}

/// Which button was pressed on an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// The item's own idea of "open me", usually its window.
    Activate,
    Secondary,
    /// Asks the item to show its own menu, for one that draws it itself.
    Context,
    /// Fetches the menu the item publishes for the host to draw.
    Menu,
    /// Tells the item an entry of that menu was chosen.
    Choose(i32),
}

/// A live view of the tray.
#[derive(Debug)]
pub struct Tray {
    items: Arc<Mutex<Vec<Item>>>,
    menu: Arc<Mutex<Option<Menu>>>,
    wake: OwnedFd,
    presses: Sender<(Item, Press)>,
}

impl Tray {
    pub fn start() -> Option<Tray> {
        let (read, write) = rustix::pipe::pipe_with(
            // Never blocking: a drain happens when poll says one of these is
            // readable, and with more than one of them the others are not. A
            // blocking read on an empty pipe would stop the bar dead -- no
            // redraws, no pointer, nothing, with the process still running.
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .ok()?;
        let (presses, inbox) = std::sync::mpsc::channel();
        let items = Arc::new(Mutex::new(Vec::new()));
        let menu = Arc::new(Mutex::new(None));
        let shared = Shared {
            items: Arc::clone(&items),
            menu: Arc::clone(&menu),
            wake: Arc::new(write),
        };
        std::thread::Builder::new()
            .name("irontile-bar tray".into())
            .spawn(move || listen(&shared, &inbox))
            .ok()?;
        Some(Tray {
            items,
            menu,
            wake: read,
            presses,
        })
    }

    pub fn items(&self) -> Vec<Item> {
        self.items
            .lock()
            .map(|items| items.clone())
            .unwrap_or_default()
    }

    /// Asks an item to do something. Sent from the thread holding the bus.
    pub fn press(&self, item: &Item, press: Press) {
        let _ = self.presses.send((item.clone(), press));
    }

    /// The menu most recently fetched, taken rather than read: it is a reply to
    /// one request and showing it twice would reopen a menu that was dismissed.
    pub fn take_menu(&self) -> Option<Menu> {
        self.menu.lock().ok()?.take()
    }

    /// Tells an item one of its menu entries was chosen.
    pub fn choose(&self, menu: &Menu, id: i32) {
        let _ = self.presses.send((
            Item {
                service: menu.service.clone(),
                path: menu.path.clone(),
                id: String::new(),
                title: String::new(),
                status: Status::Active,
                icon: Icon::None,
            },
            Press::Choose(id),
        ));
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.wake.as_fd()
    }

    pub fn drain(&self) {
        let mut buffer = [0u8; 64];
        let _ = rustix::io::read(&self.wake, &mut buffer);
    }
}

#[derive(Clone)]
struct Shared {
    items: Arc<Mutex<Vec<Item>>>,
    menu: Arc<Mutex<Option<Menu>>>,
    wake: Arc<OwnedFd>,
}

impl Shared {
    fn publish(&self, next: Vec<Item>) {
        let changed = match self.items.lock() {
            Ok(mut slot) => {
                let changed = *slot != next;
                *slot = next;
                changed
            }
            Err(_) => false,
        };
        if changed {
            let _ = rustix::io::write(&*self.wake, b"!");
        }
    }

    /// Hands over a menu that was asked for, and says so.
    fn offer(&self, menu: Menu) {
        if let Ok(mut slot) = self.menu.lock() {
            *slot = Some(menu);
        }
        let _ = rustix::io::write(&*self.wake, b"!");
    }
}

/// The list of registered items, shared with the served watcher interface.
#[derive(Clone, Default)]
struct Registry(Arc<Mutex<Vec<String>>>);

impl Registry {
    fn names(&self) -> Vec<String> {
        self.0.lock().map(|list| list.clone()).unwrap_or_default()
    }

    fn add(&self, name: String) {
        if let Ok(mut list) = self.0.lock()
            && !list.contains(&name)
        {
            list.push(name);
        }
    }

    fn remove(&self, name: &str) {
        if let Ok(mut list) = self.0.lock() {
            list.retain(|held| held != name);
        }
    }
}

/// The watcher, as served to applications looking for somewhere to register.
struct Watcher {
    registry: Registry,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    /// `service` is either a bus name or an object path; an item may send
    /// either, and the ones that send a path mean "on my own connection".
    fn register_status_notifier_item(
        &mut self,
        service: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) {
        let sender = header.sender().map(|name| name.to_string());
        let full = match (service.starts_with('/'), sender) {
            (true, Some(sender)) => format!("{sender}{service}"),
            (true, None) => return,
            (false, _) => format!("{service}/StatusNotifierItem"),
        };
        self.registry.add(full);
    }

    fn register_status_notifier_host(&mut self, _service: &str) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.registry.names()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

/// Holds the bus open, keeps the list current, and delivers presses.
fn listen(shared: &Shared, inbox: &Receiver<(Item, Press)>) {
    loop {
        if session(shared, inbox).is_none() {
            // No bus, or it went away. Say the tray is empty rather than
            // leaving icons on the bar for items that are no longer there.
            shared.publish(Vec::new());
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn session(shared: &Shared, inbox: &Receiver<(Item, Press)>) -> Option<()> {
    let registry = Registry::default();
    // Claiming the watcher is what makes items register with us. Somebody else
    // may already own it -- a panel from a full desktop environment -- and then
    // this is a plain connection that reads their list instead.
    let connection = zbus::blocking::connection::Builder::session()
        .ok()?
        .name(WATCHER)
        .ok()
        .and_then(|builder| {
            builder
                .serve_at(
                    WATCHER_PATH,
                    Watcher {
                        registry: registry.clone(),
                    },
                )
                .ok()
        })
        .and_then(|builder| builder.build().ok());
    let (connection, ours) = match connection {
        Some(connection) => (connection, true),
        None => (Connection::session().ok()?, false),
    };

    // A host has to announce itself or well-behaved items stay quiet.
    let host = format!("org.kde.StatusNotifierHost-{}", std::process::id());
    let _ = connection.request_name(host.as_str());
    let _ = connection.call_method(
        Some(WATCHER),
        WATCHER_PATH,
        Some(WATCHER),
        "RegisterStatusNotifierHost",
        &host,
    );

    loop {
        while let Some((item, press)) = match inbox.try_recv() {
            Ok(press) => Some(press),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => return Some(()),
        } {
            match press {
                Press::Menu => {
                    if let Some(menu) = read_menu(&connection, &item) {
                        shared.offer(menu);
                    }
                }
                Press::Choose(id) => choose(&connection, &item, id),
                other => send(&connection, &item, other),
            }
        }

        let names = if ours {
            registry.names()
        } else {
            registered_elsewhere(&connection)?
        };
        let mut items = Vec::new();
        for name in &names {
            match read_item(&connection, name) {
                Some(item) => items.push(item),
                // Gone, or not answering: drop it rather than leaving a dead
                // icon in the bar.
                None => registry.remove(name),
            }
        }
        items.retain(|item| item.status != Status::Passive);
        shared.publish(items);
        std::thread::sleep(POLL);
    }
}

/// The list kept by somebody else's watcher.
fn registered_elsewhere(connection: &Connection) -> Option<Vec<String>> {
    let proxy = PropertiesProxy::builder(connection)
        .destination(WATCHER)
        .ok()?
        .path(WATCHER_PATH)
        .ok()?
        .build()
        .ok()?;
    let interface = InterfaceName::try_from(WATCHER).ok()?;
    let value = proxy.get(interface, "RegisteredStatusNotifierItems").ok()?;
    Vec::<String>::try_from(value).ok()
}

/// Splits `":1.23/StatusNotifierItem"` into the name and the path.
fn address(name: &str) -> (&str, &str) {
    match name.find('/') {
        Some(at) => (&name[..at], &name[at..]),
        None => (name, "/StatusNotifierItem"),
    }
}

fn read_item(connection: &Connection, name: &str) -> Option<Item> {
    let (service, path) = address(name);
    let proxy = PropertiesProxy::builder(connection)
        .destination(BusName::try_from(service.to_owned()).ok()?)
        .ok()?
        .path(ObjectPath::try_from(path.to_owned()).ok()?)
        .ok()?
        .build()
        .ok()?;
    // One call for the lot: an item that answers none of them is gone, and
    // asking property by property would say so five times over.
    let all = proxy.get_all(InterfaceName::try_from(ITEM).ok()?).ok()?;

    let text = |key: &str| -> Option<String> {
        all.get(key)
            .and_then(|value| String::try_from(value.try_clone().ok()?).ok())
            .filter(|found| !found.is_empty())
    };
    let status = match text("Status").as_deref() {
        Some("Passive") => Status::Passive,
        Some("NeedsAttention") => Status::NeedsAttention,
        _ => Status::Active,
    };
    let icon = icon_of(&all, status);

    Some(Item {
        service: service.to_owned(),
        path: path.to_owned(),
        id: text("Id").unwrap_or_else(|| service.to_owned()),
        title: text("Title").unwrap_or_default(),
        status,
        icon,
    })
}

/// The icon an item is showing, preferring a themed name over raw pixels.
fn icon_of(all: &HashMap<String, OwnedValue>, status: Status) -> Icon {
    let named = |key: &str| -> Option<String> {
        all.get(key)
            .and_then(|value| String::try_from(value.try_clone().ok()?).ok())
            .filter(|found| !found.is_empty())
    };
    // An item wanting attention says so with a different icon, if it has one.
    let keys: &[&str] = match status {
        Status::NeedsAttention => &["AttentionIconName", "IconName"],
        _ => &["IconName"],
    };
    for key in keys {
        if let Some(name) = named(key) {
            return Icon::Named(name);
        }
    }
    let pixmaps: &[&str] = match status {
        Status::NeedsAttention => &["AttentionIconPixmap", "IconPixmap"],
        _ => &["IconPixmap"],
    };
    for key in pixmaps {
        if let Some(icon) = pixmap_of(all.get(*key)) {
            return icon;
        }
    }
    Icon::None
}

/// Picks the largest pixmap an item offers.
///
/// They come as a list of sizes; the largest scales down to a bar's height far
/// better than a small one scales up.
fn pixmap_of(value: Option<&OwnedValue>) -> Option<Icon> {
    let pixmaps = Vec::<(i32, i32, Vec<u8>)>::try_from(value?.try_clone().ok()?).ok()?;
    let (width, height, argb) = pixmaps
        .into_iter()
        .filter(|(w, h, bytes)| {
            *w > 0 && *h > 0 && bytes.len() as i64 >= i64::from(*w) * i64::from(*h) * 4
        })
        .max_by_key(|(w, h, _)| i64::from(*w) * i64::from(*h))?;
    Some(Icon::Pixels {
        width: width as u32,
        height: height as u32,
        argb: argb.into(),
    })
}

/// Fetches the whole menu an item publishes.
fn read_menu(connection: &Connection, item: &Item) -> Option<Menu> {
    let (service, path) = menu_address(connection, item)?;
    let reply = connection
        .call_method(
            Some(BusName::try_from(service.clone()).ok()?),
            ObjectPath::try_from(path.clone()).ok()?,
            Some(MENU),
            "GetLayout",
            // Every level at once, and every property: a tray menu is small
            // enough that asking for exactly what is wanted costs more in round
            // trips than it saves in bytes.
            &(0i32, -1i32, Vec::<String>::new()),
        )
        .ok()?;
    // The reply carries the root node as a structure rather than a variant; it
    // is only the children inside it that arrive wrapped.
    type Node = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);
    let body = reply.body();
    let (_revision, (id, properties, children)): (u32, Node) = body.deserialize().ok()?;
    let root = entry_from(id, &properties, &children)?;
    Some(Menu {
        service,
        path,
        items: root.children,
    })
}

/// Where an item keeps its menu, if it publishes one at all.
fn menu_address(connection: &Connection, item: &Item) -> Option<(String, String)> {
    let proxy = PropertiesProxy::builder(connection)
        .destination(BusName::try_from(item.service.clone()).ok()?)
        .ok()?
        .path(ObjectPath::try_from(item.path.clone()).ok()?)
        .ok()?
        .build()
        .ok()?;
    let value = proxy
        .get(InterfaceName::try_from(ITEM).ok()?, "Menu")
        .ok()?;
    let path = ObjectPath::try_from(value).ok()?.to_string();
    Some((item.service.clone(), path))
}

/// One node of the layout, and everything under it.
///
/// The children arrive as variants holding the same structure, so this is the
/// same function all the way down.
fn parse_entry(value: &OwnedValue) -> Option<Entry> {
    let value = unwrap_variant(value.try_clone().ok()?.into());
    let fields = zvariant::Structure::try_from(value).ok()?;
    let fields = fields.fields();
    let id = i32::try_from(fields.first()?.try_clone().ok()?).ok()?;
    let properties =
        HashMap::<String, OwnedValue>::try_from(fields.get(1)?.try_clone().ok()?).ok()?;
    let children =
        Vec::<OwnedValue>::try_from(fields.get(2)?.try_clone().ok()?).unwrap_or_default();
    entry_from(id, &properties, &children)
}

/// Builds one entry from the three parts every node has.
fn entry_from(
    id: i32,
    properties: &HashMap<String, OwnedValue>,
    children: &[OwnedValue],
) -> Option<Entry> {
    let text = |key: &str| -> Option<String> {
        properties
            .get(key)
            .and_then(|value| String::try_from(value.try_clone().ok()?).ok())
    };
    let flag = |key: &str, unset: bool| -> bool {
        properties
            .get(key)
            .and_then(|value| bool::try_from(value.try_clone().ok()?).ok())
            .unwrap_or(unset)
    };

    let children: Vec<Entry> = children.iter().filter_map(parse_entry).collect();

    // Absent means shown and usable; only an explicit false is not.
    if !flag("visible", true) {
        return None;
    }
    let toggle = properties.get("toggle-type").and_then(|value| {
        let kind = String::try_from(value.try_clone().ok()?).ok()?;
        if kind.is_empty() {
            return None;
        }
        let state = properties
            .get("toggle-state")
            .and_then(|value| i32::try_from(value.try_clone().ok()?).ok())
            .unwrap_or(-1);
        Some(state == 1)
    });

    Some(Entry {
        id,
        // The underscore marks a keyboard mnemonic, which nothing here uses.
        label: text("label").unwrap_or_default().replace('_', ""),
        enabled: flag("enabled", true),
        separator: text("type").as_deref() == Some("separator"),
        toggle,
        children,
    })
}

/// Unwraps a variant holding a variant, which is how the children arrive.
fn unwrap_variant(value: zvariant::Value<'static>) -> zvariant::Value<'static> {
    match value {
        zvariant::Value::Value(inner) => unwrap_variant(*inner),
        other => other,
    }
}

/// Tells an item that one of its menu entries was chosen.
fn choose(connection: &Connection, item: &Item, id: i32) {
    let Ok(service) = BusName::try_from(item.service.clone()) else {
        return;
    };
    let Ok(path) = ObjectPath::try_from(item.path.clone()) else {
        return;
    };
    // Some items only populate a submenu when told it is about to be shown, and
    // answer this before the click either way.
    let _ = connection.call_method(
        Some(service.clone()),
        path.clone(),
        Some(MENU),
        "AboutToShow",
        &id,
    );
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as u32)
        .unwrap_or(0);
    let _ = connection.call_method(
        Some(service),
        path,
        Some(MENU),
        "Event",
        &(id, "clicked", zvariant::Value::from(0i32), now),
    );
}

fn send(connection: &Connection, item: &Item, press: Press) {
    let method = match press {
        Press::Activate => "Activate",
        Press::Secondary => "SecondaryActivate",
        Press::Context => "ContextMenu",
        // Handled before this is reached.
        Press::Menu | Press::Choose(_) => return,
    };
    let Ok(service) = BusName::try_from(item.service.clone()) else {
        return;
    };
    let Ok(path) = ObjectPath::try_from(item.path.clone()) else {
        return;
    };
    // The coordinates are where the pointer was, which an item may use to place
    // a window near it. Zero is honest here: the bar knows where it drew the
    // icon, but not in the screen coordinates this expects.
    let _ = connection.call_method(Some(service), path, Some(ITEM), method, &(0i32, 0i32));
}
