//! What the bar shows.
//!
//! Every module turns some piece of state into a short run of text, optionally
//! flagged as warning or critical, and optionally clickable. Keeping that the
//! only shape means a new module is a small addition rather than a new concept,
//! and that the drawing code never has to know what anything means.

use irontile_ipc::{OutputId, WindowInfo, WorkspaceSummary};

use crate::audio::Volume;
use crate::config::{Kind, ModuleConfig};
use crate::theme::Color;

/// How urgent a module's value is, which decides its colour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Level {
    #[default]
    Normal,
    Warning,
    Critical,
}

/// A run of text, or an icon by name.
///
/// A segment is a sequence of these rather than one string, because an icon is
/// a picture placed in the line rather than a character in it. That is the
/// whole difference from an icon font, and it is why the name in a format
/// string can be `battery-level-50-symbolic` instead of an unreadable escape.
#[derive(Clone, Debug, PartialEq)]
pub enum Piece {
    Text(String),
    Icon(String),
    /// A tray item's own artwork, which has no themed name to look up.
    Pixels(Pixels),
}

/// Raw ARGB32 from a tray item, shared rather than copied: they arrive at up to
/// 256 by 256 and a bar rebuilds its segments several times a second.
#[derive(Clone, Debug, PartialEq)]
pub struct Pixels {
    pub width: u32,
    pub height: u32,
    pub argb: std::sync::Arc<[u8]>,
}

/// One clickable run of text.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub pieces: Vec<Piece>,
    pub level: Level,
    /// Drawn picked out, the way an active desktop is.
    ///
    /// Kept apart from `level`, which says how urgent something is: a focused
    /// desktop is not a warning, and colouring it like one would say the wrong
    /// thing about it.
    pub focused: bool,
    /// What each button does on it.
    pub buttons: Buttons,
    /// Shown while the pointer rests on it, if the module said anything.
    pub tooltip: Option<String>,
    /// How the module asked to be painted in the state it is in. Empty means
    /// the bar's own colours decide, which is what `level` is for.
    pub style: Style,
}

/// What each mouse button does on a segment.
///
/// A button can only do one thing, so where two settings want the same one --
/// `on_click` and a `format_alt` that would toggle -- the one written down
/// wins over the one implied.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Buttons {
    pub left: Option<Click>,
    pub middle: Option<Click>,
    pub right: Option<Click>,
}

impl Buttons {
    pub fn is_empty(&self) -> bool {
        self.left.is_none() && self.middle.is_none() && self.right.is_none()
    }

    pub fn get(&self, button: Button) -> Option<&Click> {
        match button {
            Button::Left => self.left.as_ref(),
            Button::Middle => self.middle.as_ref(),
            Button::Right => self.right.as_ref(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

/// What a module may override about how it is painted.
///
/// Both halves are optional and resolved separately: a module can ask for a
/// background without saying anything about the text on it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Style {
    pub color: Option<Color>,
    /// Filled behind the segment. Nothing is drawn behind one by default.
    pub background: Option<Color>,
}

impl Segment {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            pieces: pieces(&text.into()),
            level: Level::Normal,
            focused: false,
            buttons: Buttons::default(),
            tooltip: None,
            style: Style::default(),
        }
    }

    /// The text without its icons, for measuring and for tests.
    pub fn text(&self) -> String {
        self.pieces
            .iter()
            .filter_map(|p| match p {
                Piece::Text(text) => Some(text.as_str()),
                Piece::Icon(_) | Piece::Pixels(_) => None,
            })
            .collect()
    }
}

/// Splits a formatted string into text and icon pieces.
///
/// `{icon}` has already been replaced with `{icon:NAME}` by whatever chose the
/// icon, so the only thing left here is to lift those out of the line.
pub fn pieces(text: &str) -> Vec<Piece> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{icon:") {
        let (before, after) = rest.split_at(start);
        if !before.is_empty() {
            out.push(Piece::Text(before.to_owned()));
        }
        let after = &after["{icon:".len()..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                if !name.is_empty() {
                    out.push(Piece::Icon(name.to_owned()));
                }
                rest = &after[end + 1..];
            }
            // An unclosed placeholder is left as written rather than swallowing
            // the rest of the line.
            None => {
                out.push(Piece::Text(rest.to_owned()));
                return out;
            }
        }
    }
    if !rest.is_empty() {
        out.push(Piece::Text(rest.to_owned()));
    }
    out
}

/// What a click on a segment does.
#[derive(Clone, Debug, PartialEq)]
pub enum Click {
    /// Swap the named module between its two formats.
    Toggle(String),
    /// Press a tray item.
    Tray {
        service: String,
        path: String,
        press: crate::tray::Press,
    },
    /// Show a desktop by number.
    Workspace(u32),
    /// Run a command.
    Run(String),
}

/// Everything the modules read from, gathered once per redraw so that all of
/// them see the same instant.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub workspaces: Vec<WorkspaceSummary>,
    pub windows: Vec<WindowInfo>,
    /// The display this bar is on, so it shows that display's desktops rather
    /// than every desktop there is.
    pub output: Option<OutputId>,
}

impl Snapshot {
    fn focused_window(&self) -> Option<&WindowInfo> {
        self.windows.iter().find(|w| w.focused)
    }
}

/// Renders one module.
pub fn render(config: &ModuleConfig, snapshot: &Snapshot, now: &dyn World) -> Vec<Segment> {
    match config.kind {
        Kind::Workspaces => workspaces(config, snapshot),
        Kind::Window => window(config, snapshot),
        Kind::Clock => {
            let mut segment = Segment::plain(now.format(&config.format));
            // The clock's strings are strftime rather than placeholders, so its
            // tooltip goes through the same clock rather than through `expand`.
            segment.tooltip = config
                .tooltip
                .as_ref()
                .map(|format| now.format(format))
                .filter(|text| !text.trim().is_empty());
            vec![finish(config, segment)]
        }
        Kind::Battery => battery(config, now.battery()),
        Kind::Volume => volume(config, now.volume()),
        Kind::Backlight => backlight(config, now.backlight()),
        Kind::Network => network(config, &now.network()),
        Kind::Tray => tray(config, &now.tray()),
        Kind::Command => command(config, now.command(config)),
    }
}

/// The desktops on this bar's display.
fn workspaces(config: &ModuleConfig, snapshot: &Snapshot) -> Vec<Segment> {
    let mut shown: Vec<&WorkspaceSummary> = snapshot
        .workspaces
        .iter()
        // A desktop with nothing on it and no display is not worth a button.
        .filter(|ws| ws.output.is_some() || !ws.windows.is_empty())
        .collect();
    shown.sort_by_key(|ws| numeric_name(ws).unwrap_or(u32::MAX));

    shown
        .into_iter()
        .map(|ws| {
            let name = ws.name.clone().unwrap_or_else(|| ws.id.0.to_string());
            let text = if config.format == "{}" {
                format!(" {name} ")
            } else {
                config.format.replace("{name}", &name).replace("{}", &name)
            };
            let segment = Segment {
                pieces: pieces(&text),
                level: Level::Normal,
                // Picked out by being on this bar's display, not by holding
                // the keyboard. With two monitors only one desktop is globally
                // focused, so a bar keyed on that would leave every display but
                // one with nothing marked at all.
                focused: match snapshot.output {
                    Some(output) => ws.output == Some(output),
                    None => ws.focused,
                },
                buttons: Buttons {
                    left: numeric_name(ws).map(Click::Workspace),
                    ..Default::default()
                },
                tooltip: tooltip(config, &[("name", &name)], 100.0),
                style: Style::default(),
            };
            finish(config, segment)
        })
        .collect()
}

fn numeric_name(ws: &WorkspaceSummary) -> Option<u32> {
    ws.name.as_ref().and_then(|n| n.parse().ok())
}

fn window(config: &ModuleConfig, snapshot: &Snapshot) -> Vec<Segment> {
    let Some(window) = snapshot.focused_window() else {
        return Vec::new();
    };
    let title = window.title.clone().unwrap_or_default();
    let app_id = window.app_id.clone().unwrap_or_default();
    let values = [("title", title.as_str()), ("app_id", app_id.as_str())];
    let text = expand(&config.format, &values, &config.icons, 100.0);
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut segment = Segment::plain(text);
    segment.tooltip = tooltip(config, &values, 100.0);
    vec![finish(config, segment)]
}

fn battery(config: &ModuleConfig, state: Option<Battery>) -> Vec<Segment> {
    let Some(state) = state else {
        return Vec::new();
    };
    let format = match (state.charging, &config.format_charging) {
        (true, Some(charging)) => charging.as_str(),
        _ => config.format.as_str(),
    };
    let rounded = state.percent.round().to_string();
    let values = [("capacity", rounded.as_str())];
    let text = expand(format, &values, &config.icons, state.percent);

    let level = if state.charging {
        // Charging is not an emergency however low it is.
        Level::Normal
    } else {
        threshold(config, state.percent)
    };
    // On the charger is a state of its own rather than a level, and it outranks
    // the normal colours because it is the more specific thing to say.
    let style = if state.charging {
        Style {
            color: config.color_charging,
            background: config.background_charging,
        }
    } else {
        Style::default()
    };
    vec![finish(
        config,
        Segment {
            pieces: pieces(&text),
            level,
            focused: false,
            buttons: Buttons::default(),
            tooltip: tooltip(config, &values, state.percent),
            style,
        },
    )]
}

fn volume(config: &ModuleConfig, state: Option<Volume>) -> Vec<Segment> {
    let Some(state) = state else {
        return Vec::new();
    };
    let rounded = state.percent.round().to_string();
    // Muted is a state rather than a level: it is not a warning, it is what was
    // asked for, and the icon that says so is a different icon rather than the
    // same one in another colour.
    let format = match (state.muted, &config.format_muted) {
        (true, Some(muted)) => muted.as_str(),
        _ => config.format.as_str(),
    };
    let values = [("volume", rounded.as_str())];
    let text = expand(format, &values, &config.icons, state.percent);
    let style = if state.muted {
        Style {
            color: config.color_muted,
            background: config.background_muted,
        }
    } else {
        Style::default()
    };
    vec![finish(
        config,
        Segment {
            pieces: pieces(&text),
            level: threshold(config, state.percent),
            focused: false,
            buttons: Buttons::default(),
            tooltip: tooltip(config, &values, state.percent),
            style,
        },
    )]
}

fn backlight(config: &ModuleConfig, level: Option<f64>) -> Vec<Segment> {
    let Some(level) = level else {
        return Vec::new();
    };
    let rounded = level.round().to_string();
    let values = [("percent", rounded.as_str())];
    let text = expand(&config.format, &values, &config.icons, level);
    vec![finish(
        config,
        Segment {
            pieces: pieces(&text),
            level: threshold(config, level),
            focused: false,
            buttons: Buttons::default(),
            tooltip: tooltip(config, &values, level),
            style: Style::default(),
        },
    )]
}

fn network(config: &ModuleConfig, state: &Network) -> Vec<Segment> {
    // One format per kind of link, because they have nothing to say in common:
    // a wired link has no signal and a link that is down has no interface worth
    // naming. An absent one falls back to `format`.
    let format = match state.link {
        Link::Wireless => config.format_wifi.as_ref(),
        Link::Wired => config.format_ethernet.as_ref(),
        Link::Down => config.format_disconnected.as_ref(),
    }
    .unwrap_or(&config.format);

    // A link with no signal to report is treated as full rather than empty, so
    // a wired connection does not draw the icon for a dying one.
    let signal = state.signal.unwrap_or(100.0);
    let strength = signal.round().to_string();
    let values = [
        ("ifname", state.interface.as_deref().unwrap_or("")),
        ("signal", strength.as_str()),
    ];
    let text = expand(format, &values, &config.icons, signal);
    if text.trim().is_empty() {
        return Vec::new();
    }
    // Being disconnected is the one state here that is worth colouring, and it
    // is not a threshold on a number, so it is named outright.
    let level = match state.link {
        Link::Down => Level::Critical,
        _ => Level::Normal,
    };
    vec![finish(
        config,
        Segment {
            pieces: pieces(&text),
            level,
            focused: false,
            buttons: Buttons::default(),
            tooltip: tooltip(config, &values, signal),
            style: Style::default(),
        },
    )]
}

/// One segment per tray item.
///
/// Separate segments rather than one, so each is clickable on its own and the
/// pointer resting on one names that item rather than the tray as a whole.
fn tray(config: &ModuleConfig, items: &[crate::tray::Item]) -> Vec<Segment> {
    use crate::tray::{Icon, Press};

    items
        .iter()
        .map(|item| {
            let piece = match &item.icon {
                Icon::Named(name) => Piece::Icon(name.clone()),
                Icon::Pixels {
                    width,
                    height,
                    argb,
                } => Piece::Pixels(Pixels {
                    width: *width,
                    height: *height,
                    argb: argb.clone(),
                }),
                // Nothing to draw but something to click, so it is named.
                Icon::None => Piece::Text(format!(" {} ", item.id)),
            };
            let target = |press| Click::Tray {
                service: item.service.clone(),
                path: item.path.clone(),
                press,
            };
            let named = if item.title.is_empty() {
                item.id.clone()
            } else {
                item.title.clone()
            };
            finish(
                config,
                Segment {
                    pieces: vec![piece],
                    // An item asking for attention is the one thing in a tray
                    // that is worth colouring.
                    level: match item.status {
                        crate::tray::Status::NeedsAttention => Level::Warning,
                        _ => Level::Normal,
                    },
                    focused: false,
                    buttons: Buttons {
                        left: Some(target(Press::Activate)),
                        middle: Some(target(Press::Secondary)),
                        right: Some(target(Press::Context)),
                    },
                    tooltip: config.tooltip.as_ref().map(|_| named),
                    style: Style::default(),
                },
            )
        })
        .collect()
}

fn command(config: &ModuleConfig, output: Option<String>) -> Vec<Segment> {
    let Some(output) = output else {
        return Vec::new();
    };
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let value: Option<f64> = trimmed.trim_end_matches('%').parse().ok();
    let at = value.unwrap_or(100.0);
    let values = [("output", trimmed)];
    let text = expand(&config.format, &values, &config.icons, at);
    let level = value.map(|v| threshold(config, v)).unwrap_or_default();
    vec![finish(
        config,
        Segment {
            pieces: pieces(&text),
            level,
            focused: false,
            buttons: Buttons::default(),
            tooltip: tooltip(config, &values, at),
            style: Style::default(),
        },
    )]
}

/// Which way a threshold is crossed.
///
/// Inferred from the two numbers rather than configured, because the way people
/// naturally write them already says it: a battery is `warning = 30, critical =
/// 15`, where the worse number is lower, and a processor is `warning = 80,
/// critical = 95`, where it is higher. With only one threshold given there is
/// nothing to compare, so the module's kind decides: a battery is bad when it
/// is low and everything else is bad when it is high.
fn lower_is_worse(config: &ModuleConfig) -> bool {
    match (config.warning, config.critical) {
        (Some(warning), Some(critical)) => critical <= warning,
        _ => config.kind == Kind::Battery,
    }
}

fn threshold(config: &ModuleConfig, value: f64) -> Level {
    let worse = |value: f64, limit: f64| {
        if lower_is_worse(config) {
            value <= limit
        } else {
            value >= limit
        }
    };
    match (config.critical, config.warning) {
        (Some(critical), _) if worse(value, critical) => Level::Critical,
        (_, Some(warning)) if worse(value, warning) => Level::Warning,
        _ => Level::Normal,
    }
}

/// What a module asked to be painted with in one state.
///
/// Only the pair matching the state applies. A module painted with `color` for
/// looks still turns the bar's warning colour when it crosses a threshold,
/// because that colour is the message; naming `color_warning` is what makes
/// overriding it deliberate.
fn style_for(config: &ModuleConfig, level: Level) -> Style {
    match level {
        Level::Normal => Style {
            color: config.color,
            background: config.background,
        },
        Level::Warning => Style {
            color: config.color_warning,
            background: config.background_warning,
        },
        Level::Critical => Style {
            color: config.color_critical,
            background: config.background_critical,
        },
    }
}

/// Fills in a template's placeholders.
///
/// One function for every module because a module's tooltip is the same
/// substitution as its text against a different string, and doing it twice by
/// hand is how the two drift apart.
///
/// `values` are `{name}` pairs; the first also answers the bare `{}`. `{icon}`
/// is chosen from `icons` by where `at` falls between 0 and 100.
fn expand(template: &str, values: &[(&str, &str)], icons: &[String], at: f64) -> String {
    let mut out = template.to_owned();
    for (name, value) in values {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    if let Some((_, first)) = values.first() {
        out = out.replace("{}", first);
    }
    out.replace("{icon}", &icon_ref(icons, at))
}

/// The tooltip a module asked for, with the same substitutions as its text.
fn tooltip(config: &ModuleConfig, values: &[(&str, &str)], at: f64) -> Option<String> {
    let template = config.tooltip.as_ref()?;
    let text = expand(template, values, &config.icons, at);
    (!text.trim().is_empty()).then_some(text)
}

/// The chosen icon, written as a placeholder the piece splitter understands.
fn icon_ref(icons: &[String], percent: f64) -> String {
    let name = pick_icon(icons, percent);
    if name.is_empty() {
        String::new()
    } else {
        format!("{{icon:{name}}}")
    }
}

/// Picks an icon by where the value falls between 0 and 100, so five icons
/// cover it in fifths.
fn pick_icon(icons: &[String], percent: f64) -> &str {
    if icons.is_empty() {
        return "";
    }
    let span = 100.0 / icons.len() as f64;
    let index = ((percent / span).floor() as usize).min(icons.len() - 1);
    &icons[index]
}

/// Applies the settings that belong to every module regardless of what it
/// shows: its colour, and what clicking it does.
///
/// A segment that already knows what its click means keeps it, so naming an
/// `on_click` on a module of desktops does not make every desktop button run
/// the same command.
fn finish(config: &ModuleConfig, mut segment: Segment) -> Segment {
    let asked = style_for(config, segment.level);
    segment.style.color = segment.style.color.or(asked.color);
    segment.style.background = segment.style.background.or(asked.background);
    if segment.buttons.left.is_none()
        && let Some(command) = &config.on_click
    {
        segment.buttons.left = Some(Click::Run(command.clone()));
    }
    // Written down wins over implied, but silence does not: a module that gave
    // a button a meaning of its own keeps it, which is what stops a tray item's
    // buttons being cleared by a module that names no commands at all.
    segment.buttons.right = config
        .on_click_right
        .clone()
        .map(Click::Run)
        .or(segment.buttons.right);
    segment.buttons.middle = config
        .on_click_middle
        .clone()
        .map(Click::Run)
        .or(segment.buttons.middle);
    segment
}

/// What a battery reports.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Battery {
    pub percent: f64,
    pub charging: bool,
}

/// What the machine is connected by.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Link {
    Wired,
    Wireless,
    /// Nothing is up. Not the same as having no route: a captive portal is a
    /// link that works as far as the kernel is concerned.
    #[default]
    Down,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Network {
    pub link: Link,
    pub interface: Option<String>,
    /// Signal strength as a percentage, for a wireless link.
    pub signal: Option<f64>,
}

/// The outside world, behind a trait so the modules can be tested without one.
pub trait World {
    fn format(&self, format: &str) -> String;
    fn battery(&self) -> Option<Battery>;
    fn volume(&self) -> Option<Volume>;
    fn backlight(&self) -> Option<f64>;
    fn network(&self) -> Network;
    fn command(&self, config: &ModuleConfig) -> Option<String>;
    /// What is in the tray. Defaulted, because most of what implements this
    /// trait is a test fixture with no interest in one.
    fn tray(&self) -> Vec<crate::tray::Item> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::color;
    use irontile_ipc::{WindowId, WorkspaceId};

    #[derive(Default)]
    struct Fixed {
        battery: Option<Battery>,
        volume: Option<Volume>,
        backlight: Option<f64>,
        network: Network,
        command: Option<String>,
    }

    impl World for Fixed {
        fn format(&self, format: &str) -> String {
            format!("<{format}>")
        }
        fn battery(&self) -> Option<Battery> {
            self.battery
        }
        fn volume(&self) -> Option<Volume> {
            self.volume
        }
        fn backlight(&self) -> Option<f64> {
            self.backlight
        }
        fn network(&self) -> Network {
            self.network.clone()
        }
        fn command(&self, _: &ModuleConfig) -> Option<String> {
            self.command.clone()
        }
    }

    fn fixed() -> Fixed {
        Fixed::default()
    }

    fn ws(id: u64, name: &str, focused: bool, windows: usize) -> WorkspaceSummary {
        WorkspaceSummary {
            id: WorkspaceId(id),
            name: Some(name.into()),
            output: Some(OutputId(1)),
            focused,
            windows: (0..windows as u64).map(WindowId).collect(),
        }
    }

    #[test]
    fn icons_are_chosen_by_where_the_value_falls() {
        let icons: Vec<String> = ["empty", "low", "half", "high", "full"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(pick_icon(&icons, 0.0), "empty");
        assert_eq!(pick_icon(&icons, 45.0), "half");
        assert_eq!(
            pick_icon(&icons, 100.0),
            "full",
            "the top must not fall off the end"
        );
        assert_eq!(pick_icon(&[], 50.0), "");
    }

    #[test]
    fn a_battery_formats_like_waybar() {
        let config = ModuleConfig {
            kind: Kind::Battery,
            format: "{capacity}% {icon}".into(),
            format_charging: Some("{capacity}% up".into()),
            icons: vec!["low".into(), "high".into()],
            warning: Some(30.0),
            critical: Some(15.0),
            ..Default::default()
        };
        let discharging = Fixed {
            battery: Some(Battery {
                percent: 12.0,
                charging: false,
            }),
            ..Default::default()
        };
        let segments = render(&config, &Snapshot::default(), &discharging);
        assert_eq!(segments[0].text(), "12% ");
        assert_eq!(
            segments[0].pieces.last(),
            Some(&Piece::Icon("low".into())),
            "the icon is a name, not a glyph"
        );
        assert_eq!(segments[0].level, Level::Critical);

        let charging = Fixed {
            battery: Some(Battery {
                percent: 12.0,
                charging: true,
            }),
            ..Default::default()
        };
        let segments = render(&config, &Snapshot::default(), &charging);
        assert_eq!(segments[0].text(), "12% up");
        // Low and charging is not an emergency.
        assert_eq!(segments[0].level, Level::Normal);
    }

    #[test]
    fn desktops_are_listed_in_numeric_order_and_clickable() {
        // Desktop 1 holds the keyboard but sits on the other display, 2 is the
        // one on this bar's, and 10 is hidden but has a window on it.
        let mut elsewhere = ws(1, "1", true, 1);
        elsewhere.output = Some(OutputId(2));
        let mut hidden = ws(2, "10", false, 1);
        hidden.output = None;
        let snapshot = Snapshot {
            workspaces: vec![hidden, ws(0, "2", false, 1), elsewhere],
            windows: Vec::new(),
            output: Some(OutputId(1)),
        };
        let config = ModuleConfig {
            kind: Kind::Workspaces,
            ..Default::default()
        };
        let segments = render(&config, &snapshot, &fixed());

        // Numeric, not lexicographic: 10 comes after 2, not before it.
        let names: Vec<String> = segments
            .iter()
            .map(|s| s.text().trim().to_owned())
            .collect();
        assert_eq!(names, vec!["1", "2", "10"]);
        assert!(segments[1].focused, "the one on this display is picked out");
        assert!(
            !segments[0].focused,
            "the globally focused desktop is on another display, so not this bar's"
        );
        assert_eq!(
            segments[1].level,
            Level::Normal,
            "but being focused is not an emergency"
        );
        assert_eq!(segments[2].buttons.left, Some(Click::Workspace(10)));
    }

    #[test]
    fn a_muted_output_says_so_with_a_different_icon_rather_than_a_number() {
        let config = ModuleConfig {
            kind: Kind::Volume,
            format: "{volume}% {icon}".into(),
            format_muted: Some("{icon:audio-volume-muted-symbolic}".into()),
            icons: vec!["low".into(), "medium".into(), "high".into()],
            color_muted: Some(color("#695959")),
            ..Default::default()
        };
        let render = |percent, muted| {
            volume(&config, Some(Volume { percent, muted }))
                .pop()
                .unwrap()
        };

        let loud = render(90.0, false);
        assert_eq!(loud.text(), "90% ");
        assert_eq!(loud.pieces.last(), Some(&Piece::Icon("high".into())));
        assert_eq!(loud.style.color, None);

        let muted = render(90.0, true);
        assert_eq!(
            muted.text(),
            "",
            "the muted format is icon and nothing else"
        );
        assert_eq!(
            muted.pieces.last(),
            Some(&Piece::Icon("audio-volume-muted-symbolic".into()))
        );
        assert_eq!(muted.style.color, Some(color("#695959")));

        // Boost above a hundred is reported rather than clipped, and still
        // lands on the last icon.
        let boosted = render(140.0, false);
        assert_eq!(boosted.text(), "140% ");
        assert_eq!(boosted.pieces.last(), Some(&Piece::Icon("high".into())));
    }

    #[test]
    fn each_kind_of_link_has_its_own_format() {
        let config = ModuleConfig {
            kind: Kind::Network,
            format_wifi: Some("{ifname} {signal}% {icon}".into()),
            format_ethernet: Some("wired {ifname}".into()),
            format_disconnected: Some("offline".into()),
            icons: vec!["weak".into(), "ok".into(), "good".into(), "best".into()],
            ..Default::default()
        };
        let wifi = network(
            &config,
            &Network {
                link: Link::Wireless,
                interface: Some("wlan0".into()),
                signal: Some(90.0),
            },
        );
        assert_eq!(wifi[0].text(), "wlan0 90% ");
        assert_eq!(wifi[0].pieces.last(), Some(&Piece::Icon("best".into())));
        assert_eq!(wifi[0].level, Level::Normal);

        let wired = network(
            &config,
            &Network {
                link: Link::Wired,
                interface: Some("enp0s1".into()),
                signal: None,
            },
        );
        assert_eq!(wired[0].text(), "wired enp0s1");

        let down = network(&config, &Network::default());
        assert_eq!(down[0].text(), "offline");
        assert_eq!(
            down[0].level,
            Level::Critical,
            "being disconnected is worth saying in colour"
        );
    }

    #[test]
    fn a_wired_link_does_not_draw_the_icon_for_a_dying_signal() {
        // It has no signal to report, and zero would pick the weakest icon.
        let config = ModuleConfig {
            kind: Kind::Network,
            format: "{icon}".into(),
            icons: vec!["weak".into(), "strong".into()],
            ..Default::default()
        };
        let wired = network(
            &config,
            &Network {
                link: Link::Wired,
                interface: Some("eth0".into()),
                signal: None,
            },
        );
        assert_eq!(wired[0].pieces.last(), Some(&Piece::Icon("strong".into())));
    }

    #[test]
    fn the_backlight_is_a_percentage_like_any_other() {
        let config = ModuleConfig {
            kind: Kind::Backlight,
            format: "{percent}% {icon}".into(),
            icons: vec!["dim".into(), "bright".into()],
            ..Default::default()
        };
        let segments = backlight(&config, Some(62.5));
        assert_eq!(segments[0].text(), "63% ");
        assert_eq!(
            segments[0].pieces.last(),
            Some(&Piece::Icon("bright".into()))
        );
        assert!(
            backlight(&config, None).is_empty(),
            "a machine with no backlight shows no module"
        );
    }

    #[test]
    fn a_tooltip_is_the_same_substitution_against_a_different_string() {
        // The point of sharing the expander: a module's tooltip cannot drift
        // away from what the module itself says, because it is the same code.
        let config = ModuleConfig {
            kind: Kind::Network,
            format_wifi: Some("{icon}".into()),
            tooltip: Some("{ifname} at {signal}%".into()),
            icons: vec!["weak".into(), "strong".into()],
            ..Default::default()
        };
        let segments = network(
            &config,
            &Network {
                link: Link::Wireless,
                interface: Some("wlan0".into()),
                signal: Some(77.0),
            },
        );
        assert_eq!(segments[0].tooltip.as_deref(), Some("wlan0 at 77%"));
        assert_eq!(
            segments[0].pieces.last(),
            Some(&Piece::Icon("strong".into())),
            "and the module itself still says what it said"
        );
    }

    #[test]
    fn a_module_with_nothing_to_add_has_no_tooltip() {
        // An empty one would be a bordered box with nothing in it, appearing
        // whenever the pointer crossed the module.
        let config = ModuleConfig {
            kind: Kind::Backlight,
            format: "{percent}%".into(),
            tooltip: Some("  ".into()),
            ..Default::default()
        };
        assert_eq!(backlight(&config, Some(50.0))[0].tooltip, None);
    }

    #[test]
    fn the_other_buttons_are_the_modules_to_give_away() {
        let config = ModuleConfig {
            kind: Kind::Clock,
            format: "%H".into(),
            on_click: Some("left".into()),
            on_click_right: Some("right".into()),
            on_click_middle: Some("middle".into()),
            ..Default::default()
        };
        let segment = &render(&config, &Snapshot::default(), &fixed())[0];
        assert_eq!(segment.buttons.left, Some(Click::Run("left".into())));
        assert_eq!(segment.buttons.right, Some(Click::Run("right".into())));
        assert_eq!(segment.buttons.middle, Some(Click::Run("middle".into())));
    }

    #[test]
    fn a_tray_item_is_its_own_clickable_segment() {
        use crate::tray::{Icon, Item, Press, Status};

        let item = |id: &str, icon: Icon, status| Item {
            service: format!(":1.{id}"),
            path: "/StatusNotifierItem".into(),
            id: id.into(),
            title: String::new(),
            status,
            icon,
        };
        let items = vec![
            item(
                "named",
                Icon::Named("mail-unread-symbolic".into()),
                Status::Active,
            ),
            item(
                "pixels",
                Icon::Pixels {
                    width: 2,
                    height: 2,
                    argb: vec![0u8; 16].into(),
                },
                Status::NeedsAttention,
            ),
        ];
        let config = ModuleConfig {
            kind: Kind::Tray,
            ..Default::default()
        };
        let segments = tray(&config, &items);
        assert_eq!(
            segments.len(),
            2,
            "one each, so each is clickable on its own"
        );

        assert_eq!(
            segments[0].pieces,
            vec![Piece::Icon("mail-unread-symbolic".into())],
            "a themed name is looked up like any other icon"
        );
        assert!(matches!(segments[1].pieces[0], Piece::Pixels(_)));
        assert_eq!(
            segments[1].level,
            Level::Warning,
            "an item asking for attention is the one worth colouring"
        );

        // Every button does something, and each a different thing.
        let buttons = &segments[0].buttons;
        assert!(matches!(
            buttons.left,
            Some(Click::Tray {
                press: Press::Activate,
                ..
            })
        ));
        assert!(matches!(
            buttons.middle,
            Some(Click::Tray {
                press: Press::Secondary,
                ..
            })
        ));
        assert!(matches!(
            buttons.right,
            Some(Click::Tray {
                press: Press::Context,
                ..
            })
        ));
    }

    #[test]
    fn a_module_colour_is_carried_but_a_threshold_still_decides() {
        // The colour is the module's own; the level is the bar's judgement
        // about it. Both travel on the segment, and the drawing code prefers
        // the level, so a red battery cannot be painted over in beige.
        let config = ModuleConfig {
            kind: Kind::Battery,
            format: "{capacity}%".into(),
            color: Some(color("#5992bd")),
            warning: Some(30.0),
            critical: Some(15.0),
            ..Default::default()
        };
        let calm = battery(
            &config,
            Some(Battery {
                percent: 80.0,
                charging: false,
            }),
        );
        assert_eq!(calm[0].style.color, Some(color("#5992bd")));
        assert_eq!(calm[0].level, Level::Normal);

        let flat = battery(
            &config,
            Some(Battery {
                percent: 5.0,
                charging: false,
            }),
        );
        assert_eq!(flat[0].level, Level::Critical, "the level still fires");
        assert_eq!(
            flat[0].style.color, None,
            "and with nothing said about that state, the bar's colour decides"
        );
    }

    #[test]
    fn a_state_can_be_painted_on_its_own() {
        // The case this exists for: a battery that is green while it charges
        // and sits on a red field once it is nearly flat, without the text on
        // that field turning red as well.
        let config = ModuleConfig {
            kind: Kind::Battery,
            format: "{capacity}%".into(),
            color: Some(color("#d1c6b4")),
            color_charging: Some(color("#7ab972")),
            color_critical: Some(color("#d1c6b4")),
            background_critical: Some(color("#c65f5f")),
            warning: Some(30.0),
            critical: Some(15.0),
            ..Default::default()
        };
        let paint =
            |percent, charging| battery(&config, Some(Battery { percent, charging }))[0].style;

        assert_eq!(paint(80.0, false).color, Some(color("#d1c6b4")));
        assert_eq!(paint(80.0, false).background, None);

        let charging = paint(5.0, true);
        assert_eq!(
            charging.color,
            Some(color("#7ab972")),
            "on the charger outranks the plain colour, however low it is"
        );

        let flat = paint(5.0, false);
        assert_eq!(flat.background, Some(color("#c65f5f")));
        assert_eq!(
            flat.color,
            Some(color("#d1c6b4")),
            "the field is what says it is critical, not the text on it"
        );
    }

    #[test]
    fn a_click_a_segment_already_has_is_not_overwritten() {
        // Otherwise an `on_click` on a module of desktops would make every
        // desktop button run the same command instead of switching desktop.
        let snapshot = Snapshot {
            workspaces: vec![ws(0, "1", true, 1)],
            windows: Vec::new(),
            output: Some(OutputId(1)),
        };
        let config = ModuleConfig {
            kind: Kind::Workspaces,
            on_click: Some("true".into()),
            ..Default::default()
        };
        let segments = render(&config, &snapshot, &fixed());
        assert_eq!(segments[0].buttons.left, Some(Click::Workspace(1)));
    }

    #[test]
    fn the_window_module_shows_nothing_when_nothing_is_focused() {
        let config = ModuleConfig {
            kind: Kind::Window,
            ..Default::default()
        };
        assert!(render(&config, &Snapshot::default(), &fixed()).is_empty());
    }

    #[test]
    fn threshold_direction_follows_how_the_numbers_are_written() {
        // A battery: the worse number is the lower one.
        let battery = ModuleConfig {
            kind: Kind::Battery,
            warning: Some(30.0),
            critical: Some(15.0),
            ..Default::default()
        };
        assert_eq!(threshold(&battery, 50.0), Level::Normal);
        assert_eq!(threshold(&battery, 20.0), Level::Warning);
        assert_eq!(threshold(&battery, 10.0), Level::Critical);

        // A processor: the worse number is the higher one, and nothing had to
        // be configured to say so.
        let cpu = ModuleConfig {
            kind: Kind::Command,
            warning: Some(80.0),
            critical: Some(95.0),
            ..Default::default()
        };
        assert_eq!(threshold(&cpu, 50.0), Level::Normal);
        assert_eq!(threshold(&cpu, 85.0), Level::Warning);
        assert_eq!(threshold(&cpu, 99.0), Level::Critical);
    }

    #[test]
    fn a_command_module_formats_and_thresholds_its_output() {
        let config = ModuleConfig {
            kind: Kind::Command,
            format: "{}% cpu".into(),
            warning: Some(90.0),
            ..Default::default()
        };
        let world = Fixed {
            battery: None,
            command: Some("  42\n".into()),
            ..Default::default()
        };
        let segments = render(&config, &Snapshot::default(), &world);
        assert_eq!(segments[0].text(), "42% cpu");
        assert_eq!(segments[0].level, Level::Normal);

        // Output that is not a number still shows, it just cannot be compared.
        let world = Fixed {
            battery: None,
            command: Some("hello".into()),
            ..Default::default()
        };
        let segments = render(&config, &Snapshot::default(), &world);
        assert_eq!(segments[0].text(), "hello% cpu");
    }
}
