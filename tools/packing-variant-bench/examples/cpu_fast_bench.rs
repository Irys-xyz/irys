//! Benchmarks the optimised CPU implementations against the OpenSSL reference, single-threaded
//! and on 32 / 16 threads:
//! - `cpu_fast` (SHA-NI) with N interleaved chunks per thread,
//! - `cpu_avx512` (16 chunks per 512-bit vector) with G groups of 16 chunks per thread,
//! - `cpu_avx2` (8 chunks per 256-bit vector) with G groups of 8 chunks per thread, and B's
//!   data-dependent read done by gathers or by row loads + transpose.
//!
//! To see OpenSSL as it runs on a CPU without SHA-NI, mask the SHA CPUID bit for OpenSSL:
//! `OPENSSL_ia32cap=":~0x20000000" cargo run --release --example cpu_fast_bench`
//!
//! Implementations the CPU does not support are reported as n/a (never timed as a fallback).
//!
//! cargo run --release --example cpu_fast_bench

use packing_variant_bench::{
    Chunk, DEFAULT_ITERATIONS, Variant, cpu_avx2, cpu_avx512, cpu_fast, entropy, fixed_id,
    new_chunk,
};
use std::time::Instant;

/// Minimum chunks per thread in the multi-threaded runs.
const MIN_CHUNKS_PER_THREAD: usize = 16;

#[derive(Clone, Copy)]
enum Impl {
    OpenSsl,
    Ni(usize),
    Avx512(usize),
    Avx2(usize, cpu_avx2::BRead),
}

impl Impl {
    // Avx512(4) was also tried: it spills heavily and is slower than 1x16.
    const ALL: [Self; 10] = [
        Self::OpenSsl,
        Self::Ni(1),
        Self::Ni(4),
        Self::Ni(8),
        Self::Avx512(1),
        Self::Avx512(2),
        Self::Avx2(1, cpu_avx2::BRead::Transpose),
        Self::Avx2(1, cpu_avx2::BRead::Gather),
        Self::Avx2(2, cpu_avx2::BRead::Transpose),
        Self::Avx2(2, cpu_avx2::BRead::Gather),
    ];

    fn name(self) -> String {
        match self {
            Self::OpenSsl => "openssl".into(),
            Self::Ni(n) => format!("sha-ni N={n}"),
            Self::Avx512(g) => format!("avx512 {g}x16"),
            Self::Avx2(g, cpu_avx2::BRead::Transpose) => format!("avx2 {g}x8"),
            Self::Avx2(g, cpu_avx2::BRead::Gather) => format!("avx2 {g}x8 gath"),
        }
    }

    fn available(self) -> bool {
        match self {
            Self::OpenSsl => true,
            Self::Ni(_) => cpu_fast::available(),
            Self::Avx512(_) => cpu_avx512::available(),
            Self::Avx2(..) => cpu_avx2::available(),
        }
    }

    /// Chunks computed by one call.
    fn batch(self) -> usize {
        match self {
            Self::OpenSsl => 1,
            Self::Ni(n) => n,
            Self::Avx512(g) => g * cpu_avx512::LANES,
            Self::Avx2(g, _) => g * cpu_avx2::LANES,
        }
    }
}

/// Per-thread working memory for one implementation.
struct Ctx {
    bufs: Vec<Box<Chunk>>,
    scratch: cpu_avx512::Scratch,
    scratch2: cpu_avx2::Scratch,
}

impl Ctx {
    fn new(imp: Impl) -> Self {
        let (groups512, groups2) = match imp {
            Impl::Avx512(g) => (g, 0),
            Impl::Avx2(g, _) => (0, g),
            _ => (0, 0),
        };
        Self {
            bufs: (0..imp.batch()).map(|_| new_chunk()).collect(),
            scratch: cpu_avx512::Scratch::new(groups512),
            scratch2: cpu_avx2::Scratch::new(groups2),
        }
    }
}

fn ni<const N: usize>(variant: Variant, first_offset: u64, bufs: &mut [Box<Chunk>]) {
    let ids = std::array::from_fn(|n| fixed_id(first_offset + n as u64));
    let bufs: &mut [Box<Chunk>; N] = bufs.try_into().unwrap();
    cpu_fast::entropy_multi::<N>(variant, &ids, DEFAULT_ITERATIONS, bufs);
}

/// One batch of `imp.batch()` chunks with partition offsets starting at `first_offset`.
fn run_batch(imp: Impl, variant: Variant, first_offset: u64, ctx: &mut Ctx) {
    match imp {
        Impl::OpenSsl => entropy(
            variant,
            &fixed_id(first_offset),
            DEFAULT_ITERATIONS,
            &mut ctx.bufs[0],
        ),
        Impl::Ni(1) => ni::<1>(variant, first_offset, &mut ctx.bufs),
        Impl::Ni(4) => ni::<4>(variant, first_offset, &mut ctx.bufs),
        Impl::Ni(8) => ni::<8>(variant, first_offset, &mut ctx.bufs),
        Impl::Avx512(g) => {
            let ids: Vec<_> = (0..imp.batch())
                .map(|k| fixed_id(first_offset + k as u64))
                .collect();
            let (bufs, scratch) = (&mut ctx.bufs, &mut ctx.scratch);
            match g {
                1 => {
                    cpu_avx512::entropy_lanes::<1>(variant, &ids, DEFAULT_ITERATIONS, bufs, scratch)
                }
                2 => {
                    cpu_avx512::entropy_lanes::<2>(variant, &ids, DEFAULT_ITERATIONS, bufs, scratch)
                }
                _ => unreachable!(),
            }
        }
        Impl::Avx2(g, b_read) => {
            let ids: Vec<_> = (0..imp.batch())
                .map(|k| fixed_id(first_offset + k as u64))
                .collect();
            let (bufs, scratch) = (&mut ctx.bufs, &mut ctx.scratch2);
            let it = DEFAULT_ITERATIONS;
            match g {
                1 => cpu_avx2::entropy_lanes_with::<1>(variant, &ids, it, bufs, scratch, b_read),
                2 => cpu_avx2::entropy_lanes_with::<2>(variant, &ids, it, bufs, scratch, b_read),
                _ => unreachable!(),
            }
        }
        Impl::Ni(_) => unreachable!(),
    }
}

/// Single-thread seconds per chunk (one warm-up batch, then one timed batch).
fn single(imp: Impl, variant: Variant) -> f64 {
    let mut ctx = Ctx::new(imp);
    run_batch(imp, variant, 0, &mut ctx);
    let start = Instant::now();
    run_batch(imp, variant, 0, &mut ctx);
    start.elapsed().as_secs_f64() / imp.batch() as f64
}

/// Chunks/s with `threads` threads, each computing at least MIN_CHUNKS_PER_THREAD chunks.
fn multi(imp: Impl, variant: Variant, threads: usize) -> f64 {
    let batches = MIN_CHUNKS_PER_THREAD.div_ceil(imp.batch());
    let start = Instant::now();
    std::thread::scope(|s| {
        for t in 0..threads {
            s.spawn(move || {
                let mut ctx = Ctx::new(imp);
                for b in 0..batches {
                    let offset = ((t * batches + b) * imp.batch()) as u64;
                    run_batch(imp, variant, offset, &mut ctx);
                }
            });
        }
    });
    (threads * batches * imp.batch()) as f64 / start.elapsed().as_secs_f64()
}

fn main() {
    println!(
        "iterations={DEFAULT_ITERATIONS}, multi-threaded runs: >= {MIN_CHUNKS_PER_THREAD} chunks per thread"
    );
    println!(
        "{:<8} {:<14} {:>16} {:>14} {:>14}",
        "variant", "impl", "1-thread s/chunk", "32T chunk/s", "16T chunk/s"
    );
    // results[variant][impl] = (single, 32T, 16T)
    let mut results = Vec::new();
    for variant in Variant::ALL {
        let mut rows = Vec::new();
        for imp in Impl::ALL {
            if !imp.available() {
                println!("{:<8} {:<14} {:>16}", variant.name(), imp.name(), "n/a");
                rows.push(None);
                continue;
            }
            let r = (
                single(imp, variant),
                multi(imp, variant, 32),
                multi(imp, variant, 16),
            );
            println!(
                "{:<8} {:<14} {:>16.4} {:>14.1} {:>14.1}",
                variant.name(),
                imp.name(),
                r.0,
                r.1,
                r.2
            );
            rows.push(Some(r));
        }
        results.push(rows);
    }
    // B relative to A for each implementation (>1 = B slower)
    println!("\nB/A time ratio: impl, 1-thread, 32T, 16T");
    for (imp, (a, b)) in Impl::ALL.iter().zip(results[0].iter().zip(&results[1])) {
        if let (Some(a), Some(b)) = (a, b) {
            println!(
                "{:<14} {:>6.3} {:>6.3} {:>6.3}",
                imp.name(),
                b.0 / a.0,
                a.1 / b.1,
                a.2 / b.2
            );
        }
    }
}
