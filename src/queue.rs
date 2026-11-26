//! Admission of `agent:ask` work: a few questions run at once, a bounded
//! number wait, and the rest are told the agent is busy. Each question costs
//! a search, a prompt and a model call, so a burst must not start them all.

use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Questions answered at the same time.
pub const MAX_ACTIVE: usize = 4;
/// Questions that may wait for their turn.
pub const MAX_WAITING: usize = 28;

/// Shared admission control; cheap to create once and use from the event loop.
#[derive(Debug, Clone)]
pub struct AskQueue {
    /// One permit per question admitted, running or waiting.
    admitted: Arc<Semaphore>,
    /// One permit per question running.
    active: Arc<Semaphore>,
    max_active: usize,
}

/// An admitted question, which may have to wait before it runs.
#[derive(Debug)]
pub struct Ticket {
    admitted: OwnedSemaphorePermit,
    queue: AskQueue,
}

impl Default for AskQueue {
    fn default() -> Self {
        Self::new(MAX_ACTIVE, MAX_WAITING)
    }
}

impl AskQueue {
    /// A queue running `active` questions at once with `waiting` more behind them.
    pub fn new(active: usize, waiting: usize) -> Self {
        Self {
            admitted: Arc::new(Semaphore::new(active + waiting)),
            active: Arc::new(Semaphore::new(active)),
            max_active: active,
        }
    }

    /// Takes a place, or `None` when the queue is full. Never waits.
    pub fn admit(&self) -> Option<Ticket> {
        let admitted = Arc::clone(&self.admitted).try_acquire_owned().ok()?;
        Some(Ticket { admitted, queue: self.clone() })
    }

    /// Questions running now.
    pub fn running(&self) -> usize {
        self.max_active - self.active.available_permits()
    }
}

impl Ticket {
    /// Waits for a turn, then runs `work`. Both places are freed when the
    /// work ends, fails, panics or is cancelled.
    pub async fn run<F: Future>(self, work: F) -> F::Output {
        let queued = Instant::now();
        // The semaphore is never closed, so this only fails if that changes;
        // running unbounded would then be wrong, but so would dropping work.
        let _turn = self.queue.active.acquire().await.expect("the ask queue is never closed");
        let started = Instant::now();
        tracing::debug!(
            waited_ms = started.duration_since(queued).as_millis(),
            running = self.queue.running(),
            "ask started"
        );
        let output = work.await;
        tracing::debug!(duration_ms = started.elapsed().as_millis(), "ask finished");
        drop(self.admitted);
        output
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use tokio::sync::Notify;
    use tokio::task::JoinSet;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn no_more_than_the_limit_run_at_once() {
        let queue = AskQueue::new(3, 40);
        let (running, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let mut tasks = JoinSet::new();
        for _ in 0..40 {
            let ticket = queue.admit().expect("room in the queue");
            let (running, peak) = (Arc::clone(&running), Arc::clone(&peak));
            tasks.spawn(ticket.run(async move {
                peak.fetch_max(running.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                running.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        while let Some(done) = tasks.join_next().await {
            done.unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(queue.running(), 0);
    }

    #[tokio::test]
    async fn a_full_queue_refuses_until_a_place_is_free() {
        let queue = AskQueue::new(1, 1);
        let release = Arc::new(Notify::new());
        let held = Arc::clone(&release);
        let first = tokio::spawn(queue.admit().unwrap().run(async move { held.notified().await }));
        let second = queue.admit().expect("one may wait");
        assert!(queue.admit().is_none(), "the queue is full");
        release.notify_one();
        first.await.unwrap();
        second.run(async {}).await;
        assert!(queue.admit().is_some());
    }

    #[tokio::test]
    async fn failure_panic_and_cancellation_free_their_places() {
        let queue = AskQueue::new(1, 0);
        let failed: Result<(), &str> = queue.admit().unwrap().run(async { Err("provider down") }).await;
        assert!(failed.is_err());

        let panicked = tokio::spawn(queue.admit().unwrap().run(async { panic!("boom") }));
        assert!(panicked.await.unwrap_err().is_panic());

        let running = tokio::spawn(queue.admit().unwrap().run(std::future::pending::<()>()));
        tokio::task::yield_now().await;
        assert_eq!(queue.running(), 1);
        running.abort();
        assert!(running.await.unwrap_err().is_cancelled());

        // A question cancelled while waiting frees its place too.
        let queue = AskQueue::new(1, 1);
        let blocker = tokio::spawn(queue.admit().unwrap().run(std::future::pending::<()>()));
        let waiting = tokio::spawn(queue.admit().unwrap().run(async {}));
        tokio::task::yield_now().await;
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert!(queue.admit().is_some());
        blocker.abort();
    }
}
