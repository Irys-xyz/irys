//! Thread-local marker for PD precompile return data provenance.
//!
//! Stores an owned [`Bytes`] clone so the custom `RETURNDATACOPY` handler can
//! determine whether the current return data originated from the PD precompile.
//! Identity comparison (pointer + length) is used rather than content equality
//! because `Bytes` shares its backing allocation across clones.

use std::cell::RefCell;

use alloy_primitives::Bytes;

thread_local! {
    static LAST_PD_RETURN: RefCell<Option<Bytes>> = const { RefCell::new(None) };
}

/// RAII scope guard that clears the PD return marker on creation and on drop.
///
/// Intended to wrap the lifetime of a single EVM transaction execution so that
/// stale marker state from a previous transaction can never leak.
pub(crate) struct PdReturnMarkerScope;

impl PdReturnMarkerScope {
    pub(crate) fn new() -> Self {
        clear_pd_return_marker();
        Self
    }
}

impl Drop for PdReturnMarkerScope {
    fn drop(&mut self) {
        clear_pd_return_marker();
    }
}

/// Remove any stored PD return marker from the thread-local slot.
pub(crate) fn clear_pd_return_marker() {
    LAST_PD_RETURN.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

/// Record `bytes` as the most recent PD precompile return value.
///
/// Empty bytes are treated as "no marker" and clear the slot instead.
pub(crate) fn set_pd_return_marker(bytes: &Bytes) {
    if bytes.is_empty() {
        clear_pd_return_marker();
        return;
    }

    LAST_PD_RETURN.with(|slot| {
        *slot.borrow_mut() = Some(bytes.clone());
    });
}

/// Check whether `bytes` is the *same allocation* that was stored by
/// [`set_pd_return_marker`].
///
/// This is an identity check (pointer + length), not a content comparison.
/// Returns `false` for empty input regardless of stored state.
pub(crate) fn is_current_pd_return_marker(bytes: &Bytes) -> bool {
    if bytes.is_empty() {
        return false;
    }

    LAST_PD_RETURN.with(|slot| {
        slot.borrow()
            .as_ref()
            .is_some_and(|stored| stored.len() == bytes.len() && stored.as_ptr() == bytes.as_ptr())
    })
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Bytes;

    use super::*;

    #[test]
    fn same_bytes_matches() {
        let data = Bytes::from_static(b"pd-chunk-data");
        set_pd_return_marker(&data);
        assert!(is_current_pd_return_marker(&data));
        clear_pd_return_marker();
    }

    #[test]
    fn cloned_bytes_matches() {
        let data = Bytes::from(vec![1_u8, 2, 3, 4]);
        let cloned = data.clone();
        // A clone of `Bytes` shares the same backing allocation.
        set_pd_return_marker(&data);
        assert!(is_current_pd_return_marker(&cloned));
        clear_pd_return_marker();
    }

    #[test]
    fn same_contents_different_allocation_does_not_match() {
        let a = Bytes::from(vec![10_u8, 20, 30]);
        let b = Bytes::from(vec![10_u8, 20, 30]);
        // Same contents but independent allocations.
        set_pd_return_marker(&a);
        assert!(!is_current_pd_return_marker(&b));
        clear_pd_return_marker();
    }

    #[test]
    fn empty_bytes_never_match() {
        let empty = Bytes::new();
        // Setting with empty bytes should clear instead of store.
        set_pd_return_marker(&empty);
        assert!(!is_current_pd_return_marker(&empty));

        // Even if a non-empty marker is stored, empty input returns false.
        let data = Bytes::from_static(b"nonempty");
        set_pd_return_marker(&data);
        assert!(!is_current_pd_return_marker(&Bytes::new()));
        clear_pd_return_marker();
    }

    #[test]
    fn scope_guard_clears_on_entry_and_drop() {
        let data = Bytes::from_static(b"scoped");
        set_pd_return_marker(&data);
        assert!(is_current_pd_return_marker(&data));

        {
            let _guard = PdReturnMarkerScope::new();
            // Marker should be cleared on scope entry.
            assert!(!is_current_pd_return_marker(&data));

            // Set it again inside the scope.
            set_pd_return_marker(&data);
            assert!(is_current_pd_return_marker(&data));
        }

        // After the guard is dropped the marker should be cleared.
        assert!(!is_current_pd_return_marker(&data));
    }

    #[test]
    fn default_state_is_clear() {
        // On a fresh thread the marker should not match anything.
        let data = Bytes::from_static(b"anything");
        assert!(!is_current_pd_return_marker(&data));
    }

    #[test]
    fn stale_allocation_safety() {
        // The stored clone keeps the allocation alive, so even if the
        // original `Bytes` handle is dropped the marker remains valid and
        // cannot accidentally match a new allocation at the same address.
        let data = Bytes::from(vec![42_u8; 64]);
        let cloned = data.clone();
        set_pd_return_marker(&data);

        // Drop the original handle; the stored clone still owns the allocation.
        drop(data);

        // Allocate new data that *might* reuse the same heap address if the
        // allocation had been freed (it hasn't).
        let new_data = Bytes::from(vec![42_u8; 64]);

        // The cloned handle (same allocation as original) still matches.
        assert!(is_current_pd_return_marker(&cloned));

        // The independently-allocated data must not match, even if the
        // allocator happened to hand out the same address (it can't because
        // the stored clone keeps the original alive).
        assert!(!is_current_pd_return_marker(&new_data));

        clear_pd_return_marker();
    }
}
