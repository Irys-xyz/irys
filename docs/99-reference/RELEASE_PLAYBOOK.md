# Release Playbook

Step-by-step walkthrough for cutting a release. For the conceptual model
(why deployment branches exist, what each tag means, atomicity guarantees,
head-tracking semantics), see [`RELEASE_PROCESS.md`](./RELEASE_PROCESS.md).

This playbook covers the common case: a planned mainnet release of version
`X.Y.Z` that goes through testnet first, with env-specific patches on both
deployment branches and a custom changelog. Hotfixes, rollback, and
multi-major scenarios reference back to `RELEASE_PROCESS.md`.

Throughout, the example version is `1.2.3`.

## Prerequisites

- `release/1.x` exists (created once per major from `master`)
- `deployment/testnet/1.x` exists (created once per major from `release/1.x`)
- `deployment/mainnet/1.x` exists (created once per major from `release/1.x`)
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

```bash
git pull --ff-only

$EDITOR crates/chain/Cargo.toml   # set version = "1.2.3"
cargo update -p irys-chain        # keep lockfile in sync

git add crates/chain/Cargo.toml Cargo.lock
git commit -m "release: bump irys-chain to 1.2.3"
# Open a PR for the version bump; merge once CI passes.
```

After the bump lands on `release/1.x`, do not add further commits to this
branch until both the testnet and mainnet releases for `1.2.3` are tagged.
The release workflow validates that testnet and mainnet share the same
`release/1.x` merge-base; adding upstream commits between testnet and
mainnet invalidates that check.

## Phase B — Testnet release

Merge `release/1.x` forward into the testnet deployment branch and apply
any per-release testnet patches.

```bash
git checkout deployment/testnet/1.x
git pull --ff-only
git merge --no-ff origin/release/1.x \
  -m "merge: release/1.x into deployment/testnet/1.x for 1.2.3"
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

Open a PR to land these on `deployment/testnet/1.x` (it's protected).
After the PR merges:

```bash
git fetch origin
TESTNET_SHA=$(git rev-parse origin/deployment/testnet/1.x)
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

1. Verify the commit is on `deployment/testnet/<major>.x`.
2. Verify `crates/chain/Cargo.toml` version equals `1.2.3`.
3. Build `ghcr.io/<owner>/irys-testnet:1.2.3`.
4. Push git tag `testnet-1.2.3`, push the Docker image, move the `testnet-latest` git tag.
5. Auto-publish a GitHub **prerelease** with the auto-generated changelog body.
6. As the final, non-fatal step, retag and push `irys-testnet:latest` (a failure here leaves `:latest` on the previous release rather than rolling back the published one — see [`RELEASE_PROCESS.md` § Atomicity](./RELEASE_PROCESS.md#atomicity)).

Then deploy `irys-testnet:1.2.3` to testnet and validate.

### If testnet fails

| Where the bug lives | What to do |
|---|---|
| Upstream code (would also affect mainnet) | Cherry-pick the fix from `master` to `release/1.x` → bump version to `1.2.4` → repeat Phase B |
| Testnet-only (e.g. wrong bootstrap peer) | Commit the fix directly to `deployment/testnet/1.x` → bump `release/1.x` version to `1.2.4` and merge forward → repeat Phase B |

Each iteration gets a new SemVer; earlier `testnet-1.2.X` tags are orphaned
by design — see [`RELEASE_PROCESS.md` § Version Iteration](./RELEASE_PROCESS.md#version-iteration).

## Phase C — Mainnet release

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
git checkout deployment/mainnet/1.x
git pull --ff-only
git merge --no-ff origin/release/1.x \
  -m "merge: release/1.x into deployment/mainnet/1.x for 1.2.3"
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
MAINNET_SHA=$(git rev-parse origin/deployment/mainnet/1.x)
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

## Phase D — Custom changelog and publish

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
| Bug found on testnet — where do I fix it? | Upstream code: `master` → cherry-pick to `release/1.x` → bump version → re-do Phase B. Env-specific: commit to the affected `deployment/<env>/1.x` + bump version on `release/1.x`. |
| Critical mainnet hotfix without testnet? | Dispatch with `force=true`. See [`RELEASE_PROCESS.md` § Hotfixes](./RELEASE_PROCESS.md#hotfixes). |
| Wrong changelog scope on mainnet? | Edit the draft before publishing — nothing assumes the auto-generated text is final. |
| Need to roll back? | Dispatch `docker-retag.yml`. See [`RELEASE_PROCESS.md` § Rollback](./RELEASE_PROCESS.md#rollback). |
| Want to test the workflow without publishing? | Dispatch with `dry_run=true`. Validates and builds; skips tag/image push and GH Release creation. |

## Hotfixes and emergencies

For the abbreviated path (skip testnet, deploy direct to mainnet), see
[`RELEASE_PROCESS.md` § Hotfixes](./RELEASE_PROCESS.md#hotfixes). The same
phases apply; you use `force=true` to bypass the testnet-merge-base check.

## Rollback

For rolling testnet or mainnet back to a previous version, see
[`RELEASE_PROCESS.md` § Rollback](./RELEASE_PROCESS.md#rollback). Uses
`docker-retag.yml` — no rebuild, just re-tags the existing image and moves
the `<env>-latest` git tag.
