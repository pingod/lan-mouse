use ashpd::{
    desktop::{
        Session,
        input_capture::{
            Activated, ActivatedBarrier, Barrier, BarrierID, Capabilities, CreateSessionOptions,
            InputCapture, Region, ReleaseOptions,
        },
    },
    enumflags2::BitFlags,
};
use async_trait::async_trait;
use futures::{FutureExt, StreamExt};
use reis::{
    ei::{self, handshake::ContextType},
    event::{Connection, DeviceCapability, EiEvent},
    tokio::EiConvertEventStream,
};
use std::{
    cell::Cell,
    collections::HashMap,
    env, io,
    num::NonZeroU32,
    os::unix::net::UnixStream,
    pin::Pin,
    rc::Rc,
    sync::{Arc, LazyLock, Mutex},
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    sync::{
        Notify,
        mpsc::{self, Receiver, Sender},
    },
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use futures_core::Stream;

use input_event::{DisplayInfo, EdgeRange, Event, Layout, Rotation};

use crate::CaptureEvent;

use super::{
    Capture as LanMouseInputCapture, Position,
    error::{CaptureError, LibeiCaptureCreationError},
};

/* there is a bug in xdg-remote-desktop-portal-gnome / mutter that
 * prevents receiving further events after a session has been disabled once.
 * Therefore the session needs to be recreated when the barriers are updated */

/* mutter also kills the session whenever ei devices come and go, so there the
 * whole session has to be torn down and recreated on every device change.
 * Elsewhere that is pure overhead: each restart costs a CreateSession +
 * ConnectToEIS round trip, and compositors that keep per-session state around
 * (hyprland leaks a keymap fd per eis session, see hyprwm/Hyprland) can be
 * driven out of file descriptors by the churn.
 * Set LM_RESTART_SESSION_ON_DEVICE_CHANGE=1/0 to override the default. */
static RESTART_SESSION_ON_DEVICE_CHANGE: LazyLock<bool> =
    LazyLock::new(restart_session_on_device_change);

fn restart_session_on_device_change() -> bool {
    match env::var("LM_RESTART_SESSION_ON_DEVICE_CHANGE").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => env::var("XDG_CURRENT_DESKTOP")
            .is_ok_and(|desktops| desktops.to_uppercase().split(':').any(|d| d == "GNOME")),
    }
}

/// events that necessitate restarting the capture session
#[derive(Clone, Debug)]
enum LibeiNotifyEvent {
    /// a client arrived on this position, or its segments changed
    Create(Position, Vec<EdgeRange>),
    Destroy(Position),
}

#[allow(dead_code)]
pub struct LibeiInputCapture {
    /* Keeps the portal object alive for the capture task, which only holds a
     * reference to it. An `Rc` (instead of a raw pointer into a boxed value)
     * makes the lifetime guaranteed by the type system.
     */
    input_capture: Arc<InputCapture>,
    capture_task: JoinHandle<Result<(), CaptureError>>,
    event_rx: Receiver<(Position, CaptureEvent)>,
    notify_capture: Sender<LibeiNotifyEvent>,
    notify_release: Arc<Notify>,
    cancellation_token: CancellationToken,
    terminated: bool,
    /// Size of the capture area as `width << 32 | height`, `0` when unknown.
    ///
    /// Written by the capture task whenever it registers barriers. An atomic
    /// rather than an `Rc<RefCell<..>>` because the trait methods are `Send`.
    ///
    /// The whole layout, not one size: a desk has several monitors, one of them
    /// possibly rotated, and the canvas has to draw each of them. The portal's
    /// `Region` already reports the rectangle *after* rotation, so no rotation has
    /// to be reconstructed here - it is recorded as `Normal` and the rectangle is
    /// taken verbatim.
    layout: Arc<Mutex<Layout>>,
}

/// A capture region in layout coordinates.
///
/// Extracted from the portal's `Region` so the barrier geometry can be computed
/// and tested without a compositor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RegionRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl RegionRect {
    fn new(region: &Region) -> Self {
        Self {
            x: region.x_offset(),
            y: region.y_offset(),
            w: region.width() as i32,
            h: region.height() as i32,
        }
    }

    /// last inclusive coordinate of the region along x
    fn right(&self) -> i32 {
        self.x + self.w - 1
    }

    /// last inclusive coordinate of the region along y
    fn bottom(&self) -> i32 {
        self.y + self.h - 1
    }
}

/// The span of the whole capture area along the axis an edge is measured on.
///
/// Edge ranges are fractions of *this* span, not of the display that happens to
/// touch them: it is the span the pointer crosses along and the span the
/// frontend draws, so both sides agree on what `0-60%` of an edge means.
/// `None` when nothing is known or the area is a single pixel wide.
fn layout_axis(regions: &[RegionRect], pos: Position) -> Option<(i32, i32)> {
    let first = *regions.first()?;
    let (mut lo, mut hi) = match pos {
        Position::Left | Position::Right => (first.y, first.bottom()),
        Position::Top | Position::Bottom => (first.x, first.right()),
    };
    for region in regions {
        let (start, end) = match pos {
            Position::Left | Position::Right => (region.y, region.bottom()),
            Position::Top | Position::Bottom => (region.x, region.right()),
        };
        lo = lo.min(start);
        hi = hi.max(end);
    }
    (hi > lo).then_some((lo, hi))
}

/// The barrier line for one segment of one edge of one region.
///
/// `None` when the segment does not touch this region: a display whose edge is
/// not covered by any client gets no barrier at all, so the pointer hits a wall
/// there instead of switching to a machine that does not own that part of the
/// edge.
fn segment_barrier(
    region: RegionRect,
    pos: Position,
    axis: (i32, i32),
    edge: EdgeRange,
) -> Option<(i32, i32, i32, i32)> {
    /* the line the barrier sits on, and the extent of this region along the
     * axis the crossing happens on */
    let (line, region_lo, region_hi) = match pos {
        Position::Left => (region.x, region.y, region.bottom()),
        Position::Right => (region.x + region.w, region.y, region.bottom()),
        Position::Top => (region.y, region.x, region.right()),
        Position::Bottom => (region.y + region.h, region.x, region.right()),
    };
    let span = (axis.1 - axis.0 + 1) as f64;
    let scale = |permille: u16| axis.0 as f64 + (permille as f64 / 1000.0) * span;
    /* `end` is exclusive, so the last pixel of the segment is one before it */
    let lo = scale(edge.start()).round() as i32;
    let hi = scale(edge.end()).round() as i32 - 1;
    let lo = lo.max(region_lo);
    let hi = hi.min(region_hi);
    if lo > hi {
        return None;
    }
    Some(match pos {
        Position::Left | Position::Right => (line, lo, line, hi),
        Position::Top | Position::Bottom => (lo, line, hi, line),
    })
}

/// where along the edge a barrier reaches, as a fraction of the whole edge
///
/// The portal reports the cursor position when a barrier fires but not which
/// point of the edge it came from in a form that can be normalized, and until
/// now every crossing was reported as the middle of the edge - so the peer
/// always parked its cursor in the middle, no matter where the pointer actually
/// left. With the barrier geometry known, the crossing point can be recovered
/// from the cursor position.
fn crossing_fraction(axis: (i32, i32), cursor: (f32, f32), pos: Position) -> f64 {
    let span = (axis.1 - axis.0 + 1) as f64;
    if span <= 0.0 {
        return 0.5;
    }
    let along = match pos {
        Position::Left | Position::Right => cursor.1 as f64,
        Position::Top | Position::Bottom => cursor.0 as f64,
    };
    ((along - axis.0 as f64) / span).clamp(0.0, 1.0)
}

/// Ashpd does not expose fields
#[derive(Clone, Copy, Debug)]
struct ICBarrier {
    barrier_id: BarrierID,
    position: (i32, i32, i32, i32),
}

impl ICBarrier {
    fn new(barrier_id: BarrierID, position: (i32, i32, i32, i32)) -> Self {
        Self {
            barrier_id,
            position,
        }
    }
}

impl From<ICBarrier> for Barrier {
    fn from(barrier: ICBarrier) -> Self {
        Barrier::new(barrier.barrier_id, barrier.position)
    }
}

/// The barriers registered for a session, with what is needed to interpret an
/// activation.
#[derive(Clone, Default)]
struct BarrierSet {
    barriers: Vec<ICBarrier>,
    /// which position a barrier belongs to
    pos_for_barrier: HashMap<BarrierID, Position>,
    /// the span of the capture area along the axis each edge is measured on,
    /// needed to turn the cursor position of an activation into a fraction of
    /// the edge
    axes: HashMap<Position, (i32, i32)>,
    /// every monitor the portal reported, in layout coordinates
    layout: Layout,
}

/// Every capture region as a monitor.
///
/// The portal reports each region as the rectangle it occupies *after* rotation,
/// so the rotation itself is not recoverable here and is recorded as `Normal`: what
/// matters downstream is the rectangle, and reconstructing a rotation from it would
/// be a guess.
fn regions_layout(regions: &[RegionRect]) -> Layout {
    Layout::new(
        regions
            .iter()
            .enumerate()
            .filter(|(_, region)| region.w > 0 && region.h > 0)
            .map(|(index, region)| DisplayInfo {
                name: format!("output{}", index + 1),
                x: region.x,
                y: region.y,
                width: region.w as u32,
                height: region.h as u32,
                rotation: Rotation::Normal,
                enabled: true,
            })
            .collect(),
    )
}

/// One barrier per segment per region that the segment touches.
///
/// A segment on a display that is only partly covered yields a barrier over just
/// the covered part, so a crossing outside it does not fire at all.
fn select_barriers(
    regions: &[RegionRect],
    clients: &[(Position, Vec<EdgeRange>)],
    next_barrier_id: &mut NonZeroU32,
) -> BarrierSet {
    let mut set = BarrierSet {
        layout: regions_layout(regions),
        ..BarrierSet::default()
    };
    for (pos, edges) in clients {
        let Some(axis) = layout_axis(regions, *pos) else {
            log::warn!("no capture region is known, not creating a barrier for {pos}");
            continue;
        };
        set.axes.insert(*pos, axis);
        for edge in edges {
            for region in regions {
                let Some(position) = segment_barrier(*region, *pos, axis, *edge) else {
                    continue;
                };
                let id = *next_barrier_id;
                // ids wrap around instead of overflowing after 4 billion barriers
                *next_barrier_id = next_barrier_id.checked_add(1).unwrap_or(NonZeroU32::MIN);
                set.pos_for_barrier.insert(id, *pos);
                set.barriers.push(ICBarrier::new(id, position));
            }
        }
    }
    set
}

async fn update_barriers(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    active_clients: &[(Position, Vec<EdgeRange>)],
    next_barrier_id: &mut NonZeroU32,
) -> Result<BarrierSet, ashpd::Error> {
    let zones = input_capture
        .zones(session, Default::default())
        .await?
        .response()?;
    log::debug!("zones: {zones:?}");

    let regions: Vec<RegionRect> = zones.regions().iter().map(RegionRect::new).collect();
    let set = select_barriers(&regions, active_clients, next_barrier_id);
    log::debug!("barriers: {:?}", set.barriers);
    log::debug!("client for barrier id: {:?}", set.pos_for_barrier);

    let ashpd_barriers: Vec<Barrier> = set.barriers.iter().copied().map(|b| b.into()).collect();
    let response = input_capture
        .set_pointer_barriers(
            session,
            &ashpd_barriers,
            zones.zone_set(),
            Default::default(),
        )
        .await?;
    let response = response.response()?;
    log::debug!("{response:?}");
    Ok(set)
}

/// The coordinate of the barrier line that was actually crossed.
///
/// Looked up by the activation's barrier id: several barriers can share one
/// edge (one per display), so the edge alone does not identify the line.
fn crossed_barrier_edge(
    barriers: &[ICBarrier],
    activated: &Activated,
    pos: Position,
) -> Option<f64> {
    let barrier_id = match activated.barrier_id() {
        Some(ActivatedBarrier::Barrier(id)) => Some(id),
        _ => None,
    };
    crossed_barrier_edge_of(barriers, barrier_id, activated.cursor_position(), pos)
}

/// [`crossed_barrier_edge`] without the portal types, so the lookup is testable.
fn crossed_barrier_edge_of(
    barriers: &[ICBarrier],
    barrier_id: Option<BarrierID>,
    cursor: Option<(f32, f32)>,
    pos: Position,
) -> Option<f64> {
    // Compositors are not required to report which barrier fired (KDE returns
    // `UnknownBarrier`), so fall back to the cursor position: pick the barrier
    // whose line is nearest to it. This matters because the reported cursor can
    // be outside every zone, and parking relative to it would leave the pointer
    // off-screen.
    let by_id = barrier_id.and_then(|id| barriers.iter().find(|b| b.barrier_id == id));
    let barrier = match by_id {
        Some(barrier) => barrier,
        None => {
            let (cx, cy) = cursor?;
            let distance = |b: &ICBarrier| {
                let (x1, y1, _, _) = b.position;
                match pos {
                    Position::Top | Position::Bottom => (y1 as f64 - cy as f64).abs(),
                    Position::Left | Position::Right => (x1 as f64 - cx as f64).abs(),
                }
            };
            barriers
                .iter()
                .min_by(|a, b| distance(a).total_cmp(&distance(b)))?
        }
    };
    let (x1, y1, _, _) = barrier.position;
    Some(match pos {
        Position::Top | Position::Bottom => y1 as f64,
        Position::Left | Position::Right => x1 as f64,
    })
}

/// Whether two barrier sets describe the same geometry.
///
/// Only the position matters: barrier ids are handed out from a counter that
/// changes on every rebuild, so comparing ids would report a change every time.
fn barriers_unchanged(candidate: &BarrierSet, current: &BarrierSet) -> bool {
    candidate.barriers.len() == current.barriers.len()
        && candidate
            .barriers
            .iter()
            .zip(current.barriers.iter())
            .all(|(a, b)| a.position == b.position)
}

/// Rebuild the pointer barriers when the compositor's zone layout changed.
///
/// Returns `Ok(None)` when nothing changed, so the caller can skip the
/// re-registration — the portal round trip is not free and doing it every tick
/// would churn the session.
async fn refresh_barriers_if_changed(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    active_clients: &[(Position, Vec<EdgeRange>)],
    state: &mut SessionState<'_>,
    current: &BarrierSet,
) -> Result<Option<BarrierSet>, ashpd::Error> {
    let zones = input_capture
        .zones(session, Default::default())
        .await?
        .response()?;

    // Compare the geometry we would register now against what is registered.
    let regions: Vec<RegionRect> = zones.regions().iter().map(RegionRect::new).collect();
    let mut probe = *state.next_barrier_id;
    let candidate = select_barriers(&regions, active_clients, &mut probe);

    if barriers_unchanged(&candidate, current) {
        return Ok(None);
    }

    let set = update_barriers(
        input_capture,
        session,
        active_clients,
        state.next_barrier_id,
    )
    .await?;
    Ok(Some(set))
}

async fn create_session(
    input_capture: &InputCapture,
) -> std::result::Result<(Session<InputCapture>, BitFlags<Capabilities>), ashpd::Error> {
    log::debug!("creating input capture session");
    let create_session_options = CreateSessionOptions::default().set_capabilities(
        Capabilities::Keyboard | Capabilities::Pointer | Capabilities::Touchscreen,
    );
    input_capture
        .create_session(None, create_session_options)
        .await
}

async fn connect_to_eis(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
) -> Result<(ei::Context, Connection, EiConvertEventStream), CaptureError> {
    log::debug!("connect_to_eis");
    let fd = input_capture
        .connect_to_eis(session, Default::default())
        .await?;

    // create unix stream from fd
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;

    // create ei context
    let context = ei::Context::new(stream)?;
    let (conn, event_stream) = context
        .handshake_tokio("de.feschber.LanMouse", ContextType::Receiver)
        .await?;

    Ok((context, conn, event_stream))
}

async fn libei_event_handler(
    mut ei_event_stream: EiConvertEventStream,
    context: ei::Context,
    event_tx: Sender<(Position, CaptureEvent)>,
    release_session: Arc<Notify>,
    current_pos: Rc<Cell<Option<Position>>>,
) -> Result<(), CaptureError> {
    loop {
        let ei_event = ei_event_stream
            .next()
            .await
            .ok_or(CaptureError::EndOfStream)??;
        log::trace!("from ei: {ei_event:?}");
        let client = current_pos.get();
        handle_ei_event(ei_event, client, &context, &event_tx, &release_session).await?;
    }
}

/// How long to wait for the capture task to stop before abandoning it.
const TERMINATE_TIMEOUT: Duration = Duration::from_secs(3);

impl LibeiInputCapture {
    pub async fn new() -> std::result::Result<Self, LibeiCaptureCreationError> {
        let input_capture = Arc::new(InputCapture::new().await?);
        let first_session = Some(create_session(&input_capture).await?);

        let (event_tx, event_rx) = mpsc::channel(1);
        let (notify_capture, notify_rx) = mpsc::channel(1);
        let notify_release = Arc::new(Notify::new());

        let cancellation_token = CancellationToken::new();
        let layout = Arc::new(Mutex::new(Layout::default()));

        let capture = do_capture(
            Arc::clone(&input_capture),
            notify_rx,
            notify_release.clone(),
            first_session,
            event_tx,
            cancellation_token.clone(),
            Arc::clone(&layout),
        );
        let capture_task = tokio::task::spawn_local(capture);

        let producer = Self {
            input_capture,
            event_rx,
            capture_task,
            notify_capture,
            notify_release,
            cancellation_token,
            terminated: false,
            layout,
        };

        Ok(producer)
    }
}

async fn do_capture(
    input_capture: Arc<InputCapture>,
    mut capture_event: Receiver<LibeiNotifyEvent>,
    notify_release: Arc<Notify>,
    session: Option<(Session<InputCapture>, BitFlags<Capabilities>)>,
    event_tx: Sender<(Position, CaptureEvent)>,
    cancellation_token: CancellationToken,
    layout: Arc<Mutex<Layout>>,
) -> Result<(), CaptureError> {
    let mut session = session.map(|s| s.0);

    /* the task owns the `Arc`, so this borrow is valid for the whole task */
    let input_capture = input_capture.as_ref();
    /* one entry per position: the segments of every client that shares it
     * are resolved by the caller, so the barriers cover exactly the parts of
     * the edge that are owned */
    let mut active_clients: Vec<(Position, Vec<EdgeRange>)> = vec![];
    let mut next_barrier_id = NonZeroU32::new(1).expect("id must be non-zero");

    let mut zones_changed = input_capture.receive_zones_changed().await?;

    loop {
        // do capture session
        let cancel_session = CancellationToken::new();
        let cancel_update = CancellationToken::new();

        let mut capture_event_occured: Option<LibeiNotifyEvent> = None;

        // kill session if clients need to be updated
        let handle_session_update_request = async {
            tokio::select! {
                _ = cancellation_token.cancelled() => {
                    log::debug!("cancelled")
                }, /* exit requested */
                _ = cancel_update.cancelled() => {
                    log::debug!("update task cancelled");
                }, /* session exited */
                zones = zones_changed.next() => match zones {
                    Some(_) => {
                        log::debug!("zones changed!");
                    }
                    None => {
                        // The portal stream ended (portal restart or session
                        // invalidated). Leaving the branch ready would make
                        // this select spin forever and recreate the session in
                        // a tight loop, so park it instead.
                        log::warn!("zones changed stream closed");
                        std::future::pending::<()>().await;
                    }
                }, /* zones have changed */
                e = capture_event.recv() => if let Some(e) = e { /* clients changed */
                    log::debug!("capture event: {e:?}");
                    capture_event_occured.replace(e);
                },
            }
            // kill session (might already be dead!)
            log::debug!("=> cancelling session");
            cancel_session.cancel();
        };

        if !active_clients.is_empty() {
            // create session
            let mut session = match session.take() {
                Some(s) => s,
                None => create_session(input_capture).await?.0,
            };

            let mut state = SessionState {
                next_barrier_id: &mut next_barrier_id,
                layout: &layout,
            };
            let capture_session = do_capture_session(
                input_capture,
                &mut session,
                &event_tx,
                &active_clients,
                &mut state,
                &notify_release,
                (cancel_session.clone(), cancel_update.clone()),
            );

            let (capture_result, ()) = tokio::join!(capture_session, handle_session_update_request);
            log::debug!("capture session + session_update task done!");

            // disable capture
            log::debug!("disabling input capture");
            if let Err(e) = input_capture.disable(&session, Default::default()).await {
                log::warn!("input_capture.disable(&session) {e}");
            }
            if let Err(e) = session.close().await {
                log::warn!("session.close(): {e}");
            }

            // propagate error from capture session
            capture_result?;
        } else {
            handle_session_update_request.await;
        }

        // update clients if requested
        if let Some(event) = capture_event_occured.take() {
            match event {
                LibeiNotifyEvent::Create(p, edges) => {
                    match active_clients.iter_mut().find(|(pos, _)| *pos == p) {
                        Some(entry) => entry.1 = edges,
                        None => active_clients.push((p, edges)),
                    }
                }
                LibeiNotifyEvent::Destroy(p) => active_clients.retain(|(pos, _)| *pos != p),
            }
        }

        // break
        if cancellation_token.is_cancelled() {
            break Ok(());
        }
    }
}

/// The bookkeeping that has to survive a capture session.
///
/// Barrier ids must not be reused across sessions (the portal keeps the old ones
/// registered until the session is closed), and the layout size is discovered
/// inside a session, where the zones are actually read, but published for the
/// frontend to pick up at any time.
struct SessionState<'a> {
    next_barrier_id: &'a mut NonZeroU32,
    layout: &'a Mutex<Layout>,
}

async fn do_capture_session(
    input_capture: &InputCapture,
    session: &mut Session<InputCapture>,
    event_tx: &Sender<(Position, CaptureEvent)>,
    active_clients: &[(Position, Vec<EdgeRange>)],
    state: &mut SessionState<'_>,
    notify_release: &Notify,
    cancel: (CancellationToken, CancellationToken),
) -> Result<(), CaptureError> {
    let (cancel_session, cancel_update) = cancel;
    // current client
    let current_pos = Rc::new(Cell::new(None));

    // connect to eis server
    let (context, _conn, ei_event_stream) = connect_to_eis(input_capture, session).await?;

    // set barriers
    let mut barrier_set = update_barriers(
        input_capture,
        session,
        active_clients,
        state.next_barrier_id,
    )
    .await?;
    state
        .layout
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone_from(&barrier_set.layout);

    log::debug!("enabling session");
    input_capture.enable(session, Default::default()).await?;

    // cancellation token to release session
    let release_session = Arc::new(Notify::new());

    // async event task
    let cancel_ei_handler = CancellationToken::new();
    let event_chan = event_tx.clone();
    let pos = current_pos.clone();
    let cancel_session_clone = cancel_session.clone();
    let release_session_clone = release_session.clone();
    let cancel_ei_handler_clone = cancel_ei_handler.clone();
    let ei_task = async move {
        tokio::select! {
            r = libei_event_handler(
                ei_event_stream,
                context,
                event_chan,
                release_session_clone,
                pos,
            ) => {
                log::debug!("libei exited: {r:?} cancelling session task");
                cancel_session_clone.cancel();
            }
            _ = cancel_ei_handler_clone.cancelled() => {},
        }
        Ok::<(), CaptureError>(())
    };

    let capture_session_task = async {
        // receiver for activation tokens
        let mut activated = input_capture.receive_activated().await?;
        let mut ei_devices_changed = false;
        // `barrier_set` is moved into this task and replaced on a refresh.
        // Display layout can change while we run (monitor unplugged, mode
        // switch). The zones the compositor reports then no longer match the
        // barriers we registered, so the pointer can never cross back — the
        // "stuck after changing monitors" symptom of issue #512.
        //
        // Each check is a D-Bus round trip, so the cadence follows how likely a
        // change is to matter: often while the pointer is on a peer (a stale
        // barrier strands it there), rarely while it is local. The first tick
        // is skipped so a fresh session does not re-read zones it just
        // registered.
        const ZONE_CHECK_ACTIVE: Duration = Duration::from_secs(5);
        const ZONE_CHECK_IDLE: Duration = Duration::from_secs(30);
        let mut zone_check = tokio::time::interval_at(
            tokio::time::Instant::now() + ZONE_CHECK_IDLE,
            ZONE_CHECK_IDLE,
        );
        zone_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                activated = activated.next() => {
                    let activated = activated.ok_or(CaptureError::ActivationClosed)?;
                    log::debug!("activated: {activated:?}");

                    // The compositor is allowed to omit the cursor position. In
                    // that case we cannot map an unknown barrier back to a
                    // client, so only activations that report a barrier id can
                    // be handled.
                    let cursor_position = activated.cursor_position();

                    // get barrier id from activation
                    let barrier_id = match activated.barrier_id() {
                        Some(ActivatedBarrier::Barrier(id)) => Some(id),
                        // workaround for KDE plasma not reporting barrier ids
                        Some(ActivatedBarrier::UnknownBarrier) | None => {
                            cursor_position.and_then(|p| {
                                find_corresponding_client(&barrier_set.barriers, p)
                            })
                        }
                    };

                    // find client corresponding to barrier
                    let pos = barrier_id
                        .and_then(|id| barrier_set.pos_for_barrier.get(&id).copied())
                        .or_else(|| {
                            let cursor_position = cursor_position?;
                            // The compositor reported a barrier id that we did not
                            // create (or none at all): fall back to the closest
                            // barrier instead of panicking.
                            log::warn!("invalid barrier id {barrier_id:?}, using the closest barrier");
                            find_corresponding_client(&barrier_set.barriers, cursor_position)
                                .and_then(|id| barrier_set.pos_for_barrier.get(&id).copied())
                        });

                    let Some(pos) = pos else {
                        log::warn!("unable to match the activation to a capture position, ignoring it");
                        continue;
                    };
                    current_pos.replace(Some(pos));

                    /* client entered => send event.
                     *
                     * The portal reports the cursor position, and the barrier
                     * geometry says how wide the edge is, so the point the
                     * pointer crossed can be recovered. Reporting the middle of
                     * the edge for every crossing - which is what happened while
                     * this was hardcoded to 0.5 - parks the peer's cursor in the
                     * middle too, which is what made crossing back and forth feel
                     * like the cursor teleporting. */
                    let fraction = match (barrier_set.axes.get(&pos), cursor_position) {
                        (Some(axis), Some(cursor)) => {
                            crossing_fraction(*axis, cursor, pos)
                        }
                        /* a compositor that reports neither the axis nor the
                         * cursor cannot be helped: keep the old behaviour */
                        _ => 0.5,
                    };
                    if event_tx
                        .send((pos, CaptureEvent::Begin(fraction)))
                        .await
                        .is_err()
                    {
                        log::debug!("capture event channel closed");
                        break;
                    }

                    tokio::select! {
                        _ = notify_release.notified() => { /* capture release */
                            log::debug!("release session requested");
                        },
                        _ = release_session.notified() => { /* release session */
                            log::debug!("ei devices changed");
                            ei_devices_changed = true;
                        },
                        _ = cancel_session.cancelled() => { /* kill session notify */
                            log::debug!("session cancel requested");
                            break
                        },
                    }

                    release_capture(input_capture, session, activated, pos, &barrier_set.barriers)
                        .await?;

                    // The session is no longer capturing: stop forwarding
                    // events to the client we just left, otherwise events left
                    // in the socket would be replayed to it.
                    current_pos.replace(None);
                }
                _ = zone_check.tick() => {
                    // While the pointer is on a peer, a stale barrier is
                    // disruptive, so check more often.
                    let period = if current_pos.get().is_some() {
                        ZONE_CHECK_ACTIVE
                    } else {
                        ZONE_CHECK_IDLE
                    };
                    zone_check = tokio::time::interval_at(
                        tokio::time::Instant::now() + period,
                        period,
                    );
                    zone_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    match refresh_barriers_if_changed(
                        input_capture,
                        session,
                        active_clients,
                        state,
                        &barrier_set,
                    )
                    .await
                    {
                        Ok(Some(set)) => {
                            log::info!(
                                "display layout changed — rebuilding {} barrier(s)",
                                set.barriers.len()
                            );
                            state
                                .layout
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone_from(&set.layout);
                            barrier_set = set;
                        }
                        Ok(None) => log::trace!("barrier layout unchanged"),
                        Err(e) => log::warn!("could not refresh barriers: {e}"),
                    }
                }
                _ = notify_release.notified() => { /* capture release -> we are not capturing anyway, so ignore */
                    log::debug!("release session requested");
                },
                _ = release_session.notified() => { /* release session */
                    log::debug!("ei devices changed");
                    ei_devices_changed = true;
                },
                _ = cancel_session.cancelled() => { /* kill session notify */
                    log::debug!("session cancel requested");
                    break
                },
            }
            if ei_devices_changed {
                /* for whatever reason, GNOME seems to kill the session
                 * as soon as devices are added or removed, so we need
                 * to cancel */
                break;
            }
        }
        // cancel libei task
        log::debug!("session exited: killing libei task");
        cancel_ei_handler.cancel();
        Ok::<(), CaptureError>(())
    };

    let (a, b) = tokio::join!(ei_task, capture_session_task);

    cancel_update.cancel();

    log::debug!("both session and ei task finished!");
    a?;
    b?;

    Ok(())
}

async fn release_capture(
    input_capture: &InputCapture,
    session: &Session<InputCapture>,
    activated: Activated,
    current_pos: Position,
    barriers: &[ICBarrier],
) -> Result<(), CaptureError> {
    if let Some(activation_id) = activated.activation_id() {
        log::debug!("releasing input capture {activation_id}");
    }
    let (x, y) = activated.cursor_position().unwrap_or_else(|| {
        log::debug!("compositor did not report a cursor position, releasing at (0, 0)");
        (0., 0.)
    });
    log::debug!("client entered @ ({x}, {y})");
    // Park the cursor a fixed distance inside the edge, measured from the
    // barrier line rather than from wherever the cursor happened to be.
    //
    // Offsetting the cursor by one pixel was not enough: the reported position
    // can already be past the barrier, so `+1` still left it inside the region
    // that triggers the edge. The very next motion sample then re-entered the
    // peer ~1 ms after leaving it, which is what made the pointer stall on the
    // boundary instead of returning cleanly.
    const PARK_MARGIN: f64 = 24.;
    let cursor_position = match current_pos {
        Position::Top => {
            let edge = crossed_barrier_edge(barriers, &activated, current_pos).unwrap_or(y as f64);
            (x as f64, edge + PARK_MARGIN)
        }
        Position::Bottom => {
            let edge = crossed_barrier_edge(barriers, &activated, current_pos).unwrap_or(y as f64);
            (x as f64, edge - PARK_MARGIN)
        }
        Position::Left => {
            let edge = crossed_barrier_edge(barriers, &activated, current_pos).unwrap_or(x as f64);
            (edge + PARK_MARGIN, y as f64)
        }
        Position::Right => {
            let edge = crossed_barrier_edge(barriers, &activated, current_pos).unwrap_or(x as f64);
            (edge - PARK_MARGIN, y as f64)
        }
    };
    let release_options = ReleaseOptions::default()
        .set_activation_id(activated.activation_id())
        .set_cursor_position(Some(cursor_position));
    input_capture.release(session, release_options).await?;
    Ok(())
}

fn find_corresponding_client(barriers: &[ICBarrier], pos: (f32, f32)) -> Option<BarrierID> {
    let Some(closest) = barriers.iter().copied().min_by_key(|b| {
        let (x1, y1, x2, y2) = b.position;
        let (x1, y1, x2, y2) = (x1 as f32, y1 as f32, x2 as f32, y2 as f32);
        distance_to_line(((x1, y1), (x2, y2)), pos) as i32
    }) else {
        log::warn!("could not find barrier corresponding to client");
        return None;
    };
    Some(closest.barrier_id)
}

fn distance_to_line(line: ((f32, f32), (f32, f32)), p: (f32, f32)) -> f32 {
    let ((x1, y1), (x2, y2)) = line;
    let (x0, y0) = p;
    /*
     * we use the fact that for the triangle spanned by the line and p,
     * the height of the triangle is the desired distance and can be calculated by
     * h = 2A / b with b being the line_length and
     */
    let double_triangle_area = ((y2 - y1) * x0 - (x2 - x1) * y0 + x2 * y1 - y2 * x1).abs();
    let line_length = ((y2 - y1).powf(2.0) + (x2 - x1).powf(2.0)).sqrt();
    let distance = double_triangle_area / line_length;
    log::debug!("distance to line({line:?}, {p:?}) = {distance}");
    distance
}

async fn handle_ei_event(
    ei_event: EiEvent,
    current_client: Option<Position>,
    context: &ei::Context,
    event_tx: &Sender<(Position, CaptureEvent)>,
    release_session: &Notify,
) -> Result<(), CaptureError> {
    let all_capabilities = DeviceCapability::Pointer
        | DeviceCapability::PointerAbsolute
        | DeviceCapability::Keyboard
        | DeviceCapability::Touch
        | DeviceCapability::Scroll
        | DeviceCapability::Button;
    match ei_event {
        EiEvent::SeatAdded(s) => {
            s.seat.bind_capabilities(all_capabilities);
            context.flush().map_err(|e| io::Error::new(e.kind(), e))?;
        }
        EiEvent::SeatRemoved(_) => {
            log::debug!("releasing session: {ei_event:?}");
            release_session.notify_waiters();
        }
        /* EiEvent::DeviceAdded(_) | */
        EiEvent::DeviceRemoved(_) => {
            if *RESTART_SESSION_ON_DEVICE_CHANGE {
                log::debug!("releasing session: {ei_event:?}");
                release_session.notify_waiters();
            } else {
                log::debug!("ignoring device change: {ei_event:?}");
            }
        }
        EiEvent::DevicePaused(_) | EiEvent::DeviceResumed(_) => {}
        EiEvent::DeviceStartEmulating(_) => log::debug!("START EMULATING"),
        EiEvent::DeviceStopEmulating(_) => log::debug!("STOP EMULATING"),
        EiEvent::Disconnected(d) => {
            return Err(CaptureError::Disconnected(format!("{:?}", d.reason)));
        }
        _ => {
            if let Some(pos) = current_client {
                for event in Event::from_ei_event(ei_event) {
                    if event_tx
                        .send((pos, CaptureEvent::Input(event)))
                        .await
                        .is_err()
                    {
                        // the consumer is gone, nothing left to deliver to
                        return Ok(());
                    }
                }
            }
        }
    }
    Ok(())
}

impl LibeiInputCapture {
    /// Hand one message to the capture task.
    ///
    /// The channel has room for a single message and the task is the only reader,
    /// so a `send` blocks as soon as the task is gone. That is not hypothetical:
    /// the task ends when the portal session dies, and the next `create_captures`
    /// would then wait for a reader that will never come - the capture never
    /// restarted and nothing was logged, which is the worst of both. A closed
    /// channel is reported as a disconnect instead, so the retry loop can act on
    /// it.
    async fn notify(&self, event: LibeiNotifyEvent) -> Result<(), CaptureError> {
        self.notify_capture
            .send(event)
            .await
            .map_err(|_| CaptureError::Disconnected("libei capture task is gone".to_owned()))
    }
}

#[async_trait]
impl LanMouseInputCapture for LibeiInputCapture {
    async fn begin(&mut self, _pos: Position) -> Result<(), CaptureError> {
        /* The compositor owns the pointer: it can only be handed out by
         * crossing a barrier. */
        Err(CaptureError::BeginUnsupported)
    }

    async fn create(
        &mut self,
        pos: Position,
        segments: Vec<EdgeRange>,
    ) -> Result<(), CaptureError> {
        self.notify(LibeiNotifyEvent::Create(pos, segments)).await
    }

    async fn update(
        &mut self,
        pos: Position,
        segments: Vec<EdgeRange>,
    ) -> Result<(), CaptureError> {
        /* the same message as a new client: the task replaces the segments it
         * registers for this position */
        self.notify(LibeiNotifyEvent::Create(pos, segments)).await
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.notify(LibeiNotifyEvent::Destroy(pos)).await
    }

    async fn layout(&mut self) -> Option<Layout> {
        /* a poisoned lock still holds a usable layout: the task that panicked was
         * not the one that would corrupt it, and refusing to report any geometry
         * for the rest of the process is worse than reading it */
        let layout = self
            .layout
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        (!layout.is_empty()).then_some(layout)
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        // `notify_one` stores a permit when nobody is waiting, so a release
        // request can not be lost between two `notified()` polls.
        self.notify_release.notify_one();
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        self.cancellation_token.cancel();
        let task = &mut self.capture_task;
        log::debug!("waiting for capture to terminate...");
        let res = if !task.is_finished() {
            /* Bounded, because the task can be stuck in a portal call that will
             * never answer. Waiting forever here is what stopped the capture from
             * ever being restarted: `do_capture` never returned, so the retry loop
             * never ran, and the pointer silently stopped crossing edges. */
            match tokio::time::timeout(TERMINATE_TIMEOUT, &mut *task).await {
                Ok(Ok(res)) => res,
                Ok(Err(e)) => {
                    log::error!("libei capture task panicked: {e}");
                    Err(CaptureError::Io(io::Error::other(format!(
                        "libei capture task panicked: {e}"
                    ))))
                }
                Err(_) => {
                    /* The task is abandoned rather than awaited: it holds a portal
                     * session that the portal itself will reclaim when the process
                     * closes it or the session times out. Leaking one task is much
                     * better than a daemon that never captures again. */
                    log::warn!(
                        "libei capture task did not stop within {} s, abandoning it",
                        TERMINATE_TIMEOUT.as_secs()
                    );
                    task.abort();
                    Ok(())
                }
            }
        } else {
            Ok(())
        };
        self.terminated = true;
        log::debug!("done!");
        res
    }
}

impl Drop for LibeiInputCapture {
    fn drop(&mut self) {
        if !self.terminated {
            /* this workaround is needed until async drop is stabilized */
            log::error!("LibeiInputCapture dropped without being terminated!");
        }
    }
}

impl Stream for LibeiInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        // Deliver queued events first: events that were sent just before the
        // capture task ended must not be dropped.
        if let Poll::Ready(event) = self.event_rx.poll_recv(cx) {
            if event.is_some() {
                return Poll::Ready(event.map(Result::Ok));
            }
        }

        if self.terminated {
            return Poll::Ready(None);
        }

        // The capture task owns the only sender, so waiting on the channel also
        // observes the task ending (dropping the sender wakes us). Polling a
        // finished JoinHandle would panic, so only join once it is done.
        if !self.capture_task.is_finished() {
            return Poll::Pending;
        }

        self.terminated = true;
        match self.capture_task.poll_unpin(cx) {
            Poll::Ready(Ok(Ok(()))) => Poll::Ready(None),
            Poll::Ready(Ok(Err(e))) => Poll::Ready(Some(Err(e))),
            Poll::Ready(Err(e)) => Poll::Ready(Some(Err(CaptureError::Io(io::Error::other(
                format!("libei capture task failed: {e}"),
            ))))),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod barrier_tests {
    use super::*;

    fn id(n: u32) -> BarrierID {
        NonZeroU32::new(n).expect("nonzero")
    }

    fn barrier(position: (i32, i32, i32, i32)) -> ICBarrier {
        ICBarrier::new(id(1), position)
    }

    fn barrier_at(barrier_id: u32, position: (i32, i32, i32, i32)) -> ICBarrier {
        ICBarrier::new(id(barrier_id), position)
    }

    /// A barrier set holding exactly these barriers: only the geometry is
    /// compared, so the rest of the set is irrelevant here.
    fn set(barriers: Vec<ICBarrier>) -> BarrierSet {
        BarrierSet {
            barriers,
            ..BarrierSet::default()
        }
    }

    fn region(x: i32, y: i32, w: i32, h: i32) -> RegionRect {
        RegionRect { x, y, w, h }
    }

    #[test]
    fn park_position_uses_the_barrier_line_not_the_reported_cursor() {
        // The compositor reported an out-of-range cursor (Hyprland does this
        // when it constrains the pointer); the park position must come from
        // the barrier, otherwise the cursor is parked off-screen and the edge
        // keeps firing.
        let barriers = vec![
            barrier_at(1, (2560, 0, 3999, 0)),
            barrier_at(2, (0, 1120, 2559, 1120)),
        ];
        assert_eq!(
            crossed_barrier_edge_of(&barriers, None, Some((1000., 3410.)), Position::Top),
            Some(1120.)
        );
    }

    #[test]
    fn park_position_uses_the_reported_barrier_id() {
        let barriers = vec![
            barrier_at(1, (2560, 0, 3999, 0)),
            barrier_at(2, (0, 1120, 2559, 1120)),
        ];
        assert_eq!(
            crossed_barrier_edge_of(&barriers, Some(id(2)), Some((1000., 3410.)), Position::Top),
            Some(1120.)
        );
    }

    #[test]
    fn identical_geometry_is_unchanged() {
        let a = set(vec![barrier((0, 0, 10, 0))]);
        let b = set(vec![barrier((0, 0, 10, 0))]);
        assert!(barriers_unchanged(&a, &b));
    }

    #[test]
    fn moved_edge_is_a_change() {
        // A display rearrangement moves the top edge; the barrier must be
        // rebuilt or the pointer can never cross back (issue #512).
        let a = set(vec![barrier((0, 0, 10, 0))]);
        let b = set(vec![barrier((0, -100, 10, -100))]);
        assert!(!barriers_unchanged(&a, &b));
    }

    #[test]
    fn different_barrier_count_is_a_change() {
        let a = set(vec![barrier((0, 0, 10, 0))]);
        let b = set(vec![barrier((0, 0, 10, 0)), barrier((0, 20, 10, 20))]);
        assert!(!barriers_unchanged(&a, &b));
    }

    #[test]
    fn ids_are_ignored() {
        let a = set(vec![ICBarrier::new(id(1), (0, 0, 10, 0))]);
        let b = set(vec![ICBarrier::new(id(7), (0, 0, 10, 0))]);
        assert!(barriers_unchanged(&a, &b));
    }

    #[test]
    fn a_full_range_spans_the_whole_edge_of_one_display() {
        let regions = [region(0, 0, 1920, 1080)];
        let axis = layout_axis(&regions, Position::Right).expect("axis");
        assert_eq!(axis, (0, 1079));
        assert_eq!(
            segment_barrier(regions[0], Position::Right, axis, EdgeRange::FULL),
            Some((1920, 0, 1920, 1079))
        );
    }

    #[test]
    fn a_half_range_spans_half_the_edge() {
        let regions = [region(0, 0, 1920, 1080)];
        let axis = layout_axis(&regions, Position::Right).expect("axis");
        let top = EdgeRange::from_fractions(0.0, 0.5).expect("range");
        let bottom = EdgeRange::from_fractions(0.5, 1.0).expect("range");
        assert_eq!(
            segment_barrier(regions[0], Position::Right, axis, top),
            Some((1920, 0, 1920, 539))
        );
        assert_eq!(
            segment_barrier(regions[0], Position::Right, axis, bottom),
            Some((1920, 540, 1920, 1079))
        );
    }

    #[test]
    fn the_axis_is_the_whole_layout_not_one_display() {
        // Two 1080 px displays stacked: a client owning the bottom half of the
        // right edge reaches the lower display only, and the ranges are measured
        // over both displays.
        let regions = [region(0, 0, 1920, 1080), region(0, 1080, 1920, 1080)];
        let axis = layout_axis(&regions, Position::Right).expect("axis");
        assert_eq!(axis, (0, 2159));
        let bottom = EdgeRange::from_fractions(0.5, 1.0).expect("range");
        // the lower display carries the bottom half of the edge ...
        assert_eq!(
            segment_barrier(regions[1], Position::Right, axis, bottom),
            Some((1920, 1080, 1920, 2159))
        );
        // ... and the upper one is entirely outside that range, so it gets no
        // barrier at all: the pointer cannot leave the layout there
        assert_eq!(
            segment_barrier(regions[0], Position::Right, axis, bottom),
            None
        );
        let top = EdgeRange::from_fractions(0.0, 0.5).expect("range");
        assert_eq!(
            segment_barrier(regions[0], Position::Right, axis, top),
            Some((1920, 0, 1920, 1079))
        );
    }

    #[test]
    fn a_range_outside_the_axis_yields_no_barrier() {
        let regions = [region(0, 0, 1920, 1080)];
        let axis = layout_axis(&regions, Position::Top).expect("axis");
        assert_eq!(axis, (0, 1919));
        let right_half = EdgeRange::from_fractions(0.5, 1.0).expect("range");
        assert_eq!(
            segment_barrier(regions[0], Position::Top, axis, right_half),
            Some((960, 0, 1919, 0))
        );
    }

    #[test]
    fn the_crossing_point_is_reported_as_a_fraction_of_the_edge() {
        let axis = (0, 1079);
        assert_eq!(crossing_fraction(axis, (1920., 0.), Position::Right), 0.0);
        assert_eq!(crossing_fraction(axis, (1920., 540.), Position::Right), 0.5);
        /* The span is the height of the edge in pixels, so the last pixel reads
         * 1079/1080 rather than exactly 1.0 - the same reading the macOS and
         * Windows backends give. What matters is that it still falls inside the
         * range that ends at the far end of the edge. */
        let last = crossing_fraction(axis, (1920., 1079.), Position::Right);
        assert!(last > 0.999 && last < 1.0, "{last}");
        let bottom_half = EdgeRange::from_fractions(0.5, 1.0).expect("range");
        assert!(bottom_half.contains(last));
        // the horizontal axis for a top edge
        assert_eq!(
            crossing_fraction((0, 1919), (480., 0.), Position::Top),
            0.25
        );
        // a compositor reporting an out-of-range cursor is clamped
        assert_eq!(
            crossing_fraction(axis, (1920., 4000.), Position::Right),
            1.0
        );
        assert_eq!(crossing_fraction(axis, (1920., -50.), Position::Right), 0.0);
    }

    #[test]
    fn select_barriers_creates_one_per_segment_and_region() {
        let regions = [region(0, 0, 100, 100)];
        let mut next = NonZeroU32::new(1).expect("nonzero");
        let top = EdgeRange::from_fractions(0.0, 0.5).expect("range");
        let bottom = EdgeRange::from_fractions(0.5, 1.0).expect("range");
        let clients = [(Position::Right, vec![top, bottom])];
        let set = select_barriers(&regions, &clients, &mut next);
        assert_eq!(set.barriers.len(), 2);
        assert_eq!(set.barriers[0].position, (100, 0, 100, 49));
        assert_eq!(set.barriers[1].position, (100, 50, 100, 99));
        assert_eq!(set.layout.size(), Some((100, 100)));
        // every barrier maps back to the position it was created for
        for barrier in &set.barriers {
            assert_eq!(
                set.pos_for_barrier.get(&barrier.barrier_id),
                Some(&Position::Right)
            );
        }
    }

    #[test]
    fn an_uncovered_edge_gets_no_barrier() {
        let regions = [region(0, 0, 100, 100)];
        let mut next = NonZeroU32::new(1).expect("nonzero");
        let set = select_barriers(&regions, &[(Position::Right, vec![])], &mut next);
        assert!(set.barriers.is_empty());
    }

    #[test]
    fn no_region_means_no_axis_means_no_barrier() {
        let mut next = NonZeroU32::new(1).expect("nonzero");
        let set = select_barriers(&[], &[(Position::Right, vec![EdgeRange::FULL])], &mut next);
        assert!(set.barriers.is_empty());
        assert_eq!(set.layout.size(), None);
    }
}
