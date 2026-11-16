use std::fmt::{self, Display};

pub mod arrangement;
pub mod display;
pub mod edge;
pub mod error;
pub mod scancode;

// The edge a client is reached through travels with the event types: capture
// backends, the daemon and the frontends all need it, so it is not buried in
// the module.
pub use arrangement::{ArrangedMachine, Arrangement};
pub use display::{DisplayInfo, Layout, Rotation, ScreenRect, Side};
pub use edge::EdgeRange;

#[cfg(all(unix, feature = "libei", not(target_os = "macos")))]
mod libei;

// FIXME
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_RIGHT: u32 = 0x111;
pub const BTN_MIDDLE: u32 = 0x112;
pub const BTN_BACK: u32 = 0x113;
pub const BTN_FORWARD: u32 = 0x114;

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum PointerEvent {
    /// relative motion event
    Motion { time: u32, dx: f64, dy: f64 },
    /// mouse button event
    Button { time: u32, button: u32, state: u32 },
    /// axis event, scroll event for touchpads
    Axis { time: u32, axis: u8, value: f64 },
    /// discrete axis event, scroll event for mice - 120 = one scroll tick
    AxisDiscrete120 { axis: u8, value: i32 },
    /// the scroll gesture ended: the receiving compositor must stop any
    /// kinetic scrolling it started for this axis
    AxisStop { time: u32, axis: u8 },
    /// the scroll gesture was cancelled rather than finished
    AxisCancel { time: u32, axis: u8 },
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum KeyboardEvent {
    /// a key press / release event
    Key { time: u32, key: u32, state: u8 },
    /// modifiers changed state
    Modifiers {
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    },
}

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum Event {
    /// pointer event (motion / button / axis)
    Pointer(PointerEvent),
    /// keyboard events (key / modifiers)
    Keyboard(KeyboardEvent),
}

impl Display for PointerEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PointerEvent::Motion { time: _, dx, dy } => write!(f, "motion({dx},{dy})"),
            PointerEvent::Button {
                time: _,
                button,
                state,
            } => {
                let str = match *button {
                    BTN_LEFT => Some("left"),
                    BTN_RIGHT => Some("right"),
                    BTN_MIDDLE => Some("middle"),
                    BTN_FORWARD => Some("forward"),
                    BTN_BACK => Some("back"),
                    _ => None,
                };
                if let Some(button) = str {
                    write!(f, "button({button}, {state})")
                } else {
                    // the unknown button is printed instead of a name
                    write!(f, "button({button}, {state})")
                }
            }
            PointerEvent::Axis {
                time: _,
                axis,
                value,
            } => write!(f, "scroll({axis}, {value})"),
            PointerEvent::AxisDiscrete120 { axis, value } => {
                write!(f, "scroll-120 ({axis}, {value})")
            }
            PointerEvent::AxisStop { time: _, axis } => write!(f, "scroll-stop ({axis})"),
            PointerEvent::AxisCancel { time: _, axis } => write!(f, "scroll-cancel ({axis})"),
        }
    }
}

impl Display for KeyboardEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyboardEvent::Key {
                time: _,
                key,
                state,
            } => {
                let scan = scancode::Linux::try_from(*key);
                if let Ok(scan) = scan {
                    write!(f, "key({scan:?}, {state})")
                } else {
                    write!(f, "key({key}, {state})")
                }
            }
            KeyboardEvent::Modifiers {
                depressed: mods_depressed,
                latched: mods_latched,
                locked: mods_locked,
                group,
            } => write!(
                f,
                "modifiers({mods_depressed},{mods_latched},{mods_locked},{group})"
            ),
        }
    }
}

impl Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Pointer(p) => write!(f, "{p}"),
            Event::Keyboard(k) => write!(f, "{k}"),
        }
    }
}
