# Determinism Guarantees in Multi-Miner Genesis Construction

## Status
Accepted

## Context

Multi-miner genesis introduces ordering problems that would cause different operators to
generate different block hashes from the same inputs if left unconstrained:

1. **Manifest ordering**: A `genesis_miners.toml` listing miners in different sequences
   must still produce the same genesis block.
2. **Commitment ordering in the ledger**: The genesis block's system ledger records
   commitment txids; different orderings produce different block hashes.
3. **Transaction ID uniqueness**: Stake and pledge commitments use an `anchor` field
   that feeds into the txid. Without a globally-threaded anchor chain, two miners with
   identical parameters would produce identical txids — causing duplicate entries in the
   ledger that inflate the treasury.
4. **Pledge fee indexing**: Pledge fees increase with pledge count. The question is
   whether each miner's fee curve starts independently at 0 or continues a global counter.

See also: [Standalone Genesis Builder Module](genesis-builder-module.md)

## Decision

Four invariants enforce full determinism:

**1. Miners canonically ordered by `IrysAddress`.**
`GenesisMinerManifest::into_entries()` sorts miner entries by their derived `IrysAddress`
before returning them. `build_signed_genesis_block()` asserts this invariant at the call
boundary. Manifest file order is therefore irrelevant to the resulting block hash.
Duplicate keys are detected by comparing adjacent entries after the sort (identical keys
produce identical addresses) and are rejected with an error.

**2. Commitments sorted by txid in the ledger.**
After generating all commitments (key-based path) or receiving them (commitment-based
path), both paths sort commitments by their raw txid bytes before appending to the
genesis block's commitment ledger. The block hash is therefore independent of generation
order or JSON input order.

**3. Anchors thread globally across all miners.**
A single `anchor` variable threads through every commitment across every miner
sequentially. Each commitment's txid becomes the next commitment's anchor. Per-miner
anchor resets were rejected: two miners submitting identical first-pledge parameters
would otherwise produce identical txids.

**4. Per-miner pledge fee indexing.**
The pledge fee index (`i` in `new_pledge(consensus, anchor, &i, addr)`) resets to 0 for
each miner's first pledge rather than incrementing globally. This preserves compatibility
with the existing single-miner path (`get_genesis_commitments`), where all pledges
belonged to one miner and the index was always that miner's own count. A global counter
would change each miner's fee — and thus the genesis treasury — compared to what they
would have paid if they had been the sole miner.

## Consequences

- Two operators building genesis from the same manifest and config always produce the
  same block hash regardless of manifest file ordering or commitment generation order
- The `IrysAddress`-sort order is protocol-level; changing it would invalidate any
  genesis block already in production
- The commitment-based path is also deterministic: commitments are sorted by txid
  regardless of JSON input order, and validation rejects duplicates
- `build_signed_genesis_block` enforces the ordering invariant with an `eyre::ensure!`
  on miner pairs, catching direct callers that bypass `GenesisMinerManifest::into_entries`

## Source

Plan: `docs/plans/2026-03-05-genesis-cli-design.md` — Standalone Genesis Block CLI
Implementation Plan
