//! The CPU implementations as one dispatchable list, plus the shared all-threads throughput runner
//! used by both the benchmark and the auto-tuner.

use crate::{
    CHUNK_SIZE, Chunk, ChunkId, Variant, cpu_avx2, cpu_avx512, cpu_fast, entropy, new_chunk,
};
use std::{
    sync::{
        Barrier, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

/// CPU implementations. `Openssl` is the portable baseline (any CPU); `IrysC` is the unmodified
/// irys-c reference (variant A only); `ShaNi*` need the x86 SHA extensions, `Avx512*` AVX-512F
/// and `Avx2*` AVX2. All are detected at runtime and skipped when the CPU lacks them; none is
/// required.
///
/// Built with the repo's `-C target-cpu=native` on a CPU with AVX-512VL, the compiler uses
/// AVX-512 rotate/ternary-logic instructions inside the AVX2 functions too, so emulating an
/// AVX2-only CPU here (`PVB_CPU_DISABLE`) needs a generic build; see the README.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CpuKind {
    Openssl,
    IrysC,
    ShaNiX1,
    ShaNiX2,
    ShaNiX4,
    ShaNiX8,
    Avx512X16,
    Avx512X32,
    Avx2X8,
    Avx2X16,
}

/// Per-thread working memory reused across calls.
pub struct CpuScratch {
    /// irys-c reference output
    bytes: Vec<u8>,
    /// AVX-512 lane-interleaved state (grown on first use)
    avx512: cpu_avx512::Scratch,
    /// AVX2 lane-interleaved state (grown on first use)
    avx2: cpu_avx2::Scratch,
}

impl CpuScratch {
    pub fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(CHUNK_SIZE),
            avx512: cpu_avx512::Scratch::new(0),
            avx2: cpu_avx2::Scratch::new(0),
        }
    }
}

impl Default for CpuScratch {
    fn default() -> Self {
        Self::new()
    }
}

/// Instruction-set extensions that `PVB_CPU_DISABLE` can hide, to emulate CPUs without them
/// (e.g. `PVB_CPU_DISABLE=sha-ni,avx512` behaves like a Cascade Lake without AVX-512).
fn disabled(feature: &str) -> bool {
    static MASK: OnceLock<Vec<String>> = OnceLock::new();
    MASK.get_or_init(|| {
        std::env::var("PVB_CPU_DISABLE")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    })
    .iter()
    .any(|f| f == feature)
}

/// The `PVB_CPU_DISABLE` value in effect (part of the auto-tune cache key).
pub fn disabled_features() -> String {
    std::env::var("PVB_CPU_DISABLE").unwrap_or_default()
}

impl CpuKind {
    pub const ALL: [Self; 10] = [
        Self::Openssl,
        Self::IrysC,
        Self::ShaNiX1,
        Self::ShaNiX2,
        Self::ShaNiX4,
        Self::ShaNiX8,
        Self::Avx512X16,
        Self::Avx512X32,
        Self::Avx2X8,
        Self::Avx2X16,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Openssl => "openssl",
            Self::IrysC => "irys-c",
            Self::ShaNiX1 => "sha-ni-x1",
            Self::ShaNiX2 => "sha-ni-x2",
            Self::ShaNiX4 => "sha-ni-x4",
            Self::ShaNiX8 => "sha-ni-x8",
            Self::Avx512X16 => "avx512-x16",
            Self::Avx512X32 => "avx512-x32",
            Self::Avx2X8 => "avx2-x8",
            Self::Avx2X16 => "avx2-x16",
        }
    }

    /// Chunks per call: the interleaved implementations advance N hash chains together.
    pub fn batch(self) -> usize {
        match self {
            Self::Openssl | Self::IrysC | Self::ShaNiX1 => 1,
            Self::ShaNiX2 => 2,
            Self::ShaNiX4 => 4,
            Self::ShaNiX8 => 8,
            Self::Avx512X16 => cpu_avx512::LANES,
            Self::Avx512X32 => 2 * cpu_avx512::LANES,
            Self::Avx2X8 => cpu_avx2::LANES,
            Self::Avx2X16 => 2 * cpu_avx2::LANES,
        }
    }

    /// Whether this CPU can run the implementation (runtime detection, minus `PVB_CPU_DISABLE`).
    pub fn available(self) -> bool {
        match self {
            Self::Openssl | Self::IrysC => true,
            Self::ShaNiX1 | Self::ShaNiX2 | Self::ShaNiX4 | Self::ShaNiX8 => {
                cpu_fast::available() && !disabled("sha-ni")
            }
            Self::Avx512X16 | Self::Avx512X32 => cpu_avx512::available() && !disabled("avx512"),
            Self::Avx2X8 | Self::Avx2X16 => cpu_avx2::available() && !disabled("avx2"),
        }
    }

    pub fn supports(self, v: Variant) -> bool {
        self != Self::IrysC || v == Variant::A
    }

    /// Computes `bufs[k]` for `ids[k]`; both must hold exactly `self.batch()` entries.
    pub fn compute(
        self,
        v: Variant,
        ids: &[ChunkId],
        iterations: u32,
        bufs: &mut [Box<Chunk>],
        scratch: &mut CpuScratch,
    ) {
        assert_eq!(ids.len(), self.batch());
        assert_eq!(bufs.len(), self.batch());
        fn multi<const N: usize>(v: Variant, ids: &[ChunkId], it: u32, bufs: &mut [Box<Chunk>]) {
            let ids: &[ChunkId; N] = ids.try_into().unwrap();
            let bufs: &mut [Box<Chunk>; N] = bufs.try_into().unwrap();
            cpu_fast::entropy_multi::<N>(v, ids, it, bufs);
        }
        match self {
            Self::Openssl => {
                for (id, buf) in ids.iter().zip(bufs.iter_mut()) {
                    entropy(v, id, iterations, buf);
                }
            }
            Self::IrysC => crate::reference_a(&ids[0], iterations, &mut scratch.bytes),
            Self::ShaNiX1 => multi::<1>(v, ids, iterations, bufs),
            Self::ShaNiX2 => multi::<2>(v, ids, iterations, bufs),
            Self::ShaNiX4 => multi::<4>(v, ids, iterations, bufs),
            Self::ShaNiX8 => multi::<8>(v, ids, iterations, bufs),
            Self::Avx512X16 => {
                cpu_avx512::entropy_lanes::<1>(v, ids, iterations, bufs, &mut scratch.avx512)
            }
            Self::Avx512X32 => {
                cpu_avx512::entropy_lanes::<2>(v, ids, iterations, bufs, &mut scratch.avx512)
            }
            Self::Avx2X8 => {
                cpu_avx2::entropy_lanes::<1>(v, ids, iterations, bufs, &mut scratch.avx2)
            }
            Self::Avx2X16 => {
                cpu_avx2::entropy_lanes::<2>(v, ids, iterations, bufs, &mut scratch.avx2)
            }
        }
        std::hint::black_box((&bufs, &scratch.bytes));
    }
}

/// Result of an all-threads run.
#[derive(Clone, Copy, Debug)]
pub struct Throughput {
    pub chunks: u64,
    pub wall_s: f64,
}

impl Throughput {
    pub fn chunks_per_s(&self) -> f64 {
        self.chunks as f64 / self.wall_s
    }
}

/// How long every worker runs the same implementation before the timed start, so the timed part
/// runs at sustained clocks and with its buffers already faulted in.
///
/// All-core boost does not last: on a 9950X3D, AVX-512 on 32 threads runs ~25% faster for the
/// first ~0.5 s than sustained, before power/current limits pull clocks down. Without this
/// warm-up, short runs (such as auto-tune calibration) overstated AVX-512 by that much.
const WARMUP: Duration = Duration::from_secs(1);

/// Minimum batches per worker in [`throughput`].
pub const MIN_BATCHES_PER_THREAD: usize = 8;

/// Runs `threads * chunks_per_thread` chunks (rounded up to whole batches) on `threads` workers
/// pulling batches from a shared counter. `id(k)` gives the id of chunk `k`.
///
/// A fixed share per thread would make the wall time that of the slowest thread, so one briefly
/// pre-empted vCPU (common under a hypervisor) would skew the result. Every worker first runs
/// short batches for [`WARMUP`], which also pre-faults its buffers.
pub fn throughput(
    kind: CpuKind,
    v: Variant,
    threads: usize,
    chunks_per_thread: usize,
    iterations: u32,
    id: &(dyn Fn(u64) -> ChunkId + Sync),
) -> Throughput {
    let n = kind.batch();
    // The run ends when the slowest thread finishes its last batch, so with only a couple of
    // batches per thread the queue cannot balance: AVX-512 x16 at 1M iterations measured 1 015
    // chunks/s with 2 batches per thread and 1 150 with 8.
    let chunks_per_thread = chunks_per_thread.max(MIN_BATCHES_PER_THREAD * n);
    let batches = (threads * chunks_per_thread).div_ceil(n);
    let next = AtomicUsize::new(0);
    let barrier = Barrier::new(threads + 1);
    // Timed from the moment all workers are released to the moment the last one runs out of work.
    // Thread exit and freeing the buffers are not timed: under WSL2, 32 threads unmapping at once
    // cost ~0.2 s per run regardless of the work done.
    let (start, end) = std::thread::scope(|s| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let (barrier, next) = (&barrier, &next);
                s.spawn(move || {
                    let mut bufs: Vec<Box<Chunk>> = (0..n).map(|_| new_chunk()).collect();
                    let mut scratch = CpuScratch::new();
                    let warm_ids: Vec<ChunkId> = (0..n).map(|k| id(k as u64)).collect();
                    let warm_until = Instant::now() + WARMUP;
                    while Instant::now() < warm_until {
                        // Phase 1 plus a short Phase 2: touches the whole ring
                        kind.compute(v, &warm_ids, 8192 + 2048, &mut bufs, &mut scratch);
                    }
                    barrier.wait();
                    loop {
                        let b = next.fetch_add(1, Ordering::Relaxed);
                        if b >= batches {
                            break;
                        }
                        let ids: Vec<ChunkId> = (0..n).map(|k| id((b * n + k) as u64)).collect();
                        kind.compute(v, &ids, iterations, &mut bufs, &mut scratch);
                    }
                    Instant::now()
                })
            })
            .collect();
        barrier.wait();
        let start = Instant::now();
        let end = workers
            .into_iter()
            .map(|w| w.join().expect("worker panicked"))
            .max()
            .unwrap_or(start);
        (start, end)
    });
    Throughput {
        chunks: (batches * n) as u64,
        wall_s: (end - start).as_secs_f64(),
    }
}
