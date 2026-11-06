+++
id = "hash-iteration-order-in-consensus"
severity = "fork"
scope = "consensus code: block validation, block production, shadow transactions, epoch and ledger state"
flag = "a result that must be the same on every node depends on HashMap or HashSet iteration order (first match or `break`, last write wins, a tie-break, collect into a Vec, hash or serialize)"
paths = ["crates/actors/src/block_validation.rs", "crates/actors/src/block_producer.rs", "crates/actors/src/block_producer/**", "crates/actors/src/shadow_tx_generator.rs", "crates/domain/src/models/**", "crates/domain/src/snapshots/**"]
patterns = ['\bHash(Map|Set)\s*<', '\bHash(Map|Set)::(new|with_capacity|default)\b']
+++

## Pattern

Code iterates a `HashMap` or `HashSet` (`for .. in &map`, `.iter()`, `.keys()`, `.values()`,
`.into_iter()`, `.drain()`), and the order changes a result that must be the same on every node:
which entry is "first" or "last", a tie-break, the order of items in a block or a shadow
transaction, or a value that a later entry overwrites.

```rust
let slot_ranges: HashMap<usize, LedgerChunkRange> = ...;
for (slot_index, slot_range) in &slot_ranges {
    if block_range.intersection(slot_range).is_some() {
        assigned_miners = slot_address_counts[slot_index]; // first match in hash order wins
        break;
    }
}
```

## Why it is a defect

`std` hash maps use a random seed per process (`RandomState`). Each node iterates the same entries
in a different order. If the order reaches a consensus result, two honest nodes compute different
results for the same block, and the network forks. Tests often do not show it: one process uses one
seed, so the order is stable inside a test run.

## Correct form

Use `BTreeMap` / `BTreeSet` (iteration is ordered by key), or sort the items by a total order
before the order-dependent step. When the result must not depend on order at all, compute it from
the whole set (a count, a sum, a min or max by a total order). A lookup by key (`get`, `contains`)
is safe; only iteration order is the defect.

## Detection

`cargo xtask defects scan`: the patterns match a new `HashMap` / `HashSet` type or constructor in
consensus paths. Each hit is a lead: check whether the code iterates it and whether the order reaches
a consensus result. The scan misses an iteration of an existing map (no new type on the added line),
maps in other files that consensus code calls, and other unordered sources (for example `join_all`
completion order or `DashMap`). Manual check: for each consensus result in the diff, trace its
inputs back to any unordered collection.
