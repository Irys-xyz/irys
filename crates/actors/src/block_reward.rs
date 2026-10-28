use irys_reward_curve::HalvingCurve;
use irys_types::{
    ConsensusConfig, U256, UnixTimestamp,
    hardfork_config::Activation,
    storage_pricing::{Amount, PRECISION_SCALE, mul_div, phantoms::Irys, safe_sub},
};

/// A block's full reward-curve amount, split between the miner and the
/// treasury. `miner + treasury` is always the full amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRewardSplit {
    /// Paid to the miner's reward address and declared as `reward_amount`.
    pub miner: Amount<Irys>,
    /// Added to the block's CL treasury.
    pub treasury: Amount<Irys>,
}

/// The block reward curve that the consensus config sets.
pub fn reward_curve_from_config(consensus: &ConsensusConfig) -> HalvingCurve {
    HalvingCurve {
        inflation_cap: consensus.block_reward_config.inflation_cap,
        half_life_secs: consensus.block_reward_config.half_life_secs.into(),
    }
}

/// Full reward-curve amount for the block at `parent_height + 1`.
///
/// Uses fixed block intervals, not timestamps, so a producer cannot move its
/// timestamp to raise the reward, and emission stays steady over time.
pub fn full_block_reward(
    reward_curve: &HalvingCurve,
    block_time_secs: u64,
    parent_height: u64,
) -> eyre::Result<Amount<Irys>> {
    let block_time_secs = u128::from(block_time_secs);
    let parent_block_secs = u128::from(parent_height) * block_time_secs;
    reward_curve.reward_between(parent_block_secs, parent_block_secs + block_time_secs)
}

/// Tokens the block at `height` emits: its full reward-curve amount, whether
/// it goes to the miner or to the treasury. The genesis block emits nothing.
pub fn block_emission(
    reward_curve: &HalvingCurve,
    block_time_secs: u64,
    height: u64,
) -> eyre::Result<U256> {
    match height.checked_sub(1) {
        None => Ok(U256::zero()),
        Some(parent_height) => {
            Ok(full_block_reward(reward_curve, block_time_secs, parent_height)?.amount)
        }
    }
}

/// Splits the full reward-curve amount by PoA kind.
///
/// After Delta, a capacity PoA pays the miner `Delta::capacity_chunk_reward_percent`
/// of the full amount (rounded down) and the treasury gets the rest, so no
/// part of the curve is lost. A data PoA, or any block before Delta, pays the
/// miner the full amount.
///
/// A block's own timestamp controls Delta activation. The caller must pass
/// `!poa.is_data_poa()` as `is_capacity_poa`. The PoA kind cannot be faked:
/// PoA validation checks a data PoA against its data path and an entropy PoA
/// against computed entropy.
pub fn split_block_reward(
    full_reward: Amount<Irys>,
    consensus: &ConsensusConfig,
    block_timestamp: UnixTimestamp,
    is_capacity_poa: bool,
) -> eyre::Result<BlockRewardSplit> {
    let (true, Activation::Active(delta)) = (
        is_capacity_poa,
        consensus.hardforks.delta_for_block(block_timestamp),
    ) else {
        return Ok(BlockRewardSplit {
            miner: full_reward,
            treasury: Amount::new(U256::zero()),
        });
    };

    let miner = mul_div(
        full_reward.amount,
        delta.capacity_chunk_reward_percent.amount,
        PRECISION_SCALE,
    )?;
    let treasury = safe_sub(full_reward.amount, miner)?;
    Ok(BlockRewardSplit {
        miner: Amount::new(miner),
        treasury: Amount::new(treasury),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::{
        Base64, PoaData, UnixTimestampMs, hardfork_config::Delta, storage_pricing::Decimal,
    };

    fn config_with_delta(activation: u64) -> ConsensusConfig {
        let mut config = ConsensusConfig::testing();
        config.hardforks.delta = Some(Delta {
            activation_timestamp: UnixTimestamp::from_secs(activation),
            initial_slots_per_new_ledger: Delta::default_initial_slots_per_new_ledger(),
            capacity_chunk_reward_percent: Delta::default_capacity_chunk_reward_percent(),
        });
        config
    }

    fn config_with_delta_percent(activation: u64, percent: Decimal) -> ConsensusConfig {
        let mut config = config_with_delta(activation);
        let Some(delta) = config.hardforks.delta.as_mut() else {
            unreachable!("config_with_delta sets delta");
        };
        delta.capacity_chunk_reward_percent = Amount::percentage(percent).unwrap();
        config
    }

    fn amount(value: u64) -> Amount<Irys> {
        Amount::new(value.into())
    }

    fn split(miner: u64, treasury: u64) -> BlockRewardSplit {
        BlockRewardSplit {
            miner: amount(miner),
            treasury: amount(treasury),
        }
    }

    #[test]
    fn pre_delta_capacity_poa_receives_full_reward() {
        let config = config_with_delta(100);
        assert_eq!(
            split_block_reward(amount(101), &config, UnixTimestamp::from_secs(99), true).unwrap(),
            split(101, 0)
        );
    }

    #[test]
    fn delta_data_poa_receives_full_reward() {
        let config = config_with_delta(100);
        assert_eq!(
            split_block_reward(amount(101), &config, UnixTimestamp::from_secs(100), false).unwrap(),
            split(101, 0)
        );
    }

    #[test]
    fn delta_capacity_poa_receives_configured_percentage() {
        let config = config_with_delta_percent(100, Decimal::new(25, 2));
        assert_eq!(
            split_block_reward(amount(101), &config, UnixTimestamp::from_secs(100), true).unwrap(),
            split(25, 76)
        );
    }

    #[test]
    fn rounding_remainder_goes_to_treasury() {
        let config = config_with_delta(100);
        assert_eq!(
            split_block_reward(amount(101), &config, UnixTimestamp::from_secs(100), true).unwrap(),
            split(50, 51)
        );
    }

    #[rstest::rstest]
    #[case::zero_percent(Decimal::ZERO)]
    #[case::one_third(Decimal::new(3333, 4))]
    #[case::half(Decimal::new(5, 1))]
    #[case::full(Decimal::ONE)]
    fn split_sums_to_full_reward(#[case] percent: Decimal) {
        let config = config_with_delta_percent(100, percent);
        for full in [0_u64, 1, 2, 3, 101, 999_999_999_999] {
            let result =
                split_block_reward(amount(full), &config, UnixTimestamp::from_secs(100), true)
                    .unwrap();
            assert_eq!(
                result.miner.amount + result.treasury.amount,
                U256::from(full),
                "percent {percent}, full {full}"
            );
        }
    }

    #[rstest::rstest]
    #[case::before_activation_second(99_999, false)]
    #[case::first_activation_ms(100_000, true)]
    #[case::last_activation_second_ms(100_999, true)]
    #[case::next_second(101_000, true)]
    fn activation_boundary_uses_truncated_milliseconds(
        #[case] timestamp_ms: u128,
        #[case] reduced: bool,
    ) {
        let config = config_with_delta(100);
        let reward = split_block_reward(
            amount(100),
            &config,
            UnixTimestampMs::from_millis(timestamp_ms).to_secs(),
            true,
        )
        .unwrap();
        let expected = if reduced {
            split(50, 50)
        } else {
            split(100, 0)
        };
        assert_eq!(reward, expected);
    }

    #[test]
    fn no_delta_capacity_poa_receives_full_reward() {
        let mut config = ConsensusConfig::testing();
        config.hardforks.delta = None;
        assert_eq!(
            split_block_reward(
                amount(101),
                &config,
                UnixTimestamp::from_secs(u64::MAX),
                true
            )
            .unwrap(),
            split(101, 0)
        );
    }

    #[rstest::rstest]
    #[case::data(Some(Base64(vec![1])), Some(Base64(vec![1])), Some(0), false)]
    #[case::entropy(None, None, None, true)]
    #[case::data_ledger_without_paths(None, None, Some(0), true)]
    #[case::paths_without_ledger(Some(Base64(vec![1])), Some(Base64(vec![1])), None, true)]
    fn reward_follows_poa_kind_not_partition(
        #[case] tx_path: Option<Base64>,
        #[case] data_path: Option<Base64>,
        #[case] ledger_id: Option<u32>,
        #[case] reduced: bool,
    ) {
        // The partition hash is the same in every case: only the PoA kind decides.
        let poa = PoaData {
            partition_chunk_offset: 0,
            partition_hash: Default::default(),
            chunk: None,
            ledger_id,
            tx_path,
            data_path,
        };
        let config = config_with_delta(100);
        let reward = split_block_reward(
            amount(100),
            &config,
            UnixTimestamp::from_secs(100),
            !poa.is_data_poa(),
        )
        .unwrap();
        let expected = if reduced {
            split(50, 50)
        } else {
            split(100, 0)
        };
        assert_eq!(reward, expected);
    }

    #[test]
    fn full_block_reward_samples_curve_at_fixed_block_interval() {
        let config = ConsensusConfig::testing();
        let curve = reward_curve_from_config(&config);
        let block_time = config.difficulty_adjustment.block_time;
        let parent_height = 7;
        let from = u128::from(parent_height * block_time);
        assert_eq!(
            full_block_reward(&curve, block_time, parent_height).unwrap(),
            curve
                .reward_between(from, from + u128::from(block_time))
                .unwrap()
        );
    }

    #[test]
    fn block_emission_is_zero_at_genesis_and_full_reward_after() {
        let config = ConsensusConfig::testing();
        let curve = reward_curve_from_config(&config);
        let block_time = config.difficulty_adjustment.block_time;
        assert_eq!(block_emission(&curve, block_time, 0).unwrap(), U256::zero());
        for height in 1..5 {
            assert_eq!(
                block_emission(&curve, block_time, height).unwrap(),
                full_block_reward(&curve, block_time, height - 1)
                    .unwrap()
                    .amount
            );
        }
    }
}
