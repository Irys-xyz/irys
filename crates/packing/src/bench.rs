use irys_packing::capacity_pack_range_cuda_c;
use irys_types::{ConsensusConfig, H256};
use rand::Rng as _;
use std::time::*;

#[cfg(feature = "nvidia")]
pub fn main() {
    use irys_packing::CUDAConfig;
    use irys_types::IrysAddress;

    // std::env::set_var("JDBG_BLOCKS", "40"); // 3090: 82, 5090: 170
    // std::env::set_var("JDBG_THREADS", "128");

    // std::env::set_var("OPENSSL_ia32cap", "~0x200000200000000");

    let mut testing_config = ConsensusConfig::testing();
    testing_config.chunk_size = ConsensusConfig::CHUNK_SIZE;
    testing_config.entropy_packing_iterations = 1_000_000;

    let counts = [
        1, 100, 500, 1000, 1500, 2000, 2500, 3000, 4000, 5000, 6000, 7000, 8000, 9000, 10000,
        10286, // 3090 CUDA core count
        11000, 12000, 13000, 14000, 15000, 16000, 17000, 18000, 19000, 20000, 21000,
        21760, // 5090 CUDA core count
    ];

    let runs_per = 2;
    let mut out = counts.map(|c| (c, vec![]));
    let device_config = CUDAConfig::from_device_default().unwrap();
    for (count, times) in &mut out {
        let mut total_elapsed = Duration::ZERO;

        for run in 0..runs_per {
            let mut rng = rand::thread_rng();
            let mining_address = Address::random();
            let chunk_offset = rng.gen_range(1..=1000);
            let partition_hash = H256::random();

            let count = *count;
            // let num_chunks: usize = 8192;
            let num_chunks = count;

            let mut entropy: Vec<u8> =
                Vec::with_capacity(num_chunks * ConsensusConfig::CHUNK_SIZE as usize);

            let now = Instant::now();

            capacity_pack_range_cuda_c(
                num_chunks as u32,
                mining_address,
                chunk_offset,
                partition_hash,
                testing_config.entropy_packing_iterations,
                testing_config.chain_id,
                device_config.clone(),
                &mut entropy,
            )
            .unwrap();

            let elapsed = now.elapsed();
            total_elapsed += elapsed;
            println!(
                "size {} run {} C CUDA implementation: {:.2?}",
                count, run, elapsed
            );

            times.push(elapsed);
            // let chunked = entropy.chunks(ConsensusConfig::CHUNK_SIZE as usize);
            let mut computed = Vec::with_capacity(ConsensusConfig::CHUNK_SIZE as usize);

            for _i in 0..10 {
                use irys_c::capacity_single::compute_entropy_chunk;

                let pos = rng.gen_range(0..num_chunks) as u64;
                let start = pos * ConsensusConfig::CHUNK_SIZE;
                let end = (pos + 1) * ConsensusConfig::CHUNK_SIZE;
                let slice = &entropy[start as usize..end as usize];
                compute_entropy_chunk(
                    mining_address,
                    chunk_offset + pos,
                    partition_hash.0,
                    testing_config.entropy_packing_iterations,
                    ConsensusConfig::CHUNK_SIZE as usize,
                    &mut computed,
                    testing_config.chain_id,
                );
                if slice != &computed[..] {
                    eprintln!(
                        "chunk mismatch! {} {} {} {} {} {}",
                        &pos,
                        chunk_offset + pos,
                        &start,
                        &end,
                        &slice.len(),
                        &computed.len()
                    )
                }
            }
        }
    }

    println!("RESULTS");
    for (count, times) in out {
        println!(
            "{} {:?} {}ms",
            &count,
            &times
                .iter()
                .map(std::time::Duration::as_millis)
                .collect::<Vec<_>>(),
            (times.iter().fold(Duration::ZERO, |acc, v| { acc + *v }) / runs_per).as_millis()
        )
    }
}
