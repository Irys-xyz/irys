//! Latency and throughput of one 256-bit modular squaring (the VDE root is 253 of them in
//! series): portable Rust vs MULX/ADX assembly. Best of 7 alternating runs.

use packing_variant_bench::vde::{U256, sqr_asm_or_portable, sqr_portable};
use std::{hint::black_box, time::Instant};

const N: u32 = 2_000_000;

type Sqr = fn(&U256) -> U256;

fn latency(f: Sqr) -> f64 {
    let mut x: U256 = black_box([0x1234_5678_9abc_def0, 7, 11, 13]);
    let t = Instant::now();
    for _ in 0..N {
        x = f(&x);
    }
    black_box(x);
    t.elapsed().as_secs_f64() * 1e9 / N as f64
}

fn throughput(f: Sqr) -> f64 {
    let mut xs: [U256; 8] = black_box(core::array::from_fn(|k| [k as u64 + 3, 5, 7, 9]));
    let t = Instant::now();
    for _ in 0..N / 8 {
        for x in &mut xs {
            *x = f(x);
        }
    }
    black_box(xs);
    t.elapsed().as_secs_f64() * 1e9 / N as f64
}

fn main() {
    let impls: [(&str, Sqr); 2] = [
        ("portable", sqr_portable),
        ("mulx/adx asm", sqr_asm_or_portable),
    ];
    let mut best = [[f64::MAX; 2]; 2];
    for _ in 0..7 {
        for (k, (_, f)) in impls.iter().enumerate() {
            best[k][0] = best[k][0].min(latency(*f));
            best[k][1] = best[k][1].min(throughput(*f));
        }
    }
    for (k, (name, _)) in impls.iter().enumerate() {
        println!(
            "{name:>14}: {:.2} ns per squaring in series (root ~{:.2} us), {:.2} ns with 8 chains interleaved",
            best[k][0],
            best[k][0] * 266.0 / 1e3,
            best[k][1]
        );
    }
}
