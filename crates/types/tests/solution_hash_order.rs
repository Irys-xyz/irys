//! `compute_solution_hash` input order before and after Delta.

use irys_types::{
    H256, UnixTimestamp, compute_solution_hash,
    hardfork_config::{Activation, Delta},
};
use sha2::{Digest as _, Sha256};

fn sha256_of(parts: &[&[u8]]) -> H256 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    H256::from(<[u8; 32]>::from(hasher.finalize()))
}

#[test]
fn solution_hash_order_follows_delta() {
    let chunk = vec![0xA5_u8; 256 * 1024];
    let offset: u32 = 0x0102_0304;
    let seed = H256::from([7_u8; 32]);
    let delta = Delta {
        activation_timestamp: UnixTimestamp::from_secs(0),
        initial_slots_per_new_ledger: Delta::default_initial_slots_per_new_ledger(),
        capacity_chunk_reward_percent: Delta::default_capacity_chunk_reward_percent(),
    };

    assert_eq!(
        compute_solution_hash(&chunk, offset, &seed, Activation::Inactive),
        sha256_of(&[&chunk, &offset.to_le_bytes(), seed.as_bytes()])
    );
    assert_eq!(
        compute_solution_hash(&chunk, offset, &seed, Activation::Active(delta)),
        sha256_of(&[seed.as_bytes(), &offset.to_le_bytes(), &chunk])
    );
}
