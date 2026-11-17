use async_trait::async_trait;
use core::task::{Context, Poll};
use event_thread::EventThread;
use futures::Stream;
use std::pin::Pin;

use std::task::ready;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use super::{Capture, CaptureError, CaptureEvent, EdgeRange, Position};
use input_event::{DisplayInfo, Layout, Rotation};

mod display_util;
mod event_thread;

pub struct WindowsInputCapture {
    event_rx: UnboundedReceiver<(Position, CaptureEvent)>,
    event_thread: EventThread,
}

#[async_trait]
impl Capture for WindowsInputCapture {
    async fn create(
        &mut self,
        pos: Position,
        segments: Vec<EdgeRange>,
    ) -> Result<(), CaptureError> {
        self.event_thread.create(pos, segments);
        Ok(())
    }

    async fn update(
        &mut self,
        pos: Position,
        segments: Vec<EdgeRange>,
    ) -> Result<(), CaptureError> {
        /* the hook checks the segments on every crossing, so replacing them is
         * enough - there is no barrier object to move */
        self.event_thread.create(pos, segments);
        Ok(())
    }

    async fn layout(&mut self) -> Option<Layout> {
        /* Windows reports the bounding box of the desktop, not the individual
         * monitors: `SCREEN_SIZE` is what the event thread keeps up to date and it is
         * packed from `layout_bounds`. One `DisplayInfo` covering that box is what this
         * backend can honestly say - it is the area the pointer can reach, which is what
         * the edge ranges are measured against. Reporting per-monitor rectangles here
         * would mean reading the enumerated list across threads, and a wrong rectangle
         * is worse than a coarse one. */
        let (width, height) = event_thread::screen_size()?;
        Some(Layout::new(vec![DisplayInfo::new(
            "desktop",
            0,
            0,
            width,
            height,
            Rotation::Normal,
        )]))
    }

    async fn begin(&mut self, _pos: Position) -> Result<(), CaptureError> {
        /* The compositor decides who owns the pointer: it cannot be taken
         * without the pointer actually crossing a barrier. */
        Err(CaptureError::BeginUnsupported)
    }

    async fn destroy(&mut self, pos: Position) -> Result<(), CaptureError> {
        self.event_thread.destroy(pos);
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        self.event_thread.release_capture();
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl WindowsInputCapture {
    pub(crate) fn new() -> Self {
        /* unbounded: the producer runs inside a Windows hook and must never
         * block, and dropping input there loses pointer deltas and key-ups
         */
        let (event_tx, event_rx) = unbounded_channel();
        let event_thread = EventThread::new(event_tx);
        Self {
            event_thread,
            event_rx,
        }
    }
}

impl Stream for WindowsInputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}
