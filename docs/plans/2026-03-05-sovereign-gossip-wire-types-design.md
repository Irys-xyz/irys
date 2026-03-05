# Sovereign Gossip Wire Types

## Problem

Changes to types in `irys-types` can silently break the gossip wire format. Since gossip uses JSON serialization over HTTP, any serde attribute change, field rename, or enum variant reorder in shared types can cause protocol incompatibility between nodes running different versions.

## Solution

Create sovereign copies of all wire-serialized types within the p2p crate. These types own their serde configuration and are decoupled from `irys-types` internals. Conversion happens at the p2p boundary only.

## Approach: Sovereign Wire Types for All Serialized Structs

Copy gossip envelopes AND all structs that serialize over the wire into a `wire_types` module in the p2p crate. Keep primitive wrappers (`H256`, `U256`, `IrysAddress`, `IrysSignature`, `Base64`, `H256List`, `TxChunkOffset`, `UnixTimestampMs`, `B256`, `BoundedFee`, `ChainId`, `ProtocolVersion`) from `irys-types`.

## Module Structure

```
crates/p2p/src/
  wire_types/
    mod.rs          — re-exports
    block.rs        — IrysBlockHeader, VDFLimiterInfo, PoaData, DataTransactionLedger, SystemTransactionLedger, BlockBody
    transaction.rs  — DataTransactionHeader, DataTransactionHeaderWithMetadata
    commitment.rs   — CommitmentTransaction, CommitmentType (V1+V2)
    ingress.rs      — IngressProof
    chunk.rs        — UnpackedChunk
    gossip.rs       — GossipDataV1/V2, GossipDataRequestV1/V2, GossipRequestV1/V2
    handshake.rs    — HandshakeRequest/Response V1/V2, NodeInfo, PeerAddress, RethPeerInfo
    response.rs     — GossipResponse, RejectionReason
```

## Primitives (Shared, Not Copied)

These stay as imports from `irys-types` because they have stable, well-tested serde implementations (base58, decimal strings, base64url) that are unlikely to change:

- `H256`, `U256`, `IrysAddress`, `IrysSignature`, `Base64`
- `H256List`, `TxChunkOffset`, `UnixTimestampMs`
- `B256`, `BoundedFee`, `ChainId`, `ProtocolVersion`

## Conversion Strategy

Each sovereign type implements:

- `From<&irys_types::CanonicalType>` — canonical to wire (serialization path)
- `TryFrom<WireType> for irys_types::CanonicalType` — wire to canonical (deserialization path, fallible)

Envelope types convert recursively.

## Serde Configuration

Every sovereign type gets explicit `#[serde(...)]` attributes. For versioned enums currently using `IntegerTagged`, sovereign copies use hand-written or explicit serde impls that produce identical JSON. This locks down wire format independently of macro behavior.

## Integration Points

- **Gossip server** (deserialization): HTTP JSON -> wire type -> TryInto -> canonical type
- **Gossip client** (serialization): canonical type -> From -> wire type -> serde_json
- **Broadcast channels** in actors: continue using `irys_types` types unchanged
- **Fixture tests**: updated to test sovereign wire types

## Affected Files (~15)

- `crates/p2p/src/gossip_client.rs` — wrap outgoing messages
- `crates/p2p/src/server.rs` — deserialize into wire types
- `crates/p2p/src/gossip_service.rs` — handshake boundaries
- `crates/p2p/src/gossip_data_handler.rs` — minor adjustments
- `crates/p2p/src/chain_sync.rs` — request/response boundaries

## Out of Scope

- RLP encoding (block hashing, not gossip)
- Compact encoding (database storage)
- Internal actor message passing
- API server types
