# Migration & Versioning Strategy 

## Types of Data

### 🧾 1. **Canonical Data**

**Persistent • Verifiable • Historical**

This is data that's signed and persisted as part of the chain’s verifiable history. Versioning of this type of data must preserve old formats indefinitely.

- **Strategy**
    - Strict forward-compatibility
    - Enum-based schema evolution
    - Never mutate past versions
- **Examples**:
    - `BlockHeader`, `DataTransactionHeader`
    - `CommitmentTransaction`, `IngressProof`


```rust
// Version enum for discriminating header types
pub enum HeaderVersion {
	V1 = 1,
	V2 = 2,
	V3 = 3,
}

// The main versioned enum that gets Compacted and stored in MDBX
pub enum DataTransactionHeader {
	V1(DataTxHeaderV1),
	V2(DataTxHeaderV2),
	V3(DataTxHeaderV3),
}

// Rest of the codebase refers to common trait for all header versions
pub trait TransactionHeader {
	fn transaction_id(&self) -> &[u8; 32];
	fn signature(&self) -> &[u8; 64];
	fn version(&self) -> HeaderVersion;

	// Optional methods with default implementations
	fn anchor(&self) -> Option<H256> { None }
	fn gas_limit(&self) -> Option<u64> { None }
	...
}

```
	
---
### 🧠 2. **Internal Runtime Data**

**Volatile • Node-Scoped • Format-Flexible**

This is ephemeral data used for lookups, caching, and indexing. It's never transmitted or persisted in a way that requires versioning across upgrades.

- **Strategy**:
    - Loose versioning, handled by DB migrations
    - Tracked in a `Metadata` table
    - Only the current schema matters

```rust
table Metadata {
	type Key = MetadataKey;
	type Value = Vec<u8>; // Stores current schema version 
}
```

- **Examples**:    
    - `BlockIndex`, `ChunkCache`, `ValidTxCache`

---
### 📡 3. **Transient Protocol Data**

**Over-the-Wire • Ephemeral • Compatibility-Focused**

This type of data structures is used for network I/O (e.g. HTTP API and P2P). Not stored, but must be version-tolerant for interop.

- **Strategy**:
    - Include explicit version or type tags
    - Use `Option` fields for additive changes
    - Consider enum-based versioning for breaking changes
    - Ensure handlers remain forward/backward compatible
        
- **Examples**:
    - `PackedChunk`, `UnpackedChunk`, version handshake payloads
 
```rust
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChunkFormat { 	
	Unpacked(UnpackedChunk), 	
	Packed(PackedChunk), 
}
```

```rust
{   
	"type": "PackedChunk",   
	... // PackedChunk fields
}
```

Adding a new field like `packingIterations` can be handled either via:
1. `Option<u32>` for non-breaking evolution
2. Versioned enums (e.g. `PackedChunkV1`, `PackedChunkV2`) for explicit decoding paths