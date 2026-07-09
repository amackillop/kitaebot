//! Generic async retry combinator.
//!
//! The retry decision is a pure policy function; this module only owns
//! the loop and the sleep. Cancellation needs no special handling —
//! dropping the returned future drops the sleep with it.

use std::fmt::Display;
use std::future::Future;
use std::time::Duration;

use tracing::warn;

/// Run `op`, retrying while `policy` grants a delay.
///
/// `policy` receives the error and the 0-based retry attempt: `Some(d)`
/// means sleep `d` and retry, `None` means return the error.
pub async fn retry<T, E, Fut>(
    mut op: impl FnMut() -> Fut,
    policy: impl Fn(&E, u32) -> Option<Duration>,
) -> Result<T, E>
where
    Fut: Future<Output = Result<T, E>>,
    E: Display,
{
    let mut attempt = 0;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(e) => match policy(&e, attempt) {
                Some(delay) => {
                    warn!(
                        attempt = attempt + 1,
                        delay_secs = delay.as_secs(),
                        "Transient error, retrying: {e}"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                None => return Err(e),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn flaky(
        failures: usize,
        calls: &AtomicUsize,
    ) -> impl FnMut() -> std::future::Ready<Result<u32, String>> + '_ {
        move || {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(if n < failures {
                Err("boom".to_string())
            } else {
                Ok(7)
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn returns_first_success() {
        let calls = AtomicUsize::new(0);
        let result = retry(flaky(0, &calls), |_, _| Some(Duration::from_secs(1))).await;
        assert_eq!(result, Ok(7));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_until_policy_gives_up() {
        let calls = AtomicUsize::new(0);
        let result = retry(flaky(usize::MAX, &calls), |_, attempt| {
            (attempt < 2).then(|| Duration::from_secs(1))
        })
        .await;
        assert_eq!(result, Err("boom".to_string()));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_through_failures_to_success() {
        let calls = AtomicUsize::new(0);
        let result = retry(flaky(2, &calls), |_, _| Some(Duration::from_secs(1))).await;
        assert_eq!(result, Ok(7));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn sleeps_the_granted_delay() {
        let start = tokio::time::Instant::now();
        let calls = AtomicUsize::new(0);
        let _ = retry(flaky(1, &calls), |_, _| Some(Duration::from_secs(3))).await;
        assert_eq!(start.elapsed(), Duration::from_secs(3));
    }
}
