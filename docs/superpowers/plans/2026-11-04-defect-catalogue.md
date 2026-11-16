# Defect Catalogue Implementation Plan

> **Superseded.** This plan is the record of the first build. The design spec and the tree are the
> current authority: `detect.md` replaced the README index, and `scope`/`flag` replaced
> `title`/`applies_when`.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An in-repo catalogue of fixed defect classes, an instance log, a model-free `cargo xtask defects check | trend | scan`, and two agent skills (review a diff; add entries from a PR or commit), seeded with two classes.

**Architecture:** Catalogue data is markdown + TOML under `docs/99-reference/defect-catalogue/`. A new `xtask::defects` module parses and validates it (pure functions over file contents and diff text; thin CLI wrappers do the IO and `git`). Skills under `.agents/skills/` drive the review and the backfill.

**Tech Stack:** Rust (xtask crate), `toml` 0.8, `serde`, `regex` 1, `glob` 0.3, `chrono`, `clap`; `gh` + `git` for the add skill.

**Spec:** `docs/superpowers/specs/2026-11-04-defect-catalogue-design.md`

## Global Constraints

- Catalogue dir: `docs/99-reference/defect-catalogue/` with `README.md`, `classes/<id>.md`, `instances.toml`.
- Class id = file slug, kebab-case: `^[a-z0-9]+(-[a-z0-9]+)*$`.
- `severity` ∈ `fork`, `data-loss`, `dos`, `stall`, `other`.
- `found_by` ∈ `review`, `test`, `testnet`, `mainnet`, `audit`, `sweep`, `other`.
- `released_in` matches `^(testnet|mainnet)-\d+\.\d+\.\d+$`.
- `found` is a quoted `"YYYY-MM-DD"` string (parsed with `chrono::NaiveDate`).
- Class body sections, in order: `## Pattern`, `## Why it is a defect`, `## Correct form`, `## Detection`.
- README index row: `| [<id>](classes/<id>.md) | <title> | <severity> | <applies_when> |`; compared as a set, all four columns.
- `check` reads only catalogue files (no git). `scan` exits 0 on hits. CI runs `check`, never `scan`.
- Only fixed **and** released defects go in the catalogue (a `testnet-*`/`mainnet-*` tag contains the fix; no open advisory covers it).
- Use `irys_testing_utils::TempDirBuilder` for test temp dirs.
- No issue/PR numbers or dates of record in code comments or commit messages. (PR numbers in `instances.toml` `fix` fields are data, and are fine.)
- No attribution lines in commits.

## Review Focus

1. Malformed TOML front matter, or a file without `+++` delimiters → `check` reports the file path and the parse error, and does not panic. (Task 1 test `check_reports_bad_front_matter`.)
2. A README index row with a pipe or backtick inside `applies_when` → the row still parses. (Task 1 test `index_row_with_backticks_parses`; a literal `|` inside a cell is not supported, and `check` reports it as a malformed row.)
3. An empty catalogue (no classes, empty `instances.toml`) → `check` passes, `trend` prints a header only. (Task 1 `check_passes_on_empty_catalogue`; Task 2 `trend_empty`.)
4. A `-U0` diff with a rename, a deleted file, a binary file and a hunk header with no count (`@@ -3 +4 @@`) → added lines count under the new path; delete/binary give no hits; line numbers are right. (Task 3 tests.)
5. An invalid regex or glob in a class → `check` reports it; `scan` refuses to run with the same error instead of silently skipping the class. (Task 1 `check_reports_bad_regex`; Task 3 `scan_rejects_invalid_catalogue`.)

---

### Task 1: Catalogue model, parsing, `defects check`, CI wiring

**Files:**
- Create: `xtask/src/defects/mod.rs`, `xtask/src/defects/catalogue.rs`
- Create: `docs/99-reference/defect-catalogue/README.md` (skeleton: intro paragraph with the disclosure rule, empty index table header, `## Process` and `## Known non-issues` headings — Task 4 fills the prose), `docs/99-reference/defect-catalogue/instances.toml` (header comment only), `docs/99-reference/defect-catalogue/classes/.gitkeep`
- Modify: `xtask/Cargo.toml` (add `regex = "1"`, `glob = "0.3"`), `xtask/src/lib.rs` (`pub mod defects;`), `xtask/src/main.rs` (new `Defects` subcommand; call `defects check` from `LocalChecks` after `Typos`), `.github/workflows/rust.yml` (step `cargo xtask defects check` before `cargo xtask full-check`, in the same job), `docs/99-reference/README.md` (link entry)

**Interfaces:**
- Produces (in `xtask::defects::catalogue`):
  - `pub const CATALOGUE_DIR: &str = "docs/99-reference/defect-catalogue";`
  - `#[derive(Deserialize)] #[serde(rename_all = "kebab-case")] pub enum Severity { Fork, DataLoss, Dos, Stall, Other }` + `Display` (kebab-case)
  - `#[derive(Deserialize)] #[serde(deny_unknown_fields)] pub struct ClassMeta { pub id: String, pub title: String, pub severity: Severity, pub applies_when: String, #[serde(default)] pub paths: Vec<String>, #[serde(default)] pub patterns: Vec<String> }`
  - `pub struct ClassFile { pub slug: String, pub meta: ClassMeta, pub body: String }`
  - `#[derive(Deserialize)] #[serde(rename_all = "kebab-case")] pub enum FoundBy { Review, Test, Testnet, Mainnet, Audit, Sweep, Other }`
  - `#[derive(Deserialize)] #[serde(deny_unknown_fields)] pub struct Instance { pub class: String, pub site: String, pub found: String, pub found_by: FoundBy, pub fix: String, pub released_in: String, pub note: Option<String> }`
  - `pub struct IndexRow { pub id: String, pub link: String, pub title: String, pub severity: String, pub applies_when: String }`
  - `pub struct Catalogue { pub classes: Vec<ClassFile>, pub instances: Vec<Instance>, pub index: Vec<IndexRow> }`
  - `pub fn split_front_matter(text: &str) -> eyre::Result<(&str, &str)>` — returns `(toml, body)`; text must start with `+++\n`, front matter ends at the next line that is exactly `+++`.
  - `pub fn parse_index(readme: &str) -> eyre::Result<Vec<IndexRow>>` — rows of the first table whose header starts with `| Id |`; skips the header and `|---` separator.
  - `pub fn load(dir: &Path) -> (Catalogue, Vec<String>)` — loads what parses; each parse failure becomes a problem string prefixed with the file path. Never panics.
  - `pub fn validate(cat: &Catalogue) -> Vec<String>` — all cross-checks; returns problem strings.
  - `pub fn check(dir: &Path) -> Vec<String>` = `load` problems + `validate` problems.
  - `pub fn parse_release(tag: &str) -> Option<(Stream, (u64, u64, u64))>` with `pub enum Stream { Testnet, Mainnet }`.
- Produces (CLI): `cargo xtask defects check [--dir <path>]` (default `CATALOGUE_DIR` relative to the workspace root); prints each problem on its own line and exits non-zero if any.

`validate` rules (each a distinct problem string): `id != slug`; slug not kebab-case; empty `title`/`applies_when`; a body section missing or out of order; a `patterns` regex that does not compile (`regex::Regex::new`); a `paths` glob that does not compile (`glob::Pattern::new`); an instance whose `class` has no class file; `found` not `YYYY-MM-DD`; `released_in` fails `parse_release`; empty `site` or `fix`; a class file with no index row, or an index row with no class file; an index row whose `link`, `title`, `severity` or `applies_when` differs from the class file (`link` must be `classes/<id>.md`); duplicate ids in the index.

- [ ] **Step 1: Write the failing tests** in `xtask/src/defects/catalogue.rs` `#[cfg(test)] mod tests`. Helper `fn write_catalogue(dir: &Path, readme: &str, classes: &[(&str, &str)], instances: &str)`. Fixture `VALID_CLASS` = front matter `id = "uncapped-x"`, `title = "Uncapped X"`, `severity = "dos"`, `applies_when = "adds a peer HTTP read"`, `patterns = ['\.json\(\)']`, `paths = ["crates/p2p/**"]`, then the four body sections. Fixture `VALID_README` with one matching index row. Tests:
  - `check_passes_on_valid_catalogue` — one class, one instance (`found = "2026-10-13"`, `released_in = "testnet-4.1.0"`) → `check(dir).is_empty()`.
  - `check_passes_on_empty_catalogue` — README with header-only table, empty `instances.toml`, no classes → empty.
  - `check_reports_bad_front_matter` — class text `"id = 1\n"` without `+++` → one problem containing `"uncapped-x.md"`.
  - `check_reports_id_slug_mismatch` — file `other.md` with `id = "uncapped-x"` → a problem containing `"other"`.
  - `check_reports_missing_section` — body without `## Correct form` → a problem containing `"Correct form"`.
  - `check_reports_bad_regex` — `patterns = ['(']` → a problem containing `"regex"`.
  - `check_reports_unknown_class_in_log` — instance `class = "nope"` → a problem containing `"nope"`.
  - `check_reports_bad_release_tag` — `released_in = "mainnet-latest"` → a problem containing `"mainnet-latest"`.
  - `check_reports_index_drift` — README row title `"Different"` → a problem containing `"title"`.
  - `check_reports_missing_index_row` — README with no rows, one class → a problem containing `"uncapped-x"`.
  - `index_row_with_backticks_parses` — `parse_index` on a row whose `applies_when` is ``adds `reqwest` `.json()` call`` → one row, field equal to that text.
  - `parse_release_accepts_and_rejects` — `testnet-4.1.1` → `Some((Testnet,(4,1,1)))`; `mainnet-latest`, `testnet-4.1` → `None`.

- [ ] **Step 2: Run tests to verify they fail**
  Run: `cargo nextest run -p xtask defects`
  Expected: compile failure (module does not exist).

- [ ] **Step 3: Implement** the types and functions above in `catalogue.rs`; `mod.rs` holds `pub mod catalogue;` and re-exports. Collect problems; never return early on the first one.

- [ ] **Step 4: Run tests to verify they pass**
  Run: `cargo nextest run -p xtask defects`
  Expected: all PASS.

- [ ] **Step 5: Wire the CLI.** In `main.rs` add `Defects { #[clap(subcommand)] action: DefectsAction }` with `enum DefectsAction { Check { #[clap(long)] dir: Option<PathBuf> } }` (Tasks 2 and 3 add variants). Call `Check` from `LocalChecks` after `Typos`. Create the skeleton catalogue files. Add the CI step and the reference README link.
  Run: `cargo xtask defects check`
  Expected: exit 0, no output (empty catalogue).

- [ ] **Step 6: Commit**
  ```bash
  git add xtask docs/99-reference .github/workflows/rust.yml Cargo.lock
  git commit -m "feat(xtask): add defect catalogue model and check command"
  ```

---

### Task 2: `defects trend`

**Files:**
- Create: `xtask/src/defects/trend.rs`
- Modify: `xtask/src/defects/mod.rs`, `xtask/src/main.rs`

**Interfaces:**
- Consumes: `Catalogue`, `Instance`, `Severity`, `parse_release`, `load`, `validate` from Task 1.
- Produces:
  - `pub enum Window { Since(chrono::NaiveDate), LastReleases(usize) }`
  - `pub struct TrendRow { pub id: String, pub severity: String, pub total: usize, pub in_window: Option<usize>, pub last_found: Option<NaiveDate>, pub last_release: Option<String> }`
  - `pub fn trend(cat: &Catalogue, window: Option<&Window>) -> Vec<TrendRow>` — one row per class, including classes with 0 instances. Sort: `in_window` desc, then `total` desc, then `id` asc.
  - `pub fn render_trend(rows: &[TrendRow], with_window: bool) -> String` — aligned plain-text table; header `id  severity  total  [window]  last-found  last-release`.
  - CLI: `cargo xtask defects trend [--dir <path>] [--since <YYYY-MM-DD> | --last-releases <N>]` (clap `conflicts_with`). Refuses to run (non-zero exit, prints problems) if `validate` reports problems.

Window rules (put in `--help`): `Since(d)` counts instances with `found >= d`. `LastReleases(n)` takes the `n` highest distinct `(major, minor, patch)` versions among all `released_in` values, testnet and mainnet together (one stream: a version is "released" when either stream first ships it), and counts instances whose version is ≥ the lowest of those `n`. `last_release` is the `released_in` with the highest version.

- [ ] **Step 1: Write the failing tests** in `trend.rs`, building `Catalogue` values directly (no files). Fixture: classes `a` (dos), `b` (fork), `c` (stall). Instances: `a` ×3 (`found` 2026-01-10 / 2026-09-01 / 2026-10-20; `released_in` testnet-3.0.0 / testnet-4.1.0 / testnet-4.1.1), `b` ×1 (2026-10-25, mainnet-4.1.1), `c` ×0.
  - `trend_no_window` — rows ordered `a, b, c`; `a.total == 3`, `a.in_window == None`, `a.last_found == 2026-10-20`, `a.last_release == "testnet-4.1.1"`; `c.total == 0`, `c.last_found == None`.
  - `trend_since` — `Since(2026-10-01)` → `a.in_window == Some(1)`, `b.in_window == Some(1)`, order `a, b, c` (tie on window broken by total).
  - `trend_last_releases_merges_streams` — `LastReleases(1)` → versions `{4.1.1}` → `a.in_window == Some(1)`, `b.in_window == Some(1)`. `LastReleases(2)` → `{4.1.1, 4.1.0}` → `a == Some(2)`.
  - `trend_empty` — empty catalogue → `trend` returns empty; `render_trend(&[], false)` is the header line only.
  - `render_includes_window_column_only_when_set` — header contains `window` iff `with_window`.

- [ ] **Step 2: Run** `cargo nextest run -p xtask defects::trend` → FAIL (compile).
- [ ] **Step 3: Implement** `trend.rs` and the CLI variant `Trend { dir, since: Option<NaiveDate>, last_releases: Option<usize> }`.
- [ ] **Step 4: Run** `cargo nextest run -p xtask defects` → PASS. Then `cargo xtask defects trend` → header only, exit 0.
- [ ] **Step 5: Commit** `git commit -m "feat(xtask): add defect catalogue trend command"`

---

### Task 3: `defects scan`

**Files:**
- Create: `xtask/src/defects/scan.rs`
- Modify: `xtask/src/defects/mod.rs`, `xtask/src/main.rs`

**Interfaces:**
- Consumes: `Catalogue`, `ClassFile`, `load`, `validate` from Task 1.
- Produces:
  - `pub struct AddedLine { pub path: String, pub line: u32, pub text: String }`
  - `pub fn parse_added_lines(diff: &str) -> Vec<AddedLine>` — parses `git diff -U0 --find-renames` output. Path from `+++ b/<path>`; `+++ /dev/null` (deletion) sets no path, so its lines are skipped. Hunk header `@@ -a[,b] +c[,d] @@`: start counter at `c`; each `+` line (not `+++`) yields an `AddedLine` and increments the counter. `Binary files ... differ` has no hunks, so it yields nothing.
  - `pub struct CompiledClass { pub id: String, pub paths: Vec<glob::Pattern>, pub patterns: Vec<regex::Regex> }`
  - `pub fn compile(cat: &Catalogue) -> eyre::Result<Vec<CompiledClass>>` — classes with no `patterns` are left out. Error if any pattern or glob does not compile.
  - `pub struct ScanHit { pub class: String, pub path: String, pub line: u32, pub text: String }`
  - `pub fn scan(classes: &[CompiledClass], added: &[AddedLine]) -> Vec<ScanHit>` — a line matches a class if (`paths` is empty or any glob matches `path`) and any regex matches `text`. One hit per (class, line). Output order: by path, line, class.
  - CLI: `cargo xtask defects scan [--dir <path>] [--base <ref>] [--head <ref>]`. Base default: `git merge-base master HEAD`. With `--head`: `git diff -U0 --find-renames <base>...<head>`; without: `git diff -U0 --find-renames <base>` (tracked working-tree files; `--help` says untracked files are not scanned). Prints `<class>  <path>:<line>  <trimmed text>`, then `N lead(s); verify each before you report it.` Exits 0 on hits. Refuses to run (non-zero) if `load`/`validate` report problems (Review Focus 5).

- [ ] **Step 1: Write the failing tests** in `scan.rs`:
  - `parses_simple_hunk` — diff with `+++ b/crates/p2p/src/a.rs`, `@@ -10,0 +11,2 @@`, two `+` lines → lines 11 and 12 with the right text.
  - `parses_hunk_without_count` — `@@ -3 +4 @@` with one `+` line → line 4.
  - `rename_counts_under_new_path` — `rename from old.rs` / `rename to new.rs`, `--- a/old.rs`, `+++ b/new.rs`, one hunk → `path == "new.rs"`.
  - `deleted_and_binary_yield_nothing` — a deletion block (`+++ /dev/null`, `-` lines) plus `Binary files a/x.png and b/x.png differ` → empty.
  - `multiple_files` — two files, each with one hunk → paths and lines are correct for both.
  - `scan_matches_regex_within_paths` — class `paths = ["crates/p2p/**"]`, `patterns = ['\.json\(\)']`; added lines `crates/p2p/src/a.rs:5 "let v = resp.json().await?;"` and `crates/types/src/b.rs:7 "x.json()"` → one hit, in `crates/p2p/src/a.rs`.
  - `scan_empty_paths_matches_everywhere` — same class without `paths` → two hits.
  - `scan_one_hit_per_class_and_line` — two patterns that both match one line → one hit.
  - `scan_rejects_invalid_catalogue` — `compile` on a class with pattern `(` → `Err`.

- [ ] **Step 2: Run** `cargo nextest run -p xtask defects::scan` → FAIL (compile).
- [ ] **Step 3: Implement** `scan.rs` and the CLI variant `Scan { dir, base: Option<String>, head: Option<String> }` (run git through `xshell`, as the rest of `main.rs` does).
- [ ] **Step 4: Run** `cargo nextest run -p xtask defects` → PASS. Then `cargo xtask defects scan` on this branch → `0 lead(s)…`, exit 0.
- [ ] **Step 5: Commit** `git commit -m "feat(xtask): add defect catalogue diff scan"`

---

### Task 4: Skills, README process, integration docs

**Files:**
- Create: `.agents/skills/defect-catalogue-review/SKILL.md`, `.agents/skills/defect-catalogue-add/SKILL.md`
- Modify: `docs/99-reference/defect-catalogue/README.md` (process + known non-issues), `AGENTS.md` (one line under "Skills & Commands"), `.github/PULL_REQUEST_TEMPLATE.md` (one checklist item)

**Interfaces:**
- Consumes: the CLI from Tasks 1–3 (`cargo xtask defects check | trend | scan`) and the file formats in Global Constraints.

Content requirements (wording is the builder's; ASD-STE100 style; follow `.agents/skills/monotonic-time/SKILL.md` front-matter shape: `name`, `description`):
- **Review skill** `description`: use in the final pre-merge review and the version-bump review, to check a diff or range against the defect catalogue. Body = the 7 steps in the spec section "Skill: `defect-catalogue-review`", in that order, including: read the README index only, then open only matching class files; scan hits are leads; check known non-issues before reporting; question tests and design; the skill does not write to the catalogue.
- **Add skill** `description`: use to turn a PR, commit or range into catalogue entries, for a new fix or for backfill. Body = the 6 steps in the spec section "Skill: `defect-catalogue-add`", including: one-off vs. class; disclosure check with `git tag --contains <sha> | grep -E '^(testnet|mainnet)-[0-9]'` and `gh api "repos/Irys-xyz/irys/security-advisories?state=draft"` + `?state=triage` (stop on a match); confirm a squash-merged fix is in the final tree; new class → class file + index row + log row, known class → log row; optional sibling sweep (unfixed sites are reported, never logged); finish with `cargo xtask defects check`. Include a class-file template and an `[[instance]]` template with every field.
- **README:** process bullets from the spec section "README.md"; known non-issue seed `### Reth/Irys state mismatch is fatal` with **Pattern** (`RethService` exits when Reth and Irys state differ) and **Why it is correct** (continuing on a mismatched state risks a fork or a wrong state; the node must stop).
- **AGENTS.md:** one line naming both skills and when to use them.
- **PR template:** `- [ ] If this fixes a new defect class, I added it to the defect catalogue (docs/99-reference/defect-catalogue/).`

- [ ] **Step 1: Write the files.**
- [ ] **Step 2: Verify** `cargo xtask defects check` → exit 0; `typos` on the new files → clean.
- [ ] **Step 3: Commit** `git commit -m "docs: add defect catalogue skills and process"`

---

### Task 5: Seed via the add skill

**Files:**
- Create: `docs/99-reference/defect-catalogue/classes/uncapped-peer-response-body.md`, `docs/99-reference/defect-catalogue/classes/hash-iteration-order-in-consensus.md`
- Modify: `docs/99-reference/defect-catalogue/README.md` (index rows), `docs/99-reference/defect-catalogue/instances.toml`
- Delete: `docs/99-reference/defect-catalogue/classes/.gitkeep`

- [ ] **Step 1: Follow `.agents/skills/defect-catalogue-add/SKILL.md`** for #1574, then #1678, then #1425. Expected results (from the spec section "Seed"; the skill must confirm each against the final tree and the tags):
  - `uncapped-peer-response-body`, severity `dos`; `paths` = `crates/p2p/**`, `crates/actors/**`, `crates/api-client/**`, `crates/chain/**`; patterns for `.json()`, `.text()`, `.bytes()` calls; correct form names the capped reader added by #1574 and `read_capped_bytes`. Rows: #1574 sites (gossip client, data-intervals fetch; `testnet-4.1.0`), #1678 `HttpChunkFetcher` chunk-body fetch (`testnet-4.1.1`).
  - `hash-iteration-order-in-consensus`, severity `fork`. Rows (both #1425, `testnet-3.0.0`): `crates/domain/src/models/block_tree.rs::BlockTree::get_by_solution_hash`, `crates/actors/src/block_validation.rs::get_assigned_ingress_proofs`. Do not log the reverted `assigned_miners` count change.
  - Sweep: off for this seed.
- [ ] **Step 2: Verify**
  - `cargo xtask defects check` → exit 0.
  - `cargo xtask defects trend` → two rows, `uncapped-peer-response-body` total 2 or more, `hash-iteration-order-in-consensus` total 2.
  - `cargo xtask defects scan --base 2a0008be1~1 --head 2a0008be1` → runs, exit 0 (hits are expected to be few; record the count in the commit body).
- [ ] **Step 3: Commit** `git commit -m "docs: seed the defect catalogue"`

---

### Task 6: Final checks and personal review hook

- [ ] **Step 1:** `cargo fmt --all` and `cargo xtask clippy` → clean.
- [ ] **Step 2:** `cargo nextest run -p xtask` → all PASS.
- [ ] **Step 3:** Add one line to `~/.claude/commands/branch-review.md` (outside the repo, not committed): "Also run the `defect-catalogue-review` skill on the same target and include its findings."
