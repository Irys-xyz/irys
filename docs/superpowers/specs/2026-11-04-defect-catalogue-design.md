# Defect catalogue — design

Tracks GitHub issue #1754.

## Intent

We find the same class of defect again in new code. A fix for one site often does not sweep the
sibling sites. Example: #1574 capped peer HTTP response bodies on the gossip client. The chunk-body
fetch in `HttpChunkFetcher` stayed uncapped until #1678.

The defect catalogue is an in-repo list of defect classes that we found and fixed before. Reviewers
(human or agent) check new work against it.

## Who uses it, and when

Two review passes:

1. The final review pass before a PR merges.
2. The version-bump review: a review of all changes in a semantic-version range.

Both passes must stay cheap in tokens. A review must not read the full catalogue each time.

## Goals

- A curated list of defect classes, one file per class.
- A separate, append-only log of each instance: where it was found and where it was fixed.
- Trend data from the log (counts, recent counts, last seen) so we know which classes to
  prioritise.
- A cheap, model-free scan of a diff against the patterns that the classes declare.
- A skill that checks a diff against the catalogue (used by both review passes).
- A skill that turns a PR or commit into catalogue entries. We use it to seed and to backfill.

## Non-goals

- Backfill of past fixes. The add skill makes it possible; we run it later.
- A choice of trend window ("recent" is TBD). The data supports days and releases; the command takes
  the window as a flag.
- Custom lints or clippy `disallowed_methods`. An entry may name one under detection, but this work
  does not add any.
- A change to the version-bump review process itself. It is not written down in the repo; it calls
  the review skill.

## Anti-goals

- A review that reads every class file on every pass.
- Scan hits reported as findings. They are leads that the reviewer verifies.
- Any entry for a defect that is not fixed and released (see Disclosure).
- Tests taken as proof of correct behaviour. Tests can encode bad behaviour. The review skill tells
  the reviewer to question the design and tests as they are, and what they could be.

## Disclosure

This repo is public. The catalogue and the log contain only defects that are fixed **and** released
(a `testnet-*` or `mainnet-*` tag contains the fix). Unfixed or unreleased security defects stay in
private security advisories until the fix is released. The add skill checks this before it writes
anything, and stops on a defect that an open advisory still covers.

## Layout

```
docs/99-reference/defect-catalogue/
  detect.md            one detection line per class + one line per known non-issue
  README.md            process + known non-issues
  classes/<slug>.md    one file per class: TOML front matter + prose
  instances.toml       append-only instance log
.agents/skills/defect-catalogue-review/SKILL.md
.agents/skills/defect-catalogue-add/SKILL.md
xtask: `cargo xtask defects check | trend | scan`
```

`docs/99-reference/README.md` gets a link to the catalogue.

## Class file

`classes/<slug>.md`. The slug is the class id (kebab-case, stable; the log refers to it).

TOML front matter between `+++` lines (xtask already has `toml` in the workspace; no YAML
dependency). Fields:

| Field          | Type            | Purpose                                                                 |
|----------------|-----------------|-------------------------------------------------------------------------|
| `id`           | string          | Must equal the file slug.                                               |
| `severity`     | string          | Worst typical effect: `fork`, `data-loss`, `dos`, `stall`, `other`.     |
| `scope`        | string          | One line: the code this class can occur in. Used in `detect.md`.        |
| `flag`         | string          | One line: what to flag, in code terms. Used in `detect.md`.             |
| `paths`        | list of globs   | Optional. Limits the scan to these paths.                               |
| `patterns`     | list of regex   | Optional. Matched against added diff lines by `xtask defects scan`.     |

Body sections (markdown, in this order):

- **Pattern** — what the defective code looks like.
- **Why it is a defect** — the invariant it breaks and the effect (DoS, fork, data loss, stall).
- **Correct form** — the helper or idiom to use, e.g. `read_capped_bytes`.
- **Detection** — lint, test, CI grep, scan pattern, or manual check. Say what the scan pattern
  misses.

Known sites are **not** in the class file. They are in the instance log.

## Instance log

`instances.toml`, one `[[instance]]` table per instance, append only. Two PRs that both append
will conflict at the end of the file; the resolution is to keep both rows.

| Field         | Type   | Required | Purpose                                                              |
|---------------|--------|----------|----------------------------------------------------------------------|
| `class`       | string | yes      | Class id. Must exist in `classes/`.                                  |
| `site`        | string | yes      | `path::item`, e.g. `crates/actors/src/data_sync_service/chunk_fetcher.rs::HttpChunkFetcher`. |
| `found`       | date   | yes      | Date the defect was found (or the fix PR date, if unknown).          |
| `found_by`    | string | yes      | `review`, `test`, `testnet`, `mainnet`, `audit`, `sweep`, `other`.   |
| `fix`         | string | yes      | PR number (`#1678`) or commit SHA.                                   |
| `released_in` | string | yes      | First release tag that contains the fix, e.g. `testnet-4.1.1`.       |
| `note`        | string | no       | One line, if the site needs context.                                 |

A row must be traceable from the repo alone: the `fix` and `released_in` values let a reader find
the change. These are data fields, not comments, so the "no comments of record" rule does not apply.

## README.md

- One paragraph: what the catalogue is and the disclosure rule.
- Process:
  - A fix for a new defect class adds a class file and a log row in the same PR (use the add skill).
  - A fix for a known class adds a log row.
  - The sibling sweep is optional when the class is new; it may run as a separate step.
- **Known non-issues**: patterns that look like defects but are correct by design. Reviewers must
  not raise them again. Seed: a Reth/Irys state mismatch is fatal on purpose (`RethService` exits).

## `cargo xtask defects`

New xtask subcommand with three actions. No model use.

- `check` — validates the catalogue: front matter parses; `id` equals the slug; required fields and
  body sections exist; every regex compiles; every log row names an existing class; every log row
  has the required fields; `detect.md` has exactly one line per class, equal to the line its front matter gives. Exit non-zero on any error.
  Added to `cargo xtask local-checks` and to CI next to the existing checks.
- `trend [--since <YYYY-MM-DD> | --last-releases <N>]` — prints one row per class: id, severity,
  total instances, instances in the window, last `found` date, last `released_in`. Sorted by
  instances in the window, then total. With no flag, the window column is omitted.
  `--last-releases N` reads the release tags from git and takes the N highest versions of each
  stream (testnet and mainnet apart, since their version numbers differ); an instance counts when
  its `released_in` is one of those tags. A release with no logged instance still moves the window.
- `scan [--base <ref>] [--head <ref>]` — runs `git diff -U0 <base>...<head>` (default base: the
  merge-base with `master`; default head: working tree) and matches each class's `patterns`
  against **added** lines in files that match its `paths`. Prints `class  file:line  line-text`.
  Output is leads, not findings. Exit 0 whether or not there are hits.

## Skill: `defect-catalogue-review`

Used by both review passes. Steps:

1. Run `cargo xtask defects scan` for the target range.
2. Read `detect.md` only. Check the diff against each "Flag" whose scope it touches; a match is a
   lead. Drop leads that match a "Do not flag" line.
3. Verify each scan hit and lead in the code. Open the class file of a lead's class only, to
   confirm it. Do not open class files of classes with no lead.
4. Question the tests and the design as they are, not only the diff. Tests can encode bad
   behaviour.
5. Report true instances with file:line, class id, and the correct form, and suggest a log row. For a defect that fits no class,
   say "candidate new class".

The skill does not write to the catalogue. The add skill does that.

## Skill: `defect-catalogue-add`

Input: a PR number, a commit, or a range of either.

For each fix:

1. Read the diff and the PR text. Decide: a fix of a defect class, or a one-off. Skip one-offs and
   say why.
2. Disclosure check: is the fix in a released tag (`git tag --contains`)? Is it under an open
   private advisory? If not released, or under an advisory, stop for that fix and report it.
3. Read `detect.md`. If the class exists, append a log row. If not, draft a new class file and
   its `detect.md` line, then append the log row.
4. Squash-merged PRs can contain fixes that were later reverted inside the same PR. Confirm the fix
   is present in the final tree before you log it.
5. Sibling sweep: optional (`--sweep`, or ask). When on, search the codebase for other sites of the
   class and report them. Do not log unfixed sites; report them to the user so they can open an
   issue or advisory.
6. Run `cargo xtask defects check`.

Output: the changed files, and a short report (classes added, rows added, skipped fixes with
reasons, sweep leads).

## Integration

- PR template (`.github/PULL_REQUEST_TEMPLATE.md`): add a checklist item: "Does this fix a new defect
  class? If yes, add an entry to the defect catalogue (`docs/99-reference/defect-catalogue/`)."
- `AGENTS.md`: one line under "Skills & Commands" that names the two skills and when to use them.
- The user's personal `/branch-review` command (`~/.claude/commands/branch-review.md`, outside the
  repo): one line to call `defect-catalogue-review`. Not part of the PR.

## Seed

Produce the seed with the add skill (this also tests the skill):

1. Class `uncapped-peer-response-body`. Instances: #1574 (gossip client, data-intervals fetch; in
   `testnet-4.1.0`) and #1678 (`HttpChunkFetcher` chunk-body fetch; in `testnet-4.1.1`). Patterns:
   `.json()`, `.text()`, `.bytes()` on a peer response; correct form is the capped reader
   (`read_capped_bytes` / the capped helper from #1574).
2. Class `hash-iteration-order-in-consensus`: `HashMap` / `HashSet` iteration order in code whose
   output affects consensus. Instances, both from #1425 (squash merge, first released in
   `testnet-3.0.0`):
   - `crates/domain/src/models/block_tree.rs::BlockTree::get_by_solution_hash` — `solutions`
     changed from `HashMap<H256, HashSet<BlockHash>>` to `BTreeMap<H256, BTreeSet<BlockHash>>`.
   - `crates/actors/src/block_validation.rs::get_assigned_ingress_proofs` — `slot_ranges` changed
     from `HashMap` to `BTreeMap`, so the first intersecting slot is the same on all nodes. The
     semantic change (count unique miners over all slots) was reverted inside the PR; it is a rule
     change, not part of this class. Do not log it.
   The add skill confirms both against the final tree before it logs them.

The wall-clock-vs-monotonic class from the issue is left to the backfill; the `monotonic-time` skill
already covers the guidance.

## Settled details

- `released_in` must match `^(testnet|mainnet)-\d+\.\d+\.\d+$`. `check` rejects other values
  (for example `mainnet-latest`).
- `check` is offline and pure: it reads only the catalogue files, not git. It does not verify that
  the `fix` or the tag exists; the add skill does that when it writes a row. This keeps CI simple.
  (CI checkouts already use `fetch-depth: 0`, but `check` does not need it.)
- `scan` is a local and review tool. CI does not run it.
- `detect.md` is the one file a review reads in full, so it holds only what a model needs to flag
  a lead. The rationale stays in the class files, which a review opens only to confirm a lead.
  Class lines are the `- ` lines under `## Classes`, one per class:
  `` - `<id>` (<severity>). Scope: <scope>. Flag: <flag>. ``
  `check` requires each line to equal the line the class front matter gives, reports a missing,
  changed, duplicate or orphan line, and prints the expected line. Line order does not matter;
  keep lines sorted by id by convention. `scope` and `flag` are one line each with no final `.`.
- Known non-issues: a full entry in the README (one `###` heading per item, then **Pattern** and
  **Why it is correct**), and one line under `## Do not flag` in `detect.md`. `check` does not
  parse either.
- `scan` diff source: with `--head`, `git diff -U0 --find-renames <base>...<head>`; without
  `--head`, `git diff -U0 --find-renames <merge-base>` (working tree), plus every line of each
  untracked, non-ignored file. Added lines in a renamed file count under the new
  path. Deleted files and binary files are skipped.
- `paths` globs are relative to the repo root (`glob::Pattern` syntax; the `glob` crate is already
  in the lockfile). `patterns` use the Rust `regex`
  crate syntax (no look-around).
- Seed classes set `paths` to keep the scan output small. For the response-body class, restrict
  to crates that talk to peers (`crates/p2p/**`, `crates/actors/**`, `crates/api-client/**`,
  `crates/chain/**`).
- The seed is made by running the add skill. `gh` works, and so does the advisory API:
  `gh api "repos/Irys-xyz/irys/security-advisories?state=draft"` and `?state=triage`. The add skill
  reads the advisory titles and descriptions and stops on any that match the fix.

## Left to the builder

- Module layout inside `xtask` and the output formatting.
- Regex crate choice for xtask.
- Exact wording of the skills, within the steps above.
- Scan exit code policy beyond "exit 0 on hits".

## Calls I made (not discussed with the user)

- TOML front matter (`+++`) and a TOML log, to reuse the `toml` dependency.
- `severity` field on classes.
- `scan` matches added lines only, not the full files.
- `defects check` runs in `local-checks` and CI.
- Seed classes and instances as listed above.
- `AGENTS.md` line.
- Everything under "Settled details".
