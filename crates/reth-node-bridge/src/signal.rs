use reth::tasks::TaskManager;
use std::{future::Future, pin::pin};

/// Runs the given future to completion or until a critical task panicked.
///
/// Returns the error if a task panicked, or the given future returned an error.
pub async fn run_to_completion_or_panic<F>(tasks: &mut TaskManager, fut: F) -> eyre::Result<()>
where
    F: Future<Output = eyre::Result<()>>,
{
    let fut = pin!(fut);
    tokio::select! {
        err = tasks => {
            Ok(err?)
        },
        res = fut => {
            Ok(res?)
        }
    }
}
