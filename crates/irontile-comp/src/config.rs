//! The configuration file.
//!
//! TOML at `$XDG_CONFIG_HOME/irontile/irontile.toml`. A missing file is not an
//! error; it means the defaults. A malformed one is an error the caller decides
//! what to do with: at startup that means logging it and carrying on with
//! defaults, and on reload it means keeping the configuration already loaded,
//! because a compositor that exits over a typo takes the session with it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use irontile_ipc::parse_action;
use irontile_layout::{Axis, Params, Size};
use serde::Deserialize;

use crate::keymap::{Bind, Keymap, parse_combo};
use crate::theme::{Paint, Theme};

/// Everything the compositor reads from disk.
#[derive(Clone, Debug)]
pub struct Config {
    pub theme: Theme,
    pub layout: irontile_layout::Config,
    pub keymap: Keymap,
    pub cursor: CursorConfig,
    /// Per-display settings, matched by connector name.
    pub outputs: Vec<OutputConfig>,
    /// Commands run once the compositor is up.
    ///
    /// Mostly this is how a bar gets started, but on a first run on real
    /// hardware it is also the difference between a working compositor and one
    /// that merely shows a background colour: with nothing launched, there is
    /// no way to tell those apart.
    pub startup: Vec<Vec<String>>,
}

impl Default for Config {
    fn default() -> Self {
        let theme = Theme::default();
        Self {
            layout: irontile_layout::Config {
                params: theme.layout_params(),
                ..Default::default()
            },
            theme,
            keymap: Keymap::defaults(),
            cursor: CursorConfig::default(),
            outputs: Vec::new(),
            startup: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// A binding that could not be understood. Reported with its key so the
    /// line is findable, rather than being dropped silently.
    Binding { key: String, message: String },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read { path, source } => write!(f, "{}: {source}", path.display()),
            ConfigError::Parse { path, source } => write!(f, "{}: {source}", path.display()),
            ConfigError::Binding { key, message } => write!(f, "binding {key:?}: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

/// Where the configuration lives.
pub fn config_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("IRONTILE_CONFIG") {
        return PathBuf::from(explicit);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("irontile").join("irontile.toml")
}

impl Config {
    pub fn load_from(path: &std::path::Path) -> Result<Config, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config::default());
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        Config::parse(&text).map_err(|e| match e {
            ParseFailure::Toml(source) => ConfigError::Parse {
                path: path.to_owned(),
                source,
            },
            ParseFailure::Binding { key, message } => ConfigError::Binding { key, message },
        })
    }

    /// Parses configuration text. Exposed so the format can be tested without
    /// touching the filesystem.
    pub fn parse(text: &str) -> Result<Config, ParseFailure> {
        let file: ConfigFile = toml::from_str(text).map_err(ParseFailure::Toml)?;
        let theme = file.theme.into_theme()?;

        // Bindings are written on top of the built-in set rather than in place
        // of it, because adding one media key should not mean restating sixty
        // others. Removal stays expressible: a binding set to `false` is
        // removed, and `default_binds = false` starts from nothing at all.
        let mut keymap = if file.default_binds {
            Keymap::defaults()
        } else {
            Keymap::empty()
        };
        for (key, value) in &file.binds {
            let combo = parse_combo(key).map_err(|message| ParseFailure::Binding {
                key: key.clone(),
                message,
            })?;
            match parse_bind(key, value)? {
                Some(bind) => keymap.bind(combo, bind),
                None => keymap.unbind(&combo),
            }
        }

        let startup = file
            .startup
            .exec
            .iter()
            .map(|line| line.split_whitespace().map(str::to_owned).collect())
            .filter(|argv: &Vec<String>| !argv.is_empty())
            .collect();

        let outputs = file
            .outputs
            .into_iter()
            .map(OutputFile::into_output)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Config {
            layout: file.layout.into_layout(theme.layout_params()),
            theme,
            keymap,
            cursor: file.cursor,
            outputs,
            startup,
        })
    }
}

/// Which pointer images to use.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CursorConfig {
    /// An XCursor theme name. Empty means the built-in arrow only.
    pub theme: String,
    /// Nominal size in logical pixels; a display at twice the scale gets twice
    /// the image.
    pub size: i32,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self {
            // What the environment already says, if anything, so the pointer
            // matches the rest of the desktop without being configured twice.
            theme: std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into()),
            size: std::env::var("XCURSOR_SIZE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(24),
        }
    }
}

/// How one display should be set up.
///
/// Matched on the connector name the hardware reports, such as `eDP-1` or
/// `DP-3`. A `*` entry applies to any display without one of its own, which is
/// how a scale can be set for every monitor at once.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputConfig {
    pub name: String,
    /// Top-left corner in the global logical space. Displays without one are
    /// laid end to end to the right of everything that has one.
    pub position: Option<(i32, i32)>,
    /// Preferred mode. Without one, the display's own preferred mode is used.
    pub mode: Option<ModeSpec>,
    pub scale: Option<f64>,
    pub transform: Option<OutputTransform>,
    pub enabled: bool,
}

/// A requested display mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeSpec {
    pub width: i32,
    pub height: i32,
    /// Refresh in millihertz, if one was asked for.
    pub refresh: Option<i32>,
}

impl ModeSpec {
    /// Parses `2256x1504` or `2256x1504@59.99`.
    pub fn parse(text: &str) -> Option<ModeSpec> {
        let (size, refresh) = match text.split_once('@') {
            Some((size, rate)) => {
                let hz: f64 = rate.trim().trim_end_matches("Hz").trim().parse().ok()?;
                if !(hz.is_finite() && hz > 0.0) {
                    return None;
                }
                // Millihertz, which is how DRM reports it.
                (size, Some((hz * 1000.0).round() as i32))
            }
            None => (text, None),
        };
        let (w, h) = size.trim().split_once('x')?;
        let width: i32 = w.trim().parse().ok()?;
        let height: i32 = h.trim().parse().ok()?;
        (width > 0 && height > 0).then_some(ModeSpec {
            width,
            height,
            refresh,
        })
    }
}

/// Snaps a scale to the nearest 120th.
///
/// The fractional-scale protocol carries scales as 120ths, so that is the
/// finest a client can be told about. Rounding here means the compositor lays
/// windows out at exactly the number the client was given, rather than at a
/// slightly different one the protocol had no way to express. It also turns the
/// approximations people actually write into the exact values they meant:
/// `1.3333` becomes four thirds, `1.1666` becomes seven sixths.
pub fn quantize_scale(scale: f64) -> f64 {
    if !scale.is_finite() || scale <= 0.0 {
        return 1.0;
    }
    ((scale * 120.0).round() / 120.0).max(1.0 / 120.0)
}

/// A display's orientation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputTransform {
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl OutputTransform {
    pub fn parse(text: &str) -> Option<OutputTransform> {
        Some(match text.trim() {
            "normal" | "0" => OutputTransform::Normal,
            "90" => OutputTransform::Rotate90,
            "180" => OutputTransform::Rotate180,
            "270" => OutputTransform::Rotate270,
            "flipped" => OutputTransform::Flipped,
            "flipped-90" => OutputTransform::Flipped90,
            "flipped-180" => OutputTransform::Flipped180,
            "flipped-270" => OutputTransform::Flipped270,
            _ => return None,
        })
    }

    /// Whether the orientation swaps width and height.
    pub fn is_sideways(self) -> bool {
        matches!(
            self,
            OutputTransform::Rotate90
                | OutputTransform::Rotate270
                | OutputTransform::Flipped90
                | OutputTransform::Flipped270
        )
    }
}

impl Config {
    /// The settings for a display, preferring an exact name over the wildcard.
    pub fn output(&self, name: &str) -> Option<&OutputConfig> {
        self.outputs
            .iter()
            .find(|o| o.name == name)
            .or_else(|| self.outputs.iter().find(|o| o.name == "*"))
    }
}

#[derive(Debug)]
pub enum ParseFailure {
    Toml(toml::de::Error),
    Binding { key: String, message: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ConfigFile {
    theme: ThemeConfig,
    layout: LayoutConfig,
    /// Whether to start from the built-in bindings. Turning this off is for a
    /// keymap written from scratch; removing one binding is `false` against
    /// that key instead.
    default_binds: bool,
    /// Left as written so the forms can be told apart: a string is the action,
    /// a table of `action` and `repeat` says what holding the key does too, and
    /// `false` removes a binding.
    binds: BTreeMap<String, toml::Value>,
    startup: StartupConfig,
    cursor: CursorConfig,
    #[serde(rename = "output")]
    outputs: Vec<OutputFile>,
}

impl Default for ConfigFile {
    fn default() -> Self {
        Self {
            theme: ThemeConfig::default(),
            layout: LayoutConfig::default(),
            // A file that says nothing about bindings gets the built-in ones.
            default_binds: true,
            binds: BTreeMap::new(),
            startup: StartupConfig::default(),
            cursor: CursorConfig::default(),
            outputs: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct OutputFile {
    name: String,
    position: Option<[i32; 2]>,
    mode: Option<String>,
    scale: Option<f64>,
    transform: Option<String>,
    enabled: bool,
}

impl Default for OutputFile {
    fn default() -> Self {
        Self {
            name: String::new(),
            position: None,
            mode: None,
            scale: None,
            transform: None,
            enabled: true,
        }
    }
}

impl OutputFile {
    fn into_output(self) -> Result<OutputConfig, ParseFailure> {
        let fail = |message: String| ParseFailure::Binding {
            key: format!("output.{}", self.name),
            message,
        };
        if self.name.is_empty() {
            return Err(ParseFailure::Binding {
                key: "output".into(),
                message: "every [[output]] needs a name, such as \"eDP-1\" or \"*\"".into(),
            });
        }
        let mode = match &self.mode {
            Some(text) => Some(
                ModeSpec::parse(text)
                    .ok_or_else(|| fail(format!("{text:?} is not a mode like \"2256x1504@60\"")))?,
            ),
            None => None,
        };
        let transform = match &self.transform {
            Some(text) => Some(
                OutputTransform::parse(text)
                    .ok_or_else(|| fail(format!("{text:?} is not an orientation")))?,
            ),
            None => None,
        };
        if let Some(scale) = self.scale
            && !(scale.is_finite() && scale > 0.0)
        {
            return Err(fail(format!("{scale} is not a usable scale")));
        }
        Ok(OutputConfig {
            name: self.name,
            position: self.position.map(|p| (p[0], p[1])),
            mode,
            scale: self.scale.map(quantize_scale),
            transform,
            enabled: self.enabled,
        })
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct StartupConfig {
    /// Each entry is a command line, split on whitespace.
    exec: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ThemeConfig {
    border_width: i32,
    /// Left as written so both forms can be told apart: a string is one colour,
    /// a table of `colors` and `angle` is a gradient.
    border_focused: toml::Value,
    border_unfocused: toml::Value,
    background: String,
    inner_gap: i32,
    outer_gap: i32,
    min_window_width: i32,
    min_window_height: i32,
    resize_step: i32,
    terminal: Option<String>,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        let theme = Theme::default();
        Self {
            border_width: theme.border_width,
            border_focused: toml::Value::String(to_hex(theme.border_focused.stops()[0])),
            border_unfocused: toml::Value::String(to_hex(theme.border_unfocused.stops()[0])),
            background: to_hex(theme.background),
            inner_gap: theme.inner_gap,
            outer_gap: theme.outer_gap,
            min_window_width: theme.min_window.w,
            min_window_height: theme.min_window.h,
            resize_step: theme.resize_step,
            terminal: theme.terminal,
        }
    }
}

impl ThemeConfig {
    fn into_theme(self) -> Result<Theme, ParseFailure> {
        let color = |name: &str, text: &str| {
            parse_color(text).ok_or_else(|| ParseFailure::Binding {
                key: format!("theme.{name}"),
                message: format!("{text:?} is not a colour like \"#5c99d6\""),
            })
        };
        let default = Theme::default();
        Ok(Theme {
            border_width: self.border_width.max(0),
            border_focused: parse_paint("border_focused", &self.border_focused)?,
            border_unfocused: parse_paint("border_unfocused", &self.border_unfocused)?,
            background: color("background", &self.background)?,
            inner_gap: self.inner_gap.max(0),
            outer_gap: self.outer_gap.max(0),
            min_window: Size::new(self.min_window_width.max(0), self.min_window_height.max(0)),
            resize_step: self.resize_step.max(1),
            // An explicit empty string means "no terminal", which is different
            // from an absent key meaning "find one".
            terminal: self.terminal.filter(|t| !t.is_empty()).or(default.terminal),
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct LayoutConfig {
    smart_split: bool,
    default_axis: AxisName,
    reap_empty_workspaces: bool,
    focus_follows_move: bool,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        let d = irontile_layout::Config::default();
        Self {
            smart_split: d.smart_split,
            default_axis: match d.default_axis {
                Axis::Horizontal => AxisName::Horizontal,
                Axis::Vertical => AxisName::Vertical,
            },
            reap_empty_workspaces: d.reap_empty_workspaces,
            focus_follows_move: d.focus_follows_move,
        }
    }
}

impl LayoutConfig {
    fn into_layout(self, params: Params) -> irontile_layout::Config {
        irontile_layout::Config {
            params,
            default_axis: match self.default_axis {
                AxisName::Horizontal => Axis::Horizontal,
                AxisName::Vertical => Axis::Vertical,
            },
            smart_split: self.smart_split,
            reap_empty_workspaces: self.reap_empty_workspaces,
            focus_follows_move: self.focus_follows_move,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AxisName {
    Horizontal,
    Vertical,
}

/// Parses what a key does: `"focus left"`, or `{ action = "...", repeat = true }`
/// when holding it down should keep firing.
fn parse_bind(key: &str, value: &toml::Value) -> Result<Option<Bind>, ParseFailure> {
    let fail = |message: String| ParseFailure::Binding {
        key: key.to_owned(),
        message,
    };
    let action =
        |text: &str| parse_action(text).map_err(|e: irontile_ipc::ParseError| fail(e.to_string()));

    match value {
        // `false` unbinds, which is how a built-in binding is taken away
        // without having to restate every other one.
        toml::Value::Boolean(false) => Ok(None),
        toml::Value::String(text) => Ok(Some(Bind::new(action(text)?))),
        toml::Value::Table(table) => {
            for name in table.keys() {
                if name != "action" && name != "repeat" {
                    return Err(fail(format!(
                        "unknown setting {name:?}; a binding takes `action` and `repeat`"
                    )));
                }
            }
            let text = table
                .get("action")
                .ok_or_else(|| fail("a binding needs an `action`".into()))?
                .as_str()
                .ok_or_else(|| fail("`action` must be text, like \"focus left\"".into()))?;
            let mut bind = Bind::new(action(text)?);
            if let Some(repeat) = table.get("repeat") {
                bind.repeat = repeat
                    .as_bool()
                    .ok_or_else(|| fail("`repeat` must be true or false".into()))?;
            }
            Ok(Some(bind))
        }
        other => Err(fail(format!(
            "expected an action like \"focus left\", `false` to unbind, or a table of \
             `action` and `repeat`, not {}",
            other.type_str()
        ))),
    }
}

/// Parses what a border is painted with: `"#5c99d6"` for one colour, or
/// `{ colors = [...], angle = 45 }` for a gradient across the window.
///
/// Written out by hand rather than left to a serde untagged enum, because an
/// untagged enum reports a mistyped key as "matched no variant" and the rest of
/// this file names the setting that is wrong.
fn parse_paint(name: &str, value: &toml::Value) -> Result<Paint, ParseFailure> {
    let fail = |message: String| ParseFailure::Binding {
        key: format!("theme.{name}"),
        message,
    };
    let color = |text: &str| {
        parse_color(text).ok_or_else(|| fail(format!("{text:?} is not a colour like \"#5c99d6\"")))
    };

    match value {
        toml::Value::String(text) => Ok(Paint::solid(color(text)?)),
        toml::Value::Table(table) => {
            for key in table.keys() {
                if key != "colors" && key != "angle" {
                    return Err(fail(format!(
                        "unknown setting {key:?}; a gradient takes `colors` and `angle`"
                    )));
                }
            }
            let listed = table
                .get("colors")
                .ok_or_else(|| fail("a gradient needs `colors`".into()))?
                .as_array()
                .ok_or_else(|| fail("`colors` must be a list of colours".into()))?;
            let mut stops = Vec::with_capacity(listed.len());
            for entry in listed {
                let text = entry
                    .as_str()
                    .ok_or_else(|| fail(format!("{entry} is not a colour like \"#5c99d6\"")))?;
                stops.push(color(text)?);
            }
            if stops.is_empty() {
                return Err(fail("`colors` is empty".into()));
            }
            let angle = match table.get("angle") {
                None => 0.0,
                Some(toml::Value::Float(degrees)) => *degrees as f32,
                Some(toml::Value::Integer(degrees)) => *degrees as f32,
                Some(_) => return Err(fail("`angle` must be a number of degrees".into())),
            };
            Ok(Paint::gradient(stops, angle))
        }
        other => Err(fail(format!(
            "expected a colour like \"#5c99d6\" or a table of `colors` and `angle`, not {}",
            other.type_str()
        ))),
    }
}

/// Parses `#rgb`, `#rrggbb` or `#rrggbbaa`.
fn parse_color(text: &str) -> Option<[f32; 4]> {
    let hex = text.strip_prefix('#')?;
    let channel = |i: usize, width: usize| -> Option<f32> {
        let slice = hex.get(i * width..(i + 1) * width)?;
        let value = u8::from_str_radix(slice, 16).ok()?;
        // A single hex digit means the nibble is repeated, so #f0a reads the
        // same as #ff00aa.
        let value = if width == 1 { value * 17 } else { value };
        Some(f32::from(value) / 255.0)
    };
    match hex.len() {
        3 => Some([channel(0, 1)?, channel(1, 1)?, channel(2, 1)?, 1.0]),
        6 => Some([channel(0, 2)?, channel(1, 2)?, channel(2, 2)?, 1.0]),
        8 => Some([
            channel(0, 2)?,
            channel(1, 2)?,
            channel(2, 2)?,
            channel(3, 2)?,
        ]),
        _ => None,
    }
}

/// The form `parse_color` reads back, so alpha is written out when there is
/// any to write.
fn to_hex(color: [f32; 4]) -> String {
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    let (r, g, b, a) = (
        byte(color[0]),
        byte(color[1]),
        byte(color[2]),
        byte(color[3]),
    );
    if a == 255 {
        format!("#{r:02x}{g:02x}{b:02x}")
    } else {
        format!("#{r:02x}{g:02x}{b:02x}{a:02x}")
    }
}

/// Every default binding, as the text a user would write.
pub fn default_config_text() -> String {
    let mut out = String::from("# irontile configuration\n\n[binds]\n");
    for (combo, bind) in Keymap::defaults().binds() {
        let action = bind.action.to_string();
        // Written back in whichever form says the whole truth about it, so the
        // output is a file that reproduces these bindings exactly.
        if bind.repeat {
            out.push_str(&format!(
                "{:?} = {{ action = {action:?}, repeat = true }}\n",
                combo.to_string()
            ));
        } else {
            out.push_str(&format!("{:?} = {action:?}\n", combo.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_defaults() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.theme.border_width, Theme::default().border_width);
        assert!(config.keymap.binds().count() > 0);
    }

    #[test]
    fn colours_parse_in_each_width() {
        assert_eq!(parse_color("#000000"), Some([0.0, 0.0, 0.0, 1.0]));
        assert_eq!(parse_color("#ffffff"), Some([1.0, 1.0, 1.0, 1.0]));
        assert_eq!(parse_color("#fff"), parse_color("#ffffff"));
        assert_eq!(parse_color("#00000080").unwrap()[3], 128.0 / 255.0);
        assert_eq!(parse_color("5c99d6"), None);
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#gggggg"), None);
    }

    #[test]
    fn a_border_is_either_one_colour_or_a_gradient() {
        let config = Config::parse(
            r##"
            [theme]
            border_focused = { colors = ["#ddbba8ee", "#c67f5fee"], angle = 45 }
            border_unfocused = "#695959aa"
            "##,
        )
        .unwrap();
        let focused = &config.theme.border_focused;
        assert_eq!(focused.stops().len(), 2);
        assert_eq!(focused.angle(), 45.0);
        assert!(!focused.is_solid());
        assert!(config.theme.border_unfocused.is_solid());
        assert_eq!(
            config.theme.border_unfocused.stops()[0][3],
            170.0 / 255.0,
            "the alpha survives"
        );
    }

    #[test]
    fn a_gradient_that_is_written_wrong_names_the_setting() {
        // The whole file works this way: an unusable line says which one it is
        // rather than leaving a border quietly unpainted.
        for (text, expected) in [
            ("border_focused = { colours = [\"#fff\"] }", "colours"),
            ("border_focused = { colors = [\"nope\"] }", "nope"),
            ("border_focused = { angle = 45 }", "colors"),
            (
                "border_focused = { colors = [\"#fff\"], angle = \"up\" }",
                "angle",
            ),
            ("border_focused = 45", "colour"),
        ] {
            let err = Config::parse(&format!("[theme]\n{text}\n")).unwrap_err();
            let ParseFailure::Binding { key, message } = err else {
                panic!("{text} should name the setting, not fail to parse at all");
            };
            assert_eq!(key, "theme.border_focused");
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn a_binding_may_say_what_holding_it_does() {
        let config = Config::parse(
            r#"
            [binds]
            "XF86AudioRaiseVolume" = { action = "spawn wpctl set-volume @DEFAULT_AUDIO_SINK@ 5%+", repeat = true }
            "XF86AudioMute" = "spawn wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle"
            "Super+Ctrl+h" = { action = "resize left", repeat = false }
            "#,
        )
        .unwrap();
        let bind = |text: &str| {
            let combo = parse_combo(text).unwrap();
            config
                .keymap
                .binds()
                .find(|(c, _)| **c == combo)
                .map(|(_, b)| b.clone())
                .unwrap_or_else(|| panic!("no binding for {text}"))
        };
        assert!(bind("XF86AudioRaiseVolume").repeat);
        assert!(!bind("XF86AudioMute").repeat, "a toggle fires once");
        assert!(
            !bind("Super+Ctrl+h").repeat,
            "a ramp that says it does not repeat does not repeat"
        );
    }

    #[test]
    fn a_binding_that_is_written_wrong_names_the_key() {
        for (text, expected) in [
            (r#""Super+q" = { act = "close" }"#, "act"),
            (r#""Super+q" = { repeat = true }"#, "action"),
            (
                r#""Super+q" = { action = "close", repeat = "yes" }"#,
                "repeat",
            ),
            (r#""Super+q" = 7"#, "action"),
            (r#""Super+q" = "nonsense""#, "nonsense"),
        ] {
            let err = Config::parse(&format!("[binds]\n{text}\n")).unwrap_err();
            let ParseFailure::Binding { key, message } = err else {
                panic!("{text} should name the key, not fail to parse at all");
            };
            assert_eq!(key, "Super+q");
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn bindings_are_written_on_top_of_the_defaults() {
        let defaults = Keymap::defaults().binds().count();
        let config = Config::parse(
            r#"
            [binds]
            "Super+z" = "close"
            "#,
        )
        .unwrap();
        assert_eq!(config.keymap.binds().count(), defaults + 1);
    }

    #[test]
    fn a_binding_can_be_taken_away_without_restating_the_rest() {
        // Otherwise removing one default would mean copying out every other,
        // and they would drift apart the first time a default changed.
        let defaults = Keymap::defaults().binds().count();
        let config = Config::parse(
            r#"
            [binds]
            "Super+f" = false
            "#,
        )
        .unwrap();
        assert_eq!(config.keymap.binds().count(), defaults - 1);
        let combo = parse_combo("Super+f").unwrap();
        assert!(config.keymap.binds().all(|(c, _)| *c != combo));
    }

    #[test]
    fn a_keymap_can_be_started_from_nothing() {
        let config = Config::parse(
            r#"
            default_binds = false
            [binds]
            "Super+z" = "close"
            "#,
        )
        .unwrap();
        assert_eq!(config.keymap.binds().count(), 1);
    }

    #[test]
    fn a_bad_binding_names_the_key_it_came_from() {
        let err = Config::parse(
            r#"
            [binds]
            "Super+q" = "focus sideways"
            "#,
        )
        .unwrap_err();
        match err {
            ParseFailure::Binding { key, message } => {
                assert_eq!(key, "Super+q");
                assert!(message.contains("sideways"), "{message}");
            }
            other => panic!("expected a binding failure, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_key_is_rejected_rather_than_ignored() {
        // A typo in a setting name would otherwise silently do nothing.
        assert!(matches!(
            Config::parse("[theme]\nborder_with = 3\n"),
            Err(ParseFailure::Toml(_))
        ));
    }

    #[test]
    fn theme_and_layout_settings_take_effect() {
        let config = Config::parse(
            r##"
            [theme]
            border_width = 5
            inner_gap = 12
            border_focused = "#ff0000"

            [layout]
            smart_split = false
            default_axis = "vertical"
            "##,
        )
        .unwrap();
        assert_eq!(config.theme.border_width, 5);
        assert_eq!(config.theme.inner_gap, 12);
        assert_eq!(
            config.theme.border_focused,
            Paint::solid([1.0, 0.0, 0.0, 1.0])
        );
        assert!(!config.layout.smart_split);
        assert_eq!(config.layout.default_axis, Axis::Vertical);
        // Gaps configured on the theme must reach the layout engine.
        assert_eq!(config.layout.params.inner_gap, 12);
    }

    #[test]
    fn scales_are_snapped_to_what_the_protocol_can_express() {
        // Four thirds is what someone means by 1.3333, and 160/120 is exactly
        // that, so the compositor and the client end up using the same number.
        assert_eq!(quantize_scale(1.3333), 160.0 / 120.0);
        assert_eq!(quantize_scale(1.333_333_333), 160.0 / 120.0);
        // The common scales are already exact.
        for exact in [1.0, 1.25, 1.5, 1.75, 2.0, 3.0] {
            assert_eq!(quantize_scale(exact), exact, "{exact} should not move");
        }
        // Nonsense cannot produce a scale that would divide by zero.
        assert_eq!(quantize_scale(0.0), 1.0);
        assert_eq!(quantize_scale(f64::NAN), 1.0);
    }

    #[test]
    fn a_configured_scale_is_quantized() {
        let config = Config::parse(
            r#"
            [[output]]
            name = "eDP-1"
            scale = 1.3333
            "#,
        )
        .unwrap();
        assert_eq!(config.outputs[0].scale, Some(160.0 / 120.0));
    }

    #[test]
    fn startup_commands_are_split_into_arguments() {
        let config = Config::parse(
            r#"
            [startup]
            exec = ["alacritty -e htop", "waybar"]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.startup,
            vec![
                vec!["alacritty".to_owned(), "-e".to_owned(), "htop".to_owned()],
                vec!["waybar".to_owned()],
            ]
        );
    }

    #[test]
    fn the_documented_defaults_round_trip() {
        // The generated sample is what the manual shows, so it had better load.
        let text = default_config_text();
        let config = Config::parse(&text).expect("generated defaults must parse");
        assert_eq!(
            config.keymap.binds().count(),
            Keymap::defaults().binds().count()
        );
    }
}
