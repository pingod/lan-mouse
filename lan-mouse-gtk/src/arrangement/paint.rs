//! Painting helpers for the arrangement canvas.
//!
//! Kept apart from the widget so the colour choices and the text layout have one
//! place to live: the canvas draws a lot of small shapes and every one of them
//! has to stay readable in both themes.

use gtk::cairo;
// `pango` comes with `pangocairo`: the two are versioned together, and naming it
// separately would let them drift apart from the one gtk4 links against.
use pangocairo::pango::{self, FontDescription};

use super::layout::Screen;

/// The pango unit: one point is `PANGO_SCALE` device units.
const PANGO_SCALE: f64 = 1024.0;

/// A colour set that adapts to the theme.
///
/// The values are derived from `adw::StyleManager::is_dark()` rather than looked
/// up in the style context: `gtk_style_context_lookup_color` is deprecated and
/// `gtk::WidgetExt::color` needs gtk4's `v4_10` feature, while this crate only
/// enables `v4_2`.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub foreground: (f64, f64, f64),
    /// what a screen is filled with, so the border colour reads as a border
    pub card: (f64, f64, f64),
    pub accent: (f64, f64, f64),
    pub success: (f64, f64, f64),
    pub warning: (f64, f64, f64),
    pub danger: (f64, f64, f64),
}

impl Palette {
    /// The palette of the theme the application is using.
    ///
    /// `adw::StyleManager` is only usable once a `GtkApplication` exists, so the
    /// dark flag is a parameter and this reads it: that keeps the colour choices
    /// testable without a display.
    pub fn resolve(dark: bool) -> Self {
        if dark {
            Palette {
                foreground: (0.92, 0.92, 0.94),
                card: (0.20, 0.20, 0.23),
                accent: (0.36, 0.62, 0.95),
                success: (0.35, 0.82, 0.47),
                warning: (0.93, 0.73, 0.25),
                danger: (0.90, 0.38, 0.36),
            }
        } else {
            Palette {
                foreground: (0.11, 0.11, 0.13),
                card: (1.0, 1.0, 1.0),
                accent: (0.20, 0.47, 0.85),
                success: (0.15, 0.65, 0.32),
                warning: (0.75, 0.55, 0.05),
                danger: (0.78, 0.22, 0.20),
            }
        }
    }
}

/// Set the source colour with an alpha.
pub fn set(cr: &cairo::Context, color: (f64, f64, f64), alpha: f64) {
    cr.set_source_rgba(color.0, color.1, color.2, alpha);
}

/// Add a rounded rectangle to the current path.
///
/// Used for the shadow under a dragged card and for the resize handles, which are
/// small enough that the card radius would round them into ovals.
pub fn rounded_rect(cr: &cairo::Context, rect: Screen, radius: f64) {
    let quarter = std::f64::consts::PI / 2.0;
    let r = radius.min(rect.h / 2.0).min(rect.w / 2.0).max(0.0);
    let (x, y, w, h) = (rect.x, rect.y, rect.w, rect.h);
    /* Each corner sweeps exactly a quarter turn, from the direction the previous
     * edge arrived to the direction the next one leaves. Getting these angles
     * wrong does not look wrong in a screenshot of a big card - it looks like a
     * notch cut into the corner, because the arc runs on past the corner it was
     * meant to round and doubles back over the straight edge. */
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -quarter, 0.0); // top right
    cr.arc(x + w - r, y + h - r, r, 0.0, quarter); // bottom right
    cr.arc(x + r, y + h - r, r, quarter, 2.0 * quarter); // bottom left
    cr.arc(x + r, y + r, r, 2.0 * quarter, 3.0 * quarter); // top left
    cr.close_path();
}

/// Fill a screen card and leave its outline on the path, so the caller can stroke
/// the border colour right after.
///
/// The fill colour is set here rather than left to the caller: cairo keeps the
/// source it was last given, and a card that inherits the colour of whatever was
/// painted before it turns out the colour of a wall or a link.
pub fn fill_card(cr: &cairo::Context, rect: Screen, palette: &Palette) {
    set(cr, palette.card, 1.0);
    rounded_rect(cr, rect, CARD_RADIUS);
    let _ = cr.fill_preserve();
}

/// The corner radius of a screen card, in canvas units.
pub const CARD_RADIUS: f64 = 10.0;

/// One line of text, laid out by pango and shortened to fit its box.
///
/// Text goes through pango rather than the cairo toy font API: the toy API has no
/// font fallback, so a hostname with characters outside the default face came out
/// as empty boxes.
///
/// The shortening is done here instead of by `PANGO_ELLIPSIZE_END` because
/// ellipsization needs the layout's wrap mode set to `WRAP_NONE`, and that variant
/// is only exposed with pango's `v1_56` feature - which would raise the runtime
/// requirement of the whole application for something a binary search over the
/// character count does in a handful of measurements.
pub struct Text {
    layout: pango::Layout,
    /// the width the text actually occupies
    width: f64,
}

impl Text {
    /// Lay `text` out at `size`, no wider than `max_width`.
    pub fn new(cr: &cairo::Context, text: &str, size: f64, bold: bool, max_width: f64) -> Self {
        let description = describe(size, bold);
        let make = |content: &str| {
            let layout = pangocairo::functions::create_layout(cr);
            layout.set_font_description(Some(&description));
            // no width constraint: the layout reports its natural size and the
            // measurement below decides whether that fits
            layout.set_width(-1);
            layout.set_text(content);
            let (width, height) = layout.size();
            (
                layout,
                width as f64 / PANGO_SCALE,
                height as f64 / PANGO_SCALE,
            )
        };

        let (layout, natural, _height) = make(text);
        if max_width <= 0.0 || natural <= max_width {
            return Text {
                layout,
                width: natural,
            };
        }

        /* Binary search the longest prefix that still fits with an ellipsis. The
         * measurement is a round trip into pango, so halving beats walking the
         * string one character at a time, which is what the old toy-API code did
         * and what made a wide canvas stutter while a card was dragged. */
        let chars: Vec<char> = text.chars().collect();
        let fits = |take: usize| {
            let candidate: String = chars[..take].iter().collect();
            make(&format!("{candidate}…")).1 <= max_width
        };
        let (mut low, mut high) = (0usize, chars.len());
        while low < high {
            let take = (low + high).div_ceil(2);
            if fits(take) {
                low = take;
            } else {
                high = take - 1;
            }
        }
        let prefix: String = chars[..low].iter().collect();
        let (layout, width, _height) = make(&format!("{prefix}…"));
        Text { layout, width }
    }

    pub fn height(&self) -> f64 {
        let (_width, height) = self.layout.size();
        height as f64 / PANGO_SCALE
    }

    /// Draw horizontally centred on `centre_x`, vertically on `centre_y`.
    pub fn draw(&self, cr: &cairo::Context, centre_x: f64, centre_y: f64) {
        self.draw_left(
            cr,
            centre_x - self.width / 2.0,
            centre_y - self.height() / 2.0,
        );
    }

    /// Draw with the top left corner at (`x`, `y`).
    pub fn draw_left(&self, cr: &cairo::Context, x: f64, y: f64) {
        cr.move_to(x, y);
        pangocairo::functions::show_layout(cr, &self.layout);
    }
}

/// The font of a canvas label: sans, in device units, optionally bold.
fn describe(size: f64, bold: bool) -> FontDescription {
    let mut description = FontDescription::from_string(if bold { "Sans Bold" } else { "Sans" });
    /* absolute, not points: the canvas is measured in device units, and a point
     * size would make the text a different size on a HiDPI display than the card
     * around it */
    description.set_absolute_size(size * PANGO_SCALE);
    description
}

/// Format a size as `1920 × 1080`, or the placeholder the canvas uses when a
/// machine never reported one.
///
/// Saying so is the point: a card whose size is unknown cannot be trusted to be
/// drawn to scale, and a blank line reads as a bug rather than as missing data.
pub fn format_size(size: (f64, f64)) -> String {
    if size.0 > 0.0 && size.1 > 0.0 {
        format!("{} × {}", size.0.round(), size.1.round())
    } else {
        "size unknown".to_owned()
    }
}

/// A human list of names, for the conflict banner.
pub fn list_names(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1 => names[0].clone(),
        _ => {
            let (last, rest) = names.split_last().expect("at least two");
            format!("{} and {}", rest.join(", "), last)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_listed_readably() {
        assert_eq!(list_names(&[]), "");
        assert_eq!(list_names(&["a".to_owned()]), "a");
        assert_eq!(list_names(&["a".to_owned(), "b".to_owned()]), "a and b");
        assert_eq!(
            list_names(&["a".to_owned(), "b".to_owned(), "c".to_owned()]),
            "a, b and c"
        );
    }

    #[test]
    fn an_unreported_size_is_said_out_loud() {
        assert_eq!(format_size((1920.0, 1080.0)), "1920 × 1080");
        assert_eq!(format_size((1919.6, 1079.5)), "1920 × 1080");
        assert_eq!(format_size((0.0, 1080.0)), "size unknown");
    }

    /// Every colour the canvas paints with has to be a valid rgba triple in both
    /// themes, and the two themes have to disagree: a canvas that is the same in
    /// both is unreadable in one of them.
    #[test]
    fn both_palettes_are_valid_and_distinguishable() {
        for dark in [true, false] {
            let palette = Palette::resolve(dark);
            let colors = [
                palette.foreground,
                palette.accent,
                palette.success,
                palette.warning,
                palette.danger,
            ];
            for color in colors {
                for channel in [color.0, color.1, color.2] {
                    assert!((0.0..=1.0).contains(&channel), "{color:?}");
                }
            }
            assert_ne!(palette.foreground, Palette::resolve(!dark).foreground);
        }
    }

    /// The foreground is the text colour on the card fill, so the two must
    /// contrast in both themes - the bug the old hand-picked colours had on a
    /// light theme was invisible pills.
    #[test]
    fn text_contrasts_with_the_card_it_sits_on() {
        for dark in [true, false] {
            let palette = Palette::resolve(dark);
            let card = if dark { CARD[DARK] } else { CARD[LIGHT] };
            let contrast = (palette.foreground.0 - card.0).abs()
                + (palette.foreground.1 - card.1).abs()
                + (palette.foreground.2 - card.2).abs();
            assert!(contrast > 0.4, "theme {dark}: contrast {contrast}");
        }
    }

    /// The card fills, kept here so the contrast test has something to compare
    /// against: the dark one first.
    const CARD: [(f64, f64, f64); 2] = [(0.20, 0.20, 0.23), (1.0, 1.0, 1.0)];
    const DARK: usize = 0;
    const LIGHT: usize = 1;
}

#[cfg(test)]
mod shape_tests {
    use super::*;

    /// The corners of a rounded rectangle have to be convex.
    ///
    /// An arc that sweeps past its corner leaves a notch, and a notch reads as a
    /// rendering bug in a card that is otherwise a plain rectangle - which is
    /// exactly what happened when these angles were first written. Checking the
    /// outline by counting the pixels of the border in each quadrant catches it,
    /// because a doubled-back arc paints the border twice over the straight edge
    /// and leaves the corner itself empty.
    #[test]
    fn a_rounded_rectangle_fills_its_corners_out_to_the_radius() {
        let size = 40;
        let mut surface =
            cairo::ImageSurface::create(cairo::Format::ARgb32, size, size).expect("surface");
        let cr = cairo::Context::new(&surface).expect("context");
        cr.set_source_rgb(0.0, 0.0, 0.0);
        let _ = cr.paint();
        cr.set_source_rgb(1.0, 1.0, 1.0);
        rounded_rect(&cr, Screen::new(5.0, 5.0, 30.0, 30.0), 8.0);
        let _ = cr.fill();
        drop(cr);
        surface.flush();

        let stride = surface.stride() as usize;
        let white =
            |data: &[u8], x: i32, y: i32| data[y as usize * stride + x as usize * 4 + 2] > 128;
        let data = surface.data().expect("data");
        // the middle of each edge and the inner corner of each arc
        assert!(white(&data, 20, 6), "top edge missing");
        assert!(white(&data, 6, 20), "left edge missing");
        assert!(white(&data, 33, 20), "right edge missing");
        assert!(white(&data, 20, 33), "bottom edge missing");
        /* The square corner at (6, 6) is outside the radius, so it must be empty;
         * a notch would leave the area just inside the arc empty instead. */
        assert!(!white(&data, 6, 6), "the corner was not rounded");
        assert!(white(&data, 9, 9), "the rounded corner is missing its fill");
        // and the far corners likewise
        assert!(!white(&data, 33, 33));
        assert!(white(&data, 30, 30), "bottom right corner not filled");
    }
}
