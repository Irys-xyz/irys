# Data-sync index heal: leftovers in plain English

Context: PR [#1531](https://github.com/Irys-xyz/irys/pull/1531) (`fix(storage): heal path-hash indexes on startup and reconnect data_sync`), merged to master.

Reviewers’ take: **#1531 was good enough to merge.** What remains is a short list of known leftovers — not “this is broken,” more “we know these edges exist; track and fix later if they matter.”

**One-line summary:**

> Follow-ups after path-hash index heal: don’t re-migrate the whole tail after the first hole, and separately detect empty DataRootInfos when path hashes look full.

---

## 1. First-gap-only span (efficiency) — addressed on follow-ups branch

### Before

If the path-hash index had a hole, we found the **first** missing slot, then re-indexed **from that hole all the way to the end** of the partition (or the data frontier), redoing already-dense segments between later holes.

### After (this branch)

We collect **all** half-open path-hash holes and map each hole to the **minimal** inclusive block-height span that covers it. Dense runs between holes are not re-migrated.

---

## 2. DataRootInfos residual

### Two different “indexes” matter when writing a chunk

| Index | Role (simple) |
|--------|----------------|
| **Path hashes** | “We know the merkle paths for this slot” |
| **DataRootInfos** | “We know this data_root lives in this partition at these offsets” |

### What #1531 heals

Mainly the **path-hash** side (and when repair runs, migration also writes DataRootInfos for those blocks).

### What’s still possible

Path hashes look complete, but **DataRootInfos is still empty** for some data. Then data_sync still can’t place the chunk (`MissingDataRootIndex`).

### Before #1531

That could turn into **unbounded thrash**: fetch forever / block forever / repeat hard.

### After #1531

Still possible, but **bounded**: we don’t blindly flip millions of blocked offsets back to “try again” every epoch. Cap + gate. Uncomfortable, not catastrophic.

### “Detectable if the orchestrator probed the blocked data_roots”

Today we only know “write failed: missing data_root index.” We don’t remember *which* data_root failed and use that to trigger a smarter repair. A follow-up could do that.

---

## 3. PLAUSIBLE edges (might be real, not proven on every node)

These are edge cases that could still bite in theory:

| Jargon | English |
|--------|---------|
| **Stale-dense reassignment crash window** | Partition moved from ledger A → B; local index still looks “full” for the old world. Our scan only looks for *missing* entries, not *wrong* ones. A→B safety is mostly elsewhere (mining-bus reset), not this heal. A crash in a bad window could leave wrong-but-dense indexes. |
| **Sync-lag double-indexing** | Two paths try to index the same block while sync is catching up; might do redundant work (usually okay if idempotent). |
| **Latent `.expect`** | Other code still panics on “should never happen.” The heal→migration path was hardened; not every expect in the repo. |

“PLAUSIBLE” means reasonable in theory, not “we saw this blow up in canary today.”

---

## 4. Small / minor leftovers

| Item | English |
|------|---------|
| **Lock-scope** | How long we hold locks; maybe slightly wider/narrower than ideal. Performance/hygiene, not usually correctness. |
| **Telemetry-ordering** | Metrics/logs might fire in an order that confuses dashboards slightly. |
| **Test-coverage minors** | Some paths lack a perfect integration test. Unit/service tests cover the important pieces. |

---

## Bottom line

#1531 shipped the big design:

1. **Detect** holes (path-hash gap scan + O(1) healthy path)  
2. **Repair** safely (soft-fail migration, timeouts, pass caps)  
3. **Reconnect** data_sync carefully (gated + capped unblock)

This branch is for the follow-ups:

- **Efficiency** — don’t re-migrate the whole tail after the first hole  
- **Product completeness** — DataRootInfos-only breakage when path hashes look full  
- Optional: rare edges / nits as they prove out  
