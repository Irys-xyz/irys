//! RocksDB v1 stores no tx path. A mined data chunk must derive one, and
//! `poa_is_valid` must accept it. These tests run a node whose submodule
//! index is RocksDB and wait until a canonical block carries that PoA.
//!
//! Every tx here is a multiple of `chunk_size`. These tests do not cover a
//! short final chunk. PoA accepts that chunk and binds its prefix only.

use std::path::Path;
use std::sync::atomic::Ordering;

use eyre::WrapErr as _;

use crate::utils::{self, IrysNodeTest, TimeMode};
use irys_actors::block_producer::BlockProducerCommand;
use irys_chain::IrysNodeCtx;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_database::submodule::tables::SUBMODULE_SCHEMA_ROCKS_V1;
use irys_database::tx_path_derive::derive_tx_path_at_offset;
use irys_domain::ChunkType;
use irys_testing_utils::initialize_tracing;
use irys_types::block_production::{TxPathJob, TxPathSource};
use irys_types::hardfork_config::Cascade;
use irys_types::irys::IrysSigner;
use irys_types::{
    BoundedFee, DataLedger, DataTransaction, H256, IrysBlockHeader, NodeConfig, PoaData,
    SendTraced as _, SubmoduleIndexEngine, UnixTimestamp,
};

const CHUNK_SIZE: usize = 32;

fn rocks_config(chunks_in_partition: u64, blocks_in_epoch: u64) -> NodeConfig {
    let mut config = NodeConfig::testing();
    config.database.submodule_index = SubmoduleIndexEngine::Rocksdb;
    // The process-wide cache is installed by the first submodule. A chain
    // test does not need the production 1 GiB cache.
    config.database.rocksdb_block_cache_bytes = 128 * 1024 * 1024;
    config.database.rocksdb_write_buffer_bytes = 96 * 1024 * 1024;
    config.consensus.get_mut().chunk_size = CHUNK_SIZE as u64;
    config.consensus.get_mut().num_chunks_in_partition = chunks_in_partition;
    config.consensus.get_mut().num_chunks_in_recall_range = 2;
    config.consensus.get_mut().entropy_packing_iterations = 1_000;
    config.consensus.get_mut().block_migration_depth = 1;
    config.consensus.get_mut().epoch.num_blocks_in_epoch = blocks_in_epoch;
    config.consensus.get_mut().epoch.submit_ledger_epoch_length = 100;
    config
        .consensus
        .get_mut()
        .hardforks
        .frontier
        .number_of_ingress_proofs_total = 1;
    config
}

fn assert_rocks_v1(base: &Path) -> eyre::Result<()> {
    let modules = base.join("storage_modules");
    let mut found = 0_u32;
    for entry in std::fs::read_dir(&modules).wrap_err("read storage_modules")? {
        let schema = entry?.path().join("rocks").join("SCHEMA");
        if !schema.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&schema)?;
        eyre::ensure!(
            text == SUBMODULE_SCHEMA_ROCKS_V1,
            "{} is {text:?}, want RocksDB v1",
            schema.display()
        );
        found += 1;
    }
    eyre::ensure!(found > 0, "no RocksDB v1 index under {}", modules.display());
    Ok(())
}

async fn start_rocks_node(
    mut config: NodeConfig,
    submodules: usize,
) -> eyre::Result<(IrysNodeTest<IrysNodeCtx>, IrysSigner)> {
    let signer = config.new_random_signer();
    config.fund_genesis_accounts(vec![&signer]);
    // mine_until can take MINE_UNTIL_BLOCKS blocks. On the real clock that is
    // ~1 s each and outlasts the heavy-test kill, hiding the miss report.
    let node = IrysNodeTest::new_genesis(config).with_time_mode(TimeMode::Accelerated);
    StorageSubmodulesConfig::load_for_test(node.cfg.base_directory.clone(), submodules)?;
    let node = node.start_and_wait_for_packing("GENESIS", 30).await;
    node.stop_mining();
    assert_rocks_v1(&node.node_ctx.config.node_config.base_directory)?;
    Ok((node, signer))
}

fn aligned_bytes(chunks: usize, fill: u8) -> Vec<u8> {
    vec![fill; chunks * CHUNK_SIZE]
}

async fn post_ledger_tx(
    node: &IrysNodeTest<IrysNodeCtx>,
    signer: &IrysSigner,
    ledger: DataLedger,
    data: Vec<u8>,
) -> eyre::Result<DataTransaction> {
    let price = node.get_data_price(ledger, data.len() as u64).await?;
    let perm = if ledger == DataLedger::Publish {
        Some(BoundedFee::new(price.perm_fee))
    } else {
        None
    };
    let tx = signer.create_transaction_with_fees(
        data,
        node.get_anchor().await?,
        ledger,
        BoundedFee::new(price.term_fee),
        perm,
    )?;
    let tx = signer.sign_transaction(tx)?;
    node.post_data_tx_raw(&tx.header).await;
    node.wait_for_mempool(tx.header.id, 20).await?;
    node.upload_chunks(&tx).await?;
    Ok(tx)
}

fn ledger_tx_count(block: &IrysBlockHeader, ledger: DataLedger) -> usize {
    block
        .data_ledgers
        .iter()
        .find(|row| row.ledger_id == ledger as u32)
        .map(|row| row.tx_ids.0.len())
        .unwrap_or(0)
}

fn is_data_poa(poa: &PoaData, ledger: DataLedger) -> bool {
    poa.ledger_id == Some(u32::from(ledger))
        && poa.data_path.is_some()
        && poa.tx_path.as_ref().is_some_and(|path| !path.0.is_empty())
        && poa.chunk.is_some()
}

fn assigned_partition(
    node: &IrysNodeTest<IrysNodeCtx>,
    ledger: DataLedger,
    slot: usize,
) -> Option<H256> {
    let modules = node.node_ctx.storage_modules_guard.read();
    modules.iter().find_map(|module| {
        let assignment = module.partition_assignment()?;
        (assignment.ledger_id == Some(u32::from(ledger)) && assignment.slot_index == Some(slot))
            .then_some(assignment.partition_hash)
    })
}

/// Cascade active at the Unix epoch. Genesis uses the wall clock, which is
/// after that instant, so OneYear and ThirtyDay receive partitions. The epoch
/// lengths stay long so this test does not roll those ledgers.
fn cascade_since_unix_epoch(config: &mut NodeConfig) {
    config.consensus.get_mut().hardforks.cascade = Some(Cascade {
        activation_timestamp: UnixTimestamp::from_secs(0),
        one_year_epoch_length: 100,
        thirty_day_epoch_length: 100,
        annual_cost_per_gb: Cascade::default_annual_cost_per_gb(),
    });
}

/// Mine until the canonical height is past `num_blocks_in_epoch`.
async fn mine_past_epoch(node: &IrysNodeTest<IrysNodeCtx>) -> eyre::Result<()> {
    let activation = node.node_ctx.config.consensus.epoch.num_blocks_in_epoch;
    while node.get_canonical_chain_height().await <= activation {
        node.mine_block().await?;
    }
    Ok(())
}

const MINE_UNTIL_BLOCKS: usize = 96;

async fn mine_until<F>(
    node: &IrysNodeTest<IrysNodeCtx>,
    label: &str,
    mut ready: F,
) -> eyre::Result<IrysBlockHeader>
where
    F: FnMut(&IrysBlockHeader) -> bool,
{
    let mut seen = Vec::new();
    // Any partition can win a block, so a specific one can miss 24 blocks in
    // a row. 96 keeps the miss rate well under 1%.
    for _ in 0..MINE_UNTIL_BLOCKS {
        let block = node.mine_block().await?;
        let poa = &block.poa;
        seen.push(format!(
            "h{} ledger={:?} data={} tx={} part={} off={}",
            block.height,
            poa.ledger_id,
            poa.data_path.is_some(),
            poa.tx_path.is_some(),
            poa.partition_hash,
            poa.partition_chunk_offset
        ));
        if ready(&block) {
            return Ok(block);
        }
    }
    eyre::bail!("no {label} in {MINE_UNTIL_BLOCKS} blocks: {seen:?}")
}

async fn wait_for_data_on_slot(
    node: &IrysNodeTest<IrysNodeCtx>,
    ledger: DataLedger,
    slot: usize,
) -> eyre::Result<H256> {
    for _ in 0..16 {
        if let Some(hash) = assigned_partition(node, ledger, slot) {
            let intervals = node.get_storage_module_intervals(ledger, slot, ChunkType::Data);
            if !intervals.is_empty() {
                return Ok(hash);
            }
        }
        node.mine_block().await?;
    }
    eyre::bail!("{ledger:?} slot {slot} has no data interval after 16 blocks")
}

/// The PoA tx path is the derive for this slot, and the tx is not the first
/// tx in the block that introduced the chunk.
fn assert_derived_tx_index(
    node: &IrysNodeTest<IrysNodeCtx>,
    block: &IrysBlockHeader,
    ledger: DataLedger,
    slot: usize,
) -> eyre::Result<()> {
    let chunks = node.node_ctx.config.consensus.num_chunks_in_partition;
    let ledger_offset = u64::try_from(slot)
        .ok()
        .and_then(|slot| slot.checked_mul(chunks))
        .and_then(|base| base.checked_add(u64::from(block.poa.partition_chunk_offset)))
        .ok_or_else(|| eyre::eyre!("ledger offset overflow"))?;
    let derived = derive_tx_path_at_offset(
        node.node_ctx.db.as_ref(),
        ledger,
        ledger_offset,
        node.node_ctx.config.consensus.chunk_size,
    )
    .wrap_err_with(|| format!("derive {ledger:?} offset {ledger_offset}"))?;
    eyre::ensure!(
        derived.tx_index > 0,
        "{ledger:?} offset {ledger_offset} derived tx_index {}, tx_count {}",
        derived.tx_index,
        derived.tx_count
    );
    let poa_path = block
        .poa
        .tx_path
        .as_ref()
        .map(|path| path.0.as_slice())
        .unwrap_or(&[]);
    eyre::ensure!(
        poa_path == derived.proof.proof.as_slice(),
        "poa tx path ({} bytes) does not match the derived proof ({} bytes)",
        poa_path.len(),
        derived.proof.proof.len()
    );
    Ok(())
}

/// Two chunk-aligned publish txs share one block. After migration the miner
/// produces a Submit data PoA and, once the txs promote, a Publish data PoA.
/// Both go through validation on a RocksDB v1 index.
#[test_log::test(tokio::test)]
async fn heavy_test_rocks_v1_submit_and_publish_data_poa() -> eyre::Result<()> {
    initialize_tracing();
    let (node, signer) = start_rocks_node(rocks_config(4, 100), 3).await?;

    let first = post_ledger_tx(&node, &signer, DataLedger::Publish, aligned_bytes(2, 0x11)).await?;
    let second =
        post_ledger_tx(&node, &signer, DataLedger::Publish, aligned_bytes(2, 0x22)).await?;

    // Proofs are produced after inclusion. Waiting first would mine the two
    // txs into different blocks.
    let multi = node.mine_block().await?;
    assert!(
        ledger_tx_count(&multi, DataLedger::Submit) >= 2
            || ledger_tx_count(&multi, DataLedger::Publish) >= 2,
        "one block must carry both chunk-aligned txs, submit={} publish={}",
        ledger_tx_count(&multi, DataLedger::Submit),
        ledger_tx_count(&multi, DataLedger::Publish)
    );

    wait_for_data_on_slot(&node, DataLedger::Submit, 0).await?;
    mine_until(&node, "submit data poa", |block| {
        is_data_poa(&block.poa, DataLedger::Submit)
    })
    .await?;

    node.wait_for_ingress_proofs(vec![first.header.id, second.header.id], 40)
        .await?;
    node.mine_block().await?;
    wait_for_data_on_slot(&node, DataLedger::Publish, 0).await?;
    mine_until(&node, "publish data poa", |block| {
        is_data_poa(&block.poa, DataLedger::Publish)
    })
    .await?;

    node.stop().await;
    Ok(())
}

/// OneYear and ThirtyDay use the same cascade setup and the same slot-0 PoA
/// wait. The fill byte only distinguishes the payload.
async fn mine_cascaded_ledger_data_poa(
    ledger: DataLedger,
    fill: u8,
    label: &str,
) -> eyre::Result<()> {
    let mut config = rocks_config(4, 2);
    cascade_since_unix_epoch(&mut config);
    // Publish, Submit, OneYear, ThirtyDay, and one capacity partition.
    let (node, signer) = start_rocks_node(config, 5).await?;
    mine_past_epoch(&node).await?;
    let _tx = post_ledger_tx(&node, &signer, ledger, aligned_bytes(4, fill)).await?;
    wait_for_data_on_slot(&node, ledger, 0).await?;
    mine_until(&node, label, |block| is_data_poa(&block.poa, ledger)).await?;
    node.stop().await;
    Ok(())
}

/// A chunk-aligned OneYear tx is mined as a data chunk on the term ledger.
#[test_log::test(tokio::test)]
async fn heavy_test_rocks_v1_term_ledger_data_poa() -> eyre::Result<()> {
    initialize_tracing();
    mine_cascaded_ledger_data_poa(DataLedger::OneYear, 0x33, "one-year data poa").await
}

/// Two chunk-aligned publish txs overflow one partition. The epoch gives the
/// only spare capacity partition to Publish slot 1 (Publish is the first
/// active ledger). A later block's PoA is a data chunk from that slot.
///
/// Posting both txs in one epoch asks for four extra slots per ledger that
/// holds the chunks (threshold adds two, and growth adds two when the added
/// chunk count is greater than one partition). One spare partition fills
/// slot 1. The later slots stay empty, and the eight chunks occupy slot 0
/// and slot 1.
#[test_log::test(tokio::test)]
async fn heavy_test_rocks_v1_nonzero_slot_data_poa() -> eyre::Result<()> {
    initialize_tracing();
    // Testing config activates Cascade, so genesis assigns Publish, Submit,
    // OneYear, and ThirtyDay. The fifth module is the capacity partition
    // that becomes Publish slot 1.
    let (node, signer) = start_rocks_node(rocks_config(4, 2), 5).await?;

    let _first =
        post_ledger_tx(&node, &signer, DataLedger::Publish, aligned_bytes(4, 0x41)).await?;
    let _second =
        post_ledger_tx(&node, &signer, DataLedger::Publish, aligned_bytes(4, 0x42)).await?;
    let multi = node.mine_block().await?;
    assert!(
        ledger_tx_count(&multi, DataLedger::Submit) >= 2,
        "one block must carry both txs, got {}",
        ledger_tx_count(&multi, DataLedger::Submit)
    );

    let slot_one = wait_for_data_on_slot(&node, DataLedger::Publish, 1).await?;
    let block = mine_until(&node, "slot 1 data poa", |block| {
        is_data_poa(&block.poa, DataLedger::Publish) && block.poa.partition_hash == slot_one
    })
    .await?;
    assert!(block.poa.partition_chunk_offset < 4);
    // Slot 1 holds the second tx of the block that introduced these chunks.
    assert_derived_tx_index(&node, &block, DataLedger::Publish, 1)?;

    node.stop().await;
    Ok(())
}

/// A chunk-aligned ThirtyDay tx is mined as a data chunk on that ledger.
#[test_log::test(tokio::test)]
async fn heavy_test_rocks_v1_thirty_day_data_poa() -> eyre::Result<()> {
    initialize_tracing();
    mine_cascaded_ledger_data_poa(DataLedger::ThirtyDay, 0x34, "thirty-day data poa").await
}

/// Two chunk-aligned publish txs overflow one partition. They live on the
/// Submit ledger until promotion. One node cannot produce the two ingress
/// proofs promotion requires, so the spare capacity partition becomes Submit
/// slot 1. The PoA is mined there while Publish still has no data.
#[test_log::test(tokio::test)]
async fn heavy_test_rocks_v1_submit_nonzero_slot_data_poa() -> eyre::Result<()> {
    initialize_tracing();
    let mut config = rocks_config(4, 2);
    // One signer produces one ingress proof. Promotion needs two, so the
    // chunks stay on Submit for this test.
    config
        .consensus
        .get_mut()
        .hardforks
        .frontier
        .number_of_ingress_proofs_total = 2;
    let (node, signer) = start_rocks_node(config, 5).await?;

    let _first =
        post_ledger_tx(&node, &signer, DataLedger::Publish, aligned_bytes(4, 0x51)).await?;
    let _second =
        post_ledger_tx(&node, &signer, DataLedger::Publish, aligned_bytes(4, 0x52)).await?;
    let multi = node.mine_block().await?;
    assert!(
        ledger_tx_count(&multi, DataLedger::Submit) >= 2,
        "one block must carry both txs, got {}",
        ledger_tx_count(&multi, DataLedger::Submit)
    );

    let slot_one = wait_for_data_on_slot(&node, DataLedger::Submit, 1).await?;
    assert!(
        node.get_storage_module_intervals(DataLedger::Publish, 0, ChunkType::Data)
            .is_empty(),
        "submit txs must not promote onto publish"
    );
    assert!(assigned_partition(&node, DataLedger::Publish, 1).is_none());

    let block = mine_until(&node, "submit slot 1 data poa", |block| {
        is_data_poa(&block.poa, DataLedger::Submit) && block.poa.partition_hash == slot_one
    })
    .await?;
    assert_derived_tx_index(&node, &block, DataLedger::Submit, 1)?;

    node.stop().await;
    Ok(())
}

/// A solution whose derive already failed is dropped before a block is built.
/// The node then mines a later block.
#[test_log::test(tokio::test)]
async fn heavy_test_rocks_v1_failed_derive_drops_the_solution() -> eyre::Result<()> {
    initialize_tracing();
    let (node, _signer) = start_rocks_node(rocks_config(4, 100), 3).await?;
    let before = irys_actors::tx_path_derive_failed_for_test();
    let height = node.get_canonical_chain_height().await;

    node.node_ctx
        .service_senders
        .block_producer
        .send_traced(BlockProducerCommand::SetTestBlocksRemaining(Some(1)))
        .unwrap();

    let mut solution = utils::solution_context(&node.node_ctx).await?;
    let job = TxPathJob::finished(Err(eyre::eyre!("data_root mismatch at offset 0")));
    solution.tx_path = TxPathSource::Derive(job);

    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    node.node_ctx
        .service_senders
        .block_producer
        .send_traced(BlockProducerCommand::SolutionFound {
            solution,
            response: response_tx,
        })
        .unwrap();
    let produced = response_rx.await??;
    assert!(produced.is_none(), "a failed derive must not build a block");
    assert!(
        irys_actors::tx_path_derive_failed_for_test() > before,
        "tx path derive failure metric did not increase"
    );
    assert_eq!(
        node.node_ctx
            .block_producer_inner
            .tx_path_fail_streak
            .load(Ordering::Relaxed),
        1,
        "one dropped solution starts the failure streak"
    );

    let block = node.mine_block().await?;
    assert!(block.height > height, "mining continues after the drop");

    node.stop().await;
    Ok(())
}
