//! Integration tests for Aurora hardfork behavior.

use crate::utils::IrysNodeTest;
use irys_chain::IrysNodeCtx;
use irys_testing_utils::initialize_tracing;
use irys_types::{
    hardfork_config::{Aurora, FrontierParams, IrysHardforkConfig},
    irys::IrysSigner,
    CommitmentTransaction, CommitmentTransactionV1, CommitmentTransactionV2, CommitmentType,
    ConsensusConfig, IrysTransactionId, NodeConfig, UnixTimestamp,
};
use rstest::rstest;
use tracing::info;

const ONE_HOUR_SECS: u64 = 3600;
const BOUNDARY_TEST_DELAY_SECS: u64 = 5;
const EDGE_CASE_DELAY_SECS: u64 = 3;
const AURORA_MIN_VERSION: u8 = 2;
const MAX_ACTIVATION_BLOCKS: u32 = 1000;
const MAX_BLOCKS_TO_SEARCH: u64 = 5;

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
        TxVersion::V1 => CommitmentTransaction::V1(CommitmentTransactionV1 {
            commitment_type: CommitmentType::Stake,
            anchor,
            fee,
            value,
            ..CommitmentTransactionV1::new(consensus)
        }),
        TxVersion::V2 => CommitmentTransaction::V2(CommitmentTransactionV2 {
            commitment_type: CommitmentType::Stake,
            anchor,
            fee,
            value,
            ..CommitmentTransactionV2::new(consensus)
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
        aurora,
    };
    config
}

fn aurora_config_with_offset(offset_secs: i64) -> NodeConfig {
    let timestamp = if offset_secs >= 0 {
        now_secs().saturating_add(offset_secs as u64)
    } else {
        now_secs().saturating_sub(offset_secs.unsigned_abs())
    };
    create_test_config(Some(Aurora {
        activation_timestamp: UnixTimestamp::from_secs(timestamp),
        minimum_commitment_tx_version: AURORA_MIN_VERSION,
    }))
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
    for height in 1..=max_height {
        if let Ok(block) = node.get_block_by_height(height).await {
            if block.get_commitment_ledger_tx_ids().contains(tx_id) {
                return Some(height);
            }
        } else {
            tracing::debug!(
                "Block at height {} not yet available, stopping search",
                height
            );
            break;
        }
    }
    None
}

async fn wait_until_activation(node: &IrysNodeTest<IrysNodeCtx>, activation_timestamp: u64) {
    info!("Waiting for Aurora activation...");
    for _ in 0..MAX_ACTIVATION_BLOCKS {
        let block = node.mine_block().await.expect("mining should succeed");
        if block.timestamp_secs().as_secs() >= activation_timestamp {
            info!("Aurora activated at block {}", block.height);
            return;
        }
    }
    panic!(
        "Failed to reach activation timestamp after {} blocks",
        MAX_ACTIVATION_BLOCKS
    );
}

async fn wait_for_wallclock(activation_timestamp: u64) {
    const MAX_WAIT_SECS: u64 = 60;
    let deadline = now_secs().saturating_add(MAX_WAIT_SECS);
    loop {
        if now_secs() >= activation_timestamp {
            break;
        }
        if now_secs() >= deadline {
            panic!("Timed out waiting for wallclock to reach activation timestamp");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
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
            let err_msg = result.unwrap_err().to_string();
            assert!(
                err_msg.contains("400"),
                "Should return HTTP 400, got: {}",
                err_msg
            );
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
        let commitments = block.get_commitment_ledger_tx_ids();

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
        let commitments = block.get_commitment_ledger_tx_ids();

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

        let aurora_activation = now_secs().saturating_add(BOUNDARY_TEST_DELAY_SECS);
        let mut config = aurora_config_with_offset(BOUNDARY_TEST_DELAY_SECS as i64);
        let [signer1, signer2] = create_funded_signers(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        let genesis_block = node.get_block_by_height(0).await?;
        assert!(genesis_block.timestamp_secs().as_secs() < aurora_activation);

        let v1_before = create_stake_tx(&node, &signer1, TxVersion::V1).await;
        let result_before = node.post_commitment_tx(&v1_before).await;
        assert!(result_before.is_ok(), "V1 should be accepted before Aurora");

        wait_until_activation(&node, aurora_activation).await;

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
        let commitments = block.get_commitment_ledger_tx_ids();

        assert!(commitments.contains(&v1_tx.id()), "V1 should be in block");
        assert!(commitments.contains(&v2_tx.id()), "V2 should be in block");

        node.stop().await;
        Ok(())
    }
}

#[cfg(test)]
mod edge_cases {
    use super::*;

    #[test_log::test(tokio::test)]
    async fn heavy_test_v1_in_mempool_before_activation_mined_after() -> eyre::Result<()> {
        initialize_tracing();

        let aurora_activation = now_secs().saturating_add(EDGE_CASE_DELAY_SECS);
        let mut config = aurora_config_with_offset(EDGE_CASE_DELAY_SECS as i64);
        let signer = create_funded_signer(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        let v1_tx = create_stake_tx(&node, &signer, TxVersion::V1).await;
        let result = node.post_commitment_tx(&v1_tx).await;
        assert!(
            result.is_ok(),
            "V1 should be accepted into mempool before activation: {:?}",
            result.err()
        );

        wait_until_activation(&node, aurora_activation).await;

        let found_height = find_tx_in_blocks(&node, &v1_tx.id(), MAX_BLOCKS_TO_SEARCH).await;
        assert!(
            found_height.is_some(),
            "V1 tx should be mined (was in mempool before activation)"
        );
        if let Some(h) = found_height {
            info!("V1 tx found in block {}", h);
        }

        node.stop().await;
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn heavy_test_v2_accepted_at_exact_activation_boundary() -> eyre::Result<()> {
        initialize_tracing();

        let aurora_activation = now_secs().saturating_add(EDGE_CASE_DELAY_SECS);
        let mut config = aurora_config_with_offset(EDGE_CASE_DELAY_SECS as i64);
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

        let aurora_activation = now_secs().saturating_add(EDGE_CASE_DELAY_SECS);
        let mut config = aurora_config_with_offset(EDGE_CASE_DELAY_SECS as i64);
        let signer = create_funded_signer(&mut config);
        let node = IrysNodeTest::new_genesis(config).start().await;

        wait_for_wallclock(aurora_activation).await;

        let v1_tx = create_stake_tx(&node, &signer, TxVersion::V1).await;
        let result = node.post_commitment_tx(&v1_tx).await;
        assert!(
            result.is_err(),
            "V1 should be rejected at/after activation boundary"
        );

        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("400"),
            "Should return HTTP 400, got: {}",
            err_msg
        );

        node.stop().await;
        Ok(())
    }
}
