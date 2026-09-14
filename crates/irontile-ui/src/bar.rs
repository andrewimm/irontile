//! Laying a bar out and drawing it.
//!
//! Kept apart from the Wayland plumbing so that a bar can be rendered into a
//! buffer and looked at without a compositor being involved, which is what
//! makes it testable at all.

use tiny_skia::{Pixmap, PixmapMut};

use crate::config::{Config, ModuleConfig, Position};
use crate::draw::{TextRenderer, fill};
use crate::icon::IconSet;
use crate::module::{Button, Buttons, Click, Level, Piece, Segment, Snapshot, World, render};
use crate::theme::Color;

/// Where a bar is going: how large, at what scale, and which of its modules
/// are showing their second format.
pub struct Target<'a> {
    pub width: u32,
    pub scale: f32,
    pub alt: &'a dyn Fn(&str) -> bool,
}

impl std::fmt::Debug for Target<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("width", &self.width)
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}

/// A drawn segment and where it ended up, so a click can be matched to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub x: f32,
    pub width: f32,
    pub buttons: Buttons,
    /// Shown while the pointer rests inside it.
    pub tooltip: Option<String>,
}

/// One rendered bar: its pixels and what can be clicked in them.
pub struct Frame {
    pub pixmap: Pixmap,
    pub hits: Vec<Hit>,
}

impl std::fmt::Debug for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Frame")
            .field("size", &(self.pixmap.width(), self.pixmap.height()))
            .field("hits", &self.hits.len())
            .finish()
    }
}

/// Draws the whole bar at the given pixel size.
///
/// `scale` is the display's, so a bar on a scaled screen is drawn at the
/// resolution it will actually be shown at rather than being magnified.
pub fn draw(
    config: &Config,
    text: &mut TextRenderer,
    icons: &mut IconSet,
    snapshot: &Snapshot,
    world: &dyn World,
    target: &Target<'_>,
) -> Frame {
    let Target { width, scale, alt } = *target;
    // Everything is in buffer pixels from here: the configuration is written in
    // logical ones and the display's scale is what stands between them.
    text.set_size(config.font_size * scale);
    let height = (config.height as f32 * scale).round().max(1.0);
    let mut pixmap = Pixmap::new(width.max(1), height as u32)
        .unwrap_or_else(|| Pixmap::new(1, 1).expect("a one pixel buffer is always allocatable"));
    let mut canvas = pixmap.as_mut();
    let w = width as f32;

    fill(&mut canvas, 0.0, 0.0, w, height, config.background);

    let padding = config.padding as f32 * scale;
    let mut hits = Vec::new();

    let regions = [
        (&config.left, Align::Left),
        (&config.center, Align::Center),
        (&config.right, Align::Right),
    ];
    for (names, align) in regions {
        let segments = collect(config, names, snapshot, world, alt);
        // Icons are square and sized from the text, so they sit on the line
        // rather than beside it.
        let icon_size = (config.font_size * scale).round().max(1.0) as u32;
        let widths: Vec<f32> = segments
            .iter()
            .map(|s| measure(text, icons, s, icon_size) + padding * 2.0)
            .collect();
        let total: f32 = widths.iter().sum();
        let mut x = match align {
            Align::Left => 0.0,
            Align::Center => (w - total) / 2.0,
            Align::Right => w - total,
        };

        for (segment, segment_width) in segments.iter().zip(&widths) {
            // The focused desktop is picked out behind the text and underlined,
            // which reads as selection rather than as alarm.
            if let Some(background) = segment.style.background {
                fill(&mut canvas, x, 0.0, *segment_width, height, background);
            }
            if segment.focused {
                fill(
                    &mut canvas,
                    x,
                    0.0,
                    *segment_width,
                    height,
                    config.focus_background,
                );
                let thickness = (config.accent_width as f32 * scale).max(1.0);
                fill(
                    &mut canvas,
                    x,
                    height - thickness,
                    *segment_width,
                    thickness,
                    config.focus_indicator,
                );
            }
            // A module may be painted to taste, but only in a state it named:
            // otherwise a crossed threshold takes the colour back, because
            // that colour is the message.
            let color = segment.style.color.unwrap_or(match segment.level {
                Level::Normal => config.foreground,
                Level::Warning => config.warning,
                Level::Critical => config.critical,
            });
            let mut cursor = x + padding;
            for piece in &segment.pieces {
                match piece {
                    Piece::Text(run) => {
                        text.draw(&mut canvas, run, cursor, height, color);
                        cursor += text.width(run);
                    }
                    Piece::Icon(name) => {
                        let top = (height - icon_size as f32) / 2.0;
                        if icons.draw(&mut canvas, name, cursor, top, icon_size, color) {
                            cursor += icon_size as f32;
                        }
                    }
                    Piece::Pixels(pixels) => {
                        let top = (height - icon_size as f32) / 2.0;
                        if icons.draw_pixels(&mut canvas, pixels, cursor, top, icon_size) {
                            cursor += icon_size as f32;
                        }
                    }
                }
            }
            // Recorded for anything the pointer can do something with, which
            // now includes resting on it rather than only clicking it.
            if !segment.buttons.is_empty() || segment.tooltip.is_some() {
                hits.push(Hit {
                    x,
                    width: *segment_width,
                    buttons: segment.buttons.clone(),
                    tooltip: segment.tooltip.clone(),
                });
            }
            x += segment_width;
        }
    }

    accent(&mut canvas, config, w, height, scale);
    Frame { pixmap, hits }
}

/// A line along the edge facing the desktop, which is where waybar's border
/// sits and what makes the bar read as attached to the screen edge.
fn accent(canvas: &mut PixmapMut<'_>, config: &Config, w: f32, height: f32, scale: f32) {
    let thickness = config.accent_width as f32 * scale;
    if thickness <= 0.0 {
        return;
    }
    let y = match config.position {
        Position::Top => height - thickness,
        Position::Bottom => 0.0,
    };
    fill(canvas, 0.0, y, w, thickness, config.accent);
}

enum Align {
    Left,
    Center,
    Right,
}

/// How wide a segment will be once drawn.
fn measure(text: &mut TextRenderer, icons: &mut IconSet, segment: &Segment, icon_size: u32) -> f32 {
    segment
        .pieces
        .iter()
        .map(|piece| match piece {
            Piece::Text(run) => text.width(run),
            // A name that resolves to nothing takes no room, so the line closes
            // up rather than leaving a gap where an icon would have been.
            // An item's own artwork always takes its square: unlike a name,
            // there is nothing to fail to resolve.
            Piece::Pixels(_) => icon_size as f32,
            Piece::Icon(name) => {
                if icons.has(name, icon_size) {
                    icon_size as f32
                } else {
                    0.0
                }
            }
        })
        .sum()
}

fn collect(
    config: &Config,
    names: &[String],
    snapshot: &Snapshot,
    world: &dyn World,
    alt: &dyn Fn(&str) -> bool,
) -> Vec<Segment> {
    let mut out = Vec::new();
    for name in names {
        let Some(module) = config.modules.get(name) else {
            continue;
        };
        let swapped;
        let module = match (&module.format_alt, alt(name)) {
            (Some(format), true) => {
                swapped = alternate(module, format);
                &swapped
            }
            _ => module,
        };
        for mut segment in render(module, snapshot, world) {
            // The left button toggles only if it has nothing else to do. A
            // module with an `on_click` said what it wanted that button for.
            if module.format_alt.is_some() && segment.buttons.left.is_none() {
                segment.buttons.left = Some(Click::Toggle(name.clone()));
            }
            out.push(segment);
        }
    }
    out
}

/// A module showing its second format.
///
/// The formats for particular states are dropped along the way: asking a
/// network module for the address means the address, connected by wire or not.
fn alternate(module: &ModuleConfig, format: &str) -> ModuleConfig {
    ModuleConfig {
        format: format.to_owned(),
        format_charging: None,
        format_muted: None,
        format_wifi: None,
        format_ethernet: None,
        format_disconnected: None,
        ..module.clone()
    }
}

/// What a click at `x` should do, if anything.
pub fn hit(hits: &[Hit], x: f32, button: Button) -> Option<&Click> {
    at(hits, x).and_then(|(_, hit)| hit.buttons.get(button))
}

/// The segment at `x`, if the pointer is over one, and where it sits in the
/// list.
///
/// The index is the segment's identity from one frame to the next: a module
/// whose text has changed is still the same module, and a tooltip resting on it
/// wants repainting rather than taking down and putting back up.
pub fn at(hits: &[Hit], x: f32) -> Option<(usize, &Hit)> {
    hits.iter()
        .enumerate()
        .find(|(_, hit)| x >= hit.x && x < hit.x + hit.width)
}

/// Draws a tooltip, sized to what it says.
///
/// Its own surface rather than part of the bar's: it has to hang below the bar
/// into the desktop, and a bar tall enough to contain it would either swallow
/// clicks meant for the window underneath or need a hole cut in it.
pub fn tooltip(config: &Config, text: &mut TextRenderer, body: &str, scale: f32) -> Option<Frame> {
    let style = &config.tooltip;
    let lines: Vec<&str> = body
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }

    let size = style.font_size.unwrap_or(config.font_size) * scale;
    text.set_size(size);
    let padding = style.padding as f32 * scale;
    let border = (style.border_width as f32 * scale).max(0.0);
    // The same line height the text renderer lays out with, so several lines
    // sit the way they would in a paragraph.
    let line_height = (size * 1.4).ceil();

    let widest = lines
        .iter()
        .map(|line| text.width(line))
        .fold(0.0_f32, f32::max);
    let w = (widest + (padding + border) * 2.0).ceil().max(1.0);
    let h = (line_height * lines.len() as f32 + (padding + border) * 2.0)
        .ceil()
        .max(1.0);

    let mut pixmap = Pixmap::new(w as u32, h as u32)?;
    let mut canvas = pixmap.as_mut();
    // Border first and the field on top of it, which leaves the border showing
    // as a ring rather than needing four strips of its own.
    fill(&mut canvas, 0.0, 0.0, w, h, style.border);
    fill(
        &mut canvas,
        border,
        border,
        w - border * 2.0,
        h - border * 2.0,
        style.background,
    );

    for (n, line) in lines.iter().enumerate() {
        let top = border + padding + line_height * n as f32;
        text.draw_at(&mut canvas, line, border + padding, top, style.foreground);
    }

    Some(Frame {
        pixmap,
        hits: Vec::new(),
    })
}

/// A drawn menu: its pixels, and where each entry ended up.
pub struct MenuFrame {
    pub pixmap: Pixmap,
    pub rows: Vec<MenuRow>,
}

impl std::fmt::Debug for MenuFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MenuFrame")
            .field("size", &(self.pixmap.width(), self.pixmap.height()))
            .field("rows", &self.rows.len())
            .finish()
    }
}

/// One entry, and the band of the menu it occupies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MenuRow {
    pub id: i32,
    pub top: f32,
    pub height: f32,
    pub enabled: bool,
}

/// Draws a tray item's menu, with `hovered` picked out.
///
/// Entries that cannot be chosen are drawn but not offered: a menu that hides
/// what is unavailable changes shape as you use it, and a menu that lies about
/// what it can do is worse than one that says "not now".
pub fn menu(
    config: &Config,
    text: &mut TextRenderer,
    entries: &[crate::tray::Entry],
    hovered: Option<i32>,
    scale: f32,
) -> Option<MenuFrame> {
    let style = &config.menu;
    if entries.is_empty() {
        return None;
    }

    let size = style.font_size.unwrap_or(config.font_size) * scale;
    text.set_size(size);
    let padding = style.padding as f32 * scale;
    let border = (style.border_width as f32 * scale).max(0.0);
    let row = (size * 1.6).ceil();
    let rule = (size * 0.6).ceil();

    let widest = entries
        .iter()
        .filter(|entry| !entry.separator)
        .map(|entry| text.width(&label_of(entry)))
        .fold(0.0_f32, f32::max);
    let w = (widest + (padding + border) * 2.0)
        .max(style.min_width as f32 * scale)
        .ceil();
    let h = (entries
        .iter()
        .map(|entry| if entry.separator { rule } else { row })
        .sum::<f32>()
        + (padding + border) * 2.0)
        .ceil()
        .max(1.0);

    let mut pixmap = Pixmap::new(w as u32, h as u32)?;
    let mut canvas = pixmap.as_mut();
    fill(&mut canvas, 0.0, 0.0, w, h, style.border);
    fill(
        &mut canvas,
        border,
        border,
        w - border * 2.0,
        h - border * 2.0,
        style.background,
    );

    let mut rows = Vec::new();
    let mut y = border + padding;
    for entry in entries {
        if entry.separator {
            // Centred in its band, which is what makes it read as a division
            // rather than as an underline of the entry above.
            let line = (scale.round()).max(1.0);
            fill(
                &mut canvas,
                border + padding,
                (y + rule / 2.0).round(),
                w - (border + padding) * 2.0,
                line,
                style.border,
            );
            y += rule;
            continue;
        }

        let picked = hovered == Some(entry.id) && entry.enabled;
        if picked {
            fill(
                &mut canvas,
                border,
                y,
                w - border * 2.0,
                row,
                style.highlight,
            );
        }
        let colour = match (entry.enabled, picked) {
            (false, _) => style.disabled,
            (true, true) => style.highlight_foreground,
            (true, false) => style.foreground,
        };
        text.draw(
            &mut canvas,
            &label_of(entry),
            border + padding,
            // Centred within its own row: `draw` centres in the height it is
            // given, so it is given the row rather than the whole menu.
            y * 2.0 + row,
            colour,
        );
        rows.push(MenuRow {
            id: entry.id,
            top: y,
            height: row,
            enabled: entry.enabled,
        });
        y += row;
    }

    Some(MenuFrame { pixmap, rows })
}

/// What an entry reads as, including its tick and its submenu arrow.
fn label_of(entry: &crate::tray::Entry) -> String {
    let tick = match entry.toggle {
        Some(true) => "* ",
        Some(false) => "  ",
        None => "",
    };
    let arrow = if entry.children.is_empty() { "" } else { "  >" };
    format!("{tick}{}{arrow}", entry.label)
}

/// The entry at `y` in a drawn menu, if the pointer is on one.
pub fn menu_at(rows: &[MenuRow], y: f32) -> Option<&MenuRow> {
    rows.iter()
        .find(|row| y >= row.top && y < row.top + row.height)
}

/// A background colour that is not quite the bar's, for a subtle fill.
pub fn shade(color: Color, amount: f32) -> Color {
    let c = color.rgba();
    Color(
        tiny_skia::Color::from_rgba(
            (c.red() + amount).clamp(0.0, 1.0),
            (c.green() + amount).clamp(0.0, 1.0),
            (c.blue() + amount).clamp(0.0, 1.0),
            c.alpha(),
        )
        .unwrap_or(c),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::Battery;

    struct Fixed;
    impl World for Fixed {
        fn format(&self, _: &str) -> String {
            "12:34".into()
        }
        fn battery(&self) -> Option<Battery> {
            Some(Battery {
                percent: 55.0,
                charging: false,
            })
        }
        fn volume(&self) -> Option<crate::audio::Volume> {
            None
        }
        fn backlight(&self) -> Option<f64> {
            None
        }
        fn network(&self) -> crate::module::Network {
            crate::module::Network::default()
        }
        fn command(&self, _: &ModuleConfig) -> Option<String> {
            None
        }
    }

    #[test]
    fn a_click_lands_on_the_segment_under_it() {
        let desktop = |n: u32, x: f32| Hit {
            x,
            width: 30.0,
            buttons: Buttons {
                left: Some(Click::Workspace(n)),
                right: Some(Click::Run("menu".into())),
                ..Default::default()
            },
            tooltip: None,
        };
        let hits = vec![desktop(1, 0.0), desktop(2, 30.0)];
        assert_eq!(hit(&hits, 15.0, Button::Left), Some(&Click::Workspace(1)));
        assert_eq!(
            hit(&hits, 30.0, Button::Left),
            Some(&Click::Workspace(2)),
            "boundaries belong to the right"
        );
        assert_eq!(hit(&hits, 200.0, Button::Left), None);
        // Each button is asked separately, so one with nothing on it does
        // nothing rather than borrowing the left button's meaning.
        assert_eq!(
            hit(&hits, 15.0, Button::Right),
            Some(&Click::Run("menu".into()))
        );
        assert_eq!(hit(&hits, 15.0, Button::Middle), None);
        // The index is what says two readings are the same module, so it has
        // to come back alongside the segment rather than being recomputed.
        assert_eq!(at(&hits, 15.0).map(|(i, _)| i), Some(0));
        assert_eq!(at(&hits, 45.0).map(|(i, _)| i), Some(1));
        assert!(at(&hits, 200.0).is_none());
    }

    fn clock_bar(format_alt: Option<&str>) -> Config {
        let mut config = Config {
            left: Vec::new(),
            center: Vec::new(),
            right: vec!["clock".into()],
            ..Default::default()
        };
        config.modules.insert(
            "clock".into(),
            ModuleConfig {
                kind: crate::config::Kind::Clock,
                format: "%H:%M".into(),
                format_alt: format_alt.map(str::to_owned),
                ..Default::default()
            },
        );
        config
    }

    #[test]
    fn a_module_with_a_second_format_offers_the_left_button_to_swap() {
        let config = clock_bar(Some("%Y-%m-%d"));
        let mut text = TextRenderer::new(&config.font, config.font_size);
        let mut icons = IconSet::new("Adwaita", None);
        let frame = draw(
            &config,
            &mut text,
            &mut icons,
            &Snapshot::default(),
            &Fixed,
            &Target {
                width: 800,
                scale: 1.0,
                alt: &|_| false,
            },
        );
        assert_eq!(
            frame.hits[0].buttons.left,
            Some(Click::Toggle("clock".into())),
            "the module is named, so the bar knows which one to swap"
        );

        // And with no second format there is nothing to swap to, so the button
        // stays free rather than toggling between one thing and itself.
        let plain = clock_bar(None);
        let frame = draw(
            &plain,
            &mut text,
            &mut icons,
            &Snapshot::default(),
            &Fixed,
            &Target {
                width: 800,
                scale: 1.0,
                alt: &|_| false,
            },
        );
        assert!(frame.hits.is_empty());
    }

    #[test]
    fn an_explicit_click_keeps_the_left_button_from_swapping() {
        // A button can only do one thing, and the one written down wins over
        // the one implied by there being a second format.
        let mut config = clock_bar(Some("%Y-%m-%d"));
        config.modules.get_mut("clock").unwrap().on_click = Some("calendar".into());
        let mut text = TextRenderer::new(&config.font, config.font_size);
        let mut icons = IconSet::new("Adwaita", None);
        let frame = draw(
            &config,
            &mut text,
            &mut icons,
            &Snapshot::default(),
            &Fixed,
            &Target {
                width: 800,
                scale: 1.0,
                alt: &|_| false,
            },
        );
        assert_eq!(
            frame.hits[0].buttons.left,
            Some(Click::Run("calendar".into()))
        );
    }

    #[test]
    fn a_tooltip_is_as_wide_as_its_widest_line_and_as_tall_as_it_needs() {
        let config = Config::default();
        let mut text = TextRenderer::new(&config.font, config.font_size);
        let one = tooltip(&config, &mut text, "a short line", 1.0).expect("one line");
        let two = tooltip(
            &config,
            &mut text,
            "a short line\na very much longer line than that one",
            1.0,
        )
        .expect("two lines");

        assert!(
            two.pixmap.width() > one.pixmap.width(),
            "the longer line set the width"
        );
        assert!(
            two.pixmap.height() > one.pixmap.height(),
            "two lines are taller than one"
        );
        // Nothing in a tooltip can be clicked; it is not there long enough.
        assert!(one.hits.is_empty());

        // Blank lines say nothing, and a tooltip of nothing but them is no
        // tooltip rather than an empty box.
        assert!(tooltip(&config, &mut text, "  \n \n", 1.0).is_none());
    }

    #[test]
    fn a_tooltip_is_drawn_at_the_display_scale_like_the_bar() {
        let config = Config::default();
        let mut text = TextRenderer::new(&config.font, config.font_size);
        let one = tooltip(&config, &mut text, "measured", 1.0).expect("a tooltip");
        let two = tooltip(&config, &mut text, "measured", 2.0).expect("a tooltip");
        let ratio = two.pixmap.width() as f32 / one.pixmap.width() as f32;
        assert!(
            (ratio - 2.0).abs() < 0.15,
            "at twice the scale it should take about twice the pixels, not {ratio}x"
        );
    }

    #[test]
    fn a_menu_is_as_wide_as_its_widest_entry_and_lists_what_can_be_chosen() {
        use crate::tray::Entry;

        let entry = |id, label: &str, enabled, separator| Entry {
            id,
            label: label.into(),
            enabled,
            separator,
            toggle: None,
            children: Vec::new(),
        };
        let entries = vec![
            entry(1, "Open", true, false),
            entry(2, "", true, true),
            entry(3, "A considerably longer entry", true, false),
            entry(4, "Not now", false, false),
        ];
        let config = Config::default();
        let mut text = TextRenderer::new(&config.font, config.font_size);
        let drawn = menu(&config, &mut text, &entries, None, 1.0).expect("a menu");

        // A separator is drawn but cannot be aimed at: it is a division, not a
        // thing to choose.
        assert_eq!(drawn.rows.len(), 3, "three choosable, one divider");
        assert!(drawn.rows.iter().all(|row| row.id != 2));
        assert!(
            !drawn.rows.iter().find(|row| row.id == 4).unwrap().enabled,
            "an entry that cannot be chosen is still listed, and says so"
        );

        // The rows are in order, and do not overlap.
        for pair in drawn.rows.windows(2) {
            assert!(pair[0].top + pair[0].height <= pair[1].top);
        }

        let narrow = vec![entry(1, "Ok", true, false)];
        let small = menu(&config, &mut text, &narrow, None, 1.0).expect("a menu");
        assert!(
            drawn.pixmap.width() > small.pixmap.width(),
            "the longest entry sets the width"
        );
        assert!(
            small.pixmap.width() >= config.menu.min_width as u32,
            "and a menu of short words is still wide enough to aim at"
        );

        // An empty menu is no menu rather than an empty box.
        assert!(menu(&config, &mut text, &[], None, 1.0).is_none());
    }

    #[test]
    fn a_click_in_a_menu_lands_on_the_entry_it_is_over() {
        let rows = vec![
            MenuRow {
                id: 7,
                top: 10.0,
                height: 20.0,
                enabled: true,
            },
            MenuRow {
                id: 8,
                top: 30.0,
                height: 20.0,
                enabled: false,
            },
        ];
        assert_eq!(menu_at(&rows, 15.0).map(|row| row.id), Some(7));
        assert_eq!(
            menu_at(&rows, 30.0).map(|row| row.id),
            Some(8),
            "boundaries belong to the row below"
        );
        // Above the first row and below the last is the menu's own padding,
        // which is part of the menu but not part of any entry.
        assert!(menu_at(&rows, 5.0).is_none());
        assert!(menu_at(&rows, 60.0).is_none());
    }

    #[test]
    fn a_bar_is_drawn_at_the_size_asked_for() {
        let config = Config::default();
        let mut text = TextRenderer::new(&config.font, config.font_size);
        let mut icons = IconSet::new("Adwaita", None);
        let frame = draw(
            &config,
            &mut text,
            &mut icons,
            &Snapshot::default(),
            &Fixed,
            &Target {
                width: 800,
                scale: 1.0,
                alt: &|_| false,
            },
        );
        assert_eq!(frame.pixmap.width(), 800);
        assert_eq!(frame.pixmap.height(), config.height as u32);
    }

    #[test]
    fn a_module_with_a_background_is_drawn_on_a_field_of_it() {
        // The only thing that checks the fill actually reaches the pixels: a
        // state colour resolved correctly and then never painted would pass
        // every other test in this crate.
        let mut config = Config {
            left: Vec::new(),
            center: Vec::new(),
            right: vec!["battery".into()],
            ..Default::default()
        };
        config.modules.insert(
            "battery".into(),
            ModuleConfig {
                kind: crate::config::Kind::Battery,
                format: "{capacity}%".into(),
                background: Some(crate::theme::color("#c65f5f")),
                ..Default::default()
            },
        );
        let mut text = TextRenderer::new(&config.font, config.font_size);
        let mut icons = IconSet::new("Adwaita", None);
        let frame = draw(
            &config,
            &mut text,
            &mut icons,
            &Snapshot::default(),
            &Fixed,
            &Target {
                width: 800,
                scale: 1.0,
                alt: &|_| false,
            },
        );

        let at = |x: u32, y: u32| {
            let p = frame.pixmap.pixel(x, y).unwrap().demultiply();
            (p.red(), p.green(), p.blue())
        };
        // The module sits at the right-hand end; the left of the bar is bare.
        assert_eq!(at(799, 1), (0xc6, 0x5f, 0x5f), "the field is painted");
        assert_ne!(at(0, 1), (0xc6, 0x5f, 0x5f), "and only under that module");
    }

    #[test]
    fn text_grows_with_the_display_scale_like_everything_else() {
        // Height, padding and icons are all multiplied by the scale where they
        // are used. Text is the one thing rasterized somewhere else, so it is
        // the one thing that can be left behind -- and a bar whose text alone
        // stayed the same number of pixels in a larger buffer looks like the
        // font shrank, which is exactly how this was found.
        let mut config = Config {
            left: Vec::new(),
            center: Vec::new(),
            right: vec!["clock".into()],
            ..Default::default()
        };
        config.modules.insert(
            "clock".into(),
            ModuleConfig {
                kind: crate::config::Kind::Clock,
                format: "%H:%M".into(),
                // Only a clickable segment is measured into the frame's hits,
                // which is the only place a drawn width can be read back from.
                on_click: Some("true".into()),
                ..Default::default()
            },
        );
        let mut text = TextRenderer::new(&config.font, config.font_size);
        let mut icons = IconSet::new("Adwaita", None);
        let mut width_at = |scale: f32| {
            let frame = draw(
                &config,
                &mut text,
                &mut icons,
                &Snapshot::default(),
                &Fixed,
                &Target {
                    width: (800.0 * scale) as u32,
                    scale,
                    alt: &|_| false,
                },
            );
            frame.hits.first().expect("the clock is clickable").width
        };

        let one = width_at(1.0);
        let two = width_at(2.0);
        assert!(
            (two - one * 2.0).abs() < one * 0.15,
            "at twice the scale the clock should take about twice the pixels, \
             not {two} against {one}"
        );
    }

    #[test]
    fn a_scaled_bar_is_drawn_at_the_resolution_it_will_be_shown_at() {
        // Drawing at logical size and letting the compositor magnify it is what
        // makes text soft on a scaled display.
        let config = Config::default();
        let mut text = TextRenderer::new(&config.font, config.font_size);
        let mut icons = IconSet::new("Adwaita", None);
        let frame = draw(
            &config,
            &mut text,
            &mut icons,
            &Snapshot::default(),
            &Fixed,
            &Target {
                width: 1600,
                scale: 2.0,
                alt: &|_| false,
            },
        );
        assert_eq!(frame.pixmap.height(), config.height as u32 * 2);
    }
}
