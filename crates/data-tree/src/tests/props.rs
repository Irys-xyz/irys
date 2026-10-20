//! Random uploads: honest proofs verify, one changed byte does not (spec §13).

use proptest::prelude::*;

use crate::DataTree;
use crate::error::VerifyError;
use crate::geometry::Geometry;
use crate::verify::{verify_chunk, verify_segment};

proptest! {
    #[test]
    fn random_bytes_proofs_verify_and_mutations_fail(
        data in proptest::collection::vec(any::<u8>(), 1..=256)
    ) {
        check_upload(&data)?;
    }
}

fn check_upload(data: &[u8]) -> Result<(), TestCaseError> {
    let geom = Geometry::TESTING;
    let n = u64::try_from(data.len()).unwrap();
    let tree = DataTree::from_bytes(geom, data).unwrap();
    let commitment = tree.data_commitment();
    for c in 0..tree.chunk_count() {
        check_chunk(&geom, &commitment, &tree, data, n, c)?;
    }
    Ok(())
}

fn check_chunk(
    geom: &Geometry,
    commitment: &crate::DataCommitment,
    tree: &DataTree,
    data: &[u8],
    n: u64,
    c: u32,
) -> Result<(), TestCaseError> {
    let chunk = chunk_slice(geom, data, n, c);
    let proof = tree.chunk_proof(c).unwrap();
    verify_chunk(geom, commitment, n, c, &chunk, &proof)
        .map_err(|err| TestCaseError::fail(format!("chunk n={n} c={c}: {err}")))?;
    let mut bad_chunk = chunk.clone();
    bad_chunk[0] ^= 1;
    let err = verify_chunk(geom, commitment, n, c, &bad_chunk, &proof).unwrap_err();
    prop_assert_eq!(err, VerifyError::CommitmentMismatch);
    check_proof_mutation(proof, |bad| {
        verify_chunk(geom, commitment, n, c, &chunk, bad)
    })?;
    for j in 0..geom.segments_per_chunk() {
        check_segment(geom, commitment, tree, data, n, c, &chunk, j)?;
    }
    Ok(())
}

fn check_segment(
    geom: &Geometry,
    commitment: &crate::DataCommitment,
    tree: &DataTree,
    data: &[u8],
    n: u64,
    c: u32,
    chunk: &[u8],
    j: u32,
) -> Result<(), TestCaseError> {
    let segment = segment_slice(geom, data, n, c, j);
    let proof = tree.segment_proof(c, j, chunk).unwrap();
    verify_segment(geom, commitment, n, c, j, &segment, &proof)
        .map_err(|err| TestCaseError::fail(format!("segment n={n} c={c} j={j}: {err}")))?;
    if segment.is_empty() {
        // No byte to flip. A non-empty slice fails the length check (spec §5).
        let err = verify_segment(geom, commitment, n, c, j, &[0], &proof).unwrap_err();
        prop_assert_eq!(
            err,
            VerifyError::InvalidSegmentLength {
                expected: 0,
                actual: 1
            }
        );
    } else {
        let mut bad = segment.clone();
        bad[0] ^= 1;
        let err = verify_segment(geom, commitment, n, c, j, &bad, &proof).unwrap_err();
        prop_assert_eq!(err, VerifyError::CommitmentMismatch);
    }
    check_proof_mutation(proof, |bad| {
        verify_segment(geom, commitment, n, c, j, &segment, bad)
    })
}

fn check_proof_mutation(
    proof: Vec<u8>,
    check: impl Fn(&[u8]) -> Result<(), VerifyError>,
) -> Result<(), TestCaseError> {
    if proof.is_empty() {
        // `C = 1` chunk proofs are empty. Junk of 1 or 32 bytes is the wrong length.
        let err = check(&[0]).unwrap_err();
        prop_assert_eq!(
            err,
            VerifyError::InvalidProofLength {
                expected: 0,
                actual: 1
            }
        );
        let err = check(&[0_u8; 32]).unwrap_err();
        prop_assert_eq!(
            err,
            VerifyError::InvalidProofLength {
                expected: 0,
                actual: 32
            }
        );
        return Ok(());
    }
    let mut bad = proof;
    bad[0] ^= 1;
    let err = check(&bad).unwrap_err();
    prop_assert_eq!(err, VerifyError::CommitmentMismatch);
    Ok(())
}

fn chunk_slice(geom: &Geometry, data: &[u8], n: u64, c: u32) -> Vec<u8> {
    let start = usize::try_from(u64::from(c) * geom.chunk_size()).unwrap();
    let len = usize::try_from(geom.chunk_len(n, c).unwrap()).unwrap();
    data[start..start + len].to_vec()
}

fn segment_slice(geom: &Geometry, data: &[u8], n: u64, c: u32, j: u32) -> Vec<u8> {
    let len = geom.segment_len(n, c, j).unwrap();
    if len == 0 {
        return Vec::new();
    }
    let start = u64::from(c) * geom.chunk_size() + u64::from(j) * geom.segment_size();
    let start = usize::try_from(start).unwrap();
    let len = usize::try_from(len).unwrap();
    data[start..start + len].to_vec()
}
