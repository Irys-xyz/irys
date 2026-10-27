//! Rebuild of one ledger tx path from the consensus tables.
//!
//! Mining calls [`derive_tx_path_at_offset_with`] when a submodule index has no
//! stored tx path. The bench binary times the same function. The bytes are the
//! folded tree from [`DataTransactionLedger::merklize_tx_root`], with only the
//! requested leaf encoded.
//!
//! The leaf is [`DataTransactionLedger::tx_root_leaf_value`] at every height.
//! PoA validation uses that function with no height gate, so a block from
//! before the prefix-hash fold and a block from after it take the same leaf.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use eyre::eyre;
use irys_types::{
    Base64, ChunkDataPath, DataLedger, DataRootLeaf, DataTransactionHeader, DataTransactionLedger,
    H256, HASH_SIZE, Helpers as _, Node, PartitionChunkOffset, Proof, ProofDeserialize as _,
    TxChunkOffset, TxPath, expected_chunk_byte_range, generate_data_root,
    generate_leaves_from_data_roots, hash_sha256, validate_path, validate_path_byte_range,
};
use reth_db::DatabaseEnv;
use reth_db::cursor::DbCursorRO as _;
use reth_db::transaction::DbTx;

use crate::db::IrysDatabaseExt as _;
use crate::submodule::{SubmoduleIndex, SubmoduleStore as _};
use crate::tables::{IrysBlockIndexItems, MigratedBlockHashes};
use crate::{
    block_header_by_hash, block_index_item_by_height, block_index_latest_height,
    block_ledger_tx_headers, tx_header_by_txid,
};

/// Whether this binary can decode `IrysDataTxHeaders` for a schema stamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxHeaderSchema {
    /// `true` only for schema V3, the layout this binary decodes.
    pub readable: bool,
    /// Why the stamp is accepted or refused.
    pub message: String,
}

/// One derived tx path plus the split cost of the read that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct DerivedTxPath {
    /// Migrated height that introduced the chunk.
    pub height: u64,
    /// Canonical hash at `height`.
    pub block_hash: H256,
    /// Ledger the offset was resolved in.
    pub ledger: DataLedger,
    /// Index of the tx inside that ledger's `tx_ids`.
    pub tx_index: usize,
    /// Id of that tx.
    pub tx_id: H256,
    /// Number of txs in the ledger at that height. This is the tree width.
    pub tx_count: usize,
    /// `data_root` of the tx. The proof leaf is the fold, not this hash.
    pub data_root: H256,
    /// Recomputed ledger `tx_root`.
    pub tx_root: H256,
    /// Inclusive ledger chunk offset of the block that owns this path.
    pub ledger_start: u64,
    /// Inclusive ledger chunk offset of the tx that owns this path.
    pub tx_ledger_start: u64,
    /// Byte where the tx's leaf starts in the `tx_root` tree: the sum of the
    /// earlier txs' `data_size`. The block index rounds each tx up to whole
    /// chunks, so this is not `tx_ledger_start` times the chunk size.
    pub tx_byte_start: u128,
    /// `data_size` of the tx.
    pub data_size: u64,
    /// Proof for `tx_index` only.
    pub proof: Proof,
    /// Consensus reads, including the read-transaction commit.
    pub db_nanos: u64,
    /// Tree build, one proof, and the root / leaf checks.
    pub cpu_nanos: u64,
}

/// A migrated block that appended chunks to one ledger.
///
/// `start` is inclusive and `end` is exclusive, both ledger chunk offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LedgerSpan {
    /// Ledger that grew at this height.
    pub ledger: DataLedger,
    /// Migrated height.
    pub height: u64,
    /// Canonical hash at `height`.
    pub block_hash: H256,
    /// First ledger chunk offset introduced by this block.
    pub start: u64,
    /// Exclusive end (`total_chunks` after the block).
    pub end: u64,
}

/// Classify a consensus schema stamp without reading a tx header.
///
/// V3 and V4 are the readable layouts: V4 changes only the data cache tables,
/// not the tx header bytes. Older stamps still use `bundle_format` or an
/// inline `promoted_height`. Decoding those bytes with this binary panics.
pub fn tx_header_schema(version: Option<u32>) -> TxHeaderSchema {
    match version {
        Some(version @ (3 | 4)) => TxHeaderSchema {
            readable: true,
            message: format!(
                "schema V{version}: tx headers use metadata_format and decode with this binary"
            ),
        },
        Some(version) if version > 4 => TxHeaderSchema {
            readable: false,
            message: format!(
                "schema {version} is newer than V4. This binary refuses it. A read-only open does not migrate."
            ),
        },
        other => {
            let stamp = match other {
                Some(version) => version.to_string(),
                None => "absent".to_owned(),
            };
            TxHeaderSchema {
                readable: false,
                message: format!(
                    "schema {stamp} is older than V3. Tx header bytes use bundle_format: Option<u64> or an inline promoted_height, not metadata_format: u8. Decoding them with this binary panics or reads garbage. V2 to V3 rewrites every tx header. A read-only open does not migrate. Release 4.0.8 uses the same V3 layout and stamps it on startup, so a database last opened by 4.0.8 should already read. An older stamp was not opened by 4.0.8. Pass --migrate-copy only when the directory name contains \"copy\"; that opens read-write and runs ensure_db_version_compatible."
                ),
            }
        }
    }
}

/// Rebuild the tx path for the tx that owns `chunk_offset` in `ledger`.
///
/// Checks the recomputed root against the block header's ledger `tx_root`, and
/// checks the proof leaf against `fold_tx_root_leaf` of that tx. `chunk_size`
/// must be the chain chunk size: the ledger span counts chunks, and the tree
/// sizes leaves by `data_size` bytes.
pub fn derive_tx_path_at_offset(
    db: &DatabaseEnv,
    ledger: DataLedger,
    chunk_offset: u64,
    chunk_size: u64,
) -> eyre::Result<DerivedTxPath> {
    derive_tx_path_at_offset_with(db, ledger, chunk_offset, chunk_size, None)
}

/// [`derive_tx_path_at_offset`] with the tx-header reads spread over `pool`.
///
/// A cold tx-header read is a page fault on the memory map, so one thread pays
/// them one after another. Each pool thread reads its share in its own read
/// transaction so the device sees them in parallel. `None` reads serially.
pub fn derive_tx_path_at_offset_with(
    db: &DatabaseEnv,
    ledger: DataLedger,
    chunk_offset: u64,
    chunk_size: u64,
    pool: Option<&rayon::ThreadPool>,
) -> eyre::Result<DerivedTxPath> {
    let span = tracing::info_span!(
        "tx_path.derive",
        %ledger,
        chunk_offset,
        db_nanos = tracing::field::Empty,
        cpu_nanos = tracing::field::Empty,
    );
    let _entered = span.enter();
    let derived = derive_tx_path_at_offset_timed(db, ledger, chunk_offset, chunk_size, pool)?;
    span.record("db_nanos", derived.db_nanos);
    span.record("cpu_nanos", derived.cpu_nanos);
    Ok(derived)
}

fn load_bounds_and_header(
    db: &DatabaseEnv,
    ledger: DataLedger,
    chunk_offset: u64,
) -> eyre::Result<(BlockBounds, irys_types::IrysBlockHeader)> {
    db.view_eyre(|tx| {
        let bounds = locate_block(tx, ledger, chunk_offset)?;
        let header = block_header_by_hash(tx, &bounds.block_hash, false)?.ok_or(
            DeriveError::MissingBlockHeader {
                hash: bounds.block_hash,
                height: bounds.height,
            },
        )?;
        Ok((bounds, header))
    })
}

fn committed_tx_root(
    header: &irys_types::IrysBlockHeader,
    ledger: DataLedger,
) -> eyre::Result<H256> {
    header
        .data_ledgers
        .iter()
        .find(|item| item.ledger_id == ledger.get_id())
        .map(|item| item.tx_root)
        .ok_or_else(|| eyre!("ledger {ledger} absent on the block header"))
}

fn derive_tx_path_at_offset_timed(
    db: &DatabaseEnv,
    ledger: DataLedger,
    chunk_offset: u64,
    chunk_size: u64,
    pool: Option<&rayon::ThreadPool>,
) -> eyre::Result<DerivedTxPath> {
    eyre::ensure!(chunk_size > 0, "chunk size must be non-zero");
    let started = Instant::now();
    let cpu_nanos;
    let (bounds, header) = load_bounds_and_header(db, ledger, chunk_offset)?;
    let headers = match pool {
        None => db.view_eyre(|tx| block_ledger_tx_headers(tx, &header, ledger))?,
        Some(pool) => {
            let ids = header
                .data_ledgers
                .iter()
                .find(|item| item.ledger_id == ledger.get_id())
                .map(|item| item.tx_ids.0.clone())
                .unwrap_or_default();
            fetch_tx_headers(db, &ids, Some(pool))?
        }
    };
    let built = {
        let cpu_at = Instant::now();
        let (tx_index, tx_ledger_start) =
            assign_tx_index(&headers, &bounds, chunk_offset, chunk_size)?;
        let (tx_root, proof) = proof_for_headers(&headers, tx_index)?;
        let committed = committed_tx_root(&header, ledger)?;
        eyre::ensure!(
            tx_root == committed,
            "derived tx_root {tx_root} != header tx_root {committed}"
        );
        cpu_nanos = fit_nanos(cpu_at.elapsed())?;
        let chosen = &headers[tx_index];
        BuiltPath {
            height: bounds.height,
            block_hash: bounds.block_hash,
            ledger,
            tx_index,
            tx_id: chosen.id,
            tx_count: headers.len(),
            data_root: chosen.data_tree_root.0,
            tx_root,
            ledger_start: bounds.start,
            tx_ledger_start,
            tx_byte_start: tx_byte_start(&headers, tx_index),
            data_size: chosen.data_size,
            proof,
        }
    };
    finish(started, cpu_nanos, built)
}

/// One block's tx tree. A later offset in the same block skips the header read
/// and the tree build.
pub struct TxPathBlockCache {
    loaded: Option<LoadedBlock>,
}

struct LoadedBlock {
    ledger: DataLedger,
    bounds: BlockBounds,
    headers: Vec<DataTransactionHeader>,
    tx_root: H256,
    root: Node,
}

impl TxPathBlockCache {
    pub fn new() -> Self {
        Self { loaded: None }
    }
}

impl Default for TxPathBlockCache {
    fn default() -> Self {
        Self::new()
    }
}

/// [`derive_tx_path_at_offset`] that reuses `cache` for the same block.
pub fn derive_tx_path_at_offset_cached(
    db: &DatabaseEnv,
    cache: &mut TxPathBlockCache,
    ledger: DataLedger,
    chunk_offset: u64,
    chunk_size: u64,
) -> eyre::Result<DerivedTxPath> {
    eyre::ensure!(chunk_size > 0, "chunk size must be non-zero");
    let started = Instant::now();
    let hit = cache.loaded.as_ref().is_some_and(|block| {
        block.ledger == ledger
            && chunk_offset >= block.bounds.start
            && chunk_offset < block.bounds.end
    });
    if !hit {
        cache.loaded = Some(load_block(db, ledger, chunk_offset)?);
    }
    let block = cache
        .loaded
        .as_ref()
        .ok_or_else(|| eyre!("tx path block cache is empty"))?;
    let cpu_at = Instant::now();
    let (tx_index, tx_ledger_start) =
        assign_tx_index(&block.headers, &block.bounds, chunk_offset, chunk_size)?;
    let proof = resolve_proof_at(&block.root, tx_index)?;
    let chosen = &block.headers[tx_index];
    let leaf = proof_leaf_hash(&proof)?;
    let expected =
        DataTransactionLedger::fold_tx_root_leaf(chosen.data_tree_root.0, chosen.prefix_hash);
    eyre::ensure!(
        leaf == expected,
        "proof leaf {leaf} != fold of tx {}",
        chosen.id
    );
    let cpu_nanos = fit_nanos(cpu_at.elapsed())?;
    finish(
        started,
        cpu_nanos,
        BuiltPath {
            height: block.bounds.height,
            block_hash: block.bounds.block_hash,
            ledger,
            tx_index,
            tx_id: chosen.id,
            tx_count: block.headers.len(),
            data_root: chosen.data_tree_root.0,
            tx_root: block.tx_root,
            ledger_start: block.bounds.start,
            tx_ledger_start,
            tx_byte_start: tx_byte_start(&block.headers, tx_index),
            data_size: chosen.data_size,
            proof,
        },
    )
}

fn load_block(
    db: &DatabaseEnv,
    ledger: DataLedger,
    chunk_offset: u64,
) -> eyre::Result<LoadedBlock> {
    let (bounds, header) = load_bounds_and_header(db, ledger, chunk_offset)?;
    let headers = db.view_eyre(|tx| block_ledger_tx_headers(tx, &header, ledger))?;
    let (tx_root, root) = tree_for_headers(&headers)?;
    let committed = committed_tx_root(&header, ledger)?;
    eyre::ensure!(
        tx_root == committed,
        "derived tx_root {tx_root} != header tx_root {committed}"
    );
    Ok(LoadedBlock {
        ledger,
        bounds,
        headers,
        tx_root,
        root,
    })
}

/// The consensus database does not contain this offset yet.
///
/// [`consensus_is_behind`] matches this type. A plain report that uses the
/// same words is still a hard error.
#[derive(Debug)]
enum DeriveError {
    IndexEmpty,
    PastFrontier {
        chunk_offset: u64,
        ledger: DataLedger,
        anchor_max: u64,
        anchor: u64,
    },
    MissingBlockHeader {
        hash: H256,
        height: u64,
    },
}

impl std::fmt::Display for DeriveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IndexEmpty => formatter.write_str("block index is empty"),
            Self::PastFrontier {
                chunk_offset,
                ledger,
                anchor_max,
                anchor,
            } => write!(
                formatter,
                "offset {chunk_offset} is past ledger {ledger} frontier {anchor_max} at height {anchor}"
            ),
            Self::MissingBlockHeader { hash, height } => {
                write!(formatter, "block header {hash} missing at height {height}")
            }
        }
    }
}

impl std::error::Error for DeriveError {}

/// `true` when `err` means the consensus database does not contain this offset yet.
pub fn consensus_is_behind(err: &eyre::Report) -> bool {
    err.downcast_ref::<DeriveError>().is_some()
}

/// Chunk count of `ledger` at the latest migrated height.
///
/// An empty block index is an error. A ledger with no row at that height is
/// an error. An offset is covered only when it is strictly below this total,
/// the same bound [`locate_block`] uses.
#[cfg(feature = "rocksdb")]
pub(crate) fn ledger_frontier(db: &DatabaseEnv, ledger: DataLedger) -> eyre::Result<u64> {
    db.view_eyre(|tx| {
        let anchor = block_index_latest_height(tx)?.ok_or_else(|| eyre!("block index is empty"))?;
        let anchor_item = block_index_item_by_height(tx, &anchor)?;
        ledger_total(&anchor_item.ledgers, ledger)
            .ok_or_else(|| eyre!("ledger {ledger} is inactive at anchor height {anchor}"))
    })
}

/// Tx headers for `ids`, in order. With a pool the ids are cut into one
/// contiguous share per pool thread; each share is one read transaction.
pub fn fetch_tx_headers(
    db: &DatabaseEnv,
    ids: &[H256],
    pool: Option<&rayon::ThreadPool>,
) -> eyre::Result<Vec<DataTransactionHeader>> {
    let read_share = |share: &[H256]| -> eyre::Result<Vec<DataTransactionHeader>> {
        db.view_eyre(|tx| {
            share
                .iter()
                .map(|tx_id| {
                    tx_header_by_txid(tx, tx_id)?.ok_or_else(|| eyre!("tx header {tx_id} missing"))
                })
                .collect()
        })
    };
    let Some(pool) = pool.filter(|pool| pool.current_num_threads() > 1 && ids.len() > 1) else {
        return read_share(ids);
    };
    use rayon::prelude::*;
    let share = ids.len().div_ceil(pool.current_num_threads()).max(1);
    let parts: Vec<eyre::Result<Vec<DataTransactionHeader>>> =
        pool.install(|| ids.par_chunks(share).map(read_share).collect());
    let mut headers = Vec::with_capacity(ids.len());
    for part in parts {
        headers.extend(part?);
    }
    Ok(headers)
}

/// Read `tx_ids` and one block header, then build one proof.
///
/// The tx set is not the block's ledger, so the block `tx_root` is not checked.
/// The chosen leaf is still checked against that tx's fold. Repeated ids read
/// the same row again.
pub fn derive_tx_path_synthetic(
    db: &DatabaseEnv,
    tx_ids: &[H256],
    block_hash: &H256,
    leaf_index: usize,
) -> eyre::Result<DerivedTxPath> {
    derive_tx_path_synthetic_with(db, tx_ids, block_hash, leaf_index, None)
}

/// [`derive_tx_path_synthetic`] with the tx-header reads spread over `pool`.
pub fn derive_tx_path_synthetic_with(
    db: &DatabaseEnv,
    tx_ids: &[H256],
    block_hash: &H256,
    leaf_index: usize,
    pool: Option<&rayon::ThreadPool>,
) -> eyre::Result<DerivedTxPath> {
    eyre::ensure!(!tx_ids.is_empty(), "synthetic tx set is empty");
    let started = Instant::now();
    let cpu_nanos;
    let block = db
        .view_eyre(|tx| block_header_by_hash(tx, block_hash, false))?
        .ok_or_else(|| eyre!("block header {block_hash} missing"))?;
    let headers = fetch_tx_headers(db, tx_ids, pool)?;
    let built = {
        let cpu_at = Instant::now();
        let (tx_root, proof) = proof_for_headers(&headers, leaf_index)?;
        cpu_nanos = fit_nanos(cpu_at.elapsed())?;
        let chosen = &headers[leaf_index];
        BuiltPath {
            height: block.height,
            block_hash: *block_hash,
            ledger: DataLedger::Publish,
            tx_index: leaf_index,
            tx_id: chosen.id,
            tx_count: headers.len(),
            data_root: chosen.data_tree_root.0,
            tx_root,
            ledger_start: 0,
            tx_ledger_start: 0,
            tx_byte_start: tx_byte_start(&headers, leaf_index),
            data_size: chosen.data_size,
            proof,
        }
    };
    finish(started, cpu_nanos, built)
}

/// Spans of migrated blocks whose ledger `total_chunks` grew.
///
/// A height with no row for a ledger counts as total 0, matching block-bounds
/// search. A missing height, or a total that shrinks, is an error.
pub fn collect_data_spans(db: &DatabaseEnv) -> eyre::Result<Vec<LedgerSpan>> {
    db.view_eyre(|tx| {
        let mut hashes = Vec::new();
        {
            let mut cursor = tx.cursor_read::<MigratedBlockHashes>()?;
            for row in cursor.walk(None)? {
                hashes.push(row?);
            }
        }
        let mut rows_by_height: BTreeMap<u64, Vec<irys_types::LedgerIndexItem>> = BTreeMap::new();
        {
            let mut cursor = tx.cursor_dup_read::<IrysBlockIndexItems>()?;
            for row in cursor.walk(None)? {
                let (height, item) = row?;
                rows_by_height
                    .entry(height)
                    .or_default()
                    .push(irys_types::LedgerIndexItem {
                        total_chunks: item.total_chunks,
                        tx_root: item.tx_root,
                        ledger: item.ledger,
                    });
            }
        }
        spans_from_index(&hashes, &mut rows_by_height)
    })
}

/// Mining's one-offset path read: interval row, tx-path bytes, data-path bytes.
///
/// An offset with a tx-path interval but no data-path hash yields `(None, None)`,
/// because `path_hashes_in_inclusive_range` omits that offset.
pub fn read_stored_tx_data_path(
    index: &SubmoduleIndex,
    offset: PartitionChunkOffset,
) -> eyre::Result<(Option<TxPath>, Option<ChunkDataPath>)> {
    index.view(|read| {
        let rows = read.path_hashes_in_inclusive_range(offset, offset)?;
        let Some((_, hashes)) = rows.into_iter().next() else {
            return Ok((None, None));
        };
        let tx_path = match hashes.tx_path_hash {
            Some(path_hash) => read.get_full_tx_path(path_hash)?,
            None => None,
        };
        let data_path = read.get_data_path_by_offset(offset)?;
        Ok((tx_path, data_path))
    })
}

/// Proof bytes for one leaf. The root id is the ledger `tx_root`.
/// Byte where `headers[index]`'s leaf starts in the ledger `tx_root` tree.
fn tx_byte_start(headers: &[DataTransactionHeader], index: usize) -> u128 {
    headers[..index]
        .iter()
        .map(|header| u128::from(header.data_size))
        .sum()
}

fn tree_for_headers(headers: &[DataTransactionHeader]) -> eyre::Result<(H256, Node)> {
    eyre::ensure!(!headers.is_empty(), "ledger has no txs");
    let leaves = folded_leaves(headers)?;
    let nodes = generate_leaves_from_data_roots(&leaves)?;
    let root = generate_data_root(nodes)?;
    let tx_root = H256(root.id);
    Ok((tx_root, root))
}

fn proof_for_headers(
    headers: &[DataTransactionHeader],
    index: usize,
) -> eyre::Result<(H256, Proof)> {
    eyre::ensure!(
        index < headers.len(),
        "tx index {index} >= {}",
        headers.len()
    );
    let (tx_root, root) = tree_for_headers(headers)?;
    let proof = resolve_proof_at(&root, index)?;
    let leaf = proof_leaf_hash(&proof)?;
    let expected = DataTransactionLedger::fold_tx_root_leaf(
        headers[index].data_tree_root.0,
        headers[index].prefix_hash,
    );
    eyre::ensure!(
        leaf == expected,
        "proof leaf {leaf} != fold of tx {}",
        headers[index].id
    );
    Ok((tx_root, proof))
}

/// Encode the proof `resolve_proofs` would store at `index`.
///
/// Walks the existing tree and counts leaves under the left child. That count
/// is `O(N)` pointer follows and does not hash.
fn resolve_proof_at(node: &Node, index: usize) -> eyre::Result<Proof> {
    let mut proof = Proof {
        last_byte_index: 0,
        proof: Vec::new(),
    };
    descend(node, index, &mut proof)?;
    Ok(proof)
}

fn descend(node: &Node, index: usize, proof: &mut Proof) -> eyre::Result<()> {
    if node.left_child.is_none() && node.right_child.is_none() {
        let data_hash = node
            .data_hash
            .ok_or_else(|| eyre!("leaf missing data hash"))?;
        eyre::ensure!(index == 0, "leaf index {index}");
        proof.last_byte_index = node
            .max_byte_range
            .checked_sub(1)
            .ok_or_else(|| eyre!("leaf max_byte_range is 0"))?;
        proof.proof.extend(data_hash);
        proof.proof.extend(node.max_byte_range.to_note_vec());
        return Ok(());
    }
    let left = node
        .left_child
        .as_deref()
        .ok_or_else(|| eyre!("branch missing left child"))?;
    let right = node
        .right_child
        .as_deref()
        .ok_or_else(|| eyre!("branch missing right child"))?;
    proof.proof.extend(left.id);
    proof.proof.extend(right.id);
    proof.proof.extend(left.max_byte_range.to_note_vec());
    let left_leaves = leaf_count(left);
    if index < left_leaves {
        descend(left, index, proof)
    } else {
        descend(right, index - left_leaves, proof)
    }
}

fn leaf_count(node: &Node) -> usize {
    if let (Some(left), Some(right)) = (&node.left_child, &node.right_child) {
        leaf_count(left).saturating_add(leaf_count(right))
    } else {
        1
    }
}

fn proof_leaf_hash(proof: &Proof) -> eyre::Result<H256> {
    let leaf_len = HASH_SIZE.saturating_add(32);
    eyre::ensure!(proof.proof.len() >= leaf_len, "proof shorter than a leaf");
    let start = proof.proof.len() - leaf_len;
    let leaf = irys_types::LeafProof::try_from_proof_slice(&proof.proof[start..])?;
    let hash = leaf
        .hash()
        .ok_or_else(|| eyre!("leaf proof has no data hash"))?;
    Ok(H256(hash))
}

fn folded_leaves(headers: &[DataTransactionHeader]) -> eyre::Result<Vec<DataRootLeaf>> {
    let mut leaves = Vec::with_capacity(headers.len());
    for header in headers {
        let tx_size = usize::try_from(header.data_size)
            .map_err(|_| eyre!("data_size {} exceeds usize", header.data_size))?;
        leaves.push(DataRootLeaf {
            data_root: DataTransactionLedger::tx_root_leaf_value(header),
            tx_size,
        });
    }
    Ok(leaves)
}

#[derive(Clone, Copy)]
struct BlockBounds {
    height: u64,
    block_hash: H256,
    start: u64,
    end: u64,
}

/// Same search as `BlockIndex::block_bounds_at_height_in_tx`, anchored on the
/// latest migrated height in this read transaction.
fn locate_block<T: DbTx>(
    tx: &T,
    ledger: DataLedger,
    chunk_offset: u64,
) -> eyre::Result<BlockBounds> {
    let anchor = block_index_latest_height(tx)?.ok_or(DeriveError::IndexEmpty)?;
    let anchor_item = block_index_item_by_height(tx, &anchor)?;
    let anchor_max = ledger_total(&anchor_item.ledgers, ledger)
        .ok_or_else(|| eyre!("ledger {ledger} is inactive at anchor height {anchor}"))?;
    if chunk_offset >= anchor_max {
        return Err(DeriveError::PastFrontier {
            chunk_offset,
            ledger,
            anchor_max,
            anchor,
        }
        .into());
    }

    let mut lo = 0_u64;
    let mut hi = anchor;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let item = block_index_item_by_height(tx, &mid)?;
        let total = ledger_total(&item.ledgers, ledger).unwrap_or(0);
        if chunk_offset < total {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }

    let found = block_index_item_by_height(tx, &lo)?;
    let end = ledger_total(&found.ledgers, ledger)
        .ok_or_else(|| eyre!("ledger {ledger} missing at height {lo}"))?;
    let start = if lo == 0 {
        0
    } else {
        let previous = block_index_item_by_height(tx, &(lo - 1))?;
        ledger_total(&previous.ledgers, ledger).unwrap_or(0)
    };
    Ok(BlockBounds {
        height: lo,
        block_hash: found.block_hash,
        start,
        end,
    })
}

fn ledger_total(ledgers: &[irys_types::LedgerIndexItem], ledger: DataLedger) -> Option<u64> {
    ledgers
        .iter()
        .find(|item| item.ledger == ledger)
        .map(|item| item.total_chunks)
}

fn assign_tx_index(
    headers: &[DataTransactionHeader],
    bounds: &BlockBounds,
    chunk_offset: u64,
    chunk_size: u64,
) -> eyre::Result<(usize, u64)> {
    // Migration walks each tx's chunk count and does not require the sum to
    // equal the ledger span. A tail inside the span but past the last tx fails
    // below. An offset inside a tx resolves even when the two differ.
    bounds
        .end
        .checked_sub(bounds.start)
        .ok_or_else(|| eyre!("ledger span underflow at height {}", bounds.height))?;
    let mut cursor = bounds.start;
    for (index, header) in headers.iter().enumerate() {
        let chunks = header.data_size.div_ceil(chunk_size);
        let next = cursor
            .checked_add(chunks)
            .ok_or_else(|| eyre!("chunk cursor overflow"))?;
        if chunks > 0 && chunk_offset >= cursor && chunk_offset < next {
            return Ok((index, cursor));
        }
        cursor = next;
    }
    eyre::bail!(
        "offset {chunk_offset} is inside height {} but not inside a tx",
        bounds.height
    )
}

fn spans_from_index(
    hashes: &[(u64, H256)],
    rows_by_height: &mut BTreeMap<u64, Vec<irys_types::LedgerIndexItem>>,
) -> eyre::Result<Vec<LedgerSpan>> {
    if hashes.is_empty() {
        eyre::ensure!(
            rows_by_height.is_empty(),
            "block index rows exist without migrated hashes"
        );
        return Ok(Vec::new());
    }
    let mut prev = [0_u64; 4];
    let mut spans = Vec::new();
    for (ordinal, (height, hash)) in hashes.iter().enumerate() {
        let expected = u64::try_from(ordinal).map_err(|_| eyre!("height ordinal exceeds u64"))?;
        eyre::ensure!(
            *height == expected,
            "block index height gap: expected {expected}, found {height}"
        );
        let rows = rows_by_height.remove(height).unwrap_or_default();
        for (slot, ledger) in DataLedger::ALL.iter().copied().enumerate() {
            let total = ledger_total(&rows, ledger).unwrap_or(0);
            eyre::ensure!(
                total >= prev[slot],
                "ledger {ledger} total shrank at height {height}"
            );
            if total > prev[slot] {
                spans.push(LedgerSpan {
                    ledger,
                    height: *height,
                    block_hash: *hash,
                    start: prev[slot],
                    end: total,
                });
            }
            prev[slot] = total;
        }
    }
    eyre::ensure!(
        rows_by_height.is_empty(),
        "block index has heights with no migrated hash"
    );
    Ok(spans)
}

struct BuiltPath {
    height: u64,
    block_hash: H256,
    ledger: DataLedger,
    tx_index: usize,
    tx_id: H256,
    tx_count: usize,
    data_root: H256,
    tx_root: H256,
    ledger_start: u64,
    tx_ledger_start: u64,
    tx_byte_start: u128,
    data_size: u64,
    proof: Proof,
}

fn finish(started: Instant, cpu_nanos: u64, built: BuiltPath) -> eyre::Result<DerivedTxPath> {
    let total = started.elapsed().as_nanos();
    let db_nanos = total.saturating_sub(u128::from(cpu_nanos));
    Ok(DerivedTxPath {
        height: built.height,
        block_hash: built.block_hash,
        ledger: built.ledger,
        tx_index: built.tx_index,
        tx_id: built.tx_id,
        tx_count: built.tx_count,
        data_root: built.data_root,
        tx_root: built.tx_root,
        ledger_start: built.ledger_start,
        tx_ledger_start: built.tx_ledger_start,
        tx_byte_start: built.tx_byte_start,
        data_size: built.data_size,
        proof: built.proof,
        db_nanos: fit_u64(db_nanos)?,
        cpu_nanos,
    })
}

/// Identity the miner stored for this offset. Schema v2 leaves the last three empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacementCheck {
    pub data_root: H256,
    pub ledger: Option<u32>,
    pub block_height: Option<u64>,
    pub tx_index: Option<u32>,
}

/// The derived path must prove the same chunk the miner holds.
///
/// `placement` is required on RocksDB v1. A mismatch is an error the caller drops.
pub fn cross_check_derived(
    derived: &DerivedTxPath,
    ledger_offset: u64,
    chunk_size: u64,
    data_path: &[u8],
    placement: Option<PlacementCheck>,
) -> eyre::Result<()> {
    // The same checks as `poa_is_valid`: the tx leaf covers the tx's raw byte
    // range, and the data leaf covers exactly the chunk at this offset.
    let chunk_index = ledger_offset
        .checked_sub(derived.tx_ledger_start)
        .and_then(|index| u32::try_from(index).ok())
        .ok_or_else(|| eyre!("ledger offset {ledger_offset} is not inside the derived tx"))?;
    let chunk_range =
        expected_chunk_byte_range(TxChunkOffset(chunk_index), derived.data_size, chunk_size)
            .map_err(|error| {
                eyre!("derived tx path does not prove offset {ledger_offset}: {error}")
            })?;
    let tx_end = derived
        .tx_byte_start
        .checked_add(u128::from(derived.data_size))
        .ok_or_else(|| eyre!("derived tx byte range overflow"))?;
    let tx_path = validate_path(
        derived.tx_root.0,
        &Base64(derived.proof.proof.clone()),
        derived.tx_byte_start + u128::from(chunk_range.0),
    )
    .map_err(|error| eyre!("derived tx path does not prove offset {ledger_offset}: {error}"))?;
    if (tx_path.min_byte_range, tx_path.max_byte_range) != (derived.tx_byte_start, tx_end) {
        eyre::bail!("derived tx path range does not match offset {ledger_offset}");
    }
    let data = validate_path(
        derived.data_root.0,
        &Base64(data_path.to_vec()),
        u128::from(chunk_range.0),
    )
    .map_err(|error| {
        eyre!(
            "data_root mismatch: data path is not under derived data root {}: {error}",
            derived.data_root
        )
    })?;
    validate_path_byte_range(&data, chunk_range)
        .map_err(|error| eyre!("data path range does not match offset {ledger_offset}: {error}"))?;
    let Some(placement) = placement else {
        return Ok(());
    };
    let tx_index = u32::try_from(derived.tx_index).map_err(|_| eyre!("tx index exceeds u32"))?;
    if placement.data_root != derived.data_root
        || placement.block_height != Some(derived.height)
        || placement.tx_index != Some(tx_index)
        || placement.ledger != Some(derived.ledger.get_id())
    {
        eyre::bail!(
            "placement mismatch: derived data_root {} height {} tx_index {tx_index} ledger {}, placement data_root {} height {:?} tx_index {:?} ledger {:?}",
            derived.data_root,
            derived.height,
            derived.ledger.get_id(),
            placement.data_root,
            placement.block_height,
            placement.tx_index,
            placement.ledger
        );
    }
    Ok(())
}

fn fit_nanos(duration: Duration) -> eyre::Result<u64> {
    fit_u64(duration.as_nanos())
}

fn fit_u64(value: u128) -> eyre::Result<u64> {
    u64::try_from(value).map_err(|_| eyre!("nanoseconds exceed u64"))
}

/// Pool for the tx-header prefetch. `threads` below 1 is raised to 1.
/// [`fetch_tx_headers`] still reads serially when the pool has one thread.
pub fn tx_path_derive_pool(threads: usize) -> eyre::Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads.max(1))
        .thread_name(|index| format!("tx-path-{index}"))
        .build()
        .map_err(|error| eyre!("tx path derive pool: {error}"))
}

/// Tx-path hash migration stores for a proof. `SHA-256` of the proof bytes.
pub fn tx_path_hash(proof: &[u8]) -> H256 {
    H256::from(hash_sha256(proof))
}

#[cfg(test)]
mod tests {
    use super::{
        consensus_is_behind, derive_tx_path_at_offset, derive_tx_path_synthetic, proof_for_headers,
        read_stored_tx_data_path, resolve_proof_at, tx_header_schema, tx_path_hash,
    };
    use crate::db::IrysDatabaseExt as _;
    use crate::submodule::{SubmoduleIndex, SubmoduleStore as _};
    use crate::tables::IrysTables;
    use crate::{
        IrysDatabaseArgs as _, insert_block_header, insert_block_index_items_for_block,
        insert_tx_header, open_or_create_db,
    };
    use irys_testing_utils::utils::TempDirBuilder;
    use irys_types::DataTreeRoot;
    use irys_types::{
        Base64, DataLedger, DataTransactionHeader, DataTransactionLedger, H256, H256List,
        IrysBlockHeader, PartitionChunkOffset, Proof, TxChunkOffset, expected_chunk_byte_range,
        generate_data_root, generate_leaves_from_data_roots, resolve_proofs, validate_path,
    };
    use reth_db::mdbx::DatabaseArguments;

    fn hid(n: u64) -> H256 {
        let mut bytes = [0_u8; 32];
        bytes[..8].copy_from_slice(&n.to_be_bytes());
        H256::from(bytes)
    }

    fn tx_at(n: u64, size: u64) -> DataTransactionHeader {
        let mut header = DataTransactionHeader::default();
        header.id = hid(n);
        header.data_tree_root = DataTreeRoot(hid(n.saturating_add(100)));
        header.prefix_hash = hid(n.saturating_add(200));
        header.data_size = size;
        header
    }

    #[test]
    fn assign_tx_index_allows_a_chunk_sum_that_differs_from_the_span() {
        let chunk_size = 32;
        let headers = vec![tx_at(1, 32), tx_at(2, 32)];
        let bounds = super::BlockBounds {
            height: 1,
            block_hash: hid(1),
            start: 10,
            end: 14,
        };
        assert_eq!(
            super::assign_tx_index(&headers, &bounds, 10, chunk_size).expect("first"),
            (0, 10)
        );
        assert_eq!(
            super::assign_tx_index(&headers, &bounds, 11, chunk_size).expect("second"),
            (1, 11)
        );
        let gap = super::assign_tx_index(&headers, &bounds, 12, chunk_size)
            .expect_err("tail past the txs");
        assert!(gap.to_string().contains("not inside a tx"), "{gap}");
        assert!(
            !consensus_is_behind(&gap),
            "a span tail is a hard error: {gap}"
        );

        let short = super::BlockBounds {
            height: 1,
            block_hash: hid(1),
            start: 10,
            end: 11,
        };
        assert_eq!(
            super::assign_tx_index(&headers, &short, 10, chunk_size).expect("inside the span"),
            (0, 10)
        );
    }

    /// Frontier skip is a typed error. The same words in another report are not.
    #[test]
    fn consensus_behind_is_typed_not_a_substring() {
        let decoy =
            eyre::eyre!("block header storage inconsistent: the previous header is missing");
        assert!(
            !consensus_is_behind(&decoy),
            "an unrelated missing-header report is a hard error"
        );
        let words = eyre::eyre!("offset 9 is past ledger publish frontier 4 at height 2");
        assert!(
            !consensus_is_behind(&words),
            "those words in a plain report are not a frontier skip"
        );

        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open db");

        let empty =
            derive_tx_path_at_offset(&db, DataLedger::Publish, 0, 32).expect_err("empty index");
        assert!(
            consensus_is_behind(&empty),
            "an empty block index is behind: {empty}"
        );
        assert!(
            empty.to_string().contains("block index is empty"),
            "{empty}"
        );

        let block = block_with(0, &[], 4, &[], 0);
        let block_hash = block.block_hash;
        db.update_eyre(|tx| {
            insert_block_header(tx, &block)?;
            insert_block_index_items_for_block(tx, &block)?;
            Ok(())
        })
        .expect("write one block");

        let past = derive_tx_path_at_offset(&db, DataLedger::Publish, 5, 32)
            .expect_err("past the frontier");
        assert!(
            consensus_is_behind(&past),
            "an offset past the frontier is behind: {past}"
        );
        assert!(past.to_string().contains("is past ledger"), "{past}");

        let inactive = derive_tx_path_at_offset(&db, DataLedger::OneYear, 0, 32)
            .expect_err("ledger has no row");
        assert!(
            !consensus_is_behind(&inactive),
            "an inactive ledger is a hard error: {inactive}"
        );
        assert!(inactive.to_string().contains("inactive"), "{inactive}");

        db.update_eyre(|tx| {
            use crate::tables::IrysBlockHeaders;
            use reth_db::transaction::DbTxMut as _;
            tx.delete::<IrysBlockHeaders>(block_hash, None)?;
            Ok(())
        })
        .expect("drop the header");
        let missing = derive_tx_path_at_offset(&db, DataLedger::Publish, 0, 32)
            .expect_err("header row is gone");
        assert!(
            consensus_is_behind(&missing),
            "a missing block header is behind: {missing}"
        );
        assert!(missing.to_string().contains("block header"), "{missing}");
        assert!(missing.to_string().contains("missing"), "{missing}");
    }

    #[test]
    fn cross_check_accepts_the_miners_data_path_and_rejects_a_mismatch() {
        let chunk_size = 32_u64;
        let bytes = vec![7_u8; chunk_size as usize];
        let root = generate_data_root(
            irys_types::generate_leaves(std::iter::once(Ok(bytes)), chunk_size as usize)
                .expect("leaves"),
        )
        .expect("data root");
        let data_root = H256(root.id);
        let data_path = irys_types::resolve_proofs(root, None)
            .expect("proofs")
            .remove(0)
            .proof;
        let mut header = tx_at(1, chunk_size);
        header.data_tree_root = DataTreeRoot(data_root);
        let (tx_root, proof) =
            proof_for_headers(std::slice::from_ref(&header), 0).expect("tx path");
        let derived = super::DerivedTxPath {
            height: 4,
            block_hash: hid(4),
            ledger: DataLedger::Publish,
            tx_index: 0,
            tx_id: header.id,
            tx_count: 1,
            data_root,
            tx_root,
            ledger_start: 8,
            tx_ledger_start: 8,
            tx_byte_start: 0,
            data_size: chunk_size,
            proof,
            db_nanos: 0,
            cpu_nanos: 0,
        };
        let placement = super::PlacementCheck {
            data_root,
            ledger: Some(DataLedger::Publish.get_id()),
            block_height: Some(4),
            tx_index: Some(0),
        };
        super::cross_check_derived(&derived, 8, chunk_size, &data_path, Some(placement))
            .expect("matching path");

        let wrong =
            super::cross_check_derived(&derived, 8, chunk_size, &[1, 2, 3], Some(placement))
                .expect_err("bad data path");
        assert!(wrong.to_string().contains("data_root mismatch"), "{wrong}");

        let moved = super::PlacementCheck {
            tx_index: Some(1),
            ..placement
        };
        let mismatch = super::cross_check_derived(&derived, 8, chunk_size, &data_path, Some(moved))
            .expect_err("placement");
        assert!(
            mismatch.to_string().contains("placement mismatch"),
            "{mismatch}"
        );
    }

    fn headers_sized(count: usize) -> Vec<DataTransactionHeader> {
        (0..count)
            .map(|index| {
                let size = 1 + u64::try_from((index * 13) % 200).unwrap_or(1);
                tx_at(u64::try_from(index).unwrap_or(0).saturating_add(1), size)
            })
            .collect()
    }

    #[test]
    fn resolve_proof_at_matches_resolve_proofs() {
        for count in 1..=64 {
            let headers = headers_sized(count);
            let leaves = super::folded_leaves(&headers).expect("leaves");
            let nodes = generate_leaves_from_data_roots(&leaves).expect("nodes");
            let root = generate_data_root(nodes).expect("root");
            let all = resolve_proofs(root.clone(), None).expect("all proofs");
            assert_eq!(all.len(), count);
            for index in 0..count {
                let one = resolve_proof_at(&root, index).expect("one proof");
                assert_eq!(one, all[index], "count {count} index {index}");
            }
        }
    }

    /// Producer cross-check versus the validator's whole-chunk mapping.
    ///
    /// Every chunk of an honest layout passes both, however unaligned the txs
    /// before it. The proof bytes match `merklize_tx_root`, the addressed data
    /// chunk is the chunk `assign_tx_index` selected, and the miner prefix
    /// length equals the validator's chunk span.
    #[test]
    fn unaligned_offsets_agree_with_the_validator() {
        let chunk_size = 32_u64;
        let patterns: &[(&str, &[u64], u64, Option<&[bool]>)] = &[
            ("one-partial", &[40], 0, Some(&[true, true])),
            ("one-byte", &[1], 0, Some(&[true])),
            ("two-partial", &[40, 40], 0, Some(&[true, true, true, true])),
            (
                "three-odd",
                &[33, 33, 32],
                0,
                Some(&[true, true, true, true, true]),
            ),
            (
                "one-byte-then-chunk",
                &[1, 1, 32],
                0,
                Some(&[true, true, true]),
            ),
            ("mixed", &[32, 40, 32], 7, Some(&[true, true, true, true])),
            ("zero-middle", &[32, 0, 32], 0, Some(&[true, true])),
            ("five-leaves", &[40, 32, 1, 64, 33], 3, None),
            ("four-widths", &[10, 20, 32, 31], 10, None),
        ];
        for &(name, sizes, start, expected) in patterns {
            agree_on_pattern(name, sizes, start, chunk_size, expected);
        }
    }

    fn agree_on_pattern(
        name: &str,
        sizes: &[u64],
        start: u64,
        chunk_size: u64,
        expected: Option<&[bool]>,
    ) {
        let mut headers = Vec::with_capacity(sizes.len());
        let mut chunk_total = 0_u64;
        for (index, &size) in sizes.iter().enumerate() {
            let mut header = tx_at(u64::try_from(index).unwrap_or(0).saturating_add(1), size);
            if size > 0 {
                let (data_root, _) = data_tree(size, chunk_size);
                header.data_tree_root = DataTreeRoot(data_root);
            }
            chunk_total = chunk_total.saturating_add(size.div_ceil(chunk_size));
            headers.push(header);
        }
        let bounds = super::BlockBounds {
            height: 1,
            block_hash: hid(1),
            start,
            end: start.saturating_add(chunk_total),
        };
        let (merkle_root, merkle_proofs) = DataTransactionLedger::merklize_tx_root(&headers);
        let mut accepted = Vec::new();
        for offset in start..bounds.end {
            let (tx_index, tx_start) =
                super::assign_tx_index(&headers, &bounds, offset, chunk_size)
                    .unwrap_or_else(|error| panic!("{name} offset {offset}: {error}"));
            let (tx_root, proof) = proof_for_headers(&headers, tx_index)
                .unwrap_or_else(|error| panic!("{name} offset {offset}: {error}"));
            assert_eq!(tx_root, merkle_root, "{name} offset {offset} root");
            assert_eq!(
                proof.proof, merkle_proofs[tx_index].proof,
                "{name} offset {offset} proof bytes"
            );
            let local = usize::try_from(offset - tx_start).unwrap_or(usize::MAX);
            let size = headers[tx_index].data_size;
            let (data_root, data_proofs) = data_tree(size, chunk_size);
            assert_eq!(headers[tx_index].data_tree_root, DataTreeRoot(data_root));
            let data_path = &data_proofs[local].proof;
            let derived = derived_for(
                &headers, tx_index, &proof, tx_root, data_root, start, tx_start,
            );
            let cross = super::cross_check_derived(&derived, offset, chunk_size, data_path, None);
            // The validator: the tx's raw byte range in the tx_root tree, then
            // chunk `local` of that tx, each pinned to its exact range.
            let raw_start = super::tx_byte_start(&headers, tx_index);
            let raw_end = raw_start + u128::from(size);
            let data_byte = (offset - tx_start) * chunk_size;
            let chunk_range =
                expected_chunk_byte_range(TxChunkOffset::from(offset - tx_start), size, chunk_size)
                    .expect("chunk range");
            let validator = validate_path(
                tx_root.0,
                &Base64(proof.proof.clone()),
                raw_start + u128::from(data_byte),
            )
            .and_then(|tx_path| {
                eyre::ensure!(
                    (tx_path.min_byte_range, tx_path.max_byte_range) == (raw_start, raw_end),
                    "tx leaf range"
                );
                let data = validate_path(
                    data_root.0,
                    &Base64(data_path.clone()),
                    u128::from(data_byte),
                )?;
                irys_types::validate_path_byte_range(&data, chunk_range)?;
                Ok(data)
            });
            match (cross, validator) {
                (Ok(()), Ok(data)) => {
                    let end = usize::try_from(data.max_byte_range).unwrap_or(usize::MAX);
                    let rem = end % (chunk_size as usize);
                    let miner_span = if rem == 0 { chunk_size as usize } else { rem };
                    assert_eq!(
                        miner_span as u64,
                        chunk_range.1 - chunk_range.0,
                        "{name} offset {offset} miner span"
                    );
                    accepted.push(true);
                }
                (Err(cross_error), Err(_)) => {
                    let text = cross_error.to_string();
                    assert!(
                        text.contains("does not prove offset")
                            || text.contains("data_root mismatch")
                            || text.contains("range does not"),
                        "{name} offset {offset}: {text}"
                    );
                    accepted.push(false);
                }
                (Ok(()), Err(error)) => {
                    panic!("{name} offset {offset}: cross ok, validator {error}")
                }
                (Err(error), Ok(_)) => {
                    panic!("{name} offset {offset}: validator ok, cross {error}")
                }
            }
        }
        if let Some(expected) = expected {
            assert_eq!(accepted, expected, "{name}");
        }
    }

    fn data_tree(size: u64, chunk_size: u64) -> (H256, Vec<Proof>) {
        let bytes = vec![0x5A_u8; usize::try_from(size).unwrap_or(0)];
        let root = generate_data_root(
            irys_types::generate_leaves(
                std::iter::once(Ok(bytes)),
                usize::try_from(chunk_size).unwrap_or(1),
            )
            .expect("leaves"),
        )
        .expect("data root");
        let data_root = H256(root.id);
        let proofs = irys_types::resolve_proofs(root, None).expect("data proofs");
        (data_root, proofs)
    }

    fn derived_for(
        headers: &[DataTransactionHeader],
        tx_index: usize,
        proof: &Proof,
        tx_root: H256,
        data_root: H256,
        ledger_start: u64,
        tx_ledger_start: u64,
    ) -> super::DerivedTxPath {
        super::DerivedTxPath {
            height: 1,
            block_hash: hid(1),
            ledger: DataLedger::Publish,
            tx_index,
            tx_id: headers[tx_index].id,
            tx_count: headers.len(),
            data_root,
            tx_root,
            ledger_start,
            tx_ledger_start,
            tx_byte_start: super::tx_byte_start(headers, tx_index),
            data_size: headers[tx_index].data_size,
            proof: proof.clone(),
            db_nanos: 0,
            cpu_nanos: 0,
        }
    }

    fn set_ledger(
        block: &mut IrysBlockHeader,
        ledger: DataLedger,
        txs: &[DataTransactionHeader],
        total: u64,
    ) {
        let (root, _) = DataTransactionLedger::merklize_tx_root(txs);
        let slot = block
            .data_ledgers
            .iter_mut()
            .find(|item| item.ledger_id == ledger.get_id())
            .expect("mock header has the ledger");
        slot.tx_ids = H256List(txs.iter().map(|header| header.id).collect());
        slot.tx_root = root;
        slot.total_chunks = total;
    }

    fn block_with(
        height: u64,
        publish: &[DataTransactionHeader],
        publish_total: u64,
        submit: &[DataTransactionHeader],
        submit_total: u64,
    ) -> IrysBlockHeader {
        let mut block = IrysBlockHeader::new_mock_header();
        block.height = height;
        block.block_hash = hid(height.saturating_add(1));
        set_ledger(&mut block, DataLedger::Publish, publish, publish_total);
        set_ledger(&mut block, DataLedger::Submit, submit, submit_total);
        block
    }

    #[test]
    fn derive_matches_merklize_and_maps_offsets() {
        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open db");

        let publish_0 = tx_at(1, 32);
        let publish_1 = tx_at(2, 64);
        let publish_2 = tx_at(3, 32);
        let submit_0 = tx_at(4, 32);
        let publish_3 = tx_at(5, 32);
        let publish_h1 = [publish_0.clone(), publish_1.clone(), publish_2.clone()];
        let blocks = [
            block_with(0, &[], 0, &[], 0),
            block_with(1, &publish_h1, 4, std::slice::from_ref(&submit_0), 1),
            block_with(2, std::slice::from_ref(&publish_3), 5, &[], 1),
        ];
        db.update_eyre(|tx| {
            for header in [&publish_0, &publish_1, &publish_2, &submit_0, &publish_3] {
                insert_tx_header(tx, header)?;
            }
            for block in &blocks {
                insert_block_header(tx, block)?;
                insert_block_index_items_for_block(tx, block)?;
            }
            Ok(())
        })
        .expect("write chain");

        let spans = super::collect_data_spans(&db).expect("spans");
        assert!(spans.iter().any(|span| {
            span.ledger == DataLedger::Publish
                && span.height == 1
                && span.start == 0
                && span.end == 4
        }));
        assert!(spans.iter().any(|span| {
            span.ledger == DataLedger::Publish
                && span.height == 2
                && span.start == 4
                && span.end == 5
        }));
        assert!(spans.iter().any(|span| {
            span.ledger == DataLedger::Submit
                && span.height == 1
                && span.start == 0
                && span.end == 1
        }));

        let cases = [
            (DataLedger::Publish, 0, 1, 0),
            (DataLedger::Publish, 2, 1, 1),
            (DataLedger::Publish, 3, 1, 2),
            (DataLedger::Publish, 4, 2, 0),
            (DataLedger::Submit, 0, 1, 0),
        ];
        for (ledger, offset, height, tx_index) in cases {
            let derived = derive_tx_path_at_offset(&db, ledger, offset, 32).expect("derive");
            assert_eq!(derived.height, height, "offset {offset}");
            assert_eq!(derived.tx_index, tx_index, "offset {offset}");
            let txs: Vec<DataTransactionHeader> = if ledger == DataLedger::Submit {
                vec![submit_0.clone()]
            } else if height == 1 {
                publish_h1.to_vec()
            } else {
                vec![publish_3.clone()]
            };
            let (_, proofs) = DataTransactionLedger::merklize_tx_root(&txs);
            assert_eq!(derived.proof, proofs[tx_index]);
            assert_eq!(derived.tx_root, proofs_root(&txs));
        }

        let past = derive_tx_path_at_offset(&db, DataLedger::Publish, 5, 32);
        assert!(past.is_err());
        // A larger chunk size shortens the tx walk. Offset 0 is still the
        // first tx. The span tail, past that walk, is not a tx. The sum is
        // not required to equal the span.
        let still_first =
            derive_tx_path_at_offset(&db, DataLedger::Publish, 0, 64).expect("offset 0");
        assert_eq!(still_first.tx_index, 0);
        let tail = derive_tx_path_at_offset(&db, DataLedger::Publish, 3, 64);
        let message = tail.expect_err("span tail").to_string();
        assert!(message.contains("not inside a tx"), "{message}");
    }

    fn proofs_root(txs: &[DataTransactionHeader]) -> H256 {
        DataTransactionLedger::merklize_tx_root(txs).0
    }

    #[test]
    // MDBX-SUBMODULE-INDEX: delete this test. It stores a tx path in an MDBX submodule index. RocksDB v1 derives the path and does not store it.
    fn stored_tx_path_matches_merklize_bytes() {
        // These two writes are what migration commits for the tx path. The
        // data-path rows are what the body worker adds before mining's read
        // returns both paths.
        let headers = [tx_at(1, 32), tx_at(2, 64), tx_at(3, 32)];
        let (_, proofs) = DataTransactionLedger::merklize_tx_root(&headers);
        let (root, derived) = proof_for_headers(&headers, 1).expect("proof");
        assert_eq!(root, proofs_root(&headers));
        assert_eq!(derived, proofs[1]);

        let dir = TempDirBuilder::new().build();
        let index_dir = dir.path().join("submodule-db.copy");
        std::fs::create_dir_all(&index_dir).expect("index dir");
        let index = SubmoduleIndex::open_mdbx(
            &index_dir,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open index");
        let path_hash = tx_path_hash(&proofs[1].proof);
        let start = PartitionChunkOffset(1);
        let end = PartitionChunkOffset(2);
        index
            .update(|write| {
                write.add_full_tx_path(path_hash, proofs[1].proof.clone())?;
                write.add_tx_path_hash_to_offset_range(start, end, Some(path_hash))?;
                Ok(())
            })
            .expect("write tx path");

        let stored = index
            .view(|read| read.get_tx_path_by_offset(start))
            .expect("read")
            .expect("interval hit");
        assert_eq!(stored, proofs[1].proof);
        let missing = read_stored_tx_data_path(&index, start).expect("mining read");
        assert!(missing.0.is_none());
        assert!(missing.1.is_none());

        let data_path = vec![7_u8, 8, 9];
        let data_hash = H256::from(irys_types::hash_sha256(&data_path));
        index
            .update(|write| {
                write.add_full_data_path(start, data_path.clone())?;
                write.add_data_path_hash_to_offset_index(start, Some(data_hash))?;
                Ok(())
            })
            .expect("write data path");
        let (tx_path, data) = read_stored_tx_data_path(&index, start).expect("mining read");
        assert_eq!(tx_path.expect("tx path"), proofs[1].proof);
        assert_eq!(data.expect("data path"), data_path);
    }

    #[test]
    fn parallel_header_fetch_matches_serial_order() {
        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open db");
        let headers: Vec<_> = (1..=37).map(|id| tx_at(id, 32)).collect();
        db.update_eyre(|tx| {
            for header in &headers {
                insert_tx_header(tx, header)?;
            }
            Ok(())
        })
        .expect("write headers");
        let ids: Vec<_> = headers.iter().map(|header| header.id).collect();
        let serial = super::fetch_tx_headers(&db, &ids, None).expect("serial");
        for threads in [2, 4, 8, 64] {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .expect("pool");
            let parallel = super::fetch_tx_headers(&db, &ids, Some(&pool)).expect("parallel");
            assert_eq!(
                parallel.iter().map(|h| h.id).collect::<Vec<_>>(),
                serial.iter().map(|h| h.id).collect::<Vec<_>>(),
                "threads={threads}"
            );
        }
    }

    #[test]
    fn tx_header_schema_gates_v3() {
        for version in [None, Some(0), Some(1), Some(2)] {
            let schema = tx_header_schema(version);
            assert!(!schema.readable);
            assert!(
                schema.message.contains("metadata_format"),
                "{}",
                schema.message
            );
            assert!(
                schema.message.contains("bundle_format"),
                "{}",
                schema.message
            );
        }
        assert!(tx_header_schema(Some(3)).readable);
        let current = tx_header_schema(Some(4));
        assert!(current.readable);
        let newer = tx_header_schema(Some(5));
        assert!(!newer.readable);
        assert!(newer.message.contains("newer"));
    }

    #[test]
    fn synthetic_skips_block_tx_root() {
        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open db");
        let headers = [tx_at(1, 32), tx_at(2, 40), tx_at(3, 10)];
        let mut block = block_with(1, &[], 0, &[], 0);
        db.update_eyre(|tx| {
            for header in &headers {
                insert_tx_header(tx, header)?;
            }
            insert_block_header(tx, &block)?;
            insert_block_index_items_for_block(tx, &block)?;
            Ok(())
        })
        .expect("write");

        let ids = [headers[0].id, headers[1].id, headers[2].id];
        let derived = derive_tx_path_synthetic(&db, &ids, &block.block_hash, 1).expect("synthetic");
        let (_, proofs) = DataTransactionLedger::merklize_tx_root(&headers);
        assert_eq!(derived.proof, proofs[1]);
        assert_ne!(derived.tx_root, block.data_ledgers[0].tx_root);

        block.block_hash = hid(99);
        let missing = derive_tx_path_synthetic(&db, &ids, &block.block_hash, 0);
        assert!(missing.is_err());
    }

    #[test]
    // MDBX-SUBMODULE-INDEX: delete this test. It stores a tx path in an MDBX submodule index. The derive against the consensus database stays.
    fn derived_equals_stored_equals_merklize() {
        let headers = [tx_at(1, 32), tx_at(2, 64), tx_at(3, 32)];
        let (root, proofs) = DataTransactionLedger::merklize_tx_root(&headers);
        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open db");
        let blocks = [
            block_with(0, &[], 0, &[], 0),
            block_with(1, &headers, 4, &[], 0),
        ];
        db.update_eyre(|tx| {
            for header in &headers {
                insert_tx_header(tx, header)?;
            }
            for block in &blocks {
                insert_block_header(tx, block)?;
                insert_block_index_items_for_block(tx, block)?;
            }
            Ok(())
        })
        .expect("write chain");

        let index_dir = dir.path().join("submodule-db.copy");
        std::fs::create_dir_all(&index_dir).expect("index dir");
        let index = SubmoduleIndex::open_mdbx(
            &index_dir,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open index");
        let start = PartitionChunkOffset(1);
        let path_hash = tx_path_hash(&proofs[1].proof);
        let data_path = vec![7_u8, 8, 9];
        let data_hash = H256::from(irys_types::hash_sha256(&data_path));
        index
            .update(|write| {
                write.add_full_tx_path(path_hash, proofs[1].proof.clone())?;
                write.add_tx_path_hash_to_offset_range(start, start, Some(path_hash))?;
                write.add_full_data_path(start, data_path)?;
                write.add_data_path_hash_to_offset_index(start, Some(data_hash))?;
                Ok(())
            })
            .expect("write stored path");

        let derived = derive_tx_path_at_offset(&db, DataLedger::Publish, 1, 32).expect("derive");
        let (stored, _) = read_stored_tx_data_path(&index, start).expect("stored");
        assert_eq!(derived.tx_root, root);
        assert_eq!(derived.proof.proof, proofs[1].proof);
        assert_eq!(stored.expect("stored tx path"), proofs[1].proof);
    }

    #[test]
    fn derive_span_records_db_and_cpu() {
        let header = tx_at(1, 32);
        let dir = TempDirBuilder::new().build();
        let db = open_or_create_db(
            dir.path(),
            IrysTables::ALL,
            DatabaseArguments::irys_testing().expect("testing args"),
        )
        .expect("open db");
        let blocks = [
            block_with(0, &[], 0, &[], 0),
            block_with(1, std::slice::from_ref(&header), 1, &[], 0),
        ];
        db.update_eyre(|tx| {
            insert_tx_header(tx, &header)?;
            for block in &blocks {
                insert_block_header(tx, block)?;
                insert_block_index_items_for_block(tx, block)?;
            }
            Ok(())
        })
        .expect("write chain");

        let seen = SpanCapture::default();
        tracing::subscriber::with_default(seen.clone(), || {
            derive_tx_path_at_offset(&db, DataLedger::Publish, 0, 32).expect("derive");
        });
        let names = seen.names.lock().expect("names");
        assert!(
            names.iter().any(|name| name == "tx_path.derive"),
            "{names:?}"
        );
        assert!(seen.db_nanos.lock().expect("db").is_some());
        assert!(seen.cpu_nanos.lock().expect("cpu").is_some());
    }

    #[derive(Clone, Default)]
    struct SpanCapture {
        names: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        db_nanos: std::sync::Arc<std::sync::Mutex<Option<u64>>>,
        cpu_nanos: std::sync::Arc<std::sync::Mutex<Option<u64>>>,
    }

    impl tracing::Subscriber for SpanCapture {
        fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            self.names
                .lock()
                .expect("names")
                .push(span.metadata().name().to_owned());
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
            values.record(&mut FieldVisit {
                db_nanos: &self.db_nanos,
                cpu_nanos: &self.cpu_nanos,
            });
        }

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, _event: &tracing::Event<'_>) {}

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    struct FieldVisit<'a> {
        db_nanos: &'a std::sync::Mutex<Option<u64>>,
        cpu_nanos: &'a std::sync::Mutex<Option<u64>>,
    }

    impl tracing::field::Visit for FieldVisit<'_> {
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            match field.name() {
                "db_nanos" => *self.db_nanos.lock().expect("db") = Some(value),
                "cpu_nanos" => *self.cpu_nanos.lock().expect("cpu") = Some(value),
                _ => {}
            }
        }

        fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
            if let Ok(value) = u64::try_from(value) {
                self.record_u64(field, value);
            }
        }

        fn record_debug(&mut self, _field: &tracing::field::Field, _value: &dyn std::fmt::Debug) {}
    }
}
