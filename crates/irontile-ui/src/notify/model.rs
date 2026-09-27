//! What there is to show, with nothing about how it is drawn.
//!
//! The daemon fills this in from what arrives on the bus; the painting reads
//! it and nothing else. Keeping the two apart is what lets every state of the
//! panel be rendered to a PNG without a compositor, a bus, or anything having
//! sent a notification.

/// How loudly something asked to be seen.
///
/// The names are the specification's: 0 low, 1 normal, 2 critical.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Urgency {
    Low,
    #[default]
    Normal,
    Critical,
}

impl Urgency {
    pub fn from_hint(value: u8) -> Urgency {
        match value {
            0 => Urgency::Low,
            2 => Urgency::Critical,
            _ => Urgency::Normal,
        }
    }
}

/// A button a notification offered.
///
/// The key goes back to the sender when it is pressed; the label is what a
/// person reads. The specification pairs them in one flat list, which is a
/// shape worth losing this close to the drawing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action {
    pub key: String,
    pub label: String,
}

/// One notification, as far as anything that draws is concerned.
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    /// The id handed back to the sender, and what a close request names.
    pub id: u32,
    /// The application's own name, as it introduced itself.
    pub app: String,
    /// An icon name to look up in the theme, if it named one.
    pub icon: Option<String>,
    pub summary: String,
    pub body: String,
    pub urgency: Urgency,
    pub actions: Vec<Action>,
    /// When it arrived, for the "4m ago" a panel shows. Held as a duration
    /// rather than an instant so that a test can say what it wants to see.
    pub age: std::time::Duration,
}

impl Default for Notification {
    fn default() -> Self {
        Notification {
            id: 0,
            app: String::new(),
            icon: None,
            summary: String::new(),
            body: String::new(),
            urgency: Urgency::Normal,
            actions: Vec::new(),
            age: std::time::Duration::ZERO,
        }
    }
}

/// How long ago, in the words a panel uses.
///
/// Minutes up to an hour, then hours, then days. Seconds are not interesting
/// -- a notification that arrived nine seconds ago is on screen as a popup
/// anyway -- and "just now" says the same thing without a number that changes
/// while you read it.
pub fn ago(age: std::time::Duration) -> String {
    let secs = age.as_secs();
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

/// A button in the row along the bottom of the panel.
///
/// The icon is a freedesktop name resolved against the icon theme, so these
/// are SVGs from whatever theme is installed rather than glyphs from a font
/// nobody has.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Power {
    pub label: String,
    pub icon: String,
    /// What to run. Nothing here runs it; the daemon does.
    pub run: Vec<String>,
}

impl Power {
    fn new(label: &str, icon: &str, run: &[&str]) -> Power {
        Power {
            label: label.to_string(),
            icon: icon.to_string(),
            run: run.iter().map(|word| (*word).to_string()).collect(),
        }
    }
}

/// The session buttons, in the order they are shown.
///
/// Lock first because it is the one used daily and the only one that is not a
/// decision; power off last because it is the one nobody wants to hit by
/// accident. `systemctl` rather than anything cleverer: logind is what
/// actually performs these, and a desktop that wrapped them in its own verbs
/// would be a desktop with its own words for suspend.
pub fn power_buttons() -> Vec<Power> {
    vec![
        Power::new(
            "Lock",
            "system-lock-screen-symbolic",
            &["irontile-lock", "-f"],
        ),
        Power::new(
            "Sleep",
            "weather-clear-night-symbolic",
            &["systemctl", "suspend"],
        ),
        // Hibernate has no icon of its own in any theme worth naming, and the
        // ones that suggest themselves are already spoken for: a moon is
        // sleep, a power symbol is off. A pause reads as a session stopped
        // where it stands, which is what hibernating is.
        Power::new(
            "Hibernate",
            "media-playback-pause-symbolic",
            &["systemctl", "hibernate"],
        ),
        Power::new(
            "Restart",
            "system-reboot-symbolic",
            &["systemctl", "reboot"],
        ),
        Power::new(
            "Power off",
            "system-shutdown-symbolic",
            &["systemctl", "poweroff"],
        ),
    ]
}

/// Something in the panel a pointer can be over or press.
///
/// Named rather than described by coordinates, so that the drawing and the
/// pointer cannot disagree about what is where: both ask the same function
/// for the same table of rectangles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// The switch that holds notifications back instead of showing them.
    Quiet,
    /// Throws away everything in the list.
    Clear,
    /// One of the session buttons, by its place in the row.
    Power(usize),
    /// A notification, which dismisses it.
    Card(u32),
}

/// Everything the panel shows at once.
#[derive(Clone, Debug, Default)]
pub struct Centre {
    pub notifications: Vec<Notification>,
    /// Set while notifications are being held back rather than shown.
    pub quiet: bool,
    pub buttons: Vec<Power>,
    /// What the pointer is over, if anything.
    pub hovered: Option<Hit>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn how_long_ago_is_said_in_the_largest_unit_that_fits() {
        use std::time::Duration;
        assert_eq!(ago(Duration::from_secs(0)), "just now");
        assert_eq!(ago(Duration::from_secs(59)), "just now");
        assert_eq!(ago(Duration::from_secs(60)), "1m ago");
        assert_eq!(ago(Duration::from_secs(3599)), "59m ago");
        assert_eq!(ago(Duration::from_secs(3600)), "1h ago");
        assert_eq!(ago(Duration::from_secs(86_400)), "1d ago");
    }

    #[test]
    fn urgency_comes_from_a_hint_that_may_say_anything() {
        assert_eq!(Urgency::from_hint(0), Urgency::Low);
        assert_eq!(Urgency::from_hint(1), Urgency::Normal);
        assert_eq!(Urgency::from_hint(2), Urgency::Critical);
        // Not a value the specification defines, and not a reason to refuse a
        // notification: anything unrecognised is an ordinary one.
        assert_eq!(Urgency::from_hint(7), Urgency::Normal);
    }

    #[test]
    fn the_session_buttons_lead_with_the_harmless_one() {
        let buttons = power_buttons();
        assert_eq!(buttons.first().map(|b| b.label.as_str()), Some("Lock"));
        assert_eq!(buttons.last().map(|b| b.label.as_str()), Some("Power off"));
        assert!(buttons.iter().all(|b| !b.run.is_empty()));
    }
}
