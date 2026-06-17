//! Shared structural validation for data transactions.
//!
//! The rules below are enforced identically at mempool ingress
//! (`precheck_data_ingress_common`) and at consensus prevalidation
//! (`prevalidate_block`). Defining them once here keeps the two gates from drifting:
//! a tx ingress accepts is guaranteed to clear the same structural checks at consensus,
//! and a hand-crafted peer block can't smuggle past consensus what ingress would reject.
//!
//! Each call site maps the returned defect to its own error type — `TxIngressError` at
//! ingress, `PreValidationError` at consensus — so only the *rule* is shared, not the
//! layer-specific error.

use irys_types::DataTransactionHeader;

/// A structural defect in a data transaction header, in the order the checks are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataTxStructuralDefect {
    /// `data_size == 0`: stores no data and would inject a zero-width leaf into the ledger
    /// `tx_root` tree, breaking the unique-start-offset invariant PoA owning-tx recovery
    /// relies on.
    ZeroDataSize,
    /// `prefix_size > data_size`: `prefix_hash` commits to the first `prefix_size` data
    /// bytes, so a prefix longer than the data is a structurally impossible claim.
    PrefixSizeExceedsDataSize { prefix_size: u64, data_size: u64 },
    /// The tx's signed `chain_id` differs from this node's — it was signed for another chain.
    ChainIdMismatch { expected: u64, actual: u64 },
}

/// The first structural defect of a data transaction (if any), given the node's
/// `expected_chain_id`. Returns `None` for a well-formed header.
///
/// Shared by mempool ingress and consensus prevalidation so the two gates enforce
/// byte-identical rules. The check order is load-bearing for which defect is reported
/// (zero-size, then oversized prefix, then foreign chain_id), though any defect rejects the
/// transaction either way.
pub fn data_tx_structural_defect(
    tx: &DataTransactionHeader,
    expected_chain_id: u64,
) -> Option<DataTxStructuralDefect> {
    if tx.data_size == 0 {
        return Some(DataTxStructuralDefect::ZeroDataSize);
    }
    if tx.prefix_size > tx.data_size {
        return Some(DataTxStructuralDefect::PrefixSizeExceedsDataSize {
            prefix_size: tx.prefix_size,
            data_size: tx.data_size,
        });
    }
    if tx.chain_id != expected_chain_id {
        return Some(DataTxStructuralDefect::ChainIdMismatch {
            expected: expected_chain_id,
            actual: tx.chain_id,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::ConsensusConfig;

    fn header(
        consensus: &ConsensusConfig,
        data_size: u64,
        prefix_size: u64,
        chain_id: u64,
    ) -> DataTransactionHeader {
        let mut h = DataTransactionHeader::new(consensus);
        h.data_size = data_size;
        h.prefix_size = prefix_size;
        h.chain_id = chain_id;
        h
    }

    #[test]
    fn accepts_well_formed_tx() {
        let c = ConsensusConfig::testing();
        // prefix_size == data_size (whole-data prefix) and prefix_size == 0 are both valid.
        assert_eq!(
            data_tx_structural_defect(&header(&c, 100, 100, c.chain_id), c.chain_id),
            None
        );
        assert_eq!(
            data_tx_structural_defect(&header(&c, 100, 0, c.chain_id), c.chain_id),
            None
        );
    }

    #[test]
    fn flags_zero_data_size() {
        let c = ConsensusConfig::testing();
        assert_eq!(
            data_tx_structural_defect(&header(&c, 0, 0, c.chain_id), c.chain_id),
            Some(DataTxStructuralDefect::ZeroDataSize),
        );
    }

    #[test]
    fn flags_prefix_size_exceeding_data_size() {
        let c = ConsensusConfig::testing();
        assert_eq!(
            data_tx_structural_defect(&header(&c, 100, 101, c.chain_id), c.chain_id),
            Some(DataTxStructuralDefect::PrefixSizeExceedsDataSize {
                prefix_size: 101,
                data_size: 100,
            }),
        );
    }

    #[test]
    fn flags_foreign_chain_id() {
        let c = ConsensusConfig::testing();
        assert_eq!(
            data_tx_structural_defect(&header(&c, 100, 0, c.chain_id + 1), c.chain_id),
            Some(DataTxStructuralDefect::ChainIdMismatch {
                expected: c.chain_id,
                actual: c.chain_id + 1,
            }),
        );
    }

    #[test]
    fn check_order_zero_size_then_prefix_then_chain_id() {
        let c = ConsensusConfig::testing();
        // zero data_size wins even when prefix and chain_id are also wrong
        assert_eq!(
            data_tx_structural_defect(&header(&c, 0, 5, c.chain_id + 1), c.chain_id),
            Some(DataTxStructuralDefect::ZeroDataSize),
        );
        // oversized prefix wins over a foreign chain_id when data_size > 0
        assert_eq!(
            data_tx_structural_defect(&header(&c, 10, 11, c.chain_id + 1), c.chain_id),
            Some(DataTxStructuralDefect::PrefixSizeExceedsDataSize {
                prefix_size: 11,
                data_size: 10,
            }),
        );
    }
}
