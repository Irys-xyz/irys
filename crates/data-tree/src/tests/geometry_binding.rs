//! The size witness binds `data_size` and the geometry (spec §3, §10).
//!
//! A commitment and a `chunk_key` are specific to one `(data_size, chunk_size,
//! segments_per_chunk)`. `max_data_tx_chunks` is not in the preimage.

use sha2::{Digest as _, Sha256};

use crate::geometry::Geometry;
use crate::hash::TreeDomainTag;
use crate::types::{HASH_SIZE, SizeWitness};
use crate::{DataTree, chunk_root, verify_chunk, verify_segment, verify_size};

/// Tag byte in front of the size witness (spec §3).
const SIZE_WITNESS_TAG_LEN: usize = 1;
/// `be64` width (spec §3).
const BE64_LEN: usize = 8;
/// `be16` width (spec §3).
const BE16_LEN: usize = 2;
/// `0x02 ‖ be64(data_size) ‖ be64(chunk_size) ‖ be16(segments_per_chunk)` (spec §3).
const SIZE_WITNESS_PREIMAGE_LEN: usize = SIZE_WITNESS_TAG_LEN + BE64_LEN + BE64_LEN + BE16_LEN;

/// Chunk count large enough for every grid row below.
const GRID_MAX_CHUNKS: u32 = 64;

/// Break: `size_witness` omits `chunk_size` or `segments_per_chunk`.
///
/// Then one commitment is valid under two geometries that split the same bytes
/// differently, `chunk_key` aliases, and a segment proof under one geometry
/// verifies as a chunk proof under the other (spec §3, §7, §10).
#[test]
fn commitment_and_chunk_key_bind_geometry() {
    let wide = Geometry::new(8, 2, 10).unwrap();
    let narrow = Geometry::new(4, 1, 10).unwrap();
    let data = b"AAAABBBB";
    let data_size = u64::try_from(data.len()).unwrap();
    let wide_tree = DataTree::from_bytes(wide, data).unwrap();
    let narrow_tree = DataTree::from_bytes(narrow, data).unwrap();

    // Both trees are `node(leaf("AAAA"), leaf("BBBB"))`. The witness is not.
    assert_eq!(wide_tree.data_tree_root(), narrow_tree.data_tree_root());
    assert_ne!(wide_tree.data_commitment(), narrow_tree.data_commitment());
    assert_ne!(
        wide_tree.chunk_key(0).unwrap(),
        narrow_tree.chunk_key(0).unwrap()
    );

    let segment_proof = wide_tree.segment_proof(0, 1, data).unwrap();
    let chunk_proof = narrow_tree.chunk_proof(1).unwrap();
    assert_eq!(segment_proof.len(), chunk_proof.len());
    assert_eq!(
        &segment_proof[..segment_proof.len() - HASH_SIZE],
        &chunk_proof[..chunk_proof.len() - HASH_SIZE],
        "the sibling list is the shared data tree"
    );
    assert_ne!(
        &segment_proof[segment_proof.len() - HASH_SIZE..],
        &chunk_proof[chunk_proof.len() - HASH_SIZE..],
        "the last hash is the geometry-specific witness"
    );

    verify_segment(
        &wide,
        &wide_tree.data_commitment(),
        data_size,
        0,
        1,
        b"BBBB",
        &segment_proof,
    )
    .unwrap();
    verify_chunk(
        &narrow,
        &narrow_tree.data_commitment(),
        data_size,
        1,
        b"BBBB",
        &chunk_proof,
    )
    .unwrap();
    assert!(
        verify_chunk(
            &narrow,
            &narrow_tree.data_commitment(),
            data_size,
            1,
            b"BBBB",
            &segment_proof,
        )
        .is_err(),
        "the wide segment proof must fail as a narrow chunk proof"
    );
    assert!(
        verify_segment(
            &wide,
            &wide_tree.data_commitment(),
            data_size,
            0,
            1,
            b"BBBB",
            &chunk_proof,
        )
        .is_err(),
        "the narrow chunk proof must fail as a wide segment proof"
    );
    assert!(
        verify_segment(
            &wide,
            &narrow_tree.data_commitment(),
            data_size,
            0,
            1,
            b"BBBB",
            &segment_proof,
        )
        .is_err(),
        "the wide proof must fail against the narrow commitment"
    );
    assert!(
        verify_chunk(
            &narrow,
            &wide_tree.data_commitment(),
            data_size,
            1,
            b"BBBB",
            &chunk_proof,
        )
        .is_err(),
        "the narrow proof must fail against the wide commitment"
    );

    // Same height, different segment size. A chunk shorter than both segment
    // sizes shares a chunk root. The commitment still differs.
    let coarse = Geometry::new(32, 4, 10).unwrap();
    let wider_chunk = Geometry::new(64, 4, 10).unwrap();
    let short = b"xyz";
    assert_eq!(
        chunk_root(&coarse, short).unwrap(),
        chunk_root(&wider_chunk, short).unwrap()
    );
    let coarse_tree = DataTree::from_bytes(coarse, short).unwrap();
    let wider_tree = DataTree::from_bytes(wider_chunk, short).unwrap();
    assert_eq!(coarse_tree.data_tree_root(), wider_tree.data_tree_root());
    assert_ne!(coarse_tree.data_commitment(), wider_tree.data_commitment());

    // Same root and size. Only the geometry differs, so verify_size rejects the other.
    verify_size(
        &wide,
        &wide_tree.data_commitment(),
        &wide_tree.data_tree_root(),
        data_size,
    )
    .unwrap();
    assert!(
        verify_size(
            &narrow,
            &wide_tree.data_commitment(),
            &wide_tree.data_tree_root(),
            data_size,
        )
        .is_err()
    );
}

/// Break: two of `(data_size, chunk_size, segments_per_chunk)` share one encoding.
///
/// Each triple below is a valid geometry. Dropping a field, or writing two
/// fields without a fixed width, makes at least one pair equal.
#[test]
fn size_witness_differs_for_each_size_and_geometry() {
    let triples = [
        (1_u64, 8_u64, 1_u32),
        (1, 8, 2),
        (1, 16, 1),
        (1, 16, 2),
        (8, 8, 1),
        (8, 16, 1),
        (16, 8, 1),
        (16, 16, 2),
        (16, 32, 2),
        (32, 16, 2),
        (9, 8, 2),
        (255, 256, 256),
        (256, 256, 1),
        (256, 256, 256),
    ];
    let witnesses: Vec<SizeWitness> = triples
        .iter()
        .map(|(data_size, chunk_size, segments_per_chunk)| {
            witness_of(*data_size, *chunk_size, *segments_per_chunk)
        })
        .collect();
    for left in 0..witnesses.len() {
        for right in (left + 1)..witnesses.len() {
            assert_ne!(
                witnesses[left], witnesses[right],
                "{:?} and {:?} share a size witness",
                triples[left], triples[right]
            );
        }
    }

    // `max_data_tx_chunks` is not an input. Binding it would change the witness.
    let few_chunks = Geometry::new(32, 4, 1).unwrap();
    let many_chunks = Geometry::new(32, 4, GRID_MAX_CHUNKS).unwrap();
    let data = vec![1_u8; 32];
    assert_eq!(
        DataTree::from_bytes(few_chunks, &data)
            .unwrap()
            .size_witness(),
        DataTree::from_bytes(many_chunks, &data)
            .unwrap()
            .size_witness()
    );
}

/// Break: the preimage drops a field, changes a width, or uses little-endian.
#[test]
fn size_witness_preimage_is_nineteen_big_endian_bytes() {
    let geometry = Geometry::new(256, 256, 1).unwrap();
    let data_size = 1_u64;
    let tree = DataTree::from_bytes(geometry, &[0x11]).unwrap();
    let preimage = size_witness_preimage(&geometry, data_size);
    assert_eq!(
        preimage,
        [
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // data_size = 1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, // chunk_size = 256
            0x01, 0x00, // segments_per_chunk = 256
        ]
    );
    assert_eq!(
        tree.size_witness().as_bytes(),
        Sha256::digest(preimage).as_slice()
    );

    // Neither integer is a single-byte value, so a swapped endianness fails.
    let geometry = Geometry::new(0x0100, 1, 4).unwrap();
    let data_size = 0x0201_u64;
    let data = vec![0_u8; usize::try_from(data_size).unwrap()];
    let tree = DataTree::from_bytes(geometry, &data).unwrap();
    let preimage = size_witness_preimage(&geometry, data_size);
    assert_eq!(preimage.len(), SIZE_WITNESS_PREIMAGE_LEN);
    assert_eq!(&preimage[1..9], &data_size.to_be_bytes());
    assert_eq!(&preimage[9..17], &geometry.chunk_size().to_be_bytes());
    assert_eq!(&preimage[17..19], &1_u16.to_be_bytes());
    assert_eq!(
        tree.size_witness().as_bytes(),
        Sha256::digest(preimage).as_slice()
    );
}

fn witness_of(data_size: u64, chunk_size: u64, segments_per_chunk: u32) -> SizeWitness {
    let geometry = Geometry::new(chunk_size, segments_per_chunk, GRID_MAX_CHUNKS)
        .unwrap_or_else(|err| panic!("({data_size}, {chunk_size}, {segments_per_chunk}): {err}"));
    let data = vec![b'A'; usize::try_from(data_size).unwrap()];
    DataTree::from_bytes(geometry, &data)
        .unwrap_or_else(|err| {
            panic!("tree ({data_size}, {chunk_size}, {segments_per_chunk}): {err}")
        })
        .size_witness()
}

/// Independent preimage: tag, then three fixed-width big-endian integers (spec §3).
fn size_witness_preimage(geometry: &Geometry, data_size: u64) -> [u8; SIZE_WITNESS_PREIMAGE_LEN] {
    let mut preimage = [0_u8; SIZE_WITNESS_PREIMAGE_LEN];
    preimage[0] = TreeDomainTag::Size.as_byte();
    preimage[SIZE_WITNESS_TAG_LEN..SIZE_WITNESS_TAG_LEN + BE64_LEN]
        .copy_from_slice(&data_size.to_be_bytes());
    let chunk_size_at = SIZE_WITNESS_TAG_LEN + BE64_LEN;
    preimage[chunk_size_at..chunk_size_at + BE64_LEN]
        .copy_from_slice(&geometry.chunk_size().to_be_bytes());
    let segments_per_chunk = u16::try_from(geometry.segments_per_chunk())
        .expect("segments_per_chunk fits be16: Geometry::new caps it at 256");
    let segments_at = chunk_size_at + BE64_LEN;
    preimage[segments_at..segments_at + BE16_LEN]
        .copy_from_slice(&segments_per_chunk.to_be_bytes());
    preimage
}
