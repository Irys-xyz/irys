pub mod crash;
pub mod network;

use std::future::Future;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FaultError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("signal error: {0}")]
    Signal(#[from] nix::Error),
    #[error("injection failed: {0}")]
    InjectionFailed(String),
    #[error("requires root/sudo privileges")]
    InsufficientPrivileges,
}

pub enum FaultTarget {
    Process { pid: nix::unistd::Pid },
    Network { source_port: u16, dest_port: u16 },
}

pub trait FaultInjector: Send + Sync {
    fn inject(
        &self,
        target: &FaultTarget,
    ) -> impl Future<Output = Result<FaultGuard, FaultError>> + Send;
}

pub struct FaultGuard {
    undo: Option<Box<dyn FnOnce() -> Result<(), FaultError> + Send>>,
}

impl FaultGuard {
    pub fn new(undo: impl FnOnce() -> Result<(), FaultError> + Send + 'static) -> Self {
        Self {
            undo: Some(Box::new(undo)),
        }
    }

    pub fn noop() -> Self {
        Self { undo: None }
    }

    pub fn undo(mut self) -> Result<(), FaultError> {
        if let Some(f) = self.undo.take() {
            f()?;
        }
        Ok(())
    }
}

impl Drop for FaultGuard {
    fn drop(&mut self) {
        if let Some(f) = self.undo.take()
            && let Err(e) = f()
        {
            tracing::warn!(error = %e, "fault guard undo failed on drop");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn noop_guard_undo_succeeds() {
        let guard = FaultGuard::noop();
        assert!(guard.undo().is_ok());
    }

    #[test]
    fn undo_runs_exactly_once() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter); // clone: moved into closure
        let guard = FaultGuard::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        guard.undo().unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn drop_calls_undo_if_not_already_called() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter); // clone: moved into closure
        {
            let _guard = FaultGuard::new(move || {
                c.fetch_add(1, Ordering::Relaxed);
                Ok(())
            });
        }
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn explicit_undo_prevents_double_run() {
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter); // clone: moved into closure
        let guard = FaultGuard::new(move || {
            c.fetch_add(1, Ordering::Relaxed);
            Ok(())
        });
        guard.undo().unwrap();
        assert_eq!(counter.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn undo_propagates_error() {
        let guard = FaultGuard::new(|| Err(FaultError::InjectionFailed("test error".into())));
        assert!(guard.undo().is_err());
    }

    #[test]
    fn noop_guard_has_no_side_effects() {
        let counter = Arc::new(AtomicU32::new(0));
        let val_before = counter.load(Ordering::Relaxed);
        {
            let _guard = FaultGuard::noop();
        }
        assert_eq!(counter.load(Ordering::Relaxed), val_before);
    }
}
