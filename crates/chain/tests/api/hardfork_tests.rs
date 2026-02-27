use crate::utils::IrysNodeTest;
use irys_chain::IrysNodeCtx;
use irys_testing_utils::initialize_tracing;
use irys_types::{
    hardfork_config::{Aurora, FrontierParams, IrysHardforkConfig},
    irys::IrysSigner,
    CommitmentTransaction, CommitmentTransactionV1, CommitmentTransactionV2, CommitmentTypeV1,
    CommitmentTypeV2, ConsensusConfig, IrysTransactionId, NodeConfig, UnixTimestamp,
};
use rstest::rstest;
use tracing::info;

const ONE_HOUR_SECS: u64 = 3600;
const ACTIVATION_DELAY_SECS: u64 = 10;
const AURORA_MIN_VERSION: u8 = 2;
const MAX_ACTIVATION_BLOCKS: u32 = 50;
const MAX_BLOCKS_TO_SEARCH: u64 = 5;
const POLL_INTERVAL_MS: u64 = 100;

fn assert_http_bad_request(err: &eyre::Error) {
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("400") || err_msg.contains("Bad Request"),
        "Expected HTTP 400 Bad Request, got: {}",
        err_msg
    );
}

fn now_secs() -> u64 {
    UnixTimestamp::now()
        .expect("system time should be after unix epoch")
        .as_secs()
}

#[derive(Clone, Copy, Debug)]
enum TxVersion {
    V1,
    V2,
}

async fn create_stake_tx(
    node: &IrysNodeTest<IrysNodeCtx>,
    signer: &IrysSigner,
    version: TxVersion,
) -> CommitmentTransaction {
    let price_info = node
        .get_stake_price()
        .await
        .expect("Failed to get stake price");

    let consensus = &node.node_ctx.config.consensus;
    let anchor = node.get_anchor().await.expect("anchor should be available");
    let fee = price_info.fee.try_into().expect("fee should fit in u64");

    let mut stake_tx = create_stake_inner(version, consensus, anchor, fee, price_info.value);
    signer.sign_commitment(&mut stake_tx).unwrap();
    stake_tx
}

fn create_stake_inner(
    version: TxVersion,
    consensus: &ConsensusConfig,
    anchor: irys_types::H256,
    fee: u64,
    value: irys_types::U256,
) -> CommitmentTransaction {
    match version {
        TxVersion::V1 => CommitmentTransaction::V1(irys_types::CommitmentV1WithMetadata {
            tx: CommitmentTransactionV1 {
                commitment_type: CommitmentTypeV1::Stake,
                anchor,
                fee,
                value,
                ..CommitmentTransactionV1::new(consensus)
            },
            metadata: Default::default(),
        }),
        TxVersion::V2 => CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
            tx: CommitmentTransactionV2 {
                commitment_type: CommitmentTypeV2::Stake,
                anchor,
                fee,
                value,
                ..CommitmentTransactionV2::new(consensus)
            },
            metadata: Default::default(),
        }),
    }
}

fn default_test_frontier() -> FrontierParams {
    FrontierParams {
        number_of_ingress_proofs_total: 2,
        number_of_ingress_proofs_from_assignees: 0,
    }
}

fn create_test_config(aurora: Option<Aurora>) -> NodeConfig {
    let mut config = NodeConfig::testing();
    config.consensus.get_mut().hardforks = IrysHardforkConfig {
        frontier: default_test_frontier(),
        next_name_tbd: None,
        sprite: None,
        aurora,
        borealis: None,
    };
    config
}

fn aurora_config_with_timestamp(activation_secs: u64) -> NodeConfig {
    create_test_config(Some(Aurora {
        activation_timestamp: UnixTimestamp::from_secs(activation_secs),
        minimum_commitment_tx_version: AURORA_MIN_VERSION,
    }))
}

fn aurora_config_with_offset(offset_secs: i64) -> NodeConfig {
    let timestamp = if offset_secs >= 0 {
        now_secs().saturating_add(offset_secs as u64)
    } else {
        now_secs().saturating_sub(offset_secs.unsigned_abs())
    };
    aurora_config_with_timestamp(timestamp)
}

fn aurora_config_future() -> NodeConfig {
    aurora_config_with_offset(ONE_HOUR_SECS as i64)
}

fn aurora_config_past() -> NodeConfig {
    aurora_config_with_offset(-(ONE_HOUR_SECS as i64))
}

fn create_funded_signer(config: &mut NodeConfig) -> IrysSigner {
    let signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&signer]);
    signer
}

fn create_funded_signers<const N: usize>(config: &mut NodeConfig) -> [IrysSigner; N] {
    let signers: [IrysSigner; N] =
        std::array::from_fn(|_| IrysSigner::random_signer(&config.consensus_config()));
    let signer_refs: Vec<&IrysSigner> = signers.iter().collect();
    config.fund_genesis_accounts(signer_refs);
    signers
}

async fn find_tx_in_blocks(
    node: &IrysNodeTest<IrysNodeCtx>,
    tx_id: &IrysTransactionId,
    max_height: u64,
) -> Option<u64> {
    let mut scanned = 0_u64;
    let mut skipped = 0_u64;
    for height in 1..=max_height {
        if let Ok(block) = node.get_block_by_height(height).await {
            scanned += 1;
            if block.commitment_tx_ids().contains(tx_id) {
                return Some(height);
            }
        } else {
            skipped += 1;
        }
    }
    tracing::debug!(
        ?tx_id,
        scanned,
        skipped,
        max_height,
        "tx not found in {scanned} blocks ({skipped} heights failed to load)"
    );
    None
}

async fn wait_until_activation(node: &IrysNodeTest<IrysNodeCtx>, activation_timestamp: u64) {
    info!("Waiting for Aurora activation...");
    let mut last_block_timestamp = 0_u64;
    for _ in 0..MAX_ACTIVATION_BLOCKS {
        let block = node.mine_block().await.expect("mining should succeed");
        last_block_timestamp = block.timestamp_secs().as_secs();
        if last_block_timestamp >= activation_timestamp {
            info!("Aurora activated at block {}", block.height);
            return;
        }
    }
    panic!(
        "Failed to reach activation timestamp {} after {} blocks (last block timestamp: {})",
        activation_timestamp, MAX_ACTIVATION_BLOCKS, last_block_timestamp
    );
}

async fn wait_for_wallclock(activation_timestamp: u64) {
    const MAX_WAIT_SECS: u64 = 60;
    const POST_ACTIVATION_BUFFER_MS: u64 = 500;
    let deadline = now_secs().saturating_add(MAX_WAIT_SECS);
    loop {
        let current = now_secs();
        if current >= activation_timestamp {
            break;
        }
        if current >= deadline {
            panic!("Timed out waiting for wallclock to reach activation timestamp");
        }
        tokio::time::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
    // Buffer to ensure node's internal state has caught up with wall clock
    tokio::time::sleep(std::time::Duration::from_millis(POST_ACTIVATION_BUFFER_MS)).await;
}

#[cfg(test)]
mod single_version_acceptance {
    use super::*;

    #[rstest]
    #[case::pre_activation_v1_accepted(true, TxVersion::V1, true)]
    #[case::pre_activation_v2_accepted(true, TxVersion::V2, true)]
    #[case::post_activation_v1_rejected(false, TxVersion::V1, false)]
    #[case::post_activation_v2_accepted(false, TxVersion::V2, true)]
    #[test_log::test(tokio::test)]
    async fn heavy_test_aurora_tx_acceptance(
        #[case] pre_activation: bool,
        #[case] version: TxVersion,
        #[case] expect_accepted: bool,
    ) -> eyre::Result<()> {
        initialize_tracing();

        let mut config = if pre_activation {
            aurora_config_future()
        } else {
            aurora_config_past()
        };

        let signer = create_funded_signer(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        let tx = create_stake_tx(&node, &signer, version).await;
        let result = node.post_commitment_tx(&tx).await;

        if expect_accepted {
            assert!(
                result.is_ok(),
                "{:?} should be accepted (pre_activation={}): {:?}",
                version,
                pre_activation,
                result.err()
            );
        } else {
            assert!(
                result.is_err(),
                "{:?} should be rejected after Aurora",
                version
            );
            assert_http_bad_request(&result.unwrap_err());
        }

        node.stop().await;
        Ok(())
    }
}

#[cfg(test)]
mod mixed_versions {
    use super::*;

    #[test_log::test(tokio::test)]
    async fn heavy_test_pre_activation_mixed_versions_in_block() -> eyre::Result<()> {
        initialize_tracing();

        let mut config = aurora_config_future();
        let [signer1, signer2] = create_funded_signers(&mut config);

        let node = IrysNodeTest::new_genesis(config).start().await;

        let v1_tx = create_stake_tx(&node, &signer1, TxVersion::V1).await;
        let v2_tx = create_stake_tx(&node, &signer2, TxVersion::V2).await;

        node.post_commitment_tx(&v1_tx).await?;
        node.post_commitment_tx(&v2_tx).await?;

        node.mine_blocks(1).await?;

        let block = node.get_block_by_height(1).await?;
        let commitments = block.commitment_tx_ids();

        assert!(commitments.contains(&v1_tx.id()), "Block should contain V1");
        assert!(commitments.contains(&v2_tx.id()), "Block should contain V2");

        node.stop().await;
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn heavy_test_post_activation_only_v2_in_block() -> eyre::Result<()> {
        initialize_tracing();

        let mut config = aurora_config_past();
        let [signer1, signer2] = create_funded_signers(&mut config);

        let node = IrysNodeTest::new_genesis(config).start().await;

        let v1_tx = create_stake_tx(&node, &signer1, TxVersion::V1).await;
        let v2_tx = create_stake_tx(&node, &signer2, TxVersion::V2).await;

        let result_v1 = node.post_commitment_tx(&v1_tx).await;
        assert!(result_v1.is_err(), "V1 should be rejected");

        node.post_commitment_tx(&v2_tx).await?;
        node.mine_blocks(1).await?;

        let block = node.get_block_by_height(1).await?;
        let commitments = block.commitment_tx_ids();

        assert!(
            !commitments.contains(&v1_tx.id()),
            "V1 should not be in block"
        );
        assert!(commitments.contains(&v2_tx.id()), "V2 should be in block");

        node.stop().await;
        Ok(())
    }
}

#[cfg(test)]
mod boundary_crossing {
    use super::*;

    #[test_log::test(tokio::test)]
    async fn heavy_test_boundary_crossing_v1_behavior() -> eyre::Result<()> {
        initialize_tracing();

        let aurora_activation = now_secs().saturating_add(ACTIVATION_DELAY_SECS);
        let mut config = aurora_config_with_timestamp(aurora_activation);
        let [signer1, signer2] = create_funded_signers(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        let genesis_block = node.get_block_by_height(0).await?;
        assert!(genesis_block.timestamp_secs().as_secs() < aurora_activation);

        let v1_before = create_stake_tx(&node, &signer1, TxVersion::V1).await;
        let result_before = node.post_commitment_tx(&v1_before).await;
        assert!(result_before.is_ok(), "V1 should be accepted before Aurora");

        wait_until_activation(&node, aurora_activation).await;

        // Verify the pre-activation V1 tx was actually mined
        let found_height = find_tx_in_blocks(&node, &v1_before.id(), MAX_BLOCKS_TO_SEARCH).await;
        assert!(
            found_height.is_some(),
            "V1 submitted before activation should be mined in a pre-activation block"
        );

        let v1_after = create_stake_tx(&node, &signer2, TxVersion::V1).await;
        let result_after = node.post_commitment_tx(&v1_after).await;
        assert!(result_after.is_err(), "V1 should be rejected after Aurora");

        node.stop().await;
        Ok(())
    }
}

#[cfg(test)]
mod configuration {
    use super::*;

    #[test_log::test(tokio::test)]
    async fn heavy_test_aurora_disabled_accepts_all_versions() -> eyre::Result<()> {
        initialize_tracing();

        let mut config = create_test_config(None);
        let [signer1, signer2] = create_funded_signers(&mut config);

        let node = IrysNodeTest::new_genesis(config).start().await;

        let v1_tx = create_stake_tx(&node, &signer1, TxVersion::V1).await;
        let v2_tx = create_stake_tx(&node, &signer2, TxVersion::V2).await;

        let result_v1 = node.post_commitment_tx(&v1_tx).await;
        assert!(
            result_v1.is_ok(),
            "V1 should be accepted when Aurora disabled: {:?}",
            result_v1.err()
        );

        let result_v2 = node.post_commitment_tx(&v2_tx).await;
        assert!(
            result_v2.is_ok(),
            "V2 should be accepted when Aurora disabled: {:?}",
            result_v2.err()
        );

        node.mine_blocks(1).await?;

        let block = node.get_block_by_height(1).await?;
        let commitments = block.commitment_tx_ids();

        assert!(commitments.contains(&v1_tx.id()), "V1 should be in block");
        assert!(commitments.contains(&v2_tx.id()), "V2 should be in block");

        node.stop().await;
        Ok(())
    }
}

#[cfg(test)]
mod edge_cases {
    use super::*;

    /// V1 transactions in the mempool before Aurora activation must NOT be mined after
    /// activation. Block producers must filter out invalid versions based on block timestamp,
    /// and validators must reject blocks containing V1 transactions post-activation.
    /// This ensures consensus - all nodes agree on block validity.
    #[test_log::test(tokio::test)]
    async fn heavy_test_v1_in_mempool_before_activation_filtered_after() -> eyre::Result<()> {
        initialize_tracing();

        let aurora_activation = now_secs().saturating_add(ACTIVATION_DELAY_SECS);
        let mut config = aurora_config_with_timestamp(aurora_activation);
        let signer = create_funded_signer(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        let v1_tx = create_stake_tx(&node, &signer, TxVersion::V1).await;
        let result = node.post_commitment_tx(&v1_tx).await;
        assert!(
            result.is_ok(),
            "V1 should be accepted into mempool before activation: {:?}",
            result.err()
        );

        // Wait for wall clock to reach activation WITHOUT mining blocks
        // This ensures the V1 tx stays in mempool until after activation
        wait_for_wallclock(aurora_activation).await;

        // Now mine blocks - these are all post-activation blocks
        // The V1 tx should be filtered out by the block producer
        node.mine_blocks(3).await?;

        let found_height = find_tx_in_blocks(&node, &v1_tx.id(), MAX_BLOCKS_TO_SEARCH).await;
        assert!(
            found_height.is_none(),
            "V1 tx must NOT be mined after activation (consensus requirement)"
        );
        info!("V1 tx correctly filtered out of post-activation blocks");

        node.stop().await;
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn heavy_test_v2_accepted_at_exact_activation_boundary() -> eyre::Result<()> {
        initialize_tracing();

        let aurora_activation = now_secs().saturating_add(ACTIVATION_DELAY_SECS);
        let mut config = aurora_config_with_timestamp(aurora_activation);
        let signer = create_funded_signer(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        wait_for_wallclock(aurora_activation).await;

        let v2_tx = create_stake_tx(&node, &signer, TxVersion::V2).await;
        let result = node.post_commitment_tx(&v2_tx).await;
        assert!(
            result.is_ok(),
            "V2 should be accepted at/after activation boundary: {:?}",
            result.err()
        );

        node.mine_blocks(1).await?;

        let found_height = find_tx_in_blocks(&node, &v2_tx.id(), MAX_BLOCKS_TO_SEARCH).await;
        assert!(
            found_height.is_some(),
            "V2 tx should be mined after activation"
        );

        node.stop().await;
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn heavy_test_v1_rejected_at_exact_activation_boundary() -> eyre::Result<()> {
        initialize_tracing();

        let aurora_activation = now_secs().saturating_add(ACTIVATION_DELAY_SECS);
        let mut config = aurora_config_with_timestamp(aurora_activation);
        let signer = create_funded_signer(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        wait_for_wallclock(aurora_activation).await;

        let v1_tx = create_stake_tx(&node, &signer, TxVersion::V1).await;
        let result = node.post_commitment_tx(&v1_tx).await;
        assert!(
            result.is_err(),
            "V1 should be rejected at/after activation boundary"
        );

        assert_http_bad_request(&result.unwrap_err());

        node.stop().await;
        Ok(())
    }
}

#[cfg(test)]
mod epoch_block_filtering {
    use super::*;
    use irys_types::SystemLedger;

    const NUM_BLOCKS_IN_EPOCH: usize = 2;

    #[derive(Clone, Copy, Debug)]
    enum AuroraState {
        Disabled,
        PreActivation,
        PostActivation,
    }

    fn aurora_config_with_epoch(state: AuroraState) -> NodeConfig {
        let mut config = NodeConfig::testing_with_epochs(NUM_BLOCKS_IN_EPOCH);
        let aurora = match state {
            AuroraState::Disabled => None,
            AuroraState::PreActivation => Some(Aurora {
                activation_timestamp: UnixTimestamp::from_secs(
                    now_secs().saturating_add(ONE_HOUR_SECS),
                ),
                minimum_commitment_tx_version: AURORA_MIN_VERSION,
            }),
            AuroraState::PostActivation => Some(Aurora {
                activation_timestamp: UnixTimestamp::from_secs(
                    now_secs().saturating_sub(ONE_HOUR_SECS),
                ),
                minimum_commitment_tx_version: AURORA_MIN_VERSION,
            }),
        };
        config.consensus.get_mut().hardforks = IrysHardforkConfig {
            frontier: default_test_frontier(),
            next_name_tbd: None,
            sprite: None,
            aurora,
            borealis: None,
        };
        config
    }

    /// Test epoch block commitment filtering across different aurora states.
    /// - Disabled: both V1 and V2 included
    /// - PreActivation: both V1 and V2 included
    /// - PostActivation: only V2 included (V1 rejected at mempool)
    #[rstest]
    #[case::aurora_disabled_includes_all(AuroraState::Disabled, true, true)]
    #[case::pre_activation_includes_all(AuroraState::PreActivation, true, true)]
    #[case::post_activation_v2_only(AuroraState::PostActivation, false, true)]
    #[test_log::test(tokio::test)]
    async fn heavy_test_epoch_block_commitment_filtering(
        #[case] aurora_state: AuroraState,
        #[case] v1_in_epoch: bool,
        #[case] v2_in_epoch: bool,
    ) -> eyre::Result<()> {
        initialize_tracing();

        let mut config = aurora_config_with_epoch(aurora_state);
        let [signer1, signer2] = create_funded_signers(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        // Attempt to post V1 - may be rejected if post-activation
        let v1_tx = create_stake_tx(&node, &signer1, TxVersion::V1).await;
        let v1_result = node.post_commitment_tx(&v1_tx).await;

        // V2 should always be accepted
        let v2_tx = create_stake_tx(&node, &signer2, TxVersion::V2).await;
        node.post_commitment_tx(&v2_tx).await?;

        // Mine to epoch block
        node.mine_blocks(NUM_BLOCKS_IN_EPOCH).await?;

        let epoch_block = node.get_block_by_height(NUM_BLOCKS_IN_EPOCH as u64).await?;
        assert_eq!(
            epoch_block.height % NUM_BLOCKS_IN_EPOCH as u64,
            0,
            "Block should be an epoch block"
        );

        let commitment_tx_ids = epoch_block
            .system_ledgers
            .get(SystemLedger::Commitment as usize)
            .map(|l| &l.tx_ids.0)
            .cloned()
            .unwrap_or_default();

        // Verify V1 presence based on expectation
        if v1_in_epoch {
            assert!(
                v1_result.is_ok(),
                "{:?}: V1 should be accepted into mempool",
                aurora_state
            );
            assert!(
                commitment_tx_ids.contains(&v1_tx.id()),
                "{:?}: epoch block should contain V1 commitment",
                aurora_state
            );
        } else {
            assert!(
                v1_result.is_err(),
                "{:?}: V1 should be rejected by mempool",
                aurora_state
            );
            assert!(
                !commitment_tx_ids.contains(&v1_tx.id()),
                "{:?}: epoch block should NOT contain V1 commitment",
                aurora_state
            );
        }

        // Verify V2 presence
        if v2_in_epoch {
            assert!(
                commitment_tx_ids.contains(&v2_tx.id()),
                "{:?}: epoch block should contain V2 commitment",
                aurora_state
            );
        }

        node.stop().await;
        Ok(())
    }

    /// Epoch block at hardfork boundary: V1 commitments accepted pre-activation
    /// should NOT be filtered out when the epoch block falls post-activation.
    #[test_log::test(tokio::test)]
    async fn heavy_test_epoch_at_hardfork_boundary_doesnt_filter_v1() -> eyre::Result<()> {
        initialize_tracing();

        // Hardfork activates slightly in the future
        let aurora_activation = now_secs().saturating_add(ACTIVATION_DELAY_SECS);
        let mut config = NodeConfig::testing_with_epochs(NUM_BLOCKS_IN_EPOCH);
        config.consensus.get_mut().hardforks = IrysHardforkConfig {
            frontier: default_test_frontier(),
            next_name_tbd: None,
            sprite: None,
            aurora: Some(Aurora {
                activation_timestamp: UnixTimestamp::from_secs(aurora_activation),
                minimum_commitment_tx_version: AURORA_MIN_VERSION,
            }),
            borealis: None,
        };
        let [signer1, signer2] = create_funded_signers(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        // Before activation: submit both V1 and V2
        let v1_tx = create_stake_tx(&node, &signer1, TxVersion::V1).await;
        let v2_tx = create_stake_tx(&node, &signer2, TxVersion::V2).await;

        node.post_commitment_tx(&v1_tx).await?;
        node.post_commitment_tx(&v2_tx).await?;

        // Mine first block (pre-activation) - should include both
        let block1 = node.mine_block().await?;
        let block1_timestamp = block1.timestamp_secs().as_secs();

        // Wait for activation
        wait_for_wallclock(aurora_activation).await;

        // Mine epoch block (post-activation)
        let epoch_block = node.mine_block().await?;
        assert_eq!(
            epoch_block.height % NUM_BLOCKS_IN_EPOCH as u64,
            0,
            "Block should be an epoch block"
        );
        let epoch_timestamp = epoch_block.timestamp_secs().as_secs();
        assert!(
            epoch_timestamp >= aurora_activation,
            "Epoch block timestamp should be at or after activation"
        );

        let commitment_tx_ids = epoch_block
            .system_ledgers
            .get(SystemLedger::Commitment as usize)
            .map(|l| &l.tx_ids.0)
            .cloned()
            .unwrap_or_default();

        // If block1 was pre-activation, it may contain both V1 and V2.
        // But epoch block (post-activation) should contain both V1 and V2
        if block1_timestamp < aurora_activation {
            // The epoch rollup should NOT filter out V1 even if the epoch block is post-activation
            assert!(
                commitment_tx_ids.contains(&v1_tx.id()),
                "Epoch block (post-activation) include V1 from epoch commitments"
            );
        }

        assert!(
            commitment_tx_ids.contains(&v2_tx.id()),
            "Epoch block should contain V2 commitment"
        );

        node.stop().await;
        Ok(())
    }
}

#[cfg(test)]
mod borealis_hardfork {
    use super::*;
    use irys_types::{hardfork_config::Borealis, IrysAddress};

    fn create_borealis_config(borealis: Option<Borealis>, aurora: Option<Aurora>) -> NodeConfig {
        let mut config = NodeConfig::testing();
        config.consensus.get_mut().hardforks = IrysHardforkConfig {
            frontier: default_test_frontier(),
            next_name_tbd: None,
            sprite: None,
            aurora,
            borealis,
        };
        config
    }

    fn borealis_config_with_timestamp(activation_secs: u64) -> NodeConfig {
        // Aurora required for V2 transactions
        let aurora = Some(Aurora {
            activation_timestamp: UnixTimestamp::from_secs(0),
            minimum_commitment_tx_version: AURORA_MIN_VERSION,
        });
        create_borealis_config(
            Some(Borealis {
                activation_timestamp: UnixTimestamp::from_secs(activation_secs),
            }),
            aurora,
        )
    }

    fn borealis_config_future() -> NodeConfig {
        borealis_config_with_timestamp(now_secs().saturating_add(ONE_HOUR_SECS))
    }

    fn borealis_config_past() -> NodeConfig {
        borealis_config_with_timestamp(now_secs().saturating_sub(ONE_HOUR_SECS))
    }

    /// Test that UpdateRewardAddress is rejected before Borealis activation.
    #[test_log::test(tokio::test)]
    async fn heavy_test_borealis_rejects_update_reward_address_pre_activation() -> eyre::Result<()>
    {
        let mut config = borealis_config_future();
        let signer = create_funded_signer(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        node.post_stake_commitment_with_signer(&signer).await?;
        node.mine_block().await?;

        let new_address = IrysAddress::random();
        let result = node.post_update_reward_address(&signer, new_address).await;

        assert!(result.is_err());
        assert_http_bad_request(&result.unwrap_err());

        node.stop().await;
        Ok(())
    }

    /// Test that UpdateRewardAddress is accepted and mined after Borealis activation.
    #[test_log::test(tokio::test)]
    async fn heavy_test_borealis_accepts_update_reward_address_post_activation() -> eyre::Result<()>
    {
        let mut config = borealis_config_past();
        let signer = create_funded_signer(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        node.post_stake_commitment_with_signer(&signer).await?;
        node.mine_block().await?;

        let new_address = IrysAddress::random();
        let tx = node
            .post_update_reward_address(&signer, new_address)
            .await?;

        node.mine_blocks(2).await?;
        let found = find_tx_in_blocks(&node, &tx.id(), MAX_BLOCKS_TO_SEARCH).await;
        assert!(found.is_some(), "UpdateRewardAddress should be mined");

        node.stop().await;
        Ok(())
    }

    /// Test that Borealis activation is epoch-aligned: even after wall clock passes
    /// the activation timestamp, the hardfork only enables once an epoch block is
    /// created with timestamp >= activation.
    #[test_log::test(tokio::test)]
    async fn heavy_test_borealis_epoch_boundary_activation() -> eyre::Result<()> {
        let borealis_activation = now_secs().saturating_add(ACTIVATION_DELAY_SECS);
        let mut config = NodeConfig::testing_with_epochs(2);
        // Set up Aurora (already active) and Borealis (activating soon)
        config.consensus.get_mut().hardforks = IrysHardforkConfig {
            frontier: default_test_frontier(),
            next_name_tbd: None,
            sprite: None,
            aurora: Some(Aurora {
                activation_timestamp: UnixTimestamp::from_secs(0),
                minimum_commitment_tx_version: AURORA_MIN_VERSION,
            }),
            borealis: Some(Borealis {
                activation_timestamp: UnixTimestamp::from_secs(borealis_activation),
            }),
        };

        let signer = create_funded_signer(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        // Stake first (block 1, still in epoch 0 with genesis as epoch block)
        node.post_stake_commitment_with_signer(&signer).await?;
        node.mine_block().await?;

        // Before activation timestamp: UpdateRewardAddress should be rejected
        let new_reward_address = IrysAddress::random();
        let result_pre = node
            .post_update_reward_address(&signer, new_reward_address)
            .await;
        assert!(
            result_pre.is_err(),
            "UpdateRewardAddress should be rejected before activation timestamp"
        );

        // Wait for wall clock to pass activation timestamp
        wait_for_wallclock(borealis_activation).await;

        // KEY TEST: Wall clock has passed activation, but we haven't mined a new epoch block yet.
        // The current epoch block (genesis) has timestamp < activation, so hardfork should
        // still be disabled even though wall clock has passed activation.
        let result_after_wallclock = node
            .post_update_reward_address(&signer, new_reward_address)
            .await;
        assert!(
            result_after_wallclock.is_err(),
            "UpdateRewardAddress should still be rejected after wall clock passes activation \
             but before epoch block with timestamp >= activation is mined"
        );

        // Mine to next epoch boundary - this creates a new epoch block with timestamp >= activation
        node.mine_until_next_epoch().await?;

        // Now hardfork should be active because the new epoch block has timestamp >= activation
        let result_post = node
            .post_update_reward_address(&signer, new_reward_address)
            .await;
        assert!(
            result_post.is_ok(),
            "UpdateRewardAddress should be accepted after epoch block with timestamp >= activation: {:?}",
            result_post.err()
        );

        node.stop().await;
        Ok(())
    }

    /// Test that Borealis disabled (None) rejects UpdateRewardAddress.
    #[test_log::test(tokio::test)]
    async fn heavy_test_borealis_disabled_rejects_update_reward_address() -> eyre::Result<()> {
        // Aurora active (V2 required), but Borealis disabled
        let mut config = create_borealis_config(
            None,
            Some(Aurora {
                activation_timestamp: UnixTimestamp::from_secs(0),
                minimum_commitment_tx_version: AURORA_MIN_VERSION,
            }),
        );

        let signer = create_funded_signer(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        node.post_stake_commitment_with_signer(&signer).await?;
        node.mine_block().await?;

        let new_reward_address = IrysAddress::random();
        let result = node
            .post_update_reward_address(&signer, new_reward_address)
            .await;
        assert!(
            result.is_err(),
            "UpdateRewardAddress should be rejected when Borealis is disabled"
        );
        assert_http_bad_request(&result.unwrap_err());

        node.stop().await;
        Ok(())
    }
}

#[cfg(test)]
mod peer_sync_recovery {
    use std::time::Duration;

    use super::*;
    use irys_database::db::IrysDatabaseExt as _;

    const NUM_BLOCKS_IN_EPOCH: usize = 3;
    const NUM_EPOCHS_BEFORE_ACTIVATION: usize = 3;
    const NUM_EPOCHS_AFTER_ACTIVATION: usize = 3; // 3 epochs after Aurora
    const BLOCK_MIGRATION_DEPTH: usize = 2;
    const SECONDS_TO_WAIT: usize = 30;

    /// Tests that a peer node can sync through an Aurora hardfork activation boundary.
    /// Both genesis and peer start together, peer stakes to get partition assignments,
    /// then blocks are mined with V1 commitments before activation and V2 after.
    /// After restart with Aurora enabled, verifies peer syncs correctly through the boundary.
    #[test_log::test(tokio::test)]
    async fn heavy_test_aurora_hardfork_recovery_peer_sync() -> eyre::Result<()> {
        initialize_tracing();

        // Step 1: Setup Configuration (Aurora disabled initially)
        let block_migration_depth = BLOCK_MIGRATION_DEPTH;
        let mut genesis_config = NodeConfig::testing_with_epochs(NUM_BLOCKS_IN_EPOCH);
        genesis_config.consensus.get_mut().chunk_size = 32;
        genesis_config.consensus.get_mut().block_migration_depth =
            block_migration_depth.try_into()?;
        genesis_config.consensus.get_mut().hardforks = IrysHardforkConfig {
            frontier: default_test_frontier(),
            aurora: None, // Disabled initially
            next_name_tbd: None,
            sprite: None,
            borealis: None,
        };

        // Fund signers: 9 for V1 (3 epochs * 3 blocks), 9 for V2 (3 epochs * 3 blocks), 1 for peer
        let signers: [IrysSigner; 19] = create_funded_signers(&mut genesis_config);
        let peer_signer = &signers[18];

        // Step 2: Start Genesis and Peer
        let genesis_node = IrysNodeTest::new_genesis(genesis_config.clone())
            .start_and_wait_for_packing("GENESIS", SECONDS_TO_WAIT)
            .await;

        let peer_config = genesis_node.testing_peer_with_signer(peer_signer);
        let peer_node = IrysNodeTest::new(peer_config).start_with_name("PEER").await;

        // Peer stakes and pledges to participate in the network
        let peer_stake = peer_node.post_stake_commitment(None).await?;
        let peer_pledge = peer_node.post_pledge_commitment(None).await?;

        // Wait for commitments to reach genesis mempool
        genesis_node
            .wait_for_mempool(peer_stake.id(), SECONDS_TO_WAIT)
            .await?;
        genesis_node
            .wait_for_mempool(peer_pledge.id(), SECONDS_TO_WAIT)
            .await?;

        // Mine first epoch to get peer's partition assignments
        genesis_node.mine_blocks(NUM_BLOCKS_IN_EPOCH).await?;
        peer_node
            .wait_until_height(NUM_BLOCKS_IN_EPOCH as u64, SECONDS_TO_WAIT)
            .await?;

        // Wait for peer to pack its storage module with partition data
        peer_node.wait_for_packing(SECONDS_TO_WAIT).await;
        let mut stopped_peer = peer_node.stop().await;
        info!("Peer has partition assignments and is ready");

        // Step 3: Mine epochs with V1 Commitments + Data Txs
        let mut signer_idx = 0;
        for epoch in 0..NUM_EPOCHS_BEFORE_ACTIVATION {
            for block_in_epoch in 0..NUM_BLOCKS_IN_EPOCH {
                let anchor = genesis_node.get_anchor().await?;
                let signer = &signers[signer_idx];

                // Post V1 commitment
                let v1_tx = create_stake_tx(&genesis_node, signer, TxVersion::V1).await;
                genesis_node.post_commitment_tx(&v1_tx).await?;

                // Post data tx
                let data = format!("pre-aurora-epoch{}-block{}", epoch, block_in_epoch);
                genesis_node
                    .post_data_tx(anchor, data.into_bytes(), signer)
                    .await;

                // Mine block
                genesis_node.mine_block().await?;
                signer_idx += 1;
            }
        }

        let pre_activation_height = genesis_node.get_canonical_chain_height().await;
        info!(
            "Mined {} pre-activation blocks (height: {}), block_migration_depth={}",
            NUM_EPOCHS_BEFORE_ACTIVATION * NUM_BLOCKS_IN_EPOCH,
            pre_activation_height,
            block_migration_depth
        );

        // Step 3.5: Wait for Pre-Aurora Block Migration
        // Calculate expected migrated height (current - migration_depth)
        let pre_aurora_migrated_height = pre_activation_height - block_migration_depth as u64;
        info!(
            "Waiting for block index to reach height {} (chain={} - depth={})",
            pre_aurora_migrated_height, pre_activation_height, block_migration_depth
        );
        genesis_node
            .wait_for_block_in_index_height(pre_aurora_migrated_height, SECONDS_TO_WAIT)
            .await?;
        info!(
            "Pre-Aurora blocks migrated to height {}",
            pre_aurora_migrated_height
        );

        // Validate pre-aurora blocks are in the index with PoA chunks and their txs are in DB
        let mut pre_aurora_tx_count = 0;
        for height in 1..=pre_aurora_migrated_height {
            let block = genesis_node.get_block_by_height_from_index(height, true)?;
            assert!(
                block.poa.chunk.is_some(),
                "Pre-aurora migrated block at height {} should have PoA chunk",
                height
            );
            // Verify all data txs in this block are in the database
            for tx_ids in block.get_data_ledger_tx_ids().values() {
                for tx_id in tx_ids {
                    let tx_header = genesis_node.get_tx_header(tx_id)?;
                    assert_eq!(
                        &tx_header.id, tx_id,
                        "Data tx {} from block {} should be in DB",
                        tx_id, height
                    );
                    pre_aurora_tx_count += 1;
                }
            }
        }
        info!(
            "Validated {} pre-Aurora blocks with {} data txs in index",
            pre_aurora_migrated_height, pre_aurora_tx_count
        );

        // Step 4: Stop Both Nodes, Activate Aurora, Restart
        let mut stopped_genesis = genesis_node.stop().await;

        tokio::time::sleep(Duration::from_secs(1)).await;

        // Set activation in the past so all new blocks are post-activation
        let activation_timestamp = now_secs();

        // Modify genesis config to enable Aurora
        stopped_genesis.cfg.consensus.get_mut().hardforks.aurora = Some(Aurora {
            activation_timestamp: UnixTimestamp::from_secs(activation_timestamp),
            minimum_commitment_tx_version: AURORA_MIN_VERSION,
        });

        // Modify peer config to enable Aurora
        stopped_peer.cfg.consensus.get_mut().hardforks.aurora = Some(Aurora {
            activation_timestamp: UnixTimestamp::from_secs(activation_timestamp),
            minimum_commitment_tx_version: AURORA_MIN_VERSION,
        });

        // Restart genesis with Aurora enabled
        let genesis_node = stopped_genesis.start().await;

        info!("Genesis node restarted with Aurora enabled");

        // Step 5: Mine V2 Commitments + Data Txs after Aurora
        // Continue signer index from pre-Aurora
        let mut v2_signer_idx = NUM_EPOCHS_BEFORE_ACTIVATION * NUM_BLOCKS_IN_EPOCH;
        for epoch in 0..NUM_EPOCHS_AFTER_ACTIVATION {
            for block_in_epoch in 0..NUM_BLOCKS_IN_EPOCH {
                let anchor = genesis_node.get_anchor().await?;
                let signer = &signers[v2_signer_idx];

                // Post V2 commitment (V1 would be rejected now)
                let v2_tx = create_stake_tx(&genesis_node, signer, TxVersion::V2).await;
                genesis_node.post_commitment_tx(&v2_tx).await?;

                // Verify V1 is rejected after activation (only check once per epoch)
                if block_in_epoch == 0 {
                    let v1_tx = create_stake_tx(&genesis_node, signer, TxVersion::V1).await;
                    assert!(
                        genesis_node.post_commitment_tx(&v1_tx).await.is_err(),
                        "V1 should be rejected after Aurora activation"
                    );
                }

                // Post data tx
                let data = format!("post-aurora-epoch{}-block{}", epoch, block_in_epoch);
                genesis_node
                    .post_data_tx(anchor, data.into_bytes(), signer)
                    .await;

                genesis_node.mine_block().await?;
                v2_signer_idx += 1;
            }
        }

        // Mine one additional block to ensure post-Aurora blocks are pushed past migration depth
        genesis_node.mine_block().await?;

        let final_height = genesis_node.get_canonical_chain_height().await;
        info!(
            "Mined {} post-activation blocks + 1 extra (final height: {}), pre_activation_height was {}",
            NUM_EPOCHS_AFTER_ACTIVATION * NUM_BLOCKS_IN_EPOCH,
            final_height,
            pre_activation_height
        );

        // Step 5.5: Wait for Post-Aurora Block Migration
        let post_aurora_migrated_height = final_height - block_migration_depth as u64;
        info!(
            "Waiting for post-Aurora block index to reach height {} (final={} - depth={})",
            post_aurora_migrated_height, final_height, block_migration_depth
        );
        genesis_node
            .wait_for_block_in_index_height(post_aurora_migrated_height, SECONDS_TO_WAIT)
            .await?;
        info!(
            "Post-Aurora blocks migrated to height {}",
            post_aurora_migrated_height
        );

        // Validate all blocks (including post-aurora) are in the index with PoA chunks and their txs are in DB
        let mut total_tx_count = 0;
        for height in 1..=post_aurora_migrated_height {
            let block = genesis_node.get_block_by_height_from_index(height, true)?;
            assert!(
                block.poa.chunk.is_some(),
                "Migrated block at height {} should have PoA chunk",
                height
            );
            // Verify all data txs in this block are in the database
            let mut block_tx_count = 0;
            for tx_ids in block.get_data_ledger_tx_ids().values() {
                for tx_id in tx_ids {
                    let tx_header = genesis_node.get_tx_header(tx_id)?;
                    assert_eq!(
                        &tx_header.id, tx_id,
                        "Data tx {} from block {} should be in DB",
                        tx_id, height
                    );
                    block_tx_count += 1;
                }
            }
            total_tx_count += block_tx_count;
        }
        info!(
            "Validated {} blocks with {} total data txs in genesis block index",
            post_aurora_migrated_height, total_tx_count
        );

        // Verify that we have at least one post-Aurora block migrated
        // Pre-activation height was recorded before restart, post-Aurora blocks start after that
        assert!(
            post_aurora_migrated_height > pre_aurora_migrated_height,
            "Should have at least one post-Aurora block migrated: pre={}, post={}",
            pre_aurora_migrated_height,
            post_aurora_migrated_height
        );
        info!(
            "Post-Aurora blocks validated: {} blocks migrated after pre-Aurora height {}",
            post_aurora_migrated_height - pre_aurora_migrated_height,
            pre_aurora_migrated_height
        );

        // Step 6: Wait for Peer to Sync and Verify
        let peer_node = stopped_peer.start().await;
        peer_node
            .wait_until_height(final_height, SECONDS_TO_WAIT * 2)
            .await?;
        info!("Peer synced to height {}", final_height);

        // Verify chain matches at all heights
        for height in 1..=final_height {
            let genesis_block = genesis_node.get_block_by_height(height).await?;
            let peer_block = peer_node.get_block_by_height(height).await?;
            assert_eq!(
                genesis_block.block_hash, peer_block.block_hash,
                "Block hash mismatch at height {}",
                height
            );
        }
        info!(
            "Verified all {} blocks match between genesis and peer",
            final_height
        );

        // Step 7: Wait for Peer Block Migration and Validate
        peer_node
            .wait_for_block_in_index_height(post_aurora_migrated_height, SECONDS_TO_WAIT * 2)
            .await?;
        info!(
            "Peer block index migrated to height {}",
            post_aurora_migrated_height
        );

        // Verify peer's block index matches genesis AND transactions are in DB (not mempool)
        let mut peer_data_tx_count = 0;
        let mut peer_commitment_tx_count = 0;
        for height in 1..=post_aurora_migrated_height {
            let genesis_block = genesis_node.get_block_by_height_from_index(height, true)?;
            let peer_block = peer_node.get_block_by_height_from_index(height, true)?;
            assert_eq!(
                genesis_block.block_hash, peer_block.block_hash,
                "Block index hash mismatch at height {}",
                height
            );
            assert!(
                peer_block.poa.chunk.is_some(),
                "Peer migrated block at height {} should have PoA chunk",
                height
            );
            // Verify all data txs in this block are in the peer's database (direct DB read, not mempool)
            for tx_ids in peer_block.get_data_ledger_tx_ids().values() {
                for tx_id in tx_ids {
                    let tx_header = peer_node
                        .node_ctx
                        .db
                        .view_eyre(|tx| irys_database::tx_header_by_txid(tx, tx_id))?
                        .ok_or_else(|| {
                            eyre::eyre!(
                                "Data tx {} from block {} not found in peer's DB",
                                tx_id,
                                height
                            )
                        })?;
                    assert_eq!(
                        &tx_header.id, tx_id,
                        "Data tx {} from block {} should be in peer's DB",
                        tx_id, height
                    );
                    peer_data_tx_count += 1;
                }
            }
            // Verify all commitment txs in this block are in the peer's database
            for &tx_id in peer_block.commitment_tx_ids() {
                let commitment_tx = peer_node
                    .node_ctx
                    .db
                    .view_eyre(|tx| irys_database::commitment_tx_by_txid(tx, &tx_id))?
                    .ok_or_else(|| {
                        eyre::eyre!(
                            "Commitment tx {} from block {} not found in peer's DB",
                            tx_id,
                            height
                        )
                    })?;
                assert_eq!(
                    commitment_tx.id(),
                    tx_id,
                    "Commitment tx {} from block {} should be in peer's DB",
                    tx_id,
                    height
                );
                peer_commitment_tx_count += 1;
            }
        }
        info!(
            "Peer block index validated for {} migrated blocks with {} data txs and {} commitment txs",
            post_aurora_migrated_height, peer_data_tx_count, peer_commitment_tx_count
        );

        // Step 8: Additional Peer Restart After Sync
        let stopped_peer = peer_node.stop().await;
        info!("Peer stopped for additional restart test");

        let peer_node = stopped_peer.start().await;
        info!("Peer restarted after sync");

        // Wait for peer to reach the same height after restart
        peer_node
            .wait_until_height(final_height, SECONDS_TO_WAIT * 2)
            .await?;
        info!("Peer synced to height {} after restart", final_height);

        peer_node.stop().await;
        genesis_node.stop().await;
        Ok(())
    }
}
