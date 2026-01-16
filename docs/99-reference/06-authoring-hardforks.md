# Adding Hardforks to Irys

Irys hardforks activate at specific Unix timestamps, allowing synchronized activation with EVM hardforks.

## 1. Define the hardfork struct in `hardfork_config.rs`

```rust
/// The Aurora hardfork deprecates V1 Commitment Transactions due to
/// nonstandard RLP encoding incompatibilities across language implementations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Aurora {
    /// Activation timestamp (seconds since epoch)
    pub activation_timestamp: UnixTimestamp,

    /// Minimum valid commitment transaction version when active
    pub minimum_commitment_tx_version: u8,
}
```

Add utility methods to `IrysHardforkConfig`:

```rust
/// Get Aurora config if active at timestamp
pub fn aurora_at(&self, timestamp: UnixTimestamp) -> Option<&Aurora> {
    self.aurora
        .as_ref()
        .filter(|f| timestamp >= f.activation_timestamp)
}

/// Get minimum commitment tx version at timestamp
pub fn minimum_commitment_tx_version_at(&self, timestamp: UnixTimestamp) -> Option<u8> {
    self.aurora_at(timestamp)
        .map(|f| f.minimum_commitment_tx_version)
}
```

Add a component test to validate the activation time.

## 2. Implement hardfork logic

Example: `post_commitment_tx` endpoint enforcing V2 transactions post-Aurora:

```rust
pub async fn post_commitment_tx(
    state: web::Data<ApiState>,
    body: Json<CommitmentTransaction>,
) -> Result<HttpResponse, ApiError> {
    let tx = body.into_inner();

    // Check Aurora hardfork status
    let now = UnixTimestamp::from_secs(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    
    // Enforce minimum commitment tx version if Aurora active
    if let Some(aurora) = state.config.consensus.hardforks.aurora_at(now) {
        let version = tx.version();
        if version < aurora.minimum_commitment_tx_version {
            return Err(ApiError::InvalidTransactionVersion {
                version,
                minimum: aurora.minimum_commitment_tx_version,
            });
        }
    }
    // ...
}
```

> **Note:** Also add validation to block pre-validation and block production (`get_best_mempool_tx()`) if necessary.

## 3. Update `chainspec.rs` (if EVM changes required)

Add to the hardfork macro:

```rust
hardfork!(
    #[derive(serde::Serialize, serde::Deserialize)]
    IrysHardfork {
        Frontier,
        Aurora  // Add new hardfork
    }
);
```

Update [ethereum_hardfork_mapping()](https://github.com/Irys-xyz/irys/blob/3fd8226cf3fa1d57b29b4c60a74d90243db250b5/crates/types/src/chainspec.rs#L82-L117) to map Irys hardforks to EVM hardforks if there are any.

> **Note:** Skip this step if the hardfork doesn't affect the EVM.