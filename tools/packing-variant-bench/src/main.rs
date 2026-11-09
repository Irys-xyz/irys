use clap::Parser;
use packing_variant_bench::{
    CHUNK_SIZE, Chunk, ChunkId, DEFAULT_ITERATIONS, Variant,
    cpu::{self, CpuKind, CpuScratch},
    fixed_id, new_chunk,
};
use serde::Serialize;
use std::{path::PathBuf, time::Instant};

#[derive(Parser, Debug)]
#[command(about = "Compare entropy-packing variant A (current) vs B (data-dependent read index)")]
struct Args {
    /// Total SHA-256 iterations per chunk (Phase 1 + Phase 2), as `iterations` in compute_entropy_chunk.
    #[arg(default_value_t = DEFAULT_ITERATIONS)]
    iterations: u32,

    /// Variants to run.
    #[arg(long, value_enum, value_delimiter = ',', default_values_t = Variant::ALL.to_vec())]
    variants: Vec<Variant>,

    /// What to run.
    #[arg(long, value_enum, default_value_t = Mode::default())]
    mode: Mode,

    /// CPU: chunks timed sequentially on one thread (per variant).
    #[arg(long, default_value_t = 3)]
    single_chunks: u32,

    /// CPU: worker threads for the all-threads run (default: all logical CPUs).
    #[arg(long)]
    threads: Option<usize>,

    /// CPU: chunks per thread in the all-threads run (minimum 4); threads pull batches from a
    /// shared queue, so this sets the total work (threads x chunks-per-thread).
    #[arg(long, default_value_t = 4)]
    chunks_per_thread: u32,

    /// CPU: implementations to time (unavailable ones are skipped with a note).
    #[arg(long, value_enum, value_delimiter = ',', default_values_t = CpuKind::ALL.to_vec())]
    cpu_impl: Vec<CpuKind>,

    /// GPU: threads-per-block values to sweep.
    #[arg(long, value_delimiter = ',', default_values_t = vec![128_u32, 256, 512])]
    tpb: Vec<u32>,

    /// GPU: batch sizes (chunks per launch) to sweep. Device memory = batch x 256 KiB.
    #[arg(long, value_delimiter = ',', default_values_t = vec![21760_u64, 43520])]
    batch: Vec<u64>,

    /// GPU: override the block count (default: ceil(batch / tpb)).
    #[arg(long)]
    blocks: Option<u32>,

    /// GPU: kernels to run (`ref` is the production-equivalent kernel).
    #[cfg(feature = "cuda")]
    #[arg(long, value_enum, value_delimiter = ',', default_values_t = vec![
        packing_variant_bench::gpu::Kernel::Ref,
        packing_variant_bench::gpu::Kernel::FastContig,
        packing_variant_bench::gpu::Kernel::FastInterleaved,
    ])]
    kernel: Vec<packing_variant_bench::gpu::Kernel>,

    /// GPU: split each fast-kernel run into launches of at most this many Phase 2 hashes (0 = one
    /// launch). Short launches avoid long preemption windows and driver timeouts on WDDM
    /// (Windows / WSL2); measured overhead is within noise. The `ref` kernel always runs as one
    /// launch, as production does.
    #[cfg(feature = "cuda")]
    #[arg(long, default_value_t = 50_000)]
    slice: u32,

    /// GPU: chunks copied back per run and checked against the CPU implementation.
    #[arg(long, default_value_t = 3)]
    verify: u32,

    /// Write results as JSON to this path.
    #[arg(long)]
    json: Option<PathBuf>,

    /// Tune: cache file for chosen implementations (default
    /// $XDG_CACHE_HOME/packing-variant-bench/tune.json).
    #[arg(long)]
    tune_cache: Option<PathBuf>,

    /// Tune: ignore cached choices and calibrate again.
    #[arg(long)]
    retune: bool,

    /// Tune: after choosing, run the chosen configuration at full `iterations` and report it.
    #[arg(long)]
    confirm: bool,

    /// Prod: chunks per batch, or `auto` (`CUDAConfig::auto_batch_size` for --prod-concurrency).
    #[arg(long, default_value = "auto")]
    prod_batch: String,

    /// Prod: batches in flight at once, each on its own packer (the node's packing concurrency).
    #[arg(long, default_value_t = 2)]
    prod_concurrency: u16,

    /// Prod: total chunks to pack (default: 4 batches).
    #[arg(long)]
    prod_chunks: Option<u32>,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Cpu,
    Gpu,
    All,
    /// Pick the fastest CPU implementation (and GPU kernel / launch shape) for this machine.
    Tune,
    /// Production GPU packing end to end (`irys_packing::CudaPackerPool`, variant A): kernels,
    /// copy-back, CPU spot-check and one host copy per chunk, as the packing service does.
    Prod,
}

impl Default for Mode {
    fn default() -> Self {
        if cfg!(feature = "cuda") {
            Self::All
        } else {
            Self::Cpu
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use clap::ValueEnum as _;
        f.write_str(self.to_possible_value().unwrap().get_name())
    }
}

#[derive(Serialize, Debug, Clone)]
struct CpuResult {
    implementation: String,
    variant: String,
    /// Chunks computed together on one thread (interleaved hash chains).
    chunks_per_call: usize,
    single_thread_chunks: u32,
    single_thread_s_per_chunk: f64,
    threads: usize,
    chunks_per_thread: u32,
    all_threads_chunks: u64,
    all_threads_wall_s: f64,
    all_threads_chunks_per_s: f64,
}

#[derive(Serialize, Debug, Clone)]
struct GpuResult {
    kernel: String,
    variant: String,
    threads_per_block: u32,
    batch: u64,
    blocks: u32,
    blocks_per_sm_occupancy: i32,
    resident_threads: u64,
    kernel_ms: f64,
    /// Host wall-clock time around launch + synchronize, as a cross-check of `kernel_ms`.
    host_ms: f64,
    /// False when the cudaEvent time is non-positive or disagrees with the host clock.
    timing_ok: bool,
    chunks_per_s: f64,
    verified_chunks: Vec<u64>,
    verify_pass: bool,
}

#[derive(Serialize, Debug, Default)]
struct Report {
    iterations: u32,
    chunk_size: usize,
    host_logical_cpus: usize,
    cpu: Vec<CpuResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    gpu_device: Option<serde_json::Value>,
    gpu: Vec<GpuResult>,
    gpu_error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tune: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prod: Option<serde_json::Value>,
}

const OFFSET_START: u64 = 1_000;

fn main() {
    // Below 8192 iterations only Phase 1 runs (testnet and devnet use 1000); every implementation
    // handles that, and the tests cover it.
    let args = Args::parse();
    let logical = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut report = Report {
        iterations: args.iterations,
        chunk_size: CHUNK_SIZE,
        host_logical_cpus: logical,
        ..Default::default()
    };
    println!(
        "packing-variant-bench: iterations={} mode={}",
        args.iterations, args.mode
    );

    if matches!(args.mode, Mode::Cpu | Mode::All) {
        report.cpu = run_cpu(&args, logical);
    }
    if matches!(args.mode, Mode::Gpu | Mode::All) {
        run_gpu(&args, &mut report);
    }
    if args.mode == Mode::Tune {
        run_tune(&args, logical, &mut report);
    }
    if args.mode == Mode::Prod {
        run_prod(&args, &mut report);
    }

    print_summary(&report);

    if let Some(path) = &args.json {
        std::fs::write(path, serde_json::to_string_pretty(&report).unwrap())
            .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
        println!("JSON results written to {}", path.display());
    }

    let gpu_failed =
        report.gpu_error.is_some() || report.gpu.iter().any(|g| !g.verify_pass || !g.timing_ok);
    if gpu_failed {
        std::process::exit(1);
    }
}

// ------------------------------------------------------------------------------------------
// CPU

fn run_cpu(args: &Args, logical: usize) -> Vec<CpuResult> {
    let threads = args.threads.unwrap_or(logical).max(1);
    let id = |k: u64| fixed_id(OFFSET_START + k);
    let mut out = Vec::new();
    for &kind in &args.cpu_impl {
        if !kind.available() {
            println!(
                "[cpu] {:<10} skipped: not supported by this CPU",
                kind.name()
            );
            continue;
        }
        let n = kind.batch();
        // round chunk counts up to whole batches
        let single_chunks = (args.single_chunks.max(1) as usize).div_ceil(n) * n;
        let cpt = (args.chunks_per_thread.max(4) as usize).div_ceil(n) * n;
        for &v in &args.variants {
            if !kind.supports(v) {
                continue;
            }
            let label = format!("{}/{}", kind.name(), v.name());

            // single thread
            let mut bufs: Vec<Box<Chunk>> = (0..n).map(|_| new_chunk()).collect();
            let mut scratch = CpuScratch::new();
            let ids_from = |first: usize| -> Vec<ChunkId> {
                (first..first + n).map(|k| id(k as u64)).collect()
            };
            // warm-up (page faults, code/caches, frequency ramp)
            kind.compute(
                v,
                &ids_from(0),
                args.iterations.min(100_000),
                &mut bufs,
                &mut scratch,
            );
            let t = Instant::now();
            for k in (0..single_chunks).step_by(n) {
                kind.compute(v, &ids_from(k), args.iterations, &mut bufs, &mut scratch);
            }
            let single = t.elapsed().as_secs_f64() / single_chunks as f64;
            println!(
                "[cpu] {label:<14} single-thread: {single:.4} s/chunk ({single_chunks} chunks, {n} per call)"
            );

            // all threads, shared work queue (see cpu::throughput)
            let tp = cpu::throughput(kind, v, threads, cpt, args.iterations, &id);
            let cps = tp.chunks_per_s();
            println!(
                "[cpu] {label:<14} all-threads ({threads}x{}): {cps:.2} chunks/s ({} chunks in {:.2} s)",
                tp.chunks / threads as u64,
                tp.chunks,
                tp.wall_s
            );
            out.push(CpuResult {
                implementation: kind.name().to_string(),
                variant: v.name().to_string(),
                chunks_per_call: n,
                single_thread_chunks: single_chunks as u32,
                single_thread_s_per_chunk: single,
                threads,
                chunks_per_thread: (tp.chunks / threads as u64) as u32,
                all_threads_chunks: tp.chunks,
                all_threads_wall_s: tp.wall_s,
                all_threads_chunks_per_s: cps,
            });
        }
    }
    out
}

// ------------------------------------------------------------------------------------------
// Prod

#[cfg(not(feature = "cuda"))]
fn run_prod(_args: &Args, report: &mut Report) {
    let msg = "built without the `cuda` feature; rebuild with --features cuda".to_string();
    eprintln!("[prod] {msg}");
    report.gpu_error = Some(msg);
}

#[cfg(feature = "cuda")]
fn run_prod(args: &Args, report: &mut Report) {
    if let Err(e) = run_prod_inner(args, report) {
        eprintln!("[prod] {e:?}");
        report.gpu_error = Some(format!("{e:?}"));
    }
}

/// Packs `--prod-chunks` chunks (variant A) through `CudaPackerPool` on `--prod-concurrency`
/// threads, each pulling the next batch and copying every chunk of every slab out once.
#[cfg(feature = "cuda")]
fn run_prod_inner(args: &Args, report: &mut Report) -> eyre::Result<()> {
    use irys_packing::{CUDAConfig, CudaPackerPool};
    use std::sync::atomic::{AtomicU32, Ordering};

    let config = CUDAConfig::from_device_default()?;
    let concurrency = args.prod_concurrency.max(1);
    let batch = if args.prod_batch == "auto" {
        config.auto_batch_size_for_device(0, concurrency)?
    } else {
        args.prod_batch.parse()?
    };
    let total = args.prod_chunks.unwrap_or(batch.saturating_mul(4));
    let id = fixed_id(0);
    let address = irys_types::IrysAddress::from(id.address);
    let partition_hash = irys_types::H256::from(id.partition_hash);
    let pool = CudaPackerPool::default();
    let next = AtomicU32::new(0);

    // allocate the packers before timing, as a running node already has them
    std::thread::scope(|s| {
        for _ in 0..concurrency {
            s.spawn(|| pool.with_packer(batch, |_| Ok(())));
        }
    });

    let start = Instant::now();
    let results: Vec<eyre::Result<()>> = std::thread::scope(|s| {
        let workers: Vec<_> = (0..concurrency)
            .map(|_| {
                s.spawn(|| -> eyre::Result<()> {
                    let mut sink = vec![0_u8; CHUNK_SIZE];
                    loop {
                        let first = next.fetch_add(batch, Ordering::Relaxed);
                        if first >= total {
                            return Ok(());
                        }
                        let n = batch.min(total - first);
                        pool.with_packer(batch, |packer| {
                            packer.pack(
                                n,
                                address,
                                u64::from(first),
                                partition_hash,
                                args.iterations,
                                id.chain_id,
                                |_, slab| {
                                    for chunk in slab.chunks_exact(CHUNK_SIZE) {
                                        sink.copy_from_slice(chunk);
                                        std::hint::black_box(&sink);
                                    }
                                    Ok(())
                                },
                            )
                        })?;
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| w.join().expect("worker panicked"))
            .collect()
    });
    let wall = start.elapsed().as_secs_f64();
    for r in results {
        r?;
    }
    let cps = f64::from(total) / wall;
    println!(
        "[prod] batch={batch} concurrency={concurrency} chunks={total} iterations={}: {wall:.2} s, {cps:.1} chunks/s",
        args.iterations
    );
    report.prod = Some(serde_json::json!({
        "batch": batch,
        "concurrency": concurrency,
        "chunks": total,
        "wall_s": wall,
        "chunks_per_s": cps,
    }));
    Ok(())
}

// ------------------------------------------------------------------------------------------
// Tune

fn run_tune(args: &Args, logical: usize, report: &mut Report) {
    use packing_variant_bench::autotune::{self, Cache};

    let path = args
        .tune_cache
        .clone()
        .unwrap_or_else(autotune::default_cache_path);
    let mut cache = Cache::load(&path);
    let threads = args.threads.unwrap_or(logical).max(1);
    let id = |k: u64| fixed_id(OFFSET_START + k);

    for &v in &args.variants {
        let key = autotune::cpu_key(v, threads, args.iterations);
        let best = match cache.get(&key) {
            Some(best) if !args.retune => {
                println!("[tune] cpu (cached): {key}\n[tune]   -> {best}");
                best
            }
            _ => {
                let choice = autotune::tune_cpu(v, threads, args.iterations, &id);
                choice.print("cpu");
                cache.put(&choice);
                report.tune.push(serde_json::to_value(&choice).unwrap());
                choice.best
            }
        };
        if args.confirm {
            let kind = CpuKind::ALL
                .into_iter()
                .find(|k| k.name() == best)
                .expect("cached choice names a known implementation");
            let cpt =
                (args.chunks_per_thread.max(4) as usize).div_ceil(kind.batch()) * kind.batch();
            let tp = cpu::throughput(kind, v, threads, cpt, args.iterations, &id);
            println!(
                "[tune]   confirm {best}/{} at {} iterations: {:.1} chunks/s ({} chunks, {threads} threads)",
                v.name(),
                args.iterations,
                tp.chunks_per_s(),
                tp.chunks
            );
        }
        #[cfg(feature = "cuda")]
        if let Err(e) = tune_gpu(args, v, &mut cache, report) {
            report.gpu_error = Some(e.to_string());
        }
    }
    match cache.save(&path) {
        Ok(()) => println!("[tune] cache: {}", path.display()),
        Err(e) => eprintln!("[tune] could not write cache {}: {e}", path.display()),
    }
}

#[cfg(feature = "cuda")]
fn tune_gpu(
    args: &Args,
    v: Variant,
    cache: &mut packing_variant_bench::autotune::Cache,
    report: &mut Report,
) -> Result<(), packing_variant_bench::gpu::CudaError> {
    use packing_variant_bench::{autotune, gpu};

    gpu::set_chunk_id_prefix(&fixed_id(OFFSET_START))?;
    let id = |k: u64| fixed_id(k);
    let key = autotune::gpu_key(&gpu::device_info()?, v, args.iterations, args.slice);
    let cached = cache.get(&key).filter(|_| !args.retune);
    let (best, configs) = match cached {
        Some(best) => {
            println!("[tune] gpu (cached): {key}\n[tune]   -> {best}");
            (best, None)
        }
        None => {
            let (choice, configs) =
                autotune::tune_gpu(v, args.iterations, OFFSET_START, args.slice, &id)?;
            choice.print("gpu");
            cache.put(&choice);
            report.tune.push(serde_json::to_value(&choice).unwrap());
            (choice.best, Some(configs))
        }
    };
    if args.confirm {
        let Some(cfg) = configs.and_then(|c| c.into_iter().next()) else {
            println!("[tune]   confirm: rerun with --retune to time the cached GPU choice");
            return Ok(());
        };
        let mut d = gpu::DeviceChunks::new(cfg.batch)?;
        let ms = d.run(
            cfg.kernel,
            v,
            OFFSET_START,
            args.iterations,
            args.slice,
            cfg.blocks,
            cfg.threads_per_block,
        )? as f64;
        let (mut got, mut want) = (new_chunk(), new_chunk());
        d.copy_chunk(cfg.kernel, 0, &mut got)?;
        packing_variant_bench::entropy(v, &fixed_id(OFFSET_START), args.iterations, &mut want);
        println!(
            "[tune]   confirm {best} variant {} at {} iterations: {:.1} chunks/s, verify[1]={}",
            v.name(),
            args.iterations,
            cfg.batch as f64 / (ms / 1e3),
            if got[..] == want[..] { "PASS" } else { "FAIL" }
        );
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// GPU

#[cfg(not(feature = "cuda"))]
fn run_gpu(_args: &Args, report: &mut Report) {
    let msg = "built without the `cuda` feature; rebuild with --features cuda".to_string();
    eprintln!("[gpu] {msg}");
    report.gpu_error = Some(msg);
}

#[cfg(feature = "cuda")]
fn run_gpu(args: &Args, report: &mut Report) {
    if let Err(e) = run_gpu_inner(args, report) {
        report.gpu_error = Some(e.to_string());
    }
}

#[cfg(feature = "cuda")]
fn run_gpu_inner(
    args: &Args,
    report: &mut Report,
) -> Result<(), packing_variant_bench::gpu::CudaError> {
    use packing_variant_bench::gpu;

    let dev = gpu::device_info()?;
    println!(
        "[gpu] SMs={} cc={} max_threads/SM={} L2={} MiB mem free/total={:.1}/{:.1} GiB",
        dev.sm_count,
        dev.compute_capability,
        dev.max_threads_per_sm,
        dev.l2_bytes >> 20,
        dev.mem_free_bytes as f64 / (1u64 << 30) as f64,
        dev.mem_total_bytes as f64 / (1u64 << 30) as f64,
    );
    let mut kinfo = serde_json::Map::new();
    for &kern in &args.kernel {
        for &v in &args.variants {
            let k = gpu::kernel_info(kern, v)?;
            println!(
                "[gpu] kernel {} {}: {} registers/thread, {} B local/thread",
                kern.name(),
                v.name(),
                k.registers,
                k.local_bytes
            );
            kinfo.insert(
                format!("{}/{}", kern.name(), v.name()),
                serde_json::to_value(k).unwrap(),
            );
        }
    }
    report.gpu_device = Some(serde_json::json!({ "device": dev, "kernels": kinfo }));

    let id = fixed_id(OFFSET_START);
    gpu::set_chunk_id_prefix(&id)?;

    // Warm-up launch (context creation, module load) outside of any timed region.
    {
        let mut d = gpu::DeviceChunks::new(64)?;
        for &kern in &args.kernel {
            for &v in &args.variants {
                d.run(kern, v, OFFSET_START, 16_384, args.slice, 1, 64)?;
            }
        }
    }

    let mut host = new_chunk();
    let mut expect = new_chunk();
    for &batch in &args.batch {
        println!(
            "[gpu] batch={batch}: device buffer {:.2} GiB (host RAM used: {} x 256 KiB verify chunks)",
            (batch as f64 * CHUNK_SIZE as f64) / (1u64 << 30) as f64,
            args.verify.min(batch as u32),
        );
        let mut d = gpu::DeviceChunks::new(batch)?;
        for &kern in &args.kernel {
            let samples = if kern.verifiable() {
                sample_indices(batch, args.verify)
            } else {
                vec![]
            };
            for &tpb in &args.tpb {
                let blocks = args.blocks.unwrap_or(batch.div_ceil(tpb as u64) as u32);
                for &v in &args.variants {
                    let occ = gpu::occupancy_blocks_per_sm(kern, v, tpb)?;
                    let resident = (occ as u64 * dev.sm_count as u64 * tpb as u64)
                        .min(blocks as u64 * tpb as u64);
                    let host_start = Instant::now();
                    let ms = d.run(
                        kern,
                        v,
                        OFFSET_START,
                        args.iterations,
                        args.slice,
                        blocks,
                        tpb,
                    )? as f64;
                    let host_ms = host_start.elapsed().as_secs_f64() * 1e3;
                    let cps = batch as f64 / (ms / 1e3);
                    // The host time also covers launch and synchronisation overhead, so allow a
                    // small absolute slack on top of the relative tolerance.
                    let timing_ok = ms > 0.0 && (host_ms - ms).abs() <= 0.05 * host_ms + 50.0;
                    if !timing_ok {
                        eprintln!(
                            "[gpu] TIMING ANOMALY kernel {} variant {}: event {ms:.1} ms vs host {host_ms:.1} ms",
                            kern.name(),
                            v.name()
                        );
                    }

                    let mut pass = true;
                    for &idx in &samples {
                        d.copy_chunk(kern, idx, &mut host)?;
                        let cid = ChunkId {
                            partition_offset: OFFSET_START + idx,
                            ..id
                        };
                        packing_variant_bench::entropy(v, &cid, args.iterations, &mut expect);
                        if host[..] != expect[..] {
                            pass = false;
                            eprintln!(
                                "[gpu] MISMATCH kernel {} variant {} chunk {idx}",
                                kern.name(),
                                v.name()
                            );
                        }
                    }
                    println!(
                        "[gpu] {:<16} {} tpb={tpb:<3} batch={batch:<6} blocks={blocks:<4} occ={occ} blk/SM resident={resident:<6} kernel={ms:>10.1} ms  {cps:>9.1} chunks/s  verify[{}]={}",
                        kern.name(),
                        v.name(),
                        samples.len(),
                        verdict(pass, samples.is_empty())
                    );
                    report.gpu.push(GpuResult {
                        kernel: kern.name().to_string(),
                        variant: v.name().to_string(),
                        threads_per_block: tpb,
                        batch,
                        blocks,
                        blocks_per_sm_occupancy: occ,
                        resident_threads: resident,
                        kernel_ms: ms,
                        host_ms,
                        timing_ok,
                        chunks_per_s: cps,
                        verified_chunks: samples.clone(),
                        verify_pass: pass,
                    });
                }
            }
        }
    }
    Ok(())
}

/// First, last and evenly spaced chunk indices in between.
#[cfg(feature = "cuda")]
fn sample_indices(batch: u64, n: u32) -> Vec<u64> {
    let n = (n as u64).min(batch);
    if n == 0 {
        return vec![];
    }
    if n == 1 {
        return vec![0];
    }
    let mut v: Vec<u64> = (0..n).map(|k| k * (batch - 1) / (n - 1)).collect();
    v.dedup();
    v
}

// ------------------------------------------------------------------------------------------
// Summary

fn print_summary(r: &Report) {
    println!("\n=== Summary (iterations = {}) ===", r.iterations);
    if !r.cpu.is_empty() {
        println!(
            "{:<10} {:<4} {:>14} {:>18}",
            "CPU impl", "var", "1T s/chunk", "all-threads ch/s"
        );
        for c in &r.cpu {
            println!(
                "{:<10} {:<4} {:>14.4} {:>18.2}",
                c.implementation,
                c.variant,
                c.single_thread_s_per_chunk,
                c.all_threads_chunks_per_s
            );
        }
    }
    if !r.gpu.is_empty() {
        println!(
            "\n{:<16} {:<4} {:>5} {:>7} {:>6} {:>12} {:>10} {:>7}",
            "GPU kernel", "var", "tpb", "batch", "blocks", "kernel ms", "chunks/s", "verify"
        );
        for g in &r.gpu {
            println!(
                "{:<16} {:<4} {:>5} {:>7} {:>6} {:>12.1} {:>10.1} {:>7}",
                g.kernel,
                g.variant,
                g.threads_per_block,
                g.batch,
                g.blocks,
                g.kernel_ms,
                g.chunks_per_s,
                if g.timing_ok {
                    verdict(g.verify_pass, g.verified_chunks.is_empty())
                } else {
                    "TIMING"
                }
            );
        }
    }
    if let Some(e) = &r.gpu_error {
        println!("\nGPU: {e}");
    }

    let cpu = |i: &str, v: &str| {
        r.cpu
            .iter()
            .find(|c| c.implementation == i && c.variant == v)
    };
    let mut cpu_impls: Vec<&str> = vec![];
    for c in &r.cpu {
        if !cpu_impls.contains(&c.implementation.as_str()) {
            cpu_impls.push(&c.implementation);
        }
    }
    let best_gpu = |k: Option<&str>, v: &str| {
        r.gpu
            .iter()
            .filter(|g| g.variant == v && k.is_none_or(|k| g.kernel == k))
            .filter(|g| g.timing_ok && g.verify_pass)
            // across kernels, only those whose output was checked (excludes diagnostics)
            .filter(|g| k.is_some() || !g.verified_chunks.is_empty())
            .max_by(|a, b| a.chunks_per_s.total_cmp(&b.chunks_per_s))
    };
    let mut kernels: Vec<&str> = vec![];
    for g in &r.gpu {
        if !kernels.contains(&g.kernel.as_str()) {
            kernels.push(&g.kernel);
        }
    }
    println!();
    for k in kernels.iter().map(|k| Some(*k)).chain([None]) {
        if let (Some(a), Some(b)) = (best_gpu(k, "A"), best_gpu(k, "B")) {
            println!(
                "B vs A on GPU [{}] (best config each): {:+.2}% throughput",
                k.unwrap_or("best verified kernel"),
                (b.chunks_per_s / a.chunks_per_s - 1.0) * 100.0
            );
        }
    }
    for v in ["A", "B"] {
        if let Some(g) = best_gpu(None, v) {
            println!(
                "best GPU {v}: {:.1} chunks/s ({}, tpb={}, batch={})",
                g.chunks_per_s, g.kernel, g.threads_per_block, g.batch
            );
            for i in &cpu_impls {
                if let Some(c) = cpu(i, v) {
                    println!(
                        "  GPU/CPU({i}, all-threads) = {:.2}x",
                        g.chunks_per_s / c.all_threads_chunks_per_s
                    );
                }
            }
        }
    }
    for i in &cpu_impls {
        if let (Some(a), Some(b)) = (cpu(i, "A"), cpu(i, "B")) {
            println!(
                "B vs A on CPU [{i}]: single-thread {:+.2}% time, all-threads {:+.2}% throughput",
                (b.single_thread_s_per_chunk / a.single_thread_s_per_chunk - 1.0) * 100.0,
                (b.all_threads_chunks_per_s / a.all_threads_chunks_per_s - 1.0) * 100.0
            );
        }
    }
}

fn verdict(pass: bool, nothing_checked: bool) -> &'static str {
    match (nothing_checked, pass) {
        (true, _) => "n/a",
        (false, true) => "PASS",
        (false, false) => "FAIL",
    }
}
