---
name: defect-catalogue-add
description: Use to turn a PR, a commit, or a range of them into defect catalogue entries (a class file and/or instance log rows), for a new fix or to backfill old fixes. Checks the disclosure rule first. Optional sibling sweep.
---

# Add defect catalogue entries from a PR or commit

The catalogue is in `docs/99-reference/defect-catalogue/`: `detect.md` (one detection line per
class, read first in a review), `README.md` (process, known non-issues), `classes/<id>.md` (one
file per class), `instances.toml` (instance log, rows are only added).

Input: a PR number, a commit, or a range of either. Optional: `--sweep` (or ask the user) to search
for sibling sites of a new class.

For each fix, do these steps in order.

1. **Class or one-off?** Read the diff and the PR text (`gh pr view <n>`, `git show <sha>`). Decide
   whether the fix removes an instance of a defect class (a pattern that can occur at other sites)
   or a one-off. Skip one-offs and say why. One PR can contain several fixes; handle each.

2. **Disclosure check.** This repo is public. Continue only if both are true:
   - The fix is released: `git tag --contains <sha> | grep -E '^(testnet|mainnet)-[0-9]'` prints a
     tag. The first such tag (lowest version) is `released_in`.
   - No open private advisory covers it. Read the titles and descriptions of
     `gh api "repos/Irys-xyz/irys/security-advisories?state=draft"` and `?state=triage`.

   If either fails, stop for this fix and report it. Do not write anything about it.

3. **Confirm the fix is in the final tree.** A squash-merged PR can contain a fix that a later
   commit in the same PR reverted. Read the current code at the site and confirm the correct form is
   there. Do not log a reverted change.

4. **Write the entries.** Read `detect.md`.
   - The class exists: append an `[[instance]]` row to `instances.toml`.
   - The class is new: write `classes/<id>.md`, add its `detect.md` line, then append the row.

   `scope` and `flag` are what a reviewer reads before anything else, for every class, so keep them
   short and concrete. `flag` names the code shape to look for (calls, types, control flow), not
   the effect. The rationale goes in the class file body.

   Write in ASD-STE100 Simplified Technical English. No dates or issue numbers in the class prose;
   the log rows hold that data.

5. **Sibling sweep (optional).** For a new class, if the sweep is on, search the codebase for other
   sites of the pattern (scan patterns, `rg`, and reading). Report each unfixed site to the user so
   they can open an issue or a private advisory. Never log an unfixed site.

6. **Check.** Run `cargo xtask defects check` and fix every problem it reports.

Output: the files you changed, and a short report: classes added, rows added, fixes skipped (with
the reason), and sweep leads.

## Class file template

`id` must equal the file name (kebab-case). `severity` is the worst typical effect: `fork`,
`data-loss`, `dos`, `stall` or `other`. `scope` (the code this class can occur in) and `flag` (what
to flag) are each one line with no final `.`. `paths` (globs relative to the repo root) and `patterns`
(Rust `regex` syntax) are optional and feed `cargo xtask defects scan`; set `paths` to keep the scan
output small.

```markdown
+++
id = "short-class-name"
severity = "dos"
scope = "code that reads an HTTP response body from a peer"
flag = "`.json()` on a peer response with no byte cap"
paths = ["crates/p2p/**"]
patterns = ['\.json\(\)']
+++

## Pattern

What the defective code looks like.

## Why it is a defect

The invariant it breaks, and the effect (DoS, fork, data loss, stall).

## Correct form

The helper or idiom to use.

## Detection

Lint, test, CI grep, scan pattern, or manual check. Say what the scan pattern misses.
```

`detect.md` line, under `## Classes` (`cargo xtask defects check` prints the expected line if it
is missing or differs):

```markdown
- `short-class-name` (dos). Scope: code that reads an HTTP response body from a peer. Flag: `.json()` on a peer response with no byte cap.
```

A new known non-issue gets a full entry in the README and one line under `## Do not flag` in
`detect.md`.

## Instance row template

```toml
[[instance]]
class = "short-class-name"          # class id
site = "crates/p2p/src/gossip_client.rs::GossipClient::fetch"   # path::item
found = "2026-10-13"                # date found; the fix PR date if unknown
found_by = "review"                 # review | test | testnet | mainnet | audit | sweep | other
fix = "#1574"                       # PR number or commit SHA
released_in = "testnet-4.1.0"       # first release tag that contains the fix
note = "optional one line"          # optional
```
