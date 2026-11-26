//! Calling into drivers safely: a panic becomes an error for that request, and every network
//! operation has a time limit.

use crate::error::{Error, Result};
use futures::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

/// Runs `fut`, turning a panic inside it into an internal error. `what` names the operation for
/// the message ("Running the statement", "Loading the object tree").
pub async fn guarded<F, T>(what: &str, fut: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(result) => result,
        Err(payload) => Err(panic_error(what, payload.as_ref())),
    }
}

/// Runs `fut` with a time limit.
pub async fn with_timeout<F, T>(limit: Duration, fut: F) -> Result<T>
where
    F: Future<Output = Result<T>>,
{
    match tokio::time::timeout(limit, fut).await {
        Ok(result) => result,
        Err(_) => Err(Error::timeout(limit)),
    }
}

/// The error a caught panic turns into. The panic text goes to the log (which redacts it), not
/// to the user, because it may quote data.
pub fn panic_error(what: &str, payload: &(dyn std::any::Any + Send)) -> Error {
    let text = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-text panic payload".to_owned());
    tracing::error!(operation = what, panic = %text, "driver panicked");
    Error::internal(format!(
        "{what} failed because of a bug in Treasury. The connection stays open; try again, and \
         report the problem if it happens again."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;

    #[tokio::test]
    async fn panics_become_errors() {
        let err = guarded("Loading the tree", async {
            let v: Vec<u8> = Vec::new();
            #[allow(clippy::indexing_slicing)]
            let _ = v[3];
            Ok(())
        })
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Internal);
        assert!(err.message.starts_with("Loading the tree failed"));
    }

    #[tokio::test(start_paused = true)]
    async fn slow_operations_time_out() {
        let err = with_timeout(Duration::from_secs(2), async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            Ok(())
        })
        .await
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Timeout { after_ms: 2_000 });
    }
}
