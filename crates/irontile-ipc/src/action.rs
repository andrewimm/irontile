//! The user-facing control vocabulary.
//!
//! An [`Action`] is what a key binding or a `irontilectl` invocation names. It
//! is deliberately coarser than [`irontile_layout::Command`]: it always acts on
//! the focused thing, and it covers compositor-level verbs like spawning and
//! quitting that the layout engine knows nothing about. Precise, addressed
//! operations go over the wire as a `Command` instead.
//!
//! The same text form is used by the config file and by the command line, so
//! there is exactly one spelling of "focus left" to learn.

use std::fmt;

use irontile_layout::{Axis, Direction};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Action {
    Focus(Direction),
    MoveWindow(Direction),
    Resize(Direction),
    FocusOutput(Direction),
    /// Send the focused desktop to the display in this direction.
    SendToOutput(Direction),
    /// `None` flips the focused container's axis.
    Split(Option<Axis>),
    Equalize,
    ToggleFloating,
    ToggleFullscreen,
    Close,
    Workspace(u32),
    MoveToWorkspace(u32),
    /// Program and arguments.
    Spawn(Vec<String>),
    /// Switch to another virtual terminal.
    ///
    /// A compositor holding the VT in graphics mode is the only thing that can
    /// perform this; without a binding for it there is no way off the session.
    SwitchVt(i32),
    /// Re-read the configuration file.
    Reload,
    Quit,
    /// Put the pointer at a point in the space the displays share.
    ///
    /// The compositor draws the pointer itself, so nothing else can move it --
    /// which also means nothing could drive it in a test. A panel receiving a
    /// click, or the pointer resting on one long enough for a tooltip, are both
    /// only reachable this way.
    WarpPointer(i32, i32),
    /// Press and release a button where the pointer is. 1 is left, 2 middle,
    /// 3 right, as in every other numbering of mouse buttons.
    ClickPointer(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub input: String,
    pub reason: Reason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    Empty,
    UnknownVerb(String),
    MissingArgument(&'static str),
    BadDirection(String),
    BadAxis(String),
    BadNumber(String),
    TrailingInput(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot parse action {:?}: ", self.input)?;
        match &self.reason {
            Reason::Empty => write!(f, "it is empty"),
            Reason::UnknownVerb(v) => write!(f, "unknown verb {v:?}"),
            Reason::MissingArgument(what) => write!(f, "expected {what}"),
            Reason::BadDirection(d) => {
                write!(f, "{d:?} is not a direction (left, right, up, down)")
            }
            Reason::BadAxis(a) => {
                write!(f, "{a:?} is not an axis (horizontal, vertical, toggle)")
            }
            Reason::BadNumber(n) => write!(f, "{n:?} is not a number"),
            Reason::TrailingInput(rest) => write!(f, "unexpected trailing input {rest:?}"),
        }
    }
}

impl std::error::Error for ParseError {}

impl std::str::FromStr for Action {
    type Err = ParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        parse(input)
    }
}

/// Parses the text form of an action.
///
/// ```
/// use irontile_ipc::{Action, parse_action};
/// use irontile_layout::Direction;
///
/// assert_eq!(parse_action("focus left").unwrap(), Action::Focus(Direction::Left));
/// assert_eq!(parse_action("workspace 3").unwrap(), Action::Workspace(3));
/// ```
pub fn parse_action(input: &str) -> Result<Action, ParseError> {
    parse(input)
}

fn parse(input: &str) -> Result<Action, ParseError> {
    let fail = |reason| ParseError {
        input: input.to_owned(),
        reason,
    };

    let mut words = input.split_whitespace();
    let verb = words.next().ok_or_else(|| fail(Reason::Empty))?;

    // `spawn` swallows the rest of the line, so it is handled before the
    // trailing-input check that every other verb gets.
    if verb == "spawn" {
        let rest = input.trim_start()[verb.len()..].trim_start();
        let argv = split_argv(rest);
        if argv.is_empty() {
            return Err(fail(Reason::MissingArgument("a program to run")));
        }
        return Ok(Action::Spawn(argv));
    }

    let mut argument = || words.next();

    let action = match verb {
        "focus" => Action::Focus(need_direction(&mut argument, input)?),
        "move" => Action::MoveWindow(need_direction(&mut argument, input)?),
        "resize" => Action::Resize(need_direction(&mut argument, input)?),
        "output" => Action::FocusOutput(need_direction(&mut argument, input)?),
        "send-to-output" => Action::SendToOutput(need_direction(&mut argument, input)?),
        "split" => {
            let word = argument().ok_or_else(|| fail(Reason::MissingArgument("an axis")))?;
            Action::Split(axis(word).ok_or_else(|| fail(Reason::BadAxis(word.to_owned())))?)
        }
        "equalize" => Action::Equalize,
        "float" => Action::ToggleFloating,
        "fullscreen" => Action::ToggleFullscreen,
        "close" => Action::Close,
        "workspace" => Action::Workspace(need_number(&mut argument, input)?),
        "move-to-workspace" => Action::MoveToWorkspace(need_number(&mut argument, input)?),
        "vt" => {
            let word =
                argument().ok_or_else(|| fail(Reason::MissingArgument("a terminal number")))?;
            Action::SwitchVt(
                word.parse()
                    .map_err(|_| fail(Reason::BadNumber(word.to_owned())))?,
            )
        }
        "warp" => {
            let x = need_number_arg(&mut argument, input, "an x coordinate")?;
            let y = need_number_arg(&mut argument, input, "a y coordinate")?;
            Action::WarpPointer(x, y)
        }
        "click" => {
            // Left unless told otherwise, which is what a bare `click` means
            // everywhere else.
            match argument() {
                None => Action::ClickPointer(1),
                Some(word) => Action::ClickPointer(
                    word.parse()
                        .map_err(|_| fail(Reason::BadNumber(word.to_owned())))?,
                ),
            }
        }
        "reload" => Action::Reload,
        "quit" => Action::Quit,
        other => return Err(fail(Reason::UnknownVerb(other.to_owned()))),
    };

    // Catching leftovers turns a typo into an error at load time rather than a
    // binding that silently does the wrong thing.
    let rest: Vec<&str> = words.collect();
    if !rest.is_empty() {
        return Err(fail(Reason::TrailingInput(rest.join(" "))));
    }
    Ok(action)
}

/// Splits a command line into arguments, honouring quotes.
///
/// Whitespace alone is not enough: `spawn sh -c "grim -g $(slurp)"` is how a
/// binding runs anything with a pipeline or a substitution in it, and splitting
/// that into five words hands `sh -c` the word `grim` and throws the rest away.
/// Quotes group; a backslash escapes the next character.
fn split_argv(line: &str) -> Vec<String> {
    let mut argv = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for c in line.chars() {
        if escaped {
            current.push(c);
            escaped = false;
            continue;
        }
        match (c, quote) {
            ('\\', Some('\'')) => current.push(c),
            ('\\', _) => escaped = true,
            ('\'' | '"', None) => {
                quote = Some(c);
                // An empty quoted string is still an argument.
                started = true;
            }
            (c, Some(open)) if c == open => quote = None,
            (c, None) if c.is_whitespace() => {
                if started {
                    argv.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (c, _) => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        argv.push(current);
    }
    argv
}

fn need_number_arg<'a>(
    next: &mut impl FnMut() -> Option<&'a str>,
    input: &str,
    what: &'static str,
) -> Result<i32, ParseError> {
    let word = next().ok_or_else(|| ParseError {
        input: input.to_owned(),
        reason: Reason::MissingArgument(what),
    })?;
    word.parse().map_err(|_| ParseError {
        input: input.to_owned(),
        reason: Reason::BadNumber(word.to_owned()),
    })
}

fn need_direction<'a>(
    next: &mut impl FnMut() -> Option<&'a str>,
    input: &str,
) -> Result<Direction, ParseError> {
    let word = next().ok_or_else(|| ParseError {
        input: input.to_owned(),
        reason: Reason::MissingArgument("a direction"),
    })?;
    direction(word).ok_or_else(|| ParseError {
        input: input.to_owned(),
        reason: Reason::BadDirection(word.to_owned()),
    })
}

fn need_number<'a>(
    next: &mut impl FnMut() -> Option<&'a str>,
    input: &str,
) -> Result<u32, ParseError> {
    let word = next().ok_or_else(|| ParseError {
        input: input.to_owned(),
        reason: Reason::MissingArgument("a desktop number"),
    })?;
    word.parse().map_err(|_| ParseError {
        input: input.to_owned(),
        reason: Reason::BadNumber(word.to_owned()),
    })
}

fn direction(word: &str) -> Option<Direction> {
    match word {
        "left" | "l" | "west" => Some(Direction::Left),
        "right" | "r" | "east" => Some(Direction::Right),
        "up" | "u" | "north" => Some(Direction::Up),
        "down" | "d" | "south" => Some(Direction::Down),
        _ => None,
    }
}

fn axis(word: &str) -> Option<Option<Axis>> {
    match word {
        "horizontal" | "h" => Some(Some(Axis::Horizontal)),
        "vertical" | "v" => Some(Some(Axis::Vertical)),
        "toggle" | "flip" => Some(None),
        _ => None,
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn dir(d: Direction) -> &'static str {
            match d {
                Direction::Left => "left",
                Direction::Right => "right",
                Direction::Up => "up",
                Direction::Down => "down",
            }
        }
        match self {
            Action::Focus(d) => write!(f, "focus {}", dir(*d)),
            Action::MoveWindow(d) => write!(f, "move {}", dir(*d)),
            Action::Resize(d) => write!(f, "resize {}", dir(*d)),
            Action::FocusOutput(d) => write!(f, "output {}", dir(*d)),
            Action::SendToOutput(d) => write!(f, "send-to-output {}", dir(*d)),
            Action::Split(Some(Axis::Horizontal)) => write!(f, "split horizontal"),
            Action::Split(Some(Axis::Vertical)) => write!(f, "split vertical"),
            Action::Split(None) => write!(f, "split toggle"),
            Action::Equalize => write!(f, "equalize"),
            Action::ToggleFloating => write!(f, "float"),
            Action::ToggleFullscreen => write!(f, "fullscreen"),
            Action::Close => write!(f, "close"),
            Action::Workspace(n) => write!(f, "workspace {n}"),
            Action::MoveToWorkspace(n) => write!(f, "move-to-workspace {n}"),
            Action::Spawn(argv) => write!(f, "spawn {}", argv.join(" ")),
            Action::SwitchVt(n) => write!(f, "vt {n}"),
            Action::Reload => write!(f, "reload"),
            Action::Quit => write!(f, "quit"),
            Action::WarpPointer(x, y) => write!(f, "warp {x} {y}"),
            Action::ClickPointer(button) => write!(f, "click {button}"),
        }
    }
}

/// Every verb, for help text and error messages.
pub const VERBS: &[&str] = &[
    "focus <direction>",
    "move <direction>",
    "resize <direction>",
    "output <direction>",
    "send-to-output <direction>",
    "split horizontal|vertical|toggle",
    "equalize",
    "float",
    "fullscreen",
    "close",
    "workspace <n>",
    "move-to-workspace <n>",
    "spawn <program> [args...]",
    "vt <n>",
    "warp <x> <y>",
    "click [1|2|3]",
    "reload",
    "quit",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directions_parse_in_every_spelling() {
        for word in ["left", "l", "west"] {
            assert_eq!(
                parse_action(&format!("focus {word}")),
                Ok(Action::Focus(Direction::Left))
            );
        }
        assert_eq!(
            parse_action("move down"),
            Ok(Action::MoveWindow(Direction::Down))
        );
        assert_eq!(parse_action("resize up"), Ok(Action::Resize(Direction::Up)));
    }

    #[test]
    fn split_takes_an_axis_or_a_flip() {
        assert_eq!(
            parse_action("split vertical"),
            Ok(Action::Split(Some(Axis::Vertical)))
        );
        assert_eq!(
            parse_action("split h"),
            Ok(Action::Split(Some(Axis::Horizontal)))
        );
        assert_eq!(parse_action("split toggle"), Ok(Action::Split(None)));
    }

    #[test]
    fn spawn_takes_the_rest_of_the_line() {
        assert_eq!(
            parse_action("spawn foot -e htop"),
            Ok(Action::Spawn(vec![
                "foot".into(),
                "-e".into(),
                "htop".into()
            ]))
        );
    }

    #[test]
    fn typos_are_rejected_rather_than_ignored() {
        assert!(matches!(
            parse_action("focus lefft").unwrap_err().reason,
            Reason::BadDirection(_)
        ));
        assert!(matches!(
            parse_action("").unwrap_err().reason,
            Reason::Empty
        ));
        assert!(matches!(
            parse_action("wibble").unwrap_err().reason,
            Reason::UnknownVerb(_)
        ));
        assert!(matches!(
            parse_action("focus").unwrap_err().reason,
            Reason::MissingArgument(_)
        ));
        // A stray extra word would otherwise be silently dropped.
        assert!(matches!(
            parse_action("close now").unwrap_err().reason,
            Reason::TrailingInput(_)
        ));
        assert!(matches!(
            parse_action("workspace x").unwrap_err().reason,
            Reason::BadNumber(_)
        ));
    }

    #[test]
    fn a_spawn_keeps_a_quoted_argument_together() {
        // The shape a binding takes whenever it needs a pipeline or a
        // substitution: `sh -c` and one argument. Split on spaces alone, `sh`
        // is handed the word `grim` and the rest is thrown away, which fails
        // silently -- something runs, just not what was asked for.
        let action = parse_action(r#"spawn sh -c "grim -g $(slurp)""#).unwrap();
        assert_eq!(
            action,
            Action::Spawn(vec!["sh".into(), "-c".into(), "grim -g $(slurp)".into(),])
        );
    }

    #[test]
    fn quoting_is_only_grouping() {
        assert_eq!(
            parse_action("spawn foot -e htop").unwrap(),
            Action::Spawn(vec!["foot".into(), "-e".into(), "htop".into()]),
            "an unquoted line is unchanged"
        );
        assert_eq!(
            parse_action(r#"spawn echo 'one two' three"#).unwrap(),
            Action::Spawn(vec!["echo".into(), "one two".into(), "three".into()]),
            "single quotes group too"
        );
        assert_eq!(
            parse_action(r#"spawn echo a\ b"#).unwrap(),
            Action::Spawn(vec!["echo".into(), "a b".into()]),
            "and a backslash escapes the next character"
        );
        assert_eq!(
            parse_action(r#"spawn echo """#).unwrap(),
            Action::Spawn(vec!["echo".into(), String::new()]),
            "an empty argument is still an argument"
        );
        assert!(
            parse_action("spawn").is_err(),
            "and nothing to run is still an error"
        );
    }

    #[test]
    fn every_verb_in_the_help_text_parses() {
        for verb in VERBS {
            // Substitute a concrete argument for each placeholder so the
            // documented spelling is exercised, not just the verb.
            let sample = verb
                .replace("<direction>", "left")
                .replace("horizontal|vertical|toggle", "vertical")
                .replace("<n>", "1")
                .replace("<x> <y>", "100 200")
                .replace("[1|2|3]", "1")
                .replace("<program> [args...]", "true");
            assert!(parse_action(&sample).is_ok(), "{sample:?} does not parse");
        }
    }

    #[test]
    fn display_round_trips_through_the_parser() {
        let actions = [
            Action::Focus(Direction::Right),
            Action::MoveWindow(Direction::Up),
            Action::Split(None),
            Action::Split(Some(Axis::Vertical)),
            Action::Workspace(7),
            Action::MoveToWorkspace(2),
            Action::Spawn(vec!["foot".into(), "-e".into(), "htop".into()]),
            Action::SwitchVt(2),
            Action::SendToOutput(Direction::Left),
            Action::WarpPointer(100, 200),
            Action::ClickPointer(3),
            Action::Close,
            Action::Reload,
            Action::Quit,
        ];
        for action in actions {
            let text = action.to_string();
            assert_eq!(parse_action(&text), Ok(action.clone()), "{text:?}");
        }
    }
}
