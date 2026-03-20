//! Centralised version enums for the Irys protocol.
//!
//! Every "version axis" in the system lives here so that a single file
//! documents what changed at each version boundary.

use serde::{Deserialize, Serialize};

use crate::Arbitrary;

// ---------------------------------------------------------------------------
// DatabaseVersion
// ---------------------------------------------------------------------------

/// Database schema version.  Add a new variant each time a migration is needed.
///
/// Variants are ordered so that comparisons (`<`, `>`, `==`) work as expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
pub enum DatabaseVersion {
    /// Original database schema
    ///
    /// * `DataTransactionHeaderV1` contains `promoted_height` field inline.
    /// * No `db_version` metadata key exists on disk (the versioning system
    ///   had not been introduced yet).
    ///
    /// Databases at this version are detected at startup by the absence of
    /// the `db_version` metadata key combined with existing data.  Since V0
    /// and V1 share the same record format, in-place migration simply stamps
    /// the `db_version` key as V1 and then continues to V1→V2.
    V0 = 0,

    /// Introduced the `db_version` metadata key.
    ///
    /// Transaction headers store all data inline, including `promoted_height`
    /// embedded directly in `DataTransactionHeaderV1`. No per-transaction
    /// metadata tracking beyond what is in the header itself.
    ///
    /// * Record format is unchanged from V0 — `DataTransactionHeaderV1`
    ///   still contains `promoted_height` inline.
    /// * First version to have the `db_version` metadata key stamped on disk.
    V1 = 1,

    /// Metadata back-fill migration.
    ///
    /// Extracts per-transaction metadata into dedicated tables, separating
    /// lifecycle tracking from transaction headers. Adds `included_height`
    /// (block where tx entered the ledger) and moves `promoted_height`
    /// (block where data tx was promoted from Submit to Publish) out of the
    /// header. Enables the transaction status API (Pending/Confirmed/Finalized)
    /// and makes future metadata additions possible without header format
    /// migrations.
    ///
    /// * Moved the `promoted_height` field out of `DataTransactionHeaderV1`
    ///   and into `IrysDataTxMetadata`, keeping on-chain data minimal.
    /// * Back-filled `IrysDataTxMetadata` rows from existing block headers so
    ///   that every stored transaction has consistent metadata.
    V2 = 2,
}

impl DatabaseVersion {
    /// The current schema version that this binary expects.
    pub const CURRENT: Self = Self::V2;

    /// Returns `Some(version)` if the raw value corresponds to a known variant,
    /// or `None` if the value was written by a newer binary.
    pub fn from_u32(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::V0),
            1 => Some(Self::V1),
            2 => Some(Self::V2),
            _ => None,
        }
    }
}

impl std::fmt::Display for DatabaseVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", *self as u32)
    }
}

// ---------------------------------------------------------------------------
// ProtocolVersion  (P2P wire protocol)
// ---------------------------------------------------------------------------

/// P2P wire-protocol version exchanged during the gossip handshake.
///
/// A node advertises its protocol version so that peers can decide whether
/// they are compatible.  Bump this whenever the handshake or gossip message
/// format changes in a backward-incompatible way.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Hash, Arbitrary,
)]
#[repr(u32)]
pub enum ProtocolVersion {
    /// Original gossip handshake.
    ///
    /// Nodes identify by mining address with ECDSA signature proof. Uses
    /// `reth_codecs::Compact` encoding (Rust-only). Data messages
    /// authenticated by IP matching against handshake registration. No
    /// consensus compatibility check at connection time.
    ///
    /// * Compact-encoded signing preimage
    ///   ([`crate::HandshakeRequestV1`] / [`crate::HandshakeResponseV1`]).
    /// * Peers identified solely by `mining_address`.
    V1 = 1,

    /// Extended handshake with peer identity and config validation.
    ///
    /// Adds a dedicated random `peer_id` separate from mining address, so
    /// nodes can detect multiple peers mining the same address (often from
    /// copy-pasting a config). Switches to standard RLP encoding for
    /// cross-language compatibility on signed handshake messages. Includes
    /// `consensus_config_hash` to reject incompatible peers at handshake
    /// time.
    ///
    /// * Added `peer_id` ([`crate::IrysPeerId`]) for independent P2P identity.
    /// * Added `consensus_config_hash` so peers reject misconfigured nodes
    ///   at connection time instead of at block validation.
    /// * Switched signing preimage to RLP encoding for consistency with the
    ///   rest of the wire format.
    V2 = 2,
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        Self::current()
    }
}

/// Error returned when a `u32` does not correspond to any known
/// [`ProtocolVersion`] variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolVersionParseError(pub u32);

impl std::fmt::Display for ProtocolVersionParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown protocol version: {}", self.0)
    }
}

impl std::error::Error for ProtocolVersionParseError {}

impl TryFrom<u32> for ProtocolVersion {
    type Error = ProtocolVersionParseError;

    fn try_from(v: u32) -> Result<Self, Self::Error> {
        match v {
            1 => Ok(Self::V1),
            2 => Ok(Self::V2),
            _ => Err(ProtocolVersionParseError(v)),
        }
    }
}

impl ProtocolVersion {
    pub const fn current() -> Self {
        Self::V2
    }

    pub fn supported_versions() -> &'static [Self] {
        &[Self::V1, Self::V2]
    }

    pub fn supported_versions_u32() -> &'static [u32] {
        &[Self::V1 as u32, Self::V2 as u32]
    }
}

/// We can't derive these impls directly due to the RLP not supporting repr structs/enums
impl alloy_rlp::Encodable for ProtocolVersion {
    fn encode(&self, out: &mut dyn bytes::BufMut) {
        <u32 as alloy_rlp::Encodable>::encode(&(*self as u32), out);
    }

    fn length(&self) -> usize {
        <u32 as alloy_rlp::Encodable>::length(&(*self as u32))
    }
}

impl alloy_rlp::Decodable for ProtocolVersion {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let value = <u32 as alloy_rlp::Decodable>::decode(buf)?;
        Self::try_from(value).map_err(|_| alloy_rlp::Error::Custom("unknown protocol version"))
    }
}

// ---------------------------------------------------------------------------
// TransactionVersion  (commitment / data transaction format)
// ---------------------------------------------------------------------------

/// Transaction format version for commitment transactions.
///
/// Commitment transactions are the primary versioned transaction type. Data
/// transactions currently have only a single format (V1) and are **not**
/// tracked by this enum.
///
/// The version discriminant is embedded in the [`CommitmentTransaction`](crate::CommitmentTransaction)
/// envelope's `#[repr(u8)]` tag and is written to the wire and to the
/// database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum TransactionVersion {
    /// Original commitment transaction format.
    ///
    /// Supported commitment types:
    /// * `Stake`   – lock tokens to become a validator candidate.
    /// * `Pledge`  – allocate a storage partition.
    /// * `Unpledge` – release a storage partition.
    /// * `Unstake` – withdraw staked tokens.
    ///
    /// Signing preimage: Compact-encoded fields.
    ///
    /// V1 commitment transactions were deprecated by the Aurora hardfork due
    /// to non-canonical RLP encoding incompatibilities across language
    /// implementations. After Aurora activation, only V2+ commitment
    /// transactions are accepted on-chain.
    V1 = 1,

    /// Added the `UpdateRewardAddress` commitment type and switched to RLP
    /// signing.
    ///
    /// Adds the `UpdateRewardAddress` commitment transaction variant,
    /// allowing nodes to change their reward address without unstaking.
    /// Switches signing preimage from Compact encoding to canonical RLP,
    /// aligning with the rest of the protocol's wire format.
    ///
    /// * New commitment type `UpdateRewardAddress` – redirect block rewards
    ///   to a different address without unstaking.
    /// * Signing preimage changed from Compact encoding to canonical RLP.
    V2 = 2,
}

impl TransactionVersion {
    /// The current transaction format version produced by this binary.
    pub const CURRENT: Self = Self::V2;

    /// Returns `Some(version)` if the raw value corresponds to a known variant,
    /// or `None` if the value is unknown.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::V1),
            2 => Some(Self::V2),
            _ => None,
        }
    }
}

impl std::fmt::Display for TransactionVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", *self as u8)
    }
}
