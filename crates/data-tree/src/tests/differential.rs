//! Honest proofs verify. One changed field is rejected (spec §5, §10).
//!
//! A proof for index `i` shown as index `k` may verify when the bytes at both
//! indexes are identical, because that claim is true (spec §10). That case is
//! an explicit branch. It is not dropped with `prop_assume`.
//!
//! See `strategies` for the `PROPTEST_CASES` soak command. The check functions
//! take plain values so a later fuzzer can call them.

use super::strategies::{self, HonestUpload};
use crate::error::VerifyError;
use crate::geometry::Geometry;
use crate::size_witness;
use crate::types::{DataCommitment, HASH_SIZE};
use crate::verify::{verify_chunk, verify_segment};
use crate::{DataTree, depth};

/// Proptest's usual count. One case stays inside the upload cap, so this is
/// about half a second and well under 10 s. `PROPTEST_CASES` replaces it.
const HONEST_CASES: u32 = 256;

/// Low bit of the first byte of a changed buffer.
///
/// Any one bit changes that hash input (spec §10, no malleability).
const FLIPPED_BIT: u8 = 0b0000_0001;

proptest::proptest! {
    #![proptest_config(strategies::case_config(HONEST_CASES))]

    #[test]
    fn single_changes_to_an_honest_proof_are_rejected(upload in strategies::honest_upload()) {
        check_honest_upload(&upload);
    }
}

fn check_honest_upload(upload: &HonestUpload) {
    let geometry = upload.geometry;
    let data_size = u64::try_from(upload.bytes.len()).expect("honest uploads fit u64");
    let tree = DataTree::from_bytes(geometry, &upload.bytes)
        .unwrap_or_else(|err| panic!("honest upload of {data_size} bytes: {err}"));
    assert_eq!(tree.data_size(), data_size);
    let commitment = tree.data_commitment();
    let chunk_count = tree.chunk_count();
    assert!(chunk_count >= 1);

    check_data_size_changes(&geometry, &commitment, &tree, &upload.bytes, data_size);

    for chunk_index in 0..chunk_count {
        check_chunk(
            &geometry,
            &commitment,
            &tree,
            &upload.bytes,
            data_size,
            chunk_index,
            upload.probe,
        );
    }
}

fn check_data_size_changes(
    geometry: &Geometry,
    commitment: &DataCommitment,
    tree: &DataTree,
    data: &[u8],
    data_size: u64,
) {
    let chunk_count = tree.chunk_count();
    let indexes = [0, chunk_count - 1];
    for chunk_index in indexes {
        let chunk = chunk_bytes(data, geometry, data_size, chunk_index);
        let proof = tree
            .chunk_proof(chunk_index)
            .unwrap_or_else(|err| panic!("chunk proof {chunk_index}: {err}"));
        let segment = segment_bytes(data, geometry, data_size, chunk_index, 0);
        let segment_proof = tree
            .segment_proof(chunk_index, 0, chunk)
            .unwrap_or_else(|err| panic!("segment proof {chunk_index}: {err}"));
        for alternate in alternate_data_sizes(geometry, data_size) {
            // Same chunk count keeps `depth` and, for every chunk but the last,
            // `chunk_len`. The proof carries `size_witness` of the real size, so
            // a different size fails `SizeWitnessMismatch` when the length matches
            // (spec §5, §6). The last chunk's length usually changes inside that
            // window, and the length check fails first (spec §5). A different
            // chunk count fails at the first check in that same order.
            require_chunk_rejection(geometry, commitment, alternate, chunk_index, chunk, &proof);
            require_segment_rejection(
                geometry,
                commitment,
                alternate,
                chunk_index,
                0,
                segment,
                &segment_proof,
            );
        }
    }
}

fn check_chunk(
    geometry: &Geometry,
    commitment: &DataCommitment,
    tree: &DataTree,
    data: &[u8],
    data_size: u64,
    chunk_index: u32,
    probe: u64,
) {
    let chunk = chunk_bytes(data, geometry, data_size, chunk_index);
    let proof = tree
        .chunk_proof(chunk_index)
        .unwrap_or_else(|err| panic!("chunk proof {chunk_index}: {err}"));
    verify_chunk(geometry, commitment, data_size, chunk_index, chunk, &proof)
        .unwrap_or_else(|err| panic!("honest chunk {chunk_index}: {err}"));

    for other_index in other_chunk_indexes(tree.chunk_count(), chunk_index, probe) {
        let bytes_match = other_index < tree.chunk_count()
            && chunk == chunk_bytes(data, geometry, data_size, other_index);
        expect_rejection_or_identical_bytes(
            bytes_match,
            verify_chunk(geometry, commitment, data_size, other_index, chunk, &proof),
            expected_chunk_rejection(geometry, data_size, other_index, chunk, &proof),
            "chunk shown at another index",
        );
    }

    if let Some(flipped) = flip_first_bit(chunk) {
        require_chunk_rejection(
            geometry,
            commitment,
            data_size,
            chunk_index,
            &flipped,
            &proof,
        );
    }
    for hash_index in 0..proof.len() / HASH_SIZE {
        let flipped = flip_hash_bit(&proof, hash_index);
        require_chunk_rejection(
            geometry,
            commitment,
            data_size,
            chunk_index,
            chunk,
            &flipped,
        );
    }
    require_chunk_rejection(
        geometry,
        commitment,
        data_size,
        chunk_index,
        chunk,
        &with_extra_hash(&proof),
    );
    // Dropping the last hash removes the size witness. The length check rejects it.
    if let Some(shorter) = without_last_hash(&proof) {
        require_chunk_rejection(
            geometry,
            commitment,
            data_size,
            chunk_index,
            chunk,
            &shorter,
        );
    }
    require_chunk_rejection(
        geometry,
        commitment,
        data_size,
        chunk_index,
        &with_extra_byte(chunk),
        &proof,
    );
    if let Some(shorter) = without_last_byte(chunk) {
        require_chunk_rejection(
            geometry,
            commitment,
            data_size,
            chunk_index,
            &shorter,
            &proof,
        );
    }

    for segment_index in segment_indexes(geometry, data_size, chunk_index, probe) {
        check_segment(
            geometry,
            commitment,
            tree,
            data,
            data_size,
            chunk_index,
            chunk,
            segment_index,
            probe,
        );
    }
}

fn check_segment(
    geometry: &Geometry,
    commitment: &DataCommitment,
    tree: &DataTree,
    data: &[u8],
    data_size: u64,
    chunk_index: u32,
    chunk: &[u8],
    segment_index: u32,
    probe: u64,
) {
    if segment_index >= geometry.segments_per_chunk() {
        return;
    }
    let segment = segment_bytes(data, geometry, data_size, chunk_index, segment_index);
    let proof = tree
        .segment_proof(chunk_index, segment_index, chunk)
        .unwrap_or_else(|err| panic!("segment proof {chunk_index}/{segment_index}: {err}"));
    verify_segment(
        geometry,
        commitment,
        data_size,
        chunk_index,
        segment_index,
        segment,
        &proof,
    )
    .unwrap_or_else(|err| panic!("honest segment {chunk_index}/{segment_index}: {err}"));

    for other_index in other_segment_indexes(geometry.segments_per_chunk(), segment_index, probe) {
        let bytes_match = other_index < geometry.segments_per_chunk()
            && segment == segment_bytes(data, geometry, data_size, chunk_index, other_index);
        expect_rejection_or_identical_bytes(
            bytes_match,
            verify_segment(
                geometry,
                commitment,
                data_size,
                chunk_index,
                other_index,
                segment,
                &proof,
            ),
            expected_segment_rejection(
                geometry,
                data_size,
                chunk_index,
                other_index,
                segment,
                &proof,
            ),
            "segment shown at another index",
        );
    }

    // An empty segment has no bit to flip (spec §2). Adding a byte covers it.
    if let Some(flipped) = flip_first_bit(segment) {
        require_segment_rejection(
            geometry,
            commitment,
            data_size,
            chunk_index,
            segment_index,
            &flipped,
            &proof,
        );
    }
    for hash_index in 0..proof.len() / HASH_SIZE {
        let flipped = flip_hash_bit(&proof, hash_index);
        require_segment_rejection(
            geometry,
            commitment,
            data_size,
            chunk_index,
            segment_index,
            segment,
            &flipped,
        );
    }
    require_segment_rejection(
        geometry,
        commitment,
        data_size,
        chunk_index,
        segment_index,
        segment,
        &with_extra_hash(&proof),
    );
    if let Some(shorter) = without_last_hash(&proof) {
        require_segment_rejection(
            geometry,
            commitment,
            data_size,
            chunk_index,
            segment_index,
            segment,
            &shorter,
        );
    }
    require_segment_rejection(
        geometry,
        commitment,
        data_size,
        chunk_index,
        segment_index,
        &with_extra_byte(segment),
        &proof,
    );
    if let Some(shorter) = without_last_byte(segment) {
        require_segment_rejection(
            geometry,
            commitment,
            data_size,
            chunk_index,
            segment_index,
            &shorter,
            &proof,
        );
    }
}

fn expect_rejection_or_identical_bytes(
    bytes_match: bool,
    actual: Result<(), VerifyError>,
    expected: VerifyError,
    what: &str,
) {
    match actual {
        Ok(()) => assert!(
            bytes_match,
            "{what}: verify returned Ok and the bytes at that index differ (spec §10)"
        ),
        Err(error) => assert_eq!(error, expected, "{what}"),
    }
}

fn require_chunk_rejection(
    geometry: &Geometry,
    commitment: &DataCommitment,
    data_size: u64,
    chunk_index: u32,
    bytes: &[u8],
    proof: &[u8],
) {
    let actual = verify_chunk(geometry, commitment, data_size, chunk_index, bytes, proof);
    let expected = expected_chunk_rejection(geometry, data_size, chunk_index, bytes, proof);
    assert_eq!(
        actual,
        Err(expected),
        "chunk {chunk_index} data_size={data_size} bytes={} proof={}",
        bytes.len(),
        proof.len()
    );
}

fn require_segment_rejection(
    geometry: &Geometry,
    commitment: &DataCommitment,
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
    bytes: &[u8],
    proof: &[u8],
) {
    let actual = verify_segment(
        geometry,
        commitment,
        data_size,
        chunk_index,
        segment_index,
        bytes,
        proof,
    );
    let expected = expected_segment_rejection(
        geometry,
        data_size,
        chunk_index,
        segment_index,
        bytes,
        proof,
    );
    assert_eq!(
        actual,
        Err(expected),
        "segment {chunk_index}/{segment_index} data_size={data_size} bytes={} proof={}",
        bytes.len(),
        proof.len()
    );
}

/// Spec §5 order: range, byte length, proof length, size witness, then the commitment.
fn expected_chunk_rejection(
    geometry: &Geometry,
    data_size: u64,
    chunk_index: u32,
    bytes: &[u8],
    proof: &[u8],
) -> VerifyError {
    let chunk_length = match geometry.chunk_len(data_size, chunk_index) {
        Err(error) => return VerifyError::Size(error),
        Ok(chunk_length) => chunk_length,
    };
    let actual = u64::try_from(bytes.len()).expect("byte length fits u64");
    if actual != chunk_length {
        return VerifyError::InvalidChunkLength {
            expected: chunk_length,
            actual,
        };
    }
    let chunk_count = geometry
        .chunk_count(data_size)
        .expect("chunk_len proved data_size is in range");
    let hash_count =
        depth(chunk_index, chunk_count).expect("chunk_len requires chunk_index < chunk_count") + 1;
    let expected_proof = usize::try_from(hash_count).expect("depth fits usize") * HASH_SIZE;
    if proof.len() != expected_proof {
        return VerifyError::InvalidProofLength {
            expected: expected_proof,
            actual: proof.len(),
        };
    }
    if !witness_matches(proof, data_size) {
        return VerifyError::SizeWitnessMismatch;
    }
    VerifyError::CommitmentMismatch
}

/// Spec §5 order for a segment: range, byte length, `h + depth + 1` hashes
/// (the last is the size witness), then the commitment.
fn expected_segment_rejection(
    geometry: &Geometry,
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
    bytes: &[u8],
    proof: &[u8],
) -> VerifyError {
    let segment_length = match geometry.segment_len(data_size, chunk_index, segment_index) {
        Err(error) => return VerifyError::Size(error),
        Ok(segment_length) => segment_length,
    };
    let actual = u64::try_from(bytes.len()).expect("byte length fits u64");
    if actual != segment_length {
        return VerifyError::InvalidSegmentLength {
            expected: segment_length,
            actual,
        };
    }
    let chunk_count = geometry
        .chunk_count(data_size)
        .expect("segment_len proved data_size is in range");
    let hash_count = geometry.height()
        + depth(chunk_index, chunk_count).expect("segment_len requires chunk_index < chunk_count")
        + 1;
    let expected_proof = usize::try_from(hash_count).expect("hash count fits usize") * HASH_SIZE;
    if proof.len() != expected_proof {
        return VerifyError::InvalidProofLength {
            expected: expected_proof,
            actual: proof.len(),
        };
    }
    if !witness_matches(proof, data_size) {
        return VerifyError::SizeWitnessMismatch;
    }
    VerifyError::CommitmentMismatch
}

fn witness_matches(proof: &[u8], data_size: u64) -> bool {
    let start = proof.len() - HASH_SIZE;
    proof[start..] == *size_witness(data_size).as_bytes()
}

fn alternate_data_sizes(geometry: &Geometry, data_size: u64) -> Vec<u64> {
    let chunk_size = geometry.chunk_size();
    let max_data_size = geometry.max_data_size();
    let chunk_count = geometry
        .chunk_count(data_size)
        .expect("honest data_size is in range");
    let mut sizes = Vec::new();
    let window_start = u64::from(chunk_count - 1)
        .checked_mul(chunk_size)
        .expect("window start fits")
        .checked_add(1)
        .expect("window start fits");
    let window_end = u64::from(chunk_count)
        .saturating_mul(chunk_size)
        .min(max_data_size);
    push_different(&mut sizes, data_size, window_start);
    push_different(&mut sizes, data_size, window_end);
    if let Some(next) = u64::from(chunk_count)
        .checked_mul(chunk_size)
        .and_then(|end| end.checked_add(1))
        && next <= max_data_size
    {
        push_different(&mut sizes, data_size, next);
    }
    if chunk_count > 1 {
        let previous = u64::from(chunk_count - 1)
            .checked_mul(chunk_size)
            .expect("previous window fits");
        push_different(&mut sizes, data_size, previous);
    }
    push_different(&mut sizes, data_size, 0);
    if let Some(above_max) = max_data_size.checked_add(1) {
        push_different(&mut sizes, data_size, above_max);
    }
    sizes.sort_unstable();
    sizes.dedup();
    sizes
}

fn push_different(sizes: &mut Vec<u64>, current: u64, candidate: u64) {
    if candidate != current {
        sizes.push(candidate);
    }
}

fn other_chunk_indexes(chunk_count: u32, chunk_index: u32, probe: u64) -> Vec<u32> {
    // `chunk_count` is the first out-of-range index (spec §2).
    let mut indexes = vec![chunk_count];
    if chunk_count > 1 {
        indexes.push(0);
        indexes.push(chunk_count - 1);
        indexes.push(u32::try_from(probe % u64::from(chunk_count)).expect("modulo fits u32"));
    }
    indexes.retain(|index| *index != chunk_index);
    indexes.sort_unstable();
    indexes.dedup();
    indexes
}

fn segment_indexes(geometry: &Geometry, data_size: u64, chunk_index: u32, probe: u64) -> Vec<u32> {
    let segments_per_chunk = geometry.segments_per_chunk();
    let mut indexes = vec![0, segments_per_chunk - 1];
    indexes.push(u32::try_from(probe % u64::from(segments_per_chunk)).expect("modulo fits u32"));
    for segment_index in 0..segments_per_chunk {
        if geometry
            .segment_len(data_size, chunk_index, segment_index)
            .expect("chunk is in range")
            == 0
        {
            indexes.push(segment_index);
            break;
        }
    }
    indexes.sort_unstable();
    indexes.dedup();
    indexes
}

fn other_segment_indexes(segments_per_chunk: u32, segment_index: u32, probe: u64) -> Vec<u32> {
    let mut indexes = vec![
        0,
        segments_per_chunk - 1,
        segments_per_chunk,
        u32::try_from(probe % u64::from(segments_per_chunk)).expect("modulo fits u32"),
    ];
    indexes.retain(|index| *index != segment_index);
    indexes.sort_unstable();
    indexes.dedup();
    indexes
}

fn chunk_bytes<'a>(
    data: &'a [u8],
    geometry: &Geometry,
    data_size: u64,
    chunk_index: u32,
) -> &'a [u8] {
    let start = usize::try_from(u64::from(chunk_index) * geometry.chunk_size())
        .expect("honest chunk start fits usize");
    let chunk_length = usize::try_from(
        geometry
            .chunk_len(data_size, chunk_index)
            .expect("honest chunk index is in range"),
    )
    .expect("honest chunk length fits usize");
    &data[start..start + chunk_length]
}

fn segment_bytes<'a>(
    data: &'a [u8],
    geometry: &Geometry,
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
) -> &'a [u8] {
    let segment_length = geometry
        .segment_len(data_size, chunk_index, segment_index)
        .expect("honest segment index is in range");
    if segment_length == 0 {
        return &[];
    }
    let start = u64::from(chunk_index) * geometry.chunk_size()
        + u64::from(segment_index) * geometry.segment_size();
    let start = usize::try_from(start).expect("honest segment start fits usize");
    let segment_length = usize::try_from(segment_length).expect("honest segment length fits usize");
    &data[start..start + segment_length]
}

fn flip_first_bit(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut flipped = bytes.to_vec();
    let first = flipped.first_mut()?;
    *first ^= FLIPPED_BIT;
    Some(flipped)
}

fn flip_hash_bit(proof: &[u8], hash_index: usize) -> Vec<u8> {
    let mut flipped = proof.to_vec();
    flipped[hash_index * HASH_SIZE] ^= FLIPPED_BIT;
    flipped
}

fn with_extra_hash(proof: &[u8]) -> Vec<u8> {
    let mut extended = proof.to_vec();
    extended.extend_from_slice(&[0_u8; HASH_SIZE]);
    extended
}

fn without_last_hash(proof: &[u8]) -> Option<Vec<u8>> {
    proof
        .len()
        .checked_sub(HASH_SIZE)
        .map(|end| proof[..end].to_vec())
}

fn with_extra_byte(bytes: &[u8]) -> Vec<u8> {
    let mut extended = bytes.to_vec();
    extended.push(0);
    extended
}

fn without_last_byte(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.is_empty() {
        return None;
    }
    Some(bytes[..bytes.len() - 1].to_vec())
}
