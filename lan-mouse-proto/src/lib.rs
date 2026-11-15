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
pub const PEER_INFO_FIELDS: usize = 8;

/// Bytes of a [`ProtoEvent::PeerInfo`] carried per chunk.
///
/// Measured on a Linux↔macOS pair: the listening side's replies larger than a
/// few bytes never reach the connecting side, while its 2-byte `Pong` and
/// 9-byte `Hello` do. 16 keeps a chunk at 19 bytes, inside that window, and the
/// 257-byte report needs 17 of them.
pub const PEER_INFO_CHUNK: usize = 16;

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
    Enter(Position),
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
}

impl PeerInfo {
    /// Build from owned strings, truncating anything too long.
    pub fn new(fields: [&str; PEER_INFO_FIELDS]) -> Self {
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
            ProtoEvent::Enter(s) => write!(f, "Enter({s})"),
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
    KeyboardKey,
    KeyboardModifiers,
    Ping,
    Pong,
    Enter,
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
                },
                InputEvent::Keyboard(k) => match k {
                    KeyboardEvent::Key { .. } => EventType::KeyboardKey,
                    KeyboardEvent::Modifiers { .. } => EventType::KeyboardModifiers,
                },
            },
            ProtoEvent::Ping => EventType::Ping,
            ProtoEvent::Pong(_) => EventType::Pong,
            ProtoEvent::Enter(_) => EventType::Enter,
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
            EventType::Enter => Ok(Self::Enter(decode_u8(&mut buf)?.try_into()?)),
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
                ProtoEvent::Enter(pos) => encode_u8(buf, len, pos as u8),
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
