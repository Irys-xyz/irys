# Irys Release Process

## Environments

| Environment | Deploys From | Tags Required |
|---|---|---|
| **Devnet** | `deployment/devnet` branch | No |
| **Testnet** | `release/<major>.x` | `rc-X.Y.Z` |
| **Mainnet** | `release/<major>.x` | `X.Y.Z` |

## Branches

- **`master`** — integration branch. All feature work merges here via PR. Not deployed to testnet or mainnet.
- **`release/<major>.x`** — long-lived release branches (e.g. `release/1.x`). Source of truth for testnet/mainnet. Created from `master`; receives cherry-picks from `master`. Version bumps happen on these branches directly.
- **Feature branches** — short-lived, merge into `master` via PR.

## Versioning

**Semantic Versioning** (`MAJOR.MINOR.PATCH`):

- **MAJOR** — breaking protocol changes, consensus-incompatible upgrades, node-software-specific changes
- **MINOR** — new features, non-breaking protocol changes
- **PATCH** — bug fixes, performance improvements, configuration changes

### Tags

- `rc-X.Y.Z` — release candidate, deployed to testnet. Auto-published as a GitHub prerelease.
- `X.Y.Z` — production release, deployed to mainnet. Created as a draft GitHub Release — a maintainer reviews and publishes.
- `rc-latest` — head-tracking tag, always points to the most recent RC. Updated automatically.
- `latest` — head-tracking tag, always points to the most recent production release. Updated automatically.

The canonical version source is the `irys-chain` crate (`crates/chain/Cargo.toml`). The workflow validates that this version matches the release version input.

### RC Version Semantics

When iterating release candidates, each fix gets a new version: `rc-1.0.0` → fix → `rc-1.0.1` → fix → `rc-1.0.2`. The production release version matches the final RC — if the last RC is `rc-1.0.2`, the production tag is `1.0.2`. Earlier RC versions (`rc-1.0.0`, `rc-1.0.1`) are orphaned. This is expected.

## Release Flow

```
master ──► deployment/devnet ──► devnet (on demand)
  │
  ▼ (cherry-pick / merge)
release/<major>.x ──rc-X.Y.Z──► testnet (auto-published prerelease)
                        │
                        ▼ (same commit)
                     X.Y.Z──► mainnet (draft release → manual publish)
```

1. Work merges into `master`. Devnet deploys from the `deployment/devnet` branch, which is updated from `master` on demand.
2. When ready to release, cherry-pick changes from `master` into `release/<major>.x`, bump the version in `crates/chain/Cargo.toml`, and stabilize.
3. Trigger the **Release** workflow as `rc` → validates, builds Docker image, then pushes `rc-X.Y.Z` tag, pushes image, updates `rc-latest`, creates auto-published prerelease. Deploy to testnet.
4. After testnet validation, trigger the **Release** workflow as `release` on the same commit → validates RC tag exists, builds Docker image, then pushes `X.Y.Z` tag, pushes image tagged as `latest`, creates **draft** release. A maintainer reviews the notes and publishes.

### Atomicity

The workflow builds the Docker image **before** pushing any git tags. If the build fails, nothing is published. If a later step fails after the git tag is pushed, cleanup steps automatically delete orphaned git tags and restore head-tracking git tags to their previous position. Note that pushed Docker images are not automatically cleaned up on failure — use the **Docker Retag** workflow to manually restore head-tracking Docker tags if needed.

### Head-Tracking Tags

Both git and Docker have head-tracking tags that are updated automatically:

| Release Type | Git Tag | Docker Tag |
|---|---|---|
| RC | `rc-latest` | `rc-latest` |
| Release | `latest` | `latest` |

These are force-updated on every successful release. They can also be manually moved for rollback via the **Docker Retag** workflow.

## Hotfixes

Hotfixes follow the same flow — fix on `master`, cherry-pick to `release/<major>.x`, tag, deploy. In an emergency, a hotfix can be applied directly to the release branch and deployed to any environment immediately, bypassing the normal devnet → testnet → mainnet progression (use the `force` flag to skip RC validation).

## Rollback

To roll back testnet or mainnet to a previous version, use the **Docker Retag** workflow (`docker-retag.yml`):

1. Trigger the workflow with `source_tag` set to the version to roll back to (e.g., `1.0.1`) and `target_tag` set to the head-tracking tag (`latest` for mainnet, `rc-latest` for testnet).
2. The workflow re-tags the existing Docker image — no rebuild.
3. With `move_git_tag` enabled (default), the corresponding git tag is also moved to match.

This is the fastest rollback path — it re-uses the existing tested image.

## Process in Action

Starting from a new major version:

1. Branch `release/1.x` from `master`.
2. Cherry-pick the desired commits from `master`.
3. Bump version in `crates/chain/Cargo.toml` on the release branch, commit.
4. Trigger the release action as `rc` targeting a commit on `release/1.x` → validates, builds, pushes `rc-1.0.0` tag + image, updates `rc-latest`, auto-publishes prerelease. Deploy to testnet.
5. Validate on testnet. Fix issues via cherry-pick, bump version, tag `rc-1.0.1`, repeat as needed. Earlier RC versions are orphaned.
6. Once stable, trigger the release action as `release` on the same commit → validates `rc-1.0.1` exists, builds, pushes `1.0.1` tag + image, updates `latest`, creates draft release. Maintainer reviews and publishes. Deploy to mainnet.
7. Future minor/patch releases continue on `release/1.x`. A new `release/2.x` branch is created when a major version bump is needed.
