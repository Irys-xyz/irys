//! Pure, stateless VDF check functions.
//!
//! These join the existing VDF validators (`last_step_checkpoints_is_valid`,
//! `vdf_step_batch_is_valid`) to give `irys-vdf` one verification surface. They
//! return `eyre::Result<()>`; the actor validation layer maps the result back to
//! its own `ValidationResult` / `PreValidationError` types at the call sites,
//! preserving the exact error variant, message, and metric label.
//!
//! The `is_seed_data_valid` tracing span's target moves with the function, from
//! `irys_actors::block_validation` to `irys_vdf::verify`. This is intentional:
//! the check now lives here, and the span is a trace-level diagnostic rather than
//! part of the preserved OTEL metric surface.

use irys_types::IrysBlockHeader;

/// Validate a block's VDF `seed` / `next_seed` against its parent.
///
/// Recomputes the expected seed pair from the parent header and the reset
/// frequency and compares it to the block's embedded seeds.
#[tracing::instrument(level = "trace", skip_all)]
pub fn is_seed_data_valid(
    block_header: &IrysBlockHeader,
    previous_block_header: &IrysBlockHeader,
    reset_frequency: u64,
) -> eyre::Result<()> {
    let vdf_info = &block_header.vdf_limiter_info;
    let expected_seed_data = vdf_info.calculate_seeds(reset_frequency, previous_block_header);

    // TODO: difficulty validation adjustment is likely needs to be done here too,
    //  but difficulty is not yet implemented
    let are_seeds_valid =
        expected_seed_data.0 == vdf_info.next_seed && expected_seed_data.1 == vdf_info.seed;
    if are_seeds_valid {
        Ok(())
    } else {
        tracing::error!(
            "Seed data is invalid. Expected: {:?}, got: {:?}",
            expected_seed_data,
            vdf_info
        );
        Err(eyre::eyre!(
            "Expected: {:?}, got: {:?}",
            expected_seed_data,
            vdf_info
        ))
    }
}

/// Validate that a block's VDF `prev_output` matches the parent's `output`.
pub fn prev_output_is_valid(
    block: &IrysBlockHeader,
    previous_block: &IrysBlockHeader,
) -> eyre::Result<()> {
    if block.vdf_limiter_info.prev_output == previous_block.vdf_limiter_info.output {
        Ok(())
    } else {
        Err(eyre::eyre!(
            "VDF prev_output mismatch: got {:?}, expected {:?}",
            block.vdf_limiter_info.prev_output,
            previous_block.vdf_limiter_info.output
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{BlockHash, H256, H256List, IrysBlockHeader};

    // Ported verbatim in behaviour from the former
    // actors::block_validation::is_seed_data_valid_should_validate_seeds:
    // one valid branch and both invalid branches, tested directly.
    #[test]
    fn is_seed_data_valid_accepts_rotated_seeds() {
        let reset_frequency = 2;
        let mut parent = IrysBlockHeader::new_mock_header();
        let parent_next_seed = BlockHash::from_slice(&[3; 32]);
        parent.block_hash = BlockHash::from_slice(&[4; 32]);
        parent.vdf_limiter_info.seed = BlockHash::from_slice(&[2; 32]);
        parent.vdf_limiter_info.next_seed = parent_next_seed;

        let mut block = IrysBlockHeader::new_mock_header();
        // reset_frequency 2 + global_step 3 + 2 steps => seeds rotate
        block.vdf_limiter_info.global_step_number = 3;
        block.vdf_limiter_info.steps = H256List(vec![H256::zero(); 2]);
        block.vdf_limiter_info.set_seeds(reset_frequency, &parent);

        assert_eq!(block.vdf_limiter_info.next_seed, parent.block_hash);
        assert_eq!(block.vdf_limiter_info.seed, parent_next_seed);
        assert!(is_seed_data_valid(&block, &parent, reset_frequency).is_ok());
    }

    #[test]
    fn is_seed_data_valid_rejects_wrong_reset_frequency() {
        let reset_frequency = 2;
        let mut parent = IrysBlockHeader::new_mock_header();
        parent.block_hash = BlockHash::from_slice(&[4; 32]);
        parent.vdf_limiter_info.seed = BlockHash::from_slice(&[2; 32]);
        parent.vdf_limiter_info.next_seed = BlockHash::from_slice(&[3; 32]);

        let mut block = IrysBlockHeader::new_mock_header();
        block.vdf_limiter_info.global_step_number = 3;
        block.vdf_limiter_info.steps = H256List(vec![H256::zero(); 2]);
        block.vdf_limiter_info.set_seeds(reset_frequency, &parent);

        // Same embedded seeds, validated against a frequency that does not
        // trigger rotation => mismatch.
        assert!(is_seed_data_valid(&block, &parent, 100).is_err());
    }

    #[test]
    fn is_seed_data_valid_rejects_random_seeds() {
        let reset_frequency = 2;
        let mut parent = IrysBlockHeader::new_mock_header();
        parent.block_hash = BlockHash::from_slice(&[4; 32]);
        parent.vdf_limiter_info.seed = BlockHash::from_slice(&[2; 32]);
        parent.vdf_limiter_info.next_seed = BlockHash::from_slice(&[3; 32]);

        let mut block = IrysBlockHeader::new_mock_header();
        block.vdf_limiter_info.global_step_number = 3;
        block.vdf_limiter_info.steps = H256List(vec![H256::zero(); 2]);
        block.vdf_limiter_info.set_seeds(reset_frequency, &parent);
        block.vdf_limiter_info.seed = BlockHash::from_slice(&[5; 32]);
        block.vdf_limiter_info.next_seed = BlockHash::from_slice(&[6; 32]);

        assert!(is_seed_data_valid(&block, &parent, reset_frequency).is_err());
    }

    #[test]
    fn prev_output_is_valid_accepts_matching_output() {
        let mut parent = IrysBlockHeader::new_mock_header();
        parent.vdf_limiter_info.output = H256::from_low_u64_be(42);
        let mut block = IrysBlockHeader::new_mock_header();
        block.vdf_limiter_info.prev_output = H256::from_low_u64_be(42);
        assert!(prev_output_is_valid(&block, &parent).is_ok());
    }

    #[test]
    fn prev_output_is_valid_rejects_mismatched_output() {
        let mut parent = IrysBlockHeader::new_mock_header();
        parent.vdf_limiter_info.output = H256::from_low_u64_be(42);
        let mut block = IrysBlockHeader::new_mock_header();
        block.vdf_limiter_info.prev_output = H256::from_low_u64_be(7);
        assert!(prev_output_is_valid(&block, &parent).is_err());
    }
}
