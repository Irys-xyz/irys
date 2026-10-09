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
    ChunkDataPath, DataLedger, DataRootLeaf, DataTransactionHeader, DataTransactionLedger, H256,
    HASH_SIZE, Helpers as _, Node, PartitionChunkOffset, Proof, ProofDeserialize as _, TxPath,
    generate_data_root, generate_leaves_from_data_roots, hash_sha256,
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
/// V3 is the only readable layout. Older stamps still use `bundle_format` or an
/// inline `promoted_height`. Decoding those bytes with this binary panics.
pub fn tx_header_schema(version: Option<u32>) -> TxHeaderSchema {
    match version {
        Some(3) => TxHeaderSchema {
            readable: true,
            message: "schema V3: tx headers use metadata_format and decode with this binary"
                .to_owned(),
        },
        Some(version) if version > 3 => TxHeaderSchema {
            readable: false,
            message: format!(
                "schema {version} is newer than V3. This binary refuses it. A read-only open does not migrate."
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
    let (bounds, header) = db.view_eyre(|tx| {
        let bounds = locate_block(tx, ledger, chunk_offset)?;
        let header = block_header_by_hash(tx, &bounds.block_hash, false)?.ok_or_else(|| {
            eyre!(
                "block header {} missing at height {}",
                bounds.block_hash,
                bounds.height
            )
        })?;
        Ok((bounds, header))
    })?;
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
        let tx_index = assign_tx_index(&headers, &bounds, chunk_offset, chunk_size)?;
        let (tx_root, proof) = proof_for_headers(&headers, tx_index)?;
        let committed = header
            .data_ledgers
            .iter()
            .find(|item| item.ledger_id == ledger.get_id())
            .map(|item| item.tx_root)
            .ok_or_else(|| eyre!("ledger {ledger} absent on the block header"))?;
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
            data_root: chosen.data_root,
            tx_root,
            proof,
        }
    };
    finish(started, cpu_nanos, built)
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
            data_root: chosen.data_root,
            tx_root,
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
fn proof_for_headers(
    headers: &[DataTransactionHeader],
    index: usize,
) -> eyre::Result<(H256, Proof)> {
    eyre::ensure!(!headers.is_empty(), "ledger has no txs");
    eyre::ensure!(
        index < headers.len(),
        "tx index {index} >= {}",
        headers.len()
    );
    let leaves = folded_leaves(headers)?;
    let nodes = generate_leaves_from_data_roots(&leaves)?;
    let root = generate_data_root(nodes)?;
    let tx_root = H256(root.id);
    let proof = resolve_proof_at(&root, index)?;
    let leaf = proof_leaf_hash(&proof)?;
    let expected = DataTransactionLedger::fold_tx_root_leaf(
        headers[index].data_root,
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
    let anchor = block_index_latest_height(tx)?.ok_or_else(|| eyre!("block index is empty"))?;
    let anchor_item = block_index_item_by_height(tx, &anchor)?;
    let anchor_max = ledger_total(&anchor_item.ledgers, ledger)
        .ok_or_else(|| eyre!("ledger {ledger} is inactive at anchor height {anchor}"))?;
    if chunk_offset >= anchor_max {
        eyre::bail!(
            "offset {chunk_offset} is past ledger {ledger} frontier {anchor_max} at height {anchor}"
        );
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
) -> eyre::Result<usize> {
    let span = bounds
        .end
        .checked_sub(bounds.start)
        .ok_or_else(|| eyre!("ledger span underflow at height {}", bounds.height))?;
    let sum = chunk_sum(headers, chunk_size)?;
    eyre::ensure!(
        sum == span,
        "chunk sum {sum} != ledger span {span}; pass the chain chunk size"
    );
    let mut cursor = bounds.start;
    for (index, header) in headers.iter().enumerate() {
        let chunks = header.data_size.div_ceil(chunk_size);
        let next = cursor
            .checked_add(chunks)
            .ok_or_else(|| eyre!("chunk cursor overflow"))?;
        if chunks > 0 && chunk_offset >= cursor && chunk_offset < next {
            return Ok(index);
        }
        cursor = next;
    }
    eyre::bail!(
        "offset {chunk_offset} is inside height {} but not inside a tx",
        bounds.height
    )
}

fn chunk_sum(headers: &[DataTransactionHeader], chunk_size: u64) -> eyre::Result<u64> {
    let mut sum = 0_u64;
    for header in headers {
        sum = sum
            .checked_add(header.data_size.div_ceil(chunk_size))
            .ok_or_else(|| eyre!("chunk sum overflow"))?;
    }
    Ok(sum)
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
        proof: built.proof,
        db_nanos: fit_u64(db_nanos)?,
        cpu_nanos,
    })
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
        derive_tx_path_at_offset, derive_tx_path_synthetic, proof_for_headers,
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
    use irys_types::{
        DataLedger, DataTransactionHeader, DataTransactionLedger, H256, H256List, IrysBlockHeader,
        PartitionChunkOffset, generate_data_root, generate_leaves_from_data_roots, resolve_proofs,
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
        header.data_root = hid(n.saturating_add(100));
        header.prefix_hash = hid(n.saturating_add(200));
        header.data_size = size;
        header
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
        let wrong_size = derive_tx_path_at_offset(&db, DataLedger::Publish, 0, 64);
        let message = wrong_size.expect_err("wrong chunk size").to_string();
        assert!(message.contains("chunk sum"), "{message}");
    }

    fn proofs_root(txs: &[DataTransactionHeader]) -> H256 {
        DataTransactionLedger::merklize_tx_root(txs).0
    }

    #[test]
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
        let current = tx_header_schema(Some(3));
        assert!(current.readable);
        let newer = tx_header_schema(Some(4));
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
