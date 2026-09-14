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
