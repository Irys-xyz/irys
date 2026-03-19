# sccache CI Migration Design

## Problem

The 4 self-hosted CI runner containers (2x `misc-runner`, 2x `test-runner`) each rebuild Rust compilation artifacts independently. `Swatinem/rust-cache` caches the `target/` directory per-job but doesn't share artifacts across runners. This leads to redundant compilation across concurrent and sequential CI runs.

## Solution

Replace `rust-cache` with [sccache](https://github.com/mozilla/sccache) using a shared Docker volume as the cache backend. sccache caches individual compilation units (not the whole target dir), and the shared volume makes cached artifacts available to all runners immediately.

## Approach

**Approach A: sccache with shared volume, env vars in workflows.** sccache is baked into the runner Docker image. Each workflow that compiles Rust sets the sccache env vars explicitly. `rust-cache` is removed from the `setup-repo` composite action. The `falcondev-oss` GitHub Actions cache server is kept for non-Rust caching.

## Changes

### 1. Runner Image (`Irys-CI/runner/Dockerfile`)

Install sccache from official GitHub releases, pinned to a specific version:

Insert after the CUDA installation block, before `COPY start.sh`:

```dockerfile
ARG SCCACHE_VERSION="0.14.0"
RUN curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar xz --strip-components=1 -C /usr/local/bin/ \
      "sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache" \
    && chmod +x /usr/local/bin/sccache
```

Pre-create the sccache directory with correct ownership (runner runs as uid 1001):

```dockerfile
RUN mkdir -p /shared/sccache && chown runner:runner /shared/sccache
```

No `ENV RUSTC_WRAPPER` in the image — env vars are set per-workflow.

### 2. `setup-repo` Action (`.github/actions/setup-repo/action.yaml`)

Comment out the `Swatinem/rust-cache@v2` step (keep it for easy rollback). The action becomes:

```yaml
name: "Setup Repo Action"
description: "A reusable composite action that setups rust and other common tasks"

runs:
  using: "composite"
  steps:
    - name: Install rust (respect toolchain file)
      uses: actions-rust-lang/setup-rust-toolchain@v1
      with:
        components: rustfmt,clippy,cargo,llvm-tools-preview
        cache: false

    # Disabled in favor of sccache (shared across runners via /shared/sccache volume).
    # Uncomment to revert to per-runner target dir caching.
    # - uses: Swatinem/rust-cache@v2
    #   with:
    #     cache-on-failure: true
    #     cache-all-crates: true
```

### 3. Workflow Env Vars

Each affected workflow gets these added to its top-level `env:` block:

```yaml
env:
  RUSTC_WRAPPER: sccache
  SCCACHE_DIR: /shared/sccache
  SCCACHE_CACHE_SIZE: 40G
```

### 4. sccache Stats Step

Each compilation job gets a final step for observability:

```yaml
- name: sccache stats
  if: always()
  run: sccache --show-stats
```

## Affected Files

### `Irys-CI` repo (runner infrastructure)
- `runner/Dockerfile` — add sccache binary installation

### `irys-rs` repo (this repo)
- `.github/actions/setup-repo/action.yaml` — comment out `Swatinem/rust-cache@v2` (kept for easy rollback)
- `.github/workflows/rust.yml` — add env vars, add stats step to 5 jobs (test, check, clippy, doc, unused-deps)
- `.github/workflows/bench.yml` — add env vars, add stats step to bench job
- `.github/workflows/flaky.yml` — add env vars, add stats step to flaky-tests job

### Not affected
- `.github/workflows/docker.yml` — Docker image build, no direct cargo compilation
- `.github/workflows/conventional-pr.yaml` — no compilation (runs on `ubuntu-latest`, not self-hosted)

### Note on sccache scope

sccache only intercepts `rustc` invocations via `RUSTC_WRAPPER`. The following tools are **not cached** by sccache:
- `cargo fmt` — invokes `rustfmt`, not `rustc`
- `cargo-machete` (unused-deps) — static analysis of `Cargo.toml`, no compilation
- `cargo test --doc` — doc-tests are compiled by `rustdoc`, not `rustc`

Since env vars are set at workflow level, non-compilation jobs (gate, typos, fmt) will inherit `RUSTC_WRAPPER=sccache` but this is harmless — sccache passes through to `rustc` and only caches when actually invoked as a compiler wrapper.

## Concurrency & File Locking

sccache's local disk backend uses file-level locking. Multiple runners writing to `/shared/sccache` concurrently is safe:

- Multiple runners reading cached artifacts simultaneously — no contention
- Multiple runners writing different compilation units — sccache locks per-entry
- Cache eviction under `SCCACHE_CACHE_SIZE` — handled automatically by LRU

If two runners compile the exact same crate at the exact same time, one wins and the other compiles from scratch. This is a benign race — no corruption, just a missed cache opportunity on that one unit. Becomes irrelevant once the cache is warm.

## Deployment Order

1. **Build and deploy new runner image** (Irys-CI) — sccache must be installed before workflows reference it
2. **Merge irys-rs workflow changes** — once runners have sccache available

## Rollback

If sccache causes issues after deployment:

1. **Quick disable:** Set `RUSTC_WRAPPER: ""` in the workflow env block — disables sccache without reverting anything else
2. **Full revert:** Uncomment `Swatinem/rust-cache@v2` in `setup-repo/action.yaml`, remove sccache env vars from workflows

The sccache binary in the runner image is inert when `RUSTC_WRAPPER` is unset, so no runner image rebuild is needed for rollback.

## Operational Notes

- **Cold start:** The first CI run after migration will have zero cache hits and ~2-5% overhead from sccache cache-miss lookups. Subsequent runs will see cache hits.
- **Toolchain upgrades:** When `rust-toolchain.toml` changes the compiler version, all cached artifacts become stale. sccache handles this automatically (compiler binary is part of the cache key), but 40GB of stale artifacts will be evicted gradually via LRU. For an immediate cleanup, run `rm -rf /shared/sccache` on the host.
- **Permissions:** The `/shared/sccache` directory is pre-created in the Dockerfile with `runner:runner` ownership. The Docker named volume root is typically permissive, but pre-creation avoids any first-run permission issues.

## Design Decisions

- **Shared volume over Redis/MinIO/memcached** — simplest option, the `shared_data` volume already exists and is mounted on all runners at `/shared`
- **Env vars in workflows, not baked into image** — gives per-workflow control, avoids surprises with ad-hoc cargo usage on runners
- **Keep `falcondev-oss` cache server** — it serves the general GitHub Actions cache API, which may be used by non-Rust workflows or actions
- **40GB cache size** — accounts for 29 workspace crates plus the full dependency tree
- **`CARGO_INCREMENTAL=0` already set** — incremental compilation and sccache don't mix well, so no change needed
