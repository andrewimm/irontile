//! Key bindings.
//!
//! A [`Keymap`] maps a modifier set plus a keysym to an
//! [`Action`](irontile_ipc::Action) — the same vocabulary the control socket
//! and the config file use, so a binding and an `irontilectl` invocation do
//! exactly the same thing.

use std::fmt;

use irontile_ipc::{Action, parse_action};
use smithay::input::keyboard::{Keysym, ModifiersState, xkb};

/// A modifier set and a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyCombo {
    pub logo: bool,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub keysym: Keysym,
}

impl KeyCombo {
    pub fn matches(&self, mods: &ModifiersState, keysym: Keysym) -> bool {
        self.keysym == keysym
            && self.logo == mods.logo
            && self.shift == mods.shift
            && self.ctrl == mods.ctrl
            && self.alt == mods.alt
    }
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (held, name) in [
            (self.logo, "Super"),
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
        ] {
            if held {
                write!(f, "{name}+")?;
            }
        }
        write!(f, "{}", xkb::keysym_get_name(self.keysym))
    }
}

/// Parses `Super+Shift+h`.
///
/// The final segment is the key; everything before it is a modifier. Key names
/// are xkb keysym names, so anything `xkbcli` prints is usable, and the lookup
/// is case insensitive so `h` and `H` both name the same key — a shifted
/// binding is written with an explicit `Shift`.
pub fn parse_combo(text: &str) -> Result<KeyCombo, String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("a binding cannot be empty".into());
    }
    let mut parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
    let key = parts.pop().expect("split always yields one part");
    if key.is_empty() {
        return Err(format!("{text:?} has no key after the last `+`"));
    }

    let mut combo = KeyCombo {
        logo: false,
        shift: false,
        ctrl: false,
        alt: false,
        keysym: Keysym::from(0),
    };
    for part in parts {
        match part.to_ascii_lowercase().as_str() {
            "super" | "mod" | "logo" | "win" | "mod4" => combo.logo = true,
            "shift" => combo.shift = true,
            "ctrl" | "control" => combo.ctrl = true,
            "alt" | "mod1" | "meta" => combo.alt = true,
            other => return Err(format!("{other:?} is not a modifier")),
        }
    }

    let keysym = xkb::keysym_from_name(key, xkb::KEYSYM_CASE_INSENSITIVE);
    if keysym.raw() == xkb::keysyms::KEY_NoSymbol {
        return Err(format!("{key:?} is not a key name"));
    }
    combo.keysym = keysym;
    Ok(combo)
}

/// The bindings in force.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    binds: Vec<(KeyCombo, Action)>,
}

impl Keymap {
    pub fn empty() -> Self {
        Self::default()
    }

    /// The built-in bindings.
    pub fn defaults() -> Self {
        let mut keymap = Keymap::empty();
        for (combo, action) in DEFAULT_BINDS {
            // Parsed through exactly the same path a config file takes, so the
            // defaults cannot drift away from what a user is able to write. A
            // test below keeps this from ever panicking in a release.
            let combo = parse_combo(combo).expect("a default binding must parse");
            let action = parse_action(action).expect("a default action must parse");
            keymap.bind(combo, action);
        }
        keymap
    }

    /// Adds a binding, replacing any existing one for the same combination.
    pub fn bind(&mut self, combo: KeyCombo, action: Action) {
        match self.binds.iter_mut().find(|(c, _)| *c == combo) {
            Some(slot) => slot.1 = action,
            None => self.binds.push((combo, action)),
        }
    }

    pub fn action_for(&self, mods: &ModifiersState, keysym: Keysym) -> Option<&Action> {
        self.binds
            .iter()
            .find(|(combo, _)| combo.matches(mods, keysym))
            .map(|(_, action)| action)
    }

    pub fn binds(&self) -> impl Iterator<Item = (&KeyCombo, &Action)> {
        self.binds.iter().map(|(c, a)| (c, a))
    }
}

/// The default bindings, written the way a user would write them.
pub const DEFAULT_BINDS: &[(&str, &str)] = &[
    ("Super+h", "focus left"),
    ("Super+j", "focus down"),
    ("Super+k", "focus up"),
    ("Super+l", "focus right"),
    ("Super+Left", "focus left"),
    ("Super+Down", "focus down"),
    ("Super+Up", "focus up"),
    ("Super+Right", "focus right"),
    ("Super+Shift+h", "move left"),
    ("Super+Shift+j", "move down"),
    ("Super+Shift+k", "move up"),
    ("Super+Shift+l", "move right"),
    ("Super+Shift+Left", "move left"),
    ("Super+Shift+Down", "move down"),
    ("Super+Shift+Up", "move up"),
    ("Super+Shift+Right", "move right"),
    ("Super+Ctrl+h", "resize left"),
    ("Super+Ctrl+j", "resize down"),
    ("Super+Ctrl+k", "resize up"),
    ("Super+Ctrl+l", "resize right"),
    ("Super+Alt+h", "output left"),
    ("Super+Alt+l", "output right"),
    ("Super+Shift+Alt+h", "send-to-output left"),
    ("Super+Shift+Alt+l", "send-to-output right"),
    ("Super+1", "workspace 1"),
    ("Super+2", "workspace 2"),
    ("Super+3", "workspace 3"),
    ("Super+4", "workspace 4"),
    ("Super+5", "workspace 5"),
    ("Super+6", "workspace 6"),
    ("Super+7", "workspace 7"),
    ("Super+8", "workspace 8"),
    ("Super+9", "workspace 9"),
    ("Super+0", "workspace 10"),
    ("Super+Shift+1", "move-to-workspace 1"),
    ("Super+Shift+2", "move-to-workspace 2"),
    ("Super+Shift+3", "move-to-workspace 3"),
    ("Super+Shift+4", "move-to-workspace 4"),
    ("Super+Shift+5", "move-to-workspace 5"),
    ("Super+Shift+6", "move-to-workspace 6"),
    ("Super+Shift+7", "move-to-workspace 7"),
    ("Super+Shift+8", "move-to-workspace 8"),
    ("Super+Shift+9", "move-to-workspace 9"),
    ("Super+Shift+0", "move-to-workspace 10"),
    ("Super+Return", "terminal"),
    ("Super+q", "close"),
    ("Super+f", "fullscreen"),
    ("Super+Shift+space", "float"),
    ("Super+v", "split vertical"),
    ("Super+b", "split horizontal"),
    ("Super+t", "split toggle"),
    ("Super+o", "equalize"),
    ("Super+Shift+c", "reload"),
    ("Super+Shift+e", "quit"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use irontile_layout::Direction;

    fn mods(logo: bool, shift: bool, ctrl: bool, alt: bool) -> ModifiersState {
        ModifiersState {
            logo,
            shift,
            ctrl,
            alt,
            ..Default::default()
        }
    }

    #[test]
    fn every_default_binding_parses() {
        // `Keymap::defaults` unwraps; this is what keeps that honest.
        let keymap = Keymap::defaults();
        assert_eq!(keymap.binds().count(), DEFAULT_BINDS.len());
    }

    #[test]
    fn combos_parse_and_print_back() {
        let combo = parse_combo("Super+Shift+h").unwrap();
        assert!(combo.logo && combo.shift && !combo.ctrl && !combo.alt);
        assert_eq!(combo.to_string(), "Super+Shift+h");
        // Modifier spellings are interchangeable and case insensitive.
        assert_eq!(parse_combo("mod4+shift+H").unwrap(), combo);
    }

    #[test]
    fn nonsense_combos_are_rejected() {
        assert!(parse_combo("").is_err());
        assert!(parse_combo("Super+").is_err());
        assert!(parse_combo("Hyper+h").is_err());
        assert!(parse_combo("Super+notakey").is_err());
    }

    #[test]
    fn a_binding_needs_its_exact_modifiers() {
        let keymap = Keymap::defaults();
        let h = xkb::keysym_from_name("h", xkb::KEYSYM_CASE_INSENSITIVE);
        assert_eq!(
            keymap.action_for(&mods(true, false, false, false), h),
            Some(&Action::Focus(Direction::Left))
        );
        assert_eq!(
            keymap.action_for(&mods(true, true, false, false), h),
            Some(&Action::MoveWindow(Direction::Left))
        );
        // Without the modifier the key belongs to the client.
        assert_eq!(
            keymap.action_for(&mods(false, false, false, false), h),
            None
        );
        // A modifier the binding did not ask for must not match either.
        assert_eq!(keymap.action_for(&mods(true, false, true, true), h), None);
    }

    #[test]
    fn rebinding_replaces_rather_than_shadows() {
        let mut keymap = Keymap::defaults();
        let before = keymap.binds().count();
        let combo = parse_combo("Super+h").unwrap();
        keymap.bind(combo, Action::Close);
        assert_eq!(keymap.binds().count(), before);
        let h = xkb::keysym_from_name("h", xkb::KEYSYM_CASE_INSENSITIVE);
        assert_eq!(
            keymap.action_for(&mods(true, false, false, false), h),
            Some(&Action::Close)
        );
    }

    #[test]
    fn no_two_defaults_share_a_combination() {
        let keymap = Keymap::defaults();
        let mut seen: Vec<String> = keymap.binds().map(|(c, _)| c.to_string()).collect();
        let total = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(
            seen.len(),
            total,
            "a default binding is shadowed by another"
        );
    }
}
