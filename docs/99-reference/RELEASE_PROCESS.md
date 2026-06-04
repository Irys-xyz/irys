# Irys Release Process

## Environments

| Environment | Deploys From                         | Git Tag           | Image Stream                     |
|---|---|---|---|
| **Devnet**  | `release/devnet`                  | (none)            | `ghcr.io/<owner>/irys-devnet`    |
| **Testnet** | `release/testnet/<major>.x`       | `testnet-X.Y.Z`   | `ghcr.io/<owner>/irys-testnet`   |
| **Mainnet** | `release/mainnet/<major>.x`       | `mainnet-X.Y.Z`   | `ghcr.io/<owner>/irys-mainnet`   |

## Branches

- **`master`** — integration branch. All feature work merges here via PR. Not deployed to any environment.
- **`release/<major>.x`** — long-lived release branches (e.g. `release/1.x`). Source of truth for the canonical SemVer version. Created from `master`; receives cherry-picks from `master`. Version bumps in `crates/chain/Cargo.toml` happen on these branches directly. Never deployed.
- **`release/devnet`** — long-lived branch for devnet. Carries devnet-specific patches on top of `master`. Built on demand.
- **`release/testnet/<major>.x`** — long-lived branch per major. `release/<major>.x` merged forward + testnet-specific patch commits (chain IDs, bootstrap peers, etc.).
- **`release/mainnet/<major>.x`** — long-lived branch per major. `release/<major>.x` merged forward + mainnet-specific patch commits.
- **Feature branches** — short-lived, merge into `master` via PR.

## Versioning

**Semantic Versioning** (`MAJOR.MINOR.PATCH`):

- **MAJOR** — breaking protocol changes, consensus-incompatible upgrades, node-software-specific changes
- **MINOR** — new features, non-breaking protocol changes
- **PATCH** — bug fixes, performance improvements, configuration changes

### Tags

- `testnet-X.Y.Z` — testnet release, deployed to testnet. Auto-published as a GitHub prerelease.
- `mainnet-X.Y.Z` — mainnet release, deployed to mainnet. Created as a draft GitHub Release — a maintainer reviews and publishes.
- `testnet-latest` — head-tracking git tag, always points to the most recent testnet release.
- `mainnet-latest` — head-tracking git tag, always points to the most recent mainnet release.

Inside each Docker image stream, tags are plain SemVer:

- `irys-testnet:X.Y.Z`, `irys-testnet:latest`
- `irys-mainnet:X.Y.Z`, `irys-mainnet:latest`
- `irys-devnet:<short-sha>`, `irys-devnet:latest`

The image stream name disambiguates the environment, so the Docker tag itself does not carry an env prefix. Git tags do, because they share a single namespace.

The canonical version source is the `irys-chain` crate (`crates/chain/Cargo.toml`) on `release/<major>.x`. The workflow validates that the deployment-branch tip's `crates/chain/Cargo.toml` matches the release version input.

### Version Iteration

When iterating releases, each fix gets a new version: `testnet-1.0.0` → fix → `testnet-1.0.1` → fix → `testnet-1.0.2`. The mainnet release version matches the final testnet release — if the last testnet release is `testnet-1.0.2`, the mainnet tag is `mainnet-1.0.2`. Earlier testnet versions (`testnet-1.0.0`, `testnet-1.0.1`) are orphaned. This is expected.

Testnet releases ARE the release candidates for mainnet — there is no separate `rc-` concept.

## Release Flow

```text
master ──► release/devnet ──► devnet (on demand)
  │
  ▼ (cherry-pick / merge)
release/<major>.x  (canonical version source; never deployed directly)
  │
  ├──► release/testnet/<major>.x  ──testnet-X.Y.Z──► testnet (auto-published prerelease)
  └──► release/mainnet/<major>.x  ──mainnet-X.Y.Z──► mainnet (draft release → manual publish)
```

1. Work merges into `master`. Devnet builds from `release/devnet` (merged forward from `master` on demand).
2. When ready to release, cherry-pick changes from `master` into `release/<major>.x`, bump the version in `crates/chain/Cargo.toml`, and stabilize.
3. Merge `release/<major>.x` forward into `release/testnet/<major>.x`. Apply any per-release testnet patches as additional commits. Trigger the **Release** workflow as `testnet` → validates (including that the commit's `release/<major>.x` merge-base had a fully passing CI run), builds, pushes `testnet-X.Y.Z` git tag, pushes image to `irys-testnet:X.Y.Z`, updates `testnet-latest` (both Docker and git), creates auto-published prerelease. Deploy to testnet.
4. After testnet validation, merge `release/<major>.x` forward into `release/mainnet/<major>.x`. Apply any per-release mainnet patches as additional commits. Trigger the **Release** workflow as `mainnet` on a commit whose `release/<major>.x` merge-base matches the testnet release's → validates, builds, pushes `mainnet-X.Y.Z` git tag, pushes image to `irys-mainnet:X.Y.Z`, updates `mainnet-latest`, creates **draft** release. A maintainer reviews the notes and publishes.

### Atomicity

The workflow builds the Docker image **before** pushing any git tags. If the build fails, nothing is published. Publish order is: push version git tag → push versioned Docker image → move `<env>-latest` git tag → create GH Release → move Docker `:latest`. If any step from `push git tag` through `create GH Release` fails, cleanup steps automatically delete orphaned git tags and restore the env-prefixed head-tracking git tag (`testnet-latest` / `mainnet-latest`) to its previous position. The versioned Docker image stays in the registry (orphaned) on failure. The **Docker Retag** workflow only moves `:latest`, so it cannot remove or rewrite the orphaned `X.Y.Z` tag — delete it manually in GHCR, or overwrite it via a `docker.yml` rebuild of the exact released commit.

The Docker `:latest` retag is the **final** step and is marked `continue-on-error`, so a failure there does not roll back the rest of the release. If `:latest` is not moved, the workflow logs a warning telling the operator to fix it by re-running **Docker Retag** with `source_tag=<version>` and `target_tag=latest`.

### Head-Tracking Tags

Both git and Docker have head-tracking tags that are updated automatically:

| Release Type | Git Tag           | Docker Tag (within stream) |
|---|---|---|
| Testnet      | `testnet-latest`  | `latest` (on `irys-testnet`)  |
| Mainnet      | `mainnet-latest`  | `latest` (on `irys-mainnet`)  |

These are force-updated on every successful release. They can also be manually moved for rollback via the **Docker Retag** workflow.

## Initial Repo Setup

These steps run once per repo, before the first release ever lands. They establish the GitHub Environments the workflows consult, the long-lived branches the release flow needs, and the protection rules that gate writes to them.

### 1. GitHub Environments

The release-touching workflows (`release.yml`, `docker.yml`, `docker-retag.yml`) declare `environment: ${{ inputs.<env> }}` on their build/publish jobs. Three matching environments must exist in **Settings → Environments** or the workflows will fail to start with "environment was not found".

| Environment | Used by | Suggested protection rules |
|---|---|---|
| `devnet`  | `docker.yml` | None — devnet builds on demand. |
| `testnet` | `release.yml`, `docker.yml`, `docker-retag.yml` | Optional reviewers; testnet is the rc path. |
| `mainnet` | `release.yml`, `docker.yml`, `docker-retag.yml` | **Required reviewers** — the gate before any mainnet write. |

Required reviewers on `mainnet` apply to every job that uses `environment: mainnet`, so a single setting covers cutting a release, an emergency Docker rebuild, and a Docker retag/rollback. The environments are global to the repo — they do not need to be re-created per release branch.

A `release.yml` dispatch with `dry_run=true` resolves its `environment` to an empty string and therefore skips this gate entirely: a dry-run needs no reviewer approval, doesn't block on one, and doesn't even require the environments to exist yet. See [`RELEASE_PLAYBOOK.md` § Dry-run testing](./RELEASE_PLAYBOOK.md#dry-run-testing-validate-the-pipeline-without-publishing).

### 2. Self-Hosted Runners

The workflows pin to `[self-hosted, misc-runner]` and `[self-hosted, test-runner]` labels. Runner setup lives in the [`Irys-CI`](https://github.com/Irys-xyz/Irys-CI) repo and registers runners at repo or org scope. The release flow only needs `misc-runner`.

### 3. Long-Lived Branches

Create and protect:

- `master` — already exists.
- `release/devnet` — branch from `master`.

Branch protection on all of these (and on the per-major release branches below) should require: PR review, passing CI, no force-push, no direct delete.

### 4. First Release Branch

Follow [Per-Release-Branch Setup](#per-release-branch-setup) below for the first `release/<major>.x`.

## Per-Release-Branch Setup

Each time you start a new release line — typically a new major version — three branches need to exist before you can dispatch the release workflow against them.

```bash
# Pick the major you're cutting. The example below uses 1.
MAJOR=1

git fetch origin
git checkout master
git pull --ff-only

# 1. Long-lived release branch — canonical version source.
git checkout -b "release/${MAJOR}.x" master
git push -u origin "release/${MAJOR}.x"

# 2. Deployment branches — one per env, branched from the release branch.
git checkout -b "release/testnet/${MAJOR}.x" "release/${MAJOR}.x"
git push -u origin "release/testnet/${MAJOR}.x"

git checkout -b "release/mainnet/${MAJOR}.x" "release/${MAJOR}.x"
git push -u origin "release/mainnet/${MAJOR}.x"
```

Then in **Settings → Branches**, apply branch protection to all three new branches with the same rules as `master` (PR review, CI, no force-push). The Irys-CI runners listen for jobs from `release/*` already — see `rust.yml` for the always-run conditions.

You do **not** need to create new GitHub Environments for a new release branch. The three environments configured in [Initial Repo Setup](#1-github-environments) are reused across all majors.

After the branches exist, follow [`RELEASE_PLAYBOOK.md`](./RELEASE_PLAYBOOK.md) to cut the first release on that major (cherry-pick → version bump → merge forward → dispatch).

## Hotfixes

Hotfixes **invert** the normal direction. The normal flow develops on `master` — which runs *ahead* of production, carrying unreleased work — and curates changes *down* into the release line. A hotfix targets code that is already **deployed**, i.e. the release line, which sits *behind* `master`. So the fix originates on the release line and is ported *up* to `master`, never developed on `master` first — that would build it against unstable, ahead-of-production code. Pick the path by where the bug lives.

### Shared-code hotfix (must reach every environment)

The bug is in code shared across environments. The fix routes through `release/<major>.x` — the env-agnostic source of truth and the only shared ancestor of both deployment branches.

1. Branch off `release/<major>.x`, write the fix + a regression test, and pre-flight on **devnet**. (You may instead branch off a `release/<env>/<major>.x` to reproduce against exactly what's deployed — but the commit that *lands* must be the isolated, env-agnostic fix. If it won't apply to `release/<major>.x`, it's really an env-specific fix — see below.)
2. Land the fix on `release/<major>.x` (cherry-pick the isolated fix commit if you developed off an env branch) and bump the PATCH version in `crates/chain/Cargo.toml` there — the canonical version source.
3. **Backport to `master` (mandatory)** — otherwise the fix regresses the next time a release is cut from `master`. Cherry-pick the *named* commit(s) up to `master` (carry the version-bump commit too, to keep master's version mirroring the latest release; drop that hunk in the rare case master is already version-ahead). Cherry-pick the specific commits — do **not** merge, and do **not** later re-cherry-pick them downward (they now exist under two SHAs, so a range cherry-pick from `master` could re-apply and conflict).
4. Merge `release/<major>.x` forward into `release/testnet/<major>.x`, dispatch the **Release** workflow as `testnet`, deploy, and endurance-test — testnet is the release candidate.
5. After the testnet soak, merge `release/<major>.x` forward into `release/mainnet/<major>.x`, dispatch as `mainnet`, deploy.

### Env-specific hotfix (one environment only)

The bug is in environment-local code or config (chain IDs, bootstrap peers, env-only consensus divergence). The fix must **not** leave that environment, or it would contaminate the shared branches. Branch off the affected `release/<env>/<major>.x`, fix, and merge back into that same branch; bump the version on `release/<major>.x` (still the canonical source) and merge forward; dispatch the env's release. **No `master` backport** — the change is environment-local.

### Emergency (skip testnet, straight to mainnet)

Apply the fix directly to `release/mainnet/<major>.x` and dispatch with the `force` flag to skip the testnet-merge-base validation and the release-base CI gate, bypassing the normal master → release → deployment progression. Once the fire is out, reconcile: port the fix to `release/<major>.x` and `master` so it isn't stranded on the deployment branch.

## Rollback

To roll back testnet or mainnet to a previous version, use the **Docker Retag** workflow (`docker-retag.yml`):

1. Trigger the workflow with `environment` set to the env (`testnet` or `mainnet`), `source_tag` set to the Docker version to roll back to (e.g., `1.0.1` — the Docker tag inside the stream, not the env-prefixed git tag `testnet-1.0.1`), and `target_tag` set to `latest`.
2. The workflow re-tags the existing Docker image inside the env's image stream — no rebuild.
3. With `move_git_tag` enabled (default), the corresponding env-prefixed git tag (`testnet-latest` or `mainnet-latest`) is also moved to match.

This is the fastest rollback path — it re-uses the existing tested image.

> **If a release is in flight, cancel it first.** `release.yml`, `docker.yml`,
> and `docker-retag.yml` share the `release` concurrency group (with
> `cancel-in-progress: false`) so they can never race each other over `:latest`
> or a version tag. A `release.yml` run parked on its environment-approval gate
> still **holds** that slot, so a rollback dispatched while a release awaits
> approval will queue behind it. In an emergency, cancel the pending release run
> (Actions → the run → Cancel) before dispatching `docker-retag.yml`. The shared
> group is deliberate: a rollback racing a concurrent publish is more dangerous
> than a rollback that waits.

## Process in Action

Starting from a new major version:

1. Branch `release/1.x` from `master`.
2. Cherry-pick the desired commits from `master` to `release/1.x`.
3. Bump version in `crates/chain/Cargo.toml` on `release/1.x`, commit.
4. Branch `release/testnet/1.x` from `release/1.x` (first time only). Apply any sticky testnet patches.
5. Trigger the release workflow as `testnet` targeting a commit on `release/testnet/1.x` → validates, builds, pushes `testnet-1.0.0` tag + image to `irys-testnet`, updates `testnet-latest`, auto-publishes prerelease. Deploy to testnet.
6. Validate on testnet. Fix issues on `release/1.x` and backport (cherry-pick) to `master` (if upstream) or by direct commit to `release/testnet/1.x` (if env-specific), bump version on `release/1.x` and merge forward, tag `testnet-1.0.1`, repeat as needed. Earlier testnet versions are orphaned.
7. Once stable, branch `release/mainnet/1.x` from `release/1.x` (first time only). Apply any sticky mainnet patches. Merge `release/1.x` forward.
8. Trigger the release workflow as `mainnet` on a commit on `release/mainnet/1.x` whose `release/1.x` merge-base matches the testnet release's → validates, builds, pushes `mainnet-1.0.1` tag + image, updates `mainnet-latest`, creates draft release. Maintainer reviews and publishes. Deploy to mainnet.
9. Future minor/patch releases continue on `release/1.x` and flow through the same deployment branches. A new `release/2.x` branch plus new `release/testnet/2.x` and `release/mainnet/2.x` are created when a major version bump is needed.

## Authoring Deployment-Specific Patches

Deployment-specific patches (e.g., chain IDs, bootstrap peer addresses, env-specific hardcoded values) live as normal git commits on the relevant `release/<env>/<major>.x` branch. They are reviewed via PR into the deployment branch like any other change.

**Rules:**

- Version bumps in `crates/chain/Cargo.toml` happen **only on `release/<major>.x`**. Deployment branches inherit the version via merge-forward.
- If a `release/<major>.x` → deployment-branch merge produces a Cargo.toml version conflict, resolve to `release/<major>.x`'s value. The deployment branch must never carry its own version.
- Patches that should apply to multiple envs go on `release/<major>.x`, not on individual deployment branches.
- Patches that change protocol behavior should never be env-specific. Env patches are for env-bound values only.
- The testnet ↔ mainnet validation in the release workflow enforces that both commits share the same `release/<major>.x` merge-base — i.e. same upstream code, only env-patches differ.
- The release workflow also requires that `release/<major>.x` merge-base to have had a fully passing CI run (defense-in-depth on top of branch protection). `force=true` bypasses this for emergency hotfixes; `dry_run=true` skips it. Note this gates the upstream merge-base, not the env-patch commits on the deployment branch — keep env patches to env-bound values only (per the rule above) so untested protocol behavior cannot ride in via a deployment-branch patch.
