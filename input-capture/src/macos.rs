use super::{
    Capture, CaptureError, CaptureEvent, EdgeRange, Position, error::MacosCaptureCreationError,
};
use async_trait::async_trait;
use bitflags::bitflags;
use core_foundation::{
    base::{CFRelease, TCFType, kCFAllocatorDefault},
    date::CFTimeInterval,
    number::{CFBooleanRef, kCFBooleanTrue},
    runloop::{CFRunLoop, CFRunLoopSource, kCFRunLoopCommonModes},
    string::{CFStringCreateWithCString, CFStringRef, kCFStringEncodingUTF8},
};
/// Must match `input-emulation`'s tag. Duplicated instead of shared because the
/// capture and emulation crates are independent; the value is part of the
/// on-wire behaviour between them.
const INJECTED_EVENT_TAG: i64 = 0x6c616e6d; // "lanm"

/// Quartz event type of `NX_SYSDEFINED`, which carries media keys.
///
/// `core-graphics` has no variant for it: media keys are not key events, so
/// the tap has to ask for the raw type and decode the payload itself.
const SYSTEM_DEFINED_EVENT: u32 = 14;

/// `NX_KEYTYPE_*` values a keyboard can send, mapped to the evdev codes the
/// rest of lan-mouse speaks.
fn media_key_evdev(data1: isize) -> Option<(u32, u8)> {
    use input_event::scancode::Linux;

    // data1 = NX_KEYTYPE << 16 | (0x0a on press, 0x0b on release) << 8
    let key_type = (data1 >> 16) as u32 & 0xffff;
    let key = match key_type {
        0 => Linux::KeyVolumeUp,
        1 => Linux::KeyVolumeDown,
        2 => Linux::KeyBrightnessup,
        3 => Linux::KeyBrightnessdown,
        7 => Linux::KeyMute,
        16 => Linux::KeyPlaypause,
        17 => Linux::KeyNextsong,
        18 => Linux::KeyPrevioussong,
        _ => return None,
    };
    let state = if ((data1 >> 8) & 0xff) == 0x0a { 1 } else { 0 };
    Some((key as u32, state))
}

/// Decode a media key from a raw Quartz event.
///
/// The payload lives in `NSEvent.data1`, which Quartz only exposes through
/// AppKit, hence the conversion from the raw event.
fn media_key_event(cg_ev: &CGEvent) -> Option<CaptureEvent> {
    let ns_event = unsafe {
        let event = &*(ForeignType::as_ptr(cg_ev) as *const objc2_core_graphics::CGEvent);
        objc2_app_kit::NSEvent::eventWithCGEvent(event)?
    };
    let data1 = ns_event.data1();
    let (key, state) = media_key_evdev(data1)?;
    log::trace!("media key event: data1 = {data1:#x}, key = {key}, state = {state}");
    Some(CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
        time: 0,
        key,
        state,
    })))
}

/// Whether `event_type` reports pointer movement.
fn is_pointer_motion(event_type: CGEventType) -> bool {
    matches!(
        event_type,
        CGEventType::MouseMoved
            | CGEventType::LeftMouseDragged
            | CGEventType::RightMouseDragged
            | CGEventType::OtherMouseDragged
    )
}

use core_graphics::{
    base::{CGError, kCGErrorSuccess},
    display::{CGDisplay, CGPoint},
    event::{
        CGEvent, CGEventField, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions,
        CGEventTapPlacement, CGEventTapProxy, CGEventType, CallbackResult, EventField,
    },
    event_source::{CGEventSource, CGEventSourceStateID},
};
use foreign_types::ForeignType;
use futures_core::Stream;
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
};
use keycode::{KeyMap, KeyMapping};
use libc::c_void;
use once_cell::unsync::Lazy;
use std::{
    collections::HashMap,
    ffi::{CString, c_char},
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll, ready},
    thread::{self},
};
use tokio::sync::{
    Mutex,
    mpsc::{self, Sender, UnboundedReceiver, UnboundedSender},
    oneshot,
};

#[derive(Debug)]
struct Bounds {
    xmin: f64,
    xmax: f64,
    ymin: f64,
    ymax: f64,
}

impl Default for Bounds {
    fn default() -> Self {
        // Start out inverted so that the first display defines the bounds:
        // seeding with 0.0 would clamp the layout to the origin whenever no
        // display touches (0, 0).
        Self {
            xmin: f64::INFINITY,
            xmax: f64::NEG_INFINITY,
            ymin: f64::INFINITY,
            ymax: f64::NEG_INFINITY,
        }
    }
}

impl Bounds {
    /// whether at least one display contributed to these bounds
    fn is_valid(&self) -> bool {
        self.xmin <= self.xmax && self.ymin <= self.ymax
    }
}

#[derive(Debug)]
struct InputCaptureState {
    /// the segments of each edge a client can be reached through
    ///
    /// The macOS tap owns the local pointer, so unlike the compositor-driven
    /// backends it can decide, per crossing, whether the point the cursor
    /// reached the edge at belongs to a client. That is what makes a stretch of
    /// edge that nobody covers behave like a wall here too.
    active_clients: Lazy<HashMap<Position, Vec<EdgeRange>>>,
    /// the currently entered capture position, if any
    current_pos: Option<Position>,
    /// position where the cursor was captured
    enter_position: Option<CGPoint>,
    /// bounds of the input capture area
    bounds: Bounds,
    /// current state of modifier keys
    modifier_state: XMods,
    /// barrier the cursor was last released from. While the cursor stays
    /// inside the same edge band the latch suppresses a second crossing, so a
    /// peer's injected cursor parked on the return edge cannot retrigger the
    /// barrier on every 1px jitter (that produced an enter/leave fight
    /// between the two machines: the controlled session could not slide back).
    latched_barrier: Option<Position>,
}

#[derive(Debug)]
enum ProducerEvent {
    Release,
    Create(Position, Vec<EdgeRange>),
    Destroy(Position),
    Grab(Position),
    /// Take the pointer on request (a "switch device" click): grab and pin the
    /// hidden cursor to the edge, without a crossing event.
    Take(Position),
    EventTapDisabled,
    DisplayReconfigured,
}

impl InputCaptureState {
    fn new() -> Result<Self, MacosCaptureCreationError> {
        let mut res = Self {
            active_clients: Lazy::new(HashMap::new),
            current_pos: None,
            enter_position: None,
            bounds: Bounds::default(),
            modifier_state: Default::default(),
            latched_barrier: None,
        };
        res.update_bounds()?;
        Ok(res)
    }

    /// Distance (in points) the cursor must move away from an edge before the
    /// same barrier may fire again. Without this hysteresis a cursor sitting
    /// on the boundary re-triggers the barrier on every pixel of jitter.
    const BARRIER_RELEASE_MARGIN: f64 = 24.0;

    fn crossed(&mut self, event: &CGEvent) -> Option<Position> {
        if !self.bounds.is_valid() {
            // no display geometry known yet (no active display)
            return None;
        }

        let location = event.location();
        let relative_x = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X);
        let relative_y = event.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y);

        // Clear the latch once the cursor has moved away from the edge it was
        // released from, so a deliberate second crossing still works.
        if let Some(latched) = self.latched_barrier {
            let away = match latched {
                Position::Left => location.x > self.bounds.xmin + Self::BARRIER_RELEASE_MARGIN,
                Position::Right => location.x < self.bounds.xmax - Self::BARRIER_RELEASE_MARGIN,
                Position::Top => location.y > self.bounds.ymin + Self::BARRIER_RELEASE_MARGIN,
                Position::Bottom => location.y < self.bounds.ymax - Self::BARRIER_RELEASE_MARGIN,
            };
            if away {
                self.latched_barrier = None;
            }
        }

        for (&position, segments) in self.active_clients.iter() {
            if self.latched_barrier == Some(position) {
                continue;
            }
            let crossed = match position {
                Position::Left => (location.x + relative_x) <= self.bounds.xmin,
                Position::Right => (location.x + relative_x) >= self.bounds.xmax,
                Position::Top => (location.y + relative_y) <= self.bounds.ymin,
                Position::Bottom => (location.y + relative_y) >= self.bounds.ymax,
            };
            if !crossed {
                continue;
            }
            /* The tap owns the pointer, so the point of the edge it arrived at is
             * known: a stretch that no client covers is refused here and the
             * cursor simply stops at the border instead of switching. */
            let fraction =
                self.fraction_at(position, location.x + relative_x, location.y + relative_y);
            if segments.iter().any(|segment| segment.contains(fraction)) {
                log::debug!("Crossed barrier into position: {position:?}");
                self.latched_barrier = Some(position);
                return Some(position);
            }
            log::trace!(
                "the {position} edge was crossed at {:.0}% but no client covers it",
                fraction * 100.0
            );
        }
        None
    }

    /// Where along `position` a cursor at (`x`, `y`) crosses, as a fraction.
    fn fraction_at(&self, position: Position, x: f64, y: f64) -> f64 {
        let (lo, hi, value) = match position {
            Position::Left | Position::Right => (self.bounds.ymin, self.bounds.ymax, y),
            Position::Top | Position::Bottom => (self.bounds.xmin, self.bounds.xmax, x),
        };
        let span = hi - lo;
        if span <= 0.0 {
            return 0.5;
        }
        ((value - lo) / span).clamp(0.0, 1.0)
    }

    // Get the max bounds of all displays
    fn update_bounds(&mut self) -> Result<(), MacosCaptureCreationError> {
        let active_ids =
            CGDisplay::active_displays().map_err(MacosCaptureCreationError::ActiveDisplays)?;

        // recompute from scratch, so that disconnected displays do not keep
        // stretching the bounds
        let mut bounds = Bounds::default();
        for d in active_ids {
            let display_bounds = CGDisplay::new(d).bounds();
            bounds.xmin = bounds.xmin.min(display_bounds.origin.x);
            bounds.xmax = bounds
                .xmax
                .max(display_bounds.origin.x + display_bounds.size.width);
            bounds.ymin = bounds.ymin.min(display_bounds.origin.y);
            bounds.ymax = bounds
                .ymax
                .max(display_bounds.origin.y + display_bounds.size.height);
        }

        if !bounds.is_valid() {
            log::warn!("no active display found, input capture is disabled");
        }
        self.bounds = bounds;

        log::debug!("Updated displays bounds: {0:?}", self.bounds);
        Ok(())
    }

    /// start the input capture by
    fn start_capture(&mut self, event: &CGEvent, position: Position) -> Result<f64, CaptureError> {
        let mut location = event.location();
        let edge_offset = 1.0;
        // move cursor location to display bounds
        match position {
            Position::Left => location.x = self.bounds.xmin + edge_offset,
            Position::Right => location.x = self.bounds.xmax - edge_offset,
            Position::Top => location.y = self.bounds.ymin + edge_offset,
            Position::Bottom => location.y = self.bounds.ymax - edge_offset,
        };
        // where along the shared edge the pointer left this screen
        let fraction = match position {
            Position::Left | Position::Right => {
                let height = self.bounds.ymax - self.bounds.ymin;
                if height > 0.0 {
                    ((location.y - self.bounds.ymin) / height).clamp(0.0, 1.0)
                } else {
                    0.5
                }
            }
            Position::Top | Position::Bottom => {
                let width = self.bounds.xmax - self.bounds.xmin;
                if width > 0.0 {
                    ((location.x - self.bounds.xmin) / width).clamp(0.0, 1.0)
                } else {
                    0.5
                }
            }
        };

        self.enter_position = Some(location);
        self.reset_cursor()?;
        Ok(fraction)
    }

    /// The middle of one of the screen edges, in display coordinates.
    fn edge_middle(&self, pos: Position) -> (f64, f64) {
        let cx = (self.bounds.xmin + self.bounds.xmax) / 2.0;
        let cy = (self.bounds.ymin + self.bounds.ymax) / 2.0;
        if !self.bounds.is_valid() {
            return (cx, cy);
        }
        const INSET: f64 = 1.0;
        match pos {
            Position::Left => (self.bounds.xmin + INSET, cy),
            Position::Right => (self.bounds.xmax - INSET, cy),
            Position::Top => (cx, self.bounds.ymin + INSET),
            Position::Bottom => (cx, self.bounds.ymax - INSET),
        }
    }

    /// resets the cursor to the position, where the capture started
    fn reset_cursor(&mut self) -> Result<(), CaptureError> {
        // this runs inside the event tap callback: a missing position must not
        // panic across the FFI boundary
        let Some(pos) = self.enter_position else {
            log::debug!("no capture entry position, not resetting the cursor");
            return Ok(());
        };
        log::trace!("Resetting cursor position to: {}, {}", pos.x, pos.y);
        CGDisplay::warp_mouse_cursor_position(pos).map_err(CaptureError::WarpCursor)
    }

    fn hide_cursor(&self) -> Result<(), CaptureError> {
        CGDisplay::hide_cursor(&CGDisplay::main()).map_err(CaptureError::CoreGraphics)
    }

    fn show_cursor(&self) -> Result<(), CaptureError> {
        CGDisplay::show_cursor(&CGDisplay::main()).map_err(CaptureError::CoreGraphics)
    }

    async fn handle_producer_event(
        &mut self,
        producer_event: ProducerEvent,
    ) -> Result<(), CaptureError> {
        log::debug!("handling event: {producer_event:?}");
        match producer_event {
            ProducerEvent::Release => {
                if self.current_pos.is_some() {
                    self.show_cursor()?;
                    self.current_pos = None;
                }
                self.latched_barrier = None;
            }
            ProducerEvent::Grab(pos) => {
                if self.current_pos.is_none() {
                    self.hide_cursor()?;
                    self.current_pos = Some(pos);
                }
            }
            ProducerEvent::Take(pos) => {
                /* Same state as a crossing, but the pointer was never at the
                 * edge. Pin the hidden cursor to the middle of that edge so the
                 * physical pointer keeps producing motion instead of running
                 * out of screen space, which would stall the peer's cursor.
                 */
                self.hide_cursor()?;
                self.current_pos = Some(pos);
                self.latched_barrier = None;
                let (x, y) = self.edge_middle(pos);
                log::debug!("taking the pointer at {pos}, pinning the cursor to {x}, {y}");
                self.enter_position = Some(CGPoint::new(x, y));
                self.reset_cursor()?;
            }
            ProducerEvent::Create(p, segments) => {
                self.active_clients.insert(p, segments);
            }
            ProducerEvent::Destroy(p) => {
                if let Some(current) = self.current_pos {
                    if current == p {
                        self.show_cursor()?;
                        self.current_pos = None;
                    };
                }
                self.active_clients.remove(&p);
            }
            ProducerEvent::EventTapDisabled => {
                // Tap death can happen mid-capture (TCC Accessibility
                // revoked, tap-timeout, etc). Release state so we
                // don't leave the cursor hidden even if the outer
                // task only logs this error rather than propagating.
                if self.current_pos.is_some() {
                    self.show_cursor()?;
                    self.current_pos = None;
                }
                return Err(CaptureError::EventTapDisabled);
            }
            ProducerEvent::DisplayReconfigured => {
                // The macOS display configuration changed — a monitor
                // was plugged in/out, the resolution changed, the
                // arrangement was rearranged, etc. Re-fetch the
                // active-display bounds so barrier crossings and the
                // cursor-warp on capture-start use the current
                // geometry instead of whatever was true at process
                // start.
                if let Err(e) = self.update_bounds() {
                    log::warn!("failed to refresh display bounds: {e}");
                } else {
                    log::info!("display reconfigured: {:?}", self.bounds);
                }
            }
        };
        Ok(())
    }
}

fn get_events(
    ev_type: &CGEventType,
    ev: &CGEvent,
    result: &mut Vec<CaptureEvent>,
    modifier_state: &mut XMods,
) -> Result<(), CaptureError> {
    fn map_pointer_event(ev: &CGEvent) -> PointerEvent {
        PointerEvent::Motion {
            time: 0,
            dx: ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X),
            dy: ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y),
        }
    }

    fn map_key(ev: &CGEvent) -> Result<u32, CaptureError> {
        let code = ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE);
        match KeyMap::from_key_mapping(KeyMapping::Mac(code as u16)) {
            Ok(k) => Ok(k.evdev as u32),
            Err(()) => Err(CaptureError::KeyMapError(code)),
        }
    }

    match ev_type {
        CGEventType::KeyDown => {
            let k = map_key(ev)?;
            result.push(CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: k,
                state: 1,
            })));
        }
        CGEventType::KeyUp => {
            let k = map_key(ev)?;
            result.push(CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                time: 0,
                key: k,
                state: 0,
            })));
        }
        CGEventType::FlagsChanged => {
            let mut depressed = XMods::empty();
            let mut mods_locked = XMods::empty();
            let cg_flags = ev.get_flags();

            if cg_flags.contains(CGEventFlags::CGEventFlagShift) {
                depressed |= XMods::ShiftMask;
            }
            if cg_flags.contains(CGEventFlags::CGEventFlagControl) {
                depressed |= XMods::ControlMask;
            }
            if cg_flags.contains(CGEventFlags::CGEventFlagAlternate) {
                depressed |= XMods::Mod1Mask;
            }
            if cg_flags.contains(CGEventFlags::CGEventFlagCommand) {
                depressed |= XMods::Mod4Mask;
            }
            if cg_flags.contains(CGEventFlags::CGEventFlagAlphaShift) {
                depressed |= XMods::LockMask;
                mods_locked |= XMods::LockMask;
            }

            // check if pressed or released
            // `>` on the raw bitmask is wrong: pressing a second modifier
            // while a "smaller" one is held (e.g. Shift = 1, Ctrl = 4) would
            // report a key release. Only a newly set bit is a key down.
            let state = if depressed.bits() & !modifier_state.bits() != 0 {
                1
            } else {
                0
            };
            *modifier_state = depressed;

            if let Ok(key) = map_key(ev) {
                let key_event = CaptureEvent::Input(Event::Keyboard(KeyboardEvent::Key {
                    time: 0,
                    key,
                    state,
                }));
                result.push(key_event);
            }

            let modifier_event = KeyboardEvent::Modifiers {
                depressed: depressed.bits(),
                latched: 0,
                locked: mods_locked.bits(),
                group: 0,
            };

            result.push(CaptureEvent::Input(Event::Keyboard(modifier_event)));
        }
        CGEventType::MouseMoved => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::LeftMouseDragged => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::RightMouseDragged => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::OtherMouseDragged => {
            result.push(CaptureEvent::Input(Event::Pointer(map_pointer_event(ev))))
        }
        CGEventType::LeftMouseDown => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_LEFT,
                state: 1,
            })))
        }
        CGEventType::LeftMouseUp => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_LEFT,
                state: 0,
            })))
        }
        CGEventType::RightMouseDown => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_RIGHT,
                state: 1,
            })))
        }
        CGEventType::RightMouseUp => {
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button: BTN_RIGHT,
                state: 0,
            })))
        }
        CGEventType::OtherMouseDown => {
            let btn_num = ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                3 => BTN_BACK,
                4 => BTN_FORWARD,
                _ => BTN_MIDDLE,
            };
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button,
                state: 1,
            })))
        }
        CGEventType::OtherMouseUp => {
            let btn_num = ev.get_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER);
            let button = match btn_num {
                3 => BTN_BACK,
                4 => BTN_FORWARD,
                _ => BTN_MIDDLE,
            };
            result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Button {
                time: 0,
                button,
                state: 0,
            })))
        }
        CGEventType::ScrollWheel => {
            // macOS scroll deltas use the opposite sign convention
            // from evdev when Natural Scrolling is OFF. When it is ON
            // the system already negates the deltas, matching evdev.
            // Field 137 = isDirectionInvertedFromDevice (non-zero when
            // Natural Scrolling is active).
            const DIRECTION_INVERTED_FROM_DEVICE: CGEventField = 137;
            let natural = ev.get_integer_value_field(DIRECTION_INVERTED_FROM_DEVICE) != 0;
            let sign: i64 = if natural { 1 } else { -1 };

            if ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS) != 0 {
                // point deltas use the same sign convention as line deltas:
                // negating only this branch inverted trackpad scrolling
                // relative to wheel scrolling
                let v = sign
                    * ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1);
                let h = sign
                    * ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2);
                if v != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                        time: 0,
                        axis: 0, // Vertical
                        value: v as f64,
                    })));
                }
                if h != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(PointerEvent::Axis {
                        time: 0,
                        axis: 1, // Horizontal
                        value: h as f64,
                    })));
                }
            } else {
                // line based scrolling
                const LINES_PER_STEP: i32 = 3;
                const V120_STEPS_PER_LINE: i32 = 120 / LINES_PER_STEP;
                let v =
                    sign * ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1);
                let h =
                    sign * ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2);
                if v != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(
                        PointerEvent::AxisDiscrete120 {
                            axis: 0, // Vertical
                            value: V120_STEPS_PER_LINE * v as i32,
                        },
                    )));
                }
                if h != 0 {
                    result.push(CaptureEvent::Input(Event::Pointer(
                        PointerEvent::AxisDiscrete120 {
                            axis: 1, // Horizontal
                            value: V120_STEPS_PER_LINE * h as i32,
                        },
                    )));
                }
            }
        }
        _ => (),
    }
    Ok(())
}

fn create_event_tap<'a>(
    client_state: Arc<Mutex<InputCaptureState>>,
    notify_tx: Sender<ProducerEvent>,
    event_tx: UnboundedSender<Result<(Position, CaptureEvent), CaptureError>>,
) -> Result<CGEventTap<'a>, MacosCaptureCreationError> {
    // Shared slot for the tap's mach port pointer. Stored as `usize`
    // because raw pointers aren't `Send`, but the integer
    // representation is — and CGEventTapEnable is documented as
    // thread-safe. Set immediately after CGEventTap::new returns;
    // read by the callback to recover from a TapDisabledByTimeout.
    let tap_mach_port: Arc<OnceLock<usize>> = Arc::new(OnceLock::new());
    let tap_mach_port_cb = Arc::clone(&tap_mach_port);

    let cg_events_of_interest: Vec<CGEventType> = vec![
        CGEventType::LeftMouseDown,
        CGEventType::LeftMouseUp,
        CGEventType::RightMouseDown,
        CGEventType::RightMouseUp,
        CGEventType::OtherMouseDown,
        CGEventType::OtherMouseUp,
        CGEventType::MouseMoved,
        CGEventType::LeftMouseDragged,
        CGEventType::RightMouseDragged,
        CGEventType::OtherMouseDragged,
        CGEventType::ScrollWheel,
        CGEventType::KeyDown,
        CGEventType::KeyUp,
        CGEventType::FlagsChanged,
        // `NX_SYSDEFINED`: media keys (the volume knob, play/pause,
        // brightness). `core-graphics` names no variant for it, but the tap
        // mask is just a bit field, so the raw discriminant works.
        unsafe { std::mem::transmute::<u32, CGEventType>(SYSTEM_DEFINED_EVENT) },
    ];

    let event_tap_callback = move |_proxy: CGEventTapProxy,
                                   event_type: CGEventType,
                                   cg_ev: &CGEvent| {
        log::trace!("Got event from tap: {event_type:?}");
        // NOTE: this runs on the event tap thread and must return within
        // ~1s, otherwise the kernel disables the tap. Never panic here and
        // never block on a channel.
        let mut state = client_state.blocking_lock();
        let mut capture_position = None;
        let mut res_events = vec![];

        if matches!(event_type, CGEventType::TapDisabledByTimeout) {
            // The kernel disables the tap when our callback runs
            // longer than ~1s on a single event — typical causes
            // are heavy load, scheduler contention, or this
            // process being briefly suspended (e.g. App Nap on a
            // long idle). It is NOT a fatal condition: Apple's
            // documented recovery is to call CGEventTapEnable
            // and resume processing. Re-enable in place and KEEP
            // existing capture state so the user doesn't see the
            // cursor pop back to the local screen mid-session.
            if let Some(&port) = tap_mach_port_cb.get() {
                log::warn!("CGEventTap disabled by timeout — re-enabling");
                unsafe {
                    CGEventTapEnable(port as *mut c_void, true);
                }
            } else {
                log::error!(
                    "CGEventTap disabled by timeout, but mach port not yet stored — cannot re-enable"
                );
            }
            return CallbackResult::Keep;
        }

        if matches!(event_type, CGEventType::TapDisabledByUserInput) {
            // Deliberate kill — secure-input mode (e.g. password
            // field), TCC Accessibility revoked mid-session, or
            // the user disabling event-monitoring. We can't
            // recover from this; drop captured state synchronously
            // and return Keep on this event. Otherwise the
            // `current_pos.is_some()` branch below would drop this
            // event (and any racing callback still in flight) back
            // into `CallbackResult::Drop`, silently eating the
            // user's clicks and keypresses while the tap winds
            // down. Clear state + show the cursor here, then
            // notify the producer loop so the service can tear
            // down cleanly.
            log::error!("CGEventTap disabled by user input, releasing capture state");
            if state.current_pos.is_some() {
                let _ = CGDisplay::show_cursor(&CGDisplay::main());
                state.current_pos = None;
            }
            notify_tx
                .blocking_send(ProducerEvent::EventTapDisabled)
                .unwrap_or_else(|e| {
                    log::error!("Failed to send notification: {e}");
                });
            return CallbackResult::Keep;
        }

        // Skip synthesized *keyboard* events. Injected keystrokes are posted at
        // the HID layer, so without this check the capture tap picks them up and
        // forwards them straight back to the peer: a hotkey then looks like
        // local typing and the feedback loop compounds.
        //
        // Pointer motion is deliberately NOT skipped. The pointer position that
        // `crossed()` reads comes from these very events, so ignoring them left
        // the receiver unable to notice the cursor reaching its own edge — the
        // pointer could enter the peer but never come back. Feedback is avoided
        // instead by latching the barrier and by parking the cursor inside the
        // edge after a release, not by hiding the motion.
        let synthesized =
            cg_ev.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA) == INJECTED_EVENT_TAG;
        // Keys are always filtered: a re-captured keystroke would be forwarded
        // straight back to the peer.
        //
        // Pointer motion is let through, because `crossed()` reads the position
        // from these events. Filtering them out made a remote-driven machine
        // blind to the cursor reaching its own edge, so the pointer could enter
        // it but never come back. The feedback loop that the filter originally
        // guarded against is prevented by the barrier latch and by parking the
        // cursor inside the edge on release instead.
        if synthesized && !is_pointer_motion(event_type) {
            log::trace!("ignoring synthesized key event");
            return CallbackResult::Keep;
        }

        // Are we in a client?
        if let Some(current_pos) = state.current_pos {
            capture_position = Some(current_pos);
            // While capturing, a synthesized event is our own injection being
            // read back. Keep the cursor pinned but do not forward it: that
            // would send the peer its own movement.
            if !synthesized {
                if (event_type as u32) == SYSTEM_DEFINED_EVENT {
                    // Media keys are not key events: the hardware sends
                    // `NX_SYSDEFINED` with the key type packed into `data1`.
                    // Without this the volume knob kept changing the Mac's
                    // volume while the pointer was on the peer.
                    if let Some(event) = media_key_event(cg_ev) {
                        res_events.push(event);
                    }
                } else {
                    get_events(
                        &event_type,
                        cg_ev,
                        &mut res_events,
                        &mut state.modifier_state,
                    )
                    .unwrap_or_else(|e| {
                        log::error!("Failed to get events: {e}");
                    });
                }
            }

            // Keep (hidden) cursor at the edge of the screen
            if matches!(
                event_type,
                CGEventType::MouseMoved
                    | CGEventType::LeftMouseDragged
                    | CGEventType::RightMouseDragged
                    | CGEventType::OtherMouseDragged
            ) {
                state.reset_cursor().unwrap_or_else(|e| log::warn!("{e}"));
            }
        } else if matches!(event_type, CGEventType::MouseMoved) {
            // Did we cross a barrier?
            if let Some(new_pos) = state.crossed(cg_ev) {
                capture_position = Some(new_pos);
                let fraction = state.start_capture(cg_ev, new_pos).unwrap_or_else(|e| {
                    log::warn!("{e}");
                    0.5
                });
                res_events.push(CaptureEvent::Begin(fraction));
                if let Err(e) = notify_tx.try_send(ProducerEvent::Grab(new_pos)) {
                    // a dropped grab notification would leave the cursor
                    // hidden, so at least it must not panic across the FFI
                    // boundary
                    log::warn!("failed to send grab notification: {e}");
                }
            }
        }

        if let Some(pos) = capture_position {
            res_events.iter().for_each(|e| {
                // error must be ignored, since the event channel
                // may already be closed when the InputCapture instance is dropped.
                if let Err(e) = event_tx.send(Ok((pos, *e))) {
                    log::warn!("failed to forward the capture event: {e}");
                }
            });
            // Returning Drop should stop the event from being processed
            // but core fundation still returns the event
            cg_ev.set_type(CGEventType::Null);
            CallbackResult::Drop
        } else {
            CallbackResult::Keep
        }
    };

    let tap = CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        cg_events_of_interest,
        event_tap_callback,
    )
    .map_err(|_| MacosCaptureCreationError::EventTapCreation)?;

    // Hand the mach port pointer to the callback so it can re-enable
    // the tap on TapDisabledByTimeout. The pointer is valid for the
    // lifetime of `tap` (which lives on the event-tap thread until
    // the run loop exits).
    let port_ptr = tap.mach_port().as_concrete_TypeRef() as usize;
    let _ = tap_mach_port.set(port_ptr);

    let tap_source: CFRunLoopSource = tap
        .mach_port()
        .create_runloop_source(0)
        .map_err(|_| MacosCaptureCreationError::EventTapCreation)?;

    unsafe {
        CFRunLoop::get_current().add_source(&tap_source, kCFRunLoopCommonModes);
    }

    Ok(tap)
}

fn event_tap_thread(
    client_state: Arc<Mutex<InputCaptureState>>,
    event_tx: UnboundedSender<Result<(Position, CaptureEvent), CaptureError>>,
    notify_tx: Sender<ProducerEvent>,
    ready: std::sync::mpsc::Sender<Result<CFRunLoop, MacosCaptureCreationError>>,
    exit: oneshot::Sender<()>,
) {
    // Clone now: create_event_tap consumes notify_tx into its closure.
    let display_notify_tx = notify_tx.clone();

    let _tap = match create_event_tap(client_state, notify_tx, event_tx) {
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
        Ok(tap) => {
            let run_loop = CFRunLoop::get_current();
            // the receiver may already be gone if startup failed
            let _ = ready.send(Ok(run_loop));
            tap
        }
    };

    // Register a Quartz display-reconfiguration callback so the
    // capture state's bounds get refreshed when the user plugs in a
    // monitor, changes resolution, or rearranges displays. The
    // callback runs on this thread's CFRunLoop. Box-leak the sender
    // so the C side has a stable user_info pointer; reclaim it after
    // the run loop exits.
    let display_user_info = Box::into_raw(Box::new(display_notify_tx)) as *mut c_void;
    unsafe {
        CGDisplayRegisterReconfigurationCallback(
            display_reconfiguration_callback,
            display_user_info,
        );
    }

    log::debug!("running CFRunLoop...");
    CFRunLoop::run_current();
    log::debug!("event tap thread exiting!...");

    unsafe {
        CGDisplayRemoveReconfigurationCallback(display_reconfiguration_callback, display_user_info);
        // Reclaim the leaked sender Box so we don't leak a tokio
        // channel sender on every capture create/destroy cycle.
        drop(Box::from_raw(
            display_user_info as *mut Sender<ProducerEvent>,
        ));
    }

    let _ = exit.send(());
}

/// Quartz display-reconfiguration callback. Fires twice per change:
/// once with `kCGDisplayBeginConfigurationFlag` set (BEFORE the
/// change is applied — the bounds are still stale at this point),
/// then again afterwards with the actual change flags (Add, Remove,
/// Mode, DesktopShapeChanged, etc.). Skip the begin phase; on the
/// real notification, kick the producer task to refresh bounds.
extern "C" fn display_reconfiguration_callback(_display: u32, flags: u32, user_info: *mut c_void) {
    const K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG: u32 = 1 << 0;
    if flags & K_CG_DISPLAY_BEGIN_CONFIGURATION_FLAG != 0 {
        return;
    }
    if user_info.is_null() {
        return;
    }
    // SAFETY: user_info is a Box::into_raw of Sender<ProducerEvent>
    // owned by `event_tap_thread`. It's valid for the lifetime of
    // that thread; the registration is removed before the box is
    // freed. The callback only fires while the run loop is running
    // on that thread, so we know the box is live here.
    let sender = unsafe { &*(user_info as *const Sender<ProducerEvent>) };
    if let Err(e) = sender.blocking_send(ProducerEvent::DisplayReconfigured) {
        log::warn!("failed to notify display reconfiguration: {e}");
    }
}

pub struct MacOSInputCapture {
    event_rx: UnboundedReceiver<Result<(Position, CaptureEvent), CaptureError>>,
    /// Sender of the same channel `event_rx` reads from: a capture that is
    /// taken on request has to report its own `Begin` (the tap only does that
    /// for a real barrier crossing).
    event_tx: UnboundedSender<Result<(Position, CaptureEvent), CaptureError>>,
    notify_tx: Sender<ProducerEvent>,
    run_loop: CFRunLoop,
    /// The same state the tap and the producer task run on, kept so the display
    /// geometry can be read without a round trip through the run loop.
    state: Arc<Mutex<InputCaptureState>>,
}

impl MacOSInputCapture {
    pub async fn new() -> Result<Self, MacosCaptureCreationError> {
        request_macos_capture_permissions()?;

        let state = Arc::new(Mutex::new(InputCaptureState::new()?));
        /* the tap callback must never block or drop events: a dropped key
         * release leaves the peer with a stuck key */
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (notify_tx, mut notify_rx) = mpsc::channel(32);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (tap_exit_tx, mut tap_exit_rx) = oneshot::channel();

        unsafe {
            configure_cf_settings()?;
        }

        log::info!("Enabling CGEvent tap");
        let event_tap_thread_state = state.clone();
        let event_tap_notify = notify_tx.clone();
        // used by the producer task to report a fatal backend error
        let task_event_tx = event_tx.clone();
        thread::spawn(move || {
            event_tap_thread(
                event_tap_thread_state,
                event_tx,
                event_tap_notify,
                ready_tx,
                tap_exit_tx,
            )
        });

        // wait for event tap creation result
        let run_loop = ready_rx
            .recv()
            .map_err(|_| MacosCaptureCreationError::EventTapCreation)??;

        let task_error_tx = task_event_tx.clone();
        /* the task owns a handle of its own: `state` itself is kept for the struct,
         * because `screen_size()` and `create`/`update` read the same state from
         * outside the task */
        let task_state = Arc::clone(&state);
        let _tap_task: tokio::task::JoinHandle<()> = tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    producer_event = notify_rx.recv() => {
                        let Some(producer_event) = producer_event else {
                            break;
                        };
                        let mut state = task_state.lock().await;
                        if let Err(e) = state.handle_producer_event(producer_event).await {
                            // swallow-and-continue would leave the capture active
                            // forever (e.g. a dead event tap: no events arrive, but
                            // pressed keys are never released). Report it upstream.
                            log::error!("Failed to handle producer event: {e}");
                            let _ = task_error_tx.send(Err(e));
                            break;
                        }
                    }
                    _ = &mut tap_exit_rx => break,
                }
            }
            // show cursor
            let _ = CGDisplay::show_cursor(&CGDisplay::main());
        });

        Ok(Self {
            event_rx,
            event_tx: task_event_tx,
            notify_tx,
            run_loop,
            state,
        })
    }
}

fn request_macos_capture_permissions() -> Result<(), MacosCaptureCreationError> {
    // Call both request functions unconditionally so macOS surfaces both
    // TCC prompts on the very first launch. TCC always returns `false` the
    // first time a permission is requested (the grant only becomes visible
    // on the next process launch), so returning early on the first failure
    // would skip the second prompt and force the user through an extra
    // relaunch just to see it.
    let accessibility = request_accessibility_permission();
    let input_monitoring = request_input_monitoring_permission();

    if !accessibility {
        return Err(MacosCaptureCreationError::AccessibilityPermission);
    }
    if !input_monitoring {
        return Err(MacosCaptureCreationError::InputMonitoringPermission);
    }
    Ok(())
}

fn request_accessibility_permission() -> bool {
    // Silent check. The GUI owns the one-time user-visible prompt at
    // startup (see lan_mouse_gtk::macos_privacy) so retries triggered by
    // clicking the "Reenable" button don't pop a fresh Accessibility
    // alert every time.
    unsafe { AXIsProcessTrusted() }
}

fn request_input_monitoring_permission() -> bool {
    // Silent check, same reasoning as above.
    unsafe { CGPreflightListenEventAccess() }
}

impl Drop for MacOSInputCapture {
    fn drop(&mut self) {
        self.run_loop.stop();
    }
}

#[async_trait]
impl Capture for MacOSInputCapture {
    async fn create(
        &mut self,
        pos: Position,
        segments: Vec<EdgeRange>,
    ) -> Result<(), CaptureError> {
        // Send synchronously: a detached task could still be pending when
        // `destroy`/`release` arrive, reordering the state updates, and on a
        // quick shutdown the hiding cursor would never be undone.
        log::debug!("creating capture, {pos}");
        self.notify_tx
            .send(ProducerEvent::Create(pos, segments))
            .await
            .map_err(|_| CaptureError::EndOfStream)?;
        log::debug!("done !");
        Ok(())
    }

    async fn update(
        &mut self,
        pos: Position,
        segments: Vec<EdgeRange>,
    ) -> Result<(), CaptureError> {
        /* The tap checks the segments on every crossing, so replacing them is
         * enough - there is no barrier object to move. */
        self.notify_tx
            .send(ProducerEvent::Create(pos, segments))
            .await
            .map_err(|_| CaptureError::EndOfStream)?;
        Ok(())
    }

    async fn screen_size(&mut self) -> Option<(u32, u32)> {
        let state = self.state.lock().await;
        let bounds = &state.bounds;
        if !bounds.is_valid() {
            return None;
        }
        Some((
            (bounds.xmax - bounds.xmin).max(0.0) as u32,
            (bounds.ymax - bounds.ymin).max(0.0) as u32,
        ))
    }

    async fn begin(&mut self, pos: Position) -> Result<(), CaptureError> {
        /*
         * The tap only forwards input while `current_pos` is set, and
         * `ProducerEvent::Grab` is what sets it (it also hides the cursor, so
         * the pointer does not sit visibly on the edge while it is remote).
         * A real crossing lets the tap emit `Begin`; a requested start has to
         * emit it here, with an unknown fraction because nothing was crossed.
         */
        log::debug!("taking the pointer at {pos} on request");
        self.notify_tx
            .send(ProducerEvent::Take(pos))
            .await
            .map_err(|_| CaptureError::EndOfStream)?;
        if let Err(e) = self.event_tx.send(Ok((pos, CaptureEvent::Begin(0.5)))) {
            log::warn!("failed to forward the capture event: {e}");
            return Err(CaptureError::EndOfStream);
        }
        Ok(())
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        log::debug!("destroying capture {pos}");
        self.notify_tx
            .send(ProducerEvent::Destroy(pos))
            .await
            .map_err(|_| CaptureError::EndOfStream)?;
        log::debug!("done !");
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        log::debug!("notifying Release");
        self.notify_tx
            .send(ProducerEvent::Release)
            .await
            .map_err(|_| CaptureError::EndOfStream)?;
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl Stream for MacOSInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            // the backend forwards fatal errors (e.g. a disabled event tap)
            // through the event channel so the supervisor can release keys
            Some(e) => Poll::Ready(Some(e)),
        }
    }
}

type CGSConnectionID = u32;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGSSetConnectionProperty(
        cid: CGSConnectionID,
        targetCID: CGSConnectionID,
        key: CFStringRef,
        value: CFBooleanRef,
    ) -> CGError;
    fn _CGSDefaultConnection() -> CGSConnectionID;
}

extern "C" {
    fn CGEventSourceSetLocalEventsSuppressionInterval(
        event_source: CGEventSource,
        seconds: CFTimeInterval,
    );
    fn CGPreflightListenEventAccess() -> bool;
    /// Re-enable an event tap that was disabled by a
    /// `kCGEventTapDisabledByTimeout` event. The Apple-documented
    /// recovery path: see Quartz Event Services Reference. The `tap`
    /// argument is a `CFMachPortRef`; we pass the raw pointer so we
    /// can store it as `usize` for cross-thread sharing.
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);

    /// Register a callback invoked when the display configuration
    /// changes (monitor add/remove, resolution change, mirror,
    /// rearrange, etc). See Quartz Display Services Reference.
    fn CGDisplayRegisterReconfigurationCallback(
        callback: extern "C" fn(u32, u32, *mut c_void),
        user_info: *mut c_void,
    ) -> CGError;
    fn CGDisplayRemoveReconfigurationCallback(
        callback: extern "C" fn(u32, u32, *mut c_void),
        user_info: *mut c_void,
    ) -> CGError;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

unsafe fn configure_cf_settings() -> Result<(), MacosCaptureCreationError> {
    // When we warp the cursor using CGWarpMouseCursorPosition local events are suppressed for a short time
    // this leeds to the cursor not flowing when crossing back from a clinet, set this to to 0 stops the warp
    // from working, set a low value by trial and error, 0.05s seems good. 0.25s is the default
    let event_source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
        .map_err(|_| MacosCaptureCreationError::EventSourceCreation)?;
    CGEventSourceSetLocalEventsSuppressionInterval(event_source, 0.05);

    // This is a private settings that allows the cursor to be hidden while in the background.
    // It is used by Barrier and other apps.
    let key = CString::new("SetsCursorInBackground").unwrap();
    let cf_key = CFStringCreateWithCString(
        kCFAllocatorDefault,
        key.as_ptr() as *const c_char,
        kCFStringEncodingUTF8,
    );
    if cf_key.is_null() {
        log::warn!("could not create the CFString for the cursor property");
        return Err(MacosCaptureCreationError::CGCursorProperty);
    }
    let result = CGSSetConnectionProperty(
        _CGSDefaultConnection(),
        _CGSDefaultConnection(),
        cf_key,
        kCFBooleanTrue,
    );
    /* Release before returning: this used to leak the string on the error path. */
    CFRelease(cf_key as *const c_void);
    if result != kCGErrorSuccess {
        return Err(MacosCaptureCreationError::CGCursorProperty);
    }
    Ok(())
}

// From X11/X.h
bitflags! {
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
    struct XMods: u32 {
        const ShiftMask = (1<<0);
        const LockMask = (1<<1);
        const ControlMask = (1<<2);
        const Mod1Mask = (1<<3);
        const Mod2Mask = (1<<4);
        const Mod3Mask = (1<<5);
        const Mod4Mask = (1<<6);
        const Mod5Mask = (1<<7);
    }
}
