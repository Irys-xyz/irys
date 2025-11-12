use std::{future::Future, pin::pin};
use tracing::{info, trace};

/// Runs the future to completion or until:
/// - `ctrl-c` is received.
/// - `SIGTERM` is received (unix only).
/// - A message is received on the given channel.
///
/// Returns the ShutdownReason that caused termination.
pub async fn run_until_ctrl_c_or_channel_message<F>(
    fut: F,
    mut channel: tokio::sync::mpsc::Receiver<irys_types::ShutdownReason>,
    service_name: &str,
) -> eyre::Result<irys_types::ShutdownReason>
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
                trace!("Received ctrl-c");
                Ok(irys_types::ShutdownReason::Signal("SIGINT".to_string()))
            },
            _ = sigterm => {
                trace!("Received SIGTERM");
                Ok(irys_types::ShutdownReason::Signal("SIGTERM".to_string()))
            },
            reason = termination_message => {
                if let Some(reason) = reason {
                    info!("Received termination message: {}", reason);
                    Ok(reason)
                } else {
                    trace!("Received termination message (channel closed)");
                    Ok(irys_types::ShutdownReason::Signal("channel closed".to_string()))
                }
            },
            res = fut => {
                match res {
                    Ok(()) => Ok(irys_types::ShutdownReason::ServiceCompleted(service_name.to_string())),
                    Err(e) => Ok(irys_types::ShutdownReason::FatalError(e.to_string())),
                }
            },
        }
    }

    #[cfg(not(unix))]
    {
        let ctrl_c = pin!(ctrl_c);
        let fut = pin!(fut);

        tokio::select! {
            _ = ctrl_c => {
                trace!("Received ctrl-c");
                return Ok(irys_types::ShutdownReason::Signal("SIGINT".to_string()))

            },
            reason = channel.recv() => {
                if let Some(reason) = reason {
                    info!("Received shutdown message: {}", reason);
                    return Ok(reason)
                } else {
                    trace!("Received shutdown message (channel closed)");
                    return Ok(irys_types::ShutdownReason::Signal("channel closed".to_string()))
                }
            },
            res = fut =>  {
                return match res {
                    Ok(()) => Ok(irys_types::ShutdownReason::ServiceCompleted(service_name.to_string())),
                    Err(e) => Ok(irys_types::ShutdownReason::FatalError(e.to_string())),
                }
            },
        }
    }
}
