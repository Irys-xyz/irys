//! Random uploads: honest proofs verify, one changed byte does not (spec §13).

use proptest::prelude::*;

use crate::DataTree;
use crate::error::VerifyError;
use crate::geometry::Geometry;
use crate::types::HASH_SIZE;
use crate::verify::{verify_chunk, verify_segment};

proptest! {
    #[test]
    fn random_bytes_proofs_verify_and_mutations_fail(
        data in proptest::collection::vec(any::<u8>(), 1..=RANDOM_UPLOAD_BYTES)
    ) {
        check_upload(&data)?;
    }
}

fn check_upload(data: &[u8]) -> Result<(), TestCaseError> {
    let geometry = Geometry::TESTING;
    let data_size = u64::try_from(data.len()).unwrap();
    let tree = DataTree::from_bytes(geometry, data).unwrap();
    let commitment = tree.data_commitment();
    for chunk_index in 0..tree.chunk_count() {
        check_chunk(&geometry, &commitment, &tree, data, data_size, chunk_index)?;
    }
    Ok(())
}

fn check_chunk(
    geometry: &Geometry,
    commitment: &crate::DataCommitment,
    tree: &DataTree,
    data: &[u8],
    data_size: u64,
    chunk_index: u32,
) -> Result<(), TestCaseError> {
    let chunk = chunk_slice(geometry, data, data_size, chunk_index);
    let proof = tree.chunk_proof(chunk_index).unwrap();
    verify_chunk(geometry, commitment, data_size, chunk_index, &chunk, &proof).map_err(|err| {
        TestCaseError::fail(format!("chunk n={data_size} c={chunk_index}: {err}"))
    })?;
    let mut flipped_chunk = chunk.clone();
    flipped_chunk[0] ^= 1;
    let err = verify_chunk(
        geometry,
        commitment,
        data_size,
        chunk_index,
        &flipped_chunk,
        &proof,
    )
    .unwrap_err();
    prop_assert_eq!(err, VerifyError::CommitmentMismatch);
    check_proof_mutation(proof, |flipped_proof| {
        verify_chunk(
            geometry,
            commitment,
            data_size,
            chunk_index,
            &chunk,
            flipped_proof,
        )
    })?;
    for segment_index in 0..geometry.segments_per_chunk() {
        check_segment(
            geometry,
            commitment,
            tree,
            data,
            data_size,
            chunk_index,
            &chunk,
            segment_index,
        )?;
    }
    Ok(())
}

fn check_segment(
    geometry: &Geometry,
    commitment: &crate::DataCommitment,
    tree: &DataTree,
    data: &[u8],
    data_size: u64,
    chunk_index: u32,
    chunk: &[u8],
    segment_index: u32,
) -> Result<(), TestCaseError> {
    let segment = segment_slice(geometry, data, data_size, chunk_index, segment_index);
    let proof = tree
        .segment_proof(chunk_index, segment_index, chunk)
        .unwrap();
    verify_segment(
        geometry,
        commitment,
        data_size,
        chunk_index,
        segment_index,
        &segment,
        &proof,
    )
    .map_err(|err| {
        TestCaseError::fail(format!(
            "segment n={data_size} c={chunk_index} j={segment_index}: {err}"
        ))
    })?;
    if segment.is_empty() {
        // No byte to flip. A non-empty slice fails the length check (spec §5).
        let err = verify_segment(
            geometry,
            commitment,
            data_size,
            chunk_index,
            segment_index,
            &[0],
            &proof,
        )
        .unwrap_err();
        prop_assert_eq!(
            err,
            VerifyError::InvalidSegmentLength {
                expected: 0,
                actual: 1
            }
        );
    } else {
        let mut flipped_segment = segment.clone();
        flipped_segment[0] ^= 1;
        let err = verify_segment(
            geometry,
            commitment,
            data_size,
            chunk_index,
            segment_index,
            &flipped_segment,
            &proof,
        )
        .unwrap_err();
        prop_assert_eq!(err, VerifyError::CommitmentMismatch);
    }
    check_proof_mutation(proof, |flipped_proof| {
        verify_segment(
            geometry,
            commitment,
            data_size,
            chunk_index,
            segment_index,
            &segment,
            flipped_proof,
        )
    })
}

fn check_proof_mutation(
    proof: Vec<u8>,
    check: impl Fn(&[u8]) -> Result<(), VerifyError>,
) -> Result<(), TestCaseError> {
    if proof.is_empty() {
        // `C = 1` chunk proofs are empty. Junk of 1 byte or one hash is the wrong length.
        let err = check(&[0]).unwrap_err();
        prop_assert_eq!(
            err,
            VerifyError::InvalidProofLength {
                expected: 0,
                actual: 1
            }
        );
        let err = check(&[0_u8; HASH_SIZE]).unwrap_err();
        prop_assert_eq!(
            err,
            VerifyError::InvalidProofLength {
                expected: 0,
                actual: HASH_SIZE
            }
        );
        return Ok(());
    }
    let mut flipped_proof = proof;
    flipped_proof[0] ^= 1;
    let err = check(&flipped_proof).unwrap_err();
    prop_assert_eq!(err, VerifyError::CommitmentMismatch);
    Ok(())
}

/// Eight testing chunks. Enough uploads to cross several tree shapes (spec §13).
const RANDOM_UPLOAD_CHUNKS: usize = 8;

const RANDOM_UPLOAD_BYTES: usize = RANDOM_UPLOAD_CHUNKS * Geometry::TESTING.chunk_size() as usize;

fn chunk_slice(geometry: &Geometry, data: &[u8], data_size: u64, chunk_index: u32) -> Vec<u8> {
    let start = usize::try_from(u64::from(chunk_index) * geometry.chunk_size()).unwrap();
    let chunk_length =
        usize::try_from(geometry.chunk_len(data_size, chunk_index).unwrap()).unwrap();
    data[start..start + chunk_length].to_vec()
}

fn segment_slice(
    geometry: &Geometry,
    data: &[u8],
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
) -> Vec<u8> {
    let segment_length = geometry
        .segment_len(data_size, chunk_index, segment_index)
        .unwrap();
    if segment_length == 0 {
        return Vec::new();
    }
    let start = u64::from(chunk_index) * geometry.chunk_size()
        + u64::from(segment_index) * geometry.segment_size();
    let start = usize::try_from(start).unwrap();
    let segment_length = usize::try_from(segment_length).unwrap();
    data[start..start + segment_length].to_vec()
}
