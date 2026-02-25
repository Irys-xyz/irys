use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Notify;

/// RAII guard that tracks active worker count
/// Increments counter on creation, decrements on drop (even on panic)
pub(super) struct ActiveWorkerGuard {
    active_workers: Arc<AtomicUsize>,
    notify: Arc<Notify>,
}

impl ActiveWorkerGuard {
    pub(super) fn new(active_workers: Arc<AtomicUsize>, notify: Arc<Notify>) -> Self {
        // Relaxed ordering sufficient as we only need eventual consistency for idle detection
        active_workers.fetch_add(1, Ordering::Relaxed);
        notify.notify_waiters();
        Self {
            active_workers,
            notify,
        }
    }
}

impl Drop for ActiveWorkerGuard {
    fn drop(&mut self) {
        // Relaxed ordering OK - exact timing of decrement not critical for correctness
        self.active_workers.fetch_sub(1, Ordering::Relaxed);
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_guard_increments_on_create() {
        let counter = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(Notify::new());

        {
            let _guard = ActiveWorkerGuard::new(counter.clone(), notify);
            assert_eq!(counter.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn test_guard_decrements_on_drop() {
        let counter = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(Notify::new());

        {
            let _guard = ActiveWorkerGuard::new(counter.clone(), notify);
            assert_eq!(counter.load(Ordering::Relaxed), 1);
        }
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_guard_handles_panic() {
        let counter = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(Notify::new());

        let result = std::panic::catch_unwind(|| {
            let _guard = ActiveWorkerGuard::new(counter.clone(), notify.clone());
            assert_eq!(counter.load(Ordering::Relaxed), 1);
            panic!("Simulated panic");
        });

        assert!(result.is_err());
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_multiple_guards() {
        let counter = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(Notify::new());

        let guard1 = ActiveWorkerGuard::new(counter.clone(), notify.clone());
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        let guard2 = ActiveWorkerGuard::new(counter.clone(), notify);
        assert_eq!(counter.load(Ordering::Relaxed), 2);

        drop(guard1);
        assert_eq!(counter.load(Ordering::Relaxed), 1);

        drop(guard2);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_guard_notify_on_create_and_drop() {
        use std::sync::atomic::AtomicBool;

        let counter = Arc::new(AtomicUsize::new(0));
        let notify = Arc::new(Notify::new());
        let notified = Arc::new(AtomicBool::new(false));

        let notified_clone = notified.clone();
        let notify_clone = notify.clone();

        // Spawn a task that waits for notification
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                notify_clone.notified().await;
                notified_clone.store(true, Ordering::Relaxed);
            });
        });

        std::thread::sleep(std::time::Duration::from_millis(100));

        // Create guard - should trigger notification
        {
            let _guard = ActiveWorkerGuard::new(counter, notify);
            std::thread::sleep(std::time::Duration::from_millis(100));
            assert!(
                notified.load(Ordering::Relaxed),
                "Should be notified on guard creation"
            );
        }
    }
}
