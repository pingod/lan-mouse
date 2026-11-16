//! The physical screens of a machine, as geometry.
//!
//! A machine is not one rectangle: a desk has two monitors side by side, one of
//! them turned on its side, and the pointer crosses between them before it ever
//! leaves the machine. Flattening them into one bounding box - which is what this
//! code did before - makes an edge range mean "the stretch of the bounding box",
//! and a bounding box with a rotated monitor in it contains area no screen covers.
//!
//! So the layout is a list of rectangles in the compositor's own coordinate space,
//! with the rotation applied: a 2560x1440 monitor turned 270 degrees occupies
//! 1440x2560 at its offset, which is what the compositor and the pointer agree on.
//!
//! Everything is integers in pixels, because that is what every backend reports and
//! what a fraction of an edge has to be resolved against.

use std::fmt;

/// Which side of the layout a crossing happened on.
///
/// A local enum rather than `lan_mouse_ipc::Position`: this crate is the bottom of
/// the dependency graph, and the four sides are the only thing the geometry needs
/// to know about a crossing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    /// Whether the edge runs vertically, so a crossing is measured along `y`.
    pub fn is_vertical(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
}

/// How a monitor is turned, as the compositor reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rotation {
    #[default]
    Normal,
    /// 90 degrees clockwise.
    Left,
    /// 180 degrees.
    UpsideDown,
    /// 270 degrees clockwise (90 counter-clockwise).
    Right,
}

impl Rotation {
    /// Whether the monitor is taller than it is wide on screen.
    pub fn swaps_axes(self) -> bool {
        matches!(self, Rotation::Left | Rotation::Right)
    }

    /// The spelling used on the wire and in the config file.
    pub fn as_str(self) -> &'static str {
        match self {
            Rotation::Normal => "normal",
            Rotation::Left => "left",
            Rotation::UpsideDown => "upside-down",
            Rotation::Right => "right",
        }
    }

    /// Parse the spelling used on the wire.
    ///
    /// Unknown spellings become `Normal` rather than an error: a peer on a newer
    /// build may describe a rotation this one has never heard of, and drawing the
    /// screen unrotated is better than dropping it from the layout.
    pub fn parse(value: &str) -> Self {
        match value {
            "left" => Rotation::Left,
            "upside-down" => Rotation::UpsideDown,
            "right" => Rotation::Right,
            _ => Rotation::Normal,
        }
    }

    /// The rotation a compositor reports as a number of degrees clockwise.
    pub fn from_degrees(degrees: u16) -> Self {
        match degrees % 360 {
            90 => Rotation::Left,
            180 => Rotation::UpsideDown,
            270 => Rotation::Right,
            _ => Rotation::Normal,
        }
    }
}

/// One monitor: where it is, how big it is, and how it is turned.
///
/// `width` and `height` are the panel's own dimensions; [`DisplayInfo::on_screen`]
/// applies the rotation, which is the rectangle the compositor lays out and the
/// pointer moves in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
    pub name: String,
    /// left edge of the monitor in the compositor's coordinate space
    pub x: i32,
    /// top edge
    pub y: i32,
    /// panel width, before rotation
    pub width: u32,
    /// panel height, before rotation
    pub height: u32,
    pub rotation: Rotation,
    /// whether the compositor has this monitor enabled
    pub enabled: bool,
}

impl DisplayInfo {
    pub fn new(
        name: impl Into<String>,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        rotation: Rotation,
    ) -> Self {
        Self {
            name: name.into(),
            x,
            y,
            width,
            height,
            rotation,
            enabled: true,
        }
    }

    /// The rectangle the monitor occupies on screen, rotation applied.
    pub fn on_screen(&self) -> ScreenRect {
        if self.rotation.swaps_axes() {
            ScreenRect {
                x: self.x,
                y: self.y,
                width: self.height,
                height: self.width,
            }
        } else {
            ScreenRect {
                x: self.x,
                y: self.y,
                width: self.width,
                height: self.height,
            }
        }
    }
}

/// A rectangle in screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl ScreenRect {
    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }
}

/// Every monitor of a machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub displays: Vec<DisplayInfo>,
}

impl Layout {
    pub fn new(displays: Vec<DisplayInfo>) -> Self {
        Self { displays }
    }

    /// The monitors that are actually on.
    pub fn enabled(&self) -> impl Iterator<Item = &DisplayInfo> {
        self.displays.iter().filter(|display| display.enabled)
    }

    /// The bounding box of the enabled monitors.
    ///
    /// This is the area the pointer can reach, so it is what a "the whole screen"
    /// edge range means and what the barriers are built over. It is *not* the
    /// layout: the canvas draws the individual monitors, and the bounding box is
    /// only used where one rectangle is genuinely needed.
    pub fn bounds(&self) -> Option<ScreenRect> {
        let mut displays = self.enabled();
        let first = displays.next()?.on_screen();
        let mut x0 = first.x;
        let mut y0 = first.y;
        let mut x1 = first.right();
        let mut y1 = first.bottom();
        for rect in displays.map(DisplayInfo::on_screen) {
            x0 = x0.min(rect.x);
            y0 = y0.min(rect.y);
            x1 = x1.max(rect.right());
            y1 = y1.max(rect.bottom());
        }
        Some(ScreenRect {
            x: x0,
            y: y0,
            width: (x1 - x0).max(0) as u32,
            height: (y1 - y0).max(0) as u32,
        })
    }

    /// The size of the bounding box, for the places that want one size.
    pub fn size(&self) -> Option<(u32, u32)> {
        self.bounds().map(|bounds| (bounds.width, bounds.height))
    }

    pub fn is_empty(&self) -> bool {
        self.displays.is_empty()
    }

    /// Serialize for the `PeerInfo` display slot.
    ///
    /// One monitor per entry, `name:x,y:widthxheight:rotation:enabled`, entries
    /// separated by `;`. Compact because the slot is 32 bytes and a two-monitor
    /// desk has to fit in the *chunked* report rather than in one slot - the
    /// display field spans several slots (see `lan-mouse-proto`'s
    /// `info_slot::DISPLAYS` and `DISPLAY_EXTRA`).
    ///
    /// A monitor with a name containing `;` or `:` has those replaced: the format
    /// is line-oriented and a name is not worth escaping.
    pub fn encode(&self) -> String {
        self.displays
            .iter()
            .map(|display| {
                let name = display.name.replace([';', ':'], "_");
                format!(
                    "{}:{},{}:{}x{}:{}:{}",
                    name,
                    display.x,
                    display.y,
                    display.width,
                    display.height,
                    display.rotation.as_str(),
                    u8::from(display.enabled),
                )
            })
            .collect::<Vec<_>>()
            .join(";")
    }

    /// Parse what [`Layout::encode`] wrote.
    ///
    /// Returns `None` for anything it cannot make sense of as a whole, so a
    /// caller can fall back to the size-only report an older peer sends. A single
    /// malformed entry is skipped rather than failing the list: one unparseable
    /// monitor should not hide the others.
    pub fn decode(report: &str) -> Option<Self> {
        let mut displays = Vec::new();
        for entry in report.split(';').filter(|entry| !entry.is_empty()) {
            let mut parts = entry.split(':');
            let (Some(name), Some(offset), Some(size)) = (parts.next(), parts.next(), parts.next())
            else {
                // a truncated entry: skip it, the others are still usable
                continue;
            };
            let rotation = parts.next().unwrap_or("normal");
            let enabled = parts.next().unwrap_or("1");

            let (Some((x, y)), Some((width, height))) =
                (offset.split_once(','), size.split_once('x'))
            else {
                continue;
            };
            let Ok(x) = x.parse::<i32>() else { continue };
            let Ok(y) = y.parse::<i32>() else { continue };
            let Ok(width) = width.parse::<u32>() else {
                continue;
            };
            let Ok(height) = height.parse::<u32>() else {
                continue;
            };
            if width == 0 || height == 0 {
                continue;
            }
            displays.push(DisplayInfo {
                name: name.to_owned(),
                x,
                y,
                width,
                height,
                rotation: Rotation::parse(rotation),
                enabled: enabled != "0",
            });
        }
        if displays.is_empty() {
            return None;
        }
        Some(Self { displays })
    }

    /// The monitor whose rectangle contains a point, if any.
    pub fn at(&self, x: i32, y: i32) -> Option<&DisplayInfo> {
        self.enabled().find(|display| {
            let rect = display.on_screen();
            x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
        })
    }

    /// Which monitor an edge crossing belongs to, and where on that monitor.
    ///
    /// `fraction` is the crossing point along the *bounding box*'s edge, which is
    /// what the barrier reports. The monitor that owns that part of the bounding
    /// box is found by walking the monitors in order; a crossing in a gap between
    /// two monitors (a bounding box with a rotated monitor in it has them) belongs
    /// to the monitor whose edge is nearest.
    ///
    /// Returns the monitor and the fraction along *its* edge, so a range that
    /// covers the whole bounding box can be mapped onto the monitor the pointer is
    /// actually at.
    pub fn display_at_edge(&self, side: Side, fraction: f64) -> Option<(&DisplayInfo, f64)> {
        let bounds = self.bounds()?;
        let vertical = side.is_vertical();
        /* the coordinate along the edge, in screen space */
        let along = if vertical {
            bounds.y as f64 + fraction.clamp(0.0, 1.0) * bounds.height as f64
        } else {
            bounds.x as f64 + fraction.clamp(0.0, 1.0) * bounds.width as f64
        };
        /* the monitors on the same side of the bounding box, in the order the
         * pointer would meet them */
        let mut candidates = self
            .enabled()
            .filter(|display| {
                let rect = display.on_screen();
                match side {
                    Side::Left => rect.x == bounds.x,
                    Side::Right => rect.right() == bounds.right(),
                    Side::Top => rect.y == bounds.y,
                    Side::Bottom => rect.bottom() == bounds.bottom(),
                }
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            /* nothing touches that side: a gap in an L-shaped desk. Use the
             * monitor whose edge is nearest to the crossing point. */
            candidates = self.enabled().collect();
        }
        candidates.sort_by_key(|display| {
            let rect = display.on_screen();
            if vertical { rect.y } else { rect.x }
        });
        let display = candidates
            .iter()
            .min_by(|a, b| {
                let a = a.on_screen();
                let b = b.on_screen();
                let distance = |rect: ScreenRect| {
                    let (start, stop) = if vertical {
                        (rect.y as f64, rect.bottom() as f64)
                    } else {
                        (rect.x as f64, rect.right() as f64)
                    };
                    if along < start {
                        start - along
                    } else if along > stop {
                        along - stop
                    } else {
                        0.0
                    }
                };
                distance(a).total_cmp(&distance(b))
            })
            .cloned()?;
        let rect = display.on_screen();
        let (start, len) = if vertical {
            (rect.y as f64, rect.height as f64)
        } else {
            (rect.x as f64, rect.width as f64)
        };
        let local = if len <= 0.0 {
            0.5
        } else {
            ((along - start) / len).clamp(0.0, 1.0)
        };
        Some((display, local))
    }

    /// A short human description, for the peer details row.
    pub fn describe(&self) -> String {
        if self.displays.is_empty() {
            return String::new();
        }
        let monitors = self.enabled().count();
        match self.size() {
            Some((width, height)) => {
                format!("{monitors} display(s), {width}x{height}")
            }
            None => format!("{monitors} display(s)"),
        }
    }
}

impl fmt::Display for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The desk this was written for: a 2560x1440 panel turned 270 degrees at
    /// (2560, 0), and a 2560x1440 panel upright at (0, 1120).
    fn two_monitors() -> Layout {
        Layout::new(vec![
            DisplayInfo {
                name: "DP-2".to_owned(),
                x: 2560,
                y: 0,
                width: 2560,
                height: 1440,
                rotation: Rotation::Right,
                enabled: true,
            },
            DisplayInfo {
                name: "DP-3".to_owned(),
                x: 0,
                y: 1120,
                width: 2560,
                height: 1440,
                rotation: Rotation::Normal,
                enabled: true,
            },
        ])
    }

    #[test]
    fn a_rotated_monitor_occupies_the_rectangle_the_compositor_lays_out() {
        let layout = two_monitors();
        let rect = layout.displays[0].on_screen();
        // 2560x1440 turned 270 degrees is 1440 wide and 2560 tall
        assert_eq!((rect.width, rect.height), (1440, 2560));
        assert_eq!((rect.x, rect.y), (2560, 0));
    }

    #[test]
    fn the_bounds_cover_both_monitors_and_are_not_their_sum() {
        /* The bounding box is 4000x2560, not 5120x2560: the rotated monitor is
         * narrower than the upright one, and they overlap vertically. Getting this
         * wrong is what made the canvas draw a screen nobody has. */
        assert_eq!(two_monitors().size(), Some((4000, 2560)));
    }

    #[test]
    fn the_layout_round_trips_through_the_wire_format() {
        let layout = two_monitors();
        let encoded = layout.encode();
        let decoded = Layout::decode(&encoded).expect("decodes");
        assert_eq!(decoded, layout);
        // and the rotation survives, which is the part that is easy to lose
        assert_eq!(decoded.displays[0].rotation, Rotation::Right);
    }

    #[test]
    fn a_name_with_the_separators_does_not_corrupt_the_report() {
        let mut layout = two_monitors();
        layout.displays[0].name = "DP:2;left".to_owned();
        let decoded = Layout::decode(&layout.encode()).expect("decodes");
        assert_eq!(decoded.displays.len(), 2);
        assert_eq!(decoded.displays[0].name, "DP_2_left");
        // the geometry still survives
        assert_eq!(decoded.displays[0].x, 2560);
    }

    #[test]
    fn an_unparseable_entry_is_skipped_not_fatal() {
        let report = "DP-1:0,0:1920x1080:normal:1;garbage;DP-2:1920,0:1920x1080:normal:1";
        let layout = Layout::decode(report).expect("the good entries survive");
        assert_eq!(layout.displays.len(), 2);
        assert_eq!(layout.displays[1].name, "DP-2");
    }

    #[test]
    fn an_empty_or_zero_sized_report_is_none() {
        assert_eq!(Layout::decode(""), None);
        assert_eq!(Layout::decode("DP-1:0,0:0x0:normal:1"), None);
        // a report from an older peer, which only had a size
        assert_eq!(Layout::decode("2560x1440"), None);
    }

    #[test]
    fn a_point_is_attributed_to_the_monitor_it_is_on() {
        let layout = two_monitors();
        assert_eq!(layout.at(100, 1200).map(|d| d.name.as_str()), Some("DP-3"));
        assert_eq!(layout.at(3000, 100).map(|d| d.name.as_str()), Some("DP-2"));
        // the gap the rotated monitor leaves: nothing is there
        assert_eq!(layout.at(100, 100), None);
    }

    #[test]
    fn a_crossing_is_attributed_to_the_monitor_that_touches_that_side() {
        let layout = two_monitors();
        /* The right edge of the bounding box is the rotated monitor's right edge
         * (4000), and it spans y 0..2560 - the whole height. So every crossing on
         * the right edge is on DP-2. */
        for fraction in [0.0, 0.5, 1.0] {
            let (display, _) = layout
                .display_at_edge(Side::Right, fraction)
                .expect("a monitor");
            assert_eq!(display.name, "DP-2", "at {fraction}");
        }
        /* The bottom edge is DP-3's bottom (y 2560) on x 0..2560 and DP-2's bottom
         * on x 2560..4000; a crossing at 10% of 4000 px is on DP-3. */
        let (display, local) = layout
            .display_at_edge(Side::Bottom, 0.1)
            .expect("a monitor");
        assert_eq!(display.name, "DP-3");
        assert!((local - 400.0 / 2560.0).abs() < 0.01, "{local}");
    }

    #[test]
    fn the_local_fraction_is_measured_on_that_monitor_not_the_bounding_box() {
        let layout = two_monitors();
        /* The left edge of the bounding box (x 0) is DP-3, which starts at y 1120
         * and is 1440 tall. A crossing at 50% of the 2560 tall bounding box is at
         * y 1280, which is (1280-1120)/1440 = 11% of DP-3's edge. */
        let (display, local) = layout.display_at_edge(Side::Left, 0.5).expect("a monitor");
        assert_eq!(display.name, "DP-3");
        assert!((local - 160.0 / 1440.0).abs() < 0.01, "{local}");
    }

    #[test]
    fn a_disabled_monitor_is_neither_bounds_nor_target() {
        let mut layout = two_monitors();
        layout.displays[0].enabled = false;
        assert_eq!(layout.size(), Some((2560, 1440)));
        assert_eq!(layout.at(3000, 100), None);
        // and the remaining monitor is still addressable
        assert_eq!(layout.at(100, 1200).map(|d| d.name.as_str()), Some("DP-3"));
    }

    #[test]
    fn the_description_names_the_count_and_the_bounding_box() {
        assert_eq!(two_monitors().describe(), "2 display(s), 4000x2560");
        assert_eq!(Layout::default().describe(), "");
    }

    #[test]
    fn a_single_unrotated_monitor_is_just_its_size() {
        let layout = Layout::new(vec![DisplayInfo::new(
            "eDP-1",
            0,
            0,
            1920,
            1080,
            Rotation::Normal,
        )]);
        assert_eq!(layout.size(), Some((1920, 1080)));
        let (display, local) = layout.display_at_edge(Side::Right, 0.25).expect("monitor");
        assert_eq!(display.name, "eDP-1");
        assert!((local - 0.25).abs() < 1e-9);
    }

    #[test]
    fn rotations_round_trip_through_their_names() {
        for rotation in [
            Rotation::Normal,
            Rotation::Left,
            Rotation::UpsideDown,
            Rotation::Right,
        ] {
            assert_eq!(Rotation::parse(rotation.as_str()), rotation);
        }
        // a spelling from a newer build is drawn unrotated, not dropped
        assert_eq!(Rotation::parse("sideways"), Rotation::Normal);
        assert_eq!(Rotation::from_degrees(270), Rotation::Right);
        assert_eq!(Rotation::from_degrees(0), Rotation::Normal);
    }
}
