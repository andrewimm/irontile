//! Applying the configuration to input devices.
//!
//! Scroll direction, click method and the rest are libinput's to enforce, not
//! the compositor's to imitate. libinput knows which physical device an event
//! came from; the compositor sees only the event, and a touchpad, a trackpoint
//! and a wheel can all produce the same one. Settings applied here therefore
//! land on the device that was configured and nothing else.
//!
//! Only the session backend has devices to configure. Running nested, the
//! compositor underneath has already applied its own, which is why these
//! settings do nothing there -- the same as every other input setting.

use smithay::reexports::input::{ClickMethod as LibinputClickMethod, Device};

use crate::config::{ClickMethod, InputConfig};

/// What to set on one device, once it is known which kind it is.
///
/// Every field is optional in the strong sense: `None` means the device keeps
/// whatever libinput decided, which is not the same as setting the value that
/// default happens to have.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    pub natural_scroll: Option<bool>,
    pub click_method: Option<ClickMethod>,
    pub tap_to_click: Option<bool>,
    pub middle_button_emulation: Option<bool>,
}

/// Which settings apply to a device.
///
/// Split from applying them so the choice can be tested without a touchpad: the
/// interesting part is that a touchpad takes the touchpad section and
/// everything else takes only the setting that is not specific to one.
pub fn settings(config: &InputConfig, touchpad: bool) -> Settings {
    if !touchpad {
        return Settings {
            natural_scroll: config.natural_scroll,
            ..Settings::default()
        };
    }
    Settings {
        natural_scroll: config.touchpad.natural_scroll,
        click_method: config.touchpad.click_method,
        tap_to_click: config.touchpad.tap_to_click,
        middle_button_emulation: config.touchpad.middle_button_emulation,
    }
}

/// Configures a device as it appears.
pub fn apply(device: &mut Device, config: &InputConfig) {
    // A device that can count fingers for a tap is a touchpad; nothing else
    // reports one. libinput offers no plainer question than this.
    let touchpad = device.config_tap_finger_count() > 0;
    let settings = settings(config, touchpad);
    let name = device.name().to_string();

    if let Some(enabled) = settings.natural_scroll
        && device.config_scroll_has_natural_scroll()
    {
        report(
            &name,
            "natural scroll",
            device.config_scroll_set_natural_scroll_enabled(enabled),
        );
    }
    if let Some(method) = settings.click_method {
        let wanted = match method {
            ClickMethod::Clickfinger => LibinputClickMethod::Clickfinger,
            ClickMethod::ButtonAreas => LibinputClickMethod::ButtonAreas,
        };
        // Asking for a method the device does not have would be refused anyway,
        // but saying so is more use than a silent no-op on a setting somebody
        // deliberately wrote down.
        if device.config_click_methods().contains(&wanted) {
            report(
                &name,
                "click method",
                device.config_click_set_method(wanted),
            );
        } else {
            tracing::warn!(
                device = name,
                ?method,
                "the device does not offer this click method"
            );
        }
    }
    if let Some(enabled) = settings.tap_to_click {
        report(
            &name,
            "tap to click",
            device.config_tap_set_enabled(enabled),
        );
    }
    if let Some(enabled) = settings.middle_button_emulation {
        report(
            &name,
            "middle button emulation",
            device.config_middle_emulation_set_enabled(enabled),
        );
    }

    tracing::debug!(
        device = name,
        touchpad,
        ?settings,
        "configured an input device"
    );
}

/// Runs one setting and says so if the device refused it.
///
/// A refusal is worth a line: it means a setting someone wrote in a file is not
/// in force, and the only other evidence is the device not behaving.
fn report(
    device: &str,
    what: &str,
    result: Result<(), smithay::reexports::input::DeviceConfigError>,
) {
    if let Err(err) = result {
        tracing::warn!(device, what, ?err, "a device refused a setting");
    }
}

#[cfg(test)]
mod tests {
    use super::{Settings, settings};
    use crate::config::{ClickMethod, InputConfig, TouchpadConfig};

    fn config() -> InputConfig {
        InputConfig {
            natural_scroll: Some(false),
            touchpad: TouchpadConfig {
                natural_scroll: Some(true),
                click_method: Some(ClickMethod::Clickfinger),
                tap_to_click: Some(false),
                middle_button_emulation: Some(false),
            },
            ..InputConfig::default()
        }
    }

    #[test]
    fn a_touchpad_takes_the_touchpad_section() {
        assert_eq!(
            settings(&config(), true),
            Settings {
                natural_scroll: Some(true),
                click_method: Some(ClickMethod::Clickfinger),
                tap_to_click: Some(false),
                middle_button_emulation: Some(false),
            }
        );
    }

    #[test]
    fn everything_else_takes_only_what_is_not_specific_to_a_touchpad() {
        // A mouse has no tap and no click method to speak of, and writing a
        // touchpad's settings onto one would be configuring a device the file
        // never mentioned.
        assert_eq!(
            settings(&config(), false),
            Settings {
                natural_scroll: Some(false),
                ..Settings::default()
            }
        );
    }

    #[test]
    fn a_file_that_says_nothing_changes_nothing() {
        // The whole point of the settings being optional: a compositor that
        // wrote a value for every one of these on startup would override
        // choices it was never asked about.
        for touchpad in [true, false] {
            assert_eq!(
                settings(&InputConfig::default(), touchpad),
                Settings::default(),
                "touchpad={touchpad}"
            );
        }
    }
}
