//! Telling the session where this compositor is.
//!
//! A program started by the compositor inherits its environment and so finds
//! the right display. A program started by something else does not: D-Bus
//! activates a service with the environment the bus was given, and a desktop
//! entry launched through the systemd user manager gets that manager's. Both
//! were set by whatever session started first, and neither hears about a new
//! one unless it is told.
//!
//! Untold, the effect is a launcher that appears on this display and opens
//! windows on another -- which looks like the launcher misbehaving and is
//! nothing of the sort.

use std::collections::HashMap;

use zbus::blocking::Connection;

/// What is worth saying. Deliberately short.
///
/// `XDG_CURRENT_DESKTOP` is not here on purpose: it selects a desktop portal
/// backend, and naming one that has no backend installed takes away the file
/// picker rather than improving anything.
const WANTED: &[&str] = &["WAYLAND_DISPLAY", "XDG_SESSION_TYPE"];

/// Announces this compositor's display to the session.
///
/// `systemd` also tells the user manager, which is off by default because that
/// manager is shared by every session this user has open: setting it while
/// another session is running points that session's launches here too, and the
/// two are indistinguishable from the outside.
pub fn publish(socket: &str, systemd: bool) {
    let mut values: HashMap<String, String> = HashMap::new();
    values.insert("WAYLAND_DISPLAY".to_owned(), socket.to_owned());
    for name in WANTED.iter().filter(|name| **name != "WAYLAND_DISPLAY") {
        if let Some(value) = std::env::var_os(name).and_then(|v| v.into_string().ok()) {
            values.insert((*name).to_owned(), value);
        }
    }

    let connection = match Connection::session() {
        Ok(connection) => connection,
        // No session bus is a perfectly ordinary way to run a compositor; it
        // only means nothing is going to be activated.
        Err(err) => {
            tracing::debug!(%err, "no session bus to announce the display to");
            return;
        }
    };

    match tell_bus(&connection, &values) {
        Ok(()) => tracing::info!(socket, "announced the display to the session bus"),
        Err(err) => tracing::warn!(%err, "could not announce the display to the session bus"),
    }
    if systemd {
        match tell_systemd(&connection, &values) {
            Ok(()) => tracing::info!("announced the display to the systemd user manager"),
            Err(err) => tracing::warn!(%err, "could not announce the display to systemd"),
        }
    }
}

fn tell_bus(connection: &Connection, values: &HashMap<String, String>) -> Result<(), zbus::Error> {
    connection.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "UpdateActivationEnvironment",
        &values,
    )?;
    Ok(())
}

fn tell_systemd(
    connection: &Connection,
    values: &HashMap<String, String>,
) -> Result<(), zbus::Error> {
    // systemd takes them as `NAME=value`, one string each.
    let assignments: Vec<String> = values
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    connection.call_method(
        Some("org.freedesktop.systemd1"),
        "/org/freedesktop/systemd1",
        Some("org.freedesktop.systemd1.Manager"),
        "SetEnvironment",
        &assignments,
    )?;
    Ok(())
}
