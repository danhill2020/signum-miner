//! Prio retry consumes a stream of submissions and decides what to send when.
//!
//! * A new item (newer block, or a better deadline for the current block) is
//!   yielded immediately.
//! * An item equal to the last one yielded is a retry (the caller re-enqueues
//!   a submission after a transient failure).  It is yielded again after an
//!   exponential backoff, and given up after [`MAX_RETRIES`].
//! * An item older than the last one yielded is stale and dropped.
//!
//! The ordering is `S::Item: Ord`, see `SubmissionParameters`.

use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures_core::Future;
use futures_core::Stream;
use pin_project::pin_project;
use tokio::time::{self, Duration, Instant, Sleep};

/// Longest pause between two retries of the same submission.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
/// Retries of one submission before it is dropped.
const MAX_RETRIES: u32 = 12;

#[pin_project]
pub struct PrioRetry<S>
where
    S: Stream,
    S::Item: Ord + Clone + Eq,
{
    #[pin]
    stream: S,
    #[pin]
    delay: Sleep,
    base_delay: Duration,
    /// A retry waiting for `delay` to elapse.
    pending: Option<S::Item>,
    /// The most recent item handed out, used to classify new arrivals.
    last_yielded: Option<S::Item>,
    retries: u32,
    stream_done: bool,
}

impl<S> PrioRetry<S>
where
    S: Stream,
    S::Item: Ord + Clone + Eq,
{
    pub fn new(stream: S, base_delay: Duration) -> Self {
        Self {
            stream,
            delay: time::sleep(Duration::ZERO),
            base_delay,
            pending: None,
            last_yielded: None,
            retries: 0,
            stream_done: false,
        }
    }
}

impl<S> Stream for PrioRetry<S>
where
    S: Stream + Unpin,
    S::Item: Ord + Clone + Eq + Unpin,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();

        while !*this.stream_done {
            match this.stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    match this.last_yielded.as_ref() {
                        Some(last) if item == *last => {
                            // A retry of what we sent last.
                            *this.retries += 1;
                            if *this.retries > MAX_RETRIES {
                                warn!("giving up on a submission after {} retries", MAX_RETRIES);
                                *this.pending = None;
                                continue;
                            }
                            let factor = 1u32 << (*this.retries - 1).min(16);
                            let wait = this
                                .base_delay
                                .checked_mul(factor)
                                .unwrap_or(MAX_RETRY_DELAY)
                                .min(MAX_RETRY_DELAY);
                            this.delay.as_mut().reset(Instant::now() + wait);
                            *this.pending = Some(item);
                            continue;
                        }
                        Some(last) if item < *last => {
                            // Older block or a worse deadline than one already sent.
                            debug!("dropping stale submission");
                            continue;
                        }
                        _ => {}
                    }
                    // New and better: send right away and forget any pending retry.
                    *this.pending = None;
                    *this.retries = 0;
                    *this.last_yielded = Some(item.clone());
                    return Poll::Ready(Some(item));
                }
                Poll::Ready(None) => {
                    *this.stream_done = true;
                }
                Poll::Pending => break,
            }
        }

        if this.pending.is_some() && this.delay.as_mut().poll(cx).is_ready() {
            return Poll::Ready(this.pending.take());
        }

        if *this.stream_done && this.pending.is_none() {
            return Poll::Ready(None);
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tokio::sync::mpsc;
    use tokio_stream::wrappers::UnboundedReceiverStream;

    /// (block, score): higher block is newer, higher score is better.
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct Item(u64, u64);

    type TestStream = Pin<Box<PrioRetry<UnboundedReceiverStream<Item>>>>;

    fn setup() -> (mpsc::UnboundedSender<Item>, TestStream) {
        let (tx, rx) = mpsc::unbounded_channel();
        let stream = PrioRetry::new(UnboundedReceiverStream::new(rx), Duration::from_millis(50));
        (tx, Box::pin(stream))
    }

    async fn next_within(stream: &mut TestStream, ms: u64) -> Option<Item> {
        time::timeout(Duration::from_millis(ms), stream.as_mut().next())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn new_items_are_yielded_immediately() {
        let (tx, mut stream) = setup();
        let started = Instant::now();
        tx.send(Item(1, 10)).unwrap();
        assert_eq!(next_within(&mut stream, 20).await, Some(Item(1, 10)));
        assert!(started.elapsed() < Duration::from_millis(20));

        // Better deadline for the same block: also immediate.
        tx.send(Item(1, 20)).unwrap();
        assert_eq!(next_within(&mut stream, 20).await, Some(Item(1, 20)));
    }

    #[tokio::test]
    async fn stale_items_are_dropped() {
        let (tx, mut stream) = setup();
        tx.send(Item(5, 10)).unwrap();
        assert_eq!(next_within(&mut stream, 20).await, Some(Item(5, 10)));
        tx.send(Item(4, 99)).unwrap(); // older block
        tx.send(Item(5, 3)).unwrap(); // worse deadline
        assert_eq!(next_within(&mut stream, 100).await, None);
    }

    #[tokio::test]
    async fn retries_back_off_and_newer_items_cancel_them() {
        let (tx, mut stream) = setup();
        tx.send(Item(1, 10)).unwrap();
        assert_eq!(next_within(&mut stream, 20).await, Some(Item(1, 10)));

        // First retry waits ~50ms, second ~100ms.
        tx.send(Item(1, 10)).unwrap();
        let t = Instant::now();
        assert_eq!(next_within(&mut stream, 500).await, Some(Item(1, 10)));
        assert!(
            t.elapsed() >= Duration::from_millis(45),
            "{:?}",
            t.elapsed()
        );
        tx.send(Item(1, 10)).unwrap();
        let t = Instant::now();
        assert_eq!(next_within(&mut stream, 500).await, Some(Item(1, 10)));
        assert!(
            t.elapsed() >= Duration::from_millis(90),
            "{:?}",
            t.elapsed()
        );

        // A pending retry is forgotten when a newer item shows up.
        tx.send(Item(1, 10)).unwrap();
        tx.send(Item(2, 1)).unwrap();
        assert_eq!(next_within(&mut stream, 20).await, Some(Item(2, 1)));
        assert_eq!(next_within(&mut stream, 400).await, None);
    }

    #[tokio::test]
    async fn ends_when_input_ends_after_pending_retry() {
        let (tx, mut stream) = setup();
        tx.send(Item(1, 1)).unwrap();
        assert_eq!(next_within(&mut stream, 20).await, Some(Item(1, 1)));
        tx.send(Item(1, 1)).unwrap();
        drop(tx);
        assert_eq!(next_within(&mut stream, 500).await, Some(Item(1, 1)));
        assert_eq!(stream.as_mut().next().await, None);
    }
}
