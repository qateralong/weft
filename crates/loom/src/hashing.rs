//! Password hashing costs memory and CPU, so only a few hashes run at once whoever asks.

use tokio::sync::Semaphore;

static RUNNING: Semaphore = Semaphore::const_new(4);

/// Runs `work` off the async threads once a slot is free.
pub async fn run<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let _permit = RUNNING.acquire().await.ok()?;
    tokio::task::spawn_blocking(work).await.ok()
}
