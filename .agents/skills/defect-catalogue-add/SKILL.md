---
name: defect-catalogue-add
description: Use to turn a PR (open or merged), a commit, or a range of merged PRs into defect catalogue entries (a class file and/or instance log rows), in the fix PR itself or as a backfill of missed fixes. Checks the disclosure rule first. Optional sibling sweep.
---

# Add defect catalogue entries from a PR or commit

The catalogue is in `docs/99-reference/defect-catalogue/`: `detect.md` (one detection line per
class, read first in a review), `README.md` (process, known non-issues), `classes/<id>.md` (one
file per class), `instances.toml` (instance log, rows are only added).

Logging is best effort: a fix PR adds its own entries when its author knows the class, and a
backfill finds the fixes that nobody logged.

Input, one of:

- **A fix:** a PR (open or merged), a commit, or a list of them. A fix PR can run this skill on
  itself, so the entries land with the fix.
- **A backfill:** a range of merged PRs, given as a date (`--since YYYY-MM-DD`) or two refs.
  1. List the PRs: `gh pr list --state merged --search "merged:>=<date>" --limit 500 --json number,title,labels`,
     or the `(#<n>)` suffixes of `git log --format=%s <from>..<to>`.
  2. Drop each PR whose number is already a `fix` in `instances.toml`.
  3. Triage from the title and labels, and the description only when they are not clear. Keep
     the PRs that fix a defect. Read the diff of those PRs only.
  4. In the report, give the range that this backfill covered, so that the next one starts after
     it.

  A backfill also fills in `released_in`: for each row that has no `released_in`, run the tag
  command of step 2 on its fix, and add the tag if it prints one.

Optional: `--sweep` (or ask the user) to search for sibling sites of a new class.

For each fix, do these steps in order.

1. **Class or one-off?** Read the diff and the PR text (`gh pr view <n>`, `git show <sha>`). Decide
   whether the fix removes an instance of a defect class (a pattern that can occur at other sites)
   or a one-off. Skip one-offs and say why. One PR can contain several fixes; handle each.

2. **Disclosure check.** This repo is public. Continue only if both are true:
   - The fix is public: a commit on this repo's `master`, or an open PR on this repo. A fix that
     is under a private security advisory (for example in its temporary private fork) is not
     public until the advisory is published.
   - No open private advisory covers it. Read the titles and descriptions of
     `gh api --paginate "repos/Irys-xyz/irys/security-advisories?state=draft&per_page=100"` and
     the same with `state=triage`. If either call fails (for example 403), this check fails.

   If either check fails, stop for this fix. Write nothing about it in the repo, a commit, a PR,
   an issue or a comment. To the user, report only the PR or commit and one of: "not public",
   "advisory list unavailable", "open advisory". Do not describe the defect.

   `released_in`: for a merged fix, run
   `git tag --contains <sha> --sort=creatordate | grep -E '^(testnet|mainnet)-[0-9]+\.[0-9]+\.[0-9]+$' | head -1`.
   If it prints a tag, that is `released_in` (the first release, in time, that contains the fix);
   use it as printed. If it prints nothing, or the fix is an open PR, leave `released_in` out. A
   later backfill adds it.

3. **Confirm the fix is in the final tree.** A squash-merged PR can contain a fix that a later
   commit in the same PR reverted. For a merged fix, read the current code at the site; for an open
   PR, read the PR head. Confirm the correct form is there. Do not log a reverted change.

4. **Write the entries.** Read `detect.md`.
   - The class exists: append an `[[instance]]` row to `instances.toml`.
   - The class is new: write `classes/<id>.md`, add its `detect.md` line, then append the row.

   Set `enforcement` to the strongest control that stops the class today (see the template). If a
   lint or a type could stop the class, say so in **Detection** and tell the user, so they can open
   an issue for it.

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
`data-loss`, `dos`, `stall` or `other`. `enforcement` is how the class is stopped today: `review`
(only a reviewer finds it; no `patterns`), `scan` (`patterns` give leads), `lint` (a lint or another
CI check rejects the code) or `type` (a type or an API makes the defect impossible to write). `scope` (the code this class can occur in) and `flag` (what
to flag) are each one line with no final `.`. `paths` (globs relative to the repo root) and `patterns`
(Rust `regex` syntax) are optional and feed `cargo xtask defects scan`; set `paths` to keep the scan
output small.

```markdown
+++
id = "short-class-name"
severity = "dos"
enforcement = "scan"
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

A new known non-issue gets a `### <id>` entry (kebab-case) under "Known non-issues" in the README,
and one line under `## Do not flag` in `detect.md`:

```markdown
- `short-non-issue-name`: what not to flag, in code terms. This is intended.
```

## Instance row template

```toml
[[instance]]
class = "short-class-name"          # class id
site = "crates/p2p/src/gossip_client.rs::GossipClient::fetch"   # path::item
found = "2026-09-22"                # date found; if unknown, the fix PR open date (gh pr view <n> --json createdAt)
found_by = "review"                 # review | test | testnet | mainnet | audit | sweep | other
fix = "#1574"                       # PR number or commit SHA
released_in = "testnet-4.1.0"       # optional: first release tag that contains the fix; a backfill adds it later
note = "optional one line"          # optional
```
