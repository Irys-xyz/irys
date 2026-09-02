//! Term-ledger replica data sync must reach the migrated write frontier.
//!
//! The Publish/Submit replica-sync test only asserts `data <= N`, so a replica
//! that stopped one chunk short of the frontier still passed. This test posts
//! an exact `N`-chunk OneYear transaction, migrates it, and requires every
//! miner assigned to that slot — the seeder who received the upload *and* the
//! replica that has to fetch — to store offsets `{0, …, N-1}` as Data.

use std::collections::BTreeSet;
use std::time::Duration;

use irys_chain::IrysNodeCtx;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_domain::ChunkType;
use irys_testing_utils::initialize_tracing;
use irys_types::{
    BoundedFee, DataLedger, IrysAddress, LedgerChunkOffset, NodeConfig, UnixTimestamp,
    hardfork_config::Cascade, irys::IrysSigner,
};
use tracing::info;

use crate::utils::IrysNodeTest;

const CHUNK_SIZE: usize = 32;
const NUM_CHUNKS_IN_PARTITION: u64 = 10;
const NUM_DATA_CHUNKS: usize = 5;
const BLOCKS_PER_EPOCH: u64 = 4;
const SECONDS_TO_WAIT: usize = 20;

#[test_log::test(tokio::test)]
async fn slow_heavy_term_ledger_replica_syncs_migrated_frontier() -> eyre::Result<()> {
    initialize_tracing();

    let mut config = NodeConfig::testing()
        .with_consensus(|c| {
            c.chunk_size = CHUNK_SIZE as u64;
            c.num_chunks_in_partition = NUM_CHUNKS_IN_PARTITION;
            c.num_partitions_per_slot = 2;
            c.num_partitions_per_term_ledger_slot = 2;
            c.epoch.num_blocks_in_epoch = BLOCKS_PER_EPOCH;
            c.block_migration_depth = 1;
            c.epoch.submit_ledger_epoch_length = 1000;
            c.hardforks.cascade = Some(Cascade {
                activation_timestamp: UnixTimestamp::from_secs(0),
                one_year_epoch_length: 1000,
                thirty_day_epoch_length: 1000,
                annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
            });
        })
        .with_genesis_peer_discovery_timeout(1000);

    let peer_signer = IrysSigner::random_signer(&config.consensus_config());
    config.fund_genesis_accounts(vec![&peer_signer]);

    let genesis_test = IrysNodeTest::new_genesis(config.clone());
    // Cascade-from-genesis: 4 ledgers × 2 replicas. Genesis can occupy only one
    // replica per slot; extra submodules become capacity pledges.
    StorageSubmodulesConfig::load_for_test(genesis_test.cfg.base_directory.clone(), 8)?;
    let genesis_node = genesis_test
        .start_and_wait_for_packing("GENESIS", SECONDS_TO_WAIT)
        .await;
    let genesis_address = genesis_node.node_ctx.config.node_config.miner_address();
    let peer_address = peer_signer.address();

    // Peer commitments are posted to genesis so the next epoch can fill the
    // empty second replica of each slot. Stake must land on-chain before
    // pledges; four pledges match the four empty replicas
    // (Publish/Submit/OneYear/ThirtyDay).
    let stake = genesis_node
        .post_stake_commitment_with_signer(&peer_signer)
        .await?;
    genesis_node
        .wait_for_mempool(stake.id(), SECONDS_TO_WAIT)
        .await?;
    genesis_node.mine_block().await?;
    let mut pledge_ids = Vec::new();
    for _ in 0..4 {
        pledge_ids.push(
            genesis_node
                .post_pledge_commitment_with_signer(&peer_signer)
                .await
                .id(),
        );
    }
    genesis_node
        .wait_for_mempool_commitment_txs(pledge_ids, SECONDS_TO_WAIT)
        .await?;

    let mut one_year_owners = Vec::new();
    for _ in 0..3 {
        genesis_node.mine_until_next_epoch().await?;
        one_year_owners = slot_owners(&genesis_node, DataLedger::OneYear, 0);
        if one_year_owners.len() == 2 {
            break;
        }
    }
    assert_eq!(
        one_year_owners.len(),
        2,
        "OneYear slot 0 must have both replicas (owners={one_year_owners:?})"
    );
    assert!(
        one_year_owners.contains(&genesis_address) && one_year_owners.contains(&peer_address),
        "OneYear slot 0 replicas must be genesis and peer (owners={one_year_owners:?})"
    );

    let peer_config = genesis_node.testing_peer_with_signer(&peer_signer);
    let peer_test = IrysNodeTest::new(peer_config);
    StorageSubmodulesConfig::load_for_test(peer_test.cfg.base_directory.clone(), 8)?;
    let peer_node = peer_test
        .start_and_wait_for_packing("PEER", SECONDS_TO_WAIT)
        .await;
    peer_node
        .wait_until_height(
            genesis_node.get_canonical_chain_height().await,
            SECONDS_TO_WAIT,
        )
        .await?;

    let mut chunks = Vec::with_capacity(NUM_DATA_CHUNKS);
    for i in 0..NUM_DATA_CHUNKS {
        chunks.push([i as u8; 32]);
    }
    let data: Vec<u8> = chunks.concat();
    let data_size = data.len() as u64;
    let genesis_signer = genesis_node.node_ctx.config.irys_signer();
    let price = genesis_node
        .get_data_price(DataLedger::OneYear, data_size)
        .await?;
    let tx = genesis_signer.create_transaction_with_fees(
        data,
        genesis_node.get_anchor().await?,
        DataLedger::OneYear,
        BoundedFee::new(price.term_fee),
        None,
    )?;
    let tx = genesis_signer.sign_transaction(tx)?;
    genesis_node.ingest_data_tx(tx.header.clone()).await?;
    genesis_node
        .wait_for_mempool(tx.header.id, SECONDS_TO_WAIT)
        .await?;

    for i in 0..NUM_DATA_CHUNKS {
        genesis_node.post_chunk_32b(&tx, i, &chunks).await;
    }

    let inclusion = genesis_node.mine_block().await?;
    assert_eq!(
        inclusion.ledger_total_chunks(DataLedger::OneYear),
        NUM_DATA_CHUNKS as u64,
        "inclusion block must report the exclusive OneYear frontier"
    );
    assert!(
        inclusion
            .get_data_ledger_tx_ids()
            .get(&DataLedger::OneYear)
            .is_some_and(|ids| ids.contains(&tx.header.id)),
        "OneYear tx must be included in the mined block"
    );
    peer_node
        .wait_until_height(inclusion.height, SECONDS_TO_WAIT)
        .await?;

    // depth=1: this block migrates the inclusion block to the seeder SM.
    let confirm = genesis_node.mine_block().await?;
    assert_eq!(
        confirm.ledger_total_chunks(DataLedger::OneYear),
        NUM_DATA_CHUNKS as u64,
        "confirmation block must not add OneYear chunks (tip == migrated frontier)"
    );
    genesis_node
        .wait_until_block_index_height(inclusion.height, SECONDS_TO_WAIT)
        .await?;
    peer_node
        .wait_until_height(confirm.height, SECONDS_TO_WAIT)
        .await?;

    let expected: BTreeSet<u32> = (0..NUM_DATA_CHUNKS as u32).collect();
    let mut last_genesis = BTreeSet::new();
    let mut last_peer = BTreeSet::new();
    let mut synced = false;
    for attempt in 0..40 {
        last_genesis = data_offsets(&genesis_node, DataLedger::OneYear, 0);
        last_peer = data_offsets(&peer_node, DataLedger::OneYear, 0);
        info!(
            attempt,
            genesis = ?last_genesis,
            peer = ?last_peer,
            "OneYear slot 0 data offsets"
        );
        if last_genesis == expected && last_peer == expected {
            synced = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert_eq!(
        last_genesis, expected,
        "seeder must migrate every OneYear chunk through the inclusive frontier"
    );
    assert_eq!(
        last_peer, expected,
        "replica must sync every OneYear chunk through the inclusive frontier \
         (a one-chunk-short set is the off-by-one this test exists to catch)"
    );
    assert!(synced);

    let last = LedgerChunkOffset::from((NUM_DATA_CHUNKS - 1) as u64);
    genesis_node
        .verify_migrated_chunk_32b(
            DataLedger::OneYear,
            last,
            &chunks[NUM_DATA_CHUNKS - 1],
            data_size,
        )
        .await;
    peer_node
        .verify_migrated_chunk_32b(
            DataLedger::OneYear,
            last,
            &chunks[NUM_DATA_CHUNKS - 1],
            data_size,
        )
        .await;

    peer_node.stop().await;
    genesis_node.stop().await;
    Ok(())
}

fn slot_owners(
    node: &IrysNodeTest<IrysNodeCtx>,
    ledger: DataLedger,
    slot_index: usize,
) -> Vec<IrysAddress> {
    let snapshot = node.get_canonical_epoch_snapshot();
    let slots = snapshot.ledgers.get_slots(ledger);
    slots
        .get(slot_index)
        .map(|slot| {
            slot.partitions
                .iter()
                .map(|hash| {
                    snapshot
                        .partition_assignments
                        .get_assignment(*hash)
                        .expect("assigned partition must have an assignment entry")
                        .miner_address
                })
                .collect()
        })
        .unwrap_or_default()
}

fn data_offsets(
    node: &IrysNodeTest<IrysNodeCtx>,
    ledger: DataLedger,
    slot_index: usize,
) -> BTreeSet<u32> {
    let mut offsets = BTreeSet::new();
    for interval in node.get_storage_module_intervals(ledger, slot_index, ChunkType::Data) {
        let start: u32 = interval.start().into();
        let end: u32 = interval.end().into();
        offsets.extend(start..=end);
    }
    offsets
}
