//! The notification daemon, and the panel its notifications collect in.

use std::process::ExitCode;

use irontile_ui::draw::TextRenderer;
use irontile_ui::icon::IconSet;
use irontile_ui::notify::model::{Action, Centre, Hit, Notification, Urgency, power_buttons};
use irontile_ui::notify::paint::{self, Palette, size};
use irontile_ui::notify::{service, ui};

/// What a second invocation is asking the first one to do.
#[derive(Clone, Copy)]
enum Ask {
    Toggle,
    Open,
    Close,
}

const HELP: &str = "\
irontile-notify - notifications for irontile, and the panel they collect in

USAGE:
    irontile-notify [OPTIONS]

OPTIONS:
    --toggle        Show the panel if it is hidden, hide it if it is shown.
                    Asks the running daemon over the bus, which is how a key
                    binding reaches it: the key arrives at the compositor.
    --open          Show the panel.
    --close         Hide it.
    --dump PATH     Render the popups and the panel to a PNG and exit, without
                    a compositor and without anything having sent a
                    notification. The only way to work on how these look
                    without waiting for something to happen.
    --scale N       Scale to render for --dump. Default 1.
    --icon-theme NAME
                    Icon theme to draw the session buttons from.
                    Default: Adwaita.
    --version       Show the version and exit.
    --help          Show this message.
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("irontile-notify: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut dump: Option<String> = None;
    let mut toggle: Option<Ask> = None;
    let mut scale = 1.0_f32;
    let mut theme = "Adwaita".to_string();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dump" => dump = Some(args.next().ok_or("--dump needs a path")?),
            "--scale" => {
                scale = args
                    .next()
                    .ok_or("--scale needs a number")?
                    .parse()
                    .map_err(|_| "--scale needs a number")?;
            }
            "--icon-theme" => theme = args.next().ok_or("--icon-theme needs a name")?,
            "--toggle" => toggle = Some(Ask::Toggle),
            "--open" => toggle = Some(Ask::Open),
            "--close" => toggle = Some(Ask::Close),
            "--version" | "-V" => {
                println!(
                    "{}",
                    irontile_version::line("irontile-notify", env!("CARGO_PKG_VERSION"))
                );
                return Ok(());
            }
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(());
            }
            other => return Err(format!("unknown option {other:?}; try --help")),
        }
    }

    match (dump, toggle) {
        (Some(path), _) => sheet(&path, scale, &theme),
        (None, Some(what)) => ask(what),
        (None, None) => daemon(&theme),
    }
}

/// Tells the running daemon to show or hide the panel.
///
/// A key binding arrives at the compositor rather than here, so the binding
/// runs this and this asks the daemon over the bus -- which is also why it is
/// an error rather than a no-op when nothing answers: a key that silently does
/// nothing is worse than one that says why.
fn ask(what: Ask) -> Result<(), String> {
    let connection = zbus::blocking::Connection::session()
        .map_err(|err| format!("could not reach the session bus: {err}"))?;
    let method = match what {
        Ask::Toggle => "Toggle",
        Ask::Open => "Open",
        Ask::Close => "Close",
    };
    connection
        .call_method(
            Some("org.irontile.Notify"),
            "/org/irontile/Notify",
            Some("org.irontile.Notify1"),
            method,
            &(),
        )
        .map_err(|err| format!("no notification daemon answered: {err}"))?;
    Ok(())
}

/// Runs the daemon: the bus on one thread, the surfaces on this one.
fn daemon(theme: &str) -> Result<(), String> {
    let service = service::start()?;
    let families = vec!["Noto Sans".to_string()];
    eprintln!("irontile-notify: answering org.freedesktop.Notifications");
    ui::run(&service, theme, &families)
}

/// Renders the popups and the panel side by side.
fn sheet(path: &str, scale: f32, theme: &str) -> Result<(), String> {
    let palette = Palette::default();
    let mut icons = IconSet::new(theme, None);
    let families = vec!["Noto Sans".to_string()];
    let mut text = TextRenderer::new(&families, 13.0 * scale);

    let popups = examples();
    let px = |v: f32| v * scale;
    let gutter = px(24.0);
    let panel_w = px(size::PANEL_W);
    let panel_h = px(760.0);
    let card_w = px(size::CARD_W);
    let width = (gutter * 3.0 + card_w + panel_w).round() as u32;
    let height = (panel_h + gutter * 2.0).round() as u32;

    let mut pixmap =
        tiny_skia::Pixmap::new(width, height).ok_or("that size is too large to render")?;
    pixmap.fill(tiny_skia::Color::from_rgba8(0x0a, 0x09, 0x09, 0xff));

    // The popups, as they appear at the corner of a display.
    let mut y = gutter;
    for note in &popups {
        let mut canvas = pixmap.as_mut();
        let height = paint::card(
            &mut canvas,
            &mut icons,
            &mut text,
            note,
            &palette,
            (gutter, y),
            scale,
        );
        y += height + px(size::GAP);
    }

    // The panel, with everything still in it.
    let centre = Centre {
        notifications: popups.clone(),
        quiet: false,
        buttons: power_buttons(),
        hovered: Some(Hit::Power(1)),
    };
    let mut panel = tiny_skia::Pixmap::new(panel_w.round() as u32, panel_h.round() as u32)
        .ok_or("that size is too large to render")?;
    paint::centre(
        &mut panel.as_mut(),
        &mut icons,
        &mut text,
        &centre,
        &palette,
        (panel_w, panel_h),
        scale,
    );
    pixmap.draw_pixmap(
        (gutter * 2.0 + card_w).round() as i32,
        gutter.round() as i32,
        panel.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        None,
    );

    let png = pixmap.encode_png().map_err(|err| format!("{err}"))?;
    std::fs::write(path, png).map_err(|err| format!("could not write {path}: {err}"))?;
    println!("wrote {path} ({width}x{height})");
    Ok(())
}

/// One of each kind of notification, which is what there is to look at.
fn examples() -> Vec<Notification> {
    vec![
        Notification {
            id: 1,
            app: "Signal".to_string(),
            icon: Some("mail-unread-symbolic".to_string()),
            summary: "Rachel".to_string(),
            body:
                "Are we still on for tomorrow? I can move things around if the morning is easier."
                    .to_string(),
            age: std::time::Duration::from_secs(90),
            ..Notification::default()
        },
        Notification {
            id: 2,
            app: "irontile".to_string(),
            icon: Some("battery-caution-symbolic".to_string()),
            summary: "Battery low".to_string(),
            body: "14 percent remaining, and nothing is plugged in.".to_string(),
            urgency: Urgency::Critical,
            age: std::time::Duration::from_secs(60 * 7),
            ..Notification::default()
        },
        Notification {
            id: 3,
            app: "Transmission".to_string(),
            icon: None,
            summary: "Download finished".to_string(),
            body: "archlinux-2026.09.01-x86_64.iso".to_string(),
            actions: vec![
                Action {
                    key: "open".to_string(),
                    label: "Open folder".to_string(),
                },
                Action {
                    key: "dismiss".to_string(),
                    label: "Dismiss".to_string(),
                },
            ],
            age: std::time::Duration::from_secs(60 * 62),
            ..Notification::default()
        },
    ]
}

#[cfg(test)]
mod man_page {
    /// Every option the help lists is in the man page.
    ///
    /// Two places that say the same thing drift, and the drift that actually
    /// happens is a new flag added here and forgotten there -- not a man page
    /// inventing an option, which is a mistake somebody makes once. Comparing
    /// them costs nothing and the failure names the flag.
    #[test]
    fn every_option_is_in_the_man_page() {
        let page = include_str!("../../../../assets/man/irontile-notify.1");
        for word in super::HELP.split_whitespace() {
            if !word.starts_with("--") {
                continue;
            }
            let flag = word.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
            // roff escapes a leading hyphen, and every hyphen in a flag is one.
            let roff = flag.replace('-', "\\-");
            assert!(
                page.contains(&roff),
                "{flag} is in --help but not in assets/man/irontile-notify.1"
            );
        }
    }
}
