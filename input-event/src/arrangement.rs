//! The arrangement both machines agree on.
//!
//! Until this existed each side derived the picture from its own configuration:
//! its own monitors, its own links, its own idea of where the other machine sits.
//! Two derivations can always disagree - and they did, silently, because each side
//! was drawing its own answer correctly.
//!
//! This is that picture instead: absolute rectangles for every screen of every
//! machine, in one coordinate space, produced by whichever side last edited it.
//! The other side draws these rectangles rather than deriving its own, so the two
//! windows cannot show different desks.
//!
//! The rectangles are in *screen* units (the same space `Layout` uses), so the
//! receiving canvas maps them with the transform it already applies to monitors.

use crate::display::ScreenRect;

/// One machine and the rectangles of its screens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArrangedMachine {
    /// the machine's hostname, for a log or a label
    ///
    /// Not an identity: each side names the other however its own configuration
    /// says, so the same machine arrives as `192.168.1.5` on one end and as
    /// `pavia-macbook-pro.local` on the other. Matching on it made adoption fail
    /// silently - the receiving side looked for itself and found nothing.
    pub hostname: String,
    /// whether these screens are the author's own
    ///
    /// The only identity that matters: a picture has the author's machine and the
    /// machines it can see, and the receiver is one of the latter.
    pub authored: bool,
    /// its screens, in the shared coordinate space
    pub screens: Vec<ScreenRect>,
}

/// One picture of the whole desk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arrangement {
    /// bumped by whoever edits, so the two sides can agree on which picture is
    /// current without a round trip
    pub revision: u64,
    /// the hostname of the machine that produced it
    ///
    /// Part of the ordering: a revision alone is not enough, because two sides can
    /// both reach revision 3 without having seen each other's edit. The author
    /// breaks the tie, and both sides compare the same pair, so both pick the same
    /// winner.
    pub author: String,
    pub machines: Vec<ArrangedMachine>,
}

impl Arrangement {
    pub fn new(revision: u64, author: impl Into<String>) -> Self {
        Self {
            revision,
            author: author.into(),
            machines: Vec::new(),
        }
    }

    /// Whether this picture should be shown instead of `other`.
    ///
    /// Highest revision wins; on a tie the higher hostname wins, which is arbitrary
    /// but *stable* - that is the whole requirement, since both sides run this.
    pub fn supersedes(&self, other: &Arrangement) -> bool {
        (self.revision, &self.author) > (other.revision, &other.author)
    }

    /// Every rectangle, for framing the view.
    pub fn bounds(&self) -> Option<ScreenRect> {
        let mut screens = self.machines.iter().flat_map(|m| m.screens.iter());
        let first = *screens.next()?;
        Some(screens.fold(first, |acc, rect| {
            let x = acc.x.min(rect.x);
            let y = acc.y.min(rect.y);
            let right = (acc.x + acc.width as i32).max(rect.x + rect.width as i32);
            let bottom = (acc.y + acc.height as i32).max(rect.y + rect.height as i32);
            ScreenRect {
                x,
                y,
                width: (right - x) as u32,
                height: (bottom - y) as u32,
            }
        }))
    }

    /// The author's own screens.
    pub fn authored_screens(&self) -> &[ScreenRect] {
        self.machines
            .iter()
            .find(|machine| machine.authored)
            .map(|machine| machine.screens.as_slice())
            .unwrap_or(&[])
    }

    /// The screens of the machine that is not the author.
    ///
    /// That is the receiver: a picture is produced by one machine about the desk it
    /// can see, so exactly one entry is the author and the rest are peers.
    pub fn peer_screens(&self) -> Vec<(&str, &[ScreenRect])> {
        self.machines
            .iter()
            .filter(|machine| !machine.authored)
            .map(|machine| (machine.hostname.as_str(), machine.screens.as_slice()))
            .collect()
    }

    /// The screens of one machine, by hostname.
    pub fn screens_of(&self, hostname: &str) -> &[ScreenRect] {
        self.machines
            .iter()
            .find(|machine| machine.hostname == hostname)
            .map(|machine| machine.screens.as_slice())
            .unwrap_or(&[])
    }

    /// `revision|author|flag|x,y,w,h;...|flag|x,y,w,h;...`
    ///
    /// Deliberately textual: it travels in a fixed-width report that only needs to be
    /// understood by the same code on the other machine, and a format that can be
    /// read in a log is worth more than a compact one.
    ///
    /// The hostnames are left out. They are not identities - each side names the other
    /// however its own configuration says, which is what made adoption look for itself
    /// and find nothing - and a picture is read by the machine that produced it and
    /// the machine it is about, so the flag says everything the receiver needs. It is
    /// also what keeps the encoding inside the report: with names it was 140 bytes for
    /// this desk, in a slot of 124.
    pub fn encode(&self) -> String {
        let mut out = format!("{}|{}", self.revision, clean(&self.author));
        for machine in &self.machines {
            out.push('|');
            out.push(if machine.authored { '1' } else { '0' });
            out.push('|');
            let screens = machine
                .screens
                .iter()
                .map(|s| format!("{},{},{},{}", s.x, s.y, s.width, s.height))
                .collect::<Vec<_>>()
                .join(";");
            out.push_str(&screens);
        }
        out
    }

    /// The inverse of [`Arrangement::encode`].
    ///
    /// `None` when the text is not an arrangement at all. Individual machines and
    /// screens that do not parse are skipped rather than failing the whole picture:
    /// a partial desk is still worth drawing, and dropping it would leave the canvas
    /// blank because one number was wrong.
    pub fn decode(text: &str) -> Option<Self> {
        let mut parts = text.split('|');
        let revision = parts.next()?.parse().ok()?;
        let author = parts.next()?.to_owned();
        let mut machines = Vec::new();
        while let Some(flag) = parts.next() {
            let authored = flag == "1";
            let screens = parts.next().unwrap_or_default();
            let screens = screens
                .split(';')
                .filter(|piece| !piece.is_empty())
                .filter_map(parse_screen)
                .collect::<Vec<_>>();
            if screens.is_empty() {
                continue;
            }
            machines.push(ArrangedMachine {
                /* the name is not on the wire; a caller that needs one has the machine
                 * it asked about */
                hostname: String::new(),
                authored,
                screens,
            });
        }
        Some(Self {
            revision,
            author,
            machines,
        })
    }
}

fn parse_screen(piece: &str) -> Option<ScreenRect> {
    let mut numbers = piece.split(',');
    let x = numbers.next()?.parse().ok()?;
    let y = numbers.next()?.parse().ok()?;
    let width = numbers.next()?.parse().ok()?;
    let height = numbers.next()?.parse().ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(ScreenRect {
        x,
        y,
        width,
        height,
    })
}

/// Hostnames go into a `|`- and `;`-delimited string, so anything that would split
/// it is replaced. A machine is still identifiable; a name with a pipe in it is not
/// worth corrupting the picture for.
fn clean(text: &str) -> String {
    text.replace(['|', ';', ','], "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: u32, height: u32) -> ScreenRect {
        ScreenRect {
            x,
            y,
            width,
            height,
        }
    }

    /// The desk this was written for: an upright panel at (0,1120) and a rotated one
    /// at (2560,0) on one machine, a single panel above the first on the other.
    fn desk() -> Arrangement {
        let mut arrangement = Arrangement::new(7, "cachyos-x8664");
        arrangement.machines.push(ArrangedMachine {
            hostname: "cachyos-x8664".to_owned(),
            authored: true,
            screens: vec![rect(0, 1120, 2560, 1440), rect(2560, 0, 1440, 2560)],
        });
        arrangement.machines.push(ArrangedMachine {
            /* the author names this machine however its configuration says, which
             * is not what the machine calls itself */
            hostname: "192.168.1.5".to_owned(),
            authored: false,
            screens: vec![rect(0, -1500, 2560, 1440)],
        });
        arrangement
    }

    #[test]
    fn an_arrangement_survives_a_round_trip() {
        let arrangement = desk();
        let decoded = Arrangement::decode(&arrangement.encode()).expect("decodes");
        assert_eq!(decoded.revision, arrangement.revision);
        assert_eq!(decoded.author, arrangement.author);
        /* names are not on the wire; the flag and the rectangles are */
        assert_eq!(decoded.authored_screens(), arrangement.authored_screens());
        assert_eq!(
            decoded
                .peer_screens()
                .iter()
                .map(|(_, s)| *s)
                .collect::<Vec<_>>(),
            arrangement
                .peer_screens()
                .iter()
                .map(|(_, s)| *s)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_picture_covers_every_screen() {
        let bounds = desk().bounds().expect("bounds");
        /* x 0..4000, y -1500..2560 */
        assert_eq!(bounds.x, 0);
        assert_eq!(bounds.y, -1500);
        assert_eq!(bounds.width, 4000);
        assert_eq!(bounds.height, 4060);
    }

    #[test]
    fn the_author_and_the_peer_are_told_apart_without_matching_names() {
        let arrangement = desk();
        /* the receiver is not in the picture under the name it uses for itself,
         * which is exactly why the flag exists */
        assert_eq!(arrangement.authored_screens().len(), 2);
        let peers = arrangement.peer_screens();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].0, "192.168.1.5");
        assert_eq!(peers[0].1.len(), 1);
    }

    #[test]
    fn the_higher_revision_wins_and_the_author_breaks_a_tie() {
        let a = Arrangement::new(3, "alpha");
        let b = Arrangement::new(3, "beta");
        let newer = Arrangement::new(4, "alpha");
        /* both sides run this, so they have to reach the same conclusion */
        assert!(b.supersedes(&a));
        assert!(!a.supersedes(&b));
        assert!(newer.supersedes(&b));
        assert!(!b.supersedes(&newer));
    }

    #[test]
    fn text_that_is_not_an_arrangement_is_rejected() {
        assert_eq!(Arrangement::decode(""), None);
        assert_eq!(Arrangement::decode("not a revision|author"), None);
        /* a revision and an author with nothing else is an empty desk, not garbage */
        let empty = Arrangement::decode("1|me").expect("decodes");
        assert!(empty.machines.is_empty());
    }

    #[test]
    fn a_broken_screen_does_not_discard_the_rest() {
        let decoded =
            Arrangement::decode("2|author|host|0,0,100,100;garbage;10,10,0,10").expect("decodes");
        assert_eq!(decoded.machines.len(), 1);
        assert_eq!(decoded.machines[0].screens, vec![rect(0, 0, 100, 100)]);
    }

    #[test]
    fn a_hostname_cannot_split_the_encoded_string() {
        let mut arrangement = Arrangement::new(1, "a|b;c");
        arrangement.machines.push(ArrangedMachine {
            hostname: "x|y".to_owned(),
            authored: true,
            screens: vec![rect(0, 0, 10, 10)],
        });
        let decoded = Arrangement::decode(&arrangement.encode()).expect("decodes");
        /* the author still travels, so it still has to be cleaned */
        assert_eq!(decoded.author, "a_b_c");
        assert!(decoded.machines[0].authored);
        assert_eq!(decoded.machines[0].screens, vec![rect(0, 0, 10, 10)]);
    }
}
