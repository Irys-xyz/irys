//! Wire types for the node-internal block-stream contract (`/internal/blocks/*`).
//!
//! These are response-only shapes consumed by the gateway block follower. Hash fields serialise as
//! base58 (the node's default [`H256`] serde); account ids use the gateway's `OwnerId { sig_type,
//! bytes }` encoding. They are built from data the node already holds — sealed blocks on the stream
//! paths, header + resolved tx headers on the read paths — and are never derived onto the consensus
//! types directly.

use crate::{DataLedger, DataTransactionHeader, H256, IrysAddress, IrysBlockHeader, SealedBlock};
use base58::{FromBase58 as _, ToBase58 as _};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// One stream frame: a monotonic `seq` flattened beside a `kind`-tagged event.
#[derive(Debug, Clone, Serialize)]
pub struct StreamFrame {
    pub seq: u64,
    #[serde(flatten)]
    pub event: StreamEvent,
}

/// The tagged event payload. `kind` is lowercase; `observed`/`finalized` flatten a [`BlockEvent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum StreamEvent {
    Observed(BlockEvent),
    Finalized(BlockEvent),
    Reorged {
        fork_parent: BlockRef,
        /// Orphaned (old-fork) blocks, ascending by height.
        orphaned: Vec<BlockEvent>,
        /// Winning (new-fork) blocks, ascending by height.
        new_fork: Vec<BlockEvent>,
    },
}

/// A block header view plus its per-transaction metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockEvent {
    pub header: BlockHeaderView,
    pub txs: Vec<TxMeta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockHeaderView {
    pub height: u64,
    pub block_hash: H256,
    pub previous_block_hash: H256,
    /// Milliseconds since the Unix epoch.
    pub timestamp: u64,
    pub miner_address: OwnerId,
    pub data_ledgers: Vec<DataLedgerEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataLedgerEntry {
    pub ledger_id: u32,
    pub tx_root: H256,
    pub tx_ids: Vec<H256>,
    pub total_chunks: u64,
    pub required_proof_count: u32,
    pub proof_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxMeta {
    pub tx_id: H256,
    pub anchor: H256,
    pub signer: OwnerId,
    pub data_root: H256,
    pub data_size: u64,
    pub prefix_size: u64,
    pub prefix_hash: H256,
    pub ledger_id: u32,
    pub metadata_format: u8,
    /// Index of this tx within its ledger, in block order.
    pub tx_position: u32,
    /// Absolute chunk offset of this tx within its ledger.
    pub tx_start_offset: u64,
    pub promoted_height: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockRef {
    pub height: u64,
    pub block_hash: H256,
}

/// Gateway `OwnerId` encoding: a signature-scheme discriminant plus the base58 of the 20-byte address.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OwnerId {
    pub sig_type: u16,
    #[serde(
        serialize_with = "serialize_base58",
        deserialize_with = "deserialize_base58"
    )]
    pub bytes: Vec<u8>,
}

/// Ethereum / secp256k1 discriminant, matching the reference producer's frames.
const SIG_TYPE_ETHEREUM: u16 = 0;

impl OwnerId {
    fn ethereum(addr: &IrysAddress) -> Self {
        Self {
            sig_type: SIG_TYPE_ETHEREUM,
            bytes: addr.0.as_slice().to_vec(),
        }
    }
}

fn serialize_base58<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&bytes.to_base58())
}

fn deserialize_base58<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    use serde::de::Error as _;
    let encoded = String::deserialize(deserializer)?;
    encoded
        .from_base58()
        .map_err(|e| D::Error::custom(format!("invalid base58: {e:?}")))
}

/// Chunks occupied by a transaction of `data_size` bytes, rounding up.
fn ceil_chunks(data_size: u64, chunk_size: u64) -> u64 {
    if chunk_size == 0 {
        return 0;
    }
    data_size.div_ceil(chunk_size)
}

/// Absolute per-tx start offsets within a ledger, derived from the ledger's post-block `total_chunks`.
///
/// The pre-block cursor is `total_chunks` minus the chunks this block's transactions added, so each
/// tx's start accumulates from there in block order. This needs only the block in hand — no parent
/// lookup.
fn start_offsets(total_chunks: u64, data_sizes: &[u64], chunk_size: u64) -> Vec<u64> {
    let block_chunks: u64 = data_sizes.iter().map(|&s| ceil_chunks(s, chunk_size)).sum();
    let mut cursor = total_chunks.saturating_sub(block_chunks);
    data_sizes
        .iter()
        .map(|&s| {
            let start = cursor;
            cursor += ceil_chunks(s, chunk_size);
            start
        })
        .collect()
}

impl BlockEvent {
    /// Build from a sealed block (stream paths): header view plus a [`TxMeta`] per data transaction,
    /// in ledger order.
    pub fn from_sealed(block: &SealedBlock, chunk_size: u64) -> Self {
        let header = block.header();
        let txns = block.transactions();
        Self::build(header, chunk_size, |ledger| {
            txns.get_ledger_txs(ledger).to_vec()
        })
    }

    /// Build from a header and a per-ledger resolver of transaction headers (read paths). The
    /// resolver returns the ordered tx headers for a ledger (e.g. from the DB).
    pub fn from_header_and_txs(
        header: &IrysBlockHeader,
        ledger_txs: impl FnMut(DataLedger) -> Vec<DataTransactionHeader>,
        chunk_size: u64,
    ) -> Self {
        Self::build(header, chunk_size, ledger_txs)
    }

    fn build(
        header: &IrysBlockHeader,
        chunk_size: u64,
        mut ledger_txs: impl FnMut(DataLedger) -> Vec<DataTransactionHeader>,
    ) -> Self {
        let mut data_ledgers = Vec::with_capacity(header.data_ledgers.len());
        let mut txs = Vec::new();

        for dl in &header.data_ledgers {
            data_ledgers.push(DataLedgerEntry {
                ledger_id: dl.ledger_id,
                tx_root: dl.tx_root,
                tx_ids: dl.tx_ids.0.clone(),
                total_chunks: dl.total_chunks,
                required_proof_count: u32::from(dl.required_proof_count.unwrap_or(0)),
                proof_count: dl
                    .proofs
                    .as_ref()
                    .map_or(0, |p| u32::try_from(p.0.len()).unwrap_or(u32::MAX)),
            });

            let Ok(ledger) = DataLedger::try_from(dl.ledger_id) else {
                continue;
            };
            let ledger_headers = ledger_txs(ledger);
            let sizes: Vec<u64> = ledger_headers.iter().map(|t| t.data_size).collect();
            let offsets = start_offsets(dl.total_chunks, &sizes, chunk_size);

            for (i, tx) in ledger_headers.iter().enumerate() {
                txs.push(TxMeta {
                    tx_id: tx.id,
                    anchor: tx.anchor,
                    signer: OwnerId::ethereum(&tx.signer),
                    data_root: tx.data_root,
                    data_size: tx.data_size,
                    prefix_size: tx.prefix_size,
                    prefix_hash: tx.prefix_hash,
                    ledger_id: dl.ledger_id,
                    metadata_format: tx.metadata_format,
                    tx_position: u32::try_from(i).unwrap_or(u32::MAX),
                    tx_start_offset: offsets[i],
                    promoted_height: tx.promoted_height(),
                });
            }
        }

        Self {
            header: BlockHeaderView {
                height: header.height,
                block_hash: header.block_hash,
                previous_block_hash: header.previous_block_hash,
                timestamp: u64::try_from(header.timestamp.as_millis()).unwrap_or(u64::MAX),
                miner_address: OwnerId::ethereum(&header.miner_address),
                data_ledgers,
            },
            txs,
        }
    }
}

impl StreamFrame {
    /// The lowercase frame kind, for routing on the consumer side and in tests.
    pub fn kind(&self) -> &'static str {
        match self.event {
            StreamEvent::Observed(_) => "observed",
            StreamEvent::Finalized(_) => "finalized",
            StreamEvent::Reorged { .. } => "reorged",
        }
    }

    /// The block hash for `observed`/`finalized` frames; `None` for `reorged`.
    pub fn block_hash(&self) -> Option<H256> {
        match &self.event {
            StreamEvent::Observed(b) | StreamEvent::Finalized(b) => Some(b.header.block_hash),
            StreamEvent::Reorged { .. } => None,
        }
    }

    /// The data_roots carried by an `observed`/`finalized` frame; empty for `reorged`.
    pub fn data_roots(&self) -> Vec<H256> {
        match &self.event {
            StreamEvent::Observed(b) | StreamEvent::Finalized(b) => {
                b.txs.iter().map(|t| t.data_root).collect()
            }
            StreamEvent::Reorged { .. } => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_block_event(h: H256) -> BlockEvent {
        BlockEvent {
            header: BlockHeaderView {
                height: 8,
                block_hash: h,
                previous_block_hash: h,
                timestamp: 1_718_600_000_000,
                miner_address: OwnerId {
                    sig_type: 0,
                    bytes: vec![1_u8; 20],
                },
                data_ledgers: vec![DataLedgerEntry {
                    ledger_id: 1,
                    tx_root: h,
                    tx_ids: vec![h],
                    total_chunks: 12,
                    required_proof_count: 1,
                    proof_count: 0,
                }],
            },
            txs: vec![TxMeta {
                tx_id: h,
                anchor: h,
                signer: OwnerId {
                    sig_type: 0,
                    bytes: vec![1_u8; 20],
                },
                data_root: h,
                data_size: 1024,
                prefix_size: 0,
                prefix_hash: H256::zero(),
                ledger_id: 1,
                metadata_format: 0,
                tx_position: 0,
                tx_start_offset: 0,
                promoted_height: None,
            }],
        }
    }

    #[test]
    fn observed_frame_serialises_to_contract_json() {
        let h = H256::from([7_u8; 32]);
        let frame = StreamFrame {
            seq: 42,
            event: StreamEvent::Observed(sample_block_event(h)),
        };
        let v: serde_json::Value = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["seq"], 42);
        assert_eq!(v["kind"], "observed"); // flattened beside seq, lowercase
        assert_eq!(
            v["header"]["block_hash"]
                .as_str()
                .unwrap()
                .from_base58()
                .unwrap(),
            vec![7_u8; 32]
        );
        assert_eq!(v["header"]["miner_address"]["sig_type"], 0);
        assert!(v["header"]["miner_address"]["bytes"].is_string());
        assert_eq!(v["txs"][0]["promoted_height"], serde_json::Value::Null);
        assert_eq!(
            v["txs"][0]["data_root"]
                .as_str()
                .unwrap()
                .from_base58()
                .unwrap(),
            vec![7_u8; 32]
        );
    }

    #[test]
    fn reorged_frame_has_batch_shape() {
        let h = H256::from([3_u8; 32]);
        let frame = StreamFrame {
            seq: 44,
            event: StreamEvent::Reorged {
                fork_parent: BlockRef {
                    height: 7,
                    block_hash: h,
                },
                orphaned: vec![],
                new_fork: vec![],
            },
        };
        let v = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["kind"], "reorged");
        assert_eq!(v["fork_parent"]["height"], 7);
        assert!(v["orphaned"].is_array() && v["new_fork"].is_array());
    }

    #[test]
    fn event_json_round_trips_for_log_replay() {
        // The producer stores the StreamEvent JSON and rebuilds a typed frame on replay.
        let h = H256::from([9_u8; 32]);
        let event = StreamEvent::Observed(sample_block_event(h));
        let bytes = serde_json::to_vec(&event).unwrap();
        let decoded: StreamEvent = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::to_value(&decoded).unwrap()
        );
        // A frame built from the decoded event serialises with seq + kind at top level.
        let frame = StreamFrame {
            seq: 5,
            event: decoded,
        };
        let v = serde_json::to_value(&frame).unwrap();
        assert_eq!(v["seq"], 5);
        assert_eq!(v["kind"], "observed");
        assert_eq!(
            v["header"]["block_hash"]
                .as_str()
                .unwrap()
                .from_base58()
                .unwrap(),
            vec![9_u8; 32]
        );
    }

    #[test]
    fn tx_start_offset_accumulates_from_pre_block_cursor() {
        // ledger total_chunks = 10; two txs sized 1 and 2 chunks (chunk_size 1) ⇒ pre-block = 7 ⇒ [7, 8]
        assert_eq!(start_offsets(10, &[1, 2], 1), vec![7, 8]);
        // genesis: total equals this block's chunks ⇒ starts at 0
        assert_eq!(start_offsets(3, &[1, 2], 1), vec![0, 1]);
        // rounding up: 3 bytes at chunk_size 2 ⇒ 2 chunks
        assert_eq!(start_offsets(2, &[3], 2), vec![0]);
    }
}
