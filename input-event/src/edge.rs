//! The segment of a screen edge a machine can be reached through.
//!
//! lan-mouse used to give every client exactly one edge: the first client in
//! the list that owned an edge took the pointer and the others on that edge
//! were unreachable, because a barrier spans the whole edge. Sharing an edge
//! properly needs what Synergy's `links` section has always had - a range on
//! the edge, so `right(0-60%)` belongs to one machine and `right(60-100%)` to
//! another ([`Config::readSectionLinks`] in synergy, `Interval = pair<float,float>`).
//!
//! [`EdgeRange`] is that range. It is stored as **permille** (integer
//! thousandths of the edge, 0..=1000) but serialized as two floats in
//! `0.0..=1.0`, which is what a human writes in a config file and what the
//! crossing fraction on the wire already uses. Integers keep the type `Eq` +
//! `Ord` (the client configuration and the IPC messages derive both, and a
//! `f64` would break that) and make the TOML round trip exact.
//!
//! [`Config::readSectionLinks`]: https://github.com/input-leap/input-leap/blob/master/src/lib/server/Config.cpp

use std::fmt::{self, Display};
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// Number of [`EdgeRange`] steps that make up a whole edge.
pub const EDGE_PERMILLE: u16 = 1000;

/// A non-empty, half-open segment of a screen edge.
///
/// `start` is included, `end` is not, so two neighbours are written `(0, 500)`
/// and `(500, 1000)` without overlapping or leaving a gap - the same convention
/// as Synergy's `CellEdge::isInside`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct EdgeRange {
    /// first permille that belongs to this range, `0..=999`
    start: u16,
    /// first permille *after* this range, `1..=1000`
    end: u16,
}

/// A range could not be built.
///
/// Not `Eq`: the variants carry the requested fractions, and `f64` is only
/// `PartialEq`.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum EdgeRangeError {
    #[error("edge range `{start}..{end}` is outside 0.0..=1.0")]
    OutOfRange { start: f64, end: f64 },
    #[error("edge range `{start}..{end}` is empty: start must be below end")]
    Empty { start: f64, end: f64 },
    #[error("an edge range is two numbers `[start, end]`")]
    Malformed,
}

/// Serialize as `[start, end]` in `0.0..=1.0`.
#[derive(Serialize, Deserialize)]
struct AsFractions([f64; 2]);

impl EdgeRange {
    /// The whole edge: the value every client had before ranges existed.
    pub const FULL: EdgeRange = EdgeRange {
        start: 0,
        end: EDGE_PERMILLE,
    };

    /// Build from permille values, rejecting an empty or out of bounds range.
    pub fn new(start: u16, end: u16) -> Option<EdgeRange> {
        if end <= EDGE_PERMILLE && start < end {
            Some(EdgeRange { start, end })
        } else {
            None
        }
    }

    /// Build from two fractions in `0.0..=1.0`.
    ///
    /// Values are rounded to permille, so `0.0004` becomes `0` rather than
    /// silently producing a range that is one step narrower than requested.
    /// Rounding can bring two distinct fractions together, which is an empty
    /// range rather than a valid one: `0.0004, 0.0006` asks for less than one
    /// permille and cannot be honoured.
    pub fn from_fractions(start: f64, end: f64) -> Result<EdgeRange, EdgeRangeError> {
        if !(start.is_finite() && end.is_finite()) || start < 0.0 || end > 1.0 {
            return Err(EdgeRangeError::OutOfRange { start, end });
        }
        if start >= end {
            return Err(EdgeRangeError::Empty { start, end });
        }
        let low = (start * EDGE_PERMILLE as f64).round() as u16;
        let high = (end * EDGE_PERMILLE as f64).round() as u16;
        EdgeRange::new(low, high).ok_or(EdgeRangeError::Empty { start, end })
    }

    /// The range as `[start, end]` fractions of the edge.
    pub fn fractions(self) -> [f64; 2] {
        [
            self.start as f64 / EDGE_PERMILLE as f64,
            self.end as f64 / EDGE_PERMILLE as f64,
        ]
    }

    /// Pull a range that is a hair short of an end onto that end.
    ///
    /// A card dragged to cover a whole screen lands a pixel short as often as not,
    /// and a fraction of a permille is below what this type can store: `0-99.9%`
    /// leaves a sliver where the pointer hits a wall that the user never asked for
    /// and cannot close. One rule, applied both when a range is derived from a drag
    /// and when a stored config is read, so an existing file is repaired rather than
    /// left subtly wrong.
    ///
    /// The tolerance is deliberately short of anything a person would aim for: a
    /// range covering half the edge is unchanged.
    pub const SNAP_PERMILLE: u16 = 2;

    /// See [`EdgeRange::SNAP_PERMILLE`].
    pub fn snapped_to_ends(self) -> EdgeRange {
        let start = if self.start <= Self::SNAP_PERMILLE {
            0
        } else {
            self.start
        };
        let end = if self.end >= EDGE_PERMILLE - Self::SNAP_PERMILLE {
            EDGE_PERMILLE
        } else {
            self.end
        };
        if start >= end {
            return self;
        }
        EdgeRange { start, end }
    }

    /// First permille of the range.
    pub fn start(self) -> u16 {
        self.start
    }

    /// First permille after the range.
    pub fn end(self) -> u16 {
        self.end
    }

    /// Length of the range in permille.
    ///
    /// There is no `is_empty`: a range is non-empty by construction, the
    /// constructors reject `start >= end`.
    #[expect(clippy::len_without_is_empty)]
    pub fn len(self) -> u16 {
        self.end - self.start
    }

    /// Whether the range covers the whole edge.
    pub fn is_full(self) -> bool {
        self.start == 0 && self.end == EDGE_PERMILLE
    }

    /// Whether a crossing at `fraction` along the edge reaches this machine.
    ///
    /// `fraction` counts from the left or top end of the edge, exactly like the
    /// payload of the `Enter` protocol event. The range is half open, with one
    /// deliberate exception: the last pixel of the edge belongs to the range
    /// that ends at the edge's far end, otherwise `fraction == 1.0` - which a
    /// backend reports for the bottom or rightmost pixel - would reach nobody
    /// and the pointer would bounce off a wall that is not drawn.
    pub fn contains(self, fraction: f64) -> bool {
        let value = Self::permille_of(fraction);
        self.start <= value && value < self.end
    }

    /// Map a position on the whole edge to `0.0..=1.0` inside this range.
    ///
    /// This is Synergy's `CellEdge::transform`: it is the coordinate the
    /// crossing point is carried in across a link.
    pub fn transform(self, fraction: f64) -> f64 {
        let value = Self::permille_of(fraction) as f64;
        let span = (self.end - self.start) as f64;
        ((value - self.start as f64) / span).clamp(0.0, 1.0)
    }

    /// Map `0.0..=1.0` inside this range back to a position on the whole edge.
    ///
    /// The inverse of [`EdgeRange::transform`], Synergy's
    /// `CellEdge::inverseTransform`: where on *this* edge the peer's cursor has
    /// to end up when it comes back through the same link.
    pub fn inverse_transform(self, local: f64) -> f64 {
        let local = local.clamp(0.0, 1.0);
        let span = (self.end - self.start) as f64;
        (self.start as f64 + local * span) / EDGE_PERMILLE as f64
    }

    /// Whether two ranges share at least one permille.
    ///
    /// Touching ranges (`0-500` and `500-1000`) do not overlap, matching
    /// Synergy, which only refuses a link whose range "overlaps other existing
    /// edges".
    pub fn overlaps(self, other: EdgeRange) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// The pieces of this range that none of `claimed` covers.
    ///
    /// Up to one piece per gap, so subtracting a range that sits in the middle
    /// of this one yields two pieces: a single client has a single range, so
    /// [`EdgeRange::resolve`] has to pick one of them.
    pub fn subtract(self, claimed: &[EdgeRange]) -> Vec<EdgeRange> {
        let mut blockers: Vec<EdgeRange> = claimed
            .iter()
            .copied()
            .filter(|claimed| self.overlaps(*claimed))
            .collect();
        blockers.sort();
        let mut pieces = Vec::new();
        let mut cursor = self.start;
        for blocker in blockers {
            if blocker.start > cursor {
                if let Some(piece) = EdgeRange::new(cursor, blocker.start) {
                    pieces.push(piece);
                }
            }
            cursor = cursor.max(blocker.end);
        }
        if cursor < self.end {
            if let Some(piece) = EdgeRange::new(cursor, self.end) {
                pieces.push(piece);
            }
        }
        pieces
    }

    /// Resolve a list of ranges that share one edge into what each can reach.
    ///
    /// Earlier entries win: when a range overlaps the ones before it, the
    /// client keeps the largest still-free piece of it (leftmost on a tie),
    /// because one client has exactly one range and a piece in the middle would
    /// mean splitting it. The output is index aligned with the input; `None`
    /// marks a client that is completely covered by earlier ones and therefore
    /// unreachable.
    ///
    /// The order is the order the clients were configured in - the same order
    /// `position_map` already used to decide which client of an edge took the
    /// pointer, so a configuration that worked before keeps working.
    pub fn resolve(ranges: &[EdgeRange]) -> Vec<Option<EdgeRange>> {
        let mut claimed: Vec<EdgeRange> = Vec::with_capacity(ranges.len());
        let mut effective = Vec::with_capacity(ranges.len());
        for range in ranges {
            let piece = range
                .subtract(&claimed)
                .into_iter()
                .max_by_key(|piece| (piece.len(), std::cmp::Reverse(piece.start())));
            if let Some(piece) = piece {
                claimed.push(piece);
            }
            effective.push(piece);
        }
        effective
    }

    /// The smallest range that covers all of `ranges`, `None` when empty.
    pub fn union(ranges: &[EdgeRange]) -> Option<EdgeRange> {
        let start = ranges.iter().map(|r| r.start).min()?;
        let end = ranges.iter().map(|r| r.end).max()?;
        EdgeRange::new(start, end)
    }

    /// The gaps a list of ranges leaves on an edge, in ascending order.
    ///
    /// A gap is where the pointer hits a wall; the arrangement canvas draws it
    /// and the CLI reports it.
    pub fn gaps(ranges: &[EdgeRange]) -> Vec<EdgeRange> {
        let mut covered = ranges.to_vec();
        covered.sort();
        let mut gaps = Vec::new();
        let mut cursor = 0u16;
        for range in covered {
            if range.start > cursor {
                if let Some(gap) = EdgeRange::new(cursor, range.start) {
                    gaps.push(gap);
                }
            }
            cursor = cursor.max(range.end);
        }
        if cursor < EDGE_PERMILLE {
            if let Some(gap) = EdgeRange::new(cursor, EDGE_PERMILLE) {
                gaps.push(gap);
            }
        }
        gaps
    }

    /// One of `count` equal, disjoint ranges covering the whole edge.
    ///
    /// Used by "Auto arrange" and by the menu entry that hands a neighbour half
    /// of an edge.
    pub fn nth_of(count: u16, index: u16) -> EdgeRange {
        let count = count.max(1);
        let index = index.min(count - 1);
        // Round the boundaries rather than the width so the pieces always
        // cover the edge exactly: `1000 / 3` three times would leave 1‰ unused.
        let step = |i: u16| ((i as u32 * EDGE_PERMILLE as u32) / count as u32) as u16;
        EdgeRange::new(
            step(index),
            if index + 1 == count {
                EDGE_PERMILLE
            } else {
                step(index + 1)
            },
        )
        .unwrap_or(EdgeRange::FULL)
    }

    /// Quantize a fraction to a permille bucket in `0..=999`.
    fn permille_of(fraction: f64) -> u16 {
        if !fraction.is_finite() {
            return 0;
        }
        (fraction * EDGE_PERMILLE as f64)
            .floor()
            .clamp(0.0, EDGE_PERMILLE as f64 - 1.0) as u16
    }

    /// `0-60%`, with the fraction digits trimmed: `22.5-44%`.
    fn percent(value: u16) -> String {
        if value.is_multiple_of(10) {
            format!("{}", value / 10)
        } else {
            format!("{:.1}", value as f64 / 10.0)
        }
    }
}

impl Display for EdgeRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}-{}%",
            Self::percent(self.start),
            Self::percent(self.end)
        )
    }
}

/// Parse `0.0,0.6` or `0.0-0.6` (fractions of the edge).
///
/// Percentages are *not* accepted here: `0.5` is already ambiguous between "the
/// half way point" and "half a percent", and the config file, the wire and the
/// GUI all speak fractions.
impl FromStr for EdgeRange {
    type Err = EdgeRangeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let (start, end) = s
            .split_once(',')
            .or_else(|| s.split_once('-'))
            .ok_or(EdgeRangeError::Malformed)?;
        let parse = |value: &str| value.trim().parse::<f64>();
        let start = parse(start).map_err(|_| EdgeRangeError::Malformed)?;
        let end = parse(end).map_err(|_| EdgeRangeError::Malformed)?;
        EdgeRange::from_fractions(start, end)
    }
}

impl Serialize for EdgeRange {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        AsFractions(self.fractions()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EdgeRange {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let AsFractions([start, end]) = AsFractions::deserialize(deserializer)?;
        EdgeRange::from_fractions(start, end).map_err(serde::de::Error::custom)
    }
}

impl Default for EdgeRange {
    fn default() -> Self {
        EdgeRange::FULL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEFT: EdgeRange = EdgeRange { start: 0, end: 500 };
    const RIGHT: EdgeRange = EdgeRange {
        start: 500,
        end: 1000,
    };

    #[test]
    fn touching_ranges_do_not_overlap() {
        assert!(!LEFT.overlaps(RIGHT));
        assert!(!RIGHT.overlaps(LEFT));
        assert!(LEFT.overlaps(EdgeRange::nth_of(3, 1)));
    }

    #[test]
    fn a_crossing_belongs_to_exactly_one_of_two_split_ranges() {
        assert!(LEFT.contains(0.0));
        assert!(LEFT.contains(0.4999));
        assert!(!LEFT.contains(0.5));
        assert!(RIGHT.contains(0.5));
        // the last pixel of the edge must still reach the range that ends there
        assert!(RIGHT.contains(1.0));
        assert!(!LEFT.contains(1.0));
    }

    #[test]
    fn a_crossing_outside_every_range_reaches_nobody() {
        let first = EdgeRange { start: 0, end: 400 };
        let last = EdgeRange {
            start: 600,
            end: 1000,
        };
        // the 200‰ between them is a wall
        assert!(!first.contains(0.5) && !last.contains(0.5));
        assert!(first.contains(0.399));
        assert!(last.contains(0.6));
    }

    #[test]
    fn transform_and_inverse_round_trip() {
        let range = EdgeRange {
            start: 200,
            end: 700,
        };
        assert_eq!(range.transform(0.2), 0.0);
        assert_eq!(range.transform(0.7), 1.0);
        assert_eq!(range.transform(0.45), 0.5);
        assert!((range.inverse_transform(0.5) - 0.45).abs() < 1e-9);
        for fraction in [0.2, 0.31, 0.45, 0.6, 0.7] {
            let local = range.transform(fraction);
            let back = range.inverse_transform(local);
            assert!((back - fraction).abs() < 0.002, "{fraction} -> {back}");
        }
    }

    #[test]
    fn transform_clamps_positions_outside_the_range() {
        assert_eq!(RIGHT.transform(0.1), 0.0);
        assert_eq!(LEFT.transform(0.9), 1.0);
    }

    #[test]
    fn permille_quantization_is_stable() {
        let range = EdgeRange::from_fractions(0.0005, 0.9995).expect("valid");
        assert_eq!([range.start(), range.end()], [1u16, 1000u16]);
        assert_eq!(range.fractions()[1], 1.0);
    }

    #[test]
    fn rejects_empty_and_out_of_bounds() {
        assert!(EdgeRange::from_fractions(0.5, 0.5).is_err());
        assert!(EdgeRange::from_fractions(0.6, 0.4).is_err());
        assert!(EdgeRange::from_fractions(-0.1, 0.5).is_err());
        assert!(EdgeRange::from_fractions(0.0, 1.5).is_err());
        assert!(EdgeRange::from_fractions(f64::NAN, 1.0).is_err());
        assert!(EdgeRange::new(500, 500).is_none());
        assert!(EdgeRange::new(0, 1001).is_none());
    }

    #[test]
    fn parse_accepts_comma_and_dash() {
        assert_eq!(
            "0.25,0.75".parse::<EdgeRange>().unwrap(),
            EdgeRange {
                start: 250,
                end: 750
            }
        );
        assert_eq!(
            "0.25-0.75".parse::<EdgeRange>().unwrap(),
            EdgeRange {
                start: 250,
                end: 750
            }
        );
        assert!("0.75,0.25".parse::<EdgeRange>().is_err());
        assert!("half".parse::<EdgeRange>().is_err());
        assert!("0.5".parse::<EdgeRange>().is_err());
    }

    #[test]
    fn display_shows_trimmed_percentages() {
        assert_eq!(EdgeRange::FULL.to_string(), "0-100%");
        assert_eq!(LEFT.to_string(), "0-50%");
        assert_eq!(
            EdgeRange {
                start: 225,
                end: 440
            }
            .to_string(),
            "22.5-44%"
        );
    }

    #[test]
    fn resolve_lets_the_earlier_client_keep_the_overlap() {
        // 0-600 and 400-1000: the second is trimmed to 600-1000
        let effective = EdgeRange::resolve(&[
            EdgeRange { start: 0, end: 600 },
            EdgeRange {
                start: 400,
                end: 1000,
            },
        ]);
        assert_eq!(
            effective,
            vec![
                Some(EdgeRange { start: 0, end: 600 }),
                Some(EdgeRange {
                    start: 600,
                    end: 1000
                })
            ]
        );
    }

    #[test]
    fn resolve_marks_a_fully_covered_client_unreachable() {
        let effective = EdgeRange::resolve(&[
            EdgeRange::FULL,
            EdgeRange {
                start: 100,
                end: 200,
            },
        ]);
        assert_eq!(effective[0], Some(EdgeRange::FULL));
        assert_eq!(effective[1], None);
    }

    #[test]
    fn resolve_keeps_disjoint_ranges_untouched() {
        let effective = EdgeRange::resolve(&[LEFT, RIGHT]);
        assert_eq!(effective, vec![Some(LEFT), Some(RIGHT)]);
    }

    #[test]
    fn resolve_gives_a_straddling_client_the_largest_free_piece() {
        // 0-200, 300-800, then 100-900: only 200-300 and 800-900 are left, both
        // 100 wide, so the leftmost one wins (a client has one range, not two)
        let effective = EdgeRange::resolve(&[
            EdgeRange { start: 0, end: 200 },
            EdgeRange {
                start: 300,
                end: 800,
            },
            EdgeRange {
                start: 100,
                end: 900,
            },
        ]);
        assert_eq!(
            effective[2],
            Some(EdgeRange {
                start: 200,
                end: 300
            })
        );
        let covered: u16 = effective.iter().flatten().map(|r| r.len()).sum();
        assert!(covered <= EDGE_PERMILLE);
        assert!(
            EdgeRange::gaps(&effective.iter().flatten().copied().collect::<Vec<_>>())
                .iter()
                .all(|gap| gap.len() >= 100)
        );
    }

    #[test]
    fn subtract_returns_one_piece_per_gap() {
        let range = EdgeRange {
            start: 0,
            end: 1000,
        };
        assert_eq!(
            range.subtract(&[EdgeRange {
                start: 300,
                end: 500
            }]),
            vec![
                EdgeRange { start: 0, end: 300 },
                EdgeRange {
                    start: 500,
                    end: 1000
                }
            ]
        );
        // a blocker that covers everything leaves nothing
        assert!(range.subtract(&[EdgeRange::FULL]).is_empty());
        // touching is not overlapping
        assert_eq!(
            LEFT.subtract(&[RIGHT]),
            vec![EdgeRange { start: 0, end: 500 }]
        );
    }

    #[test]
    fn gaps_finds_the_walls() {
        let gaps = EdgeRange::gaps(&[
            EdgeRange { start: 0, end: 200 },
            EdgeRange {
                start: 500,
                end: 1000,
            },
        ]);
        assert_eq!(
            gaps,
            vec![EdgeRange {
                start: 200,
                end: 500
            }]
        );
        assert!(EdgeRange::gaps(&[LEFT, RIGHT]).is_empty());
        assert_eq!(EdgeRange::gaps(&[]), vec![EdgeRange::FULL]);
    }

    #[test]
    fn union_spans_from_the_first_start_to_the_last_end() {
        let ranges = [
            EdgeRange {
                start: 300,
                end: 400,
            },
            EdgeRange { start: 0, end: 100 },
            EdgeRange {
                start: 900,
                end: 1000,
            },
        ];
        assert_eq!(
            EdgeRange::union(&ranges),
            Some(EdgeRange {
                start: 0,
                end: 1000
            })
        );
        assert_eq!(EdgeRange::union(&[]), None);
    }

    #[test]
    fn auto_arrange_covers_the_edge_exactly() {
        for count in 1..=7u16 {
            let ranges: Vec<EdgeRange> = (0..count).map(|i| EdgeRange::nth_of(count, i)).collect();
            let covered: u16 = ranges.iter().map(|r| r.len()).sum();
            assert_eq!(covered, EDGE_PERMILLE, "{count} pieces cover {covered}");
            assert!(EdgeRange::gaps(&ranges).is_empty());
            for pair in ranges.windows(2) {
                assert!(!pair[0].overlaps(pair[1]), "{pair:?} overlap");
                assert_eq!(pair[0].end(), pair[1].start());
            }
        }
    }

    #[test]
    fn a_range_a_hair_short_of_an_end_is_pulled_onto_it() {
        /* the two shapes a drag actually produces when a card is pushed against the
         * edge of a screen */
        let short = EdgeRange::from_fractions(0.0, 0.999).expect("range");
        assert_eq!(short.snapped_to_ends(), EdgeRange::FULL);
        let both = EdgeRange::from_fractions(0.001, 0.999).expect("range");
        assert_eq!(both.snapped_to_ends(), EdgeRange::FULL);

        /* a range that means something is left alone */
        let half = EdgeRange::from_fractions(0.0, 0.5).expect("range");
        assert_eq!(half.snapped_to_ends(), half);
        let middle = EdgeRange::from_fractions(0.25, 0.75).expect("range");
        assert_eq!(middle.snapped_to_ends(), middle);
    }

    #[test]
    fn serde_round_trips_through_fractions() {
        let range = EdgeRange {
            start: 225,
            end: 750,
        };
        let json = serde_json::to_string(&range).expect("serialize");
        assert_eq!(json, "[0.225,0.75]");
        let back: EdgeRange = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, range);
    }

    #[test]
    fn serde_rejects_an_empty_range() {
        let error = serde_json::from_str::<EdgeRange>("[0.5,0.5]").unwrap_err();
        assert!(error.to_string().contains("empty"), "{error}");
    }

    #[test]
    fn default_is_the_whole_edge() {
        assert_eq!(EdgeRange::default(), EdgeRange::FULL);
    }
}
