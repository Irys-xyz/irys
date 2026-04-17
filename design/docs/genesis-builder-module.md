# Standalone Genesis Builder Module

## Status

Accepted

## Context

Genesis block construction was previously embedded in `IrysNode::create_new_genesis_block()`,
requiring a running node instance to produce a genesis block. Two new requirements made
this untenable:

1. **CLI tooling**: Operators need to build genesis blocks offline — before any node is
   running — to configure multi-node network launches from a declarative miner manifest.

2. **Offline commitment signing**: In deployments where individual miners sign their own
   commitments independently and hand them to a single operator, the block assembler
   must accept pre-signed commitments rather than re-deriving them from private keys it
   may not hold.

## Decision

Extract all genesis block construction into `crates/chain/src/genesis_builder.rs` as
two independent, publicly callable paths:

**Path 1 — Key-based (`build_signed_genesis_block`)**: Accepts a `Config` and a slice
of `GenesisMinerEntry` (signing key + pledge count). Generates stake and pledge
commitments internally, signs them, and assembles the genesis block. The first entry
in the sorted slice signs the block header. Used by the `build-genesis --miners` CLI
flag and by `IrysNode::create_new_genesis_block()`.

**Path 2 — Commitment-based (`build_genesis_block_from_commitments`)**: Accepts a
`Config`, pre-signed `CommitmentTransaction` objects, and a separate block-signing key.
Validates the commitments (signature validity, no duplicate txids, stake+pledge type
constraints, every pledger has a stake), then assembles the genesis block. Used by the
`build-genesis --commitments` CLI flag, allowing miners to sign their own commitments
offline and deliver them to an operator.

Both paths share internal helpers — `prepare_unsigned_genesis` (timestamp, reth chain
spec, unsigned block) and `finalize_genesis_block` (difficulty, VDF, block signature,
logging) — to keep the two commitment-generation strategies from diverging on the
surrounding logic.

`IrysNode::create_new_genesis_block()` is refactored to delegate to path 1 with a
single `GenesisMinerEntry` built from the node's own config and storage submodule
count, preserving exact behavioral compatibility with the pre-extraction implementation.

`validate_genesis_commitments` is also exposed as a standalone function for use in
tests and custom tooling.

## Consequences

- Genesis block construction is testable in isolation without instantiating a full `IrysNode`
- Operators can build genesis blocks from a TOML miner manifest (`genesis_miners.toml`)
  without running a node
- The commitment-based path supports offline signing workflows (e.g., a hardware-security
  module per miner, or an airgapped ceremony)
- `IrysNode::create_new_genesis_block()` delegates to the same code path, eliminating
  the risk of behavioral divergence between node-launched and CLI-built genesis blocks
- A commitment-based path was not in the original plan but emerged as a natural dual of
  the key-based path; both are needed to cover all realistic deployment topologies


