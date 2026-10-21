// MDBX-SUBMODULE-INDEX: remove this module with the MDBX submodule engine. The consensus database stays MDBX.
//! Page warmup and the process memlock budget for one open MDBX submodule index.
//!
//! `mdbx_env_warmup` loads the open environment and can pin it. The pin is
//! released on drop. One module holds at most one reservation.

use std::path::Path;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

use reth_db::mdbx::ffi;
use reth_db::{Database as _, DatabaseEnv};

/// 180 s, 16.16 fixed-point seconds. Zero disables the check.
const WARMUP_TIMEOUT_16DOT16: std::os::raw::c_uint = 180 << 16;
// Not `MDBX_warmup_oomsafe`: on Linux it "touches" pages by writev-ing one byte per page to /dev/null, which never
// reads the buffer, so nothing is loaded (measured on testnet: 4.7 GB "warmed" in 11 ms, then cold random reads).
// `force` alone reads one byte per page with readahead on for the range, a sequential load.
const WARMUP_LOAD: ffi::MDBX_warmup_flags_t = ffi::MDBX_warmup_force;

/// A module holds at most one reservation and never waits while holding one.
pub(super) struct MemlockBudget {
    total: u64,
    held: Mutex<u64>,
    ready: Condvar,
}

pub(super) struct MemlockPermit<'a> {
    budget: &'a MemlockBudget,
    bytes: u64,
}

impl MemlockBudget {
    pub(super) fn new(total: u64) -> Self {
        Self {
            total,
            held: Mutex::new(0),
            ready: Condvar::new(),
        }
    }

    /// `None` when `need` exceeds the budget. `on_wait(available)` runs once, under this lock.
    pub(super) fn reserve(
        &self,
        need: u64,
        on_wait: impl FnOnce(u64),
    ) -> Option<MemlockPermit<'_>> {
        if need == 0 || need > self.total {
            return None;
        }
        let mut held = self
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut free = self.total.saturating_sub(*held);
        if free < need {
            on_wait(free);
            while free < need {
                held = self
                    .ready
                    .wait(held)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                free = self.total.saturating_sub(*held);
            }
        }
        *held += need;
        Some(MemlockPermit {
            budget: self,
            bytes: need,
        })
    }
}

impl Drop for MemlockPermit<'_> {
    fn drop(&mut self) {
        let mut held = self
            .budget
            .held
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *held = held.saturating_sub(self.bytes);
        self.budget.ready.notify_all();
    }
}

fn memlock_budget() -> &'static MemlockBudget {
    static BUDGET: OnceLock<MemlockBudget> = OnceLock::new();
    // Headroom is max(soft / 8, 64 MiB). `RLIM_INFINITY` or a failed read does not gate.
    BUDGET.get_or_init(|| {
        const MIN_HEADROOM: u64 = 64 << 20;
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: `limit` is a writable `rlimit` and outlives the call.
        let rc = unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut limit) };
        let total = if rc != 0 || limit.rlim_cur == libc::RLIM_INFINITY {
            u64::MAX
        } else {
            let soft = limit.rlim_cur;
            soft.saturating_sub((soft / 8).max(MIN_HEADROOM))
        };
        MemlockBudget::new(total)
    })
}

/// Releases the pin on drop. The lock call can pin pages and still return an
/// error (a timeout after `mlock2`), so drop always unlocks, then returns any
/// reservation still held.
#[must_use = "dropping the pin releases the page lock"]
pub(super) struct WarmupPin<'a> {
    env: &'a DatabaseEnv,
    permit: Option<MemlockPermit<'static>>,
}

impl Drop for WarmupPin<'_> {
    fn drop(&mut self) {
        if let Err(error) = env_warmup(self.env, ffi::MDBX_warmup_release) {
            tracing::warn!(%error, "submodule index pre-read unlock failed");
        }
        drop(self.permit.take());
    }
}

/// Load the open env so later reads stay resident.
/// A lock error skips that load. `MDBX_warmup_touchlimit` is not set.
pub(super) fn pre_read_index_file<'a>(env: &'a DatabaseEnv, dir: &Path) -> WarmupPin<'a> {
    let path = dir.join("mdbx.dat");
    let bytes = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    let mut permit = memlock_budget().reserve(bytes, |available| {
        tracing::info!(
            path = %path.display(),
            need = bytes,
            available,
            "submodule index waiting for memlock budget"
        );
    });
    let started = Instant::now();
    let mut locked = false;
    let mut error: Option<String> = None;
    if permit.is_some() {
        match env_warmup(env, WARMUP_LOAD | ffi::MDBX_warmup_lock) {
            Ok(()) => locked = true,
            Err(err) => {
                if !err.starts_with("warmup timed out") {
                    permit = None;
                }
                error = Some(err);
            }
        }
    }
    if !locked && let Err(err) = env_warmup(env, WARMUP_LOAD) {
        error = Some(err);
    }
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    tracing::info!(
        path = %path.display(),
        bytes,
        elapsed_ms,
        locked,
        error = error.as_deref().unwrap_or(""),
        "submodule index pre-read"
    );
    WarmupPin { env, permit }
}

fn env_warmup(env: &DatabaseEnv, flags: ffi::MDBX_warmup_flags_t) -> Result<(), String> {
    let tx = env.tx().map_err(|error| error.to_string())?;
    let environment = tx.inner().env().clone();
    drop(tx);
    let rc = environment.with_raw_env_ptr(|env_ptr| unsafe {
        // SAFETY: `environment` is an Arc clone of this open env and outlives
        // the call. The read transaction is already dropped, so `txn` is null.
        // libmdbx accepts a live env with a null txn. The pointer is not stored.
        ffi::mdbx_env_warmup(env_ptr, std::ptr::null(), flags, WARMUP_TIMEOUT_16DOT16)
    });
    if rc == 0 {
        return Ok(());
    }
    let detail = if rc == ffi::MDBX_RESULT_TRUE {
        std::borrow::Cow::Borrowed("warmup timed out")
    } else {
        unsafe {
            // SAFETY: `mdbx_strerror` returns a C string that stays valid for this copy.
            std::ffi::CStr::from_ptr(ffi::mdbx_strerror(rc)).to_string_lossy()
        }
    };
    Err(format!("{detail} (rc={rc})"))
}
