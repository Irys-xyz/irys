# Sovereign Gossip Wire Types Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Create sovereign copies of all gossip wire-serialized types in the p2p crate, decoupling wire format from `irys-types` internal changes.

**Architecture:** New `wire_types` module in `crates/p2p/src/` containing copies of every struct/enum serialized over gossip HTTP. Each sovereign type has explicit serde attributes (no reliance on IntegerTagged macro) and From/TryFrom conversions to canonical `irys-types` types. Server/client code is updated to serialize/deserialize using wire types at the HTTP boundary.

**Tech Stack:** Rust, serde/serde_json, alloy_primitives (B256 via reth re-export), irys-types primitives (H256, U256, IrysAddress, etc.)

---

## Shared Primitives (NOT copied — imported from irys-types)

These primitives have stable serde and are used as field types in sovereign structs:
- `H256`, `U256`, `IrysAddress`, `IrysSignature`, `IrysPeerId`, `Base64`, `H256List`, `IngressProofsList`
- `TxChunkOffset`, `UnixTimestampMs`, `BoundedFee`, `BlockHash`, `ChunkPathHash`, `IrysTransactionId`, `DataRoot`, `PartitionHash`
- `B256` (from `reth::revm::primitives`)
- `Amount<T>` / `IrysTokenPrice` (from `irys_types::storage_pricing`)
- `ProtocolVersion` (enum with V1=1, V2=2 — simple repr, no custom serde)
- `semver::Version`

## Serde Helpers Needed

The sovereign types need access to these serde helper modules from irys-types:
- `string_u64` — serializes `u64` as `"123"`
- `optional_string_u64` — serializes `Option<u64>` as `"123"` or absent
- `string_usize` — serializes `usize` as `"123"`
- `u64_stringify` — serializes `u64` as `"123"` (used on DataTransactionLedger)
- `option_u64_stringify` — serializes `Option<u64>` as `"123"` (used on VDFLimiterInfo)

These are currently private in `irys-types`. They need to be re-exported or duplicated. Check if they're already public. If not, the simplest path is to make them `pub` in irys-types.

---

### Task 1: Expose serde helper modules from irys-types

**Files:**
- Modify: `crates/types/src/serialization.rs` (or wherever `string_u64` etc. are defined)
- Modify: `crates/types/src/lib.rs` — re-export the modules

**Step 1: Find the serde helper module locations**

Run: `grep -rn "mod string_u64\|pub mod string_u64\|fn serialize.*string_u64" crates/types/src/`

These helpers are likely in `crates/types/src/serialization.rs` or a sub-module. They need to be `pub` so the p2p crate can use them with `#[serde(with = "irys_types::string_u64")]`.

**Step 2: Make the modules public**

If `string_u64` is defined as a private module, change `mod string_u64` → `pub mod string_u64`. Do the same for `optional_string_u64`, `string_usize`, `u64_stringify`, `option_u64_stringify`.

**Step 3: Verify compilation**

Run: `cargo check -p irys-types`
Expected: compiles without errors

**Step 4: Commit**

```bash
git add crates/types/src/
git commit -m "refactor: expose serde helper modules from irys-types"
```

---

### Task 2: Create wire_types module scaffold with leaf types

**Files:**
- Create: `crates/p2p/src/wire_types/mod.rs`
- Create: `crates/p2p/src/wire_types/chunk.rs`
- Create: `crates/p2p/src/wire_types/ingress.rs`
- Modify: `crates/p2p/src/lib.rs` — add `pub(crate) mod wire_types;`

These are "leaf" types with no dependencies on other wire types.

**Step 1: Create `crates/p2p/src/wire_types/mod.rs`**

```rust
pub mod chunk;
pub mod ingress;

pub use chunk::*;
pub use ingress::*;
```

**Step 2: Create `crates/p2p/src/wire_types/chunk.rs`**

Reference: `crates/types/src/chunk.rs` — `UnpackedChunk` struct

```rust
use irys_types::{serialization::Base64, DataRoot, TxChunkOffset};
use serde::{Deserialize, Serialize};

/// Sovereign wire type for UnpackedChunk.
/// This type owns its serde configuration independently of irys_types::UnpackedChunk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UnpackedChunk {
    pub data_root: DataRoot,
    #[serde(with = "irys_types::string_u64")]
    pub data_size: u64,
    pub data_path: Base64,
    pub bytes: Base64,
    pub tx_offset: TxChunkOffset,
}

impl From<&irys_types::UnpackedChunk> for UnpackedChunk {
    fn from(c: &irys_types::UnpackedChunk) -> Self {
        Self {
            data_root: c.data_root,
            data_size: c.data_size,
            data_path: c.data_path.clone(),
            bytes: c.bytes.clone(),
            tx_offset: c.tx_offset,
        }
    }
}

impl From<UnpackedChunk> for irys_types::UnpackedChunk {
    fn from(c: UnpackedChunk) -> Self {
        Self {
            data_root: c.data_root,
            data_size: c.data_size,
            data_path: c.data_path,
            bytes: c.bytes,
            tx_offset: c.tx_offset,
        }
    }
}
```

**Step 3: Create `crates/p2p/src/wire_types/ingress.rs`**

Reference: `crates/types/src/ingress.rs` — IngressProof / IngressProofV1, uses IntegerTagged

The JSON format is flattened: `{ "version": 1, "anchor": "...", "chain_id": 1270, ... }`.
Note: IngressProofV1 uses **snake_case** field names (no `rename_all = "camelCase"`).
The `signature` field is NOT skipped in JSON (it uses default serde from IrysSignature).

```rust
use irys_types::{IrysSignature, H256};
use serde::{Deserialize, Serialize};

/// Sovereign wire type for IngressProof (flattened versioned enum).
/// Matches the IntegerTagged JSON output: { "version": N, ...inner_fields... }
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IngressProof {
    pub version: u8,
    pub signature: IrysSignature,
    pub data_root: H256,
    pub proof: H256,
    pub chain_id: u64,
    pub anchor: H256,
}

impl From<&irys_types::IngressProof> for IngressProof {
    fn from(p: &irys_types::IngressProof) -> Self {
        match p {
            irys_types::IngressProof::V1(inner) => Self {
                version: 1,
                signature: inner.signature,
                data_root: inner.data_root,
                proof: inner.proof,
                chain_id: inner.chain_id,
                anchor: inner.anchor,
            },
        }
    }
}

impl TryFrom<IngressProof> for irys_types::IngressProof {
    type Error = eyre::Report;
    fn try_from(p: IngressProof) -> eyre::Result<Self> {
        match p.version {
            1 => Ok(irys_types::IngressProof::V1(irys_types::ingress::IngressProofV1 {
                signature: p.signature,
                data_root: p.data_root,
                proof: p.proof,
                chain_id: p.chain_id,
                anchor: p.anchor,
            })),
            v => Err(eyre::eyre!("Unknown IngressProof version: {}", v)),
        }
    }
}
```

**Step 4: Add wire_types to lib.rs**

Add `pub(crate) mod wire_types;` to `crates/p2p/src/lib.rs` (after `mod gossip_service;`).

**Step 5: Verify compilation**

Run: `cargo check -p irys-p2p`
Expected: compiles

**Step 6: Commit**

```bash
git add crates/p2p/src/wire_types/ crates/p2p/src/lib.rs
git commit -m "feat: add wire_types module scaffold with chunk and ingress types"
```

---

### Task 3: Add commitment wire types

**Files:**
- Create: `crates/p2p/src/wire_types/commitment.rs`
- Modify: `crates/p2p/src/wire_types/mod.rs`

Reference: `crates/types/src/commitment_v1.rs`, `crates/types/src/commitment_v2.rs`, `crates/types/src/commitment_common.rs`

CommitmentTransaction is an IntegerTagged enum with V1 and V2 variants. Each variant wraps a `CommitmentVNWithMetadata` which has `#[serde(flatten)]` on the inner tx and `#[serde(skip)]` on metadata.

The JSON output is flattened: `{ "version": N, ...all_tx_fields... }` (metadata is skipped).

**Step 1: Create `crates/p2p/src/wire_types/commitment.rs`**

```rust
use irys_types::{IrysAddress, IrysSignature, H256, U256};
use serde::{Deserialize, Serialize};

// -- CommitmentTypeV1 --
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum CommitmentTypeV1 {
    Stake,
    Pledge {
        #[serde(rename = "pledgeCountBeforeExecuting", with = "irys_types::string_u64")]
        pledge_count_before_executing: u64,
    },
    Unpledge {
        #[serde(rename = "pledgeCountBeforeExecuting", with = "irys_types::string_u64")]
        pledge_count_before_executing: u64,
        #[serde(rename = "partitionHash")]
        partition_hash: H256,
    },
    Unstake,
}

// -- CommitmentTypeV2 --
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum CommitmentTypeV2 {
    Stake,
    Pledge {
        #[serde(rename = "pledgeCountBeforeExecuting", with = "irys_types::string_u64")]
        pledge_count_before_executing: u64,
    },
    Unpledge {
        #[serde(rename = "pledgeCountBeforeExecuting", with = "irys_types::string_u64")]
        pledge_count_before_executing: u64,
        #[serde(rename = "partitionHash")]
        partition_hash: H256,
    },
    Unstake,
    UpdateRewardAddress {
        #[serde(rename = "newRewardAddress")]
        new_reward_address: IrysAddress,
    },
}

// -- CommitmentTransactionV1 fields (flattened by IntegerTagged) --
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommitmentTransactionV1Inner {
    pub id: H256,
    pub anchor: H256,
    pub signer: IrysAddress,
    pub commitment_type: CommitmentTypeV1,
    #[serde(with = "irys_types::string_u64")]
    pub chain_id: u64,
    #[serde(with = "irys_types::string_u64")]
    pub fee: u64,
    pub value: U256,
    pub signature: IrysSignature,
}

// -- CommitmentTransactionV2 fields (flattened by IntegerTagged) --
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CommitmentTransactionV2Inner {
    pub id: H256,
    pub anchor: H256,
    pub signer: IrysAddress,
    pub commitment_type: CommitmentTypeV2,
    #[serde(with = "irys_types::string_u64")]
    pub chain_id: u64,
    #[serde(with = "irys_types::string_u64")]
    pub fee: u64,
    pub value: U256,
    pub signature: IrysSignature,
}

/// Sovereign wire type for CommitmentTransaction.
/// Replaces IntegerTagged flattening with explicit version field + serde.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitmentTransaction {
    V1(CommitmentTransactionV1Inner),
    V2(CommitmentTransactionV2Inner),
}

// Hand-written Serialize that produces flattened JSON with "version" field
// matching IntegerTagged output exactly
impl Serialize for CommitmentTransaction {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;

        let (version, inner_value) = match self {
            Self::V1(inner) => (1u8, serde_json::to_value(inner).map_err(serde::ser::Error::custom)?),
            Self::V2(inner) => (2u8, serde_json::to_value(inner).map_err(serde::ser::Error::custom)?),
        };

        if let serde_json::Value::Object(inner_map) = inner_value {
            let mut map = serializer.serialize_map(Some(inner_map.len() + 1))?;
            map.serialize_entry("version", &version)?;
            for (key, value) in inner_map {
                map.serialize_entry(&key, &value)?;
            }
            map.end()
        } else {
            Err(serde::ser::Error::custom("inner value must be a struct"))
        }
    }
}

// Hand-written Deserialize that extracts "version" field from flat JSON
impl<'de> Deserialize<'de> for CommitmentTransaction {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut map = serde_json::Map::new();
        let value = serde_json::Value::deserialize(deserializer)?;

        if let serde_json::Value::Object(obj) = value {
            let mut version: Option<u8> = None;
            let mut fields = serde_json::Map::new();
            for (key, val) in obj {
                if key == "version" {
                    version = Some(
                        serde_json::from_value(val).map_err(serde::de::Error::custom)?
                    );
                } else {
                    fields.insert(key, val);
                }
            }
            let inner_value = serde_json::Value::Object(fields);
            match version {
                Some(1) => {
                    let inner: CommitmentTransactionV1Inner =
                        serde_json::from_value(inner_value).map_err(serde::de::Error::custom)?;
                    Ok(Self::V1(inner))
                }
                Some(2) => {
                    let inner: CommitmentTransactionV2Inner =
                        serde_json::from_value(inner_value).map_err(serde::de::Error::custom)?;
                    Ok(Self::V2(inner))
                }
                Some(v) => Err(serde::de::Error::custom(format!("unknown version: {}", v))),
                None => Err(serde::de::Error::missing_field("version")),
            }
        } else {
            Err(serde::de::Error::custom("expected object"))
        }
    }
}

// -- Conversions --

impl From<&irys_types::CommitmentTypeV1> for CommitmentTypeV1 {
    fn from(ct: &irys_types::CommitmentTypeV1) -> Self {
        match ct {
            irys_types::CommitmentTypeV1::Stake => Self::Stake,
            irys_types::CommitmentTypeV1::Pledge { pledge_count_before_executing } =>
                Self::Pledge { pledge_count_before_executing: *pledge_count_before_executing },
            irys_types::CommitmentTypeV1::Unpledge { pledge_count_before_executing, partition_hash } =>
                Self::Unpledge {
                    pledge_count_before_executing: *pledge_count_before_executing,
                    partition_hash: *partition_hash,
                },
            irys_types::CommitmentTypeV1::Unstake => Self::Unstake,
        }
    }
}

impl From<&irys_types::CommitmentTypeV2> for CommitmentTypeV2 {
    fn from(ct: &irys_types::CommitmentTypeV2) -> Self {
        match ct {
            irys_types::CommitmentTypeV2::Stake => Self::Stake,
            irys_types::CommitmentTypeV2::Pledge { pledge_count_before_executing } =>
                Self::Pledge { pledge_count_before_executing: *pledge_count_before_executing },
            irys_types::CommitmentTypeV2::Unpledge { pledge_count_before_executing, partition_hash } =>
                Self::Unpledge {
                    pledge_count_before_executing: *pledge_count_before_executing,
                    partition_hash: *partition_hash,
                },
            irys_types::CommitmentTypeV2::Unstake => Self::Unstake,
            irys_types::CommitmentTypeV2::UpdateRewardAddress { new_reward_address } =>
                Self::UpdateRewardAddress { new_reward_address: *new_reward_address },
        }
    }
}

impl From<CommitmentTypeV1> for irys_types::CommitmentTypeV1 {
    fn from(ct: CommitmentTypeV1) -> Self {
        match ct {
            CommitmentTypeV1::Stake => Self::Stake,
            CommitmentTypeV1::Pledge { pledge_count_before_executing } =>
                Self::Pledge { pledge_count_before_executing },
            CommitmentTypeV1::Unpledge { pledge_count_before_executing, partition_hash } =>
                Self::Unpledge { pledge_count_before_executing, partition_hash },
            CommitmentTypeV1::Unstake => Self::Unstake,
        }
    }
}

impl From<CommitmentTypeV2> for irys_types::CommitmentTypeV2 {
    fn from(ct: CommitmentTypeV2) -> Self {
        match ct {
            CommitmentTypeV2::Stake => Self::Stake,
            CommitmentTypeV2::Pledge { pledge_count_before_executing } =>
                Self::Pledge { pledge_count_before_executing },
            CommitmentTypeV2::Unpledge { pledge_count_before_executing, partition_hash } =>
                Self::Unpledge { pledge_count_before_executing, partition_hash },
            CommitmentTypeV2::Unstake => Self::Unstake,
            CommitmentTypeV2::UpdateRewardAddress { new_reward_address } =>
                Self::UpdateRewardAddress { new_reward_address },
        }
    }
}

impl From<&irys_types::CommitmentTransaction> for CommitmentTransaction {
    fn from(ct: &irys_types::CommitmentTransaction) -> Self {
        match ct {
            irys_types::CommitmentTransaction::V1(wm) => Self::V1(CommitmentTransactionV1Inner {
                id: wm.tx.id,
                anchor: wm.tx.anchor,
                signer: wm.tx.signer,
                commitment_type: (&wm.tx.commitment_type).into(),
                chain_id: wm.tx.chain_id,
                fee: wm.tx.fee,
                value: wm.tx.value,
                signature: wm.tx.signature,
            }),
            irys_types::CommitmentTransaction::V2(wm) => Self::V2(CommitmentTransactionV2Inner {
                id: wm.tx.id,
                anchor: wm.tx.anchor,
                signer: wm.tx.signer,
                commitment_type: (&wm.tx.commitment_type).into(),
                chain_id: wm.tx.chain_id,
                fee: wm.tx.fee,
                value: wm.tx.value,
                signature: wm.tx.signature,
            }),
        }
    }
}

impl TryFrom<CommitmentTransaction> for irys_types::CommitmentTransaction {
    type Error = eyre::Report;
    fn try_from(ct: CommitmentTransaction) -> eyre::Result<Self> {
        match ct {
            CommitmentTransaction::V1(inner) => {
                Ok(irys_types::CommitmentTransaction::V1(irys_types::CommitmentV1WithMetadata {
                    tx: irys_types::CommitmentTransactionV1 {
                        id: inner.id,
                        anchor: inner.anchor,
                        signer: inner.signer,
                        commitment_type: inner.commitment_type.into(),
                        chain_id: inner.chain_id,
                        fee: inner.fee,
                        value: inner.value,
                        signature: inner.signature,
                    },
                    metadata: Default::default(),
                }))
            }
            CommitmentTransaction::V2(inner) => {
                Ok(irys_types::CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
                    tx: irys_types::CommitmentTransactionV2 {
                        id: inner.id,
                        anchor: inner.anchor,
                        signer: inner.signer,
                        commitment_type: inner.commitment_type.into(),
                        chain_id: inner.chain_id,
                        fee: inner.fee,
                        value: inner.value,
                        signature: inner.signature,
                    },
                    metadata: Default::default(),
                }))
            }
        }
    }
}
```

**Step 2: Add to mod.rs**

Add `pub mod commitment;` and `pub use commitment::*;` to `wire_types/mod.rs`.

**Step 3: Verify compilation**

Run: `cargo check -p irys-p2p`

**Step 4: Commit**

```bash
git add crates/p2p/src/wire_types/commitment.rs crates/p2p/src/wire_types/mod.rs
git commit -m "feat: add sovereign commitment wire types"
```

---

### Task 4: Add transaction wire types

**Files:**
- Create: `crates/p2p/src/wire_types/transaction.rs`
- Modify: `crates/p2p/src/wire_types/mod.rs`

Reference: `crates/types/src/transaction.rs` — DataTransactionHeader uses IntegerTagged.
DataTransactionHeaderV1WithMetadata has `#[serde(flatten)]` on tx and `#[serde(skip)]` on metadata.

The JSON is: `{ "version": 1, "id": "...", "anchor": "...", ... }` (flattened, camelCase).

**Step 1: Create `crates/p2p/src/wire_types/transaction.rs`**

```rust
use irys_types::{BoundedFee, IrysAddress, IrysSignature, H256};
use serde::{Deserialize, Serialize};

/// Sovereign wire type for the inner DataTransactionHeaderV1 fields.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DataTransactionHeaderV1Inner {
    pub id: H256,
    pub anchor: H256,
    pub signer: IrysAddress,
    pub data_root: H256,
    #[serde(with = "irys_types::string_u64")]
    pub data_size: u64,
    #[serde(with = "irys_types::string_u64")]
    pub header_size: u64,
    pub term_fee: BoundedFee,
    pub ledger_id: u32,
    #[serde(with = "irys_types::string_u64")]
    pub chain_id: u64,
    pub signature: IrysSignature,
    #[serde(default, with = "irys_types::optional_string_u64")]
    pub bundle_format: Option<u64>,
    #[serde(default)]
    pub perm_fee: Option<BoundedFee>,
}

/// Sovereign wire type for DataTransactionHeader (versioned, IntegerTagged-compatible).
#[derive(Debug, Clone, PartialEq)]
pub enum DataTransactionHeader {
    V1(DataTransactionHeaderV1Inner),
}

impl Serialize for DataTransactionHeader {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let (version, inner_value) = match self {
            Self::V1(inner) => (1u8, serde_json::to_value(inner).map_err(serde::ser::Error::custom)?),
        };
        if let serde_json::Value::Object(inner_map) = inner_value {
            let mut map = serializer.serialize_map(Some(inner_map.len() + 1))?;
            map.serialize_entry("version", &version)?;
            for (key, value) in inner_map {
                map.serialize_entry(&key, &value)?;
            }
            map.end()
        } else {
            Err(serde::ser::Error::custom("inner value must be a struct"))
        }
    }
}

impl<'de> Deserialize<'de> for DataTransactionHeader {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let serde_json::Value::Object(obj) = value {
            let mut version: Option<u8> = None;
            let mut fields = serde_json::Map::new();
            for (key, val) in obj {
                if key == "version" {
                    version = Some(serde_json::from_value(val).map_err(serde::de::Error::custom)?);
                } else {
                    fields.insert(key, val);
                }
            }
            match version {
                Some(1) => {
                    let inner: DataTransactionHeaderV1Inner =
                        serde_json::from_value(serde_json::Value::Object(fields))
                            .map_err(serde::de::Error::custom)?;
                    Ok(Self::V1(inner))
                }
                Some(v) => Err(serde::de::Error::custom(format!("unknown version: {}", v))),
                None => Err(serde::de::Error::missing_field("version")),
            }
        } else {
            Err(serde::de::Error::custom("expected object"))
        }
    }
}

// -- Conversions --

impl From<&irys_types::DataTransactionHeader> for DataTransactionHeader {
    fn from(h: &irys_types::DataTransactionHeader) -> Self {
        match h {
            irys_types::DataTransactionHeader::V1(wm) => Self::V1(DataTransactionHeaderV1Inner {
                id: wm.tx.id,
                anchor: wm.tx.anchor,
                signer: wm.tx.signer,
                data_root: wm.tx.data_root,
                data_size: wm.tx.data_size,
                header_size: wm.tx.header_size,
                term_fee: wm.tx.term_fee,
                ledger_id: wm.tx.ledger_id,
                chain_id: wm.tx.chain_id,
                signature: wm.tx.signature,
                bundle_format: wm.tx.bundle_format,
                perm_fee: wm.tx.perm_fee,
            }),
        }
    }
}

impl TryFrom<DataTransactionHeader> for irys_types::DataTransactionHeader {
    type Error = eyre::Report;
    fn try_from(h: DataTransactionHeader) -> eyre::Result<Self> {
        match h {
            DataTransactionHeader::V1(inner) => {
                Ok(irys_types::DataTransactionHeader::V1(
                    irys_types::DataTransactionHeaderV1WithMetadata {
                        tx: irys_types::DataTransactionHeaderV1 {
                            id: inner.id,
                            anchor: inner.anchor,
                            signer: inner.signer,
                            data_root: inner.data_root,
                            data_size: inner.data_size,
                            header_size: inner.header_size,
                            term_fee: inner.term_fee,
                            ledger_id: inner.ledger_id,
                            chain_id: inner.chain_id,
                            signature: inner.signature,
                            bundle_format: inner.bundle_format,
                            perm_fee: inner.perm_fee,
                        },
                        metadata: Default::default(),
                    },
                ))
            }
        }
    }
}
```

**Step 2: Add to mod.rs, verify, commit**

Run: `cargo check -p irys-p2p`

```bash
git add crates/p2p/src/wire_types/transaction.rs crates/p2p/src/wire_types/mod.rs
git commit -m "feat: add sovereign transaction wire types"
```

---

### Task 5: Add block wire types

**Files:**
- Create: `crates/p2p/src/wire_types/block.rs`
- Modify: `crates/p2p/src/wire_types/mod.rs`

Reference: `crates/types/src/block.rs`

This task covers: `PoaData`, `VDFLimiterInfo`, `DataTransactionLedger`, `SystemTransactionLedger`, `IrysBlockHeader` (IntegerTagged), and `BlockBody`.

Note: `IrysBlockHeader` uses IntegerTagged with `#[integer_tagged(tag = "version")]`. Its JSON is flattened with camelCase fields. `BlockBody` is a plain struct with `block_hash`, `data_transactions` (Vec<DataTransactionHeader>), `commitment_transactions` (Vec<CommitmentTransaction>) — these inner types are the wire types from Tasks 3-4.

**Step 1: Create `crates/p2p/src/wire_types/block.rs`**

This is the largest file. Key types:

```rust
use irys_types::{
    serialization::{Base64, H256List, IngressProofsList},
    storage_pricing::Amount,
    BlockHash, IrysAddress, IrysSignature, PartitionHash, UnixTimestampMs, H256, U256,
};
use reth::revm::primitives::B256;
use serde::{Deserialize, Serialize};

use super::{CommitmentTransaction, DataTransactionHeader};

// PoaData — plain struct, no versioning
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PoaData {
    pub partition_chunk_offset: u32,
    pub partition_hash: PartitionHash,
    pub chunk: Option<Base64>,
    pub ledger_id: Option<u32>,
    pub tx_path: Option<Base64>,
    pub data_path: Option<Base64>,
}

// VDFLimiterInfo — plain struct
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VDFLimiterInfo {
    pub output: H256,
    #[serde(with = "irys_types::string_u64")]
    pub global_step_number: u64,
    pub seed: H256,
    pub next_seed: H256,
    pub prev_output: H256,
    pub last_step_checkpoints: H256List,
    pub steps: H256List,
    #[serde(default, with = "irys_types::option_u64_stringify")]
    pub vdf_difficulty: Option<u64>,
    #[serde(default, with = "irys_types::option_u64_stringify")]
    pub next_vdf_difficulty: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DataTransactionLedger {
    pub ledger_id: u32,
    pub tx_root: H256,
    pub tx_ids: H256List,
    #[serde(default, with = "irys_types::u64_stringify")]
    pub total_chunks: u64,
    #[serde(default, skip_serializing_if = "Option::is_none", with = "irys_types::optional_string_u64")]
    pub expires: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proofs: Option<IngressProofsList>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_proof_count: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SystemTransactionLedger {
    pub ledger_id: u32,
    pub tx_ids: H256List,
}

// IrysBlockHeaderV1 inner fields
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IrysBlockHeaderV1Inner {
    pub block_hash: BlockHash,
    pub signature: IrysSignature,
    #[serde(with = "irys_types::string_u64")]
    pub height: u64,
    pub diff: U256,
    pub cumulative_diff: U256,
    pub solution_hash: H256,
    pub last_diff_timestamp: UnixTimestampMs,
    pub previous_solution_hash: H256,
    pub last_epoch_hash: H256,
    pub chunk_hash: H256,
    pub previous_block_hash: H256,
    pub previous_cumulative_diff: U256,
    pub poa: PoaData,
    pub reward_address: IrysAddress,
    pub reward_amount: U256,
    pub miner_address: IrysAddress,
    pub timestamp: UnixTimestampMs,
    pub system_ledgers: Vec<SystemTransactionLedger>,
    pub data_ledgers: Vec<DataTransactionLedger>,
    pub evm_block_hash: B256,
    pub vdf_limiter_info: VDFLimiterInfo,
    pub oracle_irys_price: Amount<(irys_types::storage_pricing::phantoms::IrysPrice, irys_types::storage_pricing::phantoms::Usd)>,
    pub ema_irys_price: Amount<(irys_types::storage_pricing::phantoms::IrysPrice, irys_types::storage_pricing::phantoms::Usd)>,
    pub treasury: U256,
}

/// Sovereign wire type for IrysBlockHeader (IntegerTagged-compatible).
#[derive(Debug, Clone, PartialEq)]
pub enum IrysBlockHeader {
    V1(IrysBlockHeaderV1Inner),
}

// Hand-written serde matching IntegerTagged flattened JSON
impl Serialize for IrysBlockHeader {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let (version, inner_value) = match self {
            Self::V1(inner) => (1u8, serde_json::to_value(inner).map_err(serde::ser::Error::custom)?),
        };
        if let serde_json::Value::Object(inner_map) = inner_value {
            let mut map = serializer.serialize_map(Some(inner_map.len() + 1))?;
            map.serialize_entry("version", &version)?;
            for (key, value) in inner_map {
                map.serialize_entry(&key, &value)?;
            }
            map.end()
        } else {
            Err(serde::ser::Error::custom("inner value must be a struct"))
        }
    }
}

impl<'de> Deserialize<'de> for IrysBlockHeader {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        if let serde_json::Value::Object(obj) = value {
            let mut version: Option<u8> = None;
            let mut fields = serde_json::Map::new();
            for (key, val) in obj {
                if key == "version" {
                    version = Some(serde_json::from_value(val).map_err(serde::de::Error::custom)?);
                } else {
                    fields.insert(key, val);
                }
            }
            match version {
                Some(1) => {
                    let inner: IrysBlockHeaderV1Inner =
                        serde_json::from_value(serde_json::Value::Object(fields))
                            .map_err(serde::de::Error::custom)?;
                    Ok(Self::V1(inner))
                }
                Some(v) => Err(serde::de::Error::custom(format!("unknown version: {}", v))),
                None => Err(serde::de::Error::missing_field("version")),
            }
        } else {
            Err(serde::de::Error::custom("expected object"))
        }
    }
}

/// Sovereign wire type for BlockBody.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlockBody {
    pub block_hash: BlockHash,
    pub data_transactions: Vec<DataTransactionHeader>,
    pub commitment_transactions: Vec<CommitmentTransaction>,
}

// -- Conversions for PoaData, VDFLimiterInfo, ledgers, IrysBlockHeader, BlockBody --
// (Implement From<&irys_types::X> for wire::X and TryFrom<wire::X> for irys_types::X)
// These are mechanical field-by-field copies.
```

The full conversion implementations are mechanical — copy each field. For nested types like `PoaData` and `VDFLimiterInfo`, convert inner fields. For `IrysBlockHeader`, convert the `V1` inner. For `BlockBody`, convert each `DataTransactionHeader` and `CommitmentTransaction` in the vectors.

**Important:** The `IrysTokenPrice` type is `Amount<(IrysPrice, Usd)>` which serializes as `{"amount": "..."}`. This is a generic type from `irys_types::storage_pricing`. Since it's parameterized with phantom types and has stable serde, import it directly rather than copying.

**Step 2: Add to mod.rs, verify, commit**

Run: `cargo check -p irys-p2p`

```bash
git add crates/p2p/src/wire_types/block.rs crates/p2p/src/wire_types/mod.rs
git commit -m "feat: add sovereign block wire types"
```

---

### Task 6: Add gossip envelope wire types

**Files:**
- Create: `crates/p2p/src/wire_types/gossip.rs`
- Modify: `crates/p2p/src/wire_types/mod.rs`

Reference: `crates/types/src/gossip.rs`

These are the top-level envelopes that wrap the inner types. `GossipDataV1/V2` and `GossipDataRequestV1/V2` use standard serde derive (externally tagged enums). `GossipRequestV1/V2` are simple wrapper structs.

Note: `GossipDataV1::Block` wraps `Arc<IrysBlockHeader>` — the wire type uses the sovereign `IrysBlockHeader` directly (no Arc needed for serde). `ExecutionPayload` wraps `reth_ethereum_primitives::Block` — this stays as-is since it's an external type with stable serde.

```rust
use std::sync::Arc;
use irys_types::{BlockHash, ChunkPathHash, IrysAddress, IrysPeerId, H256};
use reth::revm::primitives::B256;
use reth_ethereum_primitives::Block as RethBlock;
use serde::{Deserialize, Serialize};

use super::{
    BlockBody, CommitmentTransaction, DataTransactionHeader, IngressProof, IrysBlockHeader,
    UnpackedChunk,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipDataV1 {
    Chunk(UnpackedChunk),
    Transaction(DataTransactionHeader),
    CommitmentTransaction(CommitmentTransaction),
    Block(IrysBlockHeader),
    ExecutionPayload(RethBlock),
    IngressProof(IngressProof),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipDataV2 {
    Chunk(UnpackedChunk),
    Transaction(DataTransactionHeader),
    CommitmentTransaction(CommitmentTransaction),
    BlockHeader(IrysBlockHeader),
    BlockBody(BlockBody),
    ExecutionPayload(RethBlock),
    IngressProof(IngressProof),
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum GossipDataRequestV1 {
    ExecutionPayload(B256),
    Block(BlockHash),
    Chunk(ChunkPathHash),
    Transaction(H256),
}

#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum GossipDataRequestV2 {
    ExecutionPayload(B256),
    BlockHeader(BlockHash),
    BlockBody(BlockHash),
    Chunk(ChunkPathHash),
    Transaction(H256),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequestV1<T> {
    pub miner_address: IrysAddress,
    pub data: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequestV2<T> {
    pub peer_id: IrysPeerId,
    pub miner_address: IrysAddress,
    pub data: T,
}
```

Conversions: Each variant converts its inner type. `From<&irys_types::GossipDataV2>` maps each variant, unwrapping Arc and converting inner types. The reverse conversion is `TryFrom` since inner type conversions can fail.

**Step 1: Implement the full file with conversions, add to mod.rs**

**Step 2: Verify, commit**

```bash
git add crates/p2p/src/wire_types/gossip.rs crates/p2p/src/wire_types/mod.rs
git commit -m "feat: add sovereign gossip envelope wire types"
```

---

### Task 7: Add handshake and response wire types

**Files:**
- Create: `crates/p2p/src/wire_types/handshake.rs`
- Create: `crates/p2p/src/wire_types/response.rs`
- Modify: `crates/p2p/src/wire_types/mod.rs`

Reference: `crates/types/src/version.rs` and `crates/p2p/src/types.rs`

**Handshake types** — these are plain structs with no IntegerTagged complexity.
**Response types** — `GossipResponse<T>`, `RejectionReason`, `HandshakeRequirementReason` — simple enums.
**NodeInfo** — plain struct with camelCase serde.
**PeerAddress**, **RethPeerInfo** — plain structs.

Note: `PeerAddress` and `RethPeerInfo` have `#[serde(deny_unknown_fields)]` — include this in sovereign copies.

```rust
// handshake.rs
use irys_types::{IrysAddress, IrysPeerId, IrysSignature, ProtocolVersion, H256};
use semver::Version;
use serde::{Deserialize, Serialize};
// ... struct definitions matching the originals exactly
```

```rust
// response.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GossipResponse<T> {
    Accepted(T),
    Rejected(RejectionReason),
}

#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
pub enum HandshakeRequirementReason {
    RequestOriginIsNotInThePeerList,
    RequestOriginDoesNotMatchExpected,
    MinerAddressIsUnknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
pub enum RejectionReason {
    HandshakeRequired(Option<HandshakeRequirementReason>),
    GossipDisabled,
    InvalidData,
    RateLimited,
    UnableToVerifyOrigin,
    InvalidCredentials,
    ProtocolMismatch,
    UnsupportedProtocolVersion(u32),
    UnsupportedFeature,
}
```

Conversions are straightforward field copies. `GossipResponse` conversions are generic over `T`.

**Step: Implement both files, add to mod.rs, verify, commit**

```bash
git add crates/p2p/src/wire_types/handshake.rs crates/p2p/src/wire_types/response.rs crates/p2p/src/wire_types/mod.rs
git commit -m "feat: add sovereign handshake and response wire types"
```

---

### Task 8: Write fixture parity tests

**Files:**
- Create: `crates/p2p/src/wire_types/tests.rs`
- Modify: `crates/p2p/src/wire_types/mod.rs`

Write tests that verify every sovereign wire type produces **identical JSON** to the canonical irys_types type. This uses the same fixture approach as `gossip_fixture_tests.rs` but compares wire type output against canonical type output.

```rust
#[cfg(test)]
mod tests;
```

Each test:
1. Construct a canonical irys_types value (reuse fixture constructors from gossip_fixture_tests.rs or duplicate)
2. Convert to sovereign wire type via `From`
3. Serialize both to JSON
4. Assert identical output
5. Deserialize sovereign JSON, convert back to canonical, verify fields match

This is the critical validation: if the sovereign type ever drifts from the canonical type's JSON format, these tests catch it.

**Step 1: Write the test file**

**Step 2: Run tests**

Run: `cargo nextest run -p irys-p2p wire_types`
Expected: All pass

**Step 3: Commit**

```bash
git add crates/p2p/src/wire_types/tests.rs crates/p2p/src/wire_types/mod.rs
git commit -m "test: add wire type JSON parity tests"
```

---

### Task 9: Update gossip_fixture_tests.rs to use wire types

**Files:**
- Modify: `crates/p2p/src/gossip_fixture_tests.rs`

Update the existing fixture tests to construct and test sovereign wire types instead of (or in addition to) canonical types. This provides the long-term protection: fixture tests lock down wire type JSON, and wire types are decoupled from irys_types.

The key change: import from `crate::wire_types` instead of from `irys_types` for all types that have sovereign copies.

**Step 1: Update imports and test bodies**

Replace:
```rust
use irys_types::gossip::{v1::GossipDataV1, ...};
```
With:
```rust
use crate::wire_types::{GossipDataV1, ...};
```

Update fixture constructors to build wire types directly.

**Step 2: Run all fixture tests**

Run: `cargo nextest run -p irys-p2p gossip_fixture`
Expected: All 48+ tests pass with identical JSON (since wire types produce the same output)

**Step 3: Commit**

```bash
git add crates/p2p/src/gossip_fixture_tests.rs
git commit -m "test: update fixture tests to use sovereign wire types"
```

---

### Task 10: Wire up server.rs deserialization boundary

**Files:**
- Modify: `crates/p2p/src/server.rs`

The actix-web handlers currently deserialize directly into irys_types types like `web::Json<GossipRequest<UnpackedChunk>>`. Change them to deserialize into wire types, then convert.

**Pattern for each V2 handler:**

Before:
```rust
async fn handle_chunk_v2(request: web::Json<GossipRequestV2<UnpackedChunk>>, ...) {
    let request = request.into_inner();
    // use request.data directly
}
```

After:
```rust
async fn handle_chunk_v2(request: web::Json<wire_types::GossipRequestV2<wire_types::UnpackedChunk>>, ...) {
    let wire_request = request.into_inner();
    let canonical_request = irys_types::GossipRequestV2 {
        peer_id: wire_request.peer_id,
        miner_address: wire_request.miner_address,
        data: wire_request.data.into(), // From<wire::UnpackedChunk> for irys_types::UnpackedChunk
    };
    // use canonical_request.data
}
```

Apply this pattern to all ~14 V1 and V2 handlers. V1 handlers also do the V1→V2 conversion after wire→canonical conversion.

For data request/pull handlers, the request type becomes `wire_types::GossipRequestV2<wire_types::GossipDataRequestV2>`.

For handshake handlers, use `wire_types::HandshakeRequestV1/V2`.

**Important:** Response types also need updating. Handlers return `HttpResponse::Ok().json(GossipResponse::Accepted(...))` — use wire types for the response serialization.

**Step 1: Update imports in server.rs**

**Step 2: Update each handler function signature and body**

Work through handlers one by one. Each change is small (add `.into()` or `try_into()?` calls).

**Step 3: Verify compilation**

Run: `cargo check -p irys-p2p`

**Step 4: Run tests**

Run: `cargo nextest run -p irys-p2p`

**Step 5: Commit**

```bash
git add crates/p2p/src/server.rs
git commit -m "refactor: use wire types for server deserialization boundary"
```

---

### Task 11: Wire up gossip_client.rs serialization boundary

**Files:**
- Modify: `crates/p2p/src/gossip_client.rs`

The client currently serializes irys_types types directly. Change it to convert to wire types before serialization.

**Key functions to update:**

1. `pre_serialize_for_broadcast()` (line ~825): Convert each `GossipDataV2` variant to wire type before `serde_json::to_vec()`
2. `send_data_internal()` (line ~1156): Convert data to wire type before `.json(&req)`
3. `create_request_v1/v2()` (line ~1378): Return wire type requests
4. Handshake methods: `post_handshake_v1/v2()` — convert handshake request to wire type, deserialize response as wire type
5. Response parsing: `serde_json::from_str` calls should parse wire types, then convert

**Step 1: Update pre_serialize_for_broadcast()**

This is the most performance-critical path (broadcasts to many peers). Convert the `GossipDataV2` to wire type once, serialize once.

**Step 2: Update remaining methods**

**Step 3: Verify compilation and tests**

Run: `cargo check -p irys-p2p && cargo nextest run -p irys-p2p`

**Step 4: Commit**

```bash
git add crates/p2p/src/gossip_client.rs
git commit -m "refactor: use wire types for client serialization boundary"
```

---

### Task 12: Wire up gossip_data_handler.rs and chain_sync.rs

**Files:**
- Modify: `crates/p2p/src/gossip_data_handler.rs` (if needed — may already work if server converts before passing)
- Modify: `crates/p2p/src/chain_sync.rs` (if it directly serializes/deserializes gossip types)

These files primarily work with canonical types after the server has deserialized. If server.rs converts wire→canonical before passing to handlers, these files may need no changes.

Review `handle_get_data_sync()` which returns `Option<GossipDataV2>` — if this response gets serialized back to JSON via server.rs, the server's response handler needs to convert canonical→wire.

**Step 1: Audit remaining serialization boundaries**

**Step 2: Make minimal changes (likely response conversion in server.rs for get_data/pull_data)**

**Step 3: Verify, commit**

```bash
git add crates/p2p/src/gossip_data_handler.rs crates/p2p/src/chain_sync.rs crates/p2p/src/server.rs
git commit -m "refactor: complete wire type boundary for data handler and sync"
```

---

### Task 13: Run full test suite and clean up

**Step 1: Run full p2p tests**

Run: `cargo nextest run -p irys-p2p`
Expected: All tests pass

**Step 2: Run clippy**

Run: `cargo clippy -p irys-p2p --tests --all-targets`
Expected: No warnings

**Step 3: Run fmt**

Run: `cargo fmt --all`

**Step 4: Run broader checks**

Run: `cargo xtask check` (compile check across workspace)
Expected: Clean

**Step 5: Final commit**

```bash
git add -A
git commit -m "chore: clean up wire type implementation"
```

---

## Summary of Files Created/Modified

**Created (~8 files):**
- `crates/p2p/src/wire_types/mod.rs`
- `crates/p2p/src/wire_types/chunk.rs`
- `crates/p2p/src/wire_types/ingress.rs`
- `crates/p2p/src/wire_types/commitment.rs`
- `crates/p2p/src/wire_types/transaction.rs`
- `crates/p2p/src/wire_types/block.rs`
- `crates/p2p/src/wire_types/gossip.rs`
- `crates/p2p/src/wire_types/handshake.rs`
- `crates/p2p/src/wire_types/response.rs`
- `crates/p2p/src/wire_types/tests.rs`

**Modified (~6 files):**
- `crates/types/src/serialization.rs` (or lib.rs — expose serde helpers)
- `crates/p2p/src/lib.rs` (add wire_types module)
- `crates/p2p/src/server.rs` (use wire types at deserialization boundary)
- `crates/p2p/src/gossip_client.rs` (use wire types at serialization boundary)
- `crates/p2p/src/gossip_fixture_tests.rs` (test wire types)
- `crates/p2p/src/gossip_data_handler.rs` (minor — response conversion if needed)
