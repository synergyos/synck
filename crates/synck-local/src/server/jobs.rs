//! Blocking work runs off the async workers, bounded. A started job owns its
//! permit and shutdown lease, so a cancelled request cannot end it early or let
//! another job past the bound before it finishes.

use std::io::{BufWriter, Write};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::task::{TaskTracker, task_tracker::TaskTrackerToken};

use super::{Error, Result};

const WAIT: Duration = Duration::from_secs(5);
/// Bytes per streamed body chunk.
const CHUNK: usize = 64 * 1024;

#[derive(Clone)]
pub(crate) struct Jobs {
    permits: Arc<Semaphore>,
    tracker: TaskTracker,
}

impl Jobs {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(limit)),
            tracker: TaskTracker::new(),
        }
    }

    async fn admit(&self) -> Result<(TaskTrackerToken, OwnedSemaphorePermit)> {
        // The lease comes first: once `close` returns, `wait` sees every job
        // that got a permit.
        let lease = self.tracker.token();
        let permit = tokio::time::timeout(WAIT, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| Error::Overloaded)?
            .map_err(|_| Error::Overloaded)?;
        Ok((lease, permit))
    }

    pub(crate) async fn run<T, F>(&self, job: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let (lease, permit) = self.admit().await?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _lease = lease;
            job()
        })
        .await
        .map_err(|error| Error::State(format!("blocking job failed: {error}")))?
    }

    /// Run `job` writing into the returned body. The job keeps its slot until
    /// the body is read or dropped. An error after the first bytes cuts the
    /// body short, which the reader's digest check catches.
    pub(crate) async fn stream<F>(&self, job: F) -> Result<Body>
    where
        F: FnOnce(&mut dyn Write) -> Result<()> + Send + 'static,
    {
        let (lease, permit) = self.admit().await?;
        let (sender, mut receiver) = mpsc::channel(4);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _lease = lease;
            let mut body = BufWriter::with_capacity(CHUNK, Chunks(sender.clone()));
            let result = job(&mut body).and_then(|()| {
                body.flush()
                    .map_err(|error| Error::State(format!("stream body: {error}")))
            });
            if let Err(error) = result
                && !sender.is_closed()
            {
                eprintln!("synck serve: {error}");
                let _ = sender.blocking_send(Err(std::io::Error::other("body cut short")));
            }
        });
        Ok(Body::from_stream(futures_util::stream::poll_fn(
            move |context| receiver.poll_recv(context),
        )))
    }

    /// Admit no new jobs; `wait` then returns once started ones finish.
    pub(crate) fn close(&self) {
        self.permits.close();
        self.tracker.close();
    }

    pub(crate) async fn wait(&self) {
        self.tracker.wait().await;
    }
}

struct Chunks(mpsc::Sender<std::io::Result<Bytes>>);

impl Write for Chunks {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .blocking_send(Ok(Bytes::copy_from_slice(bytes)))
            .map_err(|_| std::io::ErrorKind::BrokenPipe)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[tokio::test]
    async fn cancelled_job_keeps_its_permit_until_it_finishes() {
        let jobs = Jobs::new(1);
        let started = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let first = {
            let (jobs, started, finished) = (jobs.clone(), started.clone(), finished.clone());
            tokio::spawn(async move {
                jobs.run(move || {
                    started.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(150));
                    finished.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
            })
        };
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        first.abort();
        let seen = finished.clone();
        assert!(
            jobs.run(move || Ok(seen.load(Ordering::SeqCst)))
                .await
                .unwrap()
        );

        jobs.close();
        assert!(matches!(jobs.run(|| Ok(())).await, Err(Error::Overloaded)));
        jobs.wait().await;
    }
}
