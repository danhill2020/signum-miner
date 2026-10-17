//! This is almost the exact code of the tokio Interval implementation with a simple difference:
//!
//! In scavenger we request the `MiningInfo` in a certain interval. If the pool or wallet is not
//! available in a certain time frame the request will take longer than `Interval`. As soon as the
//! pool or wallet is available a bunch of requests are going to be fired at once.
//!
//! Here we delay once an item has been processed, e.g.:
//! 1. We have a interval of 3s.
//! 2. We fire our request at time = 0s.
//! 3. We timeout after time = 10s
//! 4. We fire our next request at time t = 13s

use core::future::Future;
use futures_core::stream::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use tokio::time::{sleep_until, Sleep};

/// A stream representing notifications at a fixed interval *after* processing completes.
/// Unlike Tokio's default interval, this implementation starts the countdown after each item completes.
#[derive(Debug)]
pub struct Interval {
    sleep: Pin<Box<Sleep>>,
    duration: Duration,
}

impl Interval {
    /// Create a new `Interval` that starts at `at` and yields every `duration` interval after processing.
    pub fn new(at: Instant, duration: Duration) -> Self {
        assert!(duration > Duration::ZERO, "`duration` must be non-zero.");

        let sleep = Box::pin(sleep_until(at.into()));
        Self { sleep, duration }
    }

    /// Shortcut for starting an interval from now + duration.
    pub fn new_interval(duration: Duration) -> Self {
        Self::new(Instant::now() + duration, duration)
    }
}

impl Stream for Interval {
    type Item = Instant;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.sleep.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }

        let now = Instant::now();
        let duration = self.duration;
        self.sleep.as_mut().reset((now + duration).into());

        Poll::Ready(Some(now))
    }
}
