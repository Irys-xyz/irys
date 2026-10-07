# Release Playbook

Step-by-step walkthrough for cutting a release. For the conceptual model
(why deployment branches exist, what each tag means, atomicity guarantees,
head-tracking semantics), see [`RELEASE_PROCESS.md`](./RELEASE_PROCESS.md).

This playbook covers the common case: a planned mainnet release of version
`X.Y.Z` that goes through testnet first, with env-specific patches on both
deployment branches and a custom changelog. Hotfixes, rollback, and
multi-major scenarios reference back to `RELEASE_PROCESS.md`.

Throughout, the example version is `1.2.3`.

## Checklist

Copy this into the release issue/PR and tick it off. A release is not finished
until every box is ticked — the post-tag steps are the ones that get skipped.

```text
Phase A — release/1.x
  [ ] cherry-pick the chosen commits from master (PR, not a direct push)
  [ ] bump crates/chain/Cargo.toml + Cargo.lock, PR titled "feat: release 1.2.3"
  [ ] freeze release/1.x until both testnet and mainnet are tagged

Phase B — testnet
  [ ] merge release/1.x forward into release/testnet/1.x (PR)
  [ ] apply per-release testnet patches (env-bound values only)
  [ ] dispatch release.yml (testnet, 1.2.3, branch-tip SHA)
  [ ] testnet-1.2.3 + testnet-latest + irys-testnet:1.2.3/:latest all moved
  [ ] deploy and validate

Phase C — master
  [ ] cherry-pick the version bump onto master (PR)
  [ ] master's crates/chain/Cargo.toml == newest testnet-X.Y.Z tag

Phase D/E — mainnet
  [ ] pre-flight: release/1.x unchanged since the testnet tag
  [ ] merge release/1.x forward into release/mainnet/1.x (PR) + mainnet patches
  [ ] dispatch release.yml (mainnet, 1.2.3, branch-tip SHA)
  [ ] edit the draft release notes and publish
  [ ] deploy
```

## Prerequisites

- `release/1.x` exists (created once per major from `master`)
- `release/testnet/1.x` exists (created once per major from `release/1.x`)
- `release/mainnet/1.x` exists (created once per major from `release/1.x`)
- All deployment branches are protected (PR-only, required CI, etc.)
- You have write access to the repo and `gh` CLI authenticated, OR can use the GitHub Actions UI
- Local checkout has the latest from `origin`

## Phase A — Prep on `release/1.x`

Cherry-pick the work that's going into this release, then bump the version.
This is the only place version bumps happen.

```bash
git fetch origin
git checkout release/1.x
git pull --ff-only
```

Cherry-pick from `master` (one PR per commit batch is recommended; the
`release/1.x` branch is protected). After the cherry-pick PR(s) merge:

> **Cherry-pick — do not merge `master` into `release/1.x`.** A merge sweeps in
> *everything* sitting on `master` at that moment, including work that was never
> triaged for this release. The release line then stops being a curated subset of
> `master` and you lose the ability to state what shipped. If you really do intend
> to take all of `master`, make that explicit in the PR title/description and have
> it reviewed as such — it is a deliberate exception, not the default.

> **Everything lands via PR.** `release/1.x` and both deployment branches are
> protected: never `git push` a release commit straight to them. A direct push
> skips review, leaves no PR record of what shipped, and is how steps get missed.

```bash
git pull --ff-only

$EDITOR crates/chain/Cargo.toml   # set version = "1.2.3"
cargo update -p irys-chain        # keep lockfile in sync

git add crates/chain/Cargo.toml Cargo.lock
git commit -m "feat: release 1.2.3"
# Open a PR for the version bump; merge once CI passes.
```

> **Use `feat: release <version>` verbatim** for the bump commit and its PR title.
> `conventional-pr.yaml` rejects any PR title that is not a conventional commit
> (`release:` is not a recognised type), and `.config/cliff.toml` sets
> `filter_unconventional = true`, so a non-conventional subject is silently dropped
> from the generated changelog as well.

After the bump lands on `release/1.x`, do not add further commits to this
branch until both the testnet and mainnet releases for `1.2.3` are tagged.
The release workflow validates that testnet and mainnet share the same
`release/1.x` merge-base; adding upstream commits between testnet and
mainnet invalidates that check.

## Phase B — Testnet release

Merge `release/1.x` forward into the testnet deployment branch and apply
any per-release testnet patches.

```bash
git checkout release/testnet/1.x
git pull --ff-only
git merge --no-ff origin/release/1.x \
  -m "merge: release/1.x into release/testnet/1.x for 1.2.3"
```

If the merge produces a `crates/chain/Cargo.toml` conflict, resolve to
`release/1.x`'s value (`1.2.3`). **Deployment branches never carry their own
version** — see [`RELEASE_PROCESS.md` § Authoring Deployment-Specific
Patches](./RELEASE_PROCESS.md#authoring-deployment-specific-patches).

Apply any per-release testnet patches as additional commits:

```bash
$EDITOR <testnet-specific-config>
git add … && git commit -m "chore(testnet): update bootstrap peers for 1.2.3"
```

Open a PR to land these on `release/testnet/1.x` (it's protected).
After the PR merges:

```bash
git fetch origin
TESTNET_SHA=$(git rev-parse origin/release/testnet/1.x)
echo "$TESTNET_SHA"
```

Dispatch the release workflow:

```bash
gh workflow run release.yml \
  -f release_type=testnet \
  -f version=1.2.3 \
  -f commit="$TESTNET_SHA"
```

Or use the GitHub UI: **Actions → Release → Run workflow**.

The workflow will:

1. Verify the commit is on `release/testnet/<major>.x`, **and is the tip of that branch** — releasing a non-tip commit silently drops the later commits on the branch and is a hard error unless you dispatch with `allow_non_tip_commit=true`.
2. Verify `crates/chain/Cargo.toml` version equals `1.2.3`.
3. Verify the CI gate target had a passing **core** CI run (Rust Checks and other required jobs; Benchmarks and Coverage do not gate). By default this is the commit's `release/1.x` merge-base (the upstream code being shipped, env-patches aside); dispatch with `gate_ci_on_trunk=false` to instead gate the release commit itself (use when the deployment branch is expected to be green in its own right). Skipped entirely with `force=true` or `dry_run=true`.
4. Build `ghcr.io/<owner>/irys-testnet:1.2.3`.
5. Push git tag `testnet-1.2.3`, push the Docker image, move the `testnet-latest` git tag.
6. Auto-publish a GitHub **prerelease** with the auto-generated changelog body.
7. As the final, non-fatal step, retag and push `irys-testnet:latest` (a failure here leaves `:latest` on the previous release rather than rolling back the published one — see [`RELEASE_PROCESS.md` § Atomicity](./RELEASE_PROCESS.md#atomicity)).

Then deploy `irys-testnet:1.2.3` to testnet and validate.

Once `testnet-1.2.3` exists, go do [Phase C](#phase-c--mirror-the-version-onto-master-mandatory) —
mirroring the version onto `master` is part of the release, not an afterthought.

### If testnet fails

| Where the bug lives | What to do |
|---|---|
| Upstream code (would also affect mainnet) | Fix on `release/1.x` → bump version to `1.2.4` → cherry-pick **both the fix and the bump** to `master` → repeat Phase B |
| Testnet-only (e.g. wrong bootstrap peer) | Commit the fix directly to `release/testnet/1.x` → bump `release/1.x` version to `1.2.4` and merge forward → repeat Phase B. The *fix* does not go to `master`, but the **version bump still does** (Phase C) |

Each iteration gets a new SemVer; earlier `testnet-1.2.X` tags are orphaned
by design — see [`RELEASE_PROCESS.md` § Version Iteration](./RELEASE_PROCESS.md#version-iteration).

## Phase C — Mirror the version onto `master` (mandatory)

**Do this as soon as `testnet-1.2.3` is tagged — before you start Phase D.**

`master` carries the version of the newest *released* build, so that anyone
reading `crates/chain/Cargo.toml` on `master` sees what is actually live and the
next release branches from a truthful version. The bump commit lives on
`release/1.x`, so it has to be carried up explicitly; nothing does this for you.

```bash
git fetch origin --tags
BUMP=$(git rev-parse origin/release/1.x)   # the "feat: release 1.2.3" commit

git checkout -b chore/mirror-1.2.3 origin/master
git cherry-pick -x "$BUMP"                 # Cargo.toml + Cargo.lock only
git push -u origin chore/mirror-1.2.3
gh pr create --base master --title "feat: release 1.2.3" \
  --body "Mirror the 1.2.3 version bump from release/1.x onto master."
```

Cherry-pick the *named bump commit* — do not merge `release/1.x` into `master`.
If the version bump was squashed together with other work on `release/1.x`, carry
only the `crates/chain/Cargo.toml` + `Cargo.lock` hunks.

If testnet then fails and you iterate to `1.2.4`, mirror that version too — the
invariant is that `master` equals the newest released version, not that every
attempted version reaches `master`.

Verify before moving on:

```bash
git fetch origin --tags
LATEST=$(git tag --list 'testnet-[0-9]*' | sort -V | tail -1)
echo "latest testnet tag: $LATEST"
echo "master version:     $(git show origin/master:crates/chain/Cargo.toml | grep -m1 '^version')"
# the two must agree
```

## Phase D — Mainnet release

**Pre-flight check:** confirm `release/1.x` has not advanced since
`testnet-1.2.3` was tagged.

```bash
git fetch origin --tags
TESTNET_BASE=$(git merge-base testnet-1.2.3 origin/release/1.x)
RELEASE_HEAD=$(git rev-parse origin/release/1.x)
if [ "$TESTNET_BASE" = "$RELEASE_HEAD" ]; then
  echo "OK: release/1.x is at the same commit testnet-1.2.3 was based on"
else
  echo "WARN: release/1.x has advanced since testnet — you'll need a new testnet release first"
fi
```

If the check warns, go back to Phase B with a new version (`1.2.4`).

> This local pre-flight is intentionally stricter than the workflow gate. The
> workflow only requires the testnet and mainnet commits to share the **same
> `release/<major>.x` merge-base** (same upstream code, env patches aside) — it
> does not require `release/<major>.x` to be unchanged. Keeping the branch
> frozen between the two releases is the simplest way to guarantee that, which
> is what this check verifies.

Merge `release/1.x` forward into the mainnet deployment branch:

```bash
git checkout release/mainnet/1.x
git pull --ff-only
git merge --no-ff origin/release/1.x \
  -m "merge: release/1.x into release/mainnet/1.x for 1.2.3"
```

Resolve any `Cargo.toml` conflicts to `release/1.x`'s value. Apply any
per-release mainnet patches:

```bash
$EDITOR <mainnet-specific-config>
git add … && git commit -m "chore(mainnet): update bootstrap peers for 1.2.3"
```

Open a PR, land it, capture the SHA:

```bash
git fetch origin
MAINNET_SHA=$(git rev-parse origin/release/mainnet/1.x)
```

Dispatch:

```bash
gh workflow run release.yml \
  -f release_type=mainnet \
  -f version=1.2.3 \
  -f commit="$MAINNET_SHA"
```

The workflow does everything testnet did, plus:

- Verifies `testnet-1.2.3` exists.
- Verifies `testnet-1.2.3` and `$MAINNET_SHA` share the same `release/1.x`
  merge-base (same upstream code; only env patches differ).
- Pushes git tag `mainnet-1.2.3`, image `irys-mainnet:1.2.3`, moves the
  `mainnet-latest` git tag.
- Creates a **draft** GitHub Release — does NOT auto-publish.
- As the final, non-fatal step, retags and pushes `irys-mainnet:latest`.

## Phase E — Custom changelog and publish

The draft body the workflow created has this shape:

````markdown
## Summary

<!-- Fill in release highlights, breaking changes, critical fixes -->

## Changes

### Features
- (foo): add new fee tier
- (bar): …

### Bug Fixes
- …

## Docker

```
docker pull ghcr.io/<owner>/irys-mainnet:1.2.3
```
````

The auto-generated `## Changes` section comes from git-cliff walking
commits from `mainnet-prev..HEAD` (testnet tags excluded via
`--ignore-tags ^testnet-`). Commit groupings (Features, Bug Fixes, etc.)
are driven by `.config/cliff.toml`'s `commit_parsers`.

Three ways to add your custom prose:

### (a) Edit in the GitHub web UI

Open the draft in **Releases → Drafts**, edit the body, click **Publish**. Simplest.

### (b) Edit via `gh` CLI

```bash
gh release view mainnet-1.2.3 --json body -q .body > /tmp/draft-notes.md
$EDITOR /tmp/draft-notes.md
# Replace the <!-- … --> placeholder with the release summary,
# optionally reorganize the Changes section, add migration notes, etc.

gh release edit mainnet-1.2.3 --notes-file /tmp/draft-notes.md
gh release edit mainnet-1.2.3 --draft=false   # publish
```

### (c) Pre-compose locally

Generate the auto-changelog yourself ahead of time and replace the draft
body wholesale:

```bash
# Preview the same changelog the workflow will produce
git cliff --config .config/cliff.toml \
  --unreleased --tag mainnet-1.2.3 --ignore-tags '^testnet-' \
  > /tmp/auto-changes.md

# Compose final notes around it
cat > /tmp/release-notes.md <<EOF
## Summary

This release introduces <…>. Validators on 1.0.x should upgrade by <date>.
See migration notes below.

## Highlights

- <hand-picked bullet>
- <hand-picked bullet>

## Migration

\`\`\`
<commands or config diffs operators need to apply>
\`\`\`

## Full changelog

$(cat /tmp/auto-changes.md)

## Docker

\`\`\`
docker pull ghcr.io/<owner>/irys-mainnet:1.2.3
\`\`\`
EOF

gh release edit mainnet-1.2.3 --notes-file /tmp/release-notes.md
gh release edit mainnet-1.2.3 --draft=false
```

Approach (c) gives full control: the auto-generated content becomes one
section among several you arrange yourself.

After publishing, deploy `irys-mainnet:1.2.3` to mainnet.

## Quick decision points

| Question | Answer |
|---|---|
| Where do I bump the version? | Only on `release/1.x`. Deployment branches inherit via merge. |
| Cargo.toml conflicts during merge-forward? | Always resolve to `release/1.x`'s value. |
| Bug found on testnet — where do I fix it? | Shared code: fix on `release/1.x` → bump version → backport (cherry-pick) to `master` → re-do Phase B. Env-specific: commit to the affected `release/<env>/1.x` + bump version on `release/1.x` — the *fix* is not backported, but the *version bump* still is. |
| Does the version bump always go back to `master`? | Yes — every released version, including env-specific-fix iterations and hotfixes. Only the *code* of an env-specific fix stays off `master`. See [Phase C](#phase-c--mirror-the-version-onto-master-mandatory). |
| Can I merge `master` into `release/<major>.x` instead of cherry-picking? | Not by default — it ships whatever happens to be on `master`. If you mean it, say so in the PR and have it reviewed as a deliberate whole-branch take. |
| Can I push release commits directly to `release/*`? | No. Every commit on `release/<major>.x` and the deployment branches lands via PR. |
| What are the `release/<env>/X.Y.Z` branches? | Frozen per-version snapshots pushed by `release.yml` so the released commit has branch reachability. Cosmetic — never commit to them. |
| What about the old `deployment/*` branches? | Dead — superseded by `release/*`. Never push to them; a push there publishes nothing. |
| Critical mainnet hotfix without testnet? | Dispatch with `force=true`. See [`RELEASE_PROCESS.md` § Hotfixes](./RELEASE_PROCESS.md#hotfixes). |
| Wrong changelog scope on mainnet? | Edit the draft before publishing — nothing assumes the auto-generated text is final. |
| Need to roll back? | Dispatch `docker-retag.yml`. See [`RELEASE_PROCESS.md` § Rollback](./RELEASE_PROCESS.md#rollback). |
| Intentionally releasing a commit behind the branch tip? | Dispatch with `allow_non_tip_commit=true`. Otherwise the release is rejected — a non-tip commit drops the later commits on `release/<env>/<major>.x` (this is what caused the 4.0.2 `revertme` commit to be excluded). |
| Deployment branch itself passes CI (env patches don't break it)? | Dispatch with `gate_ci_on_trunk=false` to gate CI on the release commit directly instead of the `release/<major>.x` trunk merge-base — this way the env-specific patches get CI-verified too rather than trusted implicitly. |
| Want to test the workflow without publishing? | Dispatch with `dry_run=true`. Validates and builds; skips tag/image push and GH Release creation, skips the release-base CI gate, and runs without the environment approval gate (so a mainnet dry-run needs no reviewer and won't block a queued real release). |

## Hotfixes and emergencies

For the abbreviated path (skip testnet, deploy direct to mainnet), see
[`RELEASE_PROCESS.md` § Hotfixes](./RELEASE_PROCESS.md#hotfixes). The same
phases apply; you use `force=true` to bypass the testnet-merge-base check and the
release-base CI gate.

## Rollback

For rolling testnet or mainnet back to a previous version, see
[`RELEASE_PROCESS.md` § Rollback](./RELEASE_PROCESS.md#rollback). Uses
`docker-retag.yml` — no rebuild, just re-tags the existing image and moves
the `<env>-latest` git tag.

## Dry-run testing (validate the pipeline without publishing)

`dry_run=true` exercises the whole release path — input validation, commit
provenance, version match, the **real Docker build**, and changelog generation —
then skips every mutating step: no git tag, no image push, no `latest` move, no
GitHub Release. Use it to prove the workflow and the build are healthy before a
real cut, or after changing the workflow itself.

A dry-run also skips the release-base CI gate (it can run before `release/<major>.x`
exists — the setup below branches only the `release/<env>/<major>.x` branch from
`master`), so it does not require the merge-base commit to have a passing CI run.

A dry-run resolves its `environment` to empty, so it does **not** wait on the
`testnet`/`mainnet` approval gate and does **not** require those Environments to
exist yet. It still runs in the `release` concurrency group, so it can't race a
real publish.

### Prerequisites (from the current state of the repo)

1. **The release workflow must already be on the default branch (`master`).**
   `workflow_dispatch` workflows are only dispatchable once they exist on the
   default branch — there is no way to dispatch one that lives only on a feature
   branch. So `release.yml` must be merged to `master` before you can dispatch it.
   Dispatch from `master`; the `commit` input, not the workflow's branch, decides
   what gets built.
2. **A `release/<env>/<major>.x` branch must exist for the env you test,** with
   `crates/chain/Cargo.toml` at the version you'll pass — provenance derives that
   exact branch name. For a `3.0.0` dry-run, create `release/testnet/3.x` (it slots
   into the existing `release/testnet/*` namespace, so there's no ref conflict);
   branching it straight from `origin/master` is enough, since `master` is already
   at `3.0.0`.
3. **An online self-hosted `misc-runner` with rootless Docker** — the dry-run runs
   a full `docker build`, which is the slow part and the main thing it validates.

### Testnet dry-run (simplest)

```bash
# one-time: a 3.x testnet deployment branch (master is already at version 3.0.0)
git fetch origin
git checkout -b release/testnet/3.x origin/master
git push -u origin release/testnet/3.x

COMMIT=$(git rev-parse origin/release/testnet/3.x)

# dispatch the dry-run (release.yml must already be on master)
gh workflow run release.yml \
  -f release_type=testnet \
  -f version=3.0.0 \
  -f commit="$COMMIT" \
  -f dry_run=true

# follow it
gh run watch "$(gh run list --workflow=release.yml -L1 --json databaseId -q '.[0].databaseId')"
```

A green run means inputs, provenance, version match, the image build, and changelog
generation all succeeded — and nothing was published.

### Mainnet dry-run

Mainnet's validate job also runs the testnet↔mainnet merge-base gate, and that runs
even on a dry-run. For a standalone dry-run with no prior testnet release, skip it
with `force=true`; otherwise you'd also need a real `testnet-3.0.0` tag, a
`release/3.x` branch, and a matching merge-base.

```bash
git checkout -b release/mainnet/3.x origin/master
git push -u origin release/mainnet/3.x
COMMIT=$(git rev-parse origin/release/mainnet/3.x)

gh workflow run release.yml \
  -f release_type=mainnet \
  -f version=3.0.0 \
  -f commit="$COMMIT" \
  -f dry_run=true \
  -f force=true        # skip the testnet merge-base gate for a standalone dry-run
```

### Cleanup

A dry-run publishes nothing, so there's nothing to roll back. If you created the
`release/<env>/3.x` branches purely to test, delete them afterward. If this is
the real `3.x` line, keep them and cut the actual release by re-dispatching without
`dry_run` (and, for mainnet, without `force` once a matching `testnet-3.0.0` exists).
