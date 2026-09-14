//! The bar's configuration.
//!
//! Shaped to follow waybar, because that is what people are porting from: three
//! regions naming modules, and a table per module carrying its format string,
//! icons and thresholds.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::theme::{Color, color};

#[derive(Clone, Debug)]
pub struct Config {
    /// Height in logical pixels.
    pub height: i32,
    /// Which edge to sit on.
    pub position: Position,
    /// Font families in preference order. An icon font listed first supplies
    /// the glyphs it has, and everything else falls through to the next.
    pub font: Vec<String>,
    pub font_size: f32,
    /// An icon theme name. Icons are named by the freedesktop specification, so
    /// the names hold still across releases in a way a font's codepoints do
    /// not.
    pub icon_theme: String,
    /// A directory of `<name>.svg` files searched before the theme, for icons
    /// of your own.
    pub icon_path: Option<std::path::PathBuf>,
    pub background: Color,
    pub foreground: Color,
    /// Drawn along the inner edge, the way waybar's border does.
    pub accent: Color,
    pub accent_width: i32,
    /// Colours for a module that has crossed its thresholds.
    pub warning: Color,
    pub critical: Color,
    /// Behind the focused desktop, and the line under it.
    pub focus_background: Color,
    pub focus_indicator: Color,
    /// Space either side of each module.
    pub padding: i32,
    pub tooltip: Tooltip,
    pub menu: MenuStyle,
    /// Modules by region, naming entries in `modules`.
    pub left: Vec<String>,
    pub center: Vec<String>,
    pub right: Vec<String>,
    pub modules: BTreeMap<String, ModuleConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            height: 30,
            position: Position::Top,
            font: vec!["Noto Sans".into()],
            font_size: 14.0,
            icon_theme: "Adwaita".into(),
            icon_path: None,
            background: color("#252221"),
            foreground: color("#ab9382"),
            accent: color("#cdc0ad"),
            accent_width: 3,
            warning: color("#e0af68"),
            critical: color("#f7768e"),
            focus_background: color("#413c3a"),
            focus_indicator: color("#ffffff"),
            padding: 8,
            tooltip: Tooltip::default(),
            menu: MenuStyle::default(),
            left: vec!["window".into()],
            center: vec!["workspaces".into()],
            right: vec!["battery".into(), "clock".into()],
            modules: default_modules(),
        }
    }
}

/// The file as written, where an absent field and a field set to its default
/// are different things.
///
/// That distinction matters for the regions: defining any module of your own
/// replaces the built-in set, and the built-in regions name built-in modules,
/// so carrying those defaults over would leave a bar naming things that no
/// longer exist.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ConfigFile {
    height: Option<i32>,
    position: Option<Position>,
    font: Option<Vec<String>>,
    font_size: Option<f32>,
    icon_theme: Option<String>,
    icon_path: Option<std::path::PathBuf>,
    background: Option<Color>,
    foreground: Option<Color>,
    accent: Option<Color>,
    accent_width: Option<i32>,
    warning: Option<Color>,
    critical: Option<Color>,
    focus_background: Option<Color>,
    focus_indicator: Option<Color>,
    padding: Option<i32>,
    tooltip: Option<Tooltip>,
    menu: Option<MenuStyle>,
    left: Option<Vec<String>>,
    center: Option<Vec<String>>,
    right: Option<Vec<String>>,
    modules: Option<BTreeMap<String, ModuleConfig>>,
}

impl ConfigFile {
    fn into_config(self) -> Config {
        let defaults = Config::default();
        // Once any module is defined, the regions start empty rather than
        // naming modules the file never mentioned.
        let custom = self.modules.is_some();
        let region = |given: Option<Vec<String>>, default: Vec<String>| match given {
            Some(names) => names,
            None if custom => Vec::new(),
            None => default,
        };
        Config {
            height: self.height.unwrap_or(defaults.height),
            position: self.position.unwrap_or(defaults.position),
            font: self.font.unwrap_or(defaults.font),
            font_size: self.font_size.unwrap_or(defaults.font_size),
            icon_theme: self.icon_theme.unwrap_or(defaults.icon_theme),
            icon_path: self.icon_path.or(defaults.icon_path),
            background: self.background.unwrap_or(defaults.background),
            foreground: self.foreground.unwrap_or(defaults.foreground),
            accent: self.accent.unwrap_or(defaults.accent),
            accent_width: self.accent_width.unwrap_or(defaults.accent_width),
            warning: self.warning.unwrap_or(defaults.warning),
            critical: self.critical.unwrap_or(defaults.critical),
            focus_background: self.focus_background.unwrap_or(defaults.focus_background),
            focus_indicator: self.focus_indicator.unwrap_or(defaults.focus_indicator),
            padding: self.padding.unwrap_or(defaults.padding),
            tooltip: self.tooltip.unwrap_or(defaults.tooltip),
            menu: self.menu.unwrap_or(defaults.menu),
            left: region(self.left, defaults.left),
            center: region(self.center, defaults.center),
            right: region(self.right, defaults.right),
            modules: self.modules.unwrap_or(defaults.modules),
        }
    }
}

/// How a tray item's menu looks.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MenuStyle {
    pub background: Color,
    pub foreground: Color,
    /// An entry that cannot be chosen.
    pub disabled: Color,
    /// Behind the entry the pointer is on.
    pub highlight: Color,
    pub highlight_foreground: Color,
    pub border: Color,
    pub border_width: i32,
    /// Space around the text of each entry.
    pub padding: i32,
    /// Narrower than this and a menu of short words is uncomfortable to aim at.
    pub min_width: i32,
    pub font_size: Option<f32>,
}

impl Default for MenuStyle {
    fn default() -> Self {
        Self {
            background: color("#1b1918"),
            foreground: color("#d1c6b4"),
            disabled: color("#695959"),
            highlight: color("#413c3a"),
            highlight_foreground: color("#ffffff"),
            border: color("#413c3a"),
            border_width: 1,
            padding: 8,
            min_width: 180,
            font_size: None,
        }
    }
}

/// How a tooltip looks, and how long the pointer has to rest before one
/// appears.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Tooltip {
    /// Milliseconds the pointer must hold still on a module. Zero shows one at
    /// once, which is rarely what anybody wants while crossing the bar.
    pub delay_ms: u64,
    pub background: Color,
    pub foreground: Color,
    pub border: Color,
    pub border_width: i32,
    /// Space between the border and the text.
    pub padding: i32,
    /// Falls back to the bar's when absent, which is usually right.
    pub font_size: Option<f32>,
    /// Space between the bar and the tooltip below it.
    pub gap: i32,
}

impl Default for Tooltip {
    fn default() -> Self {
        Self {
            delay_ms: 400,
            background: color("#1b1918"),
            foreground: color("#d1c6b4"),
            border: color("#413c3a"),
            border_width: 1,
            padding: 8,
            font_size: None,
            gap: 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Position {
    #[default]
    Top,
    Bottom,
}

/// One module's settings.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModuleConfig {
    /// What the module does. Anything unrecognised is an error at load time
    /// rather than a module that silently shows nothing.
    #[serde(rename = "type")]
    pub kind: Kind,
    /// Placeholders in braces are filled in by the module: `{capacity}`, and
    /// `{icon}` for the entry chosen from `icons`.
    pub format: String,
    /// Swapped in for `format` when the module is clicked, and back again on
    /// the next click. This is the second thing a module has to say -- an
    /// address rather than an icon, a date rather than a time.
    ///
    /// While it is showing, the formats for particular states give way to it:
    /// asking for the address means the address, connected by wire or not.
    pub format_alt: Option<String>,
    /// Shown while the pointer rests on the module. Takes the same placeholders
    /// as `format`, and may run to several lines.
    pub tooltip: Option<String>,
    /// Used instead of `format` while a battery is charging.
    pub format_charging: Option<String>,
    /// Used instead of `format` while the output is muted.
    pub format_muted: Option<String>,
    /// One per kind of link, because they have nothing to say in common. An
    /// absent one falls back to `format`.
    pub format_wifi: Option<String>,
    pub format_ethernet: Option<String>,
    pub format_disconnected: Option<String>,
    /// Chosen by where the value falls between 0 and 100, so five icons cover
    /// it in fifths.
    pub icons: Vec<String>,
    /// Below this, the module is drawn in the warning colour.
    pub warning: Option<f64>,
    /// Below this, the critical colour.
    pub critical: Option<f64>,
    /// For `command`: what to run, and how often in seconds.
    pub command: Option<String>,
    pub interval: u64,
    /// Run when the module is clicked. Naming one is also what stops the left
    /// button toggling `format_alt`, since a button can only do one thing.
    pub on_click: Option<String>,
    pub on_click_right: Option<String>,
    pub on_click_middle: Option<String>,
    /// Overrides `foreground` for this module, the way a per-widget rule in a
    /// stylesheet does, and fills behind it. Both apply only while the module
    /// has nothing to say: a colour chosen for looks must not be able to hide
    /// one that means something, so a crossed threshold falls back to the
    /// bar's `warning` and `critical` unless the pair below says otherwise.
    pub color: Option<Color>,
    pub background: Option<Color>,
    /// Used instead, in the state each names. Naming one is what makes
    /// repainting a module that does have something to say deliberate.
    pub color_warning: Option<Color>,
    pub background_warning: Option<Color>,
    pub color_critical: Option<Color>,
    pub background_critical: Option<Color>,
    /// A battery on the charger. Not a level, because it is on its way up and
    /// there is nothing to do about it however low it is.
    pub color_charging: Option<Color>,
    pub background_charging: Option<Color>,
    /// A muted output. Not a level either: it is what was asked for.
    pub color_muted: Option<Color>,
    pub background_muted: Option<Color>,
}

impl Default for ModuleConfig {
    fn default() -> Self {
        Self {
            kind: Kind::Command,
            format: "{}".into(),
            format_alt: None,
            tooltip: None,
            format_charging: None,
            format_muted: None,
            format_wifi: None,
            format_ethernet: None,
            format_disconnected: None,
            icons: Vec::new(),
            warning: None,
            critical: None,
            command: None,
            interval: 5,
            on_click: None,
            on_click_right: None,
            on_click_middle: None,
            color: None,
            background: None,
            color_warning: None,
            background_warning: None,
            color_critical: None,
            background_critical: None,
            color_charging: None,
            background_charging: None,
            color_muted: None,
            background_muted: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The desktops on this display, and which is focused.
    Workspaces,
    /// The focused window's title.
    Window,
    Clock,
    Battery,
    /// The default output's volume, live from the sound server.
    Volume,
    /// The display backlight.
    Backlight,
    /// What the machine is connected by.
    Network,
    /// Applications that have put an icon in the tray.
    Tray,
    /// The output of a command, run on an interval.
    #[default]
    Command,
}

fn default_modules() -> BTreeMap<String, ModuleConfig> {
    let mut modules = BTreeMap::new();
    modules.insert(
        "workspaces".into(),
        ModuleConfig {
            kind: Kind::Workspaces,
            ..Default::default()
        },
    );
    modules.insert(
        "window".into(),
        ModuleConfig {
            kind: Kind::Window,
            ..Default::default()
        },
    );
    modules.insert(
        "clock".into(),
        ModuleConfig {
            kind: Kind::Clock,
            format: "%H:%M".into(),
            ..Default::default()
        },
    );
    modules.insert(
        "battery".into(),
        ModuleConfig {
            kind: Kind::Battery,
            format: "{capacity}%".into(),
            warning: Some(30.0),
            critical: Some(15.0),
            ..Default::default()
        },
    );
    modules
}

#[derive(Debug)]
pub enum Error {
    Read {
        path: std::path::PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: std::path::PathBuf,
        source: toml::de::Error,
    },
    /// A region names a module that has no table.
    Unknown { region: &'static str, name: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Read { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Parse { path, source } => write!(f, "{}: {source}", path.display()),
            Error::Unknown { region, name } => {
                write!(
                    f,
                    "{region} names {name:?}, which has no [modules.{name}] table"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

/// Where the bar's configuration lives.
pub fn config_path() -> std::path::PathBuf {
    if let Some(explicit) = std::env::var_os("IRONTILE_BAR_CONFIG") {
        return explicit.into();
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| ".".into());
    base.join("irontile").join("bar.toml")
}

impl Config {
    pub fn load(path: &std::path::Path) -> Result<Config, Error> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(source) => {
                return Err(Error::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let file: ConfigFile = toml::from_str(&text).map_err(|source| Error::Parse {
            path: path.to_owned(),
            source,
        })?;
        let config = file.into_config();
        config.check()?;
        Ok(config)
    }

    pub fn parse(text: &str) -> Result<Config, Error> {
        let file: ConfigFile = toml::from_str(text).map_err(|source| Error::Parse {
            path: "<inline>".into(),
            source,
        })?;
        let config = file.into_config();
        config.check()?;
        Ok(config)
    }

    /// A region naming a module that does not exist is a typo, and one that
    /// would otherwise show as a silently missing part of the bar.
    fn check(&self) -> Result<(), Error> {
        for (region, names) in [
            ("left", &self.left),
            ("center", &self.center),
            ("right", &self.right),
        ] {
            for name in names {
                if !self.modules.contains_key(name) {
                    return Err(Error::Unknown {
                        region,
                        name: name.clone(),
                    });
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_self_consistent() {
        Config::default()
            .check()
            .expect("default regions must name real modules");
    }

    #[test]
    fn a_region_naming_a_missing_module_is_rejected() {
        let err = Config::parse(
            r#"
            right = ["nosuch"]
            [modules.clock]
            type = "clock"
            "#,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                Error::Unknown {
                    region: "right",
                    ..
                }
            ),
            "{err}"
        );
    }

    #[test]
    fn defining_modules_clears_the_default_regions() {
        // Otherwise the built-in regions would name built-in modules that
        // defining your own has just replaced.
        let config = Config::parse(
            r#"
            right = ["clock"]
            [modules.clock]
            type = "clock"
            "#,
        )
        .unwrap();
        assert!(config.left.is_empty());
        assert_eq!(config.right, vec!["clock".to_string()]);
    }

    #[test]
    fn a_waybar_shaped_module_loads() {
        let config = Config::parse(
            r#"
            right = ["battery"]

            [modules.battery]
            type = "battery"
            format = "{capacity}% {icon}"
            format_charging = "{capacity}% up"
            icons = ["a", "b", "c", "d", "e"]
            warning = 30
            critical = 15
            "#,
        )
        .unwrap();
        let battery = &config.modules["battery"];
        assert_eq!(battery.kind, Kind::Battery);
        assert_eq!(battery.icons.len(), 5);
        assert_eq!(battery.critical, Some(15.0));
    }

    #[test]
    fn an_unknown_module_type_is_rejected() {
        // Silently showing nothing would be worse than refusing to start.
        assert!(matches!(
            Config::parse("[modules.x]\ntype = \"weather\"\n"),
            Err(Error::Parse { .. })
        ));
    }
}
