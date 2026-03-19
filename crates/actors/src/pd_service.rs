pub mod cache;
pub(crate) mod fetch;
pub mod provisioning;

use cache::{ChunkCache, ChunkKey};
use dashmap::DashSet;
use futures::{FutureExt as _, StreamExt as _};
use irys_domain::{BlockTreeReadGuard, PeerList, block_index_guard::BlockIndexReadGuard};
use irys_types::app_state::DatabaseProvider;
use irys_types::chunk_provider::{
    ChunkStorageProvider, PdChunkFetcher, PdChunkMessage, PdChunkReceiver,
};
use irys_types::range_specifier::ChunkRangeSpecifier;
use irys_types::{DataLedger, IrysAddress, PeerAddress, TokioServiceHandle};
use provisioning::{ProvisioningState, ProvisioningTracker};
use reth::revm::primitives::B256;
use reth::revm::primitives::bytes::Bytes;
use reth::tasks::shutdown::Shutdown;
use reth_db::Database as _;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use tokio::task::JoinSet;
use tokio_util::time::DelayQueue;
use tracing::{Instrument as _, debug, info, trace, warn};

/// Information derived from MDBX state needed to fully verify a fetched chunk.
#[derive(Debug)]
struct ChunkVerificationInfo {
    expected_data_root: irys_types::H256,
    data_size: u64,
    /// Zero-based chunk index within the transaction.
    tx_chunk_offset: u64,
}

/// The PD (Programmable Data) Service manages chunk provisioning for PD transactions.
///
/// It handles the full lifecycle:
/// 1. New PD transaction → fetch required chunks from storage into LRU cache
/// 2. Payload building → check readiness via shared `ready_pd_txs` set
/// 3. Transaction removal → release references, let LRU evict unused chunks
pub struct PdService {
    shutdown: Shutdown,
    msg_rx: PdChunkReceiver,
    cache: ChunkCache,
    tracker: ProvisioningTracker,
    storage_provider: Arc<dyn ChunkStorageProvider>,
    block_tracker: HashMap<B256, Vec<ChunkKey>>,
    /// Shared set of ready PD tx hashes. Written on provision/release.
    ready_pd_txs: Arc<DashSet<B256>>,

    // -- P2P chunk fetch infrastructure --
    /// Active fetch tasks for chunks being retrieved from peers.
    join_set: JoinSet<fetch::PdChunkFetchResult>,
    /// Timer-based queue for scheduling fetch retries with exponential backoff.
    retry_queue: DelayQueue<fetch::RetryEntry>,
    /// Per-chunk-key fetch state tracking (in-flight, backoff, attempts, etc.).
    pending_fetches: HashMap<ChunkKey, fetch::PdChunkFetchState>,
    /// Blocks waiting for P2P-fetched chunks before validation can proceed.
    pending_blocks: HashMap<B256, fetch::PendingBlockProvision>,
    /// Fetches PD chunks from remote peers (trait object bridging irys-p2p).
    chunk_fetcher: Arc<dyn PdChunkFetcher>,
    /// Shared peer list for discovering chunk sources.
    peer_list: PeerList,
    /// Read-only view of the block tree (fork-choice state).
    block_tree: BlockTreeReadGuard,
    /// Read-only view of the confirmed block index.
    block_index: BlockIndexReadGuard,
    /// Database handle for looking up data transaction headers / chunk metadata.
    db: DatabaseProvider,
    /// Number of chunks per partition (from consensus config).
    num_chunks_in_partition: u64,
    /// This node's miner address, used to identify self in peer lists.
    own_miner_address: IrysAddress,
    /// Monotonically increasing counter used to stamp each new fetch state.
    /// Allows stale retry callbacks to be discarded when a key is re-fetched.
    next_generation: u64,
}

impl PdService {
    /// Spawn the PD service as a tokio task.
    #[expect(clippy::too_many_arguments)]
    pub fn spawn_service(
        msg_rx: PdChunkReceiver,
        storage_provider: Arc<dyn ChunkStorageProvider>,
        runtime_handle: tokio::runtime::Handle,
        chunk_data_index: irys_types::chunk_provider::ChunkDataIndex,
        ready_pd_txs: Arc<DashSet<B256>>,
        peer_list: PeerList,
        chunk_fetcher: Arc<dyn PdChunkFetcher>,
        block_tree: BlockTreeReadGuard,
        block_index: BlockIndexReadGuard,
        db: DatabaseProvider,
        num_chunks_in_partition: u64,
        own_miner_address: IrysAddress,
    ) -> TokioServiceHandle {
        let (shutdown_signal, shutdown) = reth::tasks::shutdown::signal();

        let service = Self {
            shutdown,
            msg_rx,
            cache: ChunkCache::with_default_capacity(chunk_data_index),
            tracker: ProvisioningTracker::new(),
            storage_provider,
            block_tracker: HashMap::new(),
            ready_pd_txs,
            join_set: JoinSet::new(),
            retry_queue: DelayQueue::new(),
            pending_fetches: HashMap::new(),
            pending_blocks: HashMap::new(),
            chunk_fetcher,
            peer_list,
            block_tree,
            block_index,
            db,
            num_chunks_in_partition,
            own_miner_address,
            next_generation: 0,
        };

        let join_handle = runtime_handle.spawn(
            async move {
                service.start().await;
            }
            .in_current_span(),
        );

        TokioServiceHandle {
            name: "pd_service".to_string(),
            handle: join_handle,
            shutdown_signal,
        }
    }

    async fn start(mut self) {
        info!("PdService started");

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    info!("PdService received shutdown signal");
                    break;
                }

                msg = self.msg_rx.recv() => {
                    match msg {
                        Some(message) => self.handle_message(message),
                        None => { break; }
                    }
                }

                Some(result) = self.join_set.join_next() => {
                    self.on_fetch_done(result);
                }

                Some(expired) = self.retry_queue.next(), if !self.retry_queue.is_empty() => {
                    self.on_retry_ready(expired.into_inner());
                }
            }
        }

        info!("PdService stopped");
    }

    fn on_fetch_done(&mut self, result: Result<fetch::PdChunkFetchResult, tokio::task::JoinError>) {
        debug!("on_fetch_done called");
        let fetch_result = match result {
            Ok(r) => {
                debug!(
                    "on_fetch_done: key=({}, {}), success={}",
                    r.key.ledger,
                    r.key.offset,
                    r.result.is_ok()
                );
                r
            }
            Err(join_error) => {
                warn!(
                    "PD chunk fetch task panicked or was cancelled: {}",
                    join_error
                );
                return;
            }
        };

        let key = fetch_result.key;
        let serving_peer = fetch_result.serving_peer;

        match fetch_result.result {
            Ok(chunk_format) => self.on_fetch_success(key, chunk_format, serving_peer),
            Err(fetch::PdChunkFetchError::AllPeersFailed { failed_peers }) => {
                self.on_fetch_all_peers_failed(key, failed_peers);
            }
        }
    }

    /// Handle a successfully fetched chunk: unpack, verify, cache, and notify waiters.
    fn on_fetch_success(
        &mut self,
        key: ChunkKey,
        chunk_format: irys_types::ChunkFormat,
        serving_peer: Option<SocketAddr>,
    ) {
        debug!(
            "on_fetch_success: key=({}, {}), format={}",
            key.ledger,
            key.offset,
            match &chunk_format {
                irys_types::ChunkFormat::Packed(_) => "Packed",
                irys_types::ChunkFormat::Unpacked(_) => "Unpacked",
            }
        );
        // 1. Extract unpacked bytes, data_root, data_path, and data_size from the fetched chunk.
        let config = self.storage_provider.config();
        let (unpacked_bytes, chunk_data_root, data_path, chunk_data_size) = match chunk_format {
            irys_types::ChunkFormat::Unpacked(unpacked) => {
                let data_root = unpacked.data_root;
                let data_path = unpacked.data_path.clone();
                let data_size = unpacked.data_size;
                (unpacked.bytes.0, data_root, data_path, data_size)
            }
            irys_types::ChunkFormat::Packed(packed) => {
                let data_path = packed.data_path.clone();
                let data_size = packed.data_size;
                let unpacked = irys_packing::unpack(
                    &packed,
                    config.entropy_packing_iterations,
                    config.chunk_size as usize,
                    config.chain_id,
                );
                let data_root = unpacked.data_root;
                (unpacked.bytes.0, data_root, data_path, data_size)
            }
        };

        // 2. Derive chunk verification info from trusted MDBX state.
        let info = match self.derive_chunk_verification_info(&key) {
            Ok(info) => info,
            Err(e) => {
                warn!(
                    ?key,
                    error = %e,
                    "Could not derive chunk verification info — rejecting unverifiable chunk \
                     (PD only accesses migrated blocks, so this indicates missing MDBX state)"
                );
                self.fail_pending_fetch(&key);
                return;
            }
        };

        if chunk_data_root != info.expected_data_root {
            warn!(
                ?key,
                ?chunk_data_root,
                expected_data_root = ?info.expected_data_root,
                ?serving_peer,
                "Fetched chunk data_root mismatch — excluding peer and retrying"
            );
            let failed: Vec<SocketAddr> = serving_peer.into_iter().collect();
            self.on_fetch_all_peers_failed(key, failed);
            return;
        }
        debug!("data_root verified for ({}, {})", key.ledger, key.offset);

        // 3. Verify Merkle proof: data_path must be valid under data_root at the expected
        //    byte position. We use `info.data_size` from MDBX (trusted state) for the byte
        //    offset calculation — NOT `chunk_data_size` from the peer.
        let _ = chunk_data_size; // consumed above, kept to silence unused-variable warnings
        let chunk_size = config.chunk_size;
        let num_chunks = info.data_size.div_ceil(chunk_size);
        let last_chunk_offset = num_chunks.saturating_sub(1);
        let target_byte_offset: u128 = if info.tx_chunk_offset == last_chunk_offset {
            u128::from(info.data_size).saturating_sub(1)
        } else {
            u128::from(info.tx_chunk_offset + 1) * u128::from(chunk_size) - 1
        };

        match irys_types::validate_path(info.expected_data_root.0, &data_path, target_byte_offset) {
            Ok(path_result) => {
                // 4. Verify leaf hash: sha256(chunk_bytes) must match the Merkle leaf.
                let computed_hash = irys_types::hash_sha256(&unpacked_bytes);
                if path_result.leaf_hash != computed_hash {
                    warn!(
                        ?key,
                        ?serving_peer,
                        "Fetched chunk leaf hash mismatch — data_path valid but bytes \
                         don't match leaf"
                    );
                    let failed: Vec<SocketAddr> = serving_peer.into_iter().collect();
                    self.on_fetch_all_peers_failed(key, failed);
                    return;
                }
                debug!(
                    "Merkle proof and leaf hash verified for ({}, {})",
                    key.ledger, key.offset
                );
            }
            Err(e) => {
                warn!(
                    ?key,
                    ?serving_peer,
                    error = %e,
                    "Fetched chunk has invalid Merkle proof (data_path)"
                );
                let failed: Vec<SocketAddr> = serving_peer.into_iter().collect();
                self.on_fetch_all_peers_failed(key, failed);
                return;
            }
        }

        // 5. Remove the fetch state and collect all waiters.
        let Some(state) = self.pending_fetches.remove(&key) else {
            // Stale fetch result — the key was already cleaned up.
            debug!(?key, "Fetch completed but no pending state found (stale)");
            return;
        };

        // Cancel any pending retry timer if one exists.
        if let Some(retry_key) = state.retry_queue_key {
            self.retry_queue.try_remove(&retry_key);
        }

        let waiting_blocks: Vec<B256> = state.waiting_blocks.into_iter().collect();
        let waiting_txs: Vec<B256> = state.waiting_txs.into_iter().collect();

        // 6. Insert into cache with per-waiter references.
        let data = Arc::new(Bytes::from(unpacked_bytes));
        let all_waiters: Vec<B256> = waiting_blocks
            .iter()
            .chain(waiting_txs.iter())
            .copied()
            .collect();

        debug!(
            "on_fetch_success: inserting into cache, {} waiters (blocks={}, txs={})",
            all_waiters.len(),
            waiting_blocks.len(),
            waiting_txs.len()
        );
        if let Some((&first, rest)) = all_waiters.split_first() {
            self.cache.insert(key, data, first);
            for &waiter in rest {
                self.cache.add_reference(&key, waiter);
            }
        }

        // 7. Notify waiting blocks.
        for block_hash in &waiting_blocks {
            if let Some(pending) = self.pending_blocks.get_mut(block_hash) {
                pending.remaining_keys.remove(&key);
                if pending.remaining_keys.is_empty() {
                    let pending = self.pending_blocks.remove(block_hash).unwrap();
                    if pending.response.send(Ok(())).is_ok() {
                        self.block_tracker.insert(*block_hash, pending.all_keys);
                    } else {
                        // Receiver dropped (validation cancelled) — roll back cache references
                        for key in &pending.all_keys {
                            self.cache.remove_reference(key, block_hash);
                        }
                        self.cache.try_shrink_to_fit();
                    }
                }
            }
        }

        // 8. Notify waiting transactions.
        for tx_hash in &waiting_txs {
            if let Some(tx_state) = self.tracker.get_mut(tx_hash) {
                tx_state.missing_chunks.remove(&key);
                if tx_state.missing_chunks.is_empty() {
                    tx_state.state = provisioning::ProvisioningState::Ready;
                    self.ready_pd_txs.insert(*tx_hash);
                    debug!(tx_hash = %tx_hash, "PD transaction fully provisioned via P2P fetch");
                } else {
                    let found = tx_state.required_chunks.len() - tx_state.missing_chunks.len();
                    let total = tx_state.required_chunks.len();
                    tx_state.state =
                        provisioning::ProvisioningState::PartiallyReady { found, total };
                }
            }
        }

        trace!(
            ?key,
            waiting_blocks = waiting_blocks.len(),
            waiting_txs = waiting_txs.len(),
            "Fetch completed and waiters notified"
        );
    }

    /// Handle fetch failure when all peers failed — schedule retry or fail permanently.
    fn on_fetch_all_peers_failed(&mut self, key: ChunkKey, failed_peers: Vec<SocketAddr>) {
        let Some(state) = self.pending_fetches.get_mut(&key) else {
            return;
        };

        // If no one is waiting anymore, just clean up.
        if state.waiting_txs.is_empty() && state.waiting_blocks.is_empty() {
            self.pending_fetches.remove(&key);
            return;
        }

        // Clear the abort handle since the task has completed.
        state.abort_handle = None;

        // Accumulate failed peers into the excluded set.
        state.excluded_peers.extend(failed_peers.iter().copied());

        if state.attempt >= fetch::MAX_CHUNK_FETCH_RETRIES {
            warn!(
                ?key,
                attempts = state.attempt,
                "PD chunk fetch exhausted all retries — failing permanently"
            );
            self.fail_pending_fetch(&key);
            return;
        }

        let next_attempt = state.attempt + 1;
        let delay = fetch::backoff_duration(state.attempt);
        let retry_entry = fetch::RetryEntry {
            key,
            generation: state.generation,
            excluded_peers: state.excluded_peers.clone(),
        };
        let queue_key = self.retry_queue.insert(retry_entry, delay);

        state.attempt = next_attempt;
        state.status = fetch::FetchPhase::Backoff;
        state.retry_queue_key = Some(queue_key);

        debug!(
            ?key,
            attempt = next_attempt,
            delay_ms = delay.as_millis(),
            "Scheduling PD chunk fetch retry"
        );
    }

    /// Permanently fail a pending fetch: remove state, error-respond to waiting blocks,
    /// and update waiting tx provisioning states.
    fn fail_pending_fetch(&mut self, key: &ChunkKey) {
        let Some(state) = self.pending_fetches.remove(key) else {
            return;
        };

        // Cancel any pending retry timer.
        if let Some(retry_key) = state.retry_queue_key {
            self.retry_queue.try_remove(&retry_key);
        }

        // Fail waiting blocks.
        for block_hash in &state.waiting_blocks {
            if let Some(pending) = self.pending_blocks.remove(block_hash) {
                // Respond with the missing chunk as an error.
                let _ = pending.response.send(Err(vec![(key.ledger, key.offset)]));
                // Clean up any cache references that were already added for this block.
                for k in &pending.all_keys {
                    let unreferenced = self.cache.remove_reference(k, block_hash);
                    if unreferenced {
                        self.cache.remove(k);
                    }
                }
                // Detach this block from other pending fetches so that later
                // successes don't add a permanent cache reference under a
                // block that will never be released.
                for k in &pending.all_keys {
                    if let Some(fetch_state) = self.pending_fetches.get_mut(k) {
                        fetch_state.waiting_blocks.remove(block_hash);
                    }
                    self.cancel_fetch_if_no_waiters(k);
                }
            }
        }

        // Update waiting transactions — the chunk remains missing permanently.
        // The tx stays in PartiallyReady state; it won't become Ready.
        for tx_hash in &state.waiting_txs {
            if let Some(tx_state) = self.tracker.get_mut(tx_hash) {
                let found = tx_state.required_chunks.len() - tx_state.missing_chunks.len();
                let total = tx_state.required_chunks.len();
                tx_state.state = provisioning::ProvisioningState::PartiallyReady { found, total };
            }
        }

        self.cache.try_shrink_to_fit();
    }

    /// Derive chunk verification info (expected `data_root`, `data_size`, and
    /// transaction-relative chunk offset) for a chunk at the given ledger offset
    /// by walking the block index and transaction headers in MDBX.
    fn derive_chunk_verification_info(
        &self,
        key: &ChunkKey,
    ) -> eyre::Result<ChunkVerificationInfo> {
        let block_index = self.block_index.read();
        let bounds = block_index.get_block_bounds(
            DataLedger::Publish,
            irys_types::LedgerChunkOffset::from(key.offset),
        )?;

        // Get the block hash from the block index item at this height.
        let block_index_item = block_index
            .get_item(bounds.height)
            .ok_or_else(|| eyre::eyre!("Block index item not found at height {}", bounds.height))?;

        let db_tx = self.db.tx()?;
        let block_header =
            irys_database::block_header_by_hash(&db_tx, &block_index_item.block_hash, false)?
                .ok_or_else(|| {
                    eyre::eyre!(
                        "Block header not found for hash {}",
                        block_index_item.block_hash
                    )
                })?;

        let tx_ids = block_header
            .get_data_ledger_tx_ids_ordered(DataLedger::Publish)
            .unwrap_or(&[]);

        let chunk_size = self.storage_provider.config().chunk_size;
        let mut running_offset = bounds.start_chunk_offset;
        for tx_id in tx_ids {
            let tx_header = irys_database::tx_header_by_txid(&db_tx, tx_id)?
                .ok_or_else(|| eyre::eyre!("Tx header not found: {}", tx_id))?;
            let num_chunks = tx_header.data_size.div_ceil(chunk_size);
            if key.offset < running_offset + num_chunks {
                return Ok(ChunkVerificationInfo {
                    expected_data_root: tx_header.data_root,
                    data_size: tx_header.data_size,
                    tx_chunk_offset: key.offset - running_offset,
                });
            }
            running_offset += num_chunks;
        }

        Err(eyre::eyre!(
            "Chunk offset {} not found in block txs at height {}",
            key.offset,
            bounds.height
        ))
    }

    fn on_retry_ready(&mut self, entry: fetch::RetryEntry) {
        let Some(state) = self.pending_fetches.get_mut(&entry.key) else {
            return;
        };

        // Stale generation — this retry belongs to an old provisioning lifecycle
        if state.generation != entry.generation {
            return;
        }

        // The retry timer entry was already consumed by the select! arm that
        // called us. Clear the stale key immediately so that any downstream
        // call to fail_pending_fetch doesn't try to double-remove it.
        state.retry_queue_key = None;

        // Waiters vanished during backoff
        if state.waiting_txs.is_empty() && state.waiting_blocks.is_empty() {
            self.pending_fetches.remove(&entry.key);
            return;
        }

        // Re-resolve peers, excluding those that already failed
        let mut peers = self.resolve_peers_for_chunk(&entry.key, &entry.excluded_peers);
        if peers.is_empty() {
            // All peers excluded — fall back to unfiltered resolution
            peers = self.resolve_peers_for_chunk(&entry.key, &HashSet::new());
            if peers.is_empty() {
                self.fail_pending_fetch(&entry.key);
                return;
            }
        }

        let key = entry.key;
        let fetcher = self.chunk_fetcher.clone();
        let abort_handle = self.join_set.spawn(async move {
            match AssertUnwindSafe(async {
                match fetcher.fetch_chunk(&peers, key.ledger, key.offset).await {
                    Ok(success) => fetch::PdChunkFetchResult {
                        key,
                        serving_peer: Some(success.serving_peer),
                        result: Ok(success.chunk),
                    },
                    Err(failure) => fetch::PdChunkFetchResult {
                        key,
                        serving_peer: None,
                        result: Err(fetch::PdChunkFetchError::AllPeersFailed {
                            failed_peers: failure.failed_peers,
                        }),
                    },
                }
            })
            .catch_unwind()
            .await
            {
                Ok(result) => result,
                Err(_panic) => fetch::PdChunkFetchResult {
                    key,
                    serving_peer: None,
                    result: Err(fetch::PdChunkFetchError::AllPeersFailed {
                        failed_peers: vec![],
                    }),
                },
            }
        });

        let state = self
            .pending_fetches
            .get_mut(&entry.key)
            .expect("entry confirmed present above");
        state.status = fetch::FetchPhase::Fetching;
        state.abort_handle = Some(abort_handle);
        state.retry_queue_key = None;
    }

    /// Resolves which peers store the chunk at the given key's (ledger, offset).
    /// Uses partition assignments from the canonical epoch snapshot.
    ///
    /// The slot index is derived from `offset / num_chunks_in_partition`, which
    /// identifies which partition slot in the ledger covers this chunk. We then
    /// iterate over all assigned data partitions looking for matching (ledger_id,
    /// slot_index) assignments that belong to other miners.
    fn resolve_peers_for_chunk(
        &self,
        key: &ChunkKey,
        exclude: &HashSet<SocketAddr>,
    ) -> Vec<PeerAddress> {
        let slot_index = key.offset / self.num_chunks_in_partition;
        // PD is Publish-ledger-only by design — see CLAUDE.md
        let publish_ledger_id: u32 = DataLedger::Publish.into();

        let tree = self.block_tree.read();
        let epoch_snapshot = tree.canonical_epoch_snapshot();
        let assignments = &epoch_snapshot.partition_assignments.data_partitions;

        debug!(
            "resolve_peers_for_chunk: key=({}, {}), slot_index={}, num_assignments={}, own_miner={:?}",
            key.ledger,
            key.offset,
            slot_index,
            assignments.len(),
            self.own_miner_address,
        );

        let mut peers = Vec::new();
        for (hash, assignment) in assignments.iter() {
            trace!(
                "  assignment: hash={}, ledger_id={:?}, slot_index={:?}, miner={:?}",
                hash, assignment.ledger_id, assignment.slot_index, assignment.miner_address,
            );
            if assignment.ledger_id == Some(publish_ledger_id)
                && assignment.slot_index == Some(slot_index as usize)
                && assignment.miner_address != self.own_miner_address
                && let Some(peer) = self
                    .peer_list
                    .peer_by_mining_address(&assignment.miner_address)
                && !exclude.contains(&peer.address.api)
            {
                debug!("  -> matched! peer api={}", peer.address.api);
                peers.push(peer.address);
            }
        }
        debug!(
            "resolve_peers_for_chunk: found {} peers for ({}, {})",
            peers.len(),
            key.ledger,
            key.offset,
        );
        peers
    }

    fn handle_message(&mut self, msg: PdChunkMessage) {
        match msg {
            PdChunkMessage::NewTransaction {
                tx_hash,
                chunk_specs,
            } => {
                self.handle_provision_chunks(tx_hash, chunk_specs);
            }
            PdChunkMessage::TransactionRemoved { tx_hash } => {
                self.handle_release_chunks(&tx_hash);
            }
            PdChunkMessage::ProvisionBlockChunks {
                block_hash,
                chunk_specs,
                response,
            } => {
                self.handle_provision_block_chunks(block_hash, chunk_specs, response);
            }
            PdChunkMessage::ReleaseBlockChunks { block_hash } => {
                self.handle_release_block_chunks(&block_hash);
            }
        }
    }

    /// Convert chunk range specifiers to chunk keys using checked arithmetic.
    fn specs_to_keys(&self, chunk_specs: &[ChunkRangeSpecifier]) -> HashSet<ChunkKey> {
        let config = self.storage_provider.config();
        let mut keys = HashSet::new();

        for spec in chunk_specs {
            let partition_index: u64 = match spec.partition_index.try_into() {
                Ok(v) => v,
                Err(_) => {
                    warn!(
                        partition_index = %spec.partition_index,
                        "Partition index exceeds u64::MAX, skipping spec"
                    );
                    continue;
                }
            };

            let base_offset = match config.num_chunks_in_partition.checked_mul(partition_index) {
                Some(v) => v,
                None => {
                    warn!(
                        num_chunks_in_partition = config.num_chunks_in_partition,
                        partition_index, "Base offset overflow, skipping spec"
                    );
                    continue;
                }
            };

            for i in 0..spec.chunk_count {
                let ledger_offset = base_offset
                    .checked_add(spec.offset as u64)
                    .and_then(|v| v.checked_add(i as u64));

                match ledger_offset {
                    Some(offset) => {
                        // PD is Publish-ledger-only by design — see CLAUDE.md
                        keys.insert(ChunkKey { ledger: 0, offset });
                    }
                    None => {
                        warn!(
                            base_offset,
                            spec_offset = spec.offset,
                            chunk_index = i,
                            "Ledger offset overflow, skipping chunk"
                        );
                    }
                }
            }
        }
        keys
    }

    /// Provision chunks for a new PD transaction.
    fn handle_provision_chunks(&mut self, tx_hash: B256, chunk_specs: Vec<ChunkRangeSpecifier>) {
        debug!(
            "handle_provision_chunks: tx_hash={}, specs={:?}",
            tx_hash,
            chunk_specs.len()
        );
        // Guard against duplicate NewTransaction messages — don't regress an already-tracked tx.
        if self.tracker.get(&tx_hash).is_some() {
            debug!(tx_hash = %tx_hash, "PD transaction already registered, skipping");
            return;
        }

        let required_chunks = self.specs_to_keys(&chunk_specs);
        let total_chunks = required_chunks.len();

        debug!(
            tx_hash = %tx_hash,
            total_chunks,
            "Starting PD chunk provisioning"
        );

        // Register the transaction (borrows self.tracker mutably, so we drop it before the loop).
        let _ = self.tracker.register(tx_hash, required_chunks.clone());

        // Fetch missing chunks — try cache first, then local storage.
        let mut fetched = 0;
        let mut missing = HashSet::new();

        for key in &required_chunks {
            if self.cache.contains(key) {
                // Already cached — just add reference
                self.cache.add_reference(key, tx_hash);
                fetched += 1;
                trace!(
                    tx_hash = %tx_hash,
                    ledger = key.ledger,
                    offset = key.offset,
                    "Chunk already cached, added reference"
                );
            } else {
                // Fetch from storage
                match self
                    .storage_provider
                    .get_unpacked_chunk_by_ledger_offset(key.ledger, key.offset)
                {
                    Ok(Some(chunk)) => {
                        self.cache.insert(*key, Arc::new(chunk), tx_hash);
                        fetched += 1;
                        trace!(
                            tx_hash = %tx_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            fetched,
                            total = total_chunks,
                            "Fetched and cached chunk"
                        );
                    }
                    Ok(None) | Err(_) => {
                        debug!(
                            tx_hash = %tx_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            "Chunk not found locally — spawning P2P fetch"
                        );
                        let key = *key;
                        missing.insert(key);

                        if let Some(state) = self.pending_fetches.get_mut(&key) {
                            // Already being fetched — just register this tx as a waiter
                            state.waiting_txs.insert(tx_hash);
                        } else {
                            // Spawn a new fetch task
                            let peers = self.resolve_peers_for_chunk(&key, &HashSet::new());
                            let fetcher = self.chunk_fetcher.clone();
                            let abort_handle = self.join_set.spawn(async move {
                                match AssertUnwindSafe(async {
                                    match fetcher.fetch_chunk(&peers, key.ledger, key.offset).await
                                    {
                                        Ok(success) => fetch::PdChunkFetchResult {
                                            key,
                                            serving_peer: Some(success.serving_peer),
                                            result: Ok(success.chunk),
                                        },
                                        Err(failure) => fetch::PdChunkFetchResult {
                                            key,
                                            serving_peer: None,
                                            result: Err(fetch::PdChunkFetchError::AllPeersFailed {
                                                failed_peers: failure.failed_peers,
                                            }),
                                        },
                                    }
                                })
                                .catch_unwind()
                                .await
                                {
                                    Ok(result) => result,
                                    Err(_panic) => fetch::PdChunkFetchResult {
                                        key,
                                        serving_peer: None,
                                        result: Err(fetch::PdChunkFetchError::AllPeersFailed {
                                            failed_peers: vec![],
                                        }),
                                    },
                                }
                            });
                            let generation = self.next_generation;
                            self.next_generation += 1;
                            self.pending_fetches.insert(
                                key,
                                fetch::PdChunkFetchState {
                                    waiting_txs: HashSet::from([tx_hash]),
                                    waiting_blocks: HashSet::new(),
                                    attempt: 0,
                                    generation,
                                    excluded_peers: HashSet::new(),
                                    status: fetch::FetchPhase::Fetching,
                                    abort_handle: Some(abort_handle),
                                    retry_queue_key: None,
                                },
                            );
                        }
                    }
                }
            }
        }

        // Update state based on what we found — re-borrow the tracker now that the loop is done.
        if let Some(tx_state) = self.tracker.get_mut(&tx_hash) {
            tx_state.missing_chunks = missing;
            if tx_state.missing_chunks.is_empty() {
                tx_state.state = ProvisioningState::Ready;
                self.ready_pd_txs.insert(tx_hash);
            } else {
                tx_state.state = ProvisioningState::PartiallyReady {
                    found: fetched,
                    total: total_chunks,
                };
            }
        }

        debug!(
            tx_hash = %tx_hash,
            fetched,
            total = total_chunks,
            cached_chunks = self.cache.len(),
            "PD chunk provisioning complete"
        );
    }

    /// Checks if a chunk key has no remaining waiters and cancels the fetch if so.
    /// Called after removing a tx or block from a fetch state's waiter sets.
    fn cancel_fetch_if_no_waiters(&mut self, key: &ChunkKey) {
        let Some(state) = self.pending_fetches.get(key) else {
            return;
        };
        if !state.waiting_txs.is_empty() || !state.waiting_blocks.is_empty() {
            return;
        }

        let state = self.pending_fetches.remove(key).unwrap();
        match state.status {
            fetch::FetchPhase::Fetching => {
                if let Some(handle) = state.abort_handle {
                    handle.abort();
                }
            }
            fetch::FetchPhase::Backoff => {
                if let Some(queue_key) = state.retry_queue_key {
                    self.retry_queue.try_remove(&queue_key);
                }
            }
        }
    }

    /// Release chunks when a transaction is removed from the mempool.
    fn handle_release_chunks(&mut self, tx_hash: &B256) {
        self.ready_pd_txs.remove(tx_hash);

        if let Some(tx_state) = self.tracker.remove(tx_hash) {
            let mut evicted = 0;
            for key in &tx_state.required_chunks {
                let unreferenced = self.cache.remove_reference(key, tx_hash);
                if unreferenced {
                    self.cache.remove(key);
                    evicted += 1;
                }
            }

            // Cancel pending fetches that were only waiting on this tx.
            let keys: Vec<ChunkKey> = tx_state.required_chunks.into_iter().collect();
            for key in &keys {
                if let Some(state) = self.pending_fetches.get_mut(key) {
                    state.waiting_txs.remove(tx_hash);
                }
                self.cancel_fetch_if_no_waiters(key);
            }

            trace!(
                tx_hash = %tx_hash,
                evicted_chunks = evicted,
                remaining_cached = self.cache.len(),
                "PD transaction removed, references decremented"
            );
            self.cache.try_shrink_to_fit();
        }
    }

    /// Provision chunks needed for validating a peer block.
    /// Loads chunks from local storage into cache, pins them with block_hash as reference.
    /// If chunks are missing locally, spawns P2P fetch tasks and holds the oneshot open
    /// until all chunks arrive (or permanently fail).
    fn handle_provision_block_chunks(
        &mut self,
        block_hash: B256,
        chunk_specs: Vec<ChunkRangeSpecifier>,
        response: tokio::sync::oneshot::Sender<Result<(), Vec<(u32, u64)>>>,
    ) {
        let required_chunks = self.specs_to_keys(&chunk_specs);
        let chunk_keys: Vec<ChunkKey> = required_chunks.into_iter().collect();

        debug!(
            block_hash = %block_hash,
            total_chunks = chunk_keys.len(),
            "Provisioning PD chunks for block validation"
        );

        let mut missing_keys = Vec::new();

        for key in &chunk_keys {
            if self.cache.contains(key) {
                self.cache.add_reference(key, block_hash);
            } else {
                match self
                    .storage_provider
                    .get_unpacked_chunk_by_ledger_offset(key.ledger, key.offset)
                {
                    Ok(Some(chunk)) => {
                        self.cache.insert(*key, Arc::new(chunk), block_hash);
                    }
                    Ok(None) => {
                        warn!(
                            block_hash = %block_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            "Chunk not found locally for block validation"
                        );
                        missing_keys.push(*key);
                    }
                    Err(e) => {
                        warn!(
                            block_hash = %block_hash,
                            ledger = key.ledger,
                            offset = key.offset,
                            error = %e,
                            "Failed to fetch chunk from storage for block validation"
                        );
                        missing_keys.push(*key);
                    }
                }
            }
        }

        if missing_keys.is_empty() {
            // All chunks found locally — respond immediately and track for later release.
            self.block_tracker.insert(block_hash, chunk_keys);
            if response.send(Ok(())).is_err() {
                // Receiver was dropped — the validation task was cancelled before
                // it could create a PdBlockGuard. Clean up immediately to prevent
                // chunks from being pinned forever.
                warn!(
                    block_hash = %block_hash,
                    "Provision response receiver dropped (task cancelled), releasing chunks"
                );
                if let Some(keys) = self.block_tracker.remove(&block_hash) {
                    for key in &keys {
                        let unreferenced = self.cache.remove_reference(key, &block_hash);
                        if unreferenced {
                            self.cache.remove(key);
                        }
                    }
                    self.cache.try_shrink_to_fit();
                }
            }
        } else {
            // Some chunks missing — hold the oneshot and spawn P2P fetch tasks.

            // Guard against duplicate block_hash — don't overwrite an existing oneshot.
            if self.pending_blocks.contains_key(&block_hash) {
                // Duplicate — first request is still in-flight, don't touch its refs
                let _ = response.send(Err(vec![]));
                return;
            }

            let all_keys = chunk_keys;
            let remaining_keys: HashSet<_> = missing_keys.iter().copied().collect();

            self.pending_blocks.insert(
                block_hash,
                fetch::PendingBlockProvision {
                    remaining_keys,
                    all_keys,
                    response,
                },
            );

            // For each missing key, register in pending_fetches and spawn fetch task.
            for key in &missing_keys {
                if let Some(state) = self.pending_fetches.get_mut(key) {
                    // Already being fetched — just add this block as a waiter.
                    state.waiting_blocks.insert(block_hash);
                } else {
                    let peers = self.resolve_peers_for_chunk(key, &HashSet::new());
                    let fetch_key = *key;
                    let fetcher = self.chunk_fetcher.clone();
                    let abort_handle = self.join_set.spawn(async move {
                        match AssertUnwindSafe(async {
                            match fetcher
                                .fetch_chunk(&peers, fetch_key.ledger, fetch_key.offset)
                                .await
                            {
                                Ok(success) => fetch::PdChunkFetchResult {
                                    key: fetch_key,
                                    serving_peer: Some(success.serving_peer),
                                    result: Ok(success.chunk),
                                },
                                Err(failure) => fetch::PdChunkFetchResult {
                                    key: fetch_key,
                                    serving_peer: None,
                                    result: Err(fetch::PdChunkFetchError::AllPeersFailed {
                                        failed_peers: failure.failed_peers,
                                    }),
                                },
                            }
                        })
                        .catch_unwind()
                        .await
                        {
                            Ok(result) => result,
                            Err(_panic) => fetch::PdChunkFetchResult {
                                key: fetch_key,
                                serving_peer: None,
                                result: Err(fetch::PdChunkFetchError::AllPeersFailed {
                                    failed_peers: vec![],
                                }),
                            },
                        }
                    });
                    let generation = self.next_generation;
                    self.next_generation += 1;
                    self.pending_fetches.insert(
                        *key,
                        fetch::PdChunkFetchState {
                            waiting_txs: HashSet::new(),
                            waiting_blocks: HashSet::from([block_hash]),
                            attempt: 0,
                            generation,
                            excluded_peers: HashSet::new(),
                            status: fetch::FetchPhase::Fetching,
                            abort_handle: Some(abort_handle),
                            retry_queue_key: None,
                        },
                    );
                }
            }

            debug!(
                block_hash = %block_hash,
                missing_chunks = missing_keys.len(),
                "Spawned P2P fetch tasks for missing block validation chunks"
            );
        }

        debug!(
            block_hash = %block_hash,
            cached_chunks = self.cache.len(),
            "Block chunk provisioning complete"
        );
    }

    /// Release chunks provisioned for a block after validation completes.
    fn handle_release_block_chunks(&mut self, block_hash: &B256) {
        if let Some(chunk_keys) = self.block_tracker.remove(block_hash) {
            for key in &chunk_keys {
                let unreferenced = self.cache.remove_reference(key, block_hash);
                if unreferenced {
                    self.cache.remove(key);
                }
            }

            // Cancel pending fetches that were only waiting on this block.
            // (Handles the case where the oneshot is dropped due to validation cancellation.)
            for key in &chunk_keys {
                if let Some(state) = self.pending_fetches.get_mut(key) {
                    state.waiting_blocks.remove(block_hash);
                }
                self.cancel_fetch_if_no_waiters(key);
            }

            trace!(
                block_hash = %block_hash,
                released_keys = chunk_keys.len(),
                "Released block validation chunks"
            );
            self.cache.try_shrink_to_fit();
        }

        // Also cancel pending fetches tracked via pending_blocks (chunks that haven't
        // arrived yet won't be in block_tracker, only in pending_blocks).
        if let Some(pending) = self.pending_blocks.remove(block_hash) {
            let keys = pending.all_keys;
            for key in &keys {
                // Remove cache references that were already added for this block.
                let unreferenced = self.cache.remove_reference(key, block_hash);
                if unreferenced {
                    self.cache.remove(key);
                }
                // Remove this block from the fetch's waiter set and cancel if empty.
                if let Some(state) = self.pending_fetches.get_mut(key) {
                    state.waiting_blocks.remove(block_hash);
                }
                self.cancel_fetch_if_no_waiters(key);
            }
            self.cache.try_shrink_to_fit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;
    use irys_database::{open_or_create_db, tables::IrysTables};
    use irys_domain::{BlockIndex, BlockTree};
    use irys_testing_utils::IrysBlockHeaderTestExt as _;
    use irys_types::range_specifier::ChunkRangeSpecifier;
    use irys_types::{ConsensusConfig, IrysBlockHeader};
    use std::sync::RwLock;
    use tokio::sync::{mpsc, oneshot};

    #[derive(Debug, Clone)]
    struct MockChunkProvider {
        config: irys_types::chunk_provider::ChunkConfig,
        cached_chunk: reth::revm::primitives::bytes::Bytes,
    }

    impl MockChunkProvider {
        fn new() -> Self {
            let config = irys_types::chunk_provider::ChunkConfig {
                num_chunks_in_partition: 100,
                chunk_size: 256_000,
                entropy_packing_iterations: 0,
                chain_id: 1,
            };
            let cached_chunk = reth::revm::primitives::bytes::Bytes::from(vec![0_u8; 256_000]);
            Self {
                config,
                cached_chunk,
            }
        }
    }

    /// Mock fetcher that always fails — tests use local storage so P2P fetch is never needed.
    struct MockPdChunkFetcher;

    #[async_trait::async_trait]
    impl irys_types::chunk_provider::PdChunkFetcher for MockPdChunkFetcher {
        async fn fetch_chunk(
            &self,
            _peers: &[PeerAddress],
            _ledger: u32,
            _offset: u64,
        ) -> Result<
            irys_types::chunk_provider::PdChunkFetchSuccess,
            irys_types::chunk_provider::PdChunkFetchFailure,
        > {
            Err(irys_types::chunk_provider::PdChunkFetchFailure {
                message: "mock fetcher always fails".to_string(),
                failed_peers: vec![],
            })
        }
    }

    impl irys_types::chunk_provider::ChunkStorageProvider for MockChunkProvider {
        fn get_unpacked_chunk_by_ledger_offset(
            &self,
            _ledger: u32,
            _ledger_offset: u64,
        ) -> eyre::Result<Option<reth::revm::primitives::bytes::Bytes>> {
            Ok(Some(self.cached_chunk.clone()))
        }

        fn get_chunk_for_pd(
            &self,
            _ledger: u32,
            _ledger_offset: u64,
        ) -> eyre::Result<Option<irys_types::ChunkFormat>> {
            Ok(None)
        }

        fn config(&self) -> irys_types::chunk_provider::ChunkConfig {
            self.config
        }
    }

    /// Create a PdService for testing with a mock provider.
    /// Returns the service and a TempDir that must be kept alive for the DB.
    fn test_service() -> (PdService, tempfile::TempDir) {
        let (_tx, rx) = mpsc::unbounded_channel();
        let provider: Arc<dyn ChunkStorageProvider> = Arc::new(MockChunkProvider::new());
        let (_, shutdown) = reth::tasks::shutdown::signal();
        let chunk_data_index: irys_types::chunk_provider::ChunkDataIndex = Arc::new(DashMap::new());
        let ready_pd_txs = Arc::new(DashSet::new());

        // Create test DB and domain objects for the P2P fetch infrastructure
        let tmp_dir = tempfile::tempdir().unwrap();
        let db_env = open_or_create_db(tmp_dir.path(), IrysTables::ALL, None).unwrap();
        let db = DatabaseProvider(Arc::new(db_env));

        let mut genesis = IrysBlockHeader::new_mock_header();
        genesis.height = 0;
        genesis.poa.chunk = Some(Default::default());
        genesis.test_sign();
        let block_tree = BlockTreeReadGuard::new(Arc::new(RwLock::new(BlockTree::new(
            &genesis,
            ConsensusConfig::testing(),
        ))));
        let block_index = BlockIndexReadGuard::new(BlockIndex::new_for_testing(db.clone()));
        let peer_list = PeerList::test_mock().expect("failed to create test peer list");

        let service = PdService {
            shutdown,
            msg_rx: rx,
            cache: ChunkCache::with_default_capacity(chunk_data_index),
            tracker: ProvisioningTracker::new(),
            storage_provider: provider,
            block_tracker: HashMap::new(),
            ready_pd_txs,
            join_set: JoinSet::new(),
            retry_queue: DelayQueue::new(),
            pending_fetches: HashMap::new(),
            pending_blocks: HashMap::new(),
            chunk_fetcher: Arc::new(MockPdChunkFetcher),
            peer_list,
            block_tree,
            block_index,
            db,
            num_chunks_in_partition: 100,
            own_miner_address: IrysAddress::default(),
            next_generation: 0,
        };
        (service, tmp_dir)
    }

    #[test]
    fn test_provision_block_chunks_loads_into_cache() {
        let (mut service, _tmp) = test_service();
        let block_hash = B256::with_last_byte(0xAA);
        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 3,
        }];

        let (resp_tx, resp_rx) = oneshot::channel();
        service.handle_provision_block_chunks(block_hash, specs, resp_tx);

        let result = resp_rx.blocking_recv().unwrap();
        assert!(
            result.is_ok(),
            "All chunks should be available from mock provider"
        );

        // Verify chunks are in cache
        let key0 = ChunkKey {
            ledger: 0,
            offset: 0,
        };
        let key1 = ChunkKey {
            ledger: 0,
            offset: 1,
        };
        let key2 = ChunkKey {
            ledger: 0,
            offset: 2,
        };
        assert!(service.cache.contains(&key0));
        assert!(service.cache.contains(&key1));
        assert!(service.cache.contains(&key2));

        // Verify block is tracked
        assert!(service.block_tracker.contains_key(&block_hash));
    }

    #[test]
    fn test_release_block_chunks_removes_references() {
        let (mut service, _tmp) = test_service();
        let block_hash = B256::with_last_byte(0xBB);
        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];

        // Provision — hold the receiver alive so the send succeeds
        let (resp_tx, resp_rx) = oneshot::channel();
        service.handle_provision_block_chunks(block_hash, specs, resp_tx);
        let _ = resp_rx.blocking_recv();

        // Verify chunks are cached
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 0
        }));
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 1
        }));

        // Release
        service.handle_release_block_chunks(&block_hash);

        // Block tracker should be empty
        assert!(!service.block_tracker.contains_key(&block_hash));

        // Chunks should be removed (no other references)
        assert!(!service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 0
        }));
        assert!(!service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 1
        }));
    }

    #[test]
    fn test_provision_block_chunks_shared_with_tx() {
        let (mut service, _tmp) = test_service();
        let tx_hash = B256::with_last_byte(0x01);
        let block_hash = B256::with_last_byte(0xCC);

        // First, provision via a tx (simulating mempool monitor)
        let tx_specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];
        service.handle_provision_chunks(tx_hash, tx_specs);

        // Now provision same chunks for a block
        let block_specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];
        let (resp_tx, resp_rx) = oneshot::channel();
        service.handle_provision_block_chunks(block_hash, block_specs, resp_tx);

        let result = resp_rx.blocking_recv().unwrap();
        assert!(result.is_ok());

        // Release block chunks — tx still references them, so they should stay
        service.handle_release_block_chunks(&block_hash);
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 0
        }));
        assert!(service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 1
        }));

        // Release tx chunks — now they should be gone
        service.handle_release_chunks(&tx_hash);
        assert!(!service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 0
        }));
        assert!(!service.cache.contains(&ChunkKey {
            ledger: 0,
            offset: 1
        }));
    }

    #[test]
    fn test_provisioning_populates_ready_set_and_chunk_index() {
        let (mut service, _tmp) = test_service();
        let ready_set = service.ready_pd_txs.clone();
        let tx_hash = B256::with_last_byte(0x01);

        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 3,
        }];

        service.handle_provision_chunks(tx_hash, specs);

        // Check tx is marked ready
        assert!(ready_set.contains(&tx_hash));

        // Release — should remove from ready set
        service.handle_release_chunks(&tx_hash);
        assert!(!ready_set.contains(&tx_hash));
    }

    #[test]
    fn test_provision_block_chunks_cleans_up_on_dropped_receiver() {
        let (mut service, _tmp) = test_service();
        let block_hash = B256::with_last_byte(0xDD);
        let specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];

        // Create a oneshot and immediately drop the receiver to simulate
        // a cancelled validation task.
        let (resp_tx, resp_rx) = oneshot::channel();
        drop(resp_rx);

        service.handle_provision_block_chunks(block_hash, specs, resp_tx);

        // Block tracker should be empty — the failed send should have triggered cleanup
        assert!(
            !service.block_tracker.contains_key(&block_hash),
            "block_tracker should not contain cancelled block"
        );

        // Cache should be empty — chunks should have been released
        assert!(
            !service.cache.contains(&ChunkKey {
                ledger: 0,
                offset: 0
            }),
            "cache should not contain chunks from cancelled provisioning"
        );
        assert!(
            !service.cache.contains(&ChunkKey {
                ledger: 0,
                offset: 1
            }),
            "cache should not contain chunks from cancelled provisioning"
        );
    }

    #[test]
    fn test_provision_block_chunks_dropped_receiver_preserves_tx_refs() {
        let (mut service, _tmp) = test_service();
        let tx_hash = B256::with_last_byte(0x01);
        let block_hash = B256::with_last_byte(0xEE);

        // First, provision via a tx (simulating mempool monitor)
        let tx_specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];
        service.handle_provision_chunks(tx_hash, tx_specs);

        // Now provision same chunks for a block, but drop the receiver
        let block_specs = vec![ChunkRangeSpecifier {
            partition_index: Default::default(),
            offset: 0,
            chunk_count: 2,
        }];
        let (resp_tx, resp_rx) = oneshot::channel();
        drop(resp_rx);

        service.handle_provision_block_chunks(block_hash, block_specs, resp_tx);

        // Block tracker should be empty — rollback cleaned it up
        assert!(
            !service.block_tracker.contains_key(&block_hash),
            "block_tracker should not contain cancelled block"
        );

        // But chunks should STILL be in cache — tx still references them
        assert!(
            service.cache.contains(&ChunkKey {
                ledger: 0,
                offset: 0
            }),
            "cache should still contain chunks referenced by tx"
        );
        assert!(
            service.cache.contains(&ChunkKey {
                ledger: 0,
                offset: 1
            }),
            "cache should still contain chunks referenced by tx"
        );

        // Releasing tx chunks should now fully clean up
        service.handle_release_chunks(&tx_hash);
        assert!(
            !service.cache.contains(&ChunkKey {
                ledger: 0,
                offset: 0
            }),
            "cache should be empty after tx release"
        );
        assert!(
            !service.cache.contains(&ChunkKey {
                ledger: 0,
                offset: 1
            }),
            "cache should be empty after tx release"
        );
    }

    /// When one chunk permanently fails, the waiting block is failed and removed
    /// from `pending_blocks`. Any *other* in-flight fetches for the same block
    /// must have that block removed from their `waiting_blocks` set, otherwise a
    /// later successful fetch would add a cache reference under a block hash
    /// that will never be released (permanent cache pin).
    #[test]
    fn test_fail_pending_fetch_detaches_block_from_sibling_fetches() {
        let (mut service, _tmp) = test_service();
        let block_hash = B256::with_last_byte(0xF1);
        let key_a = ChunkKey {
            ledger: 0,
            offset: 10,
        };
        let key_b = ChunkKey {
            ledger: 0,
            offset: 11,
        };

        // Simulate: block needs chunks A and B, both missing locally.
        // Set up pending_blocks entry.
        let (resp_tx, resp_rx) = oneshot::channel();
        service.pending_blocks.insert(
            block_hash,
            fetch::PendingBlockProvision {
                remaining_keys: HashSet::from([key_a, key_b]),
                all_keys: vec![key_a, key_b],
                response: resp_tx,
            },
        );

        // Set up pending_fetches for both chunks, each waiting on the block.
        service.pending_fetches.insert(
            key_a,
            fetch::PdChunkFetchState {
                waiting_txs: HashSet::new(),
                waiting_blocks: HashSet::from([block_hash]),
                attempt: 0,
                generation: 0,
                excluded_peers: HashSet::new(),
                status: fetch::FetchPhase::Fetching,
                abort_handle: None,
                retry_queue_key: None,
            },
        );
        service.pending_fetches.insert(
            key_b,
            fetch::PdChunkFetchState {
                waiting_txs: HashSet::new(),
                waiting_blocks: HashSet::from([block_hash]),
                attempt: 0,
                generation: 1,
                excluded_peers: HashSet::new(),
                status: fetch::FetchPhase::Fetching,
                abort_handle: None,
                retry_queue_key: None,
            },
        );

        // Chunk A permanently fails.
        service.fail_pending_fetch(&key_a);

        // The block should have received an error.
        let result = resp_rx.blocking_recv().unwrap();
        assert!(result.is_err(), "block should have been failed");

        // pending_blocks entry should be gone.
        assert!(
            !service.pending_blocks.contains_key(&block_hash),
            "pending_blocks should not contain the failed block"
        );

        // Key B's fetch state should no longer reference the block.
        if let Some(state_b) = service.pending_fetches.get(&key_b) {
            assert!(
                !state_b.waiting_blocks.contains(&block_hash),
                "sibling fetch for key_b must not reference the failed block"
            );
        }
        // If key_b had no other waiters, it should have been cancelled entirely.
        assert!(
            !service.pending_fetches.contains_key(&key_b),
            "fetch for key_b should be cancelled (no remaining waiters)"
        );
    }

    /// Same scenario as above, but chunk B also has a *second* block waiting.
    /// After chunk A fails (killing block X), chunk B's fetch should survive
    /// because block Y still needs it.
    #[test]
    fn test_fail_pending_fetch_preserves_sibling_fetch_with_other_waiters() {
        let (mut service, _tmp) = test_service();
        let block_x = B256::with_last_byte(0xF2);
        let block_y = B256::with_last_byte(0xF3);
        let key_a = ChunkKey {
            ledger: 0,
            offset: 20,
        };
        let key_b = ChunkKey {
            ledger: 0,
            offset: 21,
        };

        // Block X needs chunks A and B.
        let (resp_tx_x, resp_rx_x) = oneshot::channel();
        service.pending_blocks.insert(
            block_x,
            fetch::PendingBlockProvision {
                remaining_keys: HashSet::from([key_a, key_b]),
                all_keys: vec![key_a, key_b],
                response: resp_tx_x,
            },
        );

        // Block Y needs only chunk B.
        let (_resp_tx_y, _resp_rx_y) = oneshot::channel();
        service.pending_blocks.insert(
            block_y,
            fetch::PendingBlockProvision {
                remaining_keys: HashSet::from([key_b]),
                all_keys: vec![key_b],
                response: _resp_tx_y,
            },
        );

        // pending_fetches: A waits on X, B waits on X and Y.
        service.pending_fetches.insert(
            key_a,
            fetch::PdChunkFetchState {
                waiting_txs: HashSet::new(),
                waiting_blocks: HashSet::from([block_x]),
                attempt: 0,
                generation: 0,
                excluded_peers: HashSet::new(),
                status: fetch::FetchPhase::Fetching,
                abort_handle: None,
                retry_queue_key: None,
            },
        );
        service.pending_fetches.insert(
            key_b,
            fetch::PdChunkFetchState {
                waiting_txs: HashSet::new(),
                waiting_blocks: HashSet::from([block_x, block_y]),
                attempt: 0,
                generation: 1,
                excluded_peers: HashSet::new(),
                status: fetch::FetchPhase::Fetching,
                abort_handle: None,
                retry_queue_key: None,
            },
        );

        // Chunk A permanently fails — kills block X.
        service.fail_pending_fetch(&key_a);

        // Block X should have received an error.
        let result = resp_rx_x.blocking_recv().unwrap();
        assert!(result.is_err());

        // Key B's fetch should still exist because block Y is waiting.
        assert!(
            service.pending_fetches.contains_key(&key_b),
            "fetch for key_b should survive — block_y still needs it"
        );
        let state_b = service.pending_fetches.get(&key_b).unwrap();
        assert!(
            !state_b.waiting_blocks.contains(&block_x),
            "block_x must be detached from key_b"
        );
        assert!(
            state_b.waiting_blocks.contains(&block_y),
            "block_y must still be waiting on key_b"
        );
    }

    /// Exercises Fetching → Backoff transitions multiple times, verifying that
    /// status, abort_handle, and retry_queue_key stay consistent at each step.
    #[tokio::test]
    async fn test_retry_fetching_to_backoff_field_consistency() {
        let (mut service, _tmp) = test_service();
        let tx_hash = B256::with_last_byte(0x01);
        let key = ChunkKey {
            ledger: 0,
            offset: 50,
        };
        let peer_1: SocketAddr = "127.0.0.1:9001".parse().unwrap();
        let peer_2: SocketAddr = "127.0.0.1:9002".parse().unwrap();

        // Manually insert a PdChunkFetchState in Fetching phase.
        service.pending_fetches.insert(
            key,
            fetch::PdChunkFetchState {
                waiting_txs: HashSet::from([tx_hash]),
                waiting_blocks: HashSet::new(),
                attempt: 0,
                generation: 0,
                excluded_peers: HashSet::new(),
                status: fetch::FetchPhase::Fetching,
                abort_handle: None,
                retry_queue_key: None,
            },
        );

        // --- Transition 1: Fetching → Backoff ---
        service.on_fetch_all_peers_failed(key, vec![peer_1]);

        let state = service.pending_fetches.get(&key).unwrap();
        assert_eq!(state.status, fetch::FetchPhase::Backoff);
        assert!(
            state.abort_handle.is_none(),
            "abort_handle must be None in Backoff"
        );
        assert!(
            state.retry_queue_key.is_some(),
            "retry_queue_key must be set in Backoff"
        );
        assert_eq!(state.attempt, 1);
        assert!(state.excluded_peers.contains(&peer_1));
        assert!(
            !service.retry_queue.is_empty(),
            "retry_queue must have an entry"
        );

        // Simulate the retry firing and a second fetch attempt completing.
        // Reset to Fetching to test the second failure transition.
        let state = service.pending_fetches.get_mut(&key).unwrap();
        state.status = fetch::FetchPhase::Fetching;
        state.retry_queue_key = None;
        state.abort_handle = None;

        // --- Transition 2: Fetching → Backoff (second failure) ---
        service.on_fetch_all_peers_failed(key, vec![peer_2]);

        let state = service.pending_fetches.get(&key).unwrap();
        assert_eq!(state.status, fetch::FetchPhase::Backoff);
        assert!(
            state.abort_handle.is_none(),
            "abort_handle must be None in Backoff (round 2)"
        );
        assert!(
            state.retry_queue_key.is_some(),
            "retry_queue_key must be set in Backoff (round 2)"
        );
        assert_eq!(state.attempt, 2);
        assert!(
            state.excluded_peers.contains(&peer_1),
            "peer_1 should still be excluded"
        );
        assert!(
            state.excluded_peers.contains(&peer_2),
            "peer_2 should now be excluded"
        );
    }

    /// When on_retry_ready fires but no peers can be resolved, the fetch must
    /// be permanently failed. This also verifies that retry_queue_key is cleared
    /// before the failure path runs (preventing double-remove panics).
    #[tokio::test]
    async fn test_retry_ready_no_peers_permanently_fails() {
        let (mut service, _tmp) = test_service();
        let block_hash = B256::with_last_byte(0xF6);
        let key = ChunkKey {
            ledger: 0,
            offset: 70,
        };

        // Set up a pending block.
        let (resp_tx, resp_rx) = oneshot::channel();
        service.pending_blocks.insert(
            block_hash,
            fetch::PendingBlockProvision {
                remaining_keys: HashSet::from([key]),
                all_keys: vec![key],
                response: resp_tx,
            },
        );

        // Insert fetch state in Backoff phase (simulating a previous failure).
        // Use a real DelayQueue key so the stale-key cleanup path is exercised.
        let retry_entry_for_queue = fetch::RetryEntry {
            key,
            generation: 0,
            excluded_peers: HashSet::new(),
        };
        let queue_key = service
            .retry_queue
            .insert(retry_entry_for_queue, std::time::Duration::from_secs(60));
        service.pending_fetches.insert(
            key,
            fetch::PdChunkFetchState {
                waiting_txs: HashSet::new(),
                waiting_blocks: HashSet::from([block_hash]),
                attempt: 1,
                generation: 0,
                excluded_peers: HashSet::new(),
                status: fetch::FetchPhase::Backoff,
                abort_handle: None,
                retry_queue_key: Some(queue_key),
            },
        );

        // Fire the retry — no peers available (mock peer list is empty).
        let retry_entry = fetch::RetryEntry {
            key,
            generation: 0,
            excluded_peers: HashSet::new(),
        };
        service.on_retry_ready(retry_entry);

        // Fetch should be permanently failed.
        assert!(
            !service.pending_fetches.contains_key(&key),
            "fetch should be removed after no-peers permanent failure"
        );
        assert!(
            !service.pending_blocks.contains_key(&block_hash),
            "pending block should be failed"
        );
        let result = resp_rx.await.unwrap();
        assert!(result.is_err(), "block should receive an error");
    }

    /// After MAX_CHUNK_FETCH_RETRIES attempts, the next failure must
    /// permanently fail the fetch and notify waiting blocks with an error.
    #[test]
    fn test_retry_exhaustion_permanently_fails() {
        let (mut service, _tmp) = test_service();
        let block_hash = B256::with_last_byte(0xF5);
        let key = ChunkKey {
            ledger: 0,
            offset: 60,
        };

        // Set up a pending block waiting on this chunk.
        let (resp_tx, resp_rx) = oneshot::channel();
        service.pending_blocks.insert(
            block_hash,
            fetch::PendingBlockProvision {
                remaining_keys: HashSet::from([key]),
                all_keys: vec![key],
                response: resp_tx,
            },
        );

        // Insert fetch state at the retry limit.
        service.pending_fetches.insert(
            key,
            fetch::PdChunkFetchState {
                waiting_txs: HashSet::new(),
                waiting_blocks: HashSet::from([block_hash]),
                attempt: fetch::MAX_CHUNK_FETCH_RETRIES,
                generation: 0,
                excluded_peers: HashSet::new(),
                status: fetch::FetchPhase::Fetching,
                abort_handle: None,
                retry_queue_key: None,
            },
        );

        // This failure should trigger permanent failure, not a retry.
        service.on_fetch_all_peers_failed(key, vec![]);

        // Fetch state should be gone.
        assert!(
            !service.pending_fetches.contains_key(&key),
            "fetch should be permanently removed after exhausting retries"
        );

        // Block should have been failed.
        assert!(
            !service.pending_blocks.contains_key(&block_hash),
            "pending block should be removed"
        );
        let result = resp_rx.blocking_recv().unwrap();
        assert!(result.is_err(), "block should receive an error");
    }
}
