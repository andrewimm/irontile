//! The default key bindings.
//!
//! A binding resolves to an [`Action`], which the input layer turns into one or
//! more layout commands. Nothing here touches layout state, so the whole
//! keymap stays a pure function of modifiers and keysym.

use irontile_layout::{Axis, Direction};
use smithay::input::keyboard::{Keysym, ModifiersState, keysyms};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Focus(Direction),
    Move(Direction),
    Resize(Direction),
    SetAxis(Option<Axis>),
    Equalize,
    ToggleFloating,
    ToggleFullscreen,
    CloseWindow,
    ShowWorkspace(u32),
    MoveToWorkspace(u32),
    FocusOutput(Direction),
    SendWorkspaceToOutput(Direction),
    SpawnTerminal,
    Quit,
}

/// The modifier every binding is behind.
fn modifier(mods: &ModifiersState) -> bool {
    mods.logo
}

/// Maps a keypress to an action.
///
/// The keysym passed in should be the unmodified one, so that `Super+Shift+1`
/// reports `1` rather than `!` and the numeric bindings work on every layout.
pub fn action_for(mods: &ModifiersState, keysym: Keysym) -> Option<Action> {
    if !modifier(mods) {
        return None;
    }
    let raw = keysym.raw();

    if let Some(number) = workspace_number(raw) {
        return Some(if mods.shift {
            Action::MoveToWorkspace(number)
        } else {
            Action::ShowWorkspace(number)
        });
    }

    let dir = direction(raw);

    match (dir, mods.shift, mods.ctrl, mods.alt) {
        // Super + direction: move focus.
        (Some(dir), false, false, false) => return Some(Action::Focus(dir)),
        // Super + Shift + direction: move the window.
        (Some(dir), true, false, false) => return Some(Action::Move(dir)),
        // Super + Ctrl + direction: resize.
        (Some(dir), false, true, false) => return Some(Action::Resize(dir)),
        // Super + Alt + direction: move focus between displays.
        (Some(dir), false, false, true) => return Some(Action::FocusOutput(dir)),
        // Super + Shift + Alt + direction: send this desktop to that display.
        (Some(dir), true, false, true) => return Some(Action::SendWorkspaceToOutput(dir)),
        _ => {}
    }

    match (raw, mods.shift) {
        (keysyms::KEY_Return, false) => Some(Action::SpawnTerminal),
        (keysyms::KEY_q, false) => Some(Action::CloseWindow),
        (keysyms::KEY_e, true) => Some(Action::Quit),
        (keysyms::KEY_f, false) => Some(Action::ToggleFullscreen),
        (keysyms::KEY_space, true) => Some(Action::ToggleFloating),
        // Split the focused container the other way.
        (keysyms::KEY_v, false) => Some(Action::SetAxis(Some(Axis::Vertical))),
        (keysyms::KEY_b, false) => Some(Action::SetAxis(Some(Axis::Horizontal))),
        (keysyms::KEY_t, false) => Some(Action::SetAxis(None)),
        (keysyms::KEY_o, false) => Some(Action::Equalize),
        _ => None,
    }
}

/// Both vi keys and arrows, so muscle memory from either works.
fn direction(raw: u32) -> Option<Direction> {
    match raw {
        keysyms::KEY_h | keysyms::KEY_Left => Some(Direction::Left),
        keysyms::KEY_j | keysyms::KEY_Down => Some(Direction::Down),
        keysyms::KEY_k | keysyms::KEY_Up => Some(Direction::Up),
        keysyms::KEY_l | keysyms::KEY_Right => Some(Direction::Right),
        _ => None,
    }
}

/// `Super+0` is desktop 10, matching how the number row reads.
fn workspace_number(raw: u32) -> Option<u32> {
    match raw {
        keysyms::KEY_1..=keysyms::KEY_9 => Some(raw - keysyms::KEY_1 + 1),
        keysyms::KEY_0 => Some(10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn bindings_require_the_modifier() {
        let plain = mods(false, false, false, false);
        assert_eq!(action_for(&plain, Keysym::from(keysyms::KEY_h)), None);
    }

    #[test]
    fn direction_bindings_layer_by_modifier() {
        let sym = Keysym::from(keysyms::KEY_l);
        assert_eq!(
            action_for(&mods(true, false, false, false), sym),
            Some(Action::Focus(Direction::Right))
        );
        assert_eq!(
            action_for(&mods(true, true, false, false), sym),
            Some(Action::Move(Direction::Right))
        );
        assert_eq!(
            action_for(&mods(true, false, true, false), sym),
            Some(Action::Resize(Direction::Right))
        );
        assert_eq!(
            action_for(&mods(true, false, false, true), sym),
            Some(Action::FocusOutput(Direction::Right))
        );
        assert_eq!(
            action_for(&mods(true, true, false, true), sym),
            Some(Action::SendWorkspaceToOutput(Direction::Right))
        );
    }

    #[test]
    fn arrows_mirror_the_vi_keys() {
        let m = mods(true, false, false, false);
        for (vi, arrow) in [
            (keysyms::KEY_h, keysyms::KEY_Left),
            (keysyms::KEY_j, keysyms::KEY_Down),
            (keysyms::KEY_k, keysyms::KEY_Up),
            (keysyms::KEY_l, keysyms::KEY_Right),
        ] {
            assert_eq!(
                action_for(&m, Keysym::from(vi)),
                action_for(&m, Keysym::from(arrow))
            );
        }
    }

    #[test]
    fn the_number_row_addresses_desktops() {
        let m = mods(true, false, false, false);
        assert_eq!(
            action_for(&m, Keysym::from(keysyms::KEY_1)),
            Some(Action::ShowWorkspace(1))
        );
        assert_eq!(
            action_for(&m, Keysym::from(keysyms::KEY_9)),
            Some(Action::ShowWorkspace(9))
        );
        // Zero sits at the end of the row, so it addresses ten.
        assert_eq!(
            action_for(&m, Keysym::from(keysyms::KEY_0)),
            Some(Action::ShowWorkspace(10))
        );

        let shifted = mods(true, true, false, false);
        assert_eq!(
            action_for(&shifted, Keysym::from(keysyms::KEY_4)),
            Some(Action::MoveToWorkspace(4))
        );
    }
}
