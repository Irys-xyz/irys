use std::{future::Future, pin::pin};

use reth::tasks::TaskManager;
use tracing::trace;

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

/// Runs the future to completion or until:
/// - `ctrl-c` is received.
/// - `SIGTERM` is received (unix only).
/// - A message is received on the given channel.
pub async fn run_until_ctrl_c_or_channel_message<F>(
    fut: F,
    mut channel: tokio::sync::mpsc::Receiver<()>,
) -> eyre::Result<()>
where
    F: Future<Output = eyre::Result<()>>,
{
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        let mut stream = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let sigterm = stream.recv();
        let termination_message = channel.recv();
        let sigterm = pin!(sigterm);
        let ctrl_c = pin!(ctrl_c);
        let fut = pin!(fut);

        tokio::select! {
            _ = ctrl_c => {
                trace!(target: "reth::cli", "Received ctrl-c");
                Ok(())
            },
            _ = sigterm => {
                trace!(target: "reth::cli", "Received SIGTERM");
                Ok(())
            },
            _ = termination_message => {
                trace!(target: "reth::cli", "Received termination message");
                Ok(())
            },
            res = fut => res,
        }
    }

    #[cfg(not(unix))]
    {
        let ctrl_c = pin!(ctrl_c);
        let fut = pin!(fut);

        tokio::select! {
            _ = ctrl_c => {
                trace!(target: "reth::cli", "Received ctrl-c");
                return Ok(())

            },
            _ = channel.recv() => {
                trace!(target: "reth::cli", "Received channel message");
                return Ok(())
            },
            res = fut =>  return res,
        }
    }
}
