use crate::kzg::{
    KzgCommitmentBytes, PROOF_SIZE, SCALAR_SIZE, derive_challenge_point, verify_chunk_opening_proof,
};
use crate::{H256, IrysAddress};
use alloy_primitives::FixedBytes;
use alloy_rlp::{RlpDecodable, RlpEncodable};
use c_kzg::KzgSettings;
use openssl::sha;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, RlpEncodable, RlpDecodable)]
pub struct CustodyChallenge {
    pub challenged_miner: IrysAddress,
    pub partition_hash: H256,
    pub challenge_seed: H256,
    pub challenge_block_height: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, RlpEncodable, RlpDecodable)]
pub struct CustodyOpening {
    pub chunk_offset: u32,
    pub data_root: H256,
    pub tx_chunk_index: u32,
    pub evaluation_point: FixedBytes<SCALAR_SIZE>,
    pub evaluation_value: FixedBytes<SCALAR_SIZE>,
    pub opening_proof: FixedBytes<PROOF_SIZE>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, RlpEncodable, RlpDecodable)]
pub struct CustodyProof {
    pub challenged_miner: IrysAddress,
    pub partition_hash: H256,
    pub challenge_seed: H256,
    pub openings: Vec<CustodyOpening>,
}

impl CustodyProof {
    /// Gossip identity: partition, seed, and challenged miner.
    pub fn gossip_cache_id(&self) -> H256 {
        let mut hasher = sha::Sha256::new();
        hasher.update(&self.partition_hash.0);
        hasher.update(&self.challenge_seed.0);
        hasher.update(&self.challenged_miner.0.0);
        H256(hasher.finish())
    }
}

/// Keccak of the RLP-encoded proof list. Empty and non-empty lists both have a root.
pub fn custody_proofs_root(proofs: &[CustodyProof]) -> H256 {
    H256(alloy_primitives::keccak256(encode_custody_proofs(proofs)).0)
}

pub fn encode_custody_proofs(proofs: &[CustodyProof]) -> Vec<u8> {
    let mut out = Vec::new();
    alloy_rlp::encode_list(proofs, &mut out);
    out
}

pub fn decode_custody_proofs(bytes: &[u8]) -> eyre::Result<Vec<CustodyProof>> {
    use alloy_rlp::Decodable as _;
    let mut rest = bytes;
    let proofs = Vec::<CustodyProof>::decode(&mut rest)
        .map_err(|err| eyre::eyre!("custody proof list decode failed: {err}"))?;
    eyre::ensure!(rest.is_empty(), "trailing bytes after custody proof list");
    Ok(proofs)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssignedChunk {
    pub data_root: H256,
    pub tx_chunk_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CustodyVerificationResult {
    Valid,
    InvalidOpeningCount {
        expected: u32,
        got: u32,
    },
    UnexpectedSeed {
        got: H256,
        expected: H256,
    },
    UnexpectedMiner {
        got: IrysAddress,
        expected: IrysAddress,
    },
    InvalidOffset {
        chunk_offset: u32,
        expected: u32,
    },
    UnassignedPartition {
        chunk_offset: u32,
    },
    AssignmentMismatch {
        chunk_offset: u32,
    },
    UnexpectedEvaluationPoint {
        chunk_offset: u32,
    },
    MissingCommitment {
        data_root: H256,
        chunk_index: u32,
    },
    InvalidProof {
        chunk_offset: u32,
    },
}

/// `challenge_seed = SHA256(vdf_output || partition_hash)`
pub fn derive_challenge_seed(vdf_output: &[u8; 32], partition_hash: &H256) -> H256 {
    let mut hasher = sha::Sha256::new();
    hasher.update(vdf_output);
    hasher.update(&partition_hash.0);
    H256(hasher.finish())
}

/// `offset_j = hash_to_u64(SHA256(challenge_seed || j_le)) % num_chunks`
pub fn select_challenged_offsets(
    challenge_seed: &H256,
    k: u32,
    num_chunks_in_partition: u64,
) -> eyre::Result<Vec<u32>> {
    eyre::ensure!(
        num_chunks_in_partition <= u64::from(u32::MAX),
        "num_chunks_in_partition ({num_chunks_in_partition}) exceeds u32 range"
    );
    (0..k)
        .map(|j| {
            let mut hasher = sha::Sha256::new();
            hasher.update(&challenge_seed.0);
            hasher.update(&j.to_le_bytes());
            let hash = hasher.finish();

            let val = u64::from_le_bytes([
                hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7],
            ]);
            Ok(u32::try_from(val % num_chunks_in_partition)?)
        })
        .collect()
}

/// `assigned_chunk` resolves the canonical chunk at a partition offset.
/// `Ok(None)` means the offset is not assigned. `Err` is a local lookup failure.
/// `get_commitment` loads the stored commitment for that assigned chunk.
pub fn verify_custody_proof(
    proof: &CustodyProof,
    expected_challenge_seed: H256,
    expected_miner: IrysAddress,
    assigned_chunk: impl Fn(u32) -> eyre::Result<Option<AssignedChunk>>,
    get_commitment: impl Fn(H256, u32) -> eyre::Result<Option<KzgCommitmentBytes>>,
    kzg_settings: &KzgSettings,
    expected_challenge_count: u32,
    num_chunks_in_partition: u64,
) -> eyre::Result<CustodyVerificationResult> {
    let got = u32::try_from(proof.openings.len())
        .map_err(|_| eyre::eyre!("opening count exceeds u32"))?;
    if got != expected_challenge_count {
        return Ok(CustodyVerificationResult::InvalidOpeningCount {
            expected: expected_challenge_count,
            got,
        });
    }
    if proof.challenge_seed != expected_challenge_seed {
        return Ok(CustodyVerificationResult::UnexpectedSeed {
            got: proof.challenge_seed,
            expected: expected_challenge_seed,
        });
    }
    if proof.challenged_miner != expected_miner {
        return Ok(CustodyVerificationResult::UnexpectedMiner {
            got: proof.challenged_miner,
            expected: expected_miner,
        });
    }

    let expected_offsets = select_challenged_offsets(
        &expected_challenge_seed,
        expected_challenge_count,
        num_chunks_in_partition,
    )?;

    for (opening, &expected_offset) in proof.openings.iter().zip(expected_offsets.iter()) {
        if opening.chunk_offset != expected_offset {
            return Ok(CustodyVerificationResult::InvalidOffset {
                chunk_offset: opening.chunk_offset,
                expected: expected_offset,
            });
        }

        let Some(assigned) = assigned_chunk(expected_offset)? else {
            return Ok(CustodyVerificationResult::UnassignedPartition {
                chunk_offset: expected_offset,
            });
        };
        if opening.data_root != assigned.data_root
            || opening.tx_chunk_index != assigned.tx_chunk_index
        {
            return Ok(CustodyVerificationResult::AssignmentMismatch {
                chunk_offset: expected_offset,
            });
        }

        let expected_point = derive_challenge_point(&expected_challenge_seed, expected_offset);
        if opening.evaluation_point.as_slice() != expected_point {
            return Ok(CustodyVerificationResult::UnexpectedEvaluationPoint {
                chunk_offset: expected_offset,
            });
        }

        let commitment = match get_commitment(assigned.data_root, assigned.tx_chunk_index)? {
            Some(c) => c,
            None => {
                return Ok(CustodyVerificationResult::MissingCommitment {
                    data_root: assigned.data_root,
                    chunk_index: assigned.tx_chunk_index,
                });
            }
        };

        let valid = verify_chunk_opening_proof(
            &commitment,
            &expected_point,
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
        CHUNK_SIZE_FOR_KZG, COMMITMENT_SIZE, compute_chunk_commitment, compute_chunk_opening_proof,
        default_kzg_settings, derive_challenge_point,
    };

    const TEST_NUM_CHUNKS: u64 = 1000;

    fn test_proof(challenge_seed: H256, openings: Vec<CustodyOpening>) -> CustodyProof {
        CustodyProof {
            challenged_miner: IrysAddress::from([0xAA_u8; 20]),
            partition_hash: H256::from([0xBB_u8; 32]),
            challenge_seed,
            openings,
        }
    }

    fn assigned_as_named(
        opening: &CustodyOpening,
    ) -> impl Fn(u32) -> eyre::Result<Option<AssignedChunk>> {
        let chunk = AssignedChunk {
            data_root: opening.data_root,
            tx_chunk_index: opening.tx_chunk_index,
        };
        let offset = opening.chunk_offset;
        move |asked| Ok(if asked == offset { Some(chunk) } else { None })
    }

    fn verify_named(
        proof: &CustodyProof,
        opening: &CustodyOpening,
        commitment: impl Fn(H256, u32) -> eyre::Result<Option<KzgCommitmentBytes>>,
        settings: &KzgSettings,
        count: u32,
    ) -> eyre::Result<CustodyVerificationResult> {
        verify_custody_proof(
            proof,
            proof.challenge_seed,
            proof.challenged_miner,
            assigned_as_named(opening),
            commitment,
            settings,
            count,
            TEST_NUM_CHUNKS,
        )
    }

    #[test]
    fn select_challenged_offsets_returns_k() {
        let seed = H256::from([42_u8; 32]);
        let offsets = select_challenged_offsets(&seed, 20, TEST_NUM_CHUNKS).unwrap();
        assert_eq!(offsets.len(), 20);
    }

    #[test]
    fn select_challenged_offsets_within_bounds() {
        let seed = H256::from([42_u8; 32]);
        let offsets = select_challenged_offsets(&seed, 20, 500).unwrap();
        for &offset in &offsets {
            assert!(u64::from(offset) < 500);
        }
    }

    #[test]
    fn select_challenged_offsets_different_seeds() {
        let offsets_a = select_challenged_offsets(&H256::from([1_u8; 32]), 20, 10_000).unwrap();
        let offsets_b = select_challenged_offsets(&H256::from([2_u8; 32]), 20, 10_000).unwrap();
        assert_ne!(offsets_a, offsets_b);
    }

    #[test]
    fn select_challenged_offsets_rejects_oversized_partition() {
        let seed = H256::from([42_u8; 32]);
        let result = select_challenged_offsets(&seed, 1, u64::from(u32::MAX) + 1);
        assert!(result.is_err());
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
        let expected_offsets =
            select_challenged_offsets(&challenge_seed, 1, TEST_NUM_CHUNKS).unwrap();
        let chunk_offset = expected_offsets[0];
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

        let proof = test_proof(challenge_seed, vec![opening.clone()]);
        let result = verify_named(
            &proof,
            &opening,
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
        let expected_offsets =
            select_challenged_offsets(&challenge_seed, 1, TEST_NUM_CHUNKS).unwrap();
        let chunk_offset = expected_offsets[0];
        let z = derive_challenge_point(&challenge_seed, chunk_offset);

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

        let proof = test_proof(challenge_seed, vec![opening.clone()]);
        let result = verify_named(
            &proof,
            &opening,
            |_data_root, _chunk_index| Ok(Some(commitment_bytes)),
            settings,
            1,
        )
        .unwrap();

        assert_eq!(
            result,
            CustodyVerificationResult::InvalidProof { chunk_offset }
        );
    }

    #[test]
    fn verify_custody_proof_wrong_offset_fails() {
        let settings = default_kzg_settings();
        let challenge_seed = H256::from([99_u8; 32]);
        let expected_offsets =
            select_challenged_offsets(&challenge_seed, 1, TEST_NUM_CHUNKS).unwrap();

        let wrong_offset = expected_offsets[0].wrapping_add(1);
        let opening = CustodyOpening {
            chunk_offset: wrong_offset,
            data_root: H256::from([1_u8; 32]),
            tx_chunk_index: 0,
            evaluation_point: FixedBytes::ZERO,
            evaluation_value: FixedBytes::ZERO,
            opening_proof: FixedBytes::ZERO,
        };

        let proof = test_proof(challenge_seed, vec![opening.clone()]);
        let result = verify_named(
            &proof,
            &opening,
            |_dr, _ci| Ok(Some(KzgCommitmentBytes::from([0_u8; COMMITMENT_SIZE]))),
            settings,
            1,
        )
        .unwrap();

        assert_eq!(
            result,
            CustodyVerificationResult::InvalidOffset {
                chunk_offset: wrong_offset,
                expected: expected_offsets[0],
            }
        );
    }

    #[test]
    fn verify_custody_proof_missing_commitment() {
        let settings = default_kzg_settings();
        let challenge_seed = H256::from([99_u8; 32]);
        let expected_offsets =
            select_challenged_offsets(&challenge_seed, 1, TEST_NUM_CHUNKS).unwrap();
        let data_root = H256::from([1_u8; 32]);

        let chunk_offset = expected_offsets[0];
        let opening = CustodyOpening {
            chunk_offset,
            data_root,
            tx_chunk_index: 0,
            evaluation_point: FixedBytes::from(derive_challenge_point(
                &challenge_seed,
                chunk_offset,
            )),
            evaluation_value: FixedBytes::ZERO,
            opening_proof: FixedBytes::ZERO,
        };

        let proof = test_proof(challenge_seed, vec![opening.clone()]);
        let result = verify_named(&proof, &opening, |_dr, _ci| Ok(None), settings, 1).unwrap();

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

        let proof = test_proof(H256::from([99_u8; 32]), vec![]);
        let result = verify_custody_proof(
            &proof,
            proof.challenge_seed,
            proof.challenged_miner,
            |_offset| Ok(None),
            |_dr, _ci| Ok(None),
            settings,
            5,
            TEST_NUM_CHUNKS,
        )
        .unwrap();

        assert_eq!(
            result,
            CustodyVerificationResult::InvalidOpeningCount {
                expected: 5,
                got: 0,
            }
        );
    }

    #[test]
    fn verify_custody_proof_prover_chosen_point_fails() {
        let settings = default_kzg_settings();
        let challenge_seed = H256::from([99_u8; 32]);
        let chunk_offset =
            select_challenged_offsets(&challenge_seed, 1, TEST_NUM_CHUNKS).unwrap()[0];
        let opening = CustodyOpening {
            chunk_offset,
            data_root: H256::from([1_u8; 32]),
            tx_chunk_index: 0,
            evaluation_point: FixedBytes::from([1_u8; SCALAR_SIZE]),
            evaluation_value: FixedBytes::ZERO,
            opening_proof: FixedBytes::ZERO,
        };
        let proof = test_proof(challenge_seed, vec![opening.clone()]);
        let result = verify_named(
            &proof,
            &opening,
            |_dr, _ci| Ok(Some(KzgCommitmentBytes::from([0_u8; COMMITMENT_SIZE]))),
            settings,
            1,
        )
        .unwrap();
        assert_eq!(
            result,
            CustodyVerificationResult::UnexpectedEvaluationPoint { chunk_offset }
        );
    }

    #[test]
    fn verify_custody_proof_wrong_assigned_chunk_fails() {
        let settings = default_kzg_settings();
        let challenge_seed = H256::from([99_u8; 32]);
        let chunk_offset =
            select_challenged_offsets(&challenge_seed, 1, TEST_NUM_CHUNKS).unwrap()[0];
        let z = derive_challenge_point(&challenge_seed, chunk_offset);
        let opening = CustodyOpening {
            chunk_offset,
            data_root: H256::from([1_u8; 32]),
            tx_chunk_index: 0,
            evaluation_point: FixedBytes::from(z),
            evaluation_value: FixedBytes::ZERO,
            opening_proof: FixedBytes::ZERO,
        };
        let proof = test_proof(challenge_seed, vec![opening]);
        let assigned = AssignedChunk {
            data_root: H256::from([2_u8; 32]),
            tx_chunk_index: 0,
        };
        let result = verify_custody_proof(
            &proof,
            proof.challenge_seed,
            proof.challenged_miner,
            move |_offset| Ok(Some(assigned)),
            |_dr, _ci| Ok(Some(KzgCommitmentBytes::from([0_u8; COMMITMENT_SIZE]))),
            settings,
            1,
            TEST_NUM_CHUNKS,
        )
        .unwrap();
        assert_eq!(
            result,
            CustodyVerificationResult::AssignmentMismatch { chunk_offset }
        );
    }
}
