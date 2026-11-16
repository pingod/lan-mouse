use super::{Emulation, EmulationHandle, error::EmulationError};
use async_trait::async_trait;
use bitflags::bitflags;
use core_foundation::{base::TCFType, string::CFString};
use core_foundation_sys::preferences::{
    CFPreferencesGetAppBooleanValue, kCFPreferencesAnyApplication,
};
use core_graphics::base::CGFloat;
use core_graphics::display::{
    CGDirectDisplayID, CGDisplay, CGDisplayBounds, CGGetDisplaysWithRect, CGPoint, CGRect, CGSize,
};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
    scancode,
};
use keycode::{KeyMap, KeyMapping};
use objc2_app_kit::{NSEvent, NSEventModifierFlags, NSEventType};
use objc2_foundation::NSPoint;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::{sync::Notify, task::JoinHandle};

use super::error::MacOSEmulationCreationError;

const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_REPEAT_INTERVAL: Duration = Duration::from_millis(32);
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

pub(crate) struct MacOSEmulation {
    /// global event source for all events
    event_source: CGEventSource,
    /// task handle for key repeats
    /// the key currently auto-repeating on the peer, if any
    repeat_task: Option<(u16, JoinHandle<()>)>,
    /// current state of the mouse buttons (tracked by evdev button code)
    pressed_buttons: HashSet<u32>,
    /// button previously pressed (evdev button code)
    previous_button: Option<u32>,
    /// timestamp of previous click (button down)
    previous_button_click: Option<Instant>,
    /// click state, i.e. number of clicks in quick succession
    button_click_state: i64,
    /// current modifier state
    modifier_state: Arc<Mutex<XMods>>,
    /// notify to cancel key repeats
    notify_repeat_task: Arc<Notify>,
    /// cached key for `natural_scroll_enabled`
    natural_scroll_key: CFString,
}

/* `CGEventSource` and `CFString` are plain reference-counted Core Foundation
 * handles without a `Send` impl in core-graphics/core-foundation, but every
 * method used here is documented as thread safe and the emulation is driven
 * from a single task, so moving it between threads is fine. */
unsafe impl Send for MacOSEmulation {}

/// Maps an evdev button code to the CGEventType used for drag events.
fn drag_event_type(button: u32) -> CGEventType {
    match button {
        BTN_LEFT => CGEventType::LeftMouseDragged,
        BTN_RIGHT => CGEventType::RightMouseDragged,
        // middle, back, forward, and any other button all use OtherMouseDragged
        _ => CGEventType::OtherMouseDragged,
    }
}

/// Whether "Natural Scrolling" (`com.apple.swipescrolldirection`) is on. Real HID devices get
/// this applied before their CGEvent exists; synthetic events bypass it, so we replicate it here
/// to match what a real mouse would do on this Mac. See lan-mouse#481.
fn natural_scroll_enabled(key: &CFString) -> bool {
    let mut key_exists_and_valid = 0;
    let value = unsafe {
        CFPreferencesGetAppBooleanValue(
            key.as_concrete_TypeRef(),
            kCFPreferencesAnyApplication,
            &mut key_exists_and_valid,
        )
    };
    // default to enabled (macOS's factory default) if unset/invalid
    key_exists_and_valid == 0 || value != 0
}

impl MacOSEmulation {
    pub(crate) fn new() -> Result<Self, MacOSEmulationCreationError> {
        request_macos_emulation_permissions()?;

        let event_source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| MacOSEmulationCreationError::EventSourceCreation)?;
        Ok(Self {
            event_source,
            pressed_buttons: HashSet::new(),
            previous_button: None,
            previous_button_click: None,
            button_click_state: 0,
            repeat_task: None,
            notify_repeat_task: Arc::new(Notify::new()),
            modifier_state: Arc::new(Mutex::new(XMods::empty())),
            natural_scroll_key: CFString::new("com.apple.swipescrolldirection"),
        })
    }

    fn get_mouse_location(&self) -> Option<CGPoint> {
        let event: CGEvent = CGEvent::new(self.event_source.clone()).ok()?;
        Some(event.location())
    }

    /// Post a key-down for `key` and make it the auto-repeating key.
    ///
    /// Only the *newest* key repeats (that is what macOS does too), but the
    /// previously repeating key must stay physically held: it is still down on
    /// the sender, and sending a key-up for it here would make multi-key holds
    /// impossible (press and hold `a`, then `b` — `a` silently went up; issue
    /// #365). So the previous repeat task is stopped *without* emitting a
    /// key-up; its key-up arrives later with the real release event.
    async fn spawn_repeat_task(&mut self, key: u16) {
        self.stop_repeat_task().await;
        // initial key event
        key_event(
            self.event_source.clone(),
            key,
            1,
            modifiers_of(&self.modifier_state),
        );
        // repeat task
        let event_source = self.event_source.clone();
        let notify = self.notify_repeat_task.clone();
        let modifiers = self.modifier_state.clone();
        let repeat_task = tokio::task::spawn_local(async move {
            let stop = tokio::select! {
                _ = tokio::time::sleep(DEFAULT_REPEAT_DELAY) => false,
                _ = notify.notified() => true,
            };
            if !stop {
                loop {
                    key_event(event_source.clone(), key, 1, modifiers_of(&modifiers));
                    tokio::select! {
                        _ = tokio::time::sleep(DEFAULT_REPEAT_INTERVAL) => {},
                        _ = notify.notified() => break,
                    }
                }
            }
            // The key-up is emitted by `release_key` (the normal release
            // path), not here: this task also ends when a *different*
            // key starts repeating, and in that case `key` is still held down
            // on the sender.
            //
            // Do NOT call update_modifiers here: `key` is a Mac CGKeyCode but
            // update_modifiers expects a Linux evdev scancode, and the two
            // codespaces collide (e.g. Mac LeftShift=56 == Linux KeyLeftAlt=56,
            // Mac Down=125 == Linux KeyLeftMeta=125), corrupting modifier
            // state for chords like Shift+Option+X or Cmd+Down. Modifier state
            // is owned by the main consume() loop, which already calls
            // update_modifiers with the correct Linux scancode on the real key
            // release event from the client.
        });
        self.repeat_task = Some((key, repeat_task));
    }

    /// Stop the repeating key *without* releasing it (it is still held).
    async fn stop_repeat_task(&mut self) {
        if let Some((_, task)) = self.repeat_task.take() {
            self.notify_repeat_task.notify_waiters();
            let _ = task.await;
        }
    }

    /// Post a key-up for `key`.
    ///
    /// Stops the repeat task only if `key` is the one repeating — releasing an
    /// older held key must not release the newest one (issue #365).
    async fn release_key(&mut self, key: u16) {
        if matches!(&self.repeat_task, Some((repeating, _)) if *repeating == key) {
            self.stop_repeat_task().await;
        }
        key_event(
            self.event_source.clone(),
            key,
            0,
            modifiers_of(&self.modifier_state),
        );
    }
}

fn request_macos_emulation_permissions() -> Result<(), MacOSEmulationCreationError> {
    // Request both permissions up front so the user sees both TCC prompts
    // on the first launch. See the matching comment in input-capture/src/
    // macos.rs::request_macos_capture_permissions for the rationale.
    let accessibility = request_accessibility_permission();
    let input_control = request_input_control_permission();

    if !accessibility {
        return Err(MacOSEmulationCreationError::AccessibilityPermission);
    }
    if !input_control {
        return Err(MacOSEmulationCreationError::InputControlPermission);
    }
    Ok(())
}

fn request_accessibility_permission() -> bool {
    // Silent check. The GUI owns the one-time user-visible prompt at
    // startup (see lan_mouse_gtk::macos_privacy).
    unsafe { AXIsProcessTrusted() }
}

fn request_input_control_permission() -> bool {
    unsafe { CGPreflightPostEventAccess() }
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightPostEventAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Mac virtual key codes for the four arrow keys.
const MAC_KEY_LEFT: u16 = 0x7B;
const MAC_KEY_RIGHT: u16 = 0x7C;
const MAC_KEY_DOWN: u16 = 0x7D;
const MAC_KEY_UP: u16 = 0x7E;

fn is_arrow_key(key: u16) -> bool {
    matches!(
        key,
        MAC_KEY_LEFT | MAC_KEY_RIGHT | MAC_KEY_DOWN | MAC_KEY_UP
    )
}

/// Marker written into `kCGEventSourceUserData` on every synthesized event.
///
/// `input-capture` skips events carrying it, so injected input is never fed
/// back into the capture pipeline.
pub const INJECTED_EVENT_TAG: i64 = 0x6c616e6d; // "lanm"

/// Post `event` tagged as synthesized.
///
/// Every injected event must carry the tag, not just keystrokes: the capture
/// tap reads mouse motion too, and an untagged synthetic move is treated as
/// local input. The captured delta then accumulates past the screen edge,
/// which fires this device's own barrier and makes the peer believe the user
/// came back — the pointer visibly stalls instead of crossing cleanly.
fn post_synthesized(event: &CGEvent) {
    event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, INJECTED_EVENT_TAG);
    event.post(CGEventTapLocation::HID);
}

/// `NX_KEYTYPE_*` identifiers of the "system defined" events macOS uses for the
/// special keys above the number row (IOKit `hidsystem/IOLLEvent.h`).
///
/// These keys have no macOS virtual keycode that a posted `CGEvent` acts on:
/// the Chromium derived table maps evdev 113/114/115 to the legacy
/// `kVK_VolumeUp`/`kVK_VolumeDown`/`kVK_Mute` codes (0x48/0x49/0x4a), but posting
/// those as ordinary keyboard events does nothing — the volume knob on a
/// keyboard was silently swallowed while the pointer was captured by macOS.
const NX_KEYTYPE_SOUND_UP: isize = 0;
const NX_KEYTYPE_SOUND_DOWN: isize = 1;
const NX_KEYTYPE_BRIGHTNESS_UP: isize = 2;
const NX_KEYTYPE_BRIGHTNESS_DOWN: isize = 3;
const NX_KEYTYPE_MUTE: isize = 7;
const NX_KEYTYPE_PLAY: isize = 16;
const NX_KEYTYPE_NEXT: isize = 17;
const NX_KEYTYPE_PREVIOUS: isize = 18;

/// maps a Linux evdev keycode to the `NX_KEYTYPE_*` identifier of the
/// system defined event macOS expects for it, if it is one of those keys
fn media_key_type(key: u32) -> Option<isize> {
    match scancode::Linux::try_from(key).ok()? {
        scancode::Linux::KeyVolumeUp => Some(NX_KEYTYPE_SOUND_UP),
        scancode::Linux::KeyVolumeDown => Some(NX_KEYTYPE_SOUND_DOWN),
        scancode::Linux::KeyMute => Some(NX_KEYTYPE_MUTE),
        scancode::Linux::KeyBrightnessup => Some(NX_KEYTYPE_BRIGHTNESS_UP),
        scancode::Linux::KeyBrightnessdown => Some(NX_KEYTYPE_BRIGHTNESS_DOWN),
        scancode::Linux::KeyPlaypause => Some(NX_KEYTYPE_PLAY),
        scancode::Linux::KeyNextsong => Some(NX_KEYTYPE_NEXT),
        scancode::Linux::KeyPrevioussong => Some(NX_KEYTYPE_PREVIOUS),
        _ => None,
    }
}

/// Posts a "system defined" event, which is how the media keys (volume, mute,
/// playback, brightness) have to be synthesized on macOS.
///
/// `CGEvent::new_keyboard_event` cannot express them: macOS routes them through
/// `NSEventTypeSystemDefined` events whose `data1` carries the `NX_KEYTYPE_*`
/// identifier in the high word and the up/down marker (0x0a / 0x0b) in the
/// second byte, exactly like the events a real Apple keyboard emits.
fn media_key_event(key_type: isize, state: u8) {
    // NX_KEYDOWN (0x0a) / NX_KEYUP (0x0b)
    let down = state != 0;
    let marker: isize = if down { 0x0a } else { 0x0b };
    let data1 = (key_type << 16) | (marker << 8);
    let flags = NSEventModifierFlags::from_bits_truncate(if down { 0xa00 } else { 0xb00 });
    let Some(event) =
        NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::SystemDefined,
            NSPoint::ZERO,
            flags,
            0.0,
            0,
            None,
            8, // NX_SUBTYPE_AUX_CONTROL_BUTTONS
            data1,
            -1,
        )
    else {
        log::warn!("unable to create the media key event");
        return;
    };
    let Some(cg_event) = event.CGEvent() else {
        log::warn!("the media key event carries no CGEvent");
        return;
    };
    objc2_core_graphics::CGEvent::post(
        objc2_core_graphics::CGEventTapLocation::HIDEventTap,
        Some(&cg_event),
    );
    log::trace!("media key event: {key_type} {state}");
}

fn key_event(event_source: CGEventSource, key: u16, state: u8, modifiers: XMods) {
    let event = match CGEvent::new_keyboard_event(event_source, key, state != 0) {
        Ok(e) => e,
        Err(_) => {
            log::warn!("unable to create key event");
            return;
        }
    };
    let mut flags = to_cgevent_flags(modifiers);
    // Hardware-generated arrow keys on macOS carry NumericPad + SecondaryFn.
    // CGEventTap-based hotkey matchers (e.g. tiling window managers) check
    // these flags to recognize navigation keys; without them synthesized
    // arrow chords fall through to the focused app.
    if is_arrow_key(key) {
        flags |= CGEventFlags::CGEventFlagNumericPad | CGEventFlags::CGEventFlagSecondaryFn;
    }
    event.set_flags(flags);
    // Tag the event so this process's own capture tap can tell it apart from
    // hardware input. Without the tag the injected keystroke is captured again
    // and forwarded to the peer, which is a feedback loop: the peer's hotkey
    // looked like local typing here.
    post_synthesized(&event);
    log::trace!("key event: {key} {state}");
}

/// Posts a `FlagsChanged` event for a modifier key.
///
/// The event MUST carry the modifier's real virtual keycode. A bare
/// `CGEvent::new()` defaults to keycode 0 (`kVK_ANSI_A`), so every modifier
/// change arrived in apps as a phantom "A" key event — holding Ctrl registered
/// as Ctrl+A and shortcut recorders captured "A" (issue #450).
///
/// Carrying the real keycode also matters for consumers that track *physical*
/// modifier transitions through AppKit's `flagsChanged(with:)` rather than the
/// flags on the key-down event — notably Apple Virtualization.framework guest
/// views (`VZVirtualMachineView`), which derive guest modifier state from these
/// `FlagsChanged` events. The event is built as a key-down so it gets a valid
/// keycode; the type is then overridden to `FlagsChanged` and the *current*
/// modifier flags (already updated by the caller) describe the new state.
fn modifier_key_event(event_source: CGEventSource, key: u16, depressed: XMods) {
    let Ok(event) = CGEvent::new_keyboard_event(event_source, key, true) else {
        log::warn!("could not create modifier key event");
        return;
    };
    event.set_type(CGEventType::FlagsChanged);
    event.set_flags(modifier_flags_changed_flags(depressed));
    post_synthesized(&event);
    log::trace!("modifier key event: {key} {depressed:?}");
}

/// Builds the flag set for a modifier `FlagsChanged` event.
///
/// Combines the device-INDEPENDENT modifier word (what ordinary AppKit apps
/// read) with the device-DEPENDENT low-word bits (IOKit `NX_DEVICE*KEYMASK` from
/// `IOKit/hidsystem/IOLLEvent.h`) that a real hardware keyboard sets.
///
/// This matters for Apple Virtualization.framework guest views
/// (`VZVirtualMachineView`, used by UTM's Apple backend, Parallels' macOS
/// guests, VirtualBuddy, etc.): they derive the guest's modifier state from the
/// `flagsChanged(with:)` responder method and read the device-dependent low
/// word, not the per-key flags. macOS does NOT synthesize the low-word bits for
/// posted (synthetic) events, so until we set them ourselves Cmd/Ctrl/Shift/Opt
/// chords reached the guest unmodified (Shift+2 → "2", Cmd+C did nothing).
///
/// Ordinary apps mask incoming events to the device-independent word
/// (`deviceIndependentFlagsMask`) and ignore these bits, so emitting them is
/// hardware-faithful and safe for every app — no VM/bundle-id detection is
/// required. See issue #450 and https://developer.apple.com/forums/thread/766014
fn modifier_flags_changed_flags(depressed: XMods) -> CGEventFlags {
    // Device-dependent left/right modifier bits (IOLLEvent.h). lan-mouse collapses
    // left and right modifiers into a single mask, so we emit the left-hand device
    // bit; AltGr (Mod5 / ISO_Level3_Shift) is physically the right Alt, so it maps
    // to the right Option bit.
    const NX_DEVICE_L_CTRL: u64 = 0x0000_0001;
    const NX_DEVICE_L_SHIFT: u64 = 0x0000_0002;
    const NX_DEVICE_L_CMD: u64 = 0x0000_0008;
    const NX_DEVICE_L_ALT: u64 = 0x0000_0020;
    const NX_DEVICE_R_ALT: u64 = 0x0000_0040;

    let mut device_bits: u64 = 0;
    if depressed.contains(XMods::ShiftMask) {
        device_bits |= NX_DEVICE_L_SHIFT;
    }
    if depressed.contains(XMods::ControlMask) {
        device_bits |= NX_DEVICE_L_CTRL;
    }
    if depressed.contains(XMods::Mod1Mask) {
        device_bits |= NX_DEVICE_L_ALT;
    }
    if depressed.contains(XMods::Mod5Mask) {
        device_bits |= NX_DEVICE_R_ALT;
    }
    if depressed.contains(XMods::Mod4Mask) {
        device_bits |= NX_DEVICE_L_CMD;
    }

    // CGEventFlagNonCoalesced is bit 8 (0x100), the marker a real hardware
    // FlagsChanged carries on both press and release; VZ expects it present.
    let flags = to_cgevent_flags(depressed) | CGEventFlags::CGEventFlagNonCoalesced;
    CGEventFlags::from_bits_retain(flags.bits() | device_bits)
}

fn get_display_at_point(x: CGFloat, y: CGFloat) -> Option<CGDirectDisplayID> {
    let mut displays: [CGDirectDisplayID; 16] = [0; 16];
    let mut display_count: u32 = 0;
    let rect = CGRect::new(&CGPoint::new(x, y), &CGSize::new(0.0, 0.0));

    let error = unsafe {
        CGGetDisplaysWithRect(
            rect,
            1,
            displays.as_mut_ptr(),
            &mut display_count as *mut u32,
        )
    };

    if error != 0 {
        log::warn!("error getting displays at point ({x}, {y}): {error}");
        return Option::None;
    }

    if display_count == 0 {
        log::debug!("no displays found at point ({x}, {y})");
        return Option::None;
    }

    displays.first().copied()
}

fn get_display_bounds(display: CGDirectDisplayID) -> (CGFloat, CGFloat, CGFloat, CGFloat) {
    unsafe {
        let bounds = CGDisplayBounds(display);
        let min_x = bounds.origin.x;
        let max_x = bounds.origin.x + bounds.size.width;
        let min_y = bounds.origin.y;
        let max_y = bounds.origin.y + bounds.size.height;
        (min_x as f64, min_y as f64, max_x as f64, max_y as f64)
    }
}

fn clamp_to_screen_space(
    current_x: CGFloat,
    current_y: CGFloat,
    dx: CGFloat,
    dy: CGFloat,
) -> (CGFloat, CGFloat) {
    // Check which display the mouse is currently on
    // Determine what the location of the mouse would be after applying the move
    // Get the display at the new location
    // If the point is not on a display
    //   Clamp the mouse to the current display
    // Else If the point is on a display
    //   Clamp the mouse to the new display
    let new_x = current_x + dx;
    let new_y = current_y + dy;

    // Prefer the display the cursor would land on, then the one it is leaving.
    // When both are off-screen the position is already invalid — clamping to
    // the display under the *old* point used to bail out early and return the
    // out-of-range coordinate unchanged, so the cursor could never come back
    // from an overshoot. Fall back to the main display instead.
    let final_display = match get_display_at_point(new_x, new_y)
        .or_else(|| get_display_at_point(current_x, current_y))
    {
        Some(display) => display,
        None => {
            log::debug!(
                "cursor off every display at ({new_x}, {new_y}); clamping to the main display"
            );
            CGDisplay::main().id
        }
    };
    let (min_x, min_y, max_x, max_y) = get_display_bounds(final_display);

    (
        new_x.clamp(min_x, max_x - 1.),
        new_y.clamp(min_y, max_y - 1.),
    )
}

#[async_trait]
impl Emulation for MacOSEmulation {
    async fn enter(&mut self, x: f64, y: f64) {
        /*
         * Synthesized motion is relative, so without this the pointer would
         * continue from wherever it was on this screen — which looks like the
         * cursor teleporting when the peer was left at the top of its screen
         * while this one had it parked at the bottom.
         */
        let bounds = CGDisplay::main().bounds();
        let point = CGPoint::new(
            bounds.origin.x + bounds.size.width * x.clamp(0.0, 1.0),
            bounds.origin.y + bounds.size.height * y.clamp(0.0, 1.0),
        );
        if let Err(e) = CGDisplay::warp_mouse_cursor_position(point) {
            log::warn!("failed to position the cursor: {e}");
        }
    }

    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        log::trace!("{event:?}");
        match event {
            Event::Pointer(pointer_event) => {
                match pointer_event {
                    PointerEvent::Motion { time: _, dx, dy } => {
                        let mut mouse_location = match self.get_mouse_location() {
                            Some(l) => l,
                            None => {
                                log::warn!("could not get mouse location!");
                                return Ok(());
                            }
                        };

                        let (new_mouse_x, new_mouse_y) =
                            clamp_to_screen_space(mouse_location.x, mouse_location.y, dx, dy);

                        mouse_location.x = new_mouse_x;
                        mouse_location.y = new_mouse_y;

                        /* If a button is held, emit a drag event for the most
                         * recently pressed one; otherwise emit a normal
                         * mouse-moved event. A HashSet iteration order would
                         * pick an arbitrary button when several are held.
                         */
                        let drag_button = self
                            .previous_button
                            .filter(|btn| self.pressed_buttons.contains(btn))
                            .or_else(|| self.pressed_buttons.iter().next().copied());
                        let (event_type, cg_button) = match drag_button {
                            Some(btn) => (
                                drag_event_type(btn),
                                match btn {
                                    BTN_LEFT => CGMouseButton::Left,
                                    BTN_RIGHT => CGMouseButton::Right,
                                    _ => CGMouseButton::Center,
                                },
                            ),
                            None => (CGEventType::MouseMoved, CGMouseButton::Left),
                        };
                        let event = match CGEvent::new_mouse_event(
                            self.event_source.clone(),
                            event_type,
                            mouse_location,
                            cg_button,
                        ) {
                            Ok(e) => e,
                            Err(_) => {
                                log::warn!("mouse event creation failed!");
                                return Ok(());
                            }
                        };
                        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx as i64);
                        event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy as i64);
                        post_synthesized(&event);
                    }
                    PointerEvent::Button {
                        time: _,
                        button,
                        state,
                    } => {
                        // button number for OtherMouse events (3 = back, 4 = forward, etc.)
                        let cg_button_number: Option<i64> = match button {
                            BTN_BACK => Some(3),
                            BTN_FORWARD => Some(4),
                            _ => None,
                        };
                        let (event_type, mouse_button) = match (button, state) {
                            (BTN_LEFT, 1) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
                            (BTN_LEFT, 0) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
                            (BTN_RIGHT, 1) => (CGEventType::RightMouseDown, CGMouseButton::Right),
                            (BTN_RIGHT, 0) => (CGEventType::RightMouseUp, CGMouseButton::Right),
                            (BTN_MIDDLE, 1) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
                            (BTN_MIDDLE, 0) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
                            (BTN_BACK, 1) | (BTN_FORWARD, 1) => {
                                (CGEventType::OtherMouseDown, CGMouseButton::Center)
                            }
                            (BTN_BACK, 0) | (BTN_FORWARD, 0) => {
                                (CGEventType::OtherMouseUp, CGMouseButton::Center)
                            }
                            _ => {
                                log::warn!("invalid button event: {button},{state}");
                                return Ok(());
                            }
                        };
                        // store button state using the evdev button code so
                        // back, forward, and middle are tracked independently
                        if state == 1 {
                            self.pressed_buttons.insert(button);
                        } else {
                            self.pressed_buttons.remove(&button);
                        }

                        // update double-click tracking using the evdev button
                        // code so that back/forward don't alias with middle
                        if state == 1 {
                            if self.previous_button == Some(button)
                                && self
                                    .previous_button_click
                                    .is_some_and(|i| i.elapsed() < DOUBLE_CLICK_INTERVAL)
                            {
                                self.button_click_state += 1;
                            } else {
                                self.button_click_state = 1;
                            }
                            self.previous_button = Some(button);
                            self.previous_button_click = Some(Instant::now());
                        }

                        log::debug!("click_state: {}", self.button_click_state);
                        let Some(location) = self.get_mouse_location() else {
                            log::warn!("could not determine the mouse location");
                            return Ok(());
                        };
                        let event = match CGEvent::new_mouse_event(
                            self.event_source.clone(),
                            event_type,
                            location,
                            mouse_button,
                        ) {
                            Ok(e) => e,
                            Err(()) => {
                                log::warn!("mouse event creation failed!");
                                return Ok(());
                            }
                        };
                        event.set_integer_value_field(
                            EventField::MOUSE_EVENT_CLICK_STATE,
                            self.button_click_state,
                        );
                        // Set the button number for extra buttons (back=3, forward=4)
                        if let Some(btn_num) = cg_button_number {
                            event.set_integer_value_field(
                                EventField::MOUSE_EVENT_BUTTON_NUMBER,
                                btn_num,
                            );
                        }
                        post_synthesized(&event);
                    }
                    PointerEvent::Axis {
                        time: _,
                        axis,
                        value,
                    } => {
                        let value = if natural_scroll_enabled(&self.natural_scroll_key) {
                            -value as i32
                        } else {
                            value as i32
                        };
                        let (count, wheel1, wheel2, wheel3) = match axis {
                            0 => (1, value, 0, 0), // 0 = vertical => 1 scroll wheel device (y axis)
                            1 => (2, 0, value, 0), // 1 = horizontal => 2 scroll wheel devices (y, x) -> (0, x)
                            _ => {
                                log::warn!("invalid scroll event: {axis}, {value}");
                                return Ok(());
                            }
                        };
                        let event = match CGEvent::new_scroll_event(
                            self.event_source.clone(),
                            ScrollEventUnit::PIXEL,
                            count,
                            wheel1,
                            wheel2,
                            wheel3,
                        ) {
                            Ok(e) => e,
                            Err(()) => {
                                log::warn!("scroll event creation failed!");
                                return Ok(());
                            }
                        };
                        post_synthesized(&event);
                    }
                    PointerEvent::AxisDiscrete120 { axis, value } => {
                        let value = if natural_scroll_enabled(&self.natural_scroll_key) {
                            value
                        } else {
                            -value
                        };
                        const LINES_PER_STEP: i32 = 3;
                        let (count, wheel1, wheel2, wheel3) = match axis {
                            0 => (1, value / (120 / LINES_PER_STEP), 0, 0), // 0 = vertical => 1 scroll wheel device (y axis)
                            1 => (2, 0, value / (120 / LINES_PER_STEP), 0), // 1 = horizontal => 2 scroll wheel devices (y, x) -> (0, x)
                            _ => {
                                log::warn!("invalid scroll event: {axis}, {value}");
                                return Ok(());
                            }
                        };
                        let event = match CGEvent::new_scroll_event(
                            self.event_source.clone(),
                            ScrollEventUnit::LINE,
                            count,
                            wheel1,
                            wheel2,
                            wheel3,
                        ) {
                            Ok(e) => e,
                            Err(()) => {
                                log::warn!("scroll event creation failed!");
                                return Ok(());
                            }
                        };
                        post_synthesized(&event);
                    }
                }

                /* Do not reset the click state here: the double click window
                 * is tracked by `previous_button_click` and a mouse movement
                 * between the two clicks of a double click (which is normal,
                 * the pointer rarely stays perfectly still) used to reset it.
                 */
            }
            Event::Keyboard(keyboard_event) => match keyboard_event {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    // The media keys above the number row (volume, mute, playback,
                    // brightness) are not keyboard events on macOS: they have to be
                    // posted as "system defined" events, which is what a real Apple
                    // keyboard emits. Routing them through the keycode table below
                    // posted the legacy kVK_VolumeUp/Down/Mute codes instead, which
                    // the window server ignores — the volume knob on the keyboard
                    // did nothing while the pointer was captured by this machine.
                    if let Some(key_type) = media_key_type(key) {
                        media_key_event(key_type, state);
                        return Ok(());
                    }
                    // Fallback table for Linux evdev keys that have no macOS CGKeyCode in
                    // the keycode crate's Chromium-derived table. F13/F14/F15 are the
                    // conventional macOS stand-ins for Print Screen / Scroll Lock / Pause,
                    // which do not exist on Apple keyboards.
                    let code: CGKeyCode = match scancode::Linux::try_from(key) {
                        Ok(scancode::Linux::KeySysrq) => 0x69,      // F13
                        Ok(scancode::Linux::KeyScrollLock) => 0x6B, // F14
                        Ok(scancode::Linux::KeyPause) => 0x71,      // F15
                        _ => match KeyMap::from_key_mapping(KeyMapping::Evdev(key as u16)) {
                            Ok(k) => k.mac as CGKeyCode,
                            Err(_) => {
                                log::warn!("unable to map key event");
                                return Ok(());
                            }
                        },
                    };
                    let is_modifier = update_modifiers(&self.modifier_state, key, state);
                    if is_modifier {
                        // Modifier keys are posted as FlagsChanged events carrying
                        // their real keycode (see modifier_key_event). They must NOT
                        // enter the key-repeat machinery: there is only one repeat
                        // slot, so pressing a second modifier would cancel the first
                        // modifier's repeat task and post a keyUp while it is still
                        // physically held, tearing chords apart (issue #450, #357).
                        modifier_key_event(
                            self.event_source.clone(),
                            code,
                            modifiers_of(&self.modifier_state),
                        );
                    } else {
                        match state {
                            // pressed
                            1 => self.spawn_repeat_task(code).await,
                            _ => self.release_key(code).await,
                        }
                    }
                }
                KeyboardEvent::Modifiers {
                    depressed,
                    latched,
                    locked,
                    group,
                } => {
                    // Only update internal modifier state here. The per-key handler
                    // above already posts a FlagsChanged event (with the real
                    // keycode) for each modifier Key event the client sends
                    // alongside this state update. Posting one here as well would
                    // duplicate it — and with the old bare CGEvent it injected a
                    // phantom keycode-0 ("A") key on every modifier change (#450).
                    set_modifiers(&self.modifier_state, depressed, latched, locked, group);
                }
            },
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _handle: EmulationHandle) {}

    async fn destroy(&mut self, _handle: EmulationHandle) {}

    async fn terminate(&mut self) {}
}

/// read a copy of the current modifier state
fn modifiers_of(modifiers: &Mutex<XMods>) -> XMods {
    modifiers.lock().map(|mods| *mods).unwrap_or_default()
}

fn update_modifiers(modifiers: &Mutex<XMods>, key: u32, state: u8) -> bool {
    if let Ok(key) = scancode::Linux::try_from(key) {
        // Caps Lock is a LOCKING modifier: a press toggles a persistent state
        // rather than being active only while physically held. Toggle on
        // key-down and ignore key-up, so the AlphaShift flag stays set on every
        // following keystroke until Caps Lock is pressed again. (A synthetic
        // CGEvent can't flip the hardware Caps Lock LED, but carrying the flag
        // produces the correct upper-case output — and, like real Caps Lock,
        // leaves the number-row symbols unshifted.)
        if matches!(key, scancode::Linux::KeyCapsLock) {
            if state == 1 {
                if let Ok(mut mods) = modifiers.lock() {
                    mods.toggle(XMods::LockMask);
                }
            }
            return true;
        }
        let mask = match key {
            scancode::Linux::KeyLeftShift | scancode::Linux::KeyRightShift => XMods::ShiftMask,
            scancode::Linux::KeyLeftCtrl | scancode::Linux::KeyRightCtrl => XMods::ControlMask,
            scancode::Linux::KeyLeftAlt | scancode::Linux::KeyRightalt => XMods::Mod1Mask,
            scancode::Linux::KeyLeftMeta | scancode::Linux::KeyRightmeta => XMods::Mod4Mask,
            _ => XMods::empty(),
        };
        // unchanged
        if mask.is_empty() {
            return false;
        }
        if let Ok(mut mods) = modifiers.lock() {
            match state {
                1 => mods.insert(mask),
                _ => mods.remove(mask),
            }
        }
        true
    } else {
        false
    }
}

fn set_modifiers(
    active_modifiers: &Mutex<XMods>,
    depressed: u32,
    latched: u32,
    locked: u32,
    group: u32,
) {
    // the masks come from the network, so unknown bits must not panic
    let depressed = XMods::from_bits_truncate(depressed);
    let locked = XMods::from_bits_truncate(locked);
    let _ = (latched, group);

    if let Ok(mut mods) = active_modifiers.lock() {
        *mods = depressed | (locked & XMods::LockMask);
    }
}

fn to_cgevent_flags(depressed: XMods) -> CGEventFlags {
    let mut flags = CGEventFlags::empty();
    if depressed.contains(XMods::ShiftMask) {
        flags |= CGEventFlags::CGEventFlagShift;
    }
    if depressed.contains(XMods::LockMask) {
        flags |= CGEventFlags::CGEventFlagAlphaShift;
    }
    if depressed.contains(XMods::ControlMask) {
        flags |= CGEventFlags::CGEventFlagControl;
    }
    // Mod1 is Alt. Mod5 is ISO_Level3_Shift (AltGr), which is how the Alt key is
    // reported on many xkb keymaps (including COSMIC's default) in the wholesale
    // Modifiers state events. Map both to Option so Alt/Option chords are not
    // silently dropped (issue #450).
    if depressed.contains(XMods::Mod1Mask) || depressed.contains(XMods::Mod5Mask) {
        flags |= CGEventFlags::CGEventFlagAlternate;
    }
    if depressed.contains(XMods::Mod4Mask) {
        flags |= CGEventFlags::CGEventFlagCommand;
    }
    flags
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
