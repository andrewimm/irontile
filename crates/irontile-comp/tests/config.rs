//! Tests that the configuration file reaches the layout engine, and that a
//! reload takes effect without restarting.

mod harness;

use harness::{Compositor, TempConfig};
use irontile_ipc::Action;

#[test]
fn a_missing_config_file_is_not_an_error() {
    // The harness already points at a nonexistent path; the compositor is
    // expected to come up on defaults rather than refuse to start.
    let mut compositor = Compositor::start("1920x1080");
    let layout = compositor.client.layout().unwrap();
    assert_eq!(layout.config().params.inner_gap, 4);
}

#[test]
fn configured_gaps_reach_the_layout_engine() {
    let config = TempConfig::new(
        r#"
        [theme]
        inner_gap = 17
        outer_gap = 23
        "#,
    );
    let mut compositor = Compositor::with_config("1920x1080", Some(config.path()));
    let layout = compositor.client.layout().unwrap();
    assert_eq!(layout.config().params.inner_gap, 17);
    assert_eq!(layout.config().params.outer_gap, 23);
}

#[test]
fn layout_policy_is_configurable() {
    let config = TempConfig::new(
        r#"
        [layout]
        smart_split = false
        reap_empty_workspaces = false
        "#,
    );
    let mut compositor = Compositor::with_config("1920x1080", Some(config.path()));
    let layout = compositor.client.layout().unwrap();
    assert!(!layout.config().smart_split);
    assert!(!layout.config().reap_empty_workspaces);

    // With reaping off, the desktop left behind by a switch must survive.
    compositor.client.action(Action::Workspace(2)).unwrap();
    let layout = compositor.client.layout().unwrap();
    assert_eq!(layout.workspaces().count(), 2);
}

#[test]
fn reloading_picks_up_a_changed_file() {
    let config = TempConfig::new("[theme]\ninner_gap = 2\n");
    let mut compositor = Compositor::with_config("1920x1080", Some(config.path()));
    assert_eq!(
        compositor
            .client
            .layout()
            .unwrap()
            .config()
            .params
            .inner_gap,
        2
    );

    config.rewrite("[theme]\ninner_gap = 30\n");
    compositor.client.action(Action::Reload).unwrap();
    assert_eq!(
        compositor
            .client
            .layout()
            .unwrap()
            .config()
            .params
            .inner_gap,
        30
    );
}

#[test]
fn a_broken_config_leaves_the_running_one_in_place() {
    let config = TempConfig::new("[theme]\ninner_gap = 11\n");
    let mut compositor = Compositor::with_config("1920x1080", Some(config.path()));
    assert_eq!(
        compositor
            .client
            .layout()
            .unwrap()
            .config()
            .params
            .inner_gap,
        11
    );

    config.rewrite("[theme]\ninner_gap = \"not a number\"\n");
    compositor.client.action(Action::Reload).unwrap();

    // Still running, still on the configuration that worked. Exiting over a
    // typo would take the session with it.
    let layout = compositor.client.layout().unwrap();
    assert_eq!(layout.config().params.inner_gap, 11);
    layout.validate().unwrap();
}

/// What the Caps Lock key is bound to, read out of a compiled keymap.
///
/// Returned as the text of the key's whole block, because neither half of its
/// shape is stable. Some versions of xkbcommon write keysyms by name and others
/// numerically, so a caller has to accept `Escape` or `0xff1b` for the same
/// binding; and a key with nothing else set on it is written as a one-liner
/// while a remapped one is not, so reading a fixed number of lines finds the
/// *next* key's symbols in the first case.
fn caps_binding(keymap: &str) -> String {
    let lines: Vec<&str> = keymap.lines().collect();
    let key = lines
        .iter()
        .position(|line| line.trim_start().starts_with("key <CAPS>"))
        .expect("every keymap binds a Caps Lock key");
    let mut block = String::new();
    for line in &lines[key..] {
        block.push_str(line.trim());
        block.push(' ');
        if line.contains("};") {
            break;
        }
    }
    block
}

fn keymap_with(config: Option<&TempConfig>) -> String {
    let compositor = Compositor::with_config("1920x1080", config.map(TempConfig::path));
    let mut client = compositor.connect_client();
    let mut keymap = None;
    client.wait_for(|client| {
        keymap = client.keymap();
        keymap.is_some()
    });
    keymap.expect("the compositor sends every keyboard a keymap")
}

#[test]
fn xkb_options_reach_the_keymap_clients_are_given() {
    // The one keyboard setting a binding cannot stand in for: a binding maps a
    // key to a compositor action, never to another key. Asserted from the
    // client's side, because the keymap the compositor compiled is only
    // observable as the one it hands out.
    let config = TempConfig::new(
        r#"
        [input.keyboard]
        layout = "us"
        options = "caps:escape"
        "#,
    );
    let remapped = caps_binding(&keymap_with(Some(&config)));
    assert!(
        remapped.contains("Escape") || remapped.contains("0xff1b"),
        "Caps Lock should produce Escape, but the keymap says {remapped:?}"
    );

    // And the contrast, so this says the setting did it rather than that the
    // keyboard was always like that: without the option the key is still a
    // Caps Lock, 0xffe5.
    let plain = caps_binding(&keymap_with(None));
    assert!(
        plain.contains("Caps_Lock") || plain.contains("0xffe5"),
        "without the option Caps Lock should be a Caps Lock, but the keymap says {plain:?}"
    );
}

#[test]
fn a_keymap_that_will_not_compile_falls_back_rather_than_refusing_to_start() {
    // On real hardware a compositor that will not start over a mistyped option
    // leaves no session to fix it from.
    let config = TempConfig::new(
        r#"
        [input.keyboard]
        layout = "definitely-not-a-layout"
        "#,
    );
    let plain = caps_binding(&keymap_with(Some(&config)));
    assert!(
        plain.contains("Caps_Lock") || plain.contains("0xffe5"),
        "the default keymap should have been used, but the keymap says {plain:?}"
    );
}
