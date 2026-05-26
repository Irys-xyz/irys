# Irys Release Process

## Environments

| Environment | Deploys From                         | Git Tag           | Image Stream                     |
|---|---|---|---|
| **Devnet**  | `deployment/devnet`                  | (none)            | `ghcr.io/<owner>/irys-devnet`    |
| **Testnet** | `deployment/testnet/<major>.x`       | `testnet-X.Y.Z`   | `ghcr.io/<owner>/irys-testnet`   |
| **Mainnet** | `deployment/mainnet/<major>.x`       | `mainnet-X.Y.Z`   | `ghcr.io/<owner>/irys-mainnet`   |

## Branches

- **`master`** — integration branch. All feature work merges here via PR. Not deployed to any environment.
- **`release/<major>.x`** — long-lived release branches (e.g. `release/1.x`). Source of truth for the canonical SemVer version. Created from `master`; receives cherry-picks from `master`. Version bumps in `crates/chain/Cargo.toml` happen on these branches directly. Never deployed.
- **`deployment/devnet`** — long-lived branch for devnet. Carries devnet-specific patches on top of `master`. Built on demand.
- **`deployment/testnet/<major>.x`** — long-lived branch per major. `release/<major>.x` merged forward + testnet-specific patch commits (chain IDs, bootstrap peers, etc.).
- **`deployment/mainnet/<major>.x`** — long-lived branch per major. `release/<major>.x` merged forward + mainnet-specific patch commits.
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

```
master ──► deployment/devnet ──► devnet (on demand)
  │
  ▼ (cherry-pick / merge)
release/<major>.x  (canonical version source; never deployed directly)
  │
  ├──► deployment/testnet/<major>.x  ──testnet-X.Y.Z──► testnet (auto-published prerelease)
  └──► deployment/mainnet/<major>.x  ──mainnet-X.Y.Z──► mainnet (draft release → manual publish)
```

1. Work merges into `master`. Devnet builds from `deployment/devnet` (merged forward from `master` on demand).
2. When ready to release, cherry-pick changes from `master` into `release/<major>.x`, bump the version in `crates/chain/Cargo.toml`, and stabilize.
3. Merge `release/<major>.x` forward into `deployment/testnet/<major>.x`. Apply any per-release testnet patches as additional commits. Trigger the **Release** workflow as `testnet` → validates, builds, pushes `testnet-X.Y.Z` git tag, pushes image to `irys-testnet:X.Y.Z`, updates `testnet-latest` (both Docker and git), creates auto-published prerelease. Deploy to testnet.
4. After testnet validation, merge `release/<major>.x` forward into `deployment/mainnet/<major>.x`. Apply any per-release mainnet patches as additional commits. Trigger the **Release** workflow as `mainnet` on a commit whose `release/<major>.x` merge-base matches the testnet release's → validates, builds, pushes `mainnet-X.Y.Z` git tag, pushes image to `irys-mainnet:X.Y.Z`, updates `mainnet-latest`, creates **draft** release. A maintainer reviews the notes and publishes.

### Atomicity

The workflow builds the Docker image **before** pushing any git tags. If the build fails, nothing is published. If a later step fails after the git tag is pushed, cleanup steps automatically delete orphaned git tags and restore the env-prefixed head-tracking git tag (`testnet-latest` / `mainnet-latest`) to its previous position. Note that pushed Docker images are not automatically cleaned up on failure — use the **Docker Retag** workflow to manually restore the `latest` Docker tag within the affected image stream if needed.

### Head-Tracking Tags

Both git and Docker have head-tracking tags that are updated automatically:

| Release Type | Git Tag           | Docker Tag (within stream) |
|---|---|---|
| Testnet      | `testnet-latest`  | `latest` (on `irys-testnet`)  |
| Mainnet      | `mainnet-latest`  | `latest` (on `irys-mainnet`)  |

These are force-updated on every successful release. They can also be manually moved for rollback via the **Docker Retag** workflow.

## Hotfixes

Hotfixes follow the same flow — fix on `master`, cherry-pick to `release/<major>.x`, merge forward to the appropriate `deployment/<env>/<major>.x` branch, tag, deploy. In an emergency, a hotfix can be applied directly to a deployment branch and deployed to its environment immediately, bypassing the normal master → release → deployment progression (use the `force` flag to skip the testnet-merge-base validation).

## Rollback

To roll back testnet or mainnet to a previous version, use the **Docker Retag** workflow (`docker-retag.yml`):

1. Trigger the workflow with `environment` set to the env (`testnet` or `mainnet`), `source_tag` set to the Docker version to roll back to (e.g., `1.0.1` — the Docker tag inside the stream, not the env-prefixed git tag `testnet-1.0.1`), and `target_tag` set to `latest`.
2. The workflow re-tags the existing Docker image inside the env's image stream — no rebuild.
3. With `move_git_tag` enabled (default), the corresponding env-prefixed git tag (`testnet-latest` or `mainnet-latest`) is also moved to match.

This is the fastest rollback path — it re-uses the existing tested image.

## Process in Action

Starting from a new major version:

1. Branch `release/1.x` from `master`.
2. Cherry-pick the desired commits from `master` to `release/1.x`.
3. Bump version in `crates/chain/Cargo.toml` on `release/1.x`, commit.
4. Branch `deployment/testnet/1.x` from `release/1.x` (first time only). Apply any sticky testnet patches.
5. Trigger the release workflow as `testnet` targeting a commit on `deployment/testnet/1.x` → validates, builds, pushes `testnet-1.0.0` tag + image to `irys-testnet`, updates `testnet-latest`, auto-publishes prerelease. Deploy to testnet.
6. Validate on testnet. Fix issues via cherry-pick to `release/1.x` (if upstream) or direct commit to `deployment/testnet/1.x` (if env-specific), bump version on `release/1.x` and merge forward, tag `testnet-1.0.1`, repeat as needed. Earlier testnet versions are orphaned.
7. Once stable, branch `deployment/mainnet/1.x` from `release/1.x` (first time only). Apply any sticky mainnet patches. Merge `release/1.x` forward.
8. Trigger the release workflow as `mainnet` on a commit on `deployment/mainnet/1.x` whose `release/1.x` merge-base matches the testnet release's → validates, builds, pushes `mainnet-1.0.1` tag + image, updates `mainnet-latest`, creates draft release. Maintainer reviews and publishes. Deploy to mainnet.
9. Future minor/patch releases continue on `release/1.x` and flow through the same deployment branches. A new `release/2.x` branch plus new `deployment/testnet/2.x` and `deployment/mainnet/2.x` are created when a major version bump is needed.

## Authoring Deployment-Specific Patches

Deployment-specific patches (e.g., chain IDs, bootstrap peer addresses, env-specific hardcoded values) live as normal git commits on the relevant `deployment/<env>/<major>.x` branch. They are reviewed via PR into the deployment branch like any other change.

**Rules:**

- Version bumps in `crates/chain/Cargo.toml` happen **only on `release/<major>.x`**. Deployment branches inherit the version via merge-forward.
- If a `release/<major>.x` → deployment-branch merge produces a Cargo.toml version conflict, resolve to `release/<major>.x`'s value. The deployment branch must never carry its own version.
- Patches that should apply to multiple envs go on `release/<major>.x`, not on individual deployment branches.
- Patches that change protocol behavior should never be env-specific. Env patches are for env-bound values only.
- The testnet ↔ mainnet validation in the release workflow enforces that both commits share the same `release/<major>.x` merge-base — i.e. same upstream code, only env-patches differ.
