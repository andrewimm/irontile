//! What the kernel says about the battery.
//!
//! Two crates want the same two sysfs reads: the bar draws them in a module,
//! and the lock screen draws them in a corner. Neither should have to take on
//! the other's dependencies to get at them -- the locker in particular is kept
//! to what it needs to draw and authenticate -- so the reads live here, in a
//! crate with nothing under it.

/// What a battery reports.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Battery {
    /// How full it is, from 0 to 100.
    pub percent: f64,
    /// On the charger. Full counts: a machine that has stopped charging
    /// because there is nowhere left to put it is still plugged in, and
    /// saying otherwise would read as "unplugged at 100%".
    pub charging: bool,
}

/// Reads the first battery the kernel reports.
///
/// Straight from sysfs rather than through a service: it is two files, it is
/// stable, and it means nothing that shows a battery depends on a daemon
/// being up. `None` covers both a desktop and a laptop whose kernel has not
/// got round to naming the battery yet, and both mean the same thing to a
/// caller -- draw nothing.
pub fn battery() -> Option<Battery> {
    for entry in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let path = entry.path();
        let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
        if kind.trim() != "Battery" {
            continue;
        }
        let Some(percent) = std::fs::read_to_string(path.join("capacity"))
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
        else {
            continue;
        };
        let status = std::fs::read_to_string(path.join("status")).unwrap_or_default();
        return Some(Battery {
            percent,
            charging: matches!(status.trim(), "Charging" | "Full"),
        });
    }
    None
}
