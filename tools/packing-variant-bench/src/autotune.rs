//! Picks the fastest packing implementation for this machine with short calibration runs, and
//! caches the choice per hardware.
//!
//! Which path wins depends on the machine, not just on which instructions it has: e.g. AVX-512
//! that is split into 256-bit halves, a small L3 that B's scratch overflows, SMT, or a GPU's power
//! limit. So every available candidate is timed on this machine, in the configuration it would run
//! in (all worker threads, or the full GPU launch shape).
//!
//! Calibration uses fewer iterations than a real chunk, so each candidate is timed at two iteration
//! counts and the time per chunk is fitted as `fixed + per_step * iterations`. Ranking by the
//! throughput this predicts at the real iteration count keeps fixed per-chunk costs (Phase 1, a
//! final layout transpose) from being over-weighted, which a single short run does: it ranked
//! SHA-NI x4 at 73% of AVX-512 x16 on a 9950X3D, where full-length runs give 55%.

use crate::{
    ChunkId, Variant,
    cpu::{self, CpuKind},
};
use serde::Serialize;
use std::{collections::BTreeMap, path::Path};

/// The two calibration lengths: Phase 1 plus 48k and 96k Phase 2 steps. Shorter runs are not on
/// the line yet: on a 9950X3D, AVX-512 A at 24k iterations costs ~25% more per chunk than the
/// fit through longer runs predicts (Phase 1 and the final per-group transpose dominate), which
/// made a 24k/56k fit overstate it at 1M iterations by ~25%.
pub const CALIBRATION_ITERATIONS: [u32; 2] = [8192 + 48_000, 8192 + 96_000];

/// Throughput predicted at `target` iterations from `chunks` timed at the two calibration lengths.
fn predict(chunks: u64, secs: [f64; 2], target: u32) -> f64 {
    let [i0, i1] = CALIBRATION_ITERATIONS.map(f64::from);
    let per_chunk = secs.map(|t| t / chunks as f64);
    let slope = (per_chunk[1] - per_chunk[0]) / (i1 - i0);
    let t = if slope > 0.0 {
        per_chunk[1] + slope * (target as f64 - i1)
    } else {
        // noise made the longer run no slower: fall back to proportional scaling
        per_chunk[1] * target as f64 / i1
    };
    1.0 / t
}

/// A calibration run shorter than this is repeated with more work, to keep timer and thread
/// start-up noise small.
const MIN_CALIBRATION_S: f64 = 0.25;

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub name: String,
    /// Throughput predicted at `Choice::target_iterations` from the two calibration runs.
    pub predicted_chunks_per_s: f64,
    pub note: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Choice {
    pub key: String,
    pub best: String,
    pub target_iterations: u32,
    pub calibration_iterations: [u32; 2],
    /// Fastest first.
    pub candidates: Vec<Candidate>,
}

impl Choice {
    fn sort(&mut self) {
        self.candidates.sort_by(|a, b| {
            b.predicted_chunks_per_s
                .total_cmp(&a.predicted_chunks_per_s)
        });
        self.best = self
            .candidates
            .first()
            .map(|c| c.name.clone())
            .unwrap_or_default();
    }

    pub fn print(&self, what: &str) {
        println!("[tune] {what}: {}", self.key);
        let Some(top) = self.candidates.first().map(|c| c.predicted_chunks_per_s) else {
            println!("[tune]   no candidate ran");
            return;
        };
        for c in &self.candidates {
            println!(
                "[tune]   {:<44} {:>9.1} chunks/s predicted at {} it  {:>5.1}%  {}",
                c.name,
                c.predicted_chunks_per_s,
                self.target_iterations,
                c.predicted_chunks_per_s / top * 100.0,
                c.note
            );
        }
        println!("[tune]   -> {}", self.best);
    }
}

/// x86 CPUID brand string, e.g. "AMD Ryzen 9 9950X3D 16-Core Processor".
pub fn cpu_brand() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::__cpuid;
        // leaf 0x80000000 reports the highest extended leaf; the brand string is leaves 2..=4
        let max_ext = __cpuid(0x8000_0000).eax;
        if max_ext >= 0x8000_0004 {
            let mut bytes = Vec::with_capacity(48);
            for leaf in 0x8000_0002..=0x8000_0004 {
                let r = __cpuid(leaf);
                for w in [r.eax, r.ebx, r.ecx, r.edx] {
                    bytes.extend_from_slice(&w.to_le_bytes());
                }
            }
            return String::from_utf8_lossy(&bytes)
                .trim_matches(char::from(0))
                .trim()
                .to_string();
        }
    }
    "unknown-cpu".to_string()
}

pub fn cpu_key(v: Variant, threads: usize) -> String {
    let avail: Vec<&str> = CpuKind::ALL
        .iter()
        .filter(|k| k.available())
        .map(|k| k.name())
        .collect();
    format!(
        "cpu | {} | threads={threads} | variant={} | available={} | PVB_CPU_DISABLE={}",
        cpu_brand(),
        v.name(),
        avail.join(","),
        cpu::disabled_features()
    )
}

/// Times every available CPU implementation for `v` on `threads` workers.
pub fn tune_cpu(
    v: Variant,
    threads: usize,
    target_iterations: u32,
    id: &(dyn Fn(u64) -> ChunkId + Sync),
) -> Choice {
    let mut choice = Choice {
        key: cpu_key(v, threads),
        best: String::new(),
        target_iterations,
        calibration_iterations: CALIBRATION_ITERATIONS,
        candidates: vec![],
    };
    let [short, long] = CALIBRATION_ITERATIONS;
    for kind in CpuKind::ALL {
        // irys-c is the production reference, not a candidate distinct from openssl
        if kind == CpuKind::IrysC || !kind.available() || !kind.supports(v) {
            continue;
        }
        // several batches per thread so the work queue can balance
        let mut cpt = 4 * kind.batch();
        let mut t0 = cpu::throughput(kind, v, threads, cpt, short, id);
        if t0.wall_s < MIN_CALIBRATION_S {
            let scale = (MIN_CALIBRATION_S / t0.wall_s).ceil() as usize;
            cpt *= scale;
            t0 = cpu::throughput(kind, v, threads, cpt, short, id);
        }
        let t1 = cpu::throughput(kind, v, threads, cpt, long, id);
        choice.candidates.push(Candidate {
            name: kind.name().to_string(),
            predicted_chunks_per_s: predict(t0.chunks, [t0.wall_s, t1.wall_s], target_iterations),
            note: format!(
                "{} chunks: {:.2} s / {:.2} s",
                t0.chunks, t0.wall_s, t1.wall_s
            ),
        });
    }
    choice.sort();
    choice
}

/// GPU launch configuration chosen by [`tune_gpu`].
#[cfg(feature = "cuda")]
#[derive(Clone, Copy, Debug, Serialize)]
pub struct GpuConfig {
    pub kernel: crate::gpu::Kernel,
    pub threads_per_block: u32,
    pub blocks: u32,
    pub batch: u64,
}

#[cfg(feature = "cuda")]
impl GpuConfig {
    pub fn name(&self) -> String {
        format!(
            "{} tpb={} blocks={} batch={}",
            self.kernel.name(),
            self.threads_per_block,
            self.blocks,
            self.batch
        )
    }
}

#[cfg(feature = "cuda")]
pub fn gpu_key(dev: &crate::gpu::DeviceInfo, v: Variant) -> String {
    format!(
        "gpu | sm={} cc={} mem={}MiB driver={} | variant={}",
        dev.sm_count,
        dev.compute_capability,
        dev.mem_total_bytes >> 20,
        dev.driver_version,
        v.name()
    )
}

/// Times the fast GPU kernels over launch shapes of whole waves (blocks = SMs x waves) that fit
/// in free device memory. Each candidate's first chunk is verified against the CPU; a candidate
/// that fails is dropped.
#[cfg(feature = "cuda")]
pub fn tune_gpu(
    v: Variant,
    target_iterations: u32,
    offset_start: u64,
    slice: u32,
    id: &dyn Fn(u64) -> ChunkId,
) -> Result<(Choice, Vec<GpuConfig>), crate::gpu::CudaError> {
    use crate::{CHUNK_SIZE, entropy, gpu, new_chunk};

    let dev = gpu::device_info()?;
    // leave headroom for the context and other users of the GPU
    let max_chunks = (dev.mem_free_bytes as f64 * 0.9) as u64 / CHUNK_SIZE as u64;
    let sms = dev.sm_count as u64;
    let mut configs = vec![];
    for kernel in [gpu::Kernel::FastInterleaved, gpu::Kernel::FastContig] {
        for tpb in [128_u32, 256, 512] {
            for waves in 1..=8_u64 {
                let blocks = sms * waves;
                let batch = blocks * tpb as u64;
                if batch > max_chunks {
                    break;
                }
                configs.push(GpuConfig {
                    kernel,
                    threads_per_block: tpb,
                    blocks: blocks as u32,
                    batch,
                });
            }
        }
    }

    let mut choice = Choice {
        key: gpu_key(&dev, v),
        best: String::new(),
        target_iterations,
        calibration_iterations: CALIBRATION_ITERATIONS,
        candidates: vec![],
    };
    let [short, long] = CALIBRATION_ITERATIONS;
    let (mut got, mut want) = (new_chunk(), new_chunk());
    entropy(v, &id(offset_start), long, &mut want);
    let mut kept = vec![];
    for cfg in configs {
        let mut d = gpu::DeviceChunks::new(cfg.batch)?;
        let mut ms = [0.0; 2];
        for (k, it) in [short, long].into_iter().enumerate() {
            let (blocks, tpb) = (cfg.blocks, cfg.threads_per_block);
            ms[k] = d.run(cfg.kernel, v, offset_start, it, slice, blocks, tpb)? as f64;
        }
        // the long run's output is still in the buffer
        d.copy_chunk(cfg.kernel, 0, &mut got)?;
        if ms.iter().any(|&m| m <= 0.0) || got[..] != want[..] {
            eprintln!("[tune] {} dropped: wrong output or timing", cfg.name());
            continue;
        }
        choice.candidates.push(Candidate {
            name: cfg.name(),
            predicted_chunks_per_s: predict(cfg.batch, ms.map(|m| m / 1e3), target_iterations),
            note: format!("{:.0} ms / {:.0} ms", ms[0], ms[1]),
        });
        kept.push(cfg);
    }
    if choice.candidates.is_empty() {
        return Err(gpu::CudaError {
            what: "GPU auto-tune",
            code: -1,
            message: "no launch shape fits in free VRAM or produced correct output".to_string(),
        });
    }
    choice.sort();
    // same order as the candidates
    kept.sort_by_key(|c| {
        choice
            .candidates
            .iter()
            .position(|cand| cand.name == c.name())
    });
    Ok((choice, kept))
}

/// Choices cached on disk by hardware key, so calibration runs once per machine (and again when
/// the CPU, GPU, driver, thread count or `PVB_CPU_DISABLE` changes).
pub struct Cache {
    entries: BTreeMap<String, serde_json::Value>,
}

impl Cache {
    pub fn load(path: &Path) -> Self {
        let entries = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self { entries }
    }

    pub fn get(&self, key: &str) -> Option<String> {
        self.entries
            .get(key)?
            .get("best")?
            .as_str()
            .map(str::to_string)
    }

    pub fn put(&mut self, choice: &Choice) {
        self.entries.insert(
            choice.key.clone(),
            serde_json::to_value(choice).expect("serialisable"),
        );
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&self.entries)?)
    }
}

/// `$XDG_CACHE_HOME/packing-variant-bench/tune.json`, falling back to `~/.cache/...`.
pub fn default_cache_path() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".cache")))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("packing-variant-bench").join("tune.json")
}
