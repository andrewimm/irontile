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

use irontile_ipc::{Action, parse_action};
use irontile_layout::{Axis, Params, Size};
use serde::Deserialize;

use crate::keymap::{Keymap, parse_combo};
use crate::theme::Theme;

/// Everything the compositor reads from disk.
#[derive(Clone, Debug)]
pub struct Config {
    pub theme: Theme,
    pub layout: irontile_layout::Config,
    pub keymap: Keymap,
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

        let mut keymap = if file.binds.is_empty() {
            Keymap::defaults()
        } else {
            // Any `[binds]` table replaces the defaults outright rather than
            // merging. Merging would make a binding impossible to remove.
            Keymap::empty()
        };
        for (key, value) in &file.binds {
            let combo = parse_combo(key).map_err(|message| ParseFailure::Binding {
                key: key.clone(),
                message,
            })?;
            let action: Action = parse_action(value).map_err(|e| ParseFailure::Binding {
                key: key.clone(),
                message: e.to_string(),
            })?;
            keymap.bind(combo, action);
        }

        Ok(Config {
            layout: file.layout.into_layout(theme.layout_params()),
            theme,
            keymap,
        })
    }
}

#[derive(Debug)]
pub enum ParseFailure {
    Toml(toml::de::Error),
    Binding { key: String, message: String },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ConfigFile {
    theme: ThemeConfig,
    layout: LayoutConfig,
    /// Key combination to action text. An empty table means the defaults.
    binds: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ThemeConfig {
    border_width: i32,
    border_focused: String,
    border_unfocused: String,
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
            border_focused: to_hex(theme.border_focused),
            border_unfocused: to_hex(theme.border_unfocused),
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
            border_focused: color("border_focused", &self.border_focused)?,
            border_unfocused: color("border_unfocused", &self.border_unfocused)?,
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

fn to_hex(color: [f32; 4]) -> String {
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!(
        "#{:02x}{:02x}{:02x}",
        byte(color[0]),
        byte(color[1]),
        byte(color[2])
    )
}

/// Every default binding, as the text a user would write.
pub fn default_config_text() -> String {
    let mut out = String::from("# irontile configuration\n\n[binds]\n");
    for (combo, action) in Keymap::defaults().binds() {
        out.push_str(&format!(
            "{:?} = {:?}\n",
            combo.to_string(),
            action.to_string()
        ));
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
    fn a_binds_table_replaces_the_defaults() {
        let config = Config::parse(
            r#"
            [binds]
            "Super+z" = "close"
            "#,
        )
        .unwrap();
        // Replacing rather than merging is what makes a default removable.
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
        assert_eq!(config.theme.border_focused, [1.0, 0.0, 0.0, 1.0]);
        assert!(!config.layout.smart_split);
        assert_eq!(config.layout.default_axis, Axis::Vertical);
        // Gaps configured on the theme must reach the layout engine.
        assert_eq!(config.layout.params.inner_gap, 12);
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
