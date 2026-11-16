use futures::{StreamExt, future};
use std::{
    env, fs, io,
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::PathBuf,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::task::JoinHandle;

use ashpd::desktop::{
    PersistMode, Session,
    remote_desktop::{DeviceType, RemoteDesktop, SelectDevicesOptions},
};
use async_trait::async_trait;

use reis::{
    ei::{self, button::ButtonState, handshake::ContextType, keyboard::KeyState},
    event::{self, Connection, DeviceCapability, DeviceEvent, EiEvent, SeatEvent},
    tokio::EiConvertEventStream,
};

use input_event::{Event, KeyboardEvent, PointerEvent};

use crate::error::EmulationError;

use super::{Emulation, EmulationHandle, error::LibeiEmulationCreationError};

/// an absolute pointer interface together with the size of the region it
/// addresses, as reported by the EIS implementation (logical pixels)
type AbsolutePointer = (ei::PointerAbsolute, (u32, u32));

#[derive(Clone, Default)]
struct Devices {
    pointer: Arc<RwLock<Option<(ei::Device, ei::Pointer)>>>,
    /// absolute pointer, plus the device size the EIS implementation reported
    /// (in logical pixels) so positions can be given in the peer's own space
    pointer_absolute: Arc<RwLock<Option<(ei::Device, AbsolutePointer)>>>,
    scroll: Arc<RwLock<Option<(ei::Device, ei::Scroll)>>>,
    button: Arc<RwLock<Option<(ei::Device, ei::Button)>>>,
    keyboard: Arc<RwLock<Option<(ei::Device, ei::Keyboard)>>>,
}

impl Devices {
    /// store an interface of `device`, replacing whatever was stored before
    fn set<T>(slot: &Arc<RwLock<Option<(ei::Device, T)>>>, device: &ei::Device, interface: T) {
        match slot.write() {
            Ok(mut slot) => *slot = Some((device.clone(), interface)),
            // the lock is only ever held by short, non-panicking sections
            Err(e) => log::error!("device lock is poisoned: {e}"),
        }
    }

    /// store every interface that `device` provides
    fn register(&self, device: &event::Device) {
        if let Some(pointer) = device.interface::<ei::Pointer>() {
            Self::set(&self.pointer, device.device(), pointer);
        }
        if let Some(pointer) = device.interface::<ei::PointerAbsolute>() {
            /* `ei_pointer_absolute.motion_absolute` takes logical pixels, so
             * the device size is needed to place the cursor. The EIS
             * implementation sends it with the device setup. */
            let dimensions = device.dimensions().unwrap_or_default();
            if dimensions == (0, 0) {
                log::debug!("absolute pointer without dimensions, cursor placement stays disabled");
            }
            Self::set(
                &self.pointer_absolute,
                device.device(),
                (pointer, dimensions),
            );
        }
        if let Some(keyboard) = device.interface::<ei::Keyboard>() {
            Self::set(&self.keyboard, device.device(), keyboard);
        }
        if let Some(scroll) = device.interface::<ei::Scroll>() {
            Self::set(&self.scroll, device.device(), scroll);
        }
        if let Some(button) = device.interface::<ei::Button>() {
            Self::set(&self.button, device.device(), button);
        }
    }

    /// clear a device slot if it currently holds the given device
    fn clear_if(slot: &Arc<RwLock<Option<(ei::Device, impl ei::Interface)>>>, device: &ei::Device) {
        if slot
            .read()
            .is_ok_and(|d| d.as_ref().is_some_and(|(el, _)| el == device))
        {
            let _ = slot.write().map(|mut d| *d = None);
        }
    }

    /// drop every reference to `device` so that no further events are sent to
    /// a paused or removed device
    fn remove(&self, device: &ei::Device) {
        Self::clear_if(&self.pointer, device);
        Self::clear_if(&self.keyboard, device);
        Self::clear_if(&self.scroll, device);
        Self::clear_if(&self.button, device);
        if self
            .pointer_absolute
            .read()
            .is_ok_and(|d| d.as_ref().is_some_and(|(el, _)| el == device))
        {
            let _ = self.pointer_absolute.write().map(|mut d| *d = None);
        }
    }

    /// The EIS device size in logical pixels, if the peer reported one.
    fn absolute_dimensions(&self) -> Option<(u32, u32)> {
        let guard = self.pointer_absolute.read().ok()?;
        let (_, (_, dimensions)) = guard.as_ref()?;
        if *dimensions == (0, 0) {
            return None;
        }
        Some(*dimensions)
    }
}

pub(crate) struct LibeiEmulation {
    context: ei::Context,
    conn: event::Connection,
    devices: Devices,
    ei_task: JoinHandle<()>,
    error: Arc<Mutex<Option<EmulationError>>>,
    libei_error: Arc<AtomicBool>,
    _remote_desktop: RemoteDesktop,
    session: Session<RemoteDesktop>,
}

/// Get the path to the RemoteDesktop token file
fn get_token_file_path() -> PathBuf {
    let cache_dir = env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            // neither XDG_CACHE_HOME nor HOME may be set (e.g. when started
            // from a systemd unit): fall back to the current directory
            // instead of panicking
            env::var("HOME")
                .ok()
                .filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .unwrap_or_else(|| {
            log::warn!(
                "neither XDG_CACHE_HOME nor HOME is set, storing the token in the current directory"
            );
            PathBuf::from(".")
        });

    cache_dir.join("lan-mouse").join("remote-desktop.token")
}

/// Read the RemoteDesktop token from file
fn read_token() -> Option<String> {
    let token_path = get_token_file_path();
    match fs::read_to_string(&token_path) {
        Ok(token) => Some(token.trim().to_string()),
        Err(_) => None,
    }
}

/// Write the RemoteDesktop token to file
fn write_token(token: &str) -> io::Result<()> {
    let token_path = get_token_file_path();
    if let Some(parent) = token_path.parent() {
        fs::create_dir_all(parent)?;
    }

    fs::write(&token_path, token)?;
    Ok(())
}

async fn get_ei_fd() -> Result<(RemoteDesktop, Session<RemoteDesktop>, OwnedFd), ashpd::Error> {
    let remote_desktop = RemoteDesktop::new().await?;

    let restore_token = read_token();

    log::debug!("creating session ...");
    let session = remote_desktop.create_session(Default::default()).await?;

    log::debug!("selecting devices ...");
    let options = SelectDevicesOptions::default()
        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
        .set_persist_mode(PersistMode::ExplicitlyRevoked)
        .set_restore_token(restore_token.as_deref());
    remote_desktop.select_devices(&session, options).await?;

    log::info!("requesting permission for input emulation");
    let start_response = remote_desktop
        .start(&session, None, Default::default())
        .await?
        .response()?;

    // The restore token is only valid once, we need to re-save it each time
    if let Some(token_str) = start_response.restore_token() {
        if let Err(e) = write_token(token_str) {
            log::warn!("failed to save RemoteDesktop token: {}", e);
        }
    }

    let fd = remote_desktop
        .connect_to_eis(&session, Default::default())
        .await?;
    Ok((remote_desktop, session, fd))
}

impl LibeiEmulation {
    pub(crate) async fn new() -> Result<Self, LibeiEmulationCreationError> {
        let (_remote_desktop, session, eifd) = get_ei_fd().await?;
        let stream = UnixStream::from(eifd);
        stream.set_nonblocking(true)?;
        let context = ei::Context::new(stream)?;
        let (conn, events) = context
            .handshake_tokio("de.feschber.LanMouse", ContextType::Sender)
            .await?;
        let devices = Devices::default();
        let libei_error = Arc::new(AtomicBool::default());
        let error = Arc::new(Mutex::new(None));
        let ei_handler = ei_task(
            events,
            conn.clone(),
            context.clone(),
            devices.clone(),
            libei_error.clone(),
            error.clone(),
        );
        let ei_task = tokio::task::spawn_local(ei_handler);

        Ok(Self {
            context,
            conn,
            devices,
            ei_task,
            error,
            libei_error,
            _remote_desktop,
            session,
        })
    }
}

impl Drop for LibeiEmulation {
    fn drop(&mut self) {
        self.ei_task.abort();
    }
}

impl LibeiEmulation {
    /// Move the absolute pointer to `x`, `y` in the receiver's layout space.
    fn absolute(&self, x: f64, y: f64) -> Result<(), EmulationError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        let guard = self
            .devices
            .pointer_absolute
            .read()
            .map_err(|e| EmulationError::Io(io::Error::other(e.to_string())))?;
        let Some((device, (pointer, dimensions))) = guard.as_ref() else {
            log::debug!("no absolute pointer device, not placing the cursor");
            return Ok(());
        };
        log::debug!(
            "placing the cursor at {x:.0}, {y:.0} (device {}x{})",
            dimensions.0,
            dimensions.1
        );
        pointer.motion_absolute(x as f32, y as f32);
        device.frame(self.conn.serial(), now);
        self.context
            .flush()
            .map_err(|e| io::Error::new(e.kind(), e))?;
        Ok(())
    }
}

#[async_trait]
impl Emulation for LibeiEmulation {
    fn scroll_units_per_notch(&self) -> Option<f64> {
        /* EI scroll deltas use the same continuous units as wl_pointer */
        Some(15.0)
    }

    async fn consume(
        &mut self,
        event: Event,
        _handle: EmulationHandle,
    ) -> Result<(), EmulationError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros() as u64;
        if self.libei_error.load(Ordering::SeqCst) {
            // don't break sending additional events but signal error
            if let Some(e) = self.error.lock().unwrap().take() {
                return Err(e);
            }
        }
        match event {
            Event::Pointer(p) => match p {
                PointerEvent::Motion { time: _, dx, dy } => {
                    let pointer_device = self.devices.pointer.read().unwrap();
                    if let Some((d, p)) = pointer_device.as_ref() {
                        p.motion_relative(dx as f32, dy as f32);
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    let button_device = self.devices.button.read().unwrap();
                    if let Some((d, b)) = button_device.as_ref() {
                        b.button(
                            button,
                            match state {
                                0 => ButtonState::Released,
                                _ => ButtonState::Press,
                            },
                        );
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    let scroll_device = self.devices.scroll.read().unwrap();
                    if let Some((d, s)) = scroll_device.as_ref() {
                        match axis {
                            0 => s.scroll(0., value as f32),
                            _ => s.scroll(value as f32, 0.),
                        }
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::AxisStop { axis, .. } | PointerEvent::AxisCancel { axis, .. } => {
                    let scroll_device = self.devices.scroll.read().unwrap();
                    if let Some((d, s)) = scroll_device.as_ref() {
                        let x = u32::from(axis == 1);
                        let y = u32::from(axis == 0);
                        let is_cancel = u32::from(matches!(p, PointerEvent::AxisCancel { .. }));
                        s.scroll_stop(x, y, is_cancel);
                        d.frame(self.conn.serial(), now);
                    }
                }
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    let scroll_device = self.devices.scroll.read().unwrap();
                    if let Some((d, s)) = scroll_device.as_ref() {
                        match axis {
                            0 => s.scroll_discrete(0, value),
                            _ => s.scroll_discrete(value, 0),
                        }
                        d.frame(self.conn.serial(), now);
                    }
                }
            },
            Event::Keyboard(k) => match k {
                KeyboardEvent::Key {
                    time: _,
                    key,
                    state,
                } => {
                    let keyboard_device = self.devices.keyboard.read().unwrap();
                    if let Some((d, k)) = keyboard_device.as_ref() {
                        k.key(
                            key,
                            match state {
                                0 => KeyState::Released,
                                _ => KeyState::Press,
                            },
                        );
                        d.frame(self.conn.serial(), now);
                    }
                }
                KeyboardEvent::Modifiers { .. } => {}
            },
        }
        self.context
            .flush()
            .map_err(|e| io::Error::new(e.kind(), e))?;
        Ok(())
    }

    async fn create(&mut self, _: EmulationHandle) {}
    async fn destroy(&mut self, _: EmulationHandle) {}

    /// Place the pointer at the edge the peer crossed.
    ///
    /// `ei_pointer_absolute.motion_absolute` takes logical pixels of the
    /// receiver's own layout, which the EIS implementation reports as the
    /// device size, so this lands exactly - unlike the wlroots virtual pointer,
    /// whose absolute request Hyprland feeds through the pointer acceleration
    /// curve.
    async fn enter(&mut self, _handle: EmulationHandle, edge: crate::ScreenEdge, fraction: f64) {
        let Some((width, height)) = self.devices.absolute_dimensions() else {
            log::debug!("no absolute pointer with a known size, not placing the cursor");
            return;
        };
        /* a couple of pixels inside, or the receiver sees the pointer cross
         * the same edge again and bounces it straight back */
        const INSET: f64 = 8.0;
        let (x, y) = edge.point_on_inset(
            fraction,
            0.0,
            0.0,
            (width - 1) as f64,
            (height - 1) as f64,
            INSET,
        );
        if let Err(e) = self.absolute(x, y) {
            log::warn!("failed to place the cursor: {e}");
        }
    }

    async fn enter_center(&mut self, _handle: EmulationHandle) {
        let Some((width, height)) = self.devices.absolute_dimensions() else {
            log::debug!("no absolute pointer with a known size, not centring the cursor");
            return;
        };
        if let Err(e) = self.absolute(width as f64 / 2.0, height as f64 / 2.0) {
            log::warn!("failed to centre the cursor: {e}");
        }
    }

    async fn terminate(&mut self) {
        let _ = self.session.close().await;
        self.ei_task.abort();
    }
}

async fn ei_task(
    mut events: EiConvertEventStream,
    _conn: Connection,
    context: ei::Context,
    devices: Devices,
    libei_error: Arc<AtomicBool>,
    error: Arc<Mutex<Option<EmulationError>>>,
) {
    loop {
        match ei_event_handler(&mut events, &context, &devices).await {
            Ok(()) => {}
            Err(e) => {
                libei_error.store(true, Ordering::SeqCst);
                error.lock().unwrap().replace(e);
                // wait for termination -> otherwise we will loop forever
                future::pending::<()>().await;
            }
        }
    }
}

async fn ei_event_handler(
    events: &mut EiConvertEventStream,
    context: &ei::Context,
    devices: &Devices,
) -> Result<(), EmulationError> {
    loop {
        let event = events.next().await.ok_or(EmulationError::EndOfStream)??;
        let capabilities = DeviceCapability::Pointer
            | DeviceCapability::PointerAbsolute
            | DeviceCapability::Keyboard
            | DeviceCapability::Touch
            | DeviceCapability::Scroll
            | DeviceCapability::Button;
        log::debug!("{event:?}");
        match event {
            EiEvent::Disconnected(e) => {
                log::debug!("ei disconnected: {e:?}");
                return Err(EmulationError::EndOfStream);
            }
            EiEvent::SeatAdded(e) => {
                e.seat().bind_capabilities(capabilities);
            }
            EiEvent::SeatRemoved(e) => {
                log::debug!("seat removed: {:?}", e.seat());
            }
            EiEvent::DeviceAdded(e) => {
                log::debug!("device added: {:?}", e.device().device_type());
                devices.register(e.device());
            }
            EiEvent::DeviceRemoved(e) => {
                log::debug!("device removed: {:?}", e.device().device_type());
                // the device is gone: drop any reference to it so that no
                // events are sent to a stale device
                devices.remove(e.device().device());
            }
            EiEvent::DevicePaused(e) => {
                log::debug!("device paused: {:?}", e.device().device_type());
                // while paused the device must not receive events
                devices.remove(e.device().device());
            }
            EiEvent::DeviceResumed(e) => {
                log::debug!("device resumed: {:?}", e.device().device_type());
                devices.register(e.device());
                e.device().device().start_emulating(0, 0);
            }
            EiEvent::KeyboardModifiers(e) => {
                log::debug!("modifiers: {e:?}");
            }
            // only for receiver context
            // EiEvent::Frame(_) => { },
            // EiEvent::DeviceStartEmulating(_) => { },
            // EiEvent::DeviceStopEmulating(_) => { },
            // EiEvent::PointerMotion(_) => { },
            // EiEvent::PointerMotionAbsolute(_) => { },
            // EiEvent::Button(_) => { },
            // EiEvent::ScrollDelta(_) => { },
            // EiEvent::ScrollStop(_) => { },
            // EiEvent::ScrollCancel(_) => { },
            // EiEvent::ScrollDiscrete(_) => { },
            // EiEvent::KeyboardKey(_) => { },
            // EiEvent::TouchDown(_) => { },
            // EiEvent::TouchUp(_) => { },
            // EiEvent::TouchMotion(_) => { },
            e => {
                // the event stream is controlled by the compositor, never
                // abort the whole process on an unexpected event
                log::debug!("ignoring unexpected ei event: {e:?}");
            }
        }
        context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
    }
}
