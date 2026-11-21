# Data-tree geometry and depth proofs

Target: `Geometry::chunk_count`, `chunk_len`, and `segment_len` in
`crates/data-tree/src/geometry.rs`. Spec §2 defines

```text
C                    = ceil(n / chunk_size)
chunk_len(n, c)      = min(chunk_size, n − c × chunk_size)    for c < C
segment_len(n, c, j) = 0                                      if seg_start ≥ n
                     = min(segment_size, n − seg_start)       otherwise
```

and says an unsigned `n − seg_start` wraps when the segment starts past the
data. The production code uses `checked_*` and `expect` for that. Its tests
sample the inputs and stop a full chunk walk at 64 chunks. Mainnet allows
20,971,520 chunks.

The emission curve (`crates/reward-curve`) moves tokens, but it is `U256`
arithmetic inside `irys-types` (reth, alloy). Kani cannot load that crate.
This arithmetic is plain integers, and a wrong length is a wrong data
commitment.

## What Kani proves

The proofs call the production methods. For `Geometry::TESTING` and
`Geometry::MAINNET`, every `data_size` and every index either matches the
spec or returns `Err`. No panic. Segment lengths inside one chunk sum to
`chunk_len` (4 segments in testing, 64 on mainnet).

`depth` (spec §5) is the number of chunk-level siblings in a proof. For every
`u32` index and count it is `None` outside `c < C`, `log2(C)` when `C` is a
power of two, and otherwise the spec's split on the greatest power of two
strictly below `C`.

## Run

Install once (about 10 minutes):

```sh
cargo install --locked kani-verifier && cargo kani setup
```

Then, about 10 minutes when the build cache is warm:

```sh
formal/verify.sh
```

That runs every `formal/<suite>/verify.sh`. This suite is the only one today.
Run it alone with `formal/data-tree-geometry/verify.sh`.

The scripts do not install Kani. Run them from anywhere; each finds its own
directory. `formal/` is excluded from the Irys workspace, and this crate is
its own workspace, so do not run `cargo kani` from the repository root.

Checked: 9 harnesses, 0 failures, in 9 minutes 54 seconds (warm cache,
2026-11-06). Eight cover `Geometry::TESTING` and `Geometry::MAINNET`.
The ninth is `depth`.
