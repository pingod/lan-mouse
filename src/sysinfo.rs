//! Collect what this machine reports about itself to peers.
//!
//! Most of it is gathered once at startup: it describes hardware and OS install
//! state, which do not change while the process runs, and reading it per
//! handshake would shell out repeatedly for no benefit. The display geometry is
//! the exception - it is what the peer needs in order to draw this screen to
//! scale, and it changes when a monitor is plugged in, so it is refreshed from
//! the capture backend rather than probed here.

use lan_mouse_proto::{PeerInfo, info_slot};

/// This machine's description, with the display slot kept up to date.
///
/// `PeerInfo` is `Copy`, so the borrow ends here and nothing hands out a
/// reference into the thread local.
pub fn local() -> PeerInfo {
    shared().info
}

/// Everything the report is assembled from, shared by every thread.
///
/// Deliberately *not* a thread-local. The capture and emulation backends run on
/// their own OS threads, so a thread-local copy meant each of them answered
/// `PeerInfoRequest` with a report built from its own half-remembered state: the
/// display list was there because the capture thread is what sets it, and the
/// arrangement was missing because it is published on the service thread. The peer
/// then stored a report that looked complete and carried nothing.
static SHARED: std::sync::OnceLock<std::sync::Mutex<Shared>> = std::sync::OnceLock::new();

struct Shared {
    /// the last geometry the capture backend reported
    layout: Option<input_event::Layout>,
    /// the picture this machine is drawing
    arrangement: Option<input_event::Arrangement>,
    /// the assembled report
    info: PeerInfo,
}

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED
        .get_or_init(|| {
            std::sync::Mutex::new(Shared {
                layout: None,
                arrangement: None,
                info: build(None, None),
            })
        })
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Record the size of the area the pointer can reach and put it in the report.
///
/// The peer draws this machine's screen to scale with it, which is what turns an
/// edge range from a decoration into something both ends agree on. `None` keeps
/// whatever the platform probe found.
pub fn set_layout(layout: Option<input_event::Layout>) {
    let mut shared = shared();
    shared.layout = layout;
    rebuild(&mut shared);
}

/// Publish the arrangement this machine is showing so the peer can draw the same
/// one, and report whether it changed - a report is only worth resending when it
/// did.
pub fn set_arrangement(arrangement: Option<input_event::Arrangement>) -> bool {
    let mut shared = shared();
    if shared.arrangement == arrangement {
        return false;
    }
    shared.arrangement = arrangement;
    rebuild(&mut shared);
    true
}

/// Re-assemble the report from the parts that change.
fn rebuild(shared: &mut Shared) {
    let updated = build(shared.layout.clone(), shared.arrangement.clone());
    shared.info = updated;
}

fn build(
    layout: Option<input_event::Layout>,
    arrangement: Option<input_event::Arrangement>,
) -> PeerInfo {
    // `PeerInfo::new` takes `&str`; the owned values live until the call
    // returns, so bind them first.
    let os_version = os_version();
    let kernel = kernel_version();
    let hostname = hostname();
    let commit = short_commit();
    let mut info = PeerInfo::new([
        os_name(),
        &os_version,
        &kernel,
        std::env::consts::ARCH,
        &hostname,
        env!("CARGO_PKG_VERSION"),
        &commit,
    ]);
    /* the display list does not fit in one slot: two monitors already run past it,
     * and a list cut there still decodes as a valid list of one monitor */
    info.set_displays(&displays(layout));
    /* the picture both machines draw, sent by whichever last edited it */
    if let Some(arrangement) = arrangement {
        let encoded = arrangement.encode();
        /* A record is fixed width, so an oversized picture is cut - and a cut
         * picture still decodes, as a desk with fewer screens. Worth saying out
         * loud, because the symptom is a screen missing on the other machine and
         * nothing here looks wrong. */
        let room = lan_mouse_proto::WORLD_SLOTS * (lan_mouse_proto::PEER_INFO_FIELD - 1);
        if encoded.len() > room {
            log::warn!(
                "the arrangement needs {} bytes but only {} fit; screens will be missing on the peer",
                encoded.len(),
                room
            );
        }
        info.set_world(&encoded);
    }
    info
}

/// `linux`, `macos`, `windows`.
fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "linux") {
        "linux"
    } else {
        std::env::consts::OS
    }
}

/// Distribution or macOS release, e.g. `CachyOS`, `14.4`.
fn os_version() -> String {
    #[cfg(target_os = "linux")]
    {
        // os-release is the standard place for this on Linux.
        if let Ok(text) = std::fs::read_to_string("/etc/os-release") {
            for key in ["PRETTY_NAME", "NAME"] {
                if let Some(value) = text
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{key}=")))
                    .map(|v| v.trim_matches('"').to_owned())
                {
                    return value;
                }
            }
        }
        String::new()
    }
    #[cfg(target_os = "macos")]
    {
        command_output("sw_vers", &["-productVersion"]).unwrap_or_default()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        String::new()
    }
}

/// Kernel release, e.g. `6.12.0-1-cachyos`, `Darwin 24.4.0`.
fn kernel_version() -> String {
    #[cfg(unix)]
    {
        command_output("uname", &["-r"]).unwrap_or_default()
    }
    #[cfg(not(unix))]
    {
        String::new()
    }
}

fn hostname() -> String {
    // Avoids pulling the `hostname` crate into this crate's dependencies; the
    // value is only used for display.
    #[cfg(unix)]
    {
        command_output("uname", &["-n"]).unwrap_or_default()
    }
    #[cfg(not(unix))]
    {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    }
}

/// Short build commit, matching what the version row shows.
fn short_commit() -> String {
    // `config` already expands `shadow!(build)`; reuse that rather than
    // declaring the macro twice.
    crate::config::local_commit_string()
}

/// Display geometry, as `"<width>x<height>"`.
///
/// The capture backend's answer wins: it is the area the pointer can actually
/// reach, which is what the peer has to line its edge up with. Only when the
/// backend does not report one does this fall back to a platform probe, and the
/// probe keeps the `N displays, WxH` shape it always had - the size parser reads
/// the leading `WxH` of either form.
fn displays(layout: Option<input_event::Layout>) -> String {
    if let Some(layout) = layout.filter(|layout| !layout.is_empty()) {
        return layout.encode();
    }
    probe_displays()
}

/// Platform probe of the display geometry.
fn probe_displays() -> String {
    #[cfg(target_os = "linux")]
    {
        // Wayland sessions usually have no X display; report nothing rather
        // than spawning a compositor-specific tool.
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return String::new();
        }
        let out = command_output("xrandr", &["--query"]).unwrap_or_default();
        let mut count = 0usize;
        let mut width = 0u32;
        let mut height = 0u32;
        for line in out.lines() {
            if let Some(rest) = line.split(" connected").nth(1) {
                count += 1;
                if let Some(geom) = rest.split_whitespace().find(|w| w.contains('x')) {
                    if let Some((w, h)) = geom.split_once('x') {
                        width = width.max(w.parse().unwrap_or(0));
                        height = height.max(h.split('+').next().unwrap_or(h).parse().unwrap_or(0));
                    }
                }
            }
        }
        if count == 0 {
            String::new()
        } else {
            format!("{count} displays, {width}x{height}")
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        String::new()
    }
}

/// Run `cmd args`, returning trimmed stdout on success.
///
/// Only the unix and macOS paths call this; Windows reads its values from the
/// environment instead.
#[cfg_attr(not(unix), allow(dead_code))]
fn command_output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Flatten a peer's report into the IPC shape.
///
/// The IPC crate deliberately does not depend on the protocol crate, so the
/// fixed-width slots are expanded into plain strings here.
pub fn flatten(info: &PeerInfo) -> lan_mouse_ipc::IncomingInfo {
    lan_mouse_ipc::IncomingInfo {
        addr: "0.0.0.0:0".parse().expect("valid addr"),
        fingerprint: String::new(),
        pos: lan_mouse_ipc::Position::Left,
        idle_secs: 0,
        received: 0,
        active: false,
        os: info.get(info_slot::OS).to_owned(),
        os_version: info.get(info_slot::OS_VERSION).to_owned(),
        kernel: info.get(info_slot::KERNEL).to_owned(),
        arch: info.get(info_slot::ARCH).to_owned(),
        hostname: info.get(info_slot::HOSTNAME).to_owned(),
        app_version: info.get(info_slot::APP_VERSION).to_owned(),
        commit: info.get(info_slot::COMMIT).to_owned(),
        displays: info.displays(),
    }
}

/// Split a report into wire-sized chunks.
///
/// The listening side cannot push a whole record to the connecting side, so
/// every report travels as small pieces regardless of direction.
pub fn chunks(info: &PeerInfo) -> Vec<lan_mouse_proto::ProtoEvent> {
    use lan_mouse_proto::{PEER_INFO_CHUNK, ProtoEvent};

    // Flatten the fixed slots into one byte string; trailing NULs are kept so
    // the receiver can rebuild the exact same layout.
    let bytes: Vec<u8> = info.fields.iter().flatten().copied().collect();
    let total = bytes.len().div_ceil(PEER_INFO_CHUNK);
    bytes
        .chunks(PEER_INFO_CHUNK)
        .enumerate()
        .map(|(i, piece)| {
            let mut data = [0u8; PEER_INFO_CHUNK];
            data[..piece.len()].copy_from_slice(piece);
            ProtoEvent::PeerInfoChunk {
                index: i as u8,
                total: total as u8,
                data,
            }
        })
        .collect()
}

/// Collects chunks until a full report is available.
///
/// Order-independent, and that is the point. A report is 20 slots of 32 bytes split
/// into 16-byte pieces - forty datagrams - and UDP delivers them in any order and
/// loses some. The first version demanded `index == next` exactly, so a single
/// reordered or dropped packet discarded the whole report: the peer kept an older
/// one, which for the display list was invisible (it rarely changes) and for the
/// arrangement meant a picture that never arrived.
///
/// Each chunk is stored where it belongs, and the report is produced once every
/// piece is present. A new report starting at index 0 resets the buffer.
#[derive(Default)]
pub struct Reassembler {
    /// one entry per chunk index; `None` until that chunk arrives
    parts: Vec<Option<[u8; lan_mouse_proto::PEER_INFO_CHUNK]>>,
    total: Option<u8>,
    /// when the current report started arriving
    ///
    /// A report that never completes - a datagram lost for good - would otherwise
    /// hold its slots forever, and since the sender keeps sending, every later report
    /// would be a different `total` or the same one with holes. A stale partial report
    /// is dropped rather than waited on.
    started: Option<std::time::Instant>,
}

/// How long to wait for the rest of a report before giving up on it.
const REASSEMBLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

impl Reassembler {
    /// Feed one chunk; returns the report once every piece has arrived.
    pub fn push(
        &mut self,
        index: u8,
        total: u8,
        data: &[u8; lan_mouse_proto::PEER_INFO_CHUNK],
    ) -> Option<PeerInfo> {
        use lan_mouse_proto::{PEER_INFO_CHUNK, PEER_INFO_FIELD, PEER_INFO_FIELDS};

        if total == 0 || index >= total {
            return None;
        }
        /* A different `total` is a different report - the sender changed size, which
         * happens when the picture it carries grew. Start over rather than mixing
         * pieces of two reports. */
        let stale = self
            .started
            .is_some_and(|started| started.elapsed() > REASSEMBLY_TIMEOUT);
        if self.total != Some(total) || stale {
            self.parts = vec![None; total as usize];
            self.total = Some(total);
            self.started = Some(std::time::Instant::now());
        }
        self.parts[index as usize] = Some(*data);

        if self.parts.iter().any(Option::is_none) {
            return None;
        }
        let mut buf = Vec::with_capacity(self.parts.len() * PEER_INFO_CHUNK);
        for part in &self.parts {
            buf.extend_from_slice(part.as_ref().expect("checked above"));
        }
        /* Ready for the next report, which starts at index 0 again. */
        self.total = None;
        self.started = None;

        let expected = PEER_INFO_FIELDS * PEER_INFO_FIELD;
        if buf.len() < expected {
            return None;
        }
        let mut fields = [[0u8; PEER_INFO_FIELD]; PEER_INFO_FIELDS];
        for (slot, out) in fields.iter_mut().zip(buf.chunks(PEER_INFO_FIELD)) {
            let n = out.len().min(PEER_INFO_FIELD);
            slot[..n].copy_from_slice(&out[..n]);
        }
        Some(PeerInfo { fields })
    }
}

/// A one-line summary for the log.
pub fn describe(info: &PeerInfo) -> String {
    format!(
        "{} {} ({}, {}), host {}",
        info.get(info_slot::OS),
        info.get(info_slot::OS_VERSION),
        info.get(info_slot::KERNEL),
        info.get(info_slot::ARCH),
        info.get(info_slot::HOSTNAME),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report is one per process now, so these tests cannot run at the same time
    /// as each other - `set_layout` in one would be visible to another. Each test
    /// takes this first; the guard is released when the test ends.
    static EXCLUSIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        EXCLUSIVE
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn one_monitor() -> input_event::Layout {
        input_event::Layout::new(vec![input_event::DisplayInfo::new(
            "DP-1",
            0,
            0,
            2560,
            1440,
            input_event::Rotation::Normal,
        )])
    }

    /// The report is refreshed, not built once, so a test that changes it must
    /// not leave the new layout behind for the tests that run after it.
    /// The picture has to fit in the slots it travels in.
    ///
    /// It did not: this desk needed 140 bytes for a slot of 124, so the arrangement
    /// arrived truncated - and a truncated one still decodes, as a desk with a screen
    /// missing, which is why nothing looked broken while nothing synced.
    #[test]
    fn the_arrangement_fits_in_the_slots_it_travels_in() {
        let _exclusive = exclusive();
        let mut arrangement = input_event::Arrangement::new(1, "cachyos-x8664");
        arrangement.machines.push(input_event::ArrangedMachine {
            hostname: "cachyos-x8664".to_owned(),
            authored: true,
            screens: vec![
                input_event::ScreenRect {
                    x: 0,
                    y: 1120,
                    width: 2560,
                    height: 1440,
                },
                input_event::ScreenRect {
                    x: 2560,
                    y: 0,
                    width: 1440,
                    height: 2560,
                },
            ],
        });
        arrangement.machines.push(input_event::ArrangedMachine {
            hostname: "192.168.1.5".to_owned(),
            authored: false,
            screens: vec![input_event::ScreenRect {
                x: 0,
                y: -1500,
                width: 2560,
                height: 1440,
            }],
        });
        set_arrangement(Some(arrangement));
        let report = local();
        assert_eq!(report.world(), {
            let mut expected = input_event::Arrangement::decode(&report.world()).expect("decodes");
            expected.revision = 1;
            expected.author = "cachyos-x8664".to_owned();
            expected.encode()
        });
        /* two screens survived, not one */
        let decoded = input_event::Arrangement::decode(&report.world()).expect("decodes");
        assert_eq!(decoded.authored_screens().len(), 2);
        assert_eq!(decoded.peer_screens().len(), 1);
        set_arrangement(None);
    }

    #[test]
    fn the_layout_goes_into_the_display_slot() {
        let _exclusive = exclusive();
        let before = local().get(info_slot::DISPLAYS).to_owned();

        set_layout(Some(one_monitor()));
        assert_eq!(
            local().get(info_slot::DISPLAYS),
            "DP-1:0,0:2560x1440:normal:1"
        );

        /* A backend that reports nothing falls back to the platform probe, which
         * is what an older or geometry-less backend used to produce. */
        set_layout(None);
        assert_eq!(local().get(info_slot::DISPLAYS), before);
    }

    #[test]
    fn local_info_is_populated() {
        let _exclusive = exclusive();
        let info = &local();
        // Architecture and app version are compile-time constants and must
        // always be present; OS detection always resolves to something.
        assert_eq!(info.get(info_slot::ARCH), std::env::consts::ARCH);
        assert_eq!(info.get(info_slot::APP_VERSION), env!("CARGO_PKG_VERSION"));
        assert!(!info.get(info_slot::OS).is_empty());
    }

    /// The display slot is what a peer reads this machine's screens out of, so
    /// what goes on the wire has to come back as the same layout.
    ///
    /// This is the join between the halves of the feature: `set_layout` writes the
    /// slot, `flatten` copies it into the peer report, and the canvas on the other
    /// end decodes it back into rectangles.
    #[test]
    fn the_display_slot_round_trips_through_the_peer_report() {
        let _exclusive = exclusive();
        set_layout(Some(one_monitor()));

        let report = flatten(&local());
        let decoded = input_event::Layout::decode(&report.displays).expect("decodes");
        assert_eq!(decoded, one_monitor());
        // and the bounding box an older peer would look for is still derivable
        assert_eq!(
            lan_mouse_ipc::parse_screen_size(&report.displays),
            Some((2560, 1440)),
            "an older peer cannot read a size out of {:?}",
            report.displays
        );

        set_layout(None);
    }

    #[test]
    fn chunks_reassemble_to_the_original() {
        let _exclusive = exclusive();
        let mut info = PeerInfo::new([
            "linux",
            "CachyOS",
            "7.3.4-1-cachyos",
            "x86_64",
            "cachyos-x8664",
            "0.11.0",
            "abc12345",
        ]);
        let displays = "DP-2:2560,0:1440x2560:right:1;DP-3:0,1120:2560x1440:normal:1";
        info.set_displays(displays);
        let pieces = chunks(&info);
        assert!(pieces.len() > 1, "a report must span chunks");

        let mut r = Reassembler::default();
        let mut out = None;
        for piece in pieces {
            let lan_mouse_proto::ProtoEvent::PeerInfoChunk { index, total, data } = piece else {
                panic!("expected a chunk");
            };
            out = r.push(index, total, &data);
        }
        let out = out.expect("all chunks arrived");
        assert_eq!(out, info);
        assert_eq!(out.get(info_slot::KERNEL), "7.3.4-1-cachyos");
        /* The list is longer than one slot and has to survive the trip intact: a
         * cut at the first slot still decodes as a valid list of one monitor, which
         * is how two machines came to draw each other as a single screen. */
        assert!(displays.len() > lan_mouse_proto::PEER_INFO_FIELD);
        assert_eq!(out.displays(), displays);
    }

    #[test]
    fn a_missing_chunk_yields_nothing() {
        let _exclusive = exclusive();
        let info = PeerInfo::new(["a", "b", "c", "d", "e", "f", "g"]);
        let mut r = Reassembler::default();
        let pieces = chunks(&info);
        // Feed everything except the last piece: no report may be produced.
        for piece in pieces.iter().take(pieces.len() - 1) {
            let lan_mouse_proto::ProtoEvent::PeerInfoChunk { index, total, data } = piece else {
                panic!("expected a chunk");
            };
            assert_eq!(r.push(*index, *total, data), None);
        }
    }

    /// UDP reorders and drops, so the reassembler must not depend on arrival order.
    ///
    /// A report is forty datagrams. The first version required `index == next`
    /// exactly and threw the whole report away on a single reordered packet, which is
    /// why the picture arrived only sometimes - and never, on a busy link.
    #[test]
    fn chunks_may_arrive_in_any_order() {
        let _exclusive = exclusive();
        let mut info = PeerInfo::new(["linux", "CachyOS", "", "x86_64", "host", "", ""]);
        info.set_displays("DP-2:2560,0:1440x2560:normal:1;DP-3:0,1120:2560x1440:normal:1");
        info.set_world(
            "1999999999999|cachyos|1|0,1120,2560,1440;2560,0,1440,2560|0|0,-1500,2560,1440",
        );

        let pieces = chunks(&info);
        assert!(pieces.len() > 10, "the report should span many datagrams");
        let mut r = Reassembler::default();
        let mut out = None;
        /* reversed, which is the worst case a real network can produce */
        for piece in pieces.iter().rev() {
            let lan_mouse_proto::ProtoEvent::PeerInfoChunk { index, total, data } = piece else {
                panic!("expected a chunk");
            };
            out = r.push(*index, *total, data).or(out);
        }
        let out = out.expect("all chunks arrived");
        assert_eq!(out.displays(), info.displays());
        assert_eq!(out.world(), info.world());
    }

    #[test]
    fn a_lost_chunk_does_not_corrupt_the_next_report() {
        let _exclusive = exclusive();
        let info = PeerInfo::new(["linux", "", "", "", "host", "", ""]);
        let pieces = chunks(&info);

        let mut r = Reassembler::default();
        /* drop one piece of the first report */
        for (i, piece) in pieces.iter().enumerate() {
            if i == 3 {
                continue;
            }
            let lan_mouse_proto::ProtoEvent::PeerInfoChunk { index, total, data } = piece else {
                panic!("expected a chunk");
            };
            assert_eq!(r.push(*index, *total, data), None);
        }
        /* the next report still assembles */
        let mut out = None;
        for piece in &pieces {
            let lan_mouse_proto::ProtoEvent::PeerInfoChunk { index, total, data } = piece else {
                panic!("expected a chunk");
            };
            out = r.push(*index, *total, data).or(out);
        }
        assert_eq!(out.expect("second report").get(info_slot::HOSTNAME), "host");
    }

    #[test]
    fn long_values_are_truncated_not_corrupted() {
        let _exclusive = exclusive();
        let long = "x".repeat(500);
        let info = PeerInfo::new([&long, "", "", "", "", "", ""]);
        let got = info.get(info_slot::OS);
        assert_eq!(got.len(), lan_mouse_proto::PEER_INFO_FIELD - 1);
        assert!(got.chars().all(|c| c == 'x'));
    }
}
