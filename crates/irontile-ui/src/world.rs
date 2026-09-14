//! Reading the things modules report on.

use std::cell::RefCell;
use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::audio::{Audio, Volume};
use crate::config::ModuleConfig;
use crate::module::{Battery, Link, Network, World};
use crate::tray::{self, Tray};

/// The real outside world.
///
/// Command output is remembered for as long as its module asked for, because a
/// bar redraws whenever anything at all changes -- a window focused, a desktop
/// switched -- and running every module's command each time would fork a
/// process per event.
#[derive(Debug)]
pub struct System {
    ran: RefCell<HashMap<String, (Instant, String)>>,
    /// A live connection to the sound server, if one could be started. Volume
    /// is the one reading with no file behind it.
    audio: Option<Audio>,
    /// The tray, which like the volume is a connection rather than a file.
    tray: Option<Tray>,
}

/// A `System` that reads files but holds no connections, which is what a test
/// wants and never what a running bar does.
impl Default for System {
    fn default() -> Self {
        System {
            ran: RefCell::default(),
            audio: None,
            tray: None,
        }
    }
}

impl System {
    /// Starts the sources that need more than a file read.
    pub fn new() -> System {
        System {
            ran: RefCell::default(),
            audio: Audio::start(),
            tray: Tray::start(),
        }
    }

    /// The menu a tray item asked to have drawn, once it has arrived.
    pub fn take_tray_menu(&self) -> Option<tray::Menu> {
        self.tray.as_ref()?.take_menu()
    }

    /// Tells a tray item which entry of its menu was chosen.
    pub fn choose_tray(&self, menu: &tray::Menu, id: i32) {
        if let Some(tray) = &self.tray {
            tray.choose(menu, id);
        }
    }

    /// Tells a tray item it was clicked.
    pub fn press_tray(&self, service: &str, path: &str, press: tray::Press) {
        let Some(tray) = &self.tray else {
            return;
        };
        if let Some(item) = tray
            .items()
            .iter()
            .find(|item| item.service == service && item.path == path)
        {
            tray.press(item, press);
        }
    }

    /// Readable whenever a reading changed under the bar rather than because
    /// the bar asked. Polled alongside the Wayland and control sockets.
    /// The descriptors that become readable when something changed under the
    /// bar rather than because the bar asked.
    pub fn wakes(&self) -> Vec<std::os::fd::BorrowedFd<'_>> {
        let mut fds = Vec::new();
        if let Some(audio) = &self.audio {
            fds.push(audio.as_fd());
        }
        if let Some(tray) = &self.tray {
            fds.push(tray.as_fd());
        }
        fds
    }

    pub fn drain_wake(&self) {
        if let Some(audio) = &self.audio {
            audio.drain();
        }
        if let Some(tray) = &self.tray {
            tray.drain();
        }
    }

    /// Waits, briefly, for the sources that arrive on their own to have said
    /// something.
    ///
    /// Only worth doing when there will be exactly one frame -- `--dump` --
    /// because a running bar redraws the moment a reading lands and would be
    /// paying this at startup for modules that fill themselves in a few
    /// milliseconds later.
    pub fn settle(&self, timeout: Duration) {
        let deadline = std::time::Instant::now() + timeout;
        // Every source gets a chance, not just the first to answer: a bar with
        // both a volume and a tray on it would otherwise draw whichever spoke
        // first and leave a hole where the other goes.
        while std::time::Instant::now() < deadline {
            let quiet = self
                .audio
                .as_ref()
                .is_none_or(|audio| audio.volume().is_some())
                && self
                    .tray
                    .as_ref()
                    .is_none_or(|tray| !tray.items().is_empty());
            if quiet {
                break;
            }
            let fds = self.wakes();
            if fds.is_empty() {
                return;
            }
            let mut polled: Vec<_> = fds
                .iter()
                .map(|fd| rustix::event::PollFd::new(fd, rustix::event::PollFlags::IN))
                .collect();
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let spec = rustix::time::Timespec {
                tv_sec: left.as_secs() as i64,
                tv_nsec: left.subsec_nanos() as i64,
            };
            if rustix::event::poll(&mut polled, Some(&spec)).unwrap_or(0) == 0 {
                break;
            }
            self.drain_wake();
        }
    }
}

impl World for System {
    fn format(&self, format: &str) -> String {
        chrono::Local::now().format(format).to_string()
    }

    /// Reads the first battery the kernel reports.
    ///
    /// Straight from sysfs rather than through a service: it is two files, it
    /// is stable, and it means the bar has no daemon to depend on.
    fn battery(&self) -> Option<Battery> {
        let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
        for entry in entries.flatten() {
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

    fn volume(&self) -> Option<Volume> {
        self.audio.as_ref()?.volume()
    }

    /// Reads the backlight the kernel exposes.
    ///
    /// The percentage is of the raw range the driver reports, which is what
    /// every other tool that writes to this file uses, so the number here and
    /// the number a brightness key sets are the same number.
    fn backlight(&self) -> Option<f64> {
        for entry in std::fs::read_dir("/sys/class/backlight").ok()?.flatten() {
            let path = entry.path();
            let now = read_number(&path.join("brightness"));
            let max = read_number(&path.join("max_brightness"));
            if let (Some(now), Some(max)) = (now, max)
                && max > 0.0
            {
                return Some(now / max * 100.0);
            }
        }
        None
    }

    /// What the machine is connected by, straight from sysfs.
    ///
    /// A wired link wins when both are up, because that is the one carrying the
    /// traffic. The network this reports is the link the kernel has, not the
    /// route it is using -- a VPN or a captive portal does not show here.
    fn network(&self) -> Network {
        let mut wireless: Option<String> = None;
        let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
            return Network::default();
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // Loopback is always up and never what anyone means.
            if name == "lo" || !matches!(text(&path.join("operstate")).as_deref(), Some("up")) {
                continue;
            }
            // Either name is used depending on the driver's vintage.
            if path.join("wireless").is_dir() || path.join("phy80211").exists() {
                wireless.get_or_insert(name);
            } else {
                return Network {
                    link: Link::Wired,
                    interface: Some(name),
                    signal: None,
                };
            }
        }
        match wireless {
            Some(name) => Network {
                signal: signal_of(&name),
                link: Link::Wireless,
                interface: Some(name),
            },
            None => Network::default(),
        }
    }

    fn tray(&self) -> Vec<tray::Item> {
        self.tray.as_ref().map(Tray::items).unwrap_or_default()
    }

    fn command(&self, config: &ModuleConfig) -> Option<String> {
        let command = config.command.as_ref()?;
        let interval = Duration::from_secs(config.interval.max(1));

        if let Some((run_at, output)) = self.ran.borrow().get(command)
            && run_at.elapsed() < interval
        {
            return Some(output.clone());
        }

        let output = Command::new("sh").arg("-c").arg(command).output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        self.ran
            .borrow_mut()
            .insert(command.clone(), (Instant::now(), text.clone()));
        Some(text)
    }
}

/// Runs a command without waiting for it, for a click action.
pub fn spawn(command: &str) {
    let _ = Command::new("sh").arg("-c").arg(command).spawn();
}

fn text(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
}

fn read_number(path: &std::path::Path) -> Option<f64> {
    text(path)?.parse().ok()
}

/// Link quality for a wireless interface, as a percentage.
///
/// `/proc/net/wireless` reports it out of seventy, which is the range the
/// wireless extensions define; nothing else the kernel offers without a netlink
/// conversation says as much in one read.
fn signal_of(interface: &str) -> Option<f64> {
    let text = std::fs::read_to_string("/proc/net/wireless").ok()?;
    signal_from(&text, interface)
}

fn signal_from(text: &str, interface: &str) -> Option<f64> {
    const FULL: f64 = 70.0;
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        if name.trim() != interface {
            continue;
        }
        // status, then link quality, which may carry a trailing dot.
        let quality: f64 = rest
            .split_whitespace()
            .nth(1)?
            .trim_end_matches('.')
            .parse()
            .ok()?;
        return Some((quality / FULL * 100.0).clamp(0.0, 100.0));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_is_not_run_again_within_its_interval() {
        // A bar redraws on every compositor event, so without this each of
        // those would fork a process for every command module on the bar.
        let system = System::default();
        let config = ModuleConfig {
            command: Some("echo $RANDOM".into()),
            interval: 3600,
            ..Default::default()
        };
        let first = system.command(&config).unwrap();
        let second = system.command(&config).unwrap();
        assert_eq!(
            first, second,
            "the second call must be the remembered output"
        );
    }

    #[test]
    fn link_quality_is_read_as_a_percentage_of_the_range_it_is_reported_in() {
        // Two header lines, a trailing dot on the quality, and the interface
        // name padded -- all of which the real file has and none of which say
        // anything.
        let text = "\
Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE
 face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22
 wlan0: 0000   63.  -47.  -256        0      0      0      0     0        0
";
        assert_eq!(signal_from(text, "wlan0"), Some(90.0));
        assert_eq!(signal_from(text, "wlan1"), None, "a different radio");
        assert_eq!(signal_from("", "wlan0"), None, "no wireless at all");
    }

    #[test]
    fn a_module_without_a_command_reports_nothing() {
        let system = System::default();
        assert_eq!(system.command(&ModuleConfig::default()), None);
    }
}
