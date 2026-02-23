use crate::kzg::{verify_chunk_opening_proof, KzgCommitmentBytes, PROOF_SIZE, SCALAR_SIZE};
use crate::{IrysAddress, H256};
use alloy_primitives::FixedBytes;
use c_kzg::KzgSettings;
use openssl::sha;
use serde::{Deserialize, Serialize};

/// A custody challenge targeting a specific miner's partition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyChallenge {
    pub challenged_miner: IrysAddress,
    pub partition_hash: H256,
    pub challenge_seed: H256,
    pub challenge_block_height: u64,
}

/// A single KZG opening for one challenged chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyOpening {
    /// Partition-relative chunk offset
    pub chunk_offset: u32,
    /// Which data_root owns this chunk
    pub data_root: H256,
    /// Chunk index within the data_root's transaction
    pub tx_chunk_index: u32,
    /// Evaluation point z (32-byte BLS12-381 scalar)
    pub evaluation_point: FixedBytes<SCALAR_SIZE>,
    /// Evaluation value y = p(z) (32-byte BLS12-381 scalar)
    pub evaluation_value: FixedBytes<SCALAR_SIZE>,
    /// Opening proof pi (48-byte compressed G1 point)
    pub opening_proof: FixedBytes<PROOF_SIZE>,
}

/// A custody proof responding to a challenge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustodyProof {
    pub challenged_miner: IrysAddress,
    pub partition_hash: H256,
    pub challenge_seed: H256,
    pub openings: Vec<CustodyOpening>,
}

/// Result of verifying a custody proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustodyVerificationResult {
    Valid,
    InvalidOpeningCount { expected: u32, got: u32 },
    MissingCommitment { data_root: H256, chunk_index: u32 },
    InvalidProof { chunk_offset: u32 },
}

/// Derive challenge seed from VDF output and partition hash.
///
/// `challenge_seed = SHA256(vdf_output || partition_hash)`
pub fn derive_challenge_seed(vdf_output: &[u8; 32], partition_hash: &H256) -> H256 {
    let mut hasher = sha::Sha256::new();
    hasher.update(vdf_output);
    hasher.update(&partition_hash.0);
    H256(hasher.finish())
}

/// Select K challenged chunk offsets from a challenge seed.
///
/// `offset_j = hash_to_u64(SHA256(challenge_seed || j_le)) % num_chunks`
pub fn select_challenged_offsets(
    challenge_seed: &H256,
    k: u32,
    num_chunks_in_partition: u64,
) -> Vec<u32> {
    (0..k)
        .map(|j| {
            let mut hasher = sha::Sha256::new();
            hasher.update(&challenge_seed.0);
            hasher.update(&j.to_le_bytes());
            let hash = hasher.finish();

            // Interpret first 8 bytes as little-endian u64
            let val = u64::from_le_bytes(
                hash[..8]
                    .try_into()
                    .expect("SHA256 output is at least 8 bytes"),
            );
            // Safe truncation: partition offsets fit in u32
            u32::try_from(val % num_chunks_in_partition)
                .expect("num_chunks_in_partition fits in u32 range for offset")
        })
        .collect()
}

/// Verify all openings in a custody proof against stored per-chunk commitments.
///
/// `get_commitment` retrieves the KZG commitment for a given (data_root, chunk_index)
/// from the database. Returns `Ok(None)` if the commitment is not found.
pub fn verify_custody_proof(
    proof: &CustodyProof,
    get_commitment: impl Fn(H256, u32) -> eyre::Result<Option<KzgCommitmentBytes>>,
    kzg_settings: &KzgSettings,
    expected_challenge_count: u32,
) -> eyre::Result<CustodyVerificationResult> {
    let got = u32::try_from(proof.openings.len())
        .map_err(|_| eyre::eyre!("opening count exceeds u32"))?;
    if got != expected_challenge_count {
        return Ok(CustodyVerificationResult::InvalidOpeningCount {
            expected: expected_challenge_count,
            got,
        });
    }

    for opening in &proof.openings {
        let commitment = match get_commitment(opening.data_root, opening.tx_chunk_index)? {
            Some(c) => c,
            None => {
                return Ok(CustodyVerificationResult::MissingCommitment {
                    data_root: opening.data_root,
                    chunk_index: opening.tx_chunk_index,
                });
            }
        };

        let valid = verify_chunk_opening_proof(
            &commitment,
            opening.evaluation_point.as_ref(),
            opening.evaluation_value.as_ref(),
            opening.opening_proof.as_ref(),
            kzg_settings,
        )?;

        if !valid {
            return Ok(CustodyVerificationResult::InvalidProof {
                chunk_offset: opening.chunk_offset,
            });
        }
    }

    Ok(CustodyVerificationResult::Valid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kzg::{
        compute_chunk_commitment, compute_chunk_opening_proof, default_kzg_settings,
        derive_challenge_point, CHUNK_SIZE_FOR_KZG, COMMITMENT_SIZE,
    };

    #[test]
    fn derive_challenge_seed_deterministic() {
        let vdf = [42_u8; 32];
        let partition = H256::from([7_u8; 32]);
        let s1 = derive_challenge_seed(&vdf, &partition);
        let s2 = derive_challenge_seed(&vdf, &partition);
        assert_eq!(s1, s2);
    }

    #[test]
    fn derive_challenge_seed_different_inputs() {
        let vdf_a = [1_u8; 32];
        let vdf_b = [2_u8; 32];
        let partition = H256::from([7_u8; 32]);
        let s1 = derive_challenge_seed(&vdf_a, &partition);
        let s2 = derive_challenge_seed(&vdf_b, &partition);
        assert_ne!(s1, s2);
    }

    #[test]
    fn select_challenged_offsets_returns_k() {
        let seed = H256::from([42_u8; 32]);
        let offsets = select_challenged_offsets(&seed, 20, 1000);
        assert_eq!(offsets.len(), 20);
    }

    #[test]
    fn select_challenged_offsets_within_bounds() {
        let seed = H256::from([42_u8; 32]);
        let num_chunks = 500_u64;
        let offsets = select_challenged_offsets(&seed, 20, num_chunks);
        for &offset in &offsets {
            assert!(u64::from(offset) < num_chunks);
        }
    }

    #[test]
    fn select_challenged_offsets_different_seeds() {
        let offsets_a = select_challenged_offsets(&H256::from([1_u8; 32]), 20, 10_000);
        let offsets_b = select_challenged_offsets(&H256::from([2_u8; 32]), 20, 10_000);
        assert_ne!(offsets_a, offsets_b);
    }

    #[test]
    fn verify_custody_proof_roundtrip() {
        let settings = default_kzg_settings();
        let chunk_data = vec![42_u8; CHUNK_SIZE_FOR_KZG];
        let commitment = compute_chunk_commitment(&chunk_data, settings).unwrap();
        let commitment_bytes = KzgCommitmentBytes::from(
            <[u8; COMMITMENT_SIZE]>::try_from(commitment.as_ref()).unwrap(),
        );

        let challenge_seed = H256::from([99_u8; 32]);
        let chunk_offset = 5_u32;
        let z = derive_challenge_point(&challenge_seed, chunk_offset);
        let (proof_bytes, y_bytes) =
            compute_chunk_opening_proof(&chunk_data, &z, settings).unwrap();

        let opening = CustodyOpening {
            chunk_offset,
            data_root: H256::from([1_u8; 32]),
            tx_chunk_index: 0,
            evaluation_point: FixedBytes::from(z),
            evaluation_value: FixedBytes::from(y_bytes),
            opening_proof: FixedBytes::from(proof_bytes),
        };

        let proof = CustodyProof {
            challenged_miner: IrysAddress::from([0xAA_u8; 20]),
            partition_hash: H256::from([0xBB_u8; 32]),
            challenge_seed,
            openings: vec![opening],
        };

        // clone: commitment_bytes is Copy but stored for closure capture
        let result = verify_custody_proof(
            &proof,
            |_data_root, _chunk_index| Ok(Some(commitment_bytes)),
            settings,
            1,
        )
        .unwrap();

        assert_eq!(result, CustodyVerificationResult::Valid);
    }

    #[test]
    fn verify_custody_proof_wrong_proof_fails() {
        let settings = default_kzg_settings();
        let chunk_data = vec![42_u8; CHUNK_SIZE_FOR_KZG];
        let commitment = compute_chunk_commitment(&chunk_data, settings).unwrap();
        let commitment_bytes = KzgCommitmentBytes::from(
            <[u8; COMMITMENT_SIZE]>::try_from(commitment.as_ref()).unwrap(),
        );

        let challenge_seed = H256::from([99_u8; 32]);
        let chunk_offset = 5_u32;
        let z = derive_challenge_point(&challenge_seed, chunk_offset);

        // Generate proof for different data
        let bad_data = vec![7_u8; CHUNK_SIZE_FOR_KZG];
        let (bad_proof, bad_y) = compute_chunk_opening_proof(&bad_data, &z, settings).unwrap();

        let opening = CustodyOpening {
            chunk_offset,
            data_root: H256::from([1_u8; 32]),
            tx_chunk_index: 0,
            evaluation_point: FixedBytes::from(z),
            evaluation_value: FixedBytes::from(bad_y),
            opening_proof: FixedBytes::from(bad_proof),
        };

        let proof = CustodyProof {
            challenged_miner: IrysAddress::from([0xAA_u8; 20]),
            partition_hash: H256::from([0xBB_u8; 32]),
            challenge_seed,
            openings: vec![opening],
        };

        let result = verify_custody_proof(
            &proof,
            |_data_root, _chunk_index| Ok(Some(commitment_bytes)),
            settings,
            1,
        )
        .unwrap();

        assert_eq!(
            result,
            CustodyVerificationResult::InvalidProof { chunk_offset: 5 }
        );
    }

    #[test]
    fn verify_custody_proof_missing_commitment() {
        let settings = default_kzg_settings();
        let challenge_seed = H256::from([99_u8; 32]);
        let data_root = H256::from([1_u8; 32]);

        let opening = CustodyOpening {
            chunk_offset: 5,
            data_root,
            tx_chunk_index: 0,
            evaluation_point: FixedBytes::ZERO,
            evaluation_value: FixedBytes::ZERO,
            opening_proof: FixedBytes::ZERO,
        };

        let proof = CustodyProof {
            challenged_miner: IrysAddress::from([0xAA_u8; 20]),
            partition_hash: H256::from([0xBB_u8; 32]),
            challenge_seed,
            openings: vec![opening],
        };

        let result = verify_custody_proof(&proof, |_dr, _ci| Ok(None), settings, 1).unwrap();

        assert_eq!(
            result,
            CustodyVerificationResult::MissingCommitment {
                data_root,
                chunk_index: 0,
            }
        );
    }

    #[test]
    fn verify_custody_proof_wrong_opening_count() {
        let settings = default_kzg_settings();

        let proof = CustodyProof {
            challenged_miner: IrysAddress::from([0xAA_u8; 20]),
            partition_hash: H256::from([0xBB_u8; 32]),
            challenge_seed: H256::from([99_u8; 32]),
            openings: vec![],
        };

        let result = verify_custody_proof(&proof, |_dr, _ci| Ok(None), settings, 5).unwrap();

        assert_eq!(
            result,
            CustodyVerificationResult::InvalidOpeningCount {
                expected: 5,
                got: 0,
            }
        );
    }
}
