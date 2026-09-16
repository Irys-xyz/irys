use std::sync::Arc;
use std::time::Duration;

use irys_types::{SendTraced as _, Traced, chunk::UnpackedChunk};
use tokio::sync::{Semaphore, mpsc::UnboundedSender};

use super::{ChunkIngressMessage, HttpAdmissionGuard};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HttpChunkEnqueueError {
    #[error("http chunk admission waiters saturated")]
    WaitersSaturated,
    #[error("timed out waiting for http chunk admission")]
    AdmissionTimeout,
    #[error("chunk ingress channel closed")]
    ChannelClosed,
}

pub async fn enqueue_http_chunk(
    chunk: UnpackedChunk,
    sender: &UnboundedSender<Traced<ChunkIngressMessage>>,
    admission: &Arc<Semaphore>,
    waiters: &Arc<Semaphore>,
    timeout: Duration,
) -> Result<(), HttpChunkEnqueueError> {
    let permit = match Arc::clone(admission).try_acquire_owned() {
        Ok(permit) => permit,
        Err(tokio::sync::TryAcquireError::Closed) => {
            return Err(HttpChunkEnqueueError::ChannelClosed);
        }
        Err(tokio::sync::TryAcquireError::NoPermits) => {
            let waiter = match Arc::clone(waiters).try_acquire_owned() {
                Ok(waiter) => waiter,
                Err(tokio::sync::TryAcquireError::Closed) => {
                    return Err(HttpChunkEnqueueError::ChannelClosed);
                }
                Err(tokio::sync::TryAcquireError::NoPermits) => {
                    return Err(HttpChunkEnqueueError::WaitersSaturated);
                }
            };
            let acquired = tokio::time::timeout(timeout, Arc::clone(admission).acquire_owned())
                .await
                .map_err(|_| HttpChunkEnqueueError::AdmissionTimeout)?
                .map_err(|_| HttpChunkEnqueueError::ChannelClosed)?;
            drop(waiter);
            acquired
        }
    };
    let guard = HttpAdmissionGuard::new(permit);
    sender
        .send_traced(ChunkIngressMessage::IngestChunk(chunk, None, Some(guard)))
        .map_err(|_| HttpChunkEnqueueError::ChannelClosed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::DataRoot;

    fn dummy_chunk() -> UnpackedChunk {
        UnpackedChunk {
            data_root: DataRoot::from([0_u8; 32]),
            data_size: 0,
            tx_offset: 0_u32.into(),
            data_path: Default::default(),
            bytes: Default::default(),
        }
    }

    #[tokio::test]
    async fn enqueue_returns_without_receiver_consuming() {
        let admission = Arc::new(Semaphore::new(1));
        let waiters = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        enqueue_http_chunk(
            dummy_chunk(),
            &tx,
            &admission,
            &waiters,
            Duration::from_secs(1),
        )
        .await
        .expect("enqueue");
        assert_eq!(admission.available_permits(), 0);
        let traced = rx.try_recv().expect("queued");
        match traced.inner {
            ChunkIngressMessage::IngestChunk(_, None, Some(_)) => {}
            other => panic!("expected fire-and-forget with guard, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn enqueue_times_out_when_admission_held() {
        let admission = Arc::new(Semaphore::new(1));
        let _hold = admission.clone().try_acquire_owned().unwrap();
        let waiters = Arc::new(Semaphore::new(1));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let started = tokio::time::Instant::now();
        let err = enqueue_http_chunk(
            dummy_chunk(),
            &tx,
            &admission,
            &waiters,
            Duration::from_millis(50),
        )
        .await
        .expect_err("timeout");
        assert!(matches!(err, HttpChunkEnqueueError::AdmissionTimeout));
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert_eq!(waiters.available_permits(), 1);
    }

    #[tokio::test]
    async fn enqueue_rejects_immediately_when_waiters_held() {
        let admission = Arc::new(Semaphore::new(0));
        let waiters = Arc::new(Semaphore::new(1));
        let _hold = waiters.clone().try_acquire_owned().unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let started = tokio::time::Instant::now();
        let err = enqueue_http_chunk(
            dummy_chunk(),
            &tx,
            &admission,
            &waiters,
            Duration::from_secs(5),
        )
        .await
        .expect_err("saturated");
        assert!(matches!(err, HttpChunkEnqueueError::WaitersSaturated));
        assert!(started.elapsed() < Duration::from_millis(200));
    }
}
