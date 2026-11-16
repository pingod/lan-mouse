use ashpd::{
    desktop::{
        PersistMode, Session,
        remote_desktop::{
            Axis, DeviceType, KeyState, NotifyPointerAxisOptions, RemoteDesktop,
            SelectDevicesOptions,
        },
    },
    zbus::AsyncDrop,
};
use async_trait::async_trait;

use futures::FutureExt;
use input_event::{
    Event::{Keyboard, Pointer},
    KeyboardEvent, PointerEvent,
};

use crate::error::EmulationError;

use super::{Emulation, EmulationHandle, error::XdpEmulationCreationError};

pub(crate) struct DesktopPortalEmulation {
    proxy: RemoteDesktop,
    session: Session<RemoteDesktop>,
}

impl DesktopPortalEmulation {
    pub(crate) async fn new() -> Result<DesktopPortalEmulation, XdpEmulationCreationError> {
        log::debug!("connecting to org.freedesktop.portal.RemoteDesktop portal ...");
        let proxy = RemoteDesktop::new().await?;

        // retry when user presses the cancel button
        log::debug!("creating session ...");
        let session = proxy.create_session(Default::default()).await?;

        log::debug!("selecting devices ...");
        let options = SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
            .set_persist_mode(PersistMode::ExplicitlyRevoked);
        proxy.select_devices(&session, options).await?;

        log::info!("requesting permission for input emulation");
        let _devices = proxy
            .start(&session, None, Default::default())
            .await?
            .response()?;

        log::debug!("started session");
        let session = session;

        Ok(Self { proxy, session })
    }
}

#[async_trait]
impl Emulation for DesktopPortalEmulation {
    fn scroll_units_per_notch(&self) -> Option<f64> {
        /* the portal forwards continuous axis deltas, same scale as wl_pointer */
        Some(15.0)
    }

    async fn consume(
        &mut self,
        event: input_event::Event,
        _client: EmulationHandle,
    ) -> Result<(), EmulationError> {
        match event {
            Pointer(p) => match p {
                PointerEvent::Motion { time: _, dx, dy } => {
                    self.proxy
                        .notify_pointer_motion(&self.session, dx, dy, Default::default())
                        .await?;
                }
                PointerEvent::Button {
                    time: _,
                    button,
                    state,
                } => {
                    let state = match state {
                        0 => KeyState::Released,
                        _ => KeyState::Pressed,
                    };
                    self.proxy
                        .notify_pointer_button(
                            &self.session,
                            button as i32,
                            state,
                            Default::default(),
                        )
                        .await?;
                }
                // the portal has no scroll-stop request: the compositor ends a
                // kinetic scroll on its own once the axis events stop arriving
                PointerEvent::AxisStop { .. } | PointerEvent::AxisCancel { .. } => {}
                PointerEvent::AxisDiscrete120 { axis, value } => {
                    let axis = match axis {
                        0 => Axis::Vertical,
                        _ => Axis::Horizontal,
                    };
                    self.proxy
                        .notify_pointer_axis_discrete(
                            &self.session,
                            axis,
                            value / 120,
                            Default::default(),
                        )
                        .await?;
                }
                PointerEvent::Axis {
                    time: _,
                    axis,
                    value,
                } => {
                    // `finish` terminates a scroll sequence; setting it on
                    // every single event made smooth scrolling stutter
                    let (dx, dy) = match axis {
                        0 => (0., value),
                        1 => (value, 0.),
                        axis => {
                            log::warn!("unsupported scroll axis: {axis}");
                            return Ok(());
                        }
                    };
                    self.proxy
                        .notify_pointer_axis(
                            &self.session,
                            dx,
                            dy,
                            NotifyPointerAxisOptions::default(),
                        )
                        .await?;
                }
            },
            Keyboard(k) => {
                match k {
                    KeyboardEvent::Key {
                        time: _,
                        key,
                        state,
                    } => {
                        let state = match state {
                            0 => KeyState::Released,
                            _ => KeyState::Pressed,
                        };
                        self.proxy
                            .notify_keyboard_keycode(
                                &self.session,
                                key as i32,
                                state,
                                Default::default(),
                            )
                            .await?;
                    }
                    KeyboardEvent::Modifiers { .. } => {
                        // ignore
                    }
                }
            }
        }
        Ok(())
    }

    async fn create(&mut self, _client: EmulationHandle) {}
    async fn destroy(&mut self, _client: EmulationHandle) {}
    async fn terminate(&mut self) {
        // the portal may never answer (e.g. the dialog is waiting for user
        // input), which would hang shutdown forever
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
        match tokio::time::timeout(TIMEOUT, self.session.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("session.close(): {e}"),
            Err(_) => log::warn!("session.close() timed out"),
        }
        match tokio::time::timeout(TIMEOUT, self.session.receive_closed()).await {
            Ok(Ok(_stream)) => {}
            Ok(Err(e)) => log::warn!("session.receive_closed(): {e}"),
            Err(_) => log::warn!("session.receive_closed() timed out"),
        }
    }
}

impl AsyncDrop for DesktopPortalEmulation {
    #[doc = r" Perform the async cleanup."]
    #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
    fn async_drop<'async_trait>(
        self,
    ) -> ::core::pin::Pin<
        Box<dyn ::core::future::Future<Output = ()> + ::core::marker::Send + 'async_trait>,
    >
    where
        Self: 'async_trait,
    {
        async move {
            let _ = self.session.close().await;
        }
        .boxed()
    }
}
