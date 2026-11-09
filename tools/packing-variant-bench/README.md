# packing-variant-bench

A standalone benchmark of Irys entropy packing on CPU and NVIDIA GPU (CUDA). It compares two
algorithm variants and, for each, production code against optimised implementations, and measures
production GPU packing end to end (`--mode prod`).
This is its own Cargo workspace and is **not** a member of the main Irys workspace. It uses
`irys-c`, `irys-types` and (with `--features cuda`) `irys-packing` through path dependencies, and
`Cargo.lock` started as a copy of the root lockfile so dependency versions match.

The optimised GPU kernel (`fast-interleaved` A) is now production code:
`crates/c/c_src/entropy_cuda.cuh`, driven by `irys_packing::CudaPacker` / `CudaPackerPool`. The
`ref` kernel here is the production kernel from before that port, kept for comparison.

## Production GPU packing, end to end

`--mode prod`: `CudaPackerPool` with `--prod-concurrency` batches in flight, every chunk copied out
of its slab once (standing in for the entropy writer). RTX 5090, 575 W. Before = master's
`capacity_pack_range_cuda_c` with a fresh `Vec` per batch, as the packing service used it.

| chunks/s | devnet / testnet (1000 iterations) | mainnet (1M iterations) |
|---|---|---|
| before, `gpu_packing_batch_size = 1024` (template default) | 6 107 | 83 |
| before, batch 21 760 (one thread per CUDA core) | 9 702 | 951 |
| after, `auto` (87 040), 1 batch in flight | 96 330 | 11 635 |
| after, `auto` (43 520 each), 2 in flight | **106 455–109 298** | **12 170–12 185** |

At 1000 iterations the kernel packs 21 760 chunks in ~7 ms; the end-to-end rate (~26 GiB/s of
entropy) is bound by the copy to the host and host memory, not the GPU. A real node is bound by its
disks long before either.

## Headline results

Ryzen 9 9950X3D (16 cores / 32 threads) + RTX 5090 (stock 575 W limit), CUDA 13.4, WSL2.
1 000 000 iterations per 256 KiB chunk, which is `ConsensusConfig::mainnet()`.
All GPU numbers are verified against the CPU implementation.

**Throughput** (chunks/s, whole machine; CPU at 32 threads):

| | variant A (current) | variant B (candidate) | B vs A |
|---|---|---|---|
| CPU, production code (`irys-c`) | 294 | — | |
| CPU, OpenSSL (same SHA backend as production) | 307 | 303 | −1% |
| CPU, AVX2 ×8 lanes (true AVX2 build; for CPUs without SHA-NI) | 382 | 371 | −3% |
| CPU, SHA-NI ×4 interleaved | 631 | 623 | −1% |
| CPU, AVX-512 ×16 lanes (optional path) | 1 144–1 257 | 856–933 | −25% |
| GPU, production kernel (`ref`) | 1 147 | 1 125 | −2% |
| GPU, optimised (`fast-interleaved`) | **13 784** | **11 655** | **−15%** |

* GPU optimised vs production kernel: **12.0×**. At production's exact launch shape
  (21 760 chunks, 170 × 128: one thread per CUDA core) it is 11 276 vs 1 075, which is 10.5×.
* CPU optimised vs production: **2.1×** (SHA-NI) and **3.9–4.3×** (AVX-512).
* GPU / CPU throughput: 3.9× with production code on both sides. With both optimised it is
  **22×** against SHA-NI and **11–12×** against AVX-512 for A; for B it is 18.7× and 12.5–13.6×.
* AVX-512 ranges are two runs with the corrected runner (1 s warm-up, ≥ 8 batches per thread).

**Latency of one chunk:**

| | variant A | variant B |
|---|---|---|
| CPU, production (`irys-c`), one thread | 0.080 s | — |
| CPU, OpenSSL, one thread | 0.079 s | 0.081 s |
| CPU, SHA-NI, one chain (fastest single-chunk latency) | **0.049 s** | **0.052 s** |
| GPU, any kernel: one chunk occupies one GPU thread for the whole run | ~1.7 s at best (optimised), ~12 s (production) | similar |

Each chunk is a strictly serial hash chain in both variants, so latency per chunk cannot be improved
by parallel hardware. Interleaving (SHA-NI ×N, AVX-512, GPU) only raises throughput across
*independent* chunks; it makes each chunk's latency worse (e.g. 16 AVX-512 chunks finish together
after ~0.26 s). For a latency-based security argument, use the single-chain CPU figure, not the
GPU's. Pinning to a specific CCD made no measurable difference under WSL2 (Hyper-V places vCPUs).
The GPU single-chunk figures are extrapolated from 50k-iteration runs on small batches.

## Variants

Each chunk is 8192 × 32-byte segments; SHA = SHA-256. `iterations` is the total hash count per chunk
(Phase 1 + Phase 2), as in `compute_entropy_chunk`.

| | Variant A (current) | Variant B (candidate) |
|---|---|---|
| seed | `SHA(addr ‖ partition_hash ‖ chain_id LE ‖ offset LE)` | same |
| Phase 1 | `prev = SHA(prev); buf[i] = prev` for 8192 segments | same |
| Phase 2 step | `buf[i] = SHA(buf[i-1] ‖ buf[i])` | `j = LE_u32(prev[0..4]) & 8191; buf[i] = SHA(prev ‖ buf[j]); prev = buf[i]` |

Both make one SHA-256 call over 64 bytes per step, and writes go round the ring in order. In B the
read address `j` is known only after the previous hash, so it cannot be prefetched.

## Implementations

### CPU (`--cpu-impl`)

| name | what | needs |
|---|---|---|
| `openssl` | `src/lib.rs`, `openssl::sha` per hash (same backend as production) | any CPU |
| `irys-c` | unmodified `irys_c::capacity_single::compute_entropy_chunk` (A only) | any CPU |
| `sha-ni-x1/x2/x4/x8` | `src/cpu_fast.rs`: hand-written SHA-NI compression, N chunks interleaved per thread | SHA-NI |
| `avx512-x16/x32` | `src/cpu_avx512.rs`: software SHA-256 across 16 (or 2 × 16) chunks in AVX-512 lanes | AVX-512F |
| `avx2-x8/x16` | `src/cpu_avx2.rs`: software SHA-256 across 8 (or 2 × 8) chunks in AVX2 lanes | AVX2 |

The dispatchable list lives in `src/cpu.rs`. SHA-NI, AVX-512 and AVX2 are detected at runtime
(`is_x86_feature_detected!`). Unsupported paths are skipped, never timed via a fallback. Nothing
requires any of them. Outputs are byte-identical to the reference (tests below).
`PVB_CPU_DISABLE=sha-ni,avx512,avx2` (any subset) hides extensions to emulate other CPUs; also set
`OPENSSL_ia32cap="~0x0:~0x20000000"` to hide SHA-NI from OpenSSL. An empty `OPENSSL_ia32cap` zeroes
OpenSSL's whole capability vector and disables all its assembly.

**AVX2 needs a generic build to emulate faithfully.** With the repo's `-C target-cpu=native` on a
CPU with AVX-512VL, LLVM compiles the AVX2 functions with AVX-512 rotate/ternary-logic instructions
(~1.5× faster than true AVX2). Build with
`--config 'build.rustflags=["-C","target-cpu=x86-64-v3"]'` (no `RUSTFLAGS`) when emulating an
AVX2-only CPU. On a real AVX2-only machine a native build is already pure AVX2. True AVX2 is ~3.4×
OpenSSL-without-SHA-NI, but only ~0.6× SHA-NI, so it only matters on CPUs without SHA-NI. Skylake-SP /
Cascade Lake Xeons lack SHA-NI but have AVX-512F, so they would run `avx512` (measure their clocks).

Why these are faster than OpenSSL, which does use SHA-NI (masking SHA-NI off makes it 2.06× slower):
* No per-call overhead. State stays in registers, and the second block of each 64-byte message is
  constant padding, so its message schedule + K is a compile-time constant.
* Interleaving. One chain waits on each SHA instruction's latency (~4 cycles per `sha256rnds2`);
  4–8 chains keep the core's single SHA unit busy (~2 cycles each, its issue limit). Beyond that the
  core is saturated. SMT siblings share the SHA unit, so 16 → 32 threads adds only 1.25× for SHA-NI
  ×4, against 1.65× for one chain per thread.
* AVX-512 avoids the SHA unit entirely: 16 chains per instruction stream on the vector ALUs. B pays
  more here: each step gathers 16 random segments, and 32 threads × 4 MiB of scratch exceeds L3.

**OpenSSL 3's one-shot `sha256()` does not scale across threads.** It is
`EVP_Q_digest(NULL, "SHA256", ..)`, which looks the digest up by name in the shared provider store
on every call. Production's `capacity_single::compute_entropy_chunk` used it for Phase 1 (8192 calls
per chunk): at 1000 iterations (testnet / devnet / tests, all Phase 1) CPU packing ran ~30× slower
on 32 threads and ~3.5× slower on one. It cost ~4% at mainnet's 1M. Fixed in production by
`fix/packing-sha256-context` (reusable `Sha256` context). This crate's `openssl` implementation uses
the context; `irys-c` is whatever the referenced `crates/c` contains.

How `all-threads` is measured (`cpu::throughput`), and why:
* A shared work queue: threads pull batches from an atomic counter. A fixed share per thread made the
  wall time that of the slowest thread; under a hypervisor one briefly preempted vCPU skewed results
  by 15–25%.
* At least 8 batches per thread. With 2 (e.g. AVX-512 ×16 at 32 chunks per thread) the queue cannot
  balance: 1 015 vs 1 150 chunks/s.
* 1 s of warm-up with the same implementation before timing. All-core boost does not last: AVX-512 on
  32 threads runs ~25% faster for the first ~0.5 s than sustained.
* Oversubscribing (64 threads on 32 vCPUs) does not help, except +0–6% for AVX-512 A; it costs
  AVX-512 B ~15% (more scratch competing for L3).

### Auto-selection (`--mode tune`, `src/autotune.rs`)

Times every available candidate on this machine, in the configuration it would run in, and picks
the fastest. CPU: every implementation above on all worker threads. GPU: the fast kernels over launch
shapes of whole waves (blocks = SMs × waves) that fit in free VRAM; each candidate's output is
verified. Calibration runs each candidate at two lengths (56k and 104k iterations) and fits
`time per chunk = fixed + per_step × iterations`, then ranks by the predicted throughput at the real
iteration count. A single short run over-weights fixed per-chunk costs, and runs under ~56k
iterations are not yet on the line (AVX-512 A at 24k costs ~25% more per chunk than the fit
predicts). Choices are cached in `$XDG_CACHE_HOME/packing-variant-bench/tune.json`, keyed by CPU brand
string, available extensions, thread count and `PVB_CPU_DISABLE` (CPU), or SM count, compute
capability, VRAM, driver version and `--slice` (GPU), plus the variant and target iteration count.
Targets below 8192 iterations are predicted at 8192 (Phase 1 always runs in full). `--retune` ignores the cache; `--confirm` runs the choice
at full length. CPU tuning takes ~1–2 min (1 s warm-up per candidate); GPU tuning ~20 s.

Results (32 threads, A / B, chunks/s at 1M iterations, predicted → confirmed):

| emulated machine | CPU pick | A | B |
|---|---|---|---|
| this 9950X3D | `avx512-x16` | 1 308 → 1 257 | 942 → 933 |
| no SHA-NI, AVX-512 (Cascade Lake-like) | `avx512-x16` | 1 290 → 1 249 | 955 → 933 |
| AVX2 only (generic build) | `avx2-x8` | 416 → 382 | 376 → 371 |
| GPU (RTX 5090) | `fast-interleaved` 170 × 512, 87 040 chunks | 13 420 → 13 784 | 11 568 → 11 655 |

On the AVX2-only machine OpenSSL manages ~110 chunks/s, so the pick is 3.3–3.5× faster.

### GPU (`--kernel`, `--features cuda`)

| name | what |
|---|---|
| `ref` | the production kernel from before the port: its device code and byte-at-a-time SHA-256 (`cuda/sha256.cuh`, moved here from `crates/c/c_src`), same nvcc flags, one launch per run. |
| `fast-interleaved` | `cuda/pvb_fast.cuh`: register-resident, fully unrolled SHA-256 specialised for 32/64/68-byte messages (constant padding block folded); segment `s` of every chunk stored side by side, so a warp's accesses coalesce |
| `fast-contig` | same kernel, each chunk contiguous (production layout) |
| `fast-nomem` | diagnostic: `fast` Phase 2 hashing with no memory traffic; the compute ceiling. Output is not entropy and is not verified. |

Findings:
* **The production kernel is ~11× off the hardware.** Its SHA-256 updates one byte at a time
  through a 64-byte buffer in local memory: ~6 300 instructions per step plus local-memory traffic,
  against ~2 300 integer instructions (1 050 funnel shifts, 611 `LOP3`, ~630 adds), all in registers.
  The latter is close to the minimum for straightforward SHA-256.
* **The optimised kernel is power-bound at 575 W.** The compute ceiling is ~14 700 chunks/s; A
  reaches ~92% of it. Moving additions onto the IMAD pipe (`PVB_FAST_FMA_ADDS=1`) balanced the pipes
  but cost energy, so it was slower; it is off by default. Streaming cache hints
  (`PVB_FAST_CACHE_HINT=1`, default) gave B +1.4%; L2-only hints (`=2`) gave nothing.
* **B costs 14% on GPU** once SHA is fast, because its reads cannot be prefetched (A loads the next
  segment while hashing). With the production kernel this was hidden (~2%).
* **The contiguous layout is pathological for A.** Every thread of a warp hits the same offset in
  chunks exactly 256 KiB apart, which piles the accesses onto the same DRAM channels: 1 000–7 000
  chunks/s. B's random index spreads them out, which is why `fast-contig` B looks fine. The
  production kernel uses this layout too, but it is too slow for it to matter there.
* **Launch shape.** Exactly one block per SM (170 blocks) is consistently best. Shapes that leave a
  partial last wave (e.g. 425 or 213 blocks) or idle SMs (85 or 43 blocks) lose up to 70%.
* **VRAM caps parallelism.** Each in-flight chunk needs 256 KiB, so 32 GB holds ~110k chunks.
  Throughput is flat from ~43k chunks upwards.

### Long kernels under WDDM (Windows / WSL2)

Three times in ~150 runs, a long single launch (25–40 s: twice `ref`, once unsliced `fast-contig`)
silently did not execute. Symptoms: stale cudaEvent timestamps (a negative elapsed time equal to the
previous run), wrong output, no CUDA error, and then a hang on the next launch with the GPU idle. The
5090 drives an active Windows desktop, so it is regularly preempted. A driver-level reset that WSL2
never reports fits the evidence, but is not confirmed. No sliced launch ever failed. Mitigations:
* the fast kernels run Phase 2 in launches of `--slice` hash counts (default 50 000, ~0.3 s each).
  The chain state lives entirely in the buffer, so a launch resumes exactly where the last stopped.
  Overhead is within noise;
* every run checks cudaEvent time against the host clock (`TIMING ANOMALY`, run fails) and verifies
  sampled chunks against the CPU. Without verification, these runs would have produced
  plausible-looking wrong numbers.

A production GPU packer on Windows hosts should keep launches short in the same way.

## Commands

Run from this directory. Do not set `RUSTFLAGS`: the repo's `.cargo/config.toml`
(`-C target-cpu=native`) applies here too. Cargo may be configured with a shared target dir; a CUDA
build and a plain build there overwrite the same binary. Use a separate `CARGO_TARGET_DIR` for the
CUDA build if both are in use, and copy the binary before long runs.

```sh
cargo build --release
cargo test --release          # A == irys-c reference; SHA-NI / AVX-512 == reference; KATs

# CPU: all implementations, 32 threads, shared work queue
cargo run --release -- 1000000 --mode cpu --threads 32 --chunks-per-thread 64 --json results-cpu.json

# GPU (nvcc; --gpu-architecture=native unless PVB_CUDA_ARCH is set, e.g. PVB_CUDA_ARCH=sm_120)
cargo build --release --features cuda
cargo run --release --features cuda -- 50000 --mode gpu --batch 1000 --tpb 128 --verify 5   # quick check
cargo run --release --features cuda -- 1000000 --mode gpu --kernel fast-interleaved \
  --batch 87040 --tpb 512 --json results-gpu.json                                           # best config

# production GPU packing end to end (also at 1000 iterations for devnet / testnet)
cargo run --release --features cuda -- 1000000 --mode prod --prod-concurrency 2

# SHA-NI / AVX-512 interleave-width comparison
cargo run --release --example cpu_fast_bench
```

Useful flags: `--variants a,b`, `--cpu-impl ...`, `--kernel ...`, `--tpb`, `--batch`, `--blocks`,
`--slice`, `--verify N`, `--threads`, `--single-chunks`, `--chunks-per-thread`, `--prod-batch`,
`--prod-concurrency`, `--prod-chunks`. Iterations below 8192 run Phase 1 only. See `--help`.

Build switches (env, CUDA only): `PVB_CUDA_ARCH`, `PVB_FAST_FMA_ADDS` (0/1), `PVB_FAST_CACHE_HINT`
(0 default caching, 1 streaming (default), 2 L2-only). nvcc flags follow `crates/c/build/capacity.rs`,
including `.debug(false)`. Without it, a cargo profile with debug info makes `cc` pass `-G`, which turns
off device optimisation (~40× slower; symptom: `992 B local/thread` for `ref`). As a check, `ref` A
at batch 21760, tpb 128 takes ~20 s on an RTX 5090 at 1M iterations.

## Memory

| batch | device memory (batch × 256 KiB) |
|---|---|
| 21 760 | 5.3 GiB |
| 43 520 | 10.6 GiB |
| 87 040 | 21.3 GiB |
| 108 800 | 26.6 GiB |

Host RAM: only `--verify` chunks are copied back (256 KiB each). A full copy-back, as production
does, would need another `batch × 256 KiB` of host RAM. CPU: 256 KiB per chunk in flight
(AVX-512: 4 MiB scratch per 16-chunk group per thread).

## Reading the output

* `occ`: the most blocks per SM that can be resident (CUDA occupancy API). `resident` =
  min(occ × SMs × tpb, blocks × tpb).
* `verify[n]=PASS|FAIL|n/a`, or `TIMING` when the event time disagrees with the host clock. Best-of
  summaries and GPU/CPU ratios use only verified runs with consistent timing.
* The summary prints B vs A per CPU implementation and per GPU kernel, and the GPU/CPU ratio for each
  CPU implementation.
* `--json PATH` writes everything, plus device and kernel info (registers, local memory, host time).

## Future work

* SHA-NI and AVX-512 on the same core at once (SHA-NI chains on one SMT sibling, AVX-512 lanes on the
  other). Might use both execution resources, or might contend for the same ports on Zen; untested.
* CCD-aware placement for B: AVX-512 on the 96 MB V-cache CCD and SHA-NI on the other, so all scratch
  fits in L3 (AVX-512 B loses ~25% to L3 overflow). Needs core pinning, which WSL2 cannot do.
* The C implementation (`c_src/capacity_single.c`) calls `EVP_DigestInit_ex(.., EVP_sha256(), ..)`
  per hash, which may also do an implicit fetch per call in OpenSSL 3. Only benches/tests use it.
