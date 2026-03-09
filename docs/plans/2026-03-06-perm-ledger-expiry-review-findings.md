# Perm Ledger Expiry — Design Review Findings

Review of `docs/plans/2026-03-06-perm-ledger-expiry-design.md` combining Codex analysis with manual code review.

---

## Finding 1 (High): Consensus config hash changes for all networks

**Issue:** Adding `publish_ledger_epoch_length: Option<u64>` to `EpochConfig` changes the output of `ConsensusConfig::keccak256_hash()` for every network — even when the value is `None`. The canonical JSON serializer (`crates/types/src/canonical.rs`) serializes `Option::None` as `null`, which becomes part of the hash input.

**Why it matters:** Nodes use `consensus_config_hash` during P2P handshake to verify they share the same consensus rules. Mixed old/new nodes will compute different hashes and reject each other. The `test_consensus_hash_regression` test (`consensus.rs:939`) will also fail since the expected hash is hardcoded.

**Reasoning:** This is expected for any consensus config change — it's the mechanism that prevents nodes with incompatible rules from connecting. Since mainnet hasn't launched with this field, mainnet defaults can include the new field from the start with no compatibility issue. For testnet, this is a coordinated upgrade — all nodes must upgrade together.

**Action:** Update the regression hash test. Coordinate testnet node upgrades. Document as a breaking consensus change.

**Refs:** `consensus.rs:435` (keccak256_hash), `consensus.rs:939` (regression test), `canonical.rs:76` (serializer)

---

## Finding 2 (Medium): `get_expiring_term_partitions()` must also be updated

**Issue:** `Ledgers` has two parallel methods: `expire_term_partitions()` (mutating, marks slots expired) and `get_expiring_term_partitions()` (read-only, returns what would expire). The design only mentions updating the mutating version. The read-only version at `data_ledger.rs:307` only iterates `self.term`.

**Why it matters:** `get_expiring_term_partitions()` is called by `EpochSnapshot::get_expiring_partition_info()` (`epoch_snapshot.rs:1199`), which feeds into the block validation path. If the mutating method expires Publish slots but the read-only method doesn't report them, validators will compute different expected state than block producers — causing consensus failures.

**Reasoning:** These two methods must always agree on what expires. They exist as separate functions because expiry has side effects (marking `is_expired = true`), and some callers need a preview without mutation. Both must include perm ledger logic or they diverge.

**Action:** Update `get_expiring_term_partitions()` with the same perm logic added to `expire_term_partitions()`. Consider renaming both methods (drop "term" from the name) since they now handle perm too.

**Refs:** `data_ledger.rs:286` (mutating), `data_ledger.rs:307` (read-only), `epoch_snapshot.rs:1199` (caller)

---

## Finding 3 (Medium): `bail!("publish ledger cannot expire")` is a latent trap

**Issue:** `collect_expired_partitions()` in `ledger_expiry.rs:301` has an explicit check: `if ledger_id == DataLedger::Publish { bail!("publish ledger cannot expire") }`. Currently safe because the block producer only calls `calculate_expired_ledger_fees()` with `DataLedger::Submit`. But with perm expiry enabled, expired Publish partitions will exist in `expired_partition_infos` — and any future caller passing `DataLedger::Publish` to the fee function will hit this bail unexpectedly.

**Why it matters:** The bail was written as a safety invariant ("this should never happen"). Once perm expiry exists, the invariant changes from "Publish never expires" to "Publish expires but has no fee distribution." A developer extending fee logic in the future could reasonably try to call the function for Publish and hit a confusing runtime error.

**Reasoning:** The fee distribution path doesn't need changes for our design (we skip fees for perm expiry). But the stale bail creates a false safety net that could confuse future work. It's better to either remove it, gate it on the config (`publish_ledger_epoch_length.is_none()`), or add a comment explaining why it's still valid.

**Action:** Add a comment to the bail explaining the design intent, or gate it on the config. No code path change needed for this feature since we don't call fee calc for Publish.

**Refs:** `ledger_expiry.rs:299-303` (bail), `block_producer.rs:1588` (hardcoded Submit call)

---

## Finding 4 (Medium): Keep expiry logic in `Ledgers`, not `PermanentLedger`

**Issue:** The original design adds `epoch_length`/`num_blocks_in_epoch` fields and `get_expired_slot_indexes()`/`expire_old_slots()` methods directly to `PermanentLedger`. This weakens the compile-time safety that the current type separation provides — the code comments at `data_ledger.rs:249-260` explicitly state this separation exists to prevent accidental permanent data expiry.

**Why it matters:** Adding expiry methods to `PermanentLedger` means any code with a `&mut PermanentLedger` can call `expire_old_slots()`. The type-level protection is gone. A future developer might call it without checking the config gate, accidentally expiring mainnet data.

**Reasoning:** The expiry logic can live entirely in `Ledgers::expire_term_partitions()` instead. `Ledgers` already has direct access to `self.perm.slots` and can read the config fields. This keeps `PermanentLedger` clean (no expiry methods), concentrates the conditional behavior in one place, and preserves the architectural intent. The optional config fields still need to live somewhere — either on `PermanentLedger` or passed through to `Ledgers`. Storing them on `Ledgers` or reading them from `ConsensusConfig` at call time keeps `PermanentLedger` unchanged.

**Action:** Store `publish_ledger_epoch_length` and `num_blocks_in_epoch` on `Ledgers` (or pass as parameters). Implement perm expiry inline within `Ledgers::expire_term_partitions()` using the same algorithm as `TermLedger::get_expired_slot_indexes()`. Do not add expiry methods to `PermanentLedger`.

**Refs:** `data_ledger.rs:249-260` (design comment), `data_ledger.rs:32-38` (PermanentLedger struct)

---

## Finding 5 (Low): Add config validation for the new field

**Issue:** No validation prevents setting `publish_ledger_epoch_length: Some(5)` on mainnet, or `Some(0)` on any network. The existing config validation in `crates/types/src/config/mod.rs:80-130` doesn't check epoch-related values beyond VDF step requirements.

**Why it matters:** An operator accidentally deploying mainnet with perm expiry enabled would cause permanent data loss. A value of `Some(0)` would attempt expiry at every epoch with undefined behavior (the TermLedger expiry math requires `epoch_length > 0`).

**Reasoning:** Consensus-critical parameters should have guardrails. This is a low-cost addition (a few `ensure!` lines) that prevents catastrophic misconfiguration. The mainnet chain_id (3282) is known and can be checked against.

**Action:** Add validation in `Config::validate()`:
- If `chain_id == 3282` (mainnet), ensure `publish_ledger_epoch_length.is_none()`
- If `Some(n)`, ensure `n > 0`

**Refs:** `config/mod.rs:80-130` (existing validation), `consensus.rs:488` (mainnet chain_id)

---

## Finding 6 (Low): `PermanentLedger::get_slot_needs()` must filter expired slots

**Issue:** `PermanentLedger`'s `LedgerCore::get_slot_needs()` implementation (`data_ledger.rs:179-192`) does not filter out `is_expired` slots, unlike `TermLedger`'s version (`data_ledger.rs:228-241`) which checks `!slot.is_expired`. When perm expiry is enabled, expired Publish slots would still report partition needs, causing the epoch snapshot to try assigning new partitions to expired slots.

**Why it matters:** Without this filter, expired Publish slots would be treated as "needing partitions," leading to partitions being assigned to slots that should be dead. This would waste capacity and create inconsistent state.

**Reasoning:** If we follow Finding 4 (keep expiry logic in `Ledgers`), we still need `PermanentLedger::get_slot_needs()` to respect the `is_expired` flag once slots are marked. The simplest fix: always filter `!slot.is_expired` in `PermanentLedger::get_slot_needs()`, matching `TermLedger`. On mainnet where no perm slots are ever expired, the filter is a no-op.

**Action:** Update `PermanentLedger::get_slot_needs()` to include `&& !slot.is_expired` in the filter, matching `TermLedger` behavior.

**Refs:** `data_ledger.rs:179-192` (PermanentLedger), `data_ledger.rs:228-241` (TermLedger for comparison)

---

## Finding 7 (Low): Serde default needed for backward-compatible deserialization

**Issue:** `EpochConfig` uses `#[serde(deny_unknown_fields)]`. Adding a new field without `#[serde(default)]` means existing TOML configs that omit the field will fail to deserialize — `deny_unknown_fields` rejects unknown fields but missing fields are a separate concern handled by `Option` defaulting to `None` only if the struct derives `Default` or the field has `#[serde(default)]`.

**Why it matters:** `EpochConfig` derives `Default`, so missing fields will deserialize to the `Default` value. For `Option<u64>`, that's `None`. However, since `EpochConfig::default()` currently has no `publish_ledger_epoch_length` field, the `Default` derive will need updating. Adding `#[serde(default)]` on the field is more explicit and robust.

**Reasoning:** Operators loading consensus config from TOML files (`ConsensusOptions::Path`) need their existing files to keep working without adding the new field. The `#[serde(default)]` annotation makes the intent explicit.

**Action:** Add `#[serde(default)]` to the `publish_ledger_epoch_length` field in `EpochConfig`.

**Refs:** `consensus.rs:312-327` (EpochConfig), `config/mod.rs:611` (TOML deserialization test)
