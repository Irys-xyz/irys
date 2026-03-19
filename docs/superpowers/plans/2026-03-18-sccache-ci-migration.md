# sccache CI Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `Swatinem/rust-cache` with sccache using a shared Docker volume, so all 4 self-hosted CI runners share compilation cache.

**Architecture:** sccache is installed in the runner Docker image. Each workflow that compiles Rust sets `RUSTC_WRAPPER=sccache` and points `SCCACHE_DIR` at a shared volume mounted at `/shared`. The old `rust-cache` lines are commented out for easy rollback.

**Tech Stack:** sccache 0.14.0, GitHub Actions workflows (YAML), Docker

**Spec:** `docs/superpowers/specs/2026-03-18-sccache-ci-migration-design.md`

---

## File Map

| Action | File | Responsibility |
|--------|------|----------------|
| Modify | `/workspaces/Irys-CI/runner/Dockerfile` | Install sccache binary, pre-create cache dir |
| Modify | `.github/actions/setup-repo/action.yaml` | Comment out rust-cache |
| Modify | `.github/workflows/rust.yml` | Add sccache env vars + stats steps |
| Modify | `.github/workflows/bench.yml` | Add sccache env vars + stats step |
| Modify | `.github/workflows/flaky.yml` | Add sccache env vars + stats step |

---

### Task 1: Install sccache in runner Dockerfile

**Files:**
- Modify: `/workspaces/Irys-CI/runner/Dockerfile:66-71` (insert before `COPY start.sh` on line 72)

- [ ] **Step 1: Add sccache installation and cache directory**

Insert the following block in `/workspaces/Irys-CI/runner/Dockerfile` **after** the CUDA environment variables block (line 68, `ENV LD_LIBRARY_PATH=...`) and **before** `COPY start.sh` (line 72):

```dockerfile
# sccache - shared compilation cache
ARG SCCACHE_VERSION="0.14.0"
RUN curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar xz --strip-components=1 -C /usr/local/bin/ \
      "sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache" \
    && chmod +x /usr/local/bin/sccache

# Pre-create the sccache directory for the shared volume mount
RUN mkdir -p /shared/sccache && chown runner:runner /shared/sccache
```

The result around lines 68-80 should look like:

```dockerfile
ENV LD_LIBRARY_PATH="${CUDA_HOME}/lib64"

# RUN nvcc --version

# sccache - shared compilation cache
ARG SCCACHE_VERSION="0.14.0"
RUN curl -fsSL "https://github.com/mozilla/sccache/releases/download/v${SCCACHE_VERSION}/sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl.tar.gz" \
    | tar xz --strip-components=1 -C /usr/local/bin/ \
      "sccache-v${SCCACHE_VERSION}-x86_64-unknown-linux-musl/sccache" \
    && chmod +x /usr/local/bin/sccache

# Pre-create the sccache directory for the shared volume mount
RUN mkdir -p /shared/sccache && chown runner:runner /shared/sccache

COPY start.sh start.sh
```

- [ ] **Step 2: Verify Dockerfile syntax**

Run: `docker build --check /workspaces/Irys-CI/runner/` (if available), or visually confirm no syntax errors.

- [ ] **Step 3: Commit**

```bash
cd /workspaces/Irys-CI
git add runner/Dockerfile
git commit -m "feat(ci): install sccache in runner image

Add sccache 0.14.0 binary and pre-create /shared/sccache directory
for shared compilation caching across runners."
```

---

### Task 2: Comment out rust-cache in setup-repo action

**Files:**
- Modify: `.github/actions/setup-repo/action.yaml:13-16`

- [ ] **Step 1: Comment out the rust-cache step**

Replace lines 13-16 in `.github/actions/setup-repo/action.yaml`:

```yaml
    - uses: Swatinem/rust-cache@v2
      with:
        cache-on-failure: true
        cache-all-crates: true
```

With:

```yaml
    # Disabled in favor of sccache (shared across runners via /shared/sccache volume).
    # Uncomment to revert to per-runner target dir caching.
    # - uses: Swatinem/rust-cache@v2
    #   with:
    #     cache-on-failure: true
    #     cache-all-crates: true
```

- [ ] **Step 2: Validate YAML syntax**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/actions/setup-repo/action.yaml'))"`
Expected: No output (clean parse)

- [ ] **Step 3: Commit**

```bash
git add .github/actions/setup-repo/action.yaml
git commit -m "feat(ci): disable rust-cache in favor of sccache

Comment out Swatinem/rust-cache@v2 instead of removing it, so
reverting is a one-line uncomment."
```

---

### Task 3: Add sccache env vars and stats to rust.yml

**Files:**
- Modify: `.github/workflows/rust.yml:16-20` (env block)
- Modify: `.github/workflows/rust.yml` (add stats step to 5 jobs)

- [ ] **Step 1: Add sccache env vars to the top-level env block**

In `.github/workflows/rust.yml`, change the `env:` block (lines 16-20) from:

```yaml
env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1
  IRYS_CUSTOM_TMP_DIR: RUNNER_TEMP
  CARGO_INCREMENTAL: 0
```

To:

```yaml
env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1
  IRYS_CUSTOM_TMP_DIR: RUNNER_TEMP
  CARGO_INCREMENTAL: 0
  RUSTC_WRAPPER: sccache
  SCCACHE_DIR: /shared/sccache
  SCCACHE_CACHE_SIZE: 40G
```

- [ ] **Step 2: Add sccache stats step to cargo-test job**

In the `cargo-test` job, add after the `cargo test` step (after line 162):

```yaml
      - name: sccache stats
        if: always()
        run: sccache --show-stats
```

- [ ] **Step 3: Add sccache stats step to cargo-check job**

In the `cargo-check` job, add after the `cargo check` step (after line 182):

```yaml
      - name: sccache stats
        if: always()
        run: sccache --show-stats
```

- [ ] **Step 4: Add sccache stats step to cargo-clippy job**

In the `cargo-clippy` job, add after the `cargo clippy` step (after line 216):

```yaml
      - name: sccache stats
        if: always()
        run: sccache --show-stats
```

- [ ] **Step 5: Add sccache stats step to cargo-doc job**

In the `cargo-doc` job, add after the `cargo test docs` step (after line 236):

```yaml
      - name: sccache stats
        if: always()
        run: sccache --show-stats
```

- [ ] **Step 6: Add sccache stats step to cargo-unused-deps job**

In the `cargo-unused-deps` job, add after the `cargo unused deps` step (after line 253):

```yaml
      - name: sccache stats
        if: always()
        run: sccache --show-stats
```

- [ ] **Step 7: Validate YAML syntax**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/rust.yml'))"`
Expected: No output (clean parse)

- [ ] **Step 8: Commit**

```bash
git add .github/workflows/rust.yml
git commit -m "feat(ci): add sccache to Rust Checks workflow

Add RUSTC_WRAPPER, SCCACHE_DIR, and SCCACHE_CACHE_SIZE env vars.
Add sccache stats step to all 5 compilation jobs for observability."
```

---

### Task 4: Add sccache env vars and stats to bench.yml

**Files:**
- Modify: `.github/workflows/bench.yml:19-23` (env block)
- Modify: `.github/workflows/bench.yml` (add stats step to cargo-bench job)

- [ ] **Step 1: Add sccache env vars to the top-level env block**

In `.github/workflows/bench.yml`, change the `env:` block (lines 19-23) from:

```yaml
env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1
  IRYS_CUSTOM_TMP_DIR: RUNNER_TEMP
  CARGO_INCREMENTAL: 0
```

To:

```yaml
env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1
  IRYS_CUSTOM_TMP_DIR: RUNNER_TEMP
  CARGO_INCREMENTAL: 0
  RUSTC_WRAPPER: sccache
  SCCACHE_DIR: /shared/sccache
  SCCACHE_CACHE_SIZE: 40G
```

- [ ] **Step 2: Add sccache stats step to cargo-bench job**

In the `cargo-bench` job, add as the **final step** (after the `Comment benchmark results URL on PR` step, after line 247):

```yaml
      - name: sccache stats
        if: always()
        run: sccache --show-stats
```

- [ ] **Step 3: Validate YAML syntax**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/bench.yml'))"`
Expected: No output (clean parse)

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/bench.yml
git commit -m "feat(ci): add sccache to Benchmarks workflow

Add RUSTC_WRAPPER, SCCACHE_DIR, and SCCACHE_CACHE_SIZE env vars.
Add sccache stats step to cargo-bench job."
```

---

### Task 5: Add sccache env vars and stats to flaky.yml

**Files:**
- Modify: `.github/workflows/flaky.yml:29-37` (env block)
- Modify: `.github/workflows/flaky.yml` (add stats step to flaky-tests job)

- [ ] **Step 1: Add sccache env vars to the top-level env block**

In `.github/workflows/flaky.yml`, change the `env:` block (lines 29-37) from:

```yaml
env:
  ITERATIONS: ${{ github.event.inputs.iterations || '3' }}
  TOLERABLE_FAILURES: ${{ github.event.inputs.tolerable_failures || '5' }}
  RETENTION_DAYS: 10
  CARGO_TERM_COLOR: always
  CARGO_INCREMENTAL: 0
  RUST_BACKTRACE: short
  # keep build artifacts contained to this repo's workspace on the self-hosted runner
  CARGO_TARGET_DIR: ${{ github.workspace }}/.target
```

To:

```yaml
env:
  ITERATIONS: ${{ github.event.inputs.iterations || '3' }}
  TOLERABLE_FAILURES: ${{ github.event.inputs.tolerable_failures || '5' }}
  RETENTION_DAYS: 10
  CARGO_TERM_COLOR: always
  CARGO_INCREMENTAL: 0
  RUST_BACKTRACE: short
  # keep build artifacts contained to this repo's workspace on the self-hosted runner
  CARGO_TARGET_DIR: ${{ github.workspace }}/.target
  RUSTC_WRAPPER: sccache
  SCCACHE_DIR: /shared/sccache
  SCCACHE_CACHE_SIZE: 40G
```

- [ ] **Step 2: Add sccache stats step to flaky-tests job**

In the `flaky-tests` job, add as the **final step** (after the `Post to job summary` step, after line 169):

```yaml
      - name: sccache stats
        if: always()
        run: sccache --show-stats
```

- [ ] **Step 3: Validate YAML syntax**

Run: `python3 -c "import yaml; yaml.safe_load(open('.github/workflows/flaky.yml'))"`
Expected: No output (clean parse)

- [ ] **Step 4: Commit**

```bash
git add .github/workflows/flaky.yml
git commit -m "feat(ci): add sccache to Flaky Test Detection workflow

Add RUSTC_WRAPPER, SCCACHE_DIR, and SCCACHE_CACHE_SIZE env vars.
Add sccache stats steps for observability."
```

---

### Task 6: Final validation

- [ ] **Step 1: Verify all workflow files parse cleanly**

```bash
python3 -c "
import yaml, sys
files = [
    '.github/actions/setup-repo/action.yaml',
    '.github/workflows/rust.yml',
    '.github/workflows/bench.yml',
    '.github/workflows/flaky.yml',
]
for f in files:
    try:
        yaml.safe_load(open(f))
        print(f'OK: {f}')
    except Exception as e:
        print(f'FAIL: {f}: {e}')
        sys.exit(1)
print('All files valid.')
"
```

Expected: All files print `OK`, final line `All files valid.`

- [ ] **Step 2: Verify no Swatinem/rust-cache active references remain**

```bash
grep -r "Swatinem/rust-cache" .github/ --include="*.yml" --include="*.yaml" | grep -v "^.*#"
```

Expected: No output (all rust-cache references should be in comments only)

- [ ] **Step 3: Verify sccache env vars are present in all three workflows**

```bash
grep -l "RUSTC_WRAPPER: sccache" .github/workflows/rust.yml .github/workflows/bench.yml .github/workflows/flaky.yml
```

Expected: All three files listed

- [ ] **Step 4: Review git log**

```bash
git log --oneline worktree-sccache-ci-migration --not master
```

Confirm all commits are present and messages are clear.
