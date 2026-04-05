use irys_types::chunk_provider::{PdChunkMessage, PdChunkSender};
use reth::revm::primitives::B256;

/// RAII guard that sends `ReleaseBlockChunks` when dropped.
///
/// Created after `ProvisionBlockChunks` succeeds during block validation.
/// Automatically releases chunks on any exit path — normal return, error
/// propagation via `?`, task cancellation, or panic.
///
/// Call [`disarm()`](Self::disarm) to prevent the release (e.g., if you want
/// to transfer ownership to another scope, though currently unused).
#[must_use = "guard releases PD chunks on drop; bind it to a variable"]
pub(crate) struct PdBlockGuard {
    sender: Option<PdChunkSender>,
    block_hash: B256,
}

impl PdBlockGuard {
    /// Create a new guard that will release chunks for `block_hash` on drop.
    pub(crate) fn new(sender: PdChunkSender, block_hash: B256) -> Self {
        Self {
            sender: Some(sender),
            block_hash,
        }
    }

    /// Prevent the guard from sending `ReleaseBlockChunks` on drop.
    #[cfg(test)]
    pub(crate) fn disarm(&mut self) {
        self.sender.take();
    }
}

impl Drop for PdBlockGuard {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(PdChunkMessage::ReleaseBlockChunks {
                block_hash: self.block_hash,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[test]
    fn test_guard_sends_release_on_drop() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let hash = B256::with_last_byte(0xAA);
        {
            let _guard = PdBlockGuard::new(tx, hash);
        }
        match rx.try_recv() {
            Ok(PdChunkMessage::ReleaseBlockChunks { block_hash }) => {
                assert_eq!(block_hash, hash);
            }
            other => panic!("expected ReleaseBlockChunks, got {:?}", other),
        }
    }

    #[test]
    fn test_guard_disarm_prevents_release() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let hash = B256::with_last_byte(0xBB);
        {
            let mut guard = PdBlockGuard::new(tx, hash);
            guard.disarm();
        }
        assert!(rx.try_recv().is_err(), "disarmed guard should not send");
    }

    #[test]
    fn test_guard_sends_release_on_panic() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let hash = B256::with_last_byte(0xCC);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = PdBlockGuard::new(tx, hash);
            panic!("simulated panic");
        }));
        assert!(result.is_err());
        match rx.try_recv() {
            Ok(PdChunkMessage::ReleaseBlockChunks { block_hash }) => {
                assert_eq!(block_hash, hash);
            }
            other => panic!("expected ReleaseBlockChunks, got {:?}", other),
        }
    }
}
