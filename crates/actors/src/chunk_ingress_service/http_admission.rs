use std::sync::{Arc, Mutex};
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
    #[error("chunk cache at its size watermark")]
    CacheFull,
}

/// `cache_watermark_reached` is `ChunkIngressState::cache_watermark_reached`,
/// read by the caller before admission.
pub async fn enqueue_http_chunk(
    chunk: UnpackedChunk,
    sender: &UnboundedSender<Traced<ChunkIngressMessage>>,
    admission: &Arc<Semaphore>,
    waiters: &Arc<Semaphore>,
    timeout: Duration,
    accepting: &Arc<Mutex<bool>>,
    cache_watermark_reached: bool,
) -> Result<(), HttpChunkEnqueueError> {
    {
        let open = accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*open {
            return Err(HttpChunkEnqueueError::ChannelClosed);
        }
    }
    if cache_watermark_reached {
        return Err(HttpChunkEnqueueError::CacheFull);
    }
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
    {
        let open = accepting
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*open {
            return Err(HttpChunkEnqueueError::ChannelClosed);
        }
        sender
            .send_traced(ChunkIngressMessage::IngestChunk(chunk, None, Some(guard)))
            .map_err(|_| HttpChunkEnqueueError::ChannelClosed)?;
    }
    Ok(())
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

    fn accepting() -> Arc<Mutex<bool>> {
        Arc::new(Mutex::new(true))
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
            &accepting(),
            false,
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
            &accepting(),
            false,
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
            &accepting(),
            false,
        )
        .await
        .expect_err("saturated");
        assert!(matches!(err, HttpChunkEnqueueError::WaitersSaturated));
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[tokio::test]
    async fn enqueue_rejects_when_ingress_closed() {
        let admission = Arc::new(Semaphore::new(1));
        let waiters = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let accepting = Arc::new(Mutex::new(false));
        let err = enqueue_http_chunk(
            dummy_chunk(),
            &tx,
            &admission,
            &waiters,
            Duration::from_secs(1),
            &accepting,
            false,
        )
        .await
        .expect_err("closed");
        assert!(matches!(err, HttpChunkEnqueueError::ChannelClosed));
        assert!(rx.try_recv().is_err());
        assert_eq!(admission.available_permits(), 1);
    }

    #[tokio::test]
    async fn enqueue_rejects_at_cache_watermark_without_taking_a_permit() {
        let admission = Arc::new(Semaphore::new(1));
        let waiters = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let err = enqueue_http_chunk(
            dummy_chunk(),
            &tx,
            &admission,
            &waiters,
            Duration::from_secs(1),
            &accepting(),
            true,
        )
        .await
        .expect_err("cache full");
        assert_eq!(err, HttpChunkEnqueueError::CacheFull);
        assert!(rx.try_recv().is_err());
        assert_eq!(admission.available_permits(), 1);
        assert_eq!(waiters.available_permits(), 1);
    }

    /// Bodies admitted or waiting when the cache reaches its mark still
    /// land; nothing new is admitted after it. The HTTP overshoot is
    /// therefore at most the admission permits plus the waiter slots.
    #[tokio::test]
    async fn cache_watermark_bounds_http_overshoot_to_admission_and_waiters() {
        let admission = Arc::new(Semaphore::new(2));
        let waiters = Arc::new(Semaphore::new(1));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let open = accepting();

        for _ in 0..2 {
            enqueue_http_chunk(
                dummy_chunk(),
                &tx,
                &admission,
                &waiters,
                Duration::from_secs(5),
                &open,
                false,
            )
            .await
            .expect("admitted below the mark");
        }
        let waiting = {
            let (tx, admission, waiters, open) = (
                tx.clone(),
                Arc::clone(&admission),
                Arc::clone(&waiters),
                Arc::clone(&open),
            );
            tokio::spawn(async move {
                enqueue_http_chunk(
                    dummy_chunk(),
                    &tx,
                    &admission,
                    &waiters,
                    Duration::from_secs(5),
                    &open,
                    false,
                )
                .await
            })
        };
        while waiters.available_permits() != 0 {
            tokio::task::yield_now().await;
        }

        for _ in 0..8 {
            let err = enqueue_http_chunk(
                dummy_chunk(),
                &tx,
                &admission,
                &waiters,
                Duration::from_secs(5),
                &open,
                true,
            )
            .await
            .expect_err("refused at the mark");
            assert_eq!(err, HttpChunkEnqueueError::CacheFull);
        }
        assert_eq!(admission.available_permits(), 0);
        assert_eq!(waiters.available_permits(), 0);

        // The service finishes one body, which frees a permit for the waiter.
        drop(rx.try_recv().expect("first admitted body"));
        waiting
            .await
            .expect("join")
            .expect("the waiter lands after the mark");
        let mut admitted = 1;
        while rx.try_recv().is_ok() {
            admitted += 1;
        }
        assert_eq!(admitted, 3, "admission (2) + waiters (1)");
    }
}
