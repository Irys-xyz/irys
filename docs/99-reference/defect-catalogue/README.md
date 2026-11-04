# Defect catalogue

A curated list of defect classes that we found and fixed before. Reviewers, human or agent, check
new work against it, so that a class we fixed once does not come back at a sibling site.

Each class has a file in [`classes/`](classes/). Each time we find an instance of a class, a row
goes into [`instances.toml`](instances.toml): where it was found and where it was fixed.

**Disclosure:** this repo is public. Add only defects that are fixed **and** released (a
`testnet-*` or `mainnet-*` tag contains the fix). Unfixed or unreleased security defects stay in
private security advisories until the fix is released.

## Index

| Id | Title | Severity | Applies when |
|----|-------|----------|--------------|
| [hash-iteration-order-in-consensus](classes/hash-iteration-order-in-consensus.md) | Hash iteration order in consensus code | fork | adds or changes a HashMap or HashSet whose iteration order can reach a consensus result |
| [uncapped-peer-response-body](classes/uncapped-peer-response-body.md) | Uncapped peer response body | dos | adds or changes code that reads an HTTP response body from a peer |

## Process

- A fix for a new defect class adds a class file, its index row, and an instance row in the same PR.
  Use the `defect-catalogue-add` skill (`.agents/skills/defect-catalogue-add/`).
- A fix for a known class adds an instance row.
- The sibling sweep for a new class is optional. It can run as a separate step.
- Reviews use the `defect-catalogue-review` skill (`.agents/skills/defect-catalogue-review/`): the
  final review before a PR merges, and the version-bump review.
- Tools (no model use):
  - `cargo xtask defects check`: validates the catalogue. CI runs it.
  - `cargo xtask defects trend [--since YYYY-MM-DD | --last-releases N]`: instance counts per
    class, most frequent first.
  - `cargo xtask defects scan [--base <ref>] [--head <ref>]`: matches class patterns against the
    lines a diff adds. Hits are leads, not findings.

## Known non-issues

Patterns that look like defects but are correct by design. Do not raise them in a review.

### Reth/Irys state mismatch is fatal

**Pattern:** `RethService` (`crates/actors/src/reth_service.rs`) stops with an error, and does not
retry, when a fork-choice update to Reth fails: the Reth (EVM) state and the Irys state do not match.

**Why it is correct:** to continue on a mismatched state risks a fork or a wrong state. The node must
stop so that an operator can repair it.
