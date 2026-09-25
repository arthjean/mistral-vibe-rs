//! Turns a response body into chunks as it arrives.
//!
//! Both backends read a stream the same way: bytes go into a dialect's event
//! source, which releases zero or more chunks per piece, and whatever it still
//! holds when the body ends is released last. The only difference is the
//! framing, which the [`EventSource`] owns.

use std::collections::VecDeque;
use std::pin::Pin;

use futures_util::{Stream, StreamExt};

use super::error::{BackendFailure, TransportFailure};
use super::transport::BodyStream;
use super::types::Chunk;

/// A stream of answer pieces.
pub type ChunkStream<'a> = Pin<Box<dyn Stream<Item = Result<Chunk, BackendFailure>> + Send + 'a>>;

/// Reads one framing of an event stream.
pub trait EventSource: Send {
    /// The chunks `bytes` completes.
    ///
    /// # Errors
    ///
    /// An event the source cannot read.
    fn push(&mut self, bytes: &[u8]) -> Result<Vec<Chunk>, BackendFailure>;

    /// The chunks still held when the body ended.
    ///
    /// # Errors
    ///
    /// A final event the source cannot read.
    fn finish(&mut self) -> Result<Vec<Chunk>, BackendFailure>;

    /// Whether the source saw the end of the stream before the body ended.
    fn done(&self) -> bool;

    /// A failure the source met after releasing chunks, reported once they
    /// are read.
    fn take_failure(&mut self) -> Option<BackendFailure> {
        None
    }
}

/// Pulls chunks out of a body through an [`EventSource`].
pub struct ChunkPump<S> {
    body: BodyStream,
    source: S,
    queue: VecDeque<Chunk>,
    finished: bool,
    on_transport: Box<dyn Fn(TransportFailure) -> BackendFailure + Send + Sync>,
}

impl<S: EventSource> ChunkPump<S> {
    pub fn new(
        body: BodyStream,
        source: S,
        on_transport: Box<dyn Fn(TransportFailure) -> BackendFailure + Send + Sync>,
    ) -> Self {
        Self {
            body,
            source,
            queue: VecDeque::new(),
            finished: false,
            on_transport,
        }
    }

    /// The next chunk, reading as much of the body as it takes.
    ///
    /// # Errors
    ///
    /// The body failing, or an event the source refuses.
    pub async fn next(&mut self) -> Result<Option<Chunk>, BackendFailure> {
        loop {
            if let Some(chunk) = self.queue.pop_front() {
                return Ok(Some(chunk));
            }
            if let Some(failure) = self.source.take_failure() {
                self.finished = true;
                return Err(failure);
            }
            if self.finished {
                return Ok(None);
            }
            if self.source.done() {
                self.finished = true;
                self.queue.extend(self.source.finish()?);
                continue;
            }
            match self.body.next().await {
                Some(Ok(bytes)) => self.queue.extend(self.source.push(&bytes)?),
                Some(Err(failure)) => {
                    self.finished = true;
                    return Err((self.on_transport)(failure));
                }
                None => {
                    self.finished = true;
                    self.queue.extend(self.source.finish()?);
                }
            }
        }
    }

    /// Puts a chunk back in front of the queue.
    pub fn push_front(&mut self, chunk: Chunk) {
        self.queue.push_front(chunk);
    }

    /// Whether a chunk is ready without reading further.
    #[must_use]
    pub fn has_ready(&self) -> bool {
        !self.queue.is_empty()
    }

    /// Makes the pump a stream, calling `settle` once with whether it ended
    /// cleanly: at its end, at its first failure, or when dropped half read.
    pub fn into_stream<'a>(self, settle: Settle) -> ChunkStream<'a>
    where
        S: 'a,
    {
        Box::pin(futures_util::stream::unfold(
            (self, Some(settle)),
            |(mut pump, mut settle)| async move {
                settle.as_ref()?;
                match pump.next().await {
                    Ok(Some(chunk)) => Some((Ok(chunk), (pump, settle))),
                    Ok(None) => {
                        if let Some(settle) = settle.take() {
                            settle.done(true);
                        }
                        None
                    }
                    Err(failure) => {
                        if let Some(settle) = settle.take() {
                            settle.done(false);
                        }
                        Some((Err(failure), (pump, None)))
                    }
                }
            },
        ))
    }
}

/// Called once when a paced call ends; a call dropped before its end settles
/// as a failure.
pub struct Settle {
    action: Option<Box<dyn FnOnce(bool) + Send>>,
}

impl Settle {
    pub fn new(action: impl FnOnce(bool) + Send + 'static) -> Self {
        Self {
            action: Some(Box::new(action)),
        }
    }

    /// A settle that does nothing.
    #[must_use]
    pub fn none() -> Self {
        Self { action: None }
    }

    pub fn done(mut self, succeeded: bool) {
        if let Some(action) = self.action.take() {
            action(succeeded);
        }
    }
}

impl Drop for Settle {
    fn drop(&mut self) {
        if let Some(action) = self.action.take() {
            action(false);
        }
    }
}
