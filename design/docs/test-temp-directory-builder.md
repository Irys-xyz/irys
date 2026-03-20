# Test Temporary Directory Builder (`TempDirBuilder`)

## Status
Accepted

## Context

Test temp directory creation was inconsistent across the workspace. Some tests called `tempfile::tempdir()` (which uses the OS temp dir, outside the project), others used the free functions `temporary_directory()` or `setup_tracing_and_temp_dir()` from `irys-testing-utils`. These free functions coupled tracing initialization with directory creation, couldn't be customized (prefix, cleanup behavior), and used a non-standard base path computation.

The result was:
- Test artifacts scattered across `/tmp` and `.tmp/`, making cleanup and debugging harder
- No way to keep a test directory on disk for post-mortem debugging without modifying the test code
- Tracing initialization was forced on callers who only needed a directory
- The `IRYS_CUSTOM_TMP_DIR` env var was only respected by some code paths

## Decision

Replace all temp directory creation in tests with a `TempDirBuilder` in `irys-testing-utils`:

```rust
let dir = TempDirBuilder::new().build();                    // defaults
let dir = TempDirBuilder::new().prefix("my-test-").build(); // custom prefix
let dir = TempDirBuilder::new().keep().build();              // keep for debugging
let dir = TempDirBuilder::new().with_tracing().build();      // init tracing first
```

Key design choices:

- **Builder pattern** over a function with boolean flags, because the number of orthogonal options (prefix, keep, tracing) would make a function signature hard to read and extend.
- **All directories go through `.tmp/`** via `tmp_base_dir()`, which respects `IRYS_CUSTOM_TMP_DIR`. This is the same base path the core pinning logic uses to detect test environments, so the heuristic remains correct.
- **`keep()` uses `tempfile::Builder::disable_cleanup(true)`** rather than a custom `Drop` implementation, delegating lifetime management to the well-tested `tempfile` crate.
- **Tracing is opt-in** via `.with_tracing()`, not coupled to directory creation. Tests that don't need tracing output avoid the global subscriber initialization overhead.

The old free functions (`temporary_directory`, `setup_tracing_and_temp_dir`) were deprecated and then removed. All ~90 call sites across `crates/actors`, `crates/database`, `crates/domain`, `crates/p2p`, and `crates/chain-tests` were migrated in crate-by-crate commits.

## Consequences

- All test artifacts land in `.tmp/` (or the `IRYS_CUSTOM_TMP_DIR` override), never in `/tmp`
- Developers can keep failing test directories for debugging without code changes: `.keep().build()`
- New options (e.g., size limits, custom cleanup hooks) can be added to the builder without breaking existing call sites
- The migration touched ~30 files (~90 call sites) but was mechanical — each commit migrated one crate for easy review
- `tempfile::TempDir`'s auto-delete semantics are preserved by default; `.keep()` is explicit opt-out

## Source
PR #1223 — feat: check database schema version on startup
