//! Can a reorg that drops an epoch block lose data that was promoted on the
//! dropped fork?
//!
//! The concern (2026-11-25): an epoch block's outcome depends on the last
//! blocks of its epoch (commitments, ledger totals including the epoch block's
//! own txs). A fork that promotes data can allocate a Publish slot and assign a
//! partition to it; miners then write the promoted chunks into that partition.
//! If the fork is dropped, the partition goes back to capacity and is wiped.
//! If by then the chunk cache has pruned the bodies (it prunes once the txs
//! migrated and a local module persisted them), the wipe may remove the last
//! copy.
//!
//! The test drives the worst case it can: the minority fork is longer than the
//! migration depth, so the reorg goes through network-partition recovery and
//! the promoted tx had migrated (cache prune allowed) before the reorg.
//!
//! It asserts that after the reorg the minority node still holds every chunk
//! of the promoted tx somewhere (a cached body or a storage module), and that
//! the tx is promoted again on the winning chain with its chunks served from
//! storage. A failure here is the data-loss case.

use crate::utils::IrysNodeTest;
use irys_chain::IrysNodeCtx;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_database::db::DatabaseProviderCacheExt as _;
use irys_domain::forkchoice_markers::finalized_height;
use irys_domain::{
    BlockIndexReadGuard, ChunkType, PACKING_PARAMS_FILE_NAME, StorageModulesReadGuard,
};
use irys_types::storage::Interval;
use irys_types::{
    DataLedger, DataTransaction, NodeConfig, PartitionChunkOffset, TxChunkOffset, UnixTimestamp,
    hardfork_config::Cascade, irys::IrysSigner,
};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tracing::{info, warn};

const CHUNK_SIZE: u64 = 32;
const CHUNKS_PER_PARTITION: u64 = 10;
/// More than one slot, so promotion needs a Publish slot beyond slot 0.
const TX_CHUNKS: usize = 12;

#[test_log::test(tokio::test)]
#[ignore = "data loss on deep partition recovery: needs finality-gated cache reclaim + epoch inputs from the finalized point (design Parts 1-2, deferred)"]
async fn heavy4_slow_promoted_data_survives_epoch_reorg() -> eyre::Result<()> {
    let seconds_to_wait = 30;
    let block_migration_depth: u32 = 1;

    let mut genesis_config = NodeConfig::testing().with_consensus(|c| {
        c.chunk_size = CHUNK_SIZE;
        c.num_chunks_in_partition = CHUNKS_PER_PARTITION;
        c.num_chunks_in_recall_range = 2;
        c.num_partitions_per_slot = irys_types::ReplicaCount::new(1);
        c.epoch.num_blocks_in_epoch = 2;
        c.block_migration_depth = block_migration_depth;
        c.entropy_packing_iterations = 1_000;
        c.genesis.initial_packed_partitions = Some(5.0);
        c.hardforks.frontier.number_of_ingress_proofs_total = 1;
        c.hardforks.cascade = Some(Cascade {
            activation_timestamp: UnixTimestamp::from_secs(0),
            one_year_epoch_length: 365,
            thirty_day_epoch_length: 30,
            annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
        });
    });
    genesis_config.storage.num_writes_before_sync = 1;
    // Prune cached bodies as early as the node allows.
    genesis_config.cache.cache_clean_lag = 0;
    // Below its size target the cache only holds bodies (HoldOnly passes); a
    // busy mainnet node is above it. 1 KiB (prune at 80%) holds the 12-chunk tx
    // until it is promoted; a later unrelated upload pushes the cache over it.
    genesis_config.cache.max_cache_size_bytes = 1024;

    let uploader = genesis_config.new_random_signer();
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&uploader, &peer_signer]);

    let genesis_test = IrysNodeTest::new_genesis(genesis_config.clone());
    StorageSubmodulesConfig::load_for_test(genesis_test.cfg.base_directory.clone(), 10)?;
    let genesis = genesis_test
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;

    let peer_config = genesis.testing_peer_with_signer(&peer_signer);
    let peer_test = IrysNodeTest::new(peer_config);
    StorageSubmodulesConfig::load_for_test(peer_test.cfg.base_directory.clone(), 10)?;
    let peer = peer_test
        .start_and_wait_for_packing("PEER", seconds_to_wait)
        .await;
    IrysNodeTest::announce_between(&genesis, &peer).await?;
    peer.stop_mining();
    genesis
        .post_stake_and_pledge_commitments(&[&peer], seconds_to_wait)
        .await?;

    // ─── Shared base: blocks 1–2 (epoch boundary assigns the peer a partition) ───
    for h in 1..=2_u64 {
        genesis.mine_block().await?;
        let block = genesis.get_block_by_height(h).await?;
        peer.wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
    }
    genesis.wait_for_packing(seconds_to_wait).await;
    peer.wait_for_packing(seconds_to_wait).await;
    let fork_height = 2_u64;
    let publish_slots_before = publish_slot_count(&genesis);
    info!(
        publish_slots_before,
        "Shared base complete at height {fork_height}"
    );

    // ─── Minority fork (genesis): upload, promote, migrate, let the cache prune ───
    genesis.gossip_disable();
    peer.gossip_disable();

    let chunks: Vec<[u8; 32]> = (0..TX_CHUNKS)
        .map(|i| [200_u8.wrapping_add(i as u8); 32])
        .collect();
    let tx = post_publish_tx(&genesis, &uploader, &chunks).await?;
    let data_root = tx.header.data_root;

    let mut promoted_at = None;
    let mut minority_blocks = 0_u64;
    let deadline = Instant::now() + Duration::from_secs(120);
    // Mine until the tx is promoted, then far enough past it for migration,
    // an epoch boundary, and the cache prune pass.
    loop {
        genesis.mine_block_without_gossip().await?;
        minority_blocks += 1;
        if promoted_at.is_none() && genesis.get_is_promoted(&tx.header.id).await? {
            promoted_at = Some(fork_height + minority_blocks);
            info!(height = ?promoted_at, "Promoted on the minority fork");
        }
        if let Some(h) = promoted_at
            && fork_height + minority_blocks >= h + 4
            && fork_height + minority_blocks >= fork_height + 6
        {
            break;
        }
        eyre::ensure!(
            Instant::now() < deadline,
            "tx was not promoted on the minority fork within 120s ({minority_blocks} blocks)"
        );
    }
    // Fill the cache past its target with an unrelated upload, as a busy node
    // would be, so prune passes reclaim bodies instead of holding them.
    let filler: Vec<[u8; 32]> = (0..40).map(|i| [10_u8.wrapping_add(i as u8); 32]).collect();
    post_publish_tx(&genesis, &uploader, &filler).await?;

    // Keep mining the minority fork until the cache has pruned the tx's bodies:
    // the worst case is a reorg after the cache let go of them.
    let prune_deadline = Instant::now() + Duration::from_secs(120);
    loop {
        genesis
            .wait_until_block_index_height(
                fork_height + minority_blocks - u64::from(block_migration_depth),
                seconds_to_wait,
            )
            .await?;
        // The prune pass runs after migration; give it a moment.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let cached = cached_bodies(&genesis, data_root, TX_CHUNKS)?;
        if cached.iter().all(|c| !*c) {
            info!(
                height = fork_height + minority_blocks,
                "Cache pruned every body of the promoted tx"
            );
            break;
        }
        if Instant::now() >= prune_deadline || minority_blocks >= 30 {
            panic!(
                "PRECONDITION NOT REACHED: the cache still holds bodies {cached:?} after \
                 {minority_blocks} minority blocks; the worst case cannot be tested"
            );
        }
        genesis.mine_block_without_gossip().await?;
        minority_blocks += 1;
    }
    let minority_tip = fork_height + minority_blocks;
    info!(modules = ?module_states(&genesis), "Genesis modules before the reorg");

    let publish_slots_minority = publish_slot_count(&genesis);
    let cached_before = cached_bodies(&genesis, data_root, TX_CHUNKS)?;
    let stored_before = stored_chunks(&genesis, data_root, TX_CHUNKS);
    let publish_modules_with_data = modules_with_data(&genesis, DataLedger::Publish);
    info!(
        minority_tip,
        publish_slots_before,
        publish_slots_minority,
        cached_bodies = cached_before.iter().filter(|c| **c).count(),
        stored_publish = stored_before.iter().filter(|s| s.0).count(),
        stored_submit = stored_before.iter().filter(|s| s.1).count(),
        ?publish_modules_with_data,
        "Minority fork state before the reorg"
    );
    assert!(
        publish_slots_minority > publish_slots_before,
        "the minority fork must allocate a new Publish slot for the scenario to apply \
         (before {publish_slots_before}, after {publish_slots_minority})"
    );

    // ─── Majority fork (peer): longer, without the tx ───
    peer.node_ctx.start_vdf();
    peer.wait_for_packing(seconds_to_wait).await;
    let mut peer_blocks = Vec::new();
    for _ in 0..(minority_blocks + 1) {
        let (block, _, _) = peer.mine_block_without_gossip().await?;
        peer_blocks.push(block);
    }
    let majority_tip = fork_height + minority_blocks + 1;

    // ─── Reorg genesis onto the majority fork ───
    genesis.gossip_enable();
    peer.gossip_enable();
    IrysNodeTest::announce_between(&genesis, &peer).await?;
    for block in &peer_blocks {
        peer.gossip_block_to_peers(block)?;
        genesis
            .wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
    }
    genesis
        .wait_until_height(majority_tip, seconds_to_wait)
        .await?;
    let tip_hash = genesis.get_block_by_height(majority_tip).await?.block_hash;
    assert_eq!(
        tip_hash,
        peer_blocks.last().unwrap().block_hash,
        "genesis must adopt the majority fork"
    );
    genesis
        .wait_until_block_index_height(
            majority_tip - u64::from(block_migration_depth),
            seconds_to_wait,
        )
        .await?;
    genesis.wait_for_packing(seconds_to_wait).await;
    info!(
        publish_slots = publish_slot_count(&genesis),
        modules = ?module_states(&genesis),
        "Genesis modules after the reorg"
    );

    // ─── 1. Right after the reorg: is every chunk still somewhere on genesis? ───
    let cached_after = cached_bodies(&genesis, data_root, TX_CHUNKS)?;
    let stored_after = stored_chunks(&genesis, data_root, TX_CHUNKS);
    let missing: Vec<usize> = (0..TX_CHUNKS)
        .filter(|i| !cached_after[*i] && !stored_after[*i].0 && !stored_after[*i].1)
        .collect();
    info!(
        cached_bodies = cached_after.iter().filter(|c| **c).count(),
        stored_publish = stored_after.iter().filter(|s| s.0).count(),
        stored_submit = stored_after.iter().filter(|s| s.1).count(),
        ?missing,
        "Genesis after the reorg"
    );
    assert!(
        missing.is_empty(),
        "DATA LOSS: after the reorg genesis holds no copy (cache or storage) of chunks \
         {missing:?} of promoted tx {} (before the reorg: cached {cached_before:?}, \
         stored (publish, submit) {stored_before:?})",
        tx.header.id
    );

    // ─── 2. The tx is promoted again on the winning chain and served from storage ───
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        genesis.mine_block().await?;
        if genesis.get_is_promoted(&tx.header.id).await?
            && stored_chunks(&genesis, data_root, TX_CHUNKS)
                .iter()
                .all(|(publish, _)| *publish)
        {
            break;
        }
        if Instant::now() >= deadline {
            let stored = stored_chunks(&genesis, data_root, TX_CHUNKS);
            let cached = cached_bodies(&genesis, data_root, TX_CHUNKS)?;
            panic!(
                "DATA LOSS: tx {} not promoted with all chunks in Publish storage on the \
                 winning chain within 120s; stored (publish, submit) {stored:?}, cached {cached:?}",
                tx.header.id
            );
        }
    }
    for (i, expected) in chunks.iter().enumerate() {
        let packed = genesis
            .node_ctx
            .chunk_provider
            .get_chunk_by_data_root(
                DataLedger::Publish,
                data_root,
                TxChunkOffset::from(i as u32),
            )?
            .expect("checked above");
        let unpacked = unpack_format(&genesis, packed);
        assert_eq!(
            &unpacked[..],
            &expected[..],
            "chunk {i} bytes after re-promotion"
        );
    }
    info!("Promoted data survived the epoch reorg");

    genesis.stop().await;
    peer.stop().await;
    Ok(())
}

/// (module id, ledger, slot, partition hash prefix, Data intervals, Entropy intervals)
fn module_states(node: &IrysNodeTest<IrysNodeCtx>) -> Vec<String> {
    let sms = node.node_ctx.storage_modules_guard.read();
    sms.iter()
        .map(|sm| {
            let pa = sm.partition_assignment();
            format!(
                "sm{} ledger={:?} slot={:?} hash={} data={:?} entropy={}",
                sm.id,
                pa.and_then(|p| p.ledger_id),
                pa.and_then(|p| p.slot_index),
                pa.map(|p| p.partition_hash.to_string()[..6].to_string())
                    .unwrap_or_else(|| "-".into()),
                sm.get_intervals(ChunkType::Data),
                sm.get_intervals(ChunkType::Entropy).len(),
            )
        })
        .collect()
}

fn publish_slot_count(node: &IrysNodeTest<IrysNodeCtx>) -> usize {
    node.node_ctx
        .block_tree_guard
        .read()
        .canonical_epoch_snapshot()
        .ledgers
        .get_slots(DataLedger::Publish)
        .len()
}

fn modules_with_data(node: &IrysNodeTest<IrysNodeCtx>, ledger: DataLedger) -> Vec<(usize, usize)> {
    let sms = node.node_ctx.storage_modules_guard.read();
    sms.iter()
        .filter_map(|sm| {
            let pa = sm.partition_assignment()?;
            (pa.ledger_id == Some(ledger.into()) && !sm.get_intervals(ChunkType::Data).is_empty())
                .then(|| (sm.id, pa.slot_index.unwrap_or(usize::MAX)))
        })
        .collect()
}

/// Per tx chunk: does the cache still hold the body?
fn cached_bodies(
    node: &IrysNodeTest<IrysNodeCtx>,
    data_root: irys_types::H256,
    count: usize,
) -> eyre::Result<Vec<bool>> {
    (0..count)
        .map(|i| {
            let entry = node.node_ctx.db.view_cache_eyre(|tx| {
                irys_database::cached_chunk_by_chunk_offset(
                    tx,
                    data_root,
                    TxChunkOffset::from(i as u32),
                )
            })?;
            Ok(entry.is_some_and(|(_, chunk)| chunk.chunk.is_some()))
        })
        .collect()
}

/// Per tx chunk: (served from Publish storage, served from Submit storage).
fn stored_chunks(
    node: &IrysNodeTest<IrysNodeCtx>,
    data_root: irys_types::H256,
    count: usize,
) -> Vec<(bool, bool)> {
    let has = |ledger, i: usize| {
        matches!(
            node.node_ctx.chunk_provider.get_chunk_by_data_root(
                ledger,
                data_root,
                TxChunkOffset::from(i as u32)
            ),
            Ok(Some(_))
        )
    };
    (0..count)
        .map(|i| (has(DataLedger::Publish, i), has(DataLedger::Submit, i)))
        .collect()
}

fn unpack_format(node: &IrysNodeTest<IrysNodeCtx>, chunk: irys_types::ChunkFormat) -> Vec<u8> {
    match chunk {
        irys_types::ChunkFormat::Unpacked(unpacked) => unpacked.bytes.0,
        irys_types::ChunkFormat::Packed(packed) => {
            irys_packing::unpack(
                &packed,
                node.node_ctx.config.consensus.entropy_packing_iterations,
                node.node_ctx.config.consensus.chunk_size as usize,
                node.node_ctx.config.consensus.chain_id,
            )
            .expect("stored chunk fits the entropy buffer")
            .bytes
            .0
        }
    }
}

async fn post_publish_tx(
    node: &IrysNodeTest<IrysNodeCtx>,
    signer: &IrysSigner,
    chunks: &[[u8; 32]],
) -> eyre::Result<DataTransaction> {
    let data: Vec<u8> = chunks.concat();
    let data_size = data.len() as u64;
    let anchor = node.get_anchor().await?;
    let price = node.get_data_price(DataLedger::Publish, data_size).await?;
    let tx = signer.create_publish_transaction(
        data,
        anchor,
        price.perm_fee.into(),
        price.term_fee.into(),
    )?;
    let tx = signer.sign_transaction(tx)?;
    node.ingest_data_tx(tx.header.clone()).await?;
    node.wait_for_mempool(tx.header.id, 10).await?;
    for i in 0..chunks.len() {
        node.post_chunk_32b_with_status(&tx, i, chunks).await;
    }
    if node.get_is_promoted(&tx.header.id).await.unwrap_or(false) {
        warn!("tx promoted before any block — unexpected");
    }
    Ok(tx)
}

/// What one storage module holds: (Data intervals, Entropy intervals, reset flag).
type ModuleView = (
    Vec<Interval<PartitionChunkOffset>>,
    Vec<Interval<PartitionChunkOffset>>,
    bool,
);

fn module_views(guard: &StorageModulesReadGuard) -> BTreeMap<usize, ModuleView> {
    guard
        .read()
        .iter()
        .map(|sm| {
            (
                sm.id,
                (
                    sm.get_intervals(ChunkType::Data),
                    sm.get_intervals(ChunkType::Entropy),
                    sm.reset_incomplete(),
                ),
            )
        })
        .collect()
}

/// Every offset in `before` is still in `after` (intervals are merged, so
/// each old interval must sit inside one new interval).
fn covers(
    before: &[Interval<PartitionChunkOffset>],
    after: &[Interval<PartitionChunkOffset>],
) -> bool {
    before.iter().all(|old| {
        after
            .iter()
            .any(|new| new.start() <= old.start() && old.end() <= new.end())
    })
}

/// Whether the block at `height` is final on this index: at or below the
/// finalized height of the index tip.
fn is_final_on(index: &BlockIndexReadGuard, config: &irys_types::Config, height: u64) -> bool {
    finalized_height(
        index.read().latest_height(),
        config.consensus.block_tree_depth,
        u64::from(config.consensus.block_migration_depth),
    ) >= height
}

/// A shallow reorg across an epoch boundary whose two sides differ in a
/// pledge-driven partition assignment must not wipe any drive before that
/// epoch is final.
///
/// The peer pledges a second partition on its own fork; the fork's epoch block
/// assigns it and the peer packs it onto an empty drive. The genesis fork,
/// longer by one block and without that pledge, wins. The reorg (2 blocks) is
/// within the migration depth. On the peer, from before the reorg until the
/// genesis-fork epoch is final, every module's Data and Entropy intervals only
/// grow, no module is reset, and the released drive's packing file is
/// untouched. Once the epoch is final the drive is released (non-vacuity).
///
/// The epoch is longer than the block tree depth so that an epoch becomes
/// final before the next one replaces its pending update.
#[test_log::test(tokio::test)]
async fn heavy4_shallow_epoch_reorg_wipes_no_drive() -> eyre::Result<()> {
    const NUM_BLOCKS_IN_EPOCH: u64 = 6;
    const MIGRATION_DEPTH: u32 = 3;
    const BLOCK_TREE_DEPTH: u64 = 5;
    const PEER_DRIVES: usize = 3;
    let seconds_to_wait = 30;

    let mut genesis_config = NodeConfig::testing_with_epochs(NUM_BLOCKS_IN_EPOCH as usize);
    genesis_config.consensus.get_mut().chunk_size = 32;
    genesis_config.consensus.get_mut().block_migration_depth = MIGRATION_DEPTH;
    genesis_config.consensus.get_mut().block_tree_depth = BLOCK_TREE_DEPTH;
    let peer_signer = genesis_config.new_random_signer();
    genesis_config.fund_genesis_accounts(vec![&peer_signer]);

    let genesis = IrysNodeTest::new_genesis(genesis_config.clone())
        .start_and_wait_for_packing("GENESIS", seconds_to_wait)
        .await;
    let peer_test = IrysNodeTest::new(genesis.testing_peer_with_signer(&peer_signer));
    let peer_paths =
        StorageSubmodulesConfig::load_for_test(peer_test.cfg.base_directory.clone(), PEER_DRIVES)?
            .submodule_paths;
    let peer = peer_test.start_with_name("PEER").await;
    IrysNodeTest::announce_between(&genesis, &peer).await?;
    let peer_addr = peer_signer.address();

    // ─── Shared chain: the peer stakes and pledges one partition ───
    let assigned_at = genesis
        .stake_and_pledge_peers(&[&peer], seconds_to_wait)
        .await?;
    // Fork two blocks before the next epoch boundary.
    let fork_height = (assigned_at / NUM_BLOCKS_IN_EPOCH + 1) * NUM_BLOCKS_IN_EPOCH - 2;
    let epoch_height = fork_height + 2;
    while genesis.get_canonical_chain_height().await < fork_height {
        genesis.mine_block().await?;
    }
    let fork_block = genesis.get_block_by_height(fork_height).await?;
    peer.wait_for_block(&fork_block.block_hash, seconds_to_wait)
        .await?;
    peer.wait_until_height(fork_height, seconds_to_wait).await?;
    genesis.wait_for_packing(seconds_to_wait).await;
    peer.wait_for_packing(seconds_to_wait).await;
    let shared_assignments = peer.get_partition_assignments(peer_addr);
    assert_eq!(shared_assignments.len(), 1, "{shared_assignments:?}");
    info!(fork_height, epoch_height, "Shared chain ready");

    // ─── Fork B (peer): a second pledge, assigned at the fork's epoch block ───
    genesis.gossip_disable();
    peer.gossip_disable();
    let pledge2 = peer
        .post_pledge_commitment_without_gossip(Some(peer.get_anchor().await?))
        .await?;
    peer.wait_for_mempool(pledge2.id(), seconds_to_wait).await?;
    let (peer_block, _, _) = peer.mine_block_without_gossip().await?;
    assert_eq!(peer_block.height, fork_height + 1);
    assert!(
        peer_block.commitment_tx_ids().contains(&pledge2.id()),
        "the peer's pledge must be included before its fork's epoch block"
    );
    let (peer_epoch, _, _) = peer.mine_block_without_gossip().await?;
    assert_eq!(peer_epoch.height, epoch_height);
    peer.wait_until_height(epoch_height, seconds_to_wait)
        .await?;
    let deadline = Instant::now() + Duration::from_secs(seconds_to_wait as u64);
    let fork_b_assignments = loop {
        let assignments = peer.get_partition_assignments(peer_addr);
        if assignments.len() == 2 {
            break assignments;
        }
        eyre::ensure!(
            Instant::now() < deadline,
            "the peer's fork epoch did not assign its second pledge: {assignments:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    // By hash: the fork's epoch may also move the shared partition into a
    // ledger slot.
    let fork_only = fork_b_assignments
        .iter()
        .find(|pa| {
            !shared_assignments
                .iter()
                .any(|shared| shared.partition_hash == pa.partition_hash)
        })
        .expect("the fork-only partition")
        .partition_hash;
    peer.wait_for_packing(seconds_to_wait).await;
    let released = {
        let sms = peer.node_ctx.storage_modules_guard.read();
        let sm = sms
            .iter()
            .find(|sm| sm.partition_hash() == Some(fork_only))
            .expect("an empty peer drive takes the fork-only partition");
        assert!(
            !sm.get_intervals(ChunkType::Entropy).is_empty(),
            "the fork-only partition is packed before the reorg"
        );
        info!(id = sm.id, params = ?sm.recorded_params(), "Peer drive holding the fork-only partition");
        sm.id
    };
    let released_file = peer
        .node_ctx
        .config
        .node_config
        .base_directory
        .join(&peer_paths[released])
        .join(PACKING_PARAMS_FILE_NAME);
    let released_bytes = std::fs::read(&released_file)?;

    // Watch the peer's modules from here until the genesis-fork epoch is final.
    let watcher = {
        let modules = peer.node_ctx.storage_modules_guard.clone();
        let index = peer.node_ctx.block_index_guard.clone();
        let config = peer.node_ctx.config.clone();
        let released_file = released_file.clone();
        let released_bytes = released_bytes.clone();
        tokio::spawn(async move {
            let mut previous = module_views(&modules);
            let mut observations = 0_usize;
            let mut violations = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(300);
            loop {
                // Read the modules first: if the epoch is not final after the
                // read, it was not final during it.
                let now = module_views(&modules);
                let file = std::fs::read(&released_file).ok();
                if is_final_on(&index, &config, epoch_height) || Instant::now() >= deadline {
                    break;
                }
                observations += 1;
                for (id, (data, entropy, reset)) in &now {
                    if *reset {
                        violations.push(format!("module {id} has a reset in progress"));
                    }
                    if let Some((old_data, old_entropy, _)) = previous.get(id) {
                        if !covers(old_data, data) {
                            violations
                                .push(format!("module {id} Data shrank: {old_data:?} -> {data:?}"));
                        }
                        if !covers(old_entropy, entropy) {
                            violations.push(format!(
                                "module {id} Entropy shrank: {old_entropy:?} -> {entropy:?}"
                            ));
                        }
                    }
                }
                if previous.keys().any(|id| !now.contains_key(id)) {
                    violations.push(format!(
                        "a module disappeared: {:?} -> {:?}",
                        previous.keys().collect::<Vec<_>>(),
                        now.keys().collect::<Vec<_>>()
                    ));
                }
                if file.as_deref() != Some(released_bytes.as_slice()) {
                    violations.push("the released drive's packing file changed".to_string());
                }
                if !violations.is_empty() {
                    break;
                }
                previous = now;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            (observations, violations)
        })
    };

    // ─── Fork A (genesis): no second pledge, one block longer ───
    let mut genesis_blocks = Vec::new();
    for _ in 0..3 {
        let (block, _, _) = genesis.mine_block_without_gossip().await?;
        genesis_blocks.push(block);
    }
    let genesis_epoch_hash = genesis_blocks[1].block_hash;
    assert_eq!(genesis_blocks[1].height, epoch_height);
    assert_ne!(genesis_epoch_hash, peer_epoch.block_hash);

    // ─── Reorg the peer onto fork A ───
    genesis.gossip_enable();
    peer.gossip_enable();
    IrysNodeTest::announce_between(&genesis, &peer).await?;
    for block in &genesis_blocks {
        genesis.gossip_block_to_peers(block)?;
        peer.wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
    }
    let reorg_tip = genesis_blocks.last().expect("three blocks");
    let deadline = Instant::now() + Duration::from_secs(seconds_to_wait as u64);
    loop {
        if peer
            .get_block_by_height(reorg_tip.height)
            .await
            .is_ok_and(|b| b.block_hash == reorg_tip.block_hash)
        {
            break;
        }
        eyre::ensure!(
            Instant::now() < deadline,
            "peer did not adopt the genesis fork"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        peer.get_partition_assignments(peer_addr).len(),
        1,
        "the fork-only partition is gone from the canonical snapshot"
    );
    info!("Peer reorged onto the genesis fork");

    // ─── Mine until the genesis-fork epoch is final on the peer ───
    let config = peer.node_ctx.config.clone();
    let mut mined = 0;
    while !is_final_on(&peer.node_ctx.block_index_guard, &config, epoch_height) {
        eyre::ensure!(
            mined < 20,
            "epoch {epoch_height} not final after {mined} blocks"
        );
        let block = genesis.mine_block().await?;
        peer.wait_for_block(&block.block_hash, seconds_to_wait)
            .await?;
        mined += 1;
        // Let migration catch up before checking again.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let final_item = peer
        .node_ctx
        .block_index_guard
        .read()
        .get_item(epoch_height)
        .expect("final epoch is in the index");
    assert_eq!(
        final_item.block_hash, genesis_epoch_hash,
        "the genesis fork's epoch block is final on the peer"
    );

    let (observations, violations) = watcher.await?;
    info!(
        observations,
        "Watched the peer's modules until the epoch was final"
    );
    assert!(
        violations.is_empty(),
        "a drive was wiped before the epoch was final: {violations:#?}"
    );
    assert!(observations > 10, "too few observations: {observations}");

    // Non-vacuity: once final, the drive holding the fork-only partition is
    // released.
    let deadline = Instant::now() + Duration::from_secs(seconds_to_wait as u64);
    loop {
        let holds = peer
            .node_ctx
            .storage_modules_guard
            .read()
            .iter()
            .find(|sm| sm.id == released)
            .and_then(|sm| sm.recorded_params())
            .and_then(|p| p.partition_hash);
        if holds != Some(fork_only) {
            info!(
                ?holds,
                "Released drive cleared after the epoch became final"
            );
            break;
        }
        eyre::ensure!(
            Instant::now() < deadline,
            "the drive holding the fork-only partition was not released once final"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    genesis.stop().await;
    peer.stop().await;
    Ok(())
}
