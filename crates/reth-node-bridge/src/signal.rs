use std::{future::Future, pin::pin};

use tracing::trace;

/// Runs the future to completion or until:
/// - `ctrl-c` is received.
/// - `SIGTERM` is received (unix only).
/// - A message is received on the given channel.
pub async fn run_until_ctrl_c_or_channel_message<F, E>(
    fut: F,
    mut channel: tokio::sync::mpsc::Receiver<()>,
) -> Result<(), E>
where
    F: Future<Output = Result<(), E>>,
    E: Send + Sync + 'static + From<std::io::Error>,
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
                return Ok(())
            },
            _ = sigterm => {
                trace!(target: "reth::cli", "Received SIGTERM");
                return Ok(())
            },
            _ = termination_message => {
                trace!(target: "reth::cli", "Received termination message");
                return Ok(())
            },
            res = fut => return Ok(res?),
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
            res = fut =>  return Ok(res?),
        }
    }

    // Ok(())
}
