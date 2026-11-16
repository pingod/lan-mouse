use input_event::{Event as InputEvent, KeyboardEvent, PointerEvent};
use num_enum::{IntoPrimitive, TryFromPrimitive, TryFromPrimitiveError};
use paste::paste;
use std::{
    fmt::{Debug, Display, Formatter},
    mem::size_of,
};
use thiserror::Error;

/// Longest string carried in a [`ProtoEvent::PeerInfo`] field.
///
/// Long enough for a kernel release (`6.12.0-1-cachyos`), a machine model
/// (`MacBookPro18,3`) and a distro description, without letting a peer send an
/// unbounded payload.
/// Width of one string slot in a [`ProtoEvent::PeerInfo`].
///
/// 32 fits a kernel release or a hostname. The whole event is 257 bytes, which
/// the connecting side delivers reliably; the listening side's push path drops
/// anything larger than a few bytes, so its report only arrives when the
/// connecting side asks for it (see `PeerInfoRequest`).
pub const PEER_INFO_FIELD: usize = 32;

/// Number of fixed-width string slots in a [`ProtoEvent::PeerInfo`].
/// How many slots a report has.
///
/// More than the eight fixed properties, because the display list has to fit
/// somewhere and it does not fit in one slot: two monitors are around 60 bytes
/// and `PEER_INFO_FIELD` is 32. It spans `DISPLAYS..PEER_INFO_FIELDS` and is
/// reassembled on the other side.
pub const PEER_INFO_FIELDS: usize = 20;

/// Bytes of a [`ProtoEvent::PeerInfo`] carried per chunk.
///
/// Measured on a Linux↔macOS pair: the listening side's replies larger than a
/// few bytes never reach the connecting side, while its 2-byte `Pong` and
/// 9-byte `Hello` do. 16 keeps a chunk at 19 bytes, inside that window, and the
/// 257-byte report needs 17 of them.
/// How many bytes of a report travel in one datagram.
///
/// Larger is better here: a report is one datagram per chunk, and every datagram is a
/// chance for the report to be lost for good. At 16 bytes a two-monitor desk needed
/// forty of them; at 64 it needs a handful, and 64 is far below any path MTU.
pub const PEER_INFO_CHUNK: usize = 64;

/// defines the maximum size an encoded event can take up
///
/// The largest event is `PeerInfo`: a type byte followed by
/// `PEER_INFO_FIELDS` NUL-padded slots. Pointer motion is smaller, so this
/// bound covers every variant.
pub const MAX_EVENT_SIZE: usize = size_of::<u8>() + PEER_INFO_FIELDS * PEER_INFO_FIELD;

/// error type for protocol violations
#[derive(Debug, Error)]
pub enum ProtocolError {
    /// event type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidEventId(#[from] TryFromPrimitiveError<EventType>),
    /// position type does not exist
    #[error("invalid event id: `{0}`")]
    InvalidPosition(#[from] TryFromPrimitiveError<Position>),
    /// the datagram was shorter than the event it claims to encode
    #[error("truncated event: needed {needed} more byte(s) than the {len} byte(s) that were sent")]
    Truncated { needed: usize, len: usize },
}

/// Position of a client
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum Position {
    Left,
    Right,
    Top,
    Bottom,
}

impl Display for Position {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let pos = match self {
            Position::Left => "left",
            Position::Right => "right",
            Position::Top => "top",
            Position::Bottom => "bottom",
        };
        write!(f, "{pos}")
    }
}

/// main lan-mouse protocol event type
#[derive(Clone, Copy, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ProtoEvent {
    /// notify a client that the cursor entered its region at the given position
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    /// The cursor entered this device through the given edge.
    ///
    /// The payload is where it crossed that edge as a fraction (0..=1)
    /// counted from its left or top end, so the receiving device can put its
    /// cursor at the matching point instead of wherever it happened to be.
    Enter(Position, f64),
    /// Put the cursor in the middle of the screen.
    ///
    /// Sent instead of a plain [`ProtoEvent::Enter`] when the pointer is handed
    /// over on request (a "switch device" click) rather than by crossing an
    /// edge: there is no crossing point to line up with, and parking the peer's
    /// cursor in the middle is what the person pressing the button expects.
    /// The receiver warps its cached cursor but does not take over input, so
    /// this must be followed by an [`ProtoEvent::Enter`].
    CenterCursor,
    /// notify a client that the cursor left its region
    /// [`ProtoEvent::Ack`] with the same serial is used for synchronization between devices
    Leave(u32),
    /// acknowledge of an [`ProtoEvent::Enter`] or [`ProtoEvent::Leave`] event
    Ack(u32),
    /// Input event
    Input(InputEvent),
    /// Ping event for tracking unresponsive clients.
    /// A client has to respond with [`ProtoEvent::Pong`].
    Ping,
    /// Response to [`ProtoEvent::Ping`], true if emulation is enabled / available
    Pong(bool),
    /// Build identification for the sending peer. Sent by the
    /// connect side once after the connection authenticates, and
    /// echoed back by the listen side in reply, so each end can
    /// display the peer's build hash and warn (soft) on mismatch.
    /// `commit` is the 8-byte ASCII short commit hash from
    /// `shadow_rs`'s `SHORT_COMMIT`. Old peers that don't
    /// recognize the event type silently skip it per the
    /// forward-compat handling in the receive loop.
    Hello { commit: [u8; 8] },
    /// Descriptive information about the sending machine.
    ///
    /// Sent once after the handshake. Older peers skip unknown event types, so
    /// a peer that does not understand this still works.
    ///
    /// The payload is ~512 bytes, larger than every other variant. It stays
    /// inline to keep `ProtoEvent` `Copy`: it is constructed once per
    /// connection and moved through channels, never stored in bulk.
    PeerInfo(PeerInfo),
    /// One slice of a [`ProtoEvent::PeerInfo`].
    ///
    /// The listening side cannot push a large record to the connecting side, so
    /// a report is split into `PEER_INFO_CHUNK`-sized pieces that each fit.
    /// `index` counts from zero; a receiver reassembles until `total` pieces
    /// have arrived.
    PeerInfoChunk {
        index: u8,
        total: u8,
        data: [u8; PEER_INFO_CHUNK],
    },
    /// Ask the peer to send its [`ProtoEvent::PeerInfo`].
    ///
    /// Needed because the pushing side may be a receive-only peer whose reply
    /// path is not established: the connecting side can always send, so it
    /// asks instead of waiting to be told.
    PeerInfoRequest,
}

/// One fixed-width string slot, NUL-padded.
pub type InfoField = [u8; PEER_INFO_FIELD];

/// What a machine reports about itself.
///
/// Fixed-width slots keep the event inside the protocol's fixed-size buffer and
/// keep `ProtoEvent` `Copy`. An all-zero slot means "not known".
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PeerInfo {
    pub fields: [InfoField; PEER_INFO_FIELDS],
}

/// Slot indices, so call sites do not rely on magic numbers.
pub mod info_slot {
    pub const OS: usize = 0;
    pub const OS_VERSION: usize = 1;
    pub const KERNEL: usize = 2;
    pub const ARCH: usize = 3;
    pub const HOSTNAME: usize = 4;
    pub const APP_VERSION: usize = 5;
    pub const COMMIT: usize = 6;
    pub const DISPLAYS: usize = 7;
    /// first slot of the arrangement, see [`super::WORLD_SLOTS`]
    pub const WORLD: usize = 12;
}

/// The slots the display list may occupy.
///
/// A report is a fixed-width record, so the list has a hard ceiling: five slots of
/// 32 bytes hold about seven monitors with connector names. That is enough for a
/// desk and far more than the one slot it used to have, which truncated every
/// report past the first monitor and is why two machines drew each other as a
/// single screen.
pub const DISPLAY_SLOTS: usize = info_slot::WORLD - info_slot::DISPLAYS;

/// The slot where the arrangement starts, and how many it spans.
///
/// The arrangement is one picture shared by both machines: whichever side last
/// edited it serialises the screen rectangles and sends them, so the other side
/// draws those rectangles rather than deriving its own from its own config. Two
/// derivations can always disagree; one shipped picture cannot.
pub const WORLD_START: usize = info_slot::WORLD;
pub const WORLD_SLOTS: usize = PEER_INFO_FIELDS - info_slot::WORLD;

impl PeerInfo {
    /// Build from the fixed properties, truncating anything too long.
    ///
    /// The display list is not one of them: it does not fit in a slot, so it is
    /// written separately by [`PeerInfo::set_displays`].
    pub fn new(fields: [&str; info_slot::DISPLAYS]) -> Self {
        let mut out = [[0u8; PEER_INFO_FIELD]; PEER_INFO_FIELDS];
        for (slot, value) in out.iter_mut().zip(fields.iter()) {
            let bytes = value.as_bytes();
            let n = bytes.len().min(PEER_INFO_FIELD - 1);
            slot[..n].copy_from_slice(&bytes[..n]);
        }
        Self { fields: out }
    }

    /// Read slot `idx`, or `""` when unset.
    pub fn get(&self, idx: usize) -> &str {
        let slot = &self.fields[idx];
        let end = slot.iter().position(|b| *b == 0).unwrap_or(slot.len());
        std::str::from_utf8(&slot[..end]).unwrap_or("")
    }

    /// Write the arrangement across the slots reserved for it.
    pub fn set_world(&mut self, world: &str) {
        self.write_span(info_slot::WORLD, world);
    }

    /// The arrangement the peer is drawing, or `""`.
    pub fn world(&self) -> String {
        self.read_span(info_slot::WORLD)
    }

    /// Write a string across every slot from `start` to the end.
    fn write_span(&mut self, start: usize, value: &str) {
        let per_slot = PEER_INFO_FIELD - 1;
        for (index, slot) in self.fields[start..].iter_mut().enumerate() {
            let begin = index * per_slot;
            let end = (begin + per_slot).min(value.len());
            *slot = [0; PEER_INFO_FIELD];
            if begin < value.len() {
                slot[..end - begin].copy_from_slice(&value.as_bytes()[begin..end]);
            }
        }
    }

    /// Read a string written by [`PeerInfo::write_span`].
    fn read_span(&self, start: usize) -> String {
        let mut out = String::new();
        for slot in &self.fields[start..] {
            let end = slot.iter().position(|b| *b == 0).unwrap_or(slot.len());
            out.push_str(std::str::from_utf8(&slot[..end]).unwrap_or(""));
        }
        out
    }

    /// Write the display list across the slots reserved for it.
    ///
    /// Every slot keeps one NUL so it stays a valid string when read on its own,
    /// which is what the chunking and the debug output assume.
    pub fn set_displays(&mut self, displays: &str) {
        let per_slot = PEER_INFO_FIELD - 1;
        for (index, slot) in self
            .fields
            .iter_mut()
            .take(info_slot::WORLD)
            .skip(info_slot::DISPLAYS)
            .enumerate()
        {
            let start = index * per_slot;
            let end = (start + per_slot).min(displays.len());
            *slot = [0; PEER_INFO_FIELD];
            if start < displays.len() {
                slot[..end - start].copy_from_slice(&displays.as_bytes()[start..end]);
            }
        }
    }

    /// The display list, reassembled from the slots it spans.
    ///
    /// The list is a string, so it can be longer than one slot; a report that was
    /// cut at the first slot is exactly the bug this fixes, and it is invisible
    /// from here because a truncated list is still a valid list of one monitor.
    pub fn displays(&self) -> String {
        let mut out = String::new();
        for slot in self
            .fields
            .iter()
            .take(info_slot::WORLD)
            .skip(info_slot::DISPLAYS)
        {
            let end = slot.iter().position(|b| *b == 0).unwrap_or(slot.len());
            out.push_str(std::str::from_utf8(&slot[..end]).unwrap_or(""));
        }
        out
    }
}

impl Default for PeerInfo {
    fn default() -> Self {
        Self {
            fields: [[0u8; PEER_INFO_FIELD]; PEER_INFO_FIELDS],
        }
    }
}

impl Debug for PeerInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PeerInfo({} {} {} {} host={} v{} {})",
            self.get(info_slot::OS),
            self.get(info_slot::OS_VERSION),
            self.get(info_slot::KERNEL),
            self.get(info_slot::ARCH),
            self.get(info_slot::HOSTNAME),
            self.get(info_slot::APP_VERSION),
            self.get(info_slot::COMMIT)
        )
    }
}

impl Display for ProtoEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoEvent::Enter(s, fraction) => {
                write!(f, "Enter({s}, {:.0}% of the edge)", fraction * 100.0)
            }
            ProtoEvent::CenterCursor => write!(f, "CenterCursor"),
            ProtoEvent::Leave(s) => write!(f, "Leave({s})"),
            ProtoEvent::Ack(s) => write!(f, "Ack({s})"),
            ProtoEvent::Input(e) => write!(f, "{e}"),
            ProtoEvent::Ping => write!(f, "ping"),
            ProtoEvent::Pong(alive) => {
                write!(
                    f,
                    "pong: {}",
                    if *alive { "alive" } else { "not available" }
                )
            }
            ProtoEvent::Hello { commit } => {
                let s = std::str::from_utf8(commit).unwrap_or("????????");
                write!(f, "Hello({s})")
            }
            ProtoEvent::PeerInfo(info) => write!(f, "{info:?}"),
            ProtoEvent::PeerInfoRequest => write!(f, "PeerInfoRequest"),
            ProtoEvent::PeerInfoChunk { index, total, .. } => {
                write!(f, "PeerInfoChunk({}/{total})", *index as u32 + 1)
            }
        }
    }
}

#[derive(TryFromPrimitive, IntoPrimitive)]
#[repr(u8)]
pub enum EventType {
    PointerMotion,
    PointerButton,
    PointerAxis,
    PointerAxisValue120,
    PointerAxisStop,
    PointerAxisCancel,
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
    CenterCursor,
    Leave,
    Ack,
    Hello,
    PeerInfo,
    PeerInfoRequest,
    PeerInfoChunk,
}

impl ProtoEvent {
    fn event_type(&self) -> EventType {
        match self {
            ProtoEvent::Input(e) => match e {
                InputEvent::Pointer(p) => match p {
                    PointerEvent::Motion { .. } => EventType::PointerMotion,
                    PointerEvent::Button { .. } => EventType::PointerButton,
                    PointerEvent::Axis { .. } => EventType::PointerAxis,
                    PointerEvent::AxisDiscrete120 { .. } => EventType::PointerAxisValue120,
                    PointerEvent::AxisStop { .. } => EventType::PointerAxisStop,
                    PointerEvent::AxisCancel { .. } => EventType::PointerAxisCancel,
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(..) => EventType::Enter,
            ProtoEvent::CenterCursor => EventType::CenterCursor,
            ProtoEvent::Leave(_) => EventType::Leave,
            ProtoEvent::Ack(_) => EventType::Ack,
            ProtoEvent::Hello { .. } => EventType::Hello,
            ProtoEvent::PeerInfo(_) => EventType::PeerInfo,
            ProtoEvent::PeerInfoRequest => EventType::PeerInfoRequest,
            ProtoEvent::PeerInfoChunk { .. } => EventType::PeerInfoChunk,
        }
    }
}

impl TryFrom<&[u8]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: &[u8]) -> Result<Self, Self::Error> {
        let mut buf = buf;
        let event_type = decode_u8(&mut buf)?;
        match EventType::try_from(event_type)? {
            EventType::PointerMotion => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Motion {
                    time: decode_u32(&mut buf)?,
                    dx: decode_f64(&mut buf)?,
                    dy: decode_f64(&mut buf)?,
                })))
            }
            EventType::PointerButton => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::Button {
                    time: decode_u32(&mut buf)?,
                    button: decode_u32(&mut buf)?,
                    state: decode_u32(&mut buf)?,
                })))
            }
            EventType::PointerAxis => Ok(Self::Input(InputEvent::Pointer(PointerEvent::Axis {
                time: decode_u32(&mut buf)?,
                axis: decode_u8(&mut buf)?,
                value: decode_f64(&mut buf)?,
            }))),
            EventType::PointerAxisValue120 => Ok(Self::Input(InputEvent::Pointer(
                PointerEvent::AxisDiscrete120 {
                    axis: decode_u8(&mut buf)?,
                    value: decode_i32(&mut buf)?,
                },
            ))),
            EventType::PointerAxisStop => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::AxisStop {
                    time: decode_u32(&mut buf)?,
                    axis: decode_u8(&mut buf)?,
                })))
            }
            EventType::PointerAxisCancel => {
                Ok(Self::Input(InputEvent::Pointer(PointerEvent::AxisCancel {
                    time: decode_u32(&mut buf)?,
                    axis: decode_u8(&mut buf)?,
                })))
            }
            EventType::KeyboardKey => Ok(Self::Input(InputEvent::Keyboard(KeyboardEvent::Key {
                time: decode_u32(&mut buf)?,
                key: decode_u32(&mut buf)?,
                state: decode_u8(&mut buf)?,
            }))),
            EventType::KeyboardModifiers => Ok(Self::Input(InputEvent::Keyboard(
                KeyboardEvent::Modifiers {
                    depressed: decode_u32(&mut buf)?,
                    latched: decode_u32(&mut buf)?,
                    locked: decode_u32(&mut buf)?,
                    group: decode_u32(&mut buf)?,
                },
            ))),
            EventType::Ping => Ok(Self::Ping),
            EventType::Pong => Ok(Self::Pong(decode_u8(&mut buf)? != 0)),
            EventType::Enter => {
                let position = decode_u8(&mut buf)?.try_into()?;
                // Older peers end the event after the position; the crossing
                // fraction was added later, so treat a short payload as
                // "unknown" instead of rejecting the whole event.
                let fraction = if buf.len() >= size_of::<f64>() {
                    decode_f64(&mut buf)?
                } else {
                    0.5
                };
                Ok(Self::Enter(position, fraction))
            }
            EventType::CenterCursor => Ok(Self::CenterCursor),
            EventType::Leave => Ok(Self::Leave(decode_u32(&mut buf)?)),
            EventType::Ack => Ok(Self::Ack(decode_u32(&mut buf)?)),
            EventType::PeerInfoRequest => Ok(Self::PeerInfoRequest),
            EventType::PeerInfoChunk => {
                let index = decode_u8(&mut buf)?;
                let total = decode_u8(&mut buf)?;
                let mut data = [0u8; PEER_INFO_CHUNK];
                for b in data.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::PeerInfoChunk { index, total, data })
            }
            EventType::PeerInfo => {
                let mut fields = [[0u8; PEER_INFO_FIELD]; PEER_INFO_FIELDS];
                for slot in fields.iter_mut() {
                    for b in slot.iter_mut() {
                        *b = decode_u8(&mut buf)?;
                    }
                }
                Ok(Self::PeerInfo(PeerInfo { fields }))
            }
            EventType::Hello => {
                let mut commit = [0u8; 8];
                for b in commit.iter_mut() {
                    *b = decode_u8(&mut buf)?;
                }
                Ok(Self::Hello { commit })
            }
        }
    }
}

impl ProtoEvent {
    /// Decode an event from a datagram of `buf.len()` bytes.
    ///
    /// Unlike [`TryFrom<[u8; MAX_EVENT_SIZE]>`] this never inspects trailing
    /// zeroes of an oversized buffer and reports a truncated datagram instead
    /// of panicking.
    pub fn decode(buf: &[u8]) -> Result<Self, ProtocolError> {
        Self::try_from(buf)
    }
}

impl TryFrom<[u8; MAX_EVENT_SIZE]> for ProtoEvent {
    type Error = ProtocolError;

    fn try_from(buf: [u8; MAX_EVENT_SIZE]) -> Result<Self, Self::Error> {
        Self::decode(&buf[..])
    }
}

impl From<ProtoEvent> for ([u8; MAX_EVENT_SIZE], usize) {
    fn from(event: ProtoEvent) -> Self {
        let mut buf = [0u8; MAX_EVENT_SIZE];
        let mut len = 0usize;
        {
            let mut buf = &mut buf[..];
            let buf = &mut buf;
            let len = &mut len;
            encode_u8(buf, len, event.event_type() as u8);
            match event {
                ProtoEvent::Input(event) => match event {
                    InputEvent::Pointer(p) => match p {
                        PointerEvent::Motion { time, dx, dy } => {
                            encode_u32(buf, len, time);
                            encode_f64(buf, len, dx);
                            encode_f64(buf, len, dy);
                        }
                        PointerEvent::Button {
                            time,
                            button,
                            state,
                        } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, button);
                            encode_u32(buf, len, state);
                        }
                        PointerEvent::Axis { time, axis, value } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                            encode_f64(buf, len, value);
                        }
                        PointerEvent::AxisDiscrete120 { axis, value } => {
                            encode_u8(buf, len, axis);
                            encode_i32(buf, len, value);
                        }
                        PointerEvent::AxisStop { time, axis } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                        }
                        PointerEvent::AxisCancel { time, axis } => {
                            encode_u32(buf, len, time);
                            encode_u8(buf, len, axis);
                        }
                    },
                    InputEvent::Keyboard(k) => match k {
                        KeyboardEvent::Key { time, key, state } => {
                            encode_u32(buf, len, time);
                            encode_u32(buf, len, key);
                            encode_u8(buf, len, state);
                        }
                        KeyboardEvent::Modifiers {
                            depressed,
                            latched,
                            locked,
                            group,
                        } => {
                            encode_u32(buf, len, depressed);
                            encode_u32(buf, len, latched);
                            encode_u32(buf, len, locked);
                            encode_u32(buf, len, group);
                        }
                    },
                },
                ProtoEvent::Ping => {}
                ProtoEvent::Pong(alive) => encode_u8(buf, len, alive as u8),
                ProtoEvent::Enter(pos, fraction) => {
                    encode_u8(buf, len, pos as u8);
                    encode_f64(buf, len, fraction);
                }
                ProtoEvent::CenterCursor => {}
                ProtoEvent::Leave(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Ack(serial) => encode_u32(buf, len, serial),
                ProtoEvent::Hello { commit } => {
                    for b in commit.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
                ProtoEvent::PeerInfoRequest => {}
                ProtoEvent::PeerInfoChunk { index, total, data } => {
                    encode_u8(buf, len, index);
                    encode_u8(buf, len, total);
                    for b in data.iter() {
                        encode_u8(buf, len, *b);
                    }
                }
                ProtoEvent::PeerInfo(info) => {
                    for slot in info.fields.iter() {
                        for b in slot.iter() {
                            encode_u8(buf, len, *b);
                        }
                    }
                }
            }
        }
        (buf, len)
    }
}

macro_rules! decode_impl {
    ($t:ty) => {
        paste! {
            fn [<decode_ $t>](data: &mut &[u8]) -> Result<$t, ProtocolError> {
                /* `split_at` panics when the datagram is shorter than the
                 * event it claims to encode, which a peer can trigger with a
                 * single short UDP payload. Report it as a protocol error
                 * instead of aborting the process.
                 */
                let Some((int_bytes, rest)) = data.split_at_checked(size_of::<$t>()) else {
                    return Err(ProtocolError::Truncated {
                        needed: size_of::<$t>(),
                        len: data.len(),
                    });
                };
                *data = rest;
                Ok($t::from_be_bytes(int_bytes.try_into().unwrap()))
            }
        }
    };
}

decode_impl!(u8);
decode_impl!(u32);
decode_impl!(i32);
decode_impl!(f64);

macro_rules! encode_impl {
    ($t:ty) => {
        paste! {
            fn [<encode_ $t>](buf: &mut &mut [u8], amt: &mut usize, n: $t) {
                let src = n.to_be_bytes();
                let data = std::mem::take(buf);
                let (int_bytes, rest) = data.split_at_mut(size_of::<$t>());
                int_bytes.copy_from_slice(&src);
                *amt += size_of::<$t>();
                *buf = rest
            }
        }
    };
}

encode_impl!(u8);
encode_impl!(u32);
encode_impl!(i32);
encode_impl!(f64);

#[cfg(test)]
mod scroll_stop_tests {
    use super::*;

    #[test]
    fn scroll_stop_round_trips() {
        let event = ProtoEvent::Input(InputEvent::Pointer(PointerEvent::AxisStop {
            time: 1_234,
            axis: 1,
        }));
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        match ProtoEvent::decode(&buf[..len]).unwrap() {
            ProtoEvent::Input(InputEvent::Pointer(PointerEvent::AxisStop { time, axis })) => {
                assert_eq!(time, 1_234);
                assert_eq!(axis, 1);
            }
            other => panic!("expected AxisStop, got {other:?}"),
        }
    }

    #[test]
    fn scroll_cancel_is_not_a_stop() {
        let event = ProtoEvent::Input(InputEvent::Pointer(PointerEvent::AxisCancel {
            time: 7,
            axis: 0,
        }));
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = event.into();
        let decoded = ProtoEvent::decode(&buf[..len]).unwrap();
        assert!(
            matches!(
                decoded,
                ProtoEvent::Input(InputEvent::Pointer(PointerEvent::AxisCancel { .. }))
            ),
            "a cancelled gesture must stay distinguishable from a stopped one"
        );
    }
}

#[cfg(test)]
mod center_cursor_tests {
    use super::*;

    #[test]
    fn center_cursor_round_trips() {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::CenterCursor.into();
        /* a single tag byte, no payload: the receiver must not need more */
        assert_eq!(len, 1);
        let decoded = ProtoEvent::decode(&buf[..len]).unwrap();
        assert!(matches!(decoded, ProtoEvent::CenterCursor));
    }

    #[test]
    fn enter_and_center_cursor_stay_distinguishable() {
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) =
            ProtoEvent::Enter(Position::Bottom, 0.25).into();
        match ProtoEvent::decode(&buf[..len]).unwrap() {
            ProtoEvent::Enter(pos, fraction) => {
                assert!(matches!(pos, Position::Bottom));
                assert!((fraction - 0.25).abs() < f64::EPSILON);
            }
            other => panic!("expected Enter, got {other:?}"),
        }
        let (buf, len): ([u8; MAX_EVENT_SIZE], usize) = ProtoEvent::CenterCursor.into();
        let decoded = ProtoEvent::decode(&buf[..len]).unwrap();
        assert!(
            !matches!(decoded, ProtoEvent::Enter(..)),
            "CenterCursor must not decode as Enter"
        );
    }
}
