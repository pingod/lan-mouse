use std::task::Poll;

use async_trait::async_trait;
use futures_core::Stream;

use super::{
    Capture, CaptureError, CaptureEvent, EdgeRange, Position, error::X11InputCaptureCreationError,
};

pub struct X11InputCapture {}

impl X11InputCapture {
    pub fn new() -> std::result::Result<Self, X11InputCaptureCreationError> {
        Err(X11InputCaptureCreationError::NotImplemented)
    }
}

#[async_trait]
impl Capture for X11InputCapture {
    async fn create(
        &mut self,
        _pos: Position,
        _segments: Vec<EdgeRange>,
    ) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn update(
        &mut self,
        _pos: Position,
        _segments: Vec<EdgeRange>,
    ) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn screen_size(&mut self) -> Option<(u32, u32)> {
        None
    }

    async fn begin(&mut self, _pos: Position) -> Result<(), CaptureError> {
        /* The compositor decides who owns the pointer: it cannot be taken
         * without the pointer actually crossing a barrier. */
        Err(CaptureError::BeginUnsupported)
    }

    async fn destroy(&mut self, _pos: Position) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn release(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }

    async fn terminate(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

impl Stream for X11InputCapture {
    type Item = Result<(Position, CaptureEvent), CaptureError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // This backend never produces events (`new()` always fails with
        // `NotImplemented`), so the stream terminates immediately instead of
        // returning a `Pending` without a registered waker.
        Poll::Ready(None)
    }
}
