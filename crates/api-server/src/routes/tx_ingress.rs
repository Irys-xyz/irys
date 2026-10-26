use crate::error::ApiError;
use awc::http::StatusCode;
use irys_actors::mempool_service::{MempoolServiceMessage, TxIngressError};
use irys_types::{SendTraced as _, Traced};
use std::time::Duration;
use tokio::sync::{Semaphore, mpsc::UnboundedSender, oneshot};

/// Bounds concurrent in-flight ingress requests, not total queued memory: a reply timeout frees
/// the permit while the message stays queued. Far above honest concurrent submitters.
pub const MAX_IN_FLIGHT_TX_INGRESS: usize = 256;

/// Several times the normal ingress latency (validation plus balance lookup), yet short enough
/// to free the client and permit before typical HTTP client timeouts.
pub const MEMPOOL_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// Sends an API ingress message to the mempool and awaits its reply.
/// Returns 503 without queueing when no admission permit is free, or when the mempool
/// does not reply within `reply_timeout`. The permit is held until the reply or timeout.
pub(crate) async fn ingest_via_mempool(
    admission: &Semaphore,
    mempool: &UnboundedSender<Traced<MempoolServiceMessage>>,
    reply_timeout: Duration,
    build_message: impl FnOnce(oneshot::Sender<Result<(), TxIngressError>>) -> MempoolServiceMessage,
) -> Result<Result<(), TxIngressError>, ApiError> {
    let _permit = admission.try_acquire().map_err(|_| {
        tracing::warn!("API: tx ingress admission limit reached");
        ApiError::from((
            "Too many transactions in flight. Please try again later.",
            StatusCode::SERVICE_UNAVAILABLE,
        ))
    })?;

    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(err) = mempool.send_traced(build_message(reply_tx)) {
        tracing::error!("API: {}", err);
        return Err(ApiError::from((
            "Failed to deliver transaction",
            StatusCode::INTERNAL_SERVER_ERROR,
        )));
    }

    match tokio::time::timeout(reply_timeout, reply_rx).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(err)) => {
            tracing::error!("API: {}", err);
            Err(ApiError::from((
                "Failed to deliver transaction",
                StatusCode::INTERNAL_SERVER_ERROR,
            )))
        }
        Err(_) => {
            tracing::warn!("API: mempool did not reply to tx ingress in time");
            Err(ApiError::from((
                "Mempool did not respond in time. Please try again later.",
                StatusCode::SERVICE_UNAVAILABLE,
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::ResponseError as _;
    use irys_types::DataTransactionHeader;
    use tokio::sync::mpsc::unbounded_channel;

    fn ingest_message(reply: oneshot::Sender<Result<(), TxIngressError>>) -> MempoolServiceMessage {
        MempoolServiceMessage::IngestDataTxFromApi(DataTransactionHeader::default(), reply)
    }

    #[tokio::test]
    async fn returns_503_when_mempool_never_replies() {
        let admission = Semaphore::new(1);
        let (mempool, mut inbox) = unbounded_channel();

        let err = ingest_via_mempool(
            &admission,
            &mempool,
            Duration::from_millis(50),
            ingest_message,
        )
        .await
        .expect_err("no reply must time out");

        assert_eq!(err.status_code(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(inbox.try_recv().is_ok(), "message must have been sent");
        assert_eq!(
            admission.available_permits(),
            1,
            "permit released on timeout"
        );
    }

    #[tokio::test]
    async fn rejects_immediately_when_no_permit_then_accepts_after_release() {
        let admission = Semaphore::new(1);
        let (mempool, mut inbox) = unbounded_channel();
        let held = admission.try_acquire().expect("permit");

        let err = ingest_via_mempool(
            &admission,
            &mempool,
            Duration::from_secs(60),
            ingest_message,
        )
        .await
        .expect_err("saturated admission must reject");
        assert_eq!(err.status_code(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            inbox.try_recv().is_err(),
            "rejected request must not be queued"
        );

        drop(held);
        let replier = tokio::spawn(async move {
            let Traced { inner, .. } = inbox.recv().await.expect("message");
            let MempoolServiceMessage::IngestDataTxFromApi(_, reply) = inner else {
                panic!("unexpected message");
            };
            reply.send(Ok(())).expect("reply");
        });
        let result = ingest_via_mempool(
            &admission,
            &mempool,
            Duration::from_secs(60),
            ingest_message,
        )
        .await
        .expect("accepted after release");
        assert!(result.is_ok());
        replier.await.expect("replier");
        assert_eq!(admission.available_permits(), 1);
    }
}
