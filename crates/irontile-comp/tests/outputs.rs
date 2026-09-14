//! Display arrangement from configuration.
//!
//! Driven against the headless backend, which names its displays `HEADLESS-1`
//! and so on, so an `[[output]]` section can be matched against them exactly as
//! it would be against a real connector.

mod harness;

use harness::{Compositor, TempConfig};
use irontile_ipc::{Output, Rect};

fn outputs(compositor: &mut Compositor) -> Vec<Output> {
    match compositor
        .client
        .query(irontile_ipc::Query::Outputs)
        .unwrap()
    {
        irontile_ipc::ResponsePayload::Outputs(outputs) => outputs,
        other => panic!("expected outputs, got {other:?}"),
    }
}

#[test]
fn displays_without_configuration_are_laid_end_to_end() {
    let mut compositor = Compositor::start("1920x1080,1280x1024");
    let outputs = outputs(&mut compositor);
    assert_eq!(outputs[0].logical, Rect::new(0, 0, 1920, 1080));
    assert_eq!(outputs[1].logical, Rect::new(1920, 0, 1280, 1024));
}

#[test]
fn a_configured_position_is_honoured() {
    let config = TempConfig::new(
        r#"
        [[output]]
        name = "HEADLESS-2"
        position = [0, 1080]
        "#,
    );
    let mut compositor = Compositor::with_config("1920x1080,1280x1024", Some(config.path()));
    let outputs = outputs(&mut compositor);
    // Stacked rather than side by side, which is what someone with a monitor
    // above their laptop wants.
    assert_eq!(outputs[1].logical, Rect::new(0, 1080, 1280, 1024));
    compositor.client.layout().unwrap().validate().unwrap();
}

#[test]
fn scale_divides_the_logical_size() {
    let config = TempConfig::new(
        r#"
        [[output]]
        name = "HEADLESS-1"
        scale = 2.0
        "#,
    );
    let mut compositor = Compositor::with_config("2256x1504", Some(config.path()));
    let outputs = outputs(&mut compositor);
    // The panel still scans out 2256x1504; windows are laid out in half that.
    assert_eq!(outputs[0].logical, Rect::new(0, 0, 1128, 752));
}

#[test]
fn a_wildcard_applies_to_every_display_without_its_own_entry() {
    let config = TempConfig::new(
        r#"
        [[output]]
        name = "*"
        scale = 2.0

        [[output]]
        name = "HEADLESS-2"
        scale = 1.0
        "#,
    );
    let mut compositor = Compositor::with_config("1920x1080,1280x1024", Some(config.path()));
    let outputs = outputs(&mut compositor);
    assert_eq!(
        outputs[0].logical.w, 960,
        "the wildcard should have applied"
    );
    assert_eq!(
        outputs[1].logical.w, 1280,
        "an exact name should win over it"
    );
}

#[test]
fn a_disabled_display_is_not_part_of_the_arrangement() {
    let config = TempConfig::new(
        r#"
        [[output]]
        name = "HEADLESS-1"
        enabled = false
        "#,
    );
    let mut compositor = Compositor::with_config("1920x1080,1280x1024", Some(config.path()));
    let outputs = outputs(&mut compositor);
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].name, "HEADLESS-2");
    // And whatever is left still has a desktop on it.
    compositor.client.layout().unwrap().validate().unwrap();
}

#[test]
fn an_unpositioned_display_never_shifts_a_positioned_one() {
    let config = TempConfig::new(
        r#"
        [[output]]
        name = "HEADLESS-1"
        position = [500, 300]
        "#,
    );
    let mut compositor = Compositor::with_config("1920x1080,1280x1024", Some(config.path()));
    let outputs = outputs(&mut compositor);
    assert_eq!(outputs[0].logical, Rect::new(500, 300, 1920, 1080));
    // The unpositioned one goes to the right of everything placed, rather than
    // landing on top of it at the origin.
    assert_eq!(outputs[1].logical.x, 2420);
}

#[test]
fn windows_tile_into_the_scaled_logical_size() {
    let config = TempConfig::new(
        r#"
        [[output]]
        name = "HEADLESS-1"
        scale = 2.0
        "#,
    );
    let mut compositor = Compositor::with_config("2256x1504", Some(config.path()));
    let mut client = compositor.connect_client();
    let window = client.map_window("solo");
    let frame = compositor.wait_for_windows(1);
    // Logical 1128x752, less the 4px outer gap on each side.
    assert_eq!(frame.placements[0].rect, Rect::new(4, 4, 1120, 744));

    // And the client is told the same thing. A configure carries logical
    // pixels, so a scaled display must not shrink the number the client sees;
    // a client told half its cell renders a quarter of one.
    let configured = client.configured(window);
    assert_eq!(
        (configured.width, configured.height),
        (1120 - 4, 744 - 4),
        "the client was configured for a different size than its cell"
    );
}

#[test]
fn a_bad_output_entry_names_the_display_it_came_from() {
    // A typo here would otherwise leave a monitor mysteriously unconfigured.
    let config = TempConfig::new(
        r#"
        [[output]]
        name = "HEADLESS-1"
        mode = "not-a-mode"
        "#,
    );
    let mut compositor = Compositor::with_config("1920x1080", Some(config.path()));
    // The compositor still starts, on defaults, rather than refusing to run.
    let outputs = outputs(&mut compositor);
    assert_eq!(outputs[0].logical, Rect::new(0, 0, 1920, 1080));
}

#[test]
fn four_thirds_divides_a_2256x1504_panel_exactly() {
    // The scale this laptop panel actually wants: 2256 * 3/4 = 1692 and
    // 1504 * 3/4 = 1128, both whole, so nothing is lost to rounding.
    let config = TempConfig::new(
        r#"
        [[output]]
        name = "HEADLESS-1"
        scale = 1.3333
        "#,
    );
    let mut compositor = Compositor::with_config("2256x1504", Some(config.path()));
    let outputs = outputs(&mut compositor);
    assert_eq!(outputs[0].logical, Rect::new(0, 0, 1692, 1128));
}

#[test]
fn a_client_is_told_the_same_scale_the_layout_used() {
    let config = TempConfig::new(
        r#"
        [[output]]
        name = "HEADLESS-1"
        scale = 1.3333
        "#,
    );
    let mut compositor = Compositor::with_config("2256x1504", Some(config.path()));
    let mut client = compositor.connect_client();
    let window = client.map_window("solo");
    compositor.wait_for_windows(1);

    // 160/120 is four thirds exactly. Had the compositor kept 1.3333 and told
    // the client 160, the two would be laying out against different numbers.
    client.wait_for(|c| c.configured(window).fractional_scale.is_some());
    assert_eq!(client.configured(window).fractional_scale, Some(160));

    let layout = compositor.client.layout().unwrap();
    assert_eq!(layout.outputs()[0].logical.w, 1692);
}
