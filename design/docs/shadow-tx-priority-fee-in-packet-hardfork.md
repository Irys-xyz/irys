# Hardfork (deferred): Embed shadow-tx priority fee in the borsh packet

## Status

**Accepted intent — deferred hardfork.**

**Interim soft-fork shipped** as PR
[#1478](https://github.com/Irys-xyz/irys/pull/1478) (merged to `master`):
consensus validates the EIP-1559 envelope tip against
`ShadowMetadata.transaction_fee`.

This document captures the **follow-up hardfork** so the dual-channel fee
model is not re-opened by a future regression that drops the CL tip check.

**Do not** add a named hardfork stub to `IrysHardforkConfig` until product
names and schedules the cut. Use this doc + GitHub issue for tracking until
then. Implementation steps follow
[`docs/99-reference/06-authoring-hardforks.md`](../../docs/99-reference/06-authoring-hardforks.md).

## Problem

Shadow transactions carry economic fees in **two places**:

| Channel | Location | Consensus check |
| --- | --- | --- |
| Packet body | Borsh `ShadowTransaction` in `tx.input` | Yes — CL regenerates and compares packets |
| Priority tip | EIP-1559 `max_priority_fee_per_gas` | Yes **after** [#1478](https://github.com/Irys-xyz/irys/pull/1478); **No** before |

The EL (`IrysEvm::process_shadow_tx`) distributes the tip as **raw wei**
(`gas_priority_fee` ← `max_priority_fee_per_gas`, not × gas) from
`packet.fee_payer_address()` to the block beneficiary **before** executing the
packet body. Shadow txs short-circuit standard revm fee accounting
(`transact_raw` → `process_shadow_tx`); other envelope fields (`max_fee_per_gas`,
`gas_limit`) are unread on that path and are **not** the skim vector.

Without pinning the tip, a producer could keep a valid packet and inflate the
envelope tip to drain the fee-payer. State roots still matched (all nodes
re-executed the same payload). That is funds theft without a fork.

Honest nonzero tips are intentional for several packet types (commitment
`tx.fee()`, storage block-producer share of term fee, fee-only unstake/unpledge
inclusion). Banning all nonzero tips except BlockReward is **not** a valid fix.

## Interim mitigation (soft fork) — shipped

PR [#1478](https://github.com/Irys-xyz/irys/pull/1478) —
`validate_shadow_transactions_match`:

```text
actual.max_priority_fee_per_gas.unwrap_or(0) == expected.transaction_fee
```

- Wire format and EL unchanged; fee still charged from the envelope tip.
- Strictly fewer valid blocks (soft fork).
- Legacy / non-dynamic-fee envelopes expose `None` tip → normalized to `0`
  (must match generator fee `0` for fee-less packets such as BlockReward / refunds).
- Dual channel remains: CL must keep checking the tip until this hardfork.

**Risk if interim is removed:** skim re-opens. Any refactor of shadow validation
must preserve the tip equality check until this hardfork activates. Code anchors
link back here under "Hardfork follow-up / INTERIM".

## Hardfork goal

Make the consensus fee part of the object CL already equality-checks: the
borsh shadow packet (or a versioned wrapper). After activation:

1. Fee is encoded in the packet (e.g. `ShadowTransaction::V2 { …, priority_fee }`
   or a field on debit-carrying variants).
2. EL charges **packet fee** as the SSOT (and/or requires envelope tip ≡ packet fee).
3. Packet equality alone implies fee equality — no second channel to forget.

Nonzero tips remain valid where the generator sets them; BlockReward stays fee `0`.

## Activation sketch

| Phase | Behavior |
| --- | --- |
| Pre-activation (today) | V1 packets; EL uses envelope tip; CL tip check (PR #1478) required |
| Activation | Timestamp hardfork (same pattern as Aurora / Cascade) |
| Post-activation | V2 (or fee-in-packet) required; EL uses packet fee; V1 rejected (or deprecated path only if explicitly designed) |

Name the hardfork when scheduled (next letter after Cascade is open); do not
invent a stub flag in config until then.

## Implementation checklist

When implementing (see also authoring hardforks guide):

- [ ] Versioned packet: borsh-stable V2 (or equivalent) with fee field; keep V1 decode for history
- [ ] `ShadowTxGenerator` / `ShadowMetadata`: write fee into the packet, not only metadata
- [ ] `compose`: envelope tip mirrors packet fee (or forced 0 if EL ignores envelope post-HF)
- [ ] EL after activation: charge packet fee; reject mismatched envelope if kept as mirror
- [ ] CL: packet equality covers fee; optional defense-in-depth envelope == packet fee
- [ ] BlockReward: fee must be 0 (EL + generator)
- [ ] Pre-activation tests: V1 + CL tip check still works (`shadow_tx_priority_fee_match` suite)
- [ ] Post-activation tests: inflate envelope with correct packet fee fails if packet is SSOT; wrong packet fee fails
- [ ] Config: hardfork struct + activation timestamp; defaults for test/main
- [ ] Update this doc status to **Implemented** and link the HF PR

### Blast radius (files)

- `crates/irys-reth/src/shadow_tx.rs` — version, encode/decode, `compose`
- `crates/irys-reth/src/evm.rs` — fee source in `process_shadow_tx` / `distribute_priority_fee`
- `crates/actors/src/shadow_tx_generator.rs` — fee into packet
- `crates/actors/src/block_producer.rs` — compose path
- `crates/actors/src/block_validation.rs` — expected/actual match
- Chain-tests / reth shadow-tx tests / fixtures

## Non-goals

- Ban all nonzero priority fees (breaks honest stake/storage BP paths).
- Change commitment or term fee *schedules* (only *where* the fee is encoded).
- Soft-fork-only forever (acceptable interim; not structural end state).
- Pinning unread envelope fields (`max_fee_per_gas`, `gas_limit`) for skim
  safety — they do not affect the EL transfer today; the hardfork SSOT is the
  packet fee, not a fuller EIP-1559 field set.

## Code anchors (interim dual channel)

Until the hardfork lands, these sites document the dual channel:

- CL match: `validate_shadow_transactions_match` in `block_validation.rs`
- EL charge: `process_shadow_tx` priority fee in `evm.rs`
- Producer compose: `ShadowTransaction::compose` in `shadow_tx.rs`
- Unit coverage: `shadow_tx_priority_fee_match_tests` in `block_validation.rs`
  (skim regression, zero-expected, packet-mismatch order, legacy `None` tip)

## References

- Interim fix (merged): https://github.com/Irys-xyz/irys/pull/1478
- Hardfork authoring: `docs/99-reference/06-authoring-hardforks.md`
- Shadow module overview: `crates/irys-reth/src/shadow_tx.rs` module docs
