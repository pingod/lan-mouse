use async_trait::async_trait;
use std::ptr;
use x11::{
    xlib::{self, XCloseDisplay},
    xtest,
};

use input_event::{
    BTN_BACK, BTN_FORWARD, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, Event, KeyboardEvent, PointerEvent,
};

use crate::error::EmulationError;

use super::{Emulation, EmulationHandle, ScreenEdge, error::X11EmulationCreationError};

pub(crate) struct X11Emulation {
    display: *mut xlib::Display,
}

unsafe impl Send for X11Emulation {}

impl X11Emulation {
    pub(crate) fn new() -> Result<Self, X11EmulationCreationError> {
        let display = unsafe {
            match xlib::XOpenDisplay(ptr::null()) {
                d if std::ptr::eq(d, ptr::null_mut::<xlib::Display>()) => {
                    Err(X11EmulationCreationError::OpenDisplay)
                }
                display => Ok(display),
            }
        }?;
        Ok(Self { display })
    }

    fn relative_motion(&self, dx: i32, dy: i32) {
        unsafe {
            xtest::XTestFakeRelativeMotionEvent(self.display, dx, dy, 0, 0);
        }
    }

    fn emulate_mouse_button(&self, button: u32, state: u32) {
        let x11_button = match button {
            BTN_RIGHT => 3,
            BTN_MIDDLE => 2,
            BTN_BACK => 8,
            BTN_FORWARD => 9,
            BTN_LEFT => 1,
            // defaulting to BTN_LEFT would turn an unknown button (e.g. a
            // side button) into a left click
            _ => {
                log::warn!("unsupported mouse button: {button}");
                return;
            }
        };
        unsafe {
            xtest::XTestFakeButtonEvent(self.display, x11_button, state as i32, 0);
        };
    }

    const SCROLL_UP: u32 = 4;
    const SCROLL_DOWN: u32 = 5;
    const SCROLL_LEFT: u32 = 6;
    const SCROLL_RIGHT: u32 = 7;

    /// emulate `steps` wheel clicks on the given axis.
    ///
    /// XTest only knows the discrete wheel buttons, so continuous scroll
    /// amounts are converted to whole wheel clicks.
    fn emulate_scroll(&self, axis: u8, steps: i64) {
        // a scroll amount of 0 is not a scroll down/right
        if steps == 0 {
            return;
        }

        let direction = match (axis, steps < 0) {
            (0, true) => Self::SCROLL_UP,
            (0, false) => Self::SCROLL_DOWN,
            (1, true) => Self::SCROLL_LEFT,
            (1, false) => Self::SCROLL_RIGHT,
            (axis, _) => {
                log::warn!("unsupported scroll axis: {axis}");
                return;
            }
        };

        unsafe {
            for _ in 0..steps.unsigned_abs() {
                xtest::XTestFakeButtonEvent(self.display, direction, 1, 0);
                xtest::XTestFakeButtonEvent(self.display, direction, 0, 0);
            }
        }
    }

    fn emulate_key(&self, key: u32, state: u8) {
        let key = key + 8; // xorg keycodes are shifted by 8
        unsafe {
            xtest::XTestFakeKeyEvent(self.display, key, state as i32, 0);
        }
    }
}

impl Drop for X11Emulation {
    fn drop(&mut self) {
        unsafe {
            XCloseDisplay(self.display);
        }
    }
}

#[async_trait]
impl Emulation for X11Emulation {
    async fn enter(&mut self, _handle: EmulationHandle, edge: ScreenEdge, fraction: f64) {
        /*
         * Place the cursor at the point the peer crossed its edge instead of
         * letting it continue from wherever it was parked on this screen.
         */
        let (width, height) = unsafe {
            let screen = xlib::XDefaultScreen(self.display);
            (
                xlib::XDisplayWidth(self.display, screen),
                xlib::XDisplayHeight(self.display, screen),
            )
        };
        if width <= 0 || height <= 0 {
            log::debug!("cannot position the cursor on a {width}x{height} screen");
            return;
        }
        let (x, y) = edge.point_on(fraction, 0.0, 0.0, (width - 1) as f64, (height - 1) as f64);
        let x = x as i32;
        let y = y as i32;
        unsafe {
            xlib::XWarpPointer(
                self.display,
                // `None` (0) as the source window: warp the real cursor
                0,
                xlib::XDefaultRootWindow(self.display),
                0,
                0,
                0,
                0,
                x,
                y,
            );
            xlib::XFlush(self.display);
        }
    }

    async fn consume(&mut self, event: Event, _: EmulationHandle) -> Result<(), EmulationError> {
        match event {
            Event::Pointer(pointer_event) => match pointer_event {
                PointerEvent::Motion { time: _, dx, dy } => {
                    self.relative_motion(dx as i32, dy as i32);
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    self.emulate_mouse_button(button, state);
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    // `value` is a continuous scroll amount; emit at least one
                    // wheel click for every started notch so that slow
                    // trackpad scrolling is not dropped entirely
                    const NOTCH: f64 = 10.;
                    let steps = (value / NOTCH).round();
                    let steps = if steps == 0. && value != 0. {
                        value.signum()
                    } else {
                        steps
                    };
                    self.emulate_scroll(axis, steps as i64);
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    // `value` is in units of 120 (one wheel notch)
                    let steps = value / 120;
                    let steps = if steps == 0 && value != 0 {
                        value.signum()
                    } else {
                        steps
                    };
                    self.emulate_scroll(axis, steps as i64);
                }
            },
            Event::Keyboard(KeyboardEvent::Key {
                time: _,
                key,
                state,
            }) => {
                self.emulate_key(key, state);
            }
            _ => {}
        }
        unsafe {
            xlib::XFlush(self.display);
        }
        // FIXME
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn destroy(&mut self, _: EmulationHandle) {
        // for our purposes it does not matter what client sent the event
    }

    async fn terminate(&mut self) {
        /* nothing to do */
    }
}
