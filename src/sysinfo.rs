//! Collect what this machine reports about itself to peers.
//!
//! The values are gathered once at startup: they describe hardware and OS
//! install state, which do not change while the process runs, and reading them
//! per handshake would shell out repeatedly for no benefit.

use std::sync::LazyLock;

use lan_mouse_proto::{PeerInfo, info_slot};

/// This machine's description, built once.
pub static LOCAL: LazyLock<PeerInfo> = LazyLock::new(build);

fn build() -> PeerInfo {
    // `PeerInfo::new` takes `&str`; the owned values live until the call
    // returns, so bind them first.
    let os_version = os_version();
    let kernel = kernel_version();
    let hostname = hostname();
    let displays = displays();
    let commit = short_commit();
    PeerInfo::new([
        os_name(),
        &os_version,
        &kernel,
        std::env::consts::ARCH,
        &hostname,
        env!("CARGO_PKG_VERSION"),
        &commit,
        &displays,
    ])
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

/// Display geometry summary.
///
/// Only a size and count: enough for a peer to sanity-check that both sides
/// agree on the layout, without shipping a per-display list over a fixed-size
/// event.
fn displays() -> String {
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
        displays: info.get(info_slot::DISPLAYS).to_owned(),
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
#[derive(Default)]
pub struct Reassembler {
    buf: Vec<u8>,
    total: Option<u8>,
    next: u8,
}

impl Reassembler {
    /// Feed one chunk; returns the report once every piece has arrived.
    pub fn push(
        &mut self,
        index: u8,
        total: u8,
        data: &[u8; lan_mouse_proto::PEER_INFO_CHUNK],
    ) -> Option<PeerInfo> {
        use lan_mouse_proto::{PEER_INFO_CHUNK, PEER_INFO_FIELD, PEER_INFO_FIELDS};

        // A new report starts at index 0; anything else is a stray piece from a
        // previous one and would corrupt the buffer.
        if index == 0 {
            self.buf.clear();
            self.next = 0;
            self.total = Some(total);
        }
        if self.total != Some(total) || index != self.next {
            return None;
        }
        self.buf.extend_from_slice(data);
        self.next += 1;

        if self.next < total {
            return None;
        }
        let expected = PEER_INFO_FIELDS * PEER_INFO_FIELD;
        if self.buf.len() < expected {
            return None;
        }
        let mut fields = [[0u8; PEER_INFO_FIELD]; PEER_INFO_FIELDS];
        for (slot, out) in fields.iter_mut().zip(self.buf.chunks(PEER_INFO_FIELD)) {
            let n = out.len().min(PEER_INFO_FIELD);
            slot[..n].copy_from_slice(&out[..n]);
        }
        let _ = PEER_INFO_CHUNK;
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

    #[test]
    fn local_info_is_populated() {
        let info = &*LOCAL;
        // Architecture and app version are compile-time constants and must
        // always be present; OS detection always resolves to something.
        assert_eq!(info.get(info_slot::ARCH), std::env::consts::ARCH);
        assert_eq!(info.get(info_slot::APP_VERSION), env!("CARGO_PKG_VERSION"));
        assert!(!info.get(info_slot::OS).is_empty());
    }

    #[test]
    fn chunks_reassemble_to_the_original() {
        let info = PeerInfo::new([
            "linux",
            "CachyOS",
            "7.3.4-1-cachyos",
            "x86_64",
            "cachyos-x8664",
            "0.11.0",
            "abc12345",
            "2 displays",
        ]);
        let pieces = chunks(&info);
        assert!(pieces.len() > 1, "a 257-byte report must span chunks");

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
    }

    #[test]
    fn a_missing_chunk_yields_nothing() {
        let info = PeerInfo::new(["a", "b", "c", "d", "e", "f", "g", "h"]);
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

    #[test]
    fn long_values_are_truncated_not_corrupted() {
        let long = "x".repeat(500);
        let info = PeerInfo::new([&long, "", "", "", "", "", "", ""]);
        let got = info.get(info_slot::OS);
        assert_eq!(got.len(), lan_mouse_proto::PEER_INFO_FIELD - 1);
        assert!(got.chars().all(|c| c == 'x'));
    }
}
