//! The notification daemon, and the panel its notifications collect in.

use std::process::ExitCode;

use irontile_ui::draw::TextRenderer;
use irontile_ui::icon::IconSet;
use irontile_ui::notify::model::{Action, Centre, Hit, Notification, Urgency, power_buttons};
use irontile_ui::notify::paint::{self, Palette, size};

const HELP: &str = "\
irontile-notify - notifications for irontile, and the panel they collect in

USAGE:
    irontile-notify [OPTIONS]

OPTIONS:
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

    match dump {
        Some(path) => sheet(&path, scale, &theme),
        // The daemon itself is the next thing to be written; until it exists,
        // say so rather than sitting there doing nothing.
        None => Err("only --dump so far; the daemon is not wired up yet".into()),
    }
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
