# Defect catalogue

A curated list of defect classes that we found and fixed before. Reviewers, human or agent, check
new work against it, so that a class we fixed once does not come back at a sibling site.

- [`detect.md`](detect.md): one line per class, with only what a reviewer needs to flag a lead.
  A review reads this file first.
- [`classes/`](classes/): one file per class, with the pattern, why it is a defect, the correct
  form and how to detect it. A reviewer opens a class file only to confirm a lead.
- [`instances.toml`](instances.toml): one row per instance found, with where it was found and
  where it was fixed.

**Disclosure:** this repo is public. Add only defects that are fixed **and** released (a
`testnet-*` or `mainnet-*` tag contains the fix). Unfixed or unreleased security defects stay in
private security advisories until the fix is released.

## Process

- A fix for a new defect class adds a class file, its `detect.md` line, and an instance row in the
  same PR.
  Use the `defect-catalogue-add` skill (`.agents/skills/defect-catalogue-add/`).
- A fix for a known class adds an instance row.
- Each class has an `enforcement` value: how the class is stopped today, the strongest control
  that applies. `review`: only a reviewer finds it. `scan`: `defects scan` patterns give leads.
  `lint`: a lint or another CI check rejects the code. `type`: a type or an API makes the defect
  impossible to write. Move a class to `lint` or `type` when that is possible: a class with many
  recent instances and `review` or `scan` enforcement is the first candidate.
- The sibling sweep for a new class is optional. It can run as a separate step.
- Reviews use the `defect-catalogue-review` skill (`.agents/skills/defect-catalogue-review/`): the
  final review before a PR merges, and the version-bump review.
- Tools (no model use):
  - `cargo xtask defects check`: validates the catalogue. CI runs it. Each `detect.md` class line
    must equal the line that the class front matter gives (`id`, `severity`, `scope`, `flag`); on
    a difference, `check` prints the expected line. The "Do not flag" lines and the known
    non-issues below must have the same ids.
  - `cargo xtask defects trend [--since YYYY-MM-DD | --last-releases N]`: instance counts per
    class, most frequent first, with each class's `enforcement`. `--last-releases N` counts
    instances whose `released_in` is one of the last N release tags in git, per stream (testnet
    and mainnet apart). `--since` counts by `found` date, for an ad hoc question.
  - "Recent" means `--last-releases 1` in the version-bump review and `--last-releases 2` in a
    quarterly priority review. A backfilled row keeps the date the defect was found, not the
    backfill date, so a backfill does not look like a spike.
  - `cargo xtask defects scan [--base <ref>] [--head <ref>]`: matches class patterns against the
    lines a diff adds (and every line of an untracked file, when there is no `--head`). Hits are
    leads, not findings.
- `detect.md` order: by severity (`fork`, `data-loss`, `dos`, `stall`, `other`), then by id. When
  the list grows, group the lines under `###` area headings (for example consensus, p2p, storage)
  inside `## Classes`.

## Known non-issues

Patterns that look like defects but are correct by design. Do not raise them in a review. Each
entry is a `### <id>` heading (kebab-case) and has one line under "Do not flag" in `detect.md`
that starts with the same id in backticks. `check` compares the two lists.

### reth-irys-state-mismatch-is-fatal

**Pattern:** `RethService` (`crates/actors/src/reth_service.rs`) stops with an error, and does not
retry, when a fork-choice update to Reth fails: the Reth (EVM) state and the Irys state do not match.

**Why it is correct:** to continue on a mismatched state risks a fork or a wrong state. The node must
stop so that an operator can repair it.
