//! Time and resident memory for the chunk-level tree of a maximum upload.
//!
//! `Geometry::MAINNET` allows `max_data_tx_chunks` chunks, 5 TiB at the mainnet
//! chunk size (spec §2). Hashing those chunk bytes dominates a real upload
//! (spec §8). This example pushes synthetic chunk roots, so it times only the
//! chunk-level tree. `with_capacity` reserves each layer at its final
//! `build_layer` length, carried node included (spec §4).

use std::time::Instant;

use irys_data_tree::{DataTreeBuilder, Geometry, HASH_SIZE, NodeHash};

fn main() {
    let geometry = Geometry::MAINNET;
    let data_size = geometry.max_data_size();
    let chunk_count = geometry.max_data_tx_chunks();
    let chunk_length = geometry.chunk_size();

    let started = Instant::now();
    let mut builder = DataTreeBuilder::with_capacity(geometry, data_size)
        .expect("the geometry maximum is a valid data size");
    for chunk_index in 0..chunk_count {
        builder
            .push_chunk_root(synthetic_chunk_root(chunk_index), chunk_length)
            .expect("each chunk is a full chunk inside the maximum");
    }
    let tree = builder.finish().expect("the upload has chunks");
    let elapsed = started.elapsed();

    // Read while `tree` is still live. The prints below are a later use, so
    // the node cache is resident here.
    #[cfg(target_os = "linux")]
    let (vm_rss_kib, vm_hwm_kib) = resident_kib();

    // Layer `l` stores `ceil(C / 2^l)` hashes, from the chunk leaves through the
    // root. A carried node is copied into each layer it passes through, so the
    // stored count is that sum and a proof lookup is a plain index (spec §4).
    // The loop is that rule; it is not a closed form. At 5 TiB the sum is
    // 41,943,041 hashes, 1,342,177,312 bytes (spec §4, §8).
    let mut layer_length = u64::from(tree.chunk_count());
    let mut node_count = layer_length;
    while layer_length > 1 {
        layer_length = layer_length.div_ceil(2);
        node_count += layer_length;
    }
    let node_cache_bytes = node_count * u64::try_from(HASH_SIZE).expect("hash width fits u64");

    println!("chunk_count={}", tree.chunk_count());
    println!("elapsed_ms={}", elapsed.as_millis());
    println!("data_tree_root={}", tree.data_tree_root());
    println!("node_cache_bytes={node_cache_bytes}");
    #[cfg(target_os = "linux")]
    {
        println!("VmRSS_kB={vm_rss_kib}");
        println!("VmHWM_kB={vm_hwm_kib}");
    }
}

fn synthetic_chunk_root(chunk_index: u32) -> NodeHash {
    let mut bytes = [0_u8; HASH_SIZE];
    let chunk_index_bytes = chunk_index.to_be_bytes();
    bytes[..chunk_index_bytes.len()].copy_from_slice(&chunk_index_bytes);
    NodeHash::from(bytes)
}

/// `VmRSS` and `VmHWM` from `/proc/self/status`, in KiB.
#[cfg(target_os = "linux")]
fn resident_kib() -> (u64, u64) {
    let status = std::fs::read_to_string("/proc/self/status").expect("read /proc/self/status");
    (proc_kib(&status, "VmRSS"), proc_kib(&status, "VmHWM"))
}

#[cfg(target_os = "linux")]
fn proc_kib(status: &str, key: &str) -> u64 {
    let prefix = format!("{key}:");
    for line in status.lines() {
        let Some(rest) = line.strip_prefix(&prefix) else {
            continue;
        };
        let Some(number) = rest.split_whitespace().next() else {
            continue;
        };
        return number
            .parse()
            .unwrap_or_else(|_| panic!("{key} is not an integer"));
    }
    panic!("missing {key} in /proc/self/status");
}
