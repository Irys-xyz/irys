//! Shared vectors (spec §10).
//!
//! Each JSON file is regenerated from its pattern. Positive proofs must verify.
//! Negative cases must fail with the recorded error kind. Printed spec §10 hex is
//! compared to the files and is not adjusted to match a mismatch.
//!
//! Mutations match `examples/gen_vectors.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use irys_data_tree::{
    DataCommitment, DataTree, DataTreeRoot, Geometry, HASH_SIZE, MAX_SUBTREE_HEIGHT, NodeHash,
    SizeError, SubtreeHeight, VerifyError, chunk_key, chunk_leaf, chunk_root, data_commitment,
    depth, empty, leaf, node, size_witness, verify_chunk, verify_segment,
};
use serde::Deserialize;

/// spec §10: `b[i] = i mod 251`.
const MOD251_MODULUS: usize = 251;
const MOD251: &str = "mod251";
const ZEROS: &str = "zeros";

/// spec §10 primitive group: 4 leaves, node, 6 size witnesses, empty(0..=8),
/// 3 spec chunk keys plus the printed key at c = 3, and 5 chunk leaves.
const PRIMITIVE_CASES: usize = 29;
/// spec §10 testing root rows.
const TESTING_ROOT_CASES: usize = 7;
/// spec §10 mainnet sizes, each as mod251 and as zeros.
const MAINNET_ROOT_CASES: usize = 34;
/// Chunk counts 1..=17, plus 3 chunks of 1 GiB, plus the 4 chunks of the printed upload.
const CHUNK_PROOF_CASES: usize = 160;
/// Mainnet segments for chunk counts 1 and 2, four boundary segments, one partial, one printed,
/// plus segments 0 and 1 of the spec §5 mixed leaf.
const SEGMENT_PROOF_CASES: usize = 200;
/// spec §10 multi-segment group: m = 1, 2, 4, 64, one adjacent pair, one two-chunk pair.
const MULTI_SEGMENT_CASES: usize = 6;
/// spec §10 negative list, with one case per sibling bit of the printed segment proof,
/// plus the spec §5 verifier-rule cases.
const FIXED_NEGATIVE_CASES: usize = 24;

/// spec §10 `size_proof_wrong_size`: each `n` and its claimed sizes `n'`. One
/// chunk case and one segment case per pair. `n'` is `n − 1`, `n + 1`, the
/// nearest size with another chunk count, and the nearest segment boundary
/// with the same chunk count, in increasing order.
const SIZE_PROOF_CLAIMS: [(u64, &[u64]); 7] = [
    (1, &[2, 8, 33]),
    (8, &[7, 9, 16, 33]),
    (9, &[8, 10, 33]),
    (32, &[24, 31, 33]),
    (33, &[32, 34, 40]),
    (112, &[96, 104, 111, 113]),
    (157, &[156, 158, 160, 161]),
];

/// Two checks, chunk and segment, per claimed size.
const SIZE_PROOF_CASES: usize = 2 * (3 + 4 + 3 + 3 + 3 + 4 + 4);

/// The fixed cases, the moved index, the size-proof cases, the chunk root as
/// a leaf, and the padding slot.
const NEGATIVE_CASES: usize = FIXED_NEGATIVE_CASES + 1 + SIZE_PROOF_CASES + 2;

/// spec §5: test geometry, `n = 20`, one hash other than `empty(0)` at leaf 3.
const MIXED_LEAF_DATA_SIZE: u64 = 20;

/// One byte of the stand-in hash. The hash is [`HASH_SIZE`] copies of this byte.
const STAND_IN_HASH_BYTE: u8 = 0xAB;

/// Two digests, `l ‖ r`, each [`HASH_SIZE`] (spec §3, §10).
const TWO_DIGEST_SOURCE_DATA_SIZE: u64 = (HASH_SIZE * 2) as u64;

/// That segment is one chunk, so the recorded maximum is 1 (spec §2, §10).
///
/// `max_data_tx_chunks` is not an input to `size_witness` (spec §3).
const TWO_DIGEST_MAX_DATA_TX_CHUNKS: u32 = 1;

/// spec §10: test geometry, `n = 20`, claimed as `n' = 25` (`rightmost_segment_padding_slot`).
const PADDING_SLOT_DATA_SIZE: u64 = 20;
const PADDING_SLOT_CLAIMED_DATA_SIZE: u64 = 25;

/// spec §10 negative names before the chunk-leaf cases, in generator order
/// (`examples/gen_vectors.rs`).
const FIXED_NEGATIVE_NAMES: [&str; FIXED_NEGATIVE_CASES] = [
    "wrong_data_size_same_chunk_count",
    "wrong_data_size_other_chunk_count",
    "chunk_index_out_of_range",
    "proof_one_hash_too_long",
    "proof_one_hash_too_short",
    "empty_proof_for_chunk_count_above_one",
    "chunk_length_minus_one",
    "chunk_length_plus_one",
    "data_size_zero",
    "proof_at_another_index",
    "unanchored_commitment",
    "zero_padded_segment",
    "flip_bit_0_of_sibling_0",
    "flip_bit_0_of_sibling_1",
    "flip_bit_0_of_sibling_2",
    "flip_bit_0_of_sibling_3",
    "segment_equal_to_two_hashes_is_a_leaf",
    "pinned_sibling_not_empty_level_0",
    "pinned_sibling_not_empty_level_1",
    "mixed_leaf_segment_2",
    "mixed_leaf_segment_3",
    "replaced_chunk_witness",
    "replaced_segment_witness",
    "data_size_above_max",
];

/// Every negative name, in generator order.
fn negative_names() -> Vec<String> {
    let mut names: Vec<String> = FIXED_NEGATIVE_NAMES
        .iter()
        .map(ToString::to_string)
        .collect();
    names.push("chunk_proof_at_moved_index".to_string());
    for (data_size, claimed_sizes) in SIZE_PROOF_CLAIMS {
        for claimed_data_size in claimed_sizes {
            for check in ["chunk", "segment"] {
                names.push(format!(
                    "size_proof_wrong_size_{check}_{data_size}_as_{claimed_data_size}"
                ));
            }
        }
    }
    names.push("chunk_root_as_leaf".to_string());
    names.push("rightmost_segment_padding_slot".to_string());
    assert_eq!(names.len(), NEGATIVE_CASES);
    names
}

const FILE_NAMES: [&str; 7] = [
    "chunk_proofs.json",
    "multi_segment.json",
    "negative.json",
    "primitives.json",
    "roots_mainnet.json",
    "roots_testing.json",
    "segment_proofs.json",
];

#[test]
fn shared_vectors_match_the_crate_and_spec() {
    assert_file_set();
    let primitives: VectorFile<PrimitiveCase> = read_json("primitives.json");
    let roots_testing: VectorFile<RootCase> = read_json("roots_testing.json");
    let roots_mainnet: VectorFile<RootCase> = read_json("roots_mainnet.json");
    let chunk_proofs: VectorFile<ChunkProofCase> = read_json("chunk_proofs.json");
    let segment_proofs: VectorFile<SegmentProofCase> = read_json("segment_proofs.json");
    let multi_segment: VectorFile<MultiSegmentCase> = read_json("multi_segment.json");
    let negative: VectorFile<NegativeCase> = read_json("negative.json");

    assert_eq!(primitives.cases.len(), PRIMITIVE_CASES);
    assert_eq!(roots_testing.cases.len(), TESTING_ROOT_CASES);
    assert_eq!(roots_mainnet.cases.len(), MAINNET_ROOT_CASES);
    assert_eq!(chunk_proofs.cases.len(), CHUNK_PROOF_CASES);
    assert_eq!(segment_proofs.cases.len(), SEGMENT_PROOF_CASES);
    assert_eq!(multi_segment.cases.len(), MULTI_SEGMENT_CASES);
    assert_eq!(negative.cases.len(), NEGATIVE_CASES);
    assert_descriptions(
        &primitives.description,
        &roots_testing.description,
        &roots_mainnet.description,
        &chunk_proofs.description,
        &segment_proofs.description,
        &multi_segment.description,
        &negative.description,
    );
    assert!(primitives.geometry.is_none(), "primitives have no geometry");
    assert!(chunk_proofs.geometry.is_none());
    assert!(segment_proofs.geometry.is_none());
    assert_same_geometry(roots_testing.geometry.as_ref().unwrap(), Geometry::TESTING);
    assert_same_geometry(roots_mainnet.geometry.as_ref().unwrap(), Geometry::MAINNET);
    assert_same_geometry(multi_segment.geometry.as_ref().unwrap(), Geometry::MAINNET);
    assert_same_geometry(negative.geometry.as_ref().unwrap(), Geometry::TESTING);
    assert_eq!(
        Geometry::TESTING.max_data_tx_chunks(),
        Geometry::MAINNET.max_data_tx_chunks(),
        "testing and mainnet share the spec §2 maximum"
    );

    // Printed hex from spec §10. A mismatch is a bug in the crate or the generator.
    assert_printed_spec(
        &primitives,
        &roots_testing,
        &roots_mainnet,
        &chunk_proofs,
        &segment_proofs,
    );

    check_primitives(&primitives);
    check_roots(&roots_testing);
    check_roots(&roots_mainnet);
    check_chunk_proofs(&chunk_proofs);
    check_segment_proofs(&segment_proofs);
    check_multi_segment(&multi_segment);
    check_negative(&negative);
}

/// Generator description text (`examples/gen_vectors.rs`). A wording change
/// has to update both. A description is not a hash, so this equality is the pin.
fn assert_descriptions(
    primitives: &str,
    roots_testing: &str,
    roots_mainnet: &str,
    chunk_proofs: &str,
    segment_proofs: &str,
    multi_segment: &str,
    negative: &str,
) {
    assert_eq!(primitives, primitives_description());
    assert_eq!(
        roots_testing,
        concat!(
            "Testing-geometry roots (spec §10): chunk_size 32, segments_per_chunk 4. ",
            "Bytes are mod251, the pattern of the printed root table. ",
            "The case holds the chunk roots, the chunk leaves chunk_leaf(c, R_c), data_tree_root, ",
            "size_witness, and data_commitment."
        )
    );
    assert_eq!(
        roots_mainnet,
        concat!(
            "Mainnet roots (spec §4, §10). Each data_size is stored twice: mod251 bytes, then zero bytes. ",
            "The case holds the chunk roots, the chunk leaves chunk_leaf(c, R_c), data_tree_root, ",
            "size_witness, and data_commitment. ",
            "Chunk counts 8, 9, 16, and 17 are full uploads (data_size = chunk_count * chunk_size). ",
            "1 GiB is 2^30 bytes (4096 chunks). 1 GiB + 1 is the next byte."
        )
    );
    assert_eq!(
        chunk_proofs,
        concat!(
            "Chunk proofs, leaf to root, one lowercase hex string (spec §5, §10). ",
            "Mainnet geometry, mod251: every chunk of each full upload with chunk count 1 through 17 ",
            "(chunk count 5, chunk index 4 is the one-sibling proof, spec §4), ",
            "and chunks 0, 2047, and 4095 of 1 GiB. ",
            "Testing geometry, mod251: every chunk of the printed 112-byte upload, including chunk 2. ",
            "size_proof is true when chunk_index is rightmost_chunk(data_size) = chunk_count - 1, ",
            "computed from data_size (spec §5, §9). Every upload here has that case."
        )
    );
    assert_eq!(
        segment_proofs,
        concat!(
            "Segment proofs: in-chunk siblings, then the chunk proof, one hex string (spec §5, §10). ",
            "Mainnet geometry, mod251: every segment of every chunk for chunk count 1 and 2 (full uploads); ",
            "segments (3, 5), (3, 31), (3, 32), and (3, 63) of the 917504-byte upload; ",
            "and segment (0, 1) of 4097 bytes, which is 1 byte. ",
            "Testing geometry, mod251: the printed proof at data_size 112, chunk 3, segment 1. ",
            "Testing geometry, n = 20: segments 0 and 1 verify against the commitment whose leaf 3 ",
            "is 32 bytes of 0xAB instead of empty(0) (spec §5). replaced_leaf names that leaf. ",
            "size_proof is true when (chunk_index, segment_index) is rightmost_segment(data_size), ",
            "computed from data_size (spec §5, §9). Every honest upload here has that case."
        )
    );
    assert_eq!(
        multi_segment,
        concat!(
            "Multi-segment proofs, option B (spec §5, §10, §11 Q2). ",
            "Each segment stores its h in-chunk siblings, level 0 first. ",
            "The chunk proof is one hex string, leaf to root, shared by the segments of that chunk. ",
            "The upload is mainnet geometry and mod251 bytes, data_size 917504: ",
            "the spec §4 example of 3.5 chunks, so chunk 3 is partial ",
            "(segments 0 to 31 hold bytes, segments 32 to 63 are empty). ",
            "Cases on that upload: m = 1, 2, 4, and 64, the first m segments of chunk 0; ",
            "an adjacent pair in chunk 3, segments 31 and 32 ",
            "(the last full segment and the first empty segment); ",
            "a pair in two chunks, segment 0 of chunk 2 and segment 0 of chunk 3, two chunk proofs."
        )
    );
    assert_eq!(
        negative,
        concat!(
            "Rejected inputs (spec §10). expected_error is the VerifyError or SizeError variant name. ",
            "A size failure is the SizeError name. source_data_size rebuilds the tree. ",
            "data_size, chunk_index, segment_index, commitment, and proof are the verifier inputs. ",
            "The mutation names how those differ from the honest proof. ",
            "The 64-byte segment is left concatenated with right, checked against the commitment of ",
            "node(left, right). That segment is a leaf (spec §3). ",
            "The unanchored case records the re-derived commitment, which verifies, ",
            "and the anchored commitment, which does not (spec §6). ",
            "A pinned in-chunk sibling that is not empty(ℓ) is checked against the commitment of that proof. ",
            "The stand-in hash is 32 bytes of 0xAB. One case is level 0 and one is a higher level (spec §5). ",
            "Test geometry, n = 20, leaf 3 is that hash: segments 2 and 3 fail. ",
            "Segments 0 and 1 of that upload verify; they are in the segment-proof file. ",
            "An honest chunk proof and an honest segment proof whose last hash is size_witness(G, n+1) ",
            "fail before the climb. ",
            "data_size = max_data_size + 1 is out of range. That case records a one-chunk maximum. ",
            "chunk_proof_at_moved_index: the bytes and proof of chunk 2 of n = 96, the last hash ",
            "size_witness(G, 64), shown as chunk 1 of n' = 64 against the commitment re-derived for 64. ",
            "The steps match, and the chunk leaf hashes the index, so it fails (spec §6). ",
            "size_proof_wrong_size_<check>_<n>_as_<n'>: for n in 1, 8, 9, 32, 33, 112, and 157, ",
            "the honest rightmost chunk or rightmost data segment of the n'-byte upload and its proof, ",
            "at the indexes computed from n', against node(data_tree_root(n), size_witness(G, n')). ",
            "n' is n - 1, n + 1, the nearest size with another chunk count, and the nearest segment ",
            "boundary with the same chunk count (spec §9 size proof). ",
            "chunk_root_as_leaf: honest one-chunk bytes and proof against ",
            "node(chunk_root(bytes), size_witness(G, n)); the one-chunk root is chunk_leaf(0, R_0). ",
            "rightmost_segment_padding_slot: the honest siblings of segment 3 of n = 20, the last hash ",
            "size_witness(G, 25), and the one byte of segment 3 of the 25-byte upload, shown as the ",
            "rightmost data segment of n' = 25 against the commitment re-derived for 25."
        )
    );
}

fn primitives_description() -> String {
    format!(
        "Domain-separated hashes (spec §3, §10). \
         Leaves hash mod251 bytes of length 0, 1, 4095, and 4096. \
         node hashes the 0-byte leaf with the 1-byte leaf. \
         size_witness is mainnet geometry and covers 1, 4096, 262144, 917504, 2^40, and 5 TiB. \
         empty covers heights 0 through {MAX_SUBTREE_HEIGHT}. \
         chunk_key uses the testing-geometry commitment of 112 mod251 bytes. \
         Chunk indexes are 0, 1, and 2^24, plus 3 so the printed chunk_key is in this file. \
         2^24 is an input of the hash, not a chunk of that 112-byte tree. \
         chunk_leaf hashes (chunk_index, chunk_root): (0, leaf(\"\")), (1, empty(6)), (2^24, empty(6)), \
         (20971519, empty(6)), the last chunk index of a 5 TiB upload, and (3, R_3) of the \
         testing-geometry 112-byte mod251 upload, the printed chunk leaf."
    )
}

fn assert_file_set() {
    let mut names = Vec::new();
    for entry in fs::read_dir(vector_dir()).expect("vectors directory") {
        let entry = entry.expect("vector entry");
        let name = entry.file_name();
        let name = name.to_str().expect("vector file name");
        if name.ends_with(".json") {
            names.push(name.to_string());
        }
    }
    names.sort();
    assert_eq!(names, FILE_NAMES);
}

fn assert_printed_spec(
    primitives: &VectorFile<PrimitiveCase>,
    roots_testing: &VectorFile<RootCase>,
    roots_mainnet: &VectorFile<RootCase>,
    chunk_proofs: &VectorFile<ChunkProofCase>,
    segment_proofs: &VectorFile<SegmentProofCase>,
) {
    assert_leaf(
        primitives,
        0,
        "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
    );
    assert_empty(
        primitives,
        6,
        "2be3cf0551cc6936d461e3dc43f3c4bf50cbee1bc091925254e879f4e7665e94",
    );
    // SPC 128 and SPC 256: heights 7 and 8, the top of the empty table (spec §2, §10).
    assert_empty(
        primitives,
        MAX_SUBTREE_HEIGHT - 1,
        "12db5262a5500d2516b8f82362d2a87278d20f712ff1fce2019d42ecba17241d",
    );
    assert_empty(
        primitives,
        MAX_SUBTREE_HEIGHT,
        "1a1a9265f869676c206824aa7bfc2fe8c7fe34691dddfb35797b6a321f977dfc",
    );
    assert_size_witness(
        primitives,
        1,
        "e677df7f858cd910622cfd5c6ac7bbf6200e7829c3e4a7325590a6b6309c0597",
    );
    assert_size_witness(
        primitives,
        4_096,
        "c0c06f04b271e626a424dad90c9ad30189e210d4b9690f231d9147cdeca448f0",
    );
    assert_size_witness(
        primitives,
        262_144,
        "86b2be42530761806437d0632cfb93c56f9294c8cc1c8776ad01d08b3078fa49",
    );
    assert_size_witness(
        primitives,
        917_504,
        "438a131aaf3445c16ab8b8f66374f8ec253f8bce7acc9efd91f722e2409ef89e",
    );
    assert_spec_roots(roots_testing, TESTING_ROOT_ROWS);
    assert_spec_roots(roots_mainnet, MAINNET_ROOT_ROWS);
    assert_spec_chunk_proof(chunk_proofs);
    assert_spec_segment_proof(segment_proofs);
    assert_spec_chunk_key(primitives);
    assert_spec_chunk_leaves(primitives, roots_testing);
}

/// spec §10 printed chunk leaves. For `C = 1` the `data_tree_root` is
/// `chunk_leaf(0, R_0)`. `n = 112` prints `R_3` and `chunk_leaf(3, R_3)`.
fn assert_spec_chunk_leaves(
    primitives: &VectorFile<PrimitiveCase>,
    roots_testing: &VectorFile<RootCase>,
) {
    for (data_size, chunk_leaf_hex) in [
        (
            1,
            "0f5fe2ca48d3d3f37848fc851257fc094c8b27feaa82c905b2b8ffbd177f6543",
        ),
        (
            32,
            "5a3b3c37b21874d69166aa06c52a0b5dd87ec378187adab4bc9d4ee42eb0ef0d",
        ),
    ] {
        let case = find_root(roots_testing, MOD251, data_size);
        assert_eq!(
            case.chunk_leaves,
            [chunk_leaf_hex],
            "spec §10 n = {data_size}"
        );
        assert_eq!(
            case.data_tree_root, chunk_leaf_hex,
            "spec §10 n = {data_size}"
        );
    }
    let printed_root = "dbc3c2433981a503f8ffa38629a9ec5d41501618b2afd860af52d6a93e06e8b6";
    let printed_leaf = "ec954565828ae61fff557dcb9cc3b4e6a5da4b5417db597bffc7e9d6ac716332";
    let case = find_root(roots_testing, MOD251, 112);
    assert_eq!(case.chunk_roots[3], printed_root, "spec §10 n = 112 R_3");
    assert_eq!(case.chunk_leaves[3], printed_leaf, "spec §10 n = 112 L_3");
    assert!(
        primitives.cases.iter().any(|primitive| matches!(
            primitive,
            PrimitiveCase::ChunkLeaf { chunk_index: 3, chunk_root, hash }
                if chunk_root == printed_root && hash == printed_leaf
        )),
        "spec §10 chunk_leaf(3, R_3) primitive"
    );
}

/// `(data_size, chunk_count, data_tree_root, data_commitment)` from spec §10. mod251.
const TESTING_ROOT_ROWS: &[(u64, u32, &str, &str)] = &[
    (
        1,
        1,
        "0f5fe2ca48d3d3f37848fc851257fc094c8b27feaa82c905b2b8ffbd177f6543",
        "6766c8a6ed3afeaeb38d74b508b774ee33c8396d1061ac2bd272bedd02f5c421",
    ),
    (
        8,
        1,
        "bed46322520adad4b5c1808a4a30269e4fdbf89e89b83a1b8439442c01db2c83",
        "e35809d4fdba5e31164f9e58ac91d659a117776eb7748cd7e518239039a92f2b",
    ),
    (
        9,
        1,
        "ffa37bcad0b5968333e8f06b23b1f4cc430d4ae8a37a344de34ffc631dd8bf21",
        "515b428e3fa513840a5cf842a30d364a47d3cba26d7a483c845431d7ffebc852",
    ),
    (
        32,
        1,
        "5a3b3c37b21874d69166aa06c52a0b5dd87ec378187adab4bc9d4ee42eb0ef0d",
        "c21dea135c5673eb07cf2d641d7893b22ca83dd0986dc51b09b99b29026fdd6f",
    ),
    (
        33,
        2,
        "3bf5a0750be0980d8941fdf6778ba8b61b633599aee08a7d710d8566a4baa787",
        "c6ae46330fe254aebad232ff69bd954da161053bf629b8a64f130a4b2a4eb54d",
    ),
    (
        112,
        4,
        "0a22641c8d9f53afbe333e832a5187307f4d4f31a0b43dad7ea3dfc0f84deb6e",
        "82204503a718db465b3eeecea24b2896411211af0080555657eb2098fb969620",
    ),
    (
        157,
        5,
        "ca4c355dba86031d9f6eb8d8955305abf03cdc51de06ed264873a247148d64e1",
        "74e1e5bff4f5047875b49f5d20afc762335d3c07002b8e55acba6e65357ba20d",
    ),
];

/// `(data_size, chunk_count, data_tree_root, data_commitment)` from spec §10. mod251.
const MAINNET_ROOT_ROWS: &[(u64, u32, &str, &str)] = &[
    (
        1,
        1,
        "646847d975529f524fd1f887a1dbdfb4514bece22e63e115d8a27b596bc5b827",
        "a345efb5e682e6b6d54bf57e9d5632976c1ccef897b0214a1c4c2b4cece7eb5a",
    ),
    (
        4_095,
        1,
        "ead4d363fdb5a39a35e89c15b67574c9116018ad3f02264c1ab7a7248f44b632",
        "8994d60a594d001d6511afd273403fdc41c48e5e7b744a640ffab046737959b0",
    ),
    (
        4_096,
        1,
        "b298a715260a196611a1a200298372756ad785dd459933d83ddfde245cd93bed",
        "46e6159e0bfad886ad65fd1a4c48ae7f93160944353e7d678a6d2a2f14178d30",
    ),
    (
        4_097,
        1,
        "2a6d480a36e68670061d18861bb2c4308663b7c26963d4b13fbc5ccf42b463d2",
        "d57e36e3153ab9be17bd7cb1421909fbfe4360ee9d6d126d6a8705dcdf1e030f",
    ),
    (
        262_143,
        1,
        "32ca509db863c5bccc517b8f6e94927da71404e1023f89c93390ff58af9326da",
        "2bb9b4b7a3b6eeecb1e99eae46cd85efd969619f2d65e649628c10f4d53d544c",
    ),
    (
        262_144,
        1,
        "cc7b595dd3aecd15cfb5ce7a733f449e63c05545a8d5f9e8e67d5bf4b6069b0d",
        "9821bdbe69355f8a624164e5d73fb676e2cfb7af7483327cf6fc922c0b873b39",
    ),
    (
        262_145,
        2,
        "5504c86e3cf316bf05ecd96a2585c3b09781382c25f6c664b5330a5a444c3101",
        "d9243949c7c500fa0606a8a7d4ad4285c727e81944f42d283f49f0d0544e770c",
    ),
    (
        917_504,
        4,
        "7f4f92981ff51822adf2c7f7a394a9536e764256c8bd8b4d5daef31a62b8de07",
        "0e751421c6429cba0e9ac6edc8c32edb88006e0d052518f6bd933d4b2a000d5e",
    ),
    (
        1_310_720,
        5,
        "29f12d7ecbcface82c0ca4787c60bd6d6a0fb271a173599c977eee68aff59731",
        "bac42e0026f41eff11d056d4ee30904dcf963734eef88c310c9a950f72ab0069",
    ),
];

fn assert_spec_roots(file: &VectorFile<RootCase>, rows: &[(u64, u32, &str, &str)]) {
    for &(data_size, chunk_count, root, commitment) in rows {
        let case = find_root(file, MOD251, data_size);
        assert_eq!(
            case.chunk_count, chunk_count,
            "spec §10 chunk count {data_size}"
        );
        assert_eq!(
            case.data_tree_root, root,
            "spec §10 data_tree_root {data_size}"
        );
        assert_eq!(
            case.data_commitment, commitment,
            "spec §10 data_commitment {data_size}"
        );
    }
}

fn assert_spec_chunk_proof(file: &VectorFile<ChunkProofCase>) {
    let case = file
        .cases
        .iter()
        .find(|case| {
            case.data_size == 112 && case.chunk_index == 2 && case.geometry.chunk_size == 32
        })
        .expect("spec §10 chunk proof");
    // spec §10: the generator printed the siblings. The last hash is the witness.
    let siblings = concat!(
        "ec954565828ae61fff557dcb9cc3b4e6a5da4b5417db597bffc7e9d6ac716332",
        "4bde04027ed46b4dc676b850d95e5b4f8ab4092f4c374528905967fbc157e399",
    );
    assert!(
        case.proof.starts_with(siblings),
        "spec §10 n=112 chunk proof siblings"
    );
    assert_eq!(
        case.proof.len(),
        siblings.len() + HASH_SIZE * 2,
        "spec §10 n=112 chunk proof length"
    );
    assert_eq!(
        &case.proof[siblings.len()..],
        hex_of(size_witness(&geometry_of(&case.geometry), 112).as_bytes()),
        "spec §10 n=112 chunk proof size witness"
    );
    assert_eq!(
        case.data_commitment,
        "82204503a718db465b3eeecea24b2896411211af0080555657eb2098fb969620"
    );
}

fn assert_spec_segment_proof(file: &VectorFile<SegmentProofCase>) {
    let case = file
        .cases
        .iter()
        .find(|case| {
            case.data_size == 112
                && case.chunk_index == 3
                && case.segment_index == 1
                && case.geometry.chunk_size == 32
        })
        .expect("spec §10 segment proof");
    // spec §10: the generator printed the siblings. The last hash is the witness.
    let siblings = concat!(
        "82bdff7cd7950d248431b84dd588f1777777ee141ebc86ac8f72080aaf11e0e5",
        "fe43d66afa4a9a5c4f9c9da89f4ffb52635c8f342e7ffb731d68e36c5982072a",
        "fbc10578ab6a756a167ab57c071797202dd7a56d16acca9b268c230c87b0a7cc",
        "4bde04027ed46b4dc676b850d95e5b4f8ab4092f4c374528905967fbc157e399",
    );
    assert!(
        case.proof.starts_with(siblings),
        "spec §10 n=112 segment proof siblings"
    );
    assert_eq!(
        case.proof.len(),
        siblings.len() + HASH_SIZE * 2,
        "spec §10 n=112 segment proof length"
    );
    assert_eq!(
        &case.proof[siblings.len()..],
        hex_of(size_witness(&geometry_of(&case.geometry), 112).as_bytes()),
        "spec §10 n=112 segment proof size witness"
    );
}

fn assert_spec_chunk_key(file: &VectorFile<PrimitiveCase>) {
    let Some(PrimitiveCase::ChunkKey {
        data_size,
        chunk_index,
        commitment,
        hash,
        ..
    }) = file
        .cases
        .iter()
        .find(|case| matches!(case, PrimitiveCase::ChunkKey { chunk_index: 3, .. }))
    else {
        panic!("spec §10 chunk_key");
    };
    assert_eq!(*data_size, 112);
    assert_eq!(*chunk_index, 3);
    assert_eq!(
        *commitment,
        "82204503a718db465b3eeecea24b2896411211af0080555657eb2098fb969620"
    );
    assert_eq!(
        *hash,
        "507655a3cf308a380bd30de15bade83e504e6271cbba0482de73d5e58fe37bbc"
    );
}

fn assert_leaf(file: &VectorFile<PrimitiveCase>, data_size: u64, hash: &str) {
    let Some(PrimitiveCase::Leaf {
        pattern,
        hash: actual,
        ..
    }) = file.cases.iter().find(
        |case| matches!(case, PrimitiveCase::Leaf { data_size: size, .. } if *size == data_size),
    )
    else {
        panic!("spec §10 leaf {data_size}");
    };
    assert_eq!(pattern, MOD251);
    assert_eq!(actual, hash, "spec §10 leaf {data_size}");
}

fn assert_empty_heights(file: &VectorFile<PrimitiveCase>) {
    let heights: Vec<u32> = file
        .cases
        .iter()
        .filter_map(|case| match case {
            PrimitiveCase::Empty { empty_height, .. } => Some(*empty_height),
            _ => None,
        })
        .collect();
    let expected: Vec<u32> = (0..=MAX_SUBTREE_HEIGHT).collect();
    assert_eq!(
        heights, expected,
        "spec §10 pins empty(0) through empty(MAX_SUBTREE_HEIGHT)"
    );
}

fn assert_empty(file: &VectorFile<PrimitiveCase>, empty_height: u32, hash: &str) {
    let Some(PrimitiveCase::Empty { hash: actual, .. }) = file.cases.iter().find(
        |case| matches!(case, PrimitiveCase::Empty { empty_height: height, .. } if *height == empty_height),
    ) else {
        panic!("spec §10 empty {empty_height}");
    };
    assert_eq!(actual, hash, "spec §10 empty {empty_height}");
}

fn assert_size_witness(file: &VectorFile<PrimitiveCase>, data_size: u64, hash: &str) {
    let Some(PrimitiveCase::SizeWitness { hash: actual, .. }) = file.cases.iter().find(
        |case| matches!(case, PrimitiveCase::SizeWitness { data_size: size, .. } if *size == data_size),
    ) else {
        panic!("spec §10 size_witness {data_size}");
    };
    assert_eq!(actual, hash, "spec §10 size_witness {data_size}");
}

fn find_root<'a>(file: &'a VectorFile<RootCase>, pattern: &str, data_size: u64) -> &'a RootCase {
    file.cases
        .iter()
        .find(|case| case.pattern == pattern && case.data_size == data_size)
        .unwrap_or_else(|| panic!("missing root {pattern} {data_size}"))
}

fn check_primitives(file: &VectorFile<PrimitiveCase>) {
    assert_empty_heights(file);
    for case in &file.cases {
        match case {
            PrimitiveCase::Leaf {
                pattern,
                data_size,
                hash,
            } => {
                let bytes = data_bytes(pattern, *data_size);
                assert_eq!(hex_of(leaf(&bytes).as_bytes()), *hash, "leaf {data_size}");
            }
            PrimitiveCase::Node { left, right, hash } => {
                let left_hash = leaf(&data_bytes(&left.pattern, left.data_size));
                let right_hash = leaf(&data_bytes(&right.pattern, right.data_size));
                assert_eq!(
                    hex_of(node(&left_hash, &right_hash).as_bytes()),
                    *hash,
                    "node"
                );
            }
            PrimitiveCase::SizeWitness {
                data_size,
                geometry,
                hash,
            } => {
                assert_spec_max_data_tx_chunks(
                    geometry.max_data_tx_chunks,
                    &format!("size_witness {data_size}"),
                );
                let geometry = geometry_of(geometry);
                assert_eq!(
                    hex_of(size_witness(&geometry, *data_size).as_bytes()),
                    *hash,
                    "size_witness {data_size}"
                );
            }
            PrimitiveCase::Empty { empty_height, hash } => {
                assert_eq!(
                    hex_of(empty_at_height(*empty_height).as_bytes()),
                    *hash,
                    "empty {empty_height}"
                );
            }
            PrimitiveCase::ChunkKey {
                pattern,
                data_size,
                geometry,
                chunk_index,
                commitment,
                hash,
            } => {
                assert_spec_max_data_tx_chunks(
                    geometry.max_data_tx_chunks,
                    &format!("chunk_key {chunk_index}"),
                );
                let geometry = geometry_of(geometry);
                let data = data_bytes(pattern, *data_size);
                let tree = DataTree::from_bytes(geometry, &data)
                    .unwrap_or_else(|err| panic!("chunk_key tree: {err}"));
                assert_eq!(hex_of(tree.data_commitment().as_bytes()), *commitment);
                // `2^24` is a hash input. It is not an index in the 112-byte tree (spec §3, §10).
                let key = chunk_key(&tree.data_commitment(), *chunk_index);
                assert_eq!(hex_of(key.as_bytes()), *hash, "chunk_key {chunk_index}");
            }
            PrimitiveCase::ChunkLeaf {
                chunk_index,
                chunk_root,
                hash,
            } => {
                let root = NodeHash::from(decode_hash(chunk_root));
                assert_eq!(
                    hex_of(chunk_leaf(*chunk_index, &root).as_bytes()),
                    *hash,
                    "chunk_leaf {chunk_index}"
                );
            }
        }
    }
}

fn check_roots(file: &VectorFile<RootCase>) {
    let geometry = file.geometry.as_ref().expect("root file has a geometry");
    let groups = group_uploads(&file.cases, |case| {
        UploadKey::new(geometry, &case.pattern, case.data_size)
    });
    for (key, cases) in groups {
        let (_data, tree) = build_upload_with_bytes(&key);
        for case in cases {
            let label = format!("{} {}", case.pattern, case.data_size);
            assert_eq!(tree.chunk_count(), case.chunk_count, "{label}");
            assert_eq!(
                hex_of(tree.data_tree_root().as_bytes()),
                case.data_tree_root,
                "{label}"
            );
            assert_eq!(
                hex_of(tree.size_witness().as_bytes()),
                case.size_witness,
                "{label}"
            );
            assert_eq!(
                hex_of(tree.data_commitment().as_bytes()),
                case.data_commitment,
                "{label}"
            );
            assert_eq!(
                case.chunk_roots.len(),
                usize::try_from(case.chunk_count).unwrap(),
                "{label}"
            );
            assert_eq!(case.chunk_leaves.len(), case.chunk_roots.len(), "{label}");
            for (chunk_index, (expected_root, expected_leaf)) in
                case.chunk_roots.iter().zip(&case.chunk_leaves).enumerate()
            {
                let chunk_index = u32::try_from(chunk_index).expect("chunk index fits");
                // spec §4: layer 0 holds `chunk_leaf(c, R_c)`. The stored leaf
                // binds the file's chunk root without hashing the chunk again.
                // `tests/spec_reference.rs` re-derives each root from the bytes.
                let root = NodeHash::from(decode_hash(expected_root));
                let stored_leaf = tree
                    .chunk_leaf(chunk_index)
                    .unwrap_or_else(|err| panic!("{label}: {err}"));
                assert_eq!(
                    stored_leaf,
                    chunk_leaf(chunk_index, &root),
                    "{label} chunk root {chunk_index}"
                );
                assert_eq!(
                    hex_of(stored_leaf.as_bytes()),
                    *expected_leaf,
                    "{label} chunk leaf {chunk_index}"
                );
            }
        }
    }
}

fn check_chunk_proofs(file: &VectorFile<ChunkProofCase>) {
    let groups = group_uploads(&file.cases, |case| {
        UploadKey::new(&case.geometry, &case.pattern, case.data_size)
    });
    for (key, cases) in groups {
        let geometry = key.geometry();
        let (data, tree) = build_upload_with_bytes(&key);
        for case in cases {
            let label = format!("chunk {} {}", case.data_size, case.chunk_index);
            assert_spec_max_data_tx_chunks(case.geometry.max_data_tx_chunks, &label);
            assert_eq!(case.pattern, MOD251, "{label}");
            assert_eq!(tree.chunk_count(), case.chunk_count, "{label}");
            assert_eq!(
                depth(case.chunk_index, case.chunk_count),
                Some(case.depth),
                "{label}"
            );
            // spec §5 `rightmost_chunk(n) = C − 1`, computed from `n`.
            assert_eq!(
                case.size_proof,
                case.chunk_index == tree.chunk_count() - 1,
                "{label} size_proof"
            );
            assert_eq!(
                hex_of(tree.data_commitment().as_bytes()),
                case.data_commitment,
                "{label}"
            );
            let proof = tree
                .chunk_proof(case.chunk_index)
                .unwrap_or_else(|err| panic!("{label}: {err}"));
            assert_eq!(hex_of(&proof), case.proof, "{label}");
            let chunk = chunk_slice(&geometry, &data, case.data_size, case.chunk_index);
            verify_chunk(
                &geometry,
                &tree.data_commitment(),
                case.data_size,
                case.chunk_index,
                chunk,
                &proof,
            )
            .unwrap_or_else(|err| panic!("{label}: {err}"));
        }
    }
}

fn check_segment_proofs(file: &VectorFile<SegmentProofCase>) {
    let (mixed, honest): (Vec<&SegmentProofCase>, Vec<&SegmentProofCase>) = file
        .cases
        .iter()
        .partition(|case| case.replaced_leaf.is_some());
    assert_eq!(honest.len() + mixed.len(), SEGMENT_PROOF_CASES);
    check_mixed_leaf_accepts(&mixed);
    let groups = group_segment_proofs(&honest);
    for (key, cases) in groups {
        let geometry = key.geometry();
        let (data, tree) = build_upload_with_bytes(&key);
        for case in cases {
            let label = format!(
                "segment {} chunk {} segment {}",
                case.data_size, case.chunk_index, case.segment_index
            );
            assert_spec_max_data_tx_chunks(case.geometry.max_data_tx_chunks, &label);
            assert_eq!(case.pattern, MOD251, "{label}");
            assert_eq!(tree.chunk_count(), case.chunk_count, "{label}");
            assert_eq!(
                hex_of(tree.data_commitment().as_bytes()),
                case.data_commitment,
                "{label}"
            );
            let segment_length = geometry
                .segment_len(case.data_size, case.chunk_index, case.segment_index)
                .unwrap_or_else(|err| panic!("{label}: {err}"));
            assert_eq!(segment_length, case.segment_length, "{label}");
            assert_eq!(
                case.size_proof,
                (case.chunk_index, case.segment_index)
                    == rightmost_segment(&geometry, case.data_size),
                "{label} size_proof"
            );
            let chunk = chunk_slice(&geometry, &data, case.data_size, case.chunk_index);
            let proof = tree
                .segment_proof(case.chunk_index, case.segment_index, chunk)
                .unwrap_or_else(|err| panic!("{label}: {err}"));
            assert_eq!(hex_of(&proof), case.proof, "{label}");
            let segment = segment_slice(
                &geometry,
                &data,
                case.data_size,
                case.chunk_index,
                case.segment_index,
            );
            verify_segment(
                &geometry,
                &tree.data_commitment(),
                case.data_size,
                case.chunk_index,
                case.segment_index,
                segment,
                &proof,
            )
            .unwrap_or_else(|err| panic!("{label}: {err}"));
        }
    }
}

fn check_multi_segment(file: &VectorFile<MultiSegmentCase>) {
    let geometry_json = file
        .geometry
        .as_ref()
        .expect("multi-segment file has a geometry");
    let geometry = geometry_of(geometry_json);
    let data_size = half_past_three_chunks(&geometry);
    let shapes = multi_segment_shapes(&geometry, data_size);
    assert_eq!(file.cases.len(), shapes.len());
    assert_eq!(shapes.len(), MULTI_SEGMENT_CASES);

    let groups = group_uploads(&file.cases, |case| {
        UploadKey::new(geometry_json, &case.pattern, case.data_size)
    });
    assert_eq!(groups.len(), 1, "the multi-segment cases share one upload");
    for (key, cases) in groups {
        let (data, tree) = build_upload_with_bytes(&key);
        assert_eq!(cases.len(), shapes.len());
        for (case, shape) in cases.iter().zip(&shapes) {
            check_one_multi_segment(&geometry, data_size, &data, &tree, case, shape);
        }
    }
}

/// spec §4: 3.5 mainnet chunks. The last chunk is partial.
fn half_past_three_chunks(geometry: &Geometry) -> u64 {
    let chunk_size = geometry.chunk_size();
    let data_size = 3 * chunk_size + chunk_size / 2;
    assert_eq!(data_size, 917_504, "spec §4");
    let chunk_count = geometry.chunk_count(data_size).expect("data size in range");
    assert_eq!(chunk_count, 4, "spec §4");
    let last_chunk_index = chunk_count - 1;
    let last_chunk_length = geometry
        .chunk_len(data_size, last_chunk_index)
        .expect("last chunk in range");
    assert!(last_chunk_length < chunk_size, "the last chunk is partial");
    data_size
}

/// The six spec §10 shapes, in generator order.
fn multi_segment_shapes(geometry: &Geometry, data_size: u64) -> Vec<Vec<(u32, Vec<u32>)>> {
    assert_eq!(
        geometry.segments_per_chunk(),
        64,
        "spec §10: m = 64 is every segment of one mainnet chunk"
    );
    let chunk_count = geometry.chunk_count(data_size).expect("data size in range");
    let last_chunk_index = chunk_count - 1;
    let mut shapes = Vec::new();
    // Chunk 0 is full. m = 1, 2, 4, and 64 are its first m segments.
    for segment_count in [1_u32, 2, 4, geometry.segments_per_chunk()] {
        let segments: Vec<u32> = (0..segment_count).collect();
        shapes.push(vec![(0, segments)]);
    }
    // Half of the last chunk holds bytes (spec §4). The next segment is empty.
    let first_empty_segment = geometry.segments_per_chunk() / 2;
    let last_full_segment = first_empty_segment - 1;
    assert_eq!(
        first_empty_segment,
        last_full_segment + 1,
        "the pair is adjacent"
    );
    shapes.push(vec![(
        last_chunk_index,
        vec![last_full_segment, first_empty_segment],
    )]);
    shapes.push(vec![
        (last_chunk_index - 1, vec![0]),
        (last_chunk_index, vec![0]),
    ]);
    assert!(
        shapes.iter().any(|shape| shape
            .iter()
            .any(|(chunk_index, _)| *chunk_index == last_chunk_index)),
        "one case touches the last chunk"
    );
    shapes
}

fn check_one_multi_segment(
    geometry: &Geometry,
    data_size: u64,
    data: &[u8],
    tree: &DataTree,
    case: &MultiSegmentCase,
    shape: &[(u32, Vec<u32>)],
) {
    let label = format!("multi-segment {}", case.data_size);
    assert_eq!(case.pattern, MOD251, "{label}");
    assert_eq!(case.data_size, data_size, "{label}");
    assert_eq!(
        hex_of(tree.data_commitment().as_bytes()),
        case.data_commitment,
        "{label}"
    );
    assert_eq!(case.chunks.len(), shape.len(), "{label}");
    if case.chunks.len() == 2 {
        assert_ne!(
            case.chunks[0].chunk_proof, case.chunks[1].chunk_proof,
            "{label}: two chunks, two proofs"
        );
    }
    for (chunk, (chunk_index, segment_indexes)) in case.chunks.iter().zip(shape) {
        assert_eq!(chunk.chunk_index, *chunk_index, "{label}");
        let chunk_label = format!("{label} chunk {}", chunk.chunk_index);
        let chunk_proof = tree
            .chunk_proof(chunk.chunk_index)
            .unwrap_or_else(|err| panic!("{chunk_label}: {err}"));
        let stored_chunk_proof = decode_hex(&chunk.chunk_proof, &chunk_label);
        assert_eq!(stored_chunk_proof, chunk_proof, "{chunk_label}");
        let chunk_bytes = chunk_slice(geometry, data, case.data_size, chunk.chunk_index);
        assert_eq!(chunk.segments.len(), segment_indexes.len(), "{chunk_label}");
        for (segment, &segment_index) in chunk.segments.iter().zip(segment_indexes) {
            assert_eq!(segment.segment_index, segment_index, "{chunk_label}");
            let segment_label = format!("{chunk_label} segment {segment_index}");
            let segment_proof = tree
                .segment_proof(chunk.chunk_index, segment_index, chunk_bytes)
                .unwrap_or_else(|err| panic!("{segment_label}: {err}"));
            // spec §5: in-chunk siblings concatenated with the chunk proof.
            // The chunk proof ends with the size witness, so this is the whole
            // proof in every case, including one chunk.
            let in_chunk_len = usize::try_from(geometry.height()).expect("height fits") * HASH_SIZE;
            assert_eq!(
                segment_proof.len(),
                in_chunk_len + chunk_proof.len(),
                "{segment_label}"
            );
            let stored_siblings = decode_hex(&segment.in_chunk_siblings, &segment_label);
            assert_eq!(
                stored_siblings,
                segment_proof[..in_chunk_len],
                "{segment_label}"
            );
            assert_eq!(
                &segment_proof[in_chunk_len..],
                chunk_proof.as_slice(),
                "{segment_label}"
            );
            let mut proof = stored_siblings;
            proof.extend_from_slice(&stored_chunk_proof);
            let segment_bytes = segment_slice(
                geometry,
                data,
                case.data_size,
                chunk.chunk_index,
                segment_index,
            );
            verify_segment(
                geometry,
                &tree.data_commitment(),
                case.data_size,
                chunk.chunk_index,
                segment_index,
                segment_bytes,
                &proof,
            )
            .unwrap_or_else(|err| panic!("{segment_label}: {err}"));
        }
    }
}

fn check_negative(file: &VectorFile<NegativeCase>) {
    let names: Vec<String> = file.cases.iter().map(|case| case.name.clone()).collect();
    assert_eq!(names, negative_names());
    let file_geometry = file.geometry.as_ref().expect("negative file geometry");
    for case in &file.cases {
        if let Some(geometry) = &case.geometry {
            if case.mutation == Mutation::SegmentEqualsLeftAndRight {
                assert_eq!(
                    geometry.max_data_tx_chunks, TWO_DIGEST_MAX_DATA_TX_CHUNKS,
                    "{}",
                    case.name
                );
            } else if case.mutation == Mutation::DataSizeAboveMax {
                assert_eq!(geometry.max_data_tx_chunks, 1, "{}", case.name);
            } else {
                assert_spec_max_data_tx_chunks(geometry.max_data_tx_chunks, &case.name);
            }
        }
        let geometry = geometry_of(case.geometry.as_ref().unwrap_or(file_geometry));
        match case.mutation {
            Mutation::SegmentEqualsLeftAndRight => check_two_digest_segment(&geometry, case),
            Mutation::IndexOutOfRange | Mutation::DataSizeZero => check_range_case(&geometry, case),
            Mutation::ProofFromOtherChunk => check_other_index(&geometry, case),
            Mutation::ZeroPadSegment | Mutation::FlipSiblingBit0 => {
                check_segment_mutation(&geometry, case)
            }
            Mutation::HonestChunk
            | Mutation::RederivedWitness
            | Mutation::AppendOneZeroHash
            | Mutation::DropLastHash
            | Mutation::EmptyProof
            | Mutation::DropLastChunkByte
            | Mutation::AppendZeroChunkByte => check_chunk_mutation(&geometry, case),
            Mutation::PinnedSiblingNotEmpty => check_pinned_sibling(&geometry, case),
            Mutation::BadEmptyLeaf => check_bad_empty_leaf(&geometry, case),
            Mutation::ReplacedSizeWitness => check_replaced_size_witness(&geometry, case),
            Mutation::DataSizeAboveMax => check_data_size_above_max(&geometry, case),
            Mutation::ChunkProofAtMovedIndex => check_chunk_proof_at_moved_index(&geometry, case),
            Mutation::RightmostProofOfClaimedSize => {
                check_rightmost_proof_of_claimed_size(&geometry, case)
            }
            Mutation::CommitmentOverChunkRoot => check_commitment_over_chunk_root(&geometry, case),
            Mutation::PaddingSlotAsRightmostSegment => check_padding_slot(&geometry, case),
        }
    }
}

fn check_range_case(geometry: &Geometry, case: &NegativeCase) {
    let tree = build_pattern(geometry, case);
    assert_eq!(
        hex_of(tree.data_commitment().as_bytes()),
        case.commitment,
        "{}",
        case.name
    );
    assert!(case.proof.is_empty(), "{}", case.name);
    expect_chunk_error(geometry, &tree.data_commitment(), case, &[], &[]);
}

fn check_chunk_mutation(geometry: &Geometry, case: &NegativeCase) {
    let (data, tree) = build_pattern_with_bytes(geometry, case);
    let chunk = chunk_slice(geometry, &data, case.source_data_size, case.chunk_index);
    let honest = tree
        .chunk_proof(case.chunk_index)
        .unwrap_or_else(|err| panic!("{}: {err}", case.name));
    let mut proof = mutate_chunk_proof(&honest, case.mutation);
    if case.unanchored_commitment.is_some() {
        // spec §6: the proof's last hash is size_witness of the claimed size.
        // The true witness is rejected before the climb (spec §5).
        assert_eq!(
            case.mutation,
            Mutation::RederivedWitness,
            "{}: the last hash is size_witness of the claimed size",
            case.name
        );
        let witness = size_witness(geometry, case.data_size);
        let start = proof.len() - HASH_SIZE;
        proof[start..].copy_from_slice(witness.as_bytes());
    }
    let bytes = mutate_chunk_bytes(chunk, case.mutation);
    assert_eq!(hex_of(&proof), case.proof, "{}", case.name);
    assert_eq!(
        hex_of(tree.data_commitment().as_bytes()),
        case.commitment,
        "{}",
        case.name
    );
    if let Some(rederived_hex) = &case.unanchored_commitment {
        // spec §6: the re-derived commitment passes. The anchored one does not.
        let rederived = data_commitment(geometry, &tree.data_tree_root(), case.data_size);
        assert_eq!(
            hex_of(rederived.as_bytes()),
            *rederived_hex,
            "{}",
            case.name
        );
        verify_chunk(
            geometry,
            &rederived,
            case.data_size,
            case.chunk_index,
            &bytes,
            &proof,
        )
        .unwrap_or_else(|err| panic!("{} re-derived commitment: {err}", case.name));
    }
    expect_chunk_error(geometry, &tree.data_commitment(), case, &bytes, &proof);
}

fn check_other_index(geometry: &Geometry, case: &NegativeCase) {
    let (data, tree) = build_pattern_with_bytes(geometry, case);
    let proof_chunk_index = case.proof_chunk_index.expect("proof chunk index");
    let bytes = chunk_slice(geometry, &data, case.source_data_size, case.chunk_index).to_vec();
    let other = chunk_slice(geometry, &data, case.source_data_size, proof_chunk_index);
    assert_ne!(bytes.as_slice(), other, "{}", case.name);
    let proof = tree
        .chunk_proof(proof_chunk_index)
        .unwrap_or_else(|err| panic!("{}: {err}", case.name));
    assert_eq!(hex_of(&proof), case.proof, "{}", case.name);
    assert_eq!(
        hex_of(tree.data_commitment().as_bytes()),
        case.commitment,
        "{}",
        case.name
    );
    expect_chunk_error(geometry, &tree.data_commitment(), case, &bytes, &proof);
}

fn check_segment_mutation(geometry: &Geometry, case: &NegativeCase) {
    let (data, tree) = build_pattern_with_bytes(geometry, case);
    let segment_index = case.segment_index.expect("segment index");
    let chunk = chunk_slice(geometry, &data, case.source_data_size, case.chunk_index);
    let segment = segment_slice(
        geometry,
        &data,
        case.source_data_size,
        case.chunk_index,
        segment_index,
    );
    let honest = tree
        .segment_proof(case.chunk_index, segment_index, chunk)
        .unwrap_or_else(|err| panic!("{}: {err}", case.name));
    let (bytes, proof) = match case.mutation {
        Mutation::FlipSiblingBit0 => (
            segment.to_vec(),
            flip_sibling_bit_0(&honest, case.sibling_index.expect("sibling index")),
        ),
        Mutation::ZeroPadSegment => (zero_pad_segment(geometry, segment), honest),
        Mutation::HonestChunk
        | Mutation::RederivedWitness
        | Mutation::AppendOneZeroHash
        | Mutation::DropLastHash
        | Mutation::EmptyProof
        | Mutation::DropLastChunkByte
        | Mutation::AppendZeroChunkByte
        | Mutation::ProofFromOtherChunk
        | Mutation::IndexOutOfRange
        | Mutation::DataSizeZero
        | Mutation::SegmentEqualsLeftAndRight
        | Mutation::PinnedSiblingNotEmpty
        | Mutation::BadEmptyLeaf
        | Mutation::ReplacedSizeWitness
        | Mutation::DataSizeAboveMax
        | Mutation::ChunkProofAtMovedIndex
        | Mutation::RightmostProofOfClaimedSize
        | Mutation::CommitmentOverChunkRoot
        | Mutation::PaddingSlotAsRightmostSegment => {
            panic!("{} is not a segment mutation", case.name)
        }
    };
    assert_eq!(hex_of(&proof), case.proof, "{}", case.name);
    assert_eq!(
        hex_of(tree.data_commitment().as_bytes()),
        case.commitment,
        "{}",
        case.name
    );
    expect_segment_error(geometry, &tree.data_commitment(), case, &bytes, &proof);
}

fn check_two_digest_segment(geometry: &Geometry, case: &NegativeCase) {
    let left = NodeHash::from(decode_hash(case.left.as_deref().expect("left")));
    let right = NodeHash::from(decode_hash(case.right.as_deref().expect("right")));
    let mut bytes = Vec::with_capacity(HASH_SIZE * 2);
    bytes.extend_from_slice(left.as_bytes());
    bytes.extend_from_slice(right.as_bytes());
    let leaf_hash = leaf(&bytes);
    let node_hash = node(&left, &right);
    // spec §3: the segment is a leaf. It is not the node of the two digests.
    assert_ne!(leaf_hash.as_bytes(), node_hash.as_bytes(), "{}", case.name);
    assert_eq!(
        hex_of(leaf_hash.as_bytes()),
        case.leaf_hash.as_deref().expect("leaf hash")
    );
    assert_eq!(
        hex_of(node_hash.as_bytes()),
        case.node_hash.as_deref().expect("node hash")
    );
    let data_size = u64::try_from(bytes.len()).expect("two digests fit");
    assert_eq!(data_size, TWO_DIGEST_SOURCE_DATA_SIZE, "{}", case.name);
    assert_eq!(case.source_data_size, data_size, "{}", case.name);
    assert_eq!(data_size, case.data_size, "{}", case.name);
    assert_eq!(
        geometry.max_data_tx_chunks(),
        TWO_DIGEST_MAX_DATA_TX_CHUNKS,
        "{}",
        case.name
    );
    let mistaken = data_commitment(
        geometry,
        &DataTreeRoot::from(*node_hash.as_bytes()),
        data_size,
    );
    assert_eq!(
        hex_of(mistaken.as_bytes()),
        case.commitment,
        "{}",
        case.name
    );
    let tree = DataTree::from_bytes(*geometry, &bytes).expect("two digests are one chunk");
    // `h = 0` and `C = 1`: the segment proof is the size witness (spec §5).
    let proof = tree
        .segment_proof(0, 0, &bytes)
        .expect("one segment of one chunk");
    assert_eq!(
        proof.as_slice(),
        size_witness(geometry, data_size).as_bytes()
    );
    assert_eq!(hex_of(&proof), case.proof, "{}", case.name);
    verify_segment(
        geometry,
        &tree.data_commitment(),
        data_size,
        0,
        0,
        &bytes,
        &proof,
    )
    .unwrap_or_else(|err| panic!("{} honest leaf: {err}", case.name));
    expect_segment_error(geometry, &mistaken, case, &bytes, &proof);
}

fn expect_chunk_error(
    geometry: &Geometry,
    commitment: &DataCommitment,
    case: &NegativeCase,
    bytes: &[u8],
    proof: &[u8],
) {
    assert_eq!(case.check, Check::VerifyChunk, "{}", case.name);
    match verify_chunk(
        geometry,
        commitment,
        case.data_size,
        case.chunk_index,
        bytes,
        proof,
    ) {
        Ok(()) => panic!("{} verified, expected {}", case.name, case.expected_error),
        Err(err) => assert_eq!(error_kind(&err), case.expected_error, "{}", case.name),
    }
}

fn expect_segment_error(
    geometry: &Geometry,
    commitment: &DataCommitment,
    case: &NegativeCase,
    bytes: &[u8],
    proof: &[u8],
) {
    assert_eq!(case.check, Check::VerifySegment, "{}", case.name);
    let segment_index = case.segment_index.expect("segment index");
    match verify_segment(
        geometry,
        commitment,
        case.data_size,
        case.chunk_index,
        segment_index,
        bytes,
        proof,
    ) {
        Ok(()) => panic!("{} verified, expected {}", case.name, case.expected_error),
        Err(err) => assert_eq!(error_kind(&err), case.expected_error, "{}", case.name),
    }
}

fn mutate_chunk_proof(honest: &[u8], mutation: Mutation) -> Vec<u8> {
    match mutation {
        Mutation::AppendOneZeroHash => {
            let mut proof = honest.to_vec();
            // One extra hash before the witness. The length check rejects it (spec §5).
            let witness = proof.split_off(proof.len() - HASH_SIZE);
            proof.extend_from_slice(&[0_u8; HASH_SIZE]);
            proof.extend_from_slice(&witness);
            proof
        }
        Mutation::DropLastHash => {
            assert!(
                honest.len() >= 2 * HASH_SIZE,
                "one hash too short drops a sibling and keeps the size witness"
            );
            let witness_start = honest.len() - HASH_SIZE;
            let mut proof = honest[..witness_start - HASH_SIZE].to_vec();
            proof.extend_from_slice(&honest[witness_start..]);
            proof
        }
        Mutation::EmptyProof => {
            assert!(
                !honest.is_empty(),
                "empty proof is for a chunk count above 1"
            );
            Vec::new()
        }
        Mutation::HonestChunk
        | Mutation::RederivedWitness
        | Mutation::DropLastChunkByte
        | Mutation::AppendZeroChunkByte => honest.to_vec(),
        Mutation::ZeroPadSegment
        | Mutation::FlipSiblingBit0
        | Mutation::ProofFromOtherChunk
        | Mutation::IndexOutOfRange
        | Mutation::DataSizeZero
        | Mutation::SegmentEqualsLeftAndRight
        | Mutation::PinnedSiblingNotEmpty
        | Mutation::BadEmptyLeaf
        | Mutation::ReplacedSizeWitness
        | Mutation::DataSizeAboveMax
        | Mutation::ChunkProofAtMovedIndex
        | Mutation::RightmostProofOfClaimedSize
        | Mutation::CommitmentOverChunkRoot
        | Mutation::PaddingSlotAsRightmostSegment => panic!("not a chunk-proof mutation"),
    }
}

fn mutate_chunk_bytes(chunk: &[u8], mutation: Mutation) -> Vec<u8> {
    match mutation {
        Mutation::DropLastChunkByte => {
            assert!(!chunk.is_empty(), "chunk length minus one");
            chunk[..chunk.len() - 1].to_vec()
        }
        Mutation::AppendZeroChunkByte => {
            let mut bytes = chunk.to_vec();
            bytes.push(0);
            bytes
        }
        Mutation::HonestChunk
        | Mutation::RederivedWitness
        | Mutation::AppendOneZeroHash
        | Mutation::DropLastHash
        | Mutation::EmptyProof => chunk.to_vec(),
        Mutation::ZeroPadSegment
        | Mutation::FlipSiblingBit0
        | Mutation::ProofFromOtherChunk
        | Mutation::IndexOutOfRange
        | Mutation::DataSizeZero
        | Mutation::SegmentEqualsLeftAndRight
        | Mutation::PinnedSiblingNotEmpty
        | Mutation::BadEmptyLeaf
        | Mutation::ReplacedSizeWitness
        | Mutation::DataSizeAboveMax
        | Mutation::ChunkProofAtMovedIndex
        | Mutation::RightmostProofOfClaimedSize
        | Mutation::CommitmentOverChunkRoot
        | Mutation::PaddingSlotAsRightmostSegment => panic!("not a chunk-byte mutation"),
    }
}

fn zero_pad_segment(geometry: &Geometry, segment: &[u8]) -> Vec<u8> {
    let segment_size = usize::try_from(geometry.segment_size()).expect("segment size fits");
    assert!(
        segment.len() < segment_size,
        "zero padding extends a short segment"
    );
    let mut bytes = segment.to_vec();
    bytes.resize(segment_size, 0);
    bytes
}

/// Bit 0 of the sibling's first byte (spec §10).
fn flip_sibling_bit_0(honest: &[u8], sibling_index: u32) -> Vec<u8> {
    let mut proof = honest.to_vec();
    let offset = usize::try_from(sibling_index).expect("sibling index fits") * HASH_SIZE;
    assert!(
        offset < proof.len(),
        "sibling {sibling_index} is in the proof"
    );
    proof[offset] ^= 1;
    proof
}

fn build_pattern(geometry: &Geometry, case: &NegativeCase) -> DataTree {
    build_pattern_with_bytes(geometry, case).1
}

fn build_pattern_with_bytes(geometry: &Geometry, case: &NegativeCase) -> (Vec<u8>, DataTree) {
    let pattern = case
        .pattern
        .as_deref()
        .unwrap_or_else(|| panic!("{} has no pattern", case.name));
    let data = data_bytes(pattern, case.source_data_size);
    let tree =
        DataTree::from_bytes(*geometry, &data).unwrap_or_else(|err| panic!("{}: {err}", case.name));
    (data, tree)
}

fn group_uploads<'a, T>(
    cases: &'a [T],
    key_of: impl Fn(&T) -> UploadKey,
) -> BTreeMap<UploadKey, Vec<&'a T>> {
    let mut groups: BTreeMap<UploadKey, Vec<&'a T>> = BTreeMap::new();
    for case in cases {
        groups.entry(key_of(case)).or_default().push(case);
    }
    groups
}

fn build_upload_with_bytes(key: &UploadKey) -> (Vec<u8>, DataTree) {
    let data = data_bytes(&key.pattern, key.data_size);
    let tree = DataTree::from_bytes(key.geometry(), &data)
        .unwrap_or_else(|err| panic!("build {} {}: {err}", key.pattern, key.data_size));
    (data, tree)
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct UploadKey {
    chunk_size: u64,
    segments_per_chunk: u32,
    max_data_tx_chunks: u32,
    pattern: String,
    data_size: u64,
}

impl UploadKey {
    fn new(geometry: &GeometryJson, pattern: &str, data_size: u64) -> Self {
        Self {
            chunk_size: geometry.chunk_size,
            segments_per_chunk: geometry.segments_per_chunk,
            max_data_tx_chunks: geometry.max_data_tx_chunks,
            pattern: pattern.to_string(),
            data_size,
        }
    }

    fn geometry(&self) -> Geometry {
        Geometry::new(
            self.chunk_size,
            self.segments_per_chunk,
            self.max_data_tx_chunks,
        )
        .expect("vector geometry is valid")
    }
}

fn data_bytes(pattern: &str, data_size: u64) -> Vec<u8> {
    let byte_length = usize::try_from(data_size).expect("data size fits usize");
    match pattern {
        MOD251 => {
            let mut data = Vec::with_capacity(byte_length);
            for byte_index in 0..byte_length {
                let reduced = byte_index % MOD251_MODULUS;
                data.push(u8::try_from(reduced).expect("mod 251 fits in a byte"));
            }
            data
        }
        ZEROS => vec![0; byte_length],
        _ => panic!("unknown data pattern {pattern}"),
    }
}

fn chunk_slice<'a>(
    geometry: &Geometry,
    data: &'a [u8],
    data_size: u64,
    chunk_index: u32,
) -> &'a [u8] {
    let chunk_size = usize::try_from(geometry.chunk_size()).expect("chunk size fits usize");
    let start = usize::try_from(chunk_index).expect("chunk index fits usize") * chunk_size;
    let chunk_length = usize::try_from(
        geometry
            .chunk_len(data_size, chunk_index)
            .unwrap_or_else(|err| panic!("chunk length: {err}")),
    )
    .expect("chunk length fits usize");
    &data[start..start + chunk_length]
}

fn segment_slice<'a>(
    geometry: &Geometry,
    data: &'a [u8],
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
) -> &'a [u8] {
    let segment_length = geometry
        .segment_len(data_size, chunk_index, segment_index)
        .unwrap_or_else(|err| panic!("segment length: {err}"));
    if segment_length == 0 {
        return &[];
    }
    let chunk_size = usize::try_from(geometry.chunk_size()).expect("chunk size fits usize");
    let segment_size = usize::try_from(geometry.segment_size()).expect("segment size fits usize");
    let start = usize::try_from(chunk_index).expect("chunk index fits") * chunk_size
        + usize::try_from(segment_index).expect("segment index fits") * segment_size;
    let segment_length = usize::try_from(segment_length).expect("segment length fits usize");
    &data[start..start + segment_length]
}

fn geometry_of(json: &GeometryJson) -> Geometry {
    Geometry::new(
        json.chunk_size,
        json.segments_per_chunk,
        json.max_data_tx_chunks,
    )
    .unwrap_or_else(|err| panic!("vector geometry: {err}"))
}

/// spec §2 maximum. It is not an input to `size_witness` (spec §3), so a
/// changed value still matches every hash in the file.
fn assert_spec_max_data_tx_chunks(max_data_tx_chunks: u32, label: &str) {
    assert_eq!(
        max_data_tx_chunks,
        Geometry::MAINNET.max_data_tx_chunks(),
        "{label}"
    );
}

fn assert_same_geometry(json: &GeometryJson, geometry: Geometry) {
    assert_eq!(json.chunk_size, geometry.chunk_size());
    assert_eq!(json.segments_per_chunk, geometry.segments_per_chunk());
    assert_eq!(json.max_data_tx_chunks, geometry.max_data_tx_chunks());
}

fn error_kind(err: &VerifyError) -> &'static str {
    match err {
        VerifyError::Size(SizeError::DataSizeOutOfRange { .. }) => "DataSizeOutOfRange",
        VerifyError::Size(SizeError::ChunkIndexOutOfRange { .. }) => "ChunkIndexOutOfRange",
        VerifyError::Size(SizeError::SegmentIndexOutOfRange { .. }) => "SegmentIndexOutOfRange",
        VerifyError::InvalidChunkLength { .. } => "InvalidChunkLength",
        VerifyError::InvalidSegmentLength { .. } => "InvalidSegmentLength",
        VerifyError::InvalidProofLength { .. } => "InvalidProofLength",
        VerifyError::SizeWitnessMismatch => "SizeWitnessMismatch",
        VerifyError::CommitmentMismatch => "CommitmentMismatch",
    }
}

fn hex_of(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(bytes)
}

fn decode_hash(text: &str) -> [u8; HASH_SIZE] {
    let bytes = hex::decode(text).unwrap_or_else(|err| panic!("hex {text}: {err}"));
    <[u8; HASH_SIZE]>::try_from(bytes.as_slice()).expect("digest is 32 bytes")
}

fn decode_hex(text: &str, label: &str) -> Vec<u8> {
    hex::decode(text).unwrap_or_else(|err| panic!("{label}: hex: {err}"))
}

fn vector_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vectors")
}

fn read_json<T: for<'de> Deserialize<'de>>(file_name: &str) -> T {
    let path = vector_dir().join(file_name);
    let text =
        fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("parse {}: {err}", path.display()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct VectorFile<T> {
    description: String,
    #[serde(default)]
    geometry: Option<GeometryJson>,
    cases: Vec<T>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GeometryJson {
    chunk_size: u64,
    segments_per_chunk: u32,
    max_data_tx_chunks: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LeafInput {
    pattern: String,
    data_size: u64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum PrimitiveCase {
    Leaf {
        pattern: String,
        data_size: u64,
        hash: String,
    },
    Node {
        left: LeafInput,
        right: LeafInput,
        hash: String,
    },
    SizeWitness {
        data_size: u64,
        geometry: GeometryJson,
        hash: String,
    },
    Empty {
        empty_height: u32,
        hash: String,
    },
    ChunkKey {
        pattern: String,
        data_size: u64,
        geometry: GeometryJson,
        chunk_index: u32,
        commitment: String,
        hash: String,
    },
    ChunkLeaf {
        chunk_index: u32,
        chunk_root: String,
        hash: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootCase {
    pattern: String,
    data_size: u64,
    chunk_count: u32,
    chunk_roots: Vec<String>,
    chunk_leaves: Vec<String>,
    data_tree_root: String,
    size_witness: String,
    data_commitment: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkProofCase {
    geometry: GeometryJson,
    pattern: String,
    data_size: u64,
    chunk_index: u32,
    chunk_count: u32,
    depth: u32,
    data_commitment: String,
    proof: String,
    size_proof: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SegmentProofCase {
    geometry: GeometryJson,
    pattern: String,
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
    segment_length: u64,
    chunk_count: u32,
    data_commitment: String,
    proof: String,
    size_proof: bool,
    /// Set when leaf `leaf_index` is the stand-in hash instead of `empty(0)`.
    /// Honest segment proofs omit it (spec §5).
    #[serde(default)]
    replaced_leaf: Option<ReplacedLeaf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplacedLeaf {
    leaf_index: u32,
    hash: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MultiSegmentCase {
    pattern: String,
    data_size: u64,
    data_commitment: String,
    chunks: Vec<MultiSegmentChunk>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MultiSegmentChunk {
    chunk_index: u32,
    chunk_proof: String,
    segments: Vec<MultiSegmentSegment>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MultiSegmentSegment {
    segment_index: u32,
    in_chunk_siblings: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NegativeCase {
    name: String,
    check: Check,
    #[serde(default)]
    geometry: Option<GeometryJson>,
    #[serde(default)]
    pattern: Option<String>,
    source_data_size: u64,
    data_size: u64,
    chunk_index: u32,
    #[serde(default)]
    segment_index: Option<u32>,
    mutation: Mutation,
    #[serde(default)]
    sibling_index: Option<u32>,
    #[serde(default)]
    proof_chunk_index: Option<u32>,
    #[serde(default)]
    left: Option<String>,
    #[serde(default)]
    right: Option<String>,
    #[serde(default)]
    leaf_hash: Option<String>,
    #[serde(default)]
    node_hash: Option<String>,
    commitment: String,
    #[serde(default)]
    unanchored_commitment: Option<String>,
    proof: String,
    expected_error: String,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Check {
    VerifyChunk,
    VerifySegment,
}

#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Mutation {
    HonestChunk,
    /// Honest chunk bytes. The stored proof's last hash is `size_witness(G, data_size)`,
    /// the witness of the claimed size (spec §6).
    RederivedWitness,
    AppendOneZeroHash,
    DropLastHash,
    EmptyProof,
    DropLastChunkByte,
    AppendZeroChunkByte,
    ZeroPadSegment,
    FlipSiblingBit0,
    ProofFromOtherChunk,
    IndexOutOfRange,
    DataSizeZero,
    SegmentEqualsLeftAndRight,
    /// One in-chunk sibling that the pin requires to be `empty(ℓ)` is the stand-in hash.
    /// `sibling_index` is the level (spec §5).
    PinnedSiblingNotEmpty,
    /// Leaf `segments_per_chunk - 1` is the stand-in hash. Test geometry, `n = 20` (spec §5).
    BadEmptyLeaf,
    /// The last hash is `size_witness(G, data_size + 1)`. The claim stays `data_size` (spec §5).
    ReplacedSizeWitness,
    /// `data_size = max_data_size + 1` for the case geometry (spec §5).
    DataSizeAboveMax,
    /// Chunk `proof_chunk_index`'s bytes and siblings shown as chunk `chunk_index`
    /// of the claimed size, against the re-derived commitment (spec §6).
    ChunkProofAtMovedIndex,
    /// The claimed upload's honest rightmost proof against the source root (spec §9).
    RightmostProofOfClaimedSize,
    /// One-chunk proof against `node(R_0, size_witness(G, n))` (spec §4).
    CommitmentOverChunkRoot,
    /// The source siblings of a padding slot as the claimed rightmost data segment (spec §9).
    PaddingSlotAsRightmostSegment,
}

fn group_segment_proofs<'a>(
    cases: &[&'a SegmentProofCase],
) -> BTreeMap<UploadKey, Vec<&'a SegmentProofCase>> {
    let mut groups: BTreeMap<UploadKey, Vec<&'a SegmentProofCase>> = BTreeMap::new();
    for case in cases {
        groups
            .entry(UploadKey::new(
                &case.geometry,
                &case.pattern,
                case.data_size,
            ))
            .or_default()
            .push(*case);
    }
    groups
}

/// spec §5: segments 0 and 1 verify under the commitment whose leaf 3 is the stand-in.
fn check_mixed_leaf_accepts(cases: &[&SegmentProofCase]) {
    assert_eq!(cases.len(), 2, "segments 0 and 1 of n = 20");
    let geometry = Geometry::TESTING;
    let stand_in = stand_in_hash();
    assert_ne!(stand_in, empty_at_height(0), "the stand-in is not empty(0)");
    let mut commitments = Vec::new();
    for (expected_segment, case) in [0_u32, 1].into_iter().zip(cases) {
        let label = format!("mixed leaf segment {expected_segment}");
        assert_eq!(case.segment_index, expected_segment, "{label}");
        assert_eq!(case.data_size, MIXED_LEAF_DATA_SIZE, "{label}");
        assert_eq!(case.chunk_index, 0, "{label}");
        assert_eq!(case.chunk_count, 1, "{label}");
        assert_eq!(case.pattern, MOD251, "{label}");
        assert_same_geometry(&case.geometry, geometry);
        assert_spec_max_data_tx_chunks(case.geometry.max_data_tx_chunks, &label);
        let replaced = case.replaced_leaf.as_ref().expect("replaced leaf");
        assert_eq!(replaced.leaf_index, 3, "spec §5: leaf 3");
        assert_eq!(replaced.leaf_index, geometry.segments_per_chunk() - 1);
        assert_eq!(replaced.hash, hex_of(stand_in.as_bytes()), "{label}");
        let (commitment, proof, segment) = mixed_leaf_inputs(&geometry, expected_segment);
        assert_eq!(
            hex_of(commitment.as_bytes()),
            case.data_commitment,
            "{label}"
        );
        assert_eq!(hex_of(&proof), case.proof, "{label}");
        assert_eq!(
            u64::try_from(segment.len()).unwrap(),
            case.segment_length,
            "{label}"
        );
        verify_segment(
            &geometry,
            &commitment,
            MIXED_LEAF_DATA_SIZE,
            0,
            expected_segment,
            &segment,
            &proof,
        )
        .unwrap_or_else(|err| panic!("{label}: {err}"));
        commitments.push(case.data_commitment.clone());
    }
    assert_eq!(commitments[0], commitments[1], "one malicious commitment");
}

fn check_pinned_sibling(geometry: &Geometry, case: &NegativeCase) {
    assert_same_geometry_value(geometry, Geometry::TESTING, &case.name);
    let level = case.sibling_index.expect("pinned level");
    let segment_index = case.segment_index.expect("segment index");
    assert_eq!(segment_index, 0, "{}", case.name);
    let (data, tree) = build_pattern_with_bytes(geometry, case);
    assert!(
        case.chunk_index >= 1 && case.chunk_index == tree.chunk_count() - 1,
        "{}: the malformed chunk is the last of more than one",
        case.name
    );
    let chunk = chunk_slice(geometry, &data, case.data_size, case.chunk_index);
    let segment = segment_slice(
        geometry,
        &data,
        case.data_size,
        case.chunk_index,
        segment_index,
    );
    let mut proof = tree
        .segment_proof(case.chunk_index, segment_index, chunk)
        .unwrap_or_else(|err| panic!("{}: {err}", case.name));
    let data_segments = data_segment_count(geometry, case.data_size, case.chunk_index);
    assert_eq!(
        sibling_start(segment_index, level),
        data_segments,
        "{}: the pin boundary is start = s",
        case.name
    );
    assert_eq!(
        sibling_hash(&proof, level),
        empty_at_height(level),
        "{}: the honest sibling is empty({level})",
        case.name
    );
    if level == 0 {
        let higher = level + 1;
        assert!(higher < geometry.height(), "{}", case.name);
        assert_eq!(
            sibling_hash(&proof, higher),
            empty_at_height(higher),
            "{}",
            case.name
        );
    }
    let stand_in = stand_in_hash();
    assert_ne!(stand_in, empty_at_height(level), "{}", case.name);
    replace_sibling(&mut proof, level, &stand_in);
    let commitment = commitment_of_unpinned_proof(
        geometry,
        case.data_size,
        case.chunk_index,
        segment_index,
        segment,
        &proof,
    );
    assert_ne!(
        hex_of(tree.data_commitment().as_bytes()),
        case.commitment,
        "{}: the honest commitment does not catch a missing pin",
        case.name
    );
    assert_eq!(hex_of(&proof), case.proof, "{}", case.name);
    assert_eq!(
        hex_of(commitment.as_bytes()),
        case.commitment,
        "{}",
        case.name
    );
    expect_segment_error(geometry, &commitment, case, segment, &proof);
}

fn check_bad_empty_leaf(geometry: &Geometry, case: &NegativeCase) {
    assert_same_geometry_value(geometry, Geometry::TESTING, &case.name);
    let segment_index = case.segment_index.expect("segment index");
    assert!(segment_index == 2 || segment_index == 3, "{}", case.name);
    assert_eq!(case.data_size, MIXED_LEAF_DATA_SIZE, "{}", case.name);
    assert_eq!(case.chunk_index, 0, "{}", case.name);
    let (commitment, proof, segment) = mixed_leaf_inputs(geometry, segment_index);
    assert_eq!(hex_of(&proof), case.proof, "{}", case.name);
    assert_eq!(
        hex_of(commitment.as_bytes()),
        case.commitment,
        "{}",
        case.name
    );
    let unpinned = commitment_of_unpinned_proof(
        geometry,
        MIXED_LEAF_DATA_SIZE,
        0,
        segment_index,
        &segment,
        &proof,
    );
    if segment_index == 2 {
        assert_eq!(
            unpinned, commitment,
            "{}: the pin is what rejects",
            case.name
        );
    } else {
        assert_ne!(
            unpinned, commitment,
            "{}: empty bytes are not the stand-in",
            case.name
        );
    }
    expect_segment_error(geometry, &commitment, case, &segment, &proof);
}

fn check_replaced_size_witness(geometry: &Geometry, case: &NegativeCase) {
    assert_same_geometry_value(geometry, Geometry::TESTING, &case.name);
    assert_eq!(case.data_size, geometry.chunk_size(), "{}", case.name);
    assert_eq!(case.source_data_size, case.data_size, "{}", case.name);
    assert_eq!(case.chunk_index, 0, "{}", case.name);
    let (data, tree) = build_pattern_with_bytes(geometry, case);
    let chunk = chunk_slice(geometry, &data, case.data_size, 0);
    let honest = match case.check {
        Check::VerifyChunk => tree
            .chunk_proof(0)
            .unwrap_or_else(|err| panic!("{}: {err}", case.name)),
        Check::VerifySegment => {
            assert_eq!(case.segment_index, Some(0), "{}", case.name);
            tree.segment_proof(0, 0, chunk)
                .unwrap_or_else(|err| panic!("{}: {err}", case.name))
        }
    };
    let claimed = size_witness(geometry, case.data_size);
    let replaced = size_witness(geometry, case.data_size + 1);
    assert_ne!(claimed.as_bytes(), replaced.as_bytes(), "{}", case.name);
    assert_eq!(&honest[honest.len() - HASH_SIZE..], claimed.as_bytes());
    let mut proof = honest;
    let start = proof.len() - HASH_SIZE;
    proof[start..].copy_from_slice(replaced.as_bytes());
    assert_eq!(hex_of(&proof), case.proof, "{}", case.name);
    assert_eq!(
        hex_of(tree.data_commitment().as_bytes()),
        case.commitment,
        "{}",
        case.name
    );
    let bytes = match case.check {
        Check::VerifyChunk => chunk.to_vec(),
        Check::VerifySegment => segment_slice(geometry, &data, case.data_size, 0, 0).to_vec(),
    };
    match case.check {
        Check::VerifyChunk => {
            expect_chunk_error(geometry, &tree.data_commitment(), case, &bytes, &proof)
        }
        Check::VerifySegment => {
            expect_segment_error(geometry, &tree.data_commitment(), case, &bytes, &proof)
        }
    }
}

fn check_data_size_above_max(recorded: &Geometry, case: &NegativeCase) {
    assert_eq!(case.check, Check::VerifyChunk, "{}", case.name);
    assert_eq!(recorded.max_data_tx_chunks(), 1, "{}", case.name);
    assert_eq!(
        case.data_size,
        recorded.max_data_size() + 1,
        "{}",
        case.name
    );
    assert_eq!(case.source_data_size, case.data_size, "{}", case.name);
    assert_eq!(case.chunk_index, 0, "{}", case.name);
    assert_eq!(
        (recorded.chunk_size(), recorded.segments_per_chunk()),
        (
            Geometry::TESTING.chunk_size(),
            Geometry::TESTING.segments_per_chunk()
        ),
        "{}",
        case.name
    );
    let wide = Geometry::new(recorded.chunk_size(), recorded.segments_per_chunk(), 2)
        .expect("two testing chunks fit");
    let pattern = case.pattern.as_deref().expect("pattern");
    let data = data_bytes(pattern, case.data_size);
    let tree =
        DataTree::from_bytes(wide, &data).unwrap_or_else(|err| panic!("{}: {err}", case.name));
    let proof = tree
        .chunk_proof(0)
        .unwrap_or_else(|err| panic!("{}: {err}", case.name));
    let bytes = chunk_slice(&wide, &data, case.data_size, 0);
    assert_eq!(hex_of(&proof), case.proof, "{}", case.name);
    assert_eq!(
        hex_of(tree.data_commitment().as_bytes()),
        case.commitment,
        "{}",
        case.name
    );
    verify_chunk(
        &wide,
        &tree.data_commitment(),
        case.data_size,
        0,
        bytes,
        &proof,
    )
    .unwrap_or_else(|err| panic!("{} wide geometry: {err}", case.name));
    expect_chunk_error(recorded, &tree.data_commitment(), case, bytes, &proof);
}

/// Replace the proof's last hash with `size_witness(G, data_size)` (spec §6).
fn with_size_witness(geometry: &Geometry, mut proof: Vec<u8>, data_size: u64) -> Vec<u8> {
    let witness_start = proof.len() - HASH_SIZE;
    proof[witness_start..].copy_from_slice(size_witness(geometry, data_size).as_bytes());
    proof
}

/// Assert the file's commitment and proof, then the expected rejection.
fn expect_claim_error(
    geometry: &Geometry,
    case: &NegativeCase,
    commitment: &DataCommitment,
    bytes: &[u8],
    proof: &[u8],
) {
    assert_eq!(hex_of(proof), case.proof, "{}", case.name);
    assert_eq!(
        hex_of(commitment.as_bytes()),
        case.commitment,
        "{}",
        case.name
    );
    match case.check {
        Check::VerifyChunk => expect_chunk_error(geometry, commitment, case, bytes, proof),
        Check::VerifySegment => expect_segment_error(geometry, commitment, case, bytes, proof),
    }
}

/// spec §6: `n = 96` chunk 2 as chunk 1 of `n' = 64`. The steps match, so the
/// honest chunk leaf of index 2 climbs to the honest root under `(1, 2)`. The
/// verifier hashes index 1 into the leaf and fails.
fn check_chunk_proof_at_moved_index(geometry: &Geometry, case: &NegativeCase) {
    assert_eq!(
        (case.source_data_size, case.data_size, case.chunk_index),
        (96, 64, 1),
        "{}: spec §6",
        case.name
    );
    let proof_chunk_index = case.proof_chunk_index.expect("proof chunk index");
    assert_eq!(proof_chunk_index, 2, "{}: spec §6", case.name);
    let (data, tree) = build_pattern_with_bytes(geometry, case);
    let bytes = chunk_slice(geometry, &data, case.source_data_size, proof_chunk_index);
    let honest = tree
        .chunk_proof(proof_chunk_index)
        .unwrap_or_else(|err| panic!("{}: {err}", case.name));
    let proof = with_size_witness(geometry, honest, case.data_size);
    let siblings = &proof[..proof.len() - HASH_SIZE];
    let claimed_chunk_count = geometry.chunk_count(case.data_size).unwrap();
    assert_eq!(
        climb_without_empty_pin(
            tree.chunk_leaf(proof_chunk_index).unwrap(),
            case.chunk_index,
            claimed_chunk_count,
            siblings,
        ),
        NodeHash::from(*tree.data_tree_root().as_bytes()),
        "{}: only the index in the leaf differs",
        case.name
    );
    let rederived = data_commitment(geometry, &tree.data_tree_root(), case.data_size);
    expect_claim_error(geometry, case, &rederived, bytes, &proof);
}

/// spec §9 size proof: the `n'`-byte upload's honest rightmost proof, at the
/// indexes computed from `n'`, against `node(data_tree_root(n), size_witness(G, n'))`.
fn check_rightmost_proof_of_claimed_size(geometry: &Geometry, case: &NegativeCase) {
    let claims = SIZE_PROOF_CLAIMS
        .iter()
        .find(|(data_size, _)| *data_size == case.source_data_size)
        .unwrap_or_else(|| panic!("{}: source size", case.name));
    assert!(
        claims.1.contains(&case.data_size),
        "{}: claimed size",
        case.name
    );
    let source = build_pattern(geometry, case);
    let claimed_data = data_bytes(MOD251, case.data_size);
    let claimed = DataTree::from_bytes(*geometry, &claimed_data)
        .unwrap_or_else(|err| panic!("{}: {err}", case.name));
    let (chunk_index, segment_index) = rightmost_segment(geometry, case.data_size);
    assert_eq!(case.chunk_index, chunk_index, "{}", case.name);
    let chunk = chunk_slice(geometry, &claimed_data, case.data_size, chunk_index);
    let (bytes, proof) = match case.check {
        Check::VerifyChunk => (
            chunk,
            claimed
                .chunk_proof(chunk_index)
                .unwrap_or_else(|err| panic!("{}: {err}", case.name)),
        ),
        Check::VerifySegment => {
            assert_eq!(case.segment_index, Some(segment_index), "{}", case.name);
            (
                segment_slice(
                    geometry,
                    &claimed_data,
                    case.data_size,
                    chunk_index,
                    segment_index,
                ),
                claimed
                    .segment_proof(chunk_index, segment_index, chunk)
                    .unwrap_or_else(|err| panic!("{}: {err}", case.name)),
            )
        }
    };
    let rederived = data_commitment(geometry, &source.data_tree_root(), case.data_size);
    expect_claim_error(geometry, case, &rederived, bytes, &proof);
}

/// spec §4: for `C = 1` the root is `chunk_leaf(0, R_0)`. A commitment over
/// `R_0` itself does not open.
fn check_commitment_over_chunk_root(geometry: &Geometry, case: &NegativeCase) {
    let (data, tree) = build_pattern_with_bytes(geometry, case);
    assert_eq!(tree.chunk_count(), 1, "{}", case.name);
    let bytes = chunk_slice(geometry, &data, case.source_data_size, 0);
    let root = chunk_root(geometry, bytes).unwrap_or_else(|err| panic!("{}: {err}", case.name));
    assert_eq!(
        NodeHash::from(*tree.data_tree_root().as_bytes()),
        chunk_leaf(0, &root),
        "{}",
        case.name
    );
    let proof = tree
        .chunk_proof(0)
        .unwrap_or_else(|err| panic!("{}: {err}", case.name));
    verify_chunk(
        geometry,
        &tree.data_commitment(),
        case.data_size,
        0,
        bytes,
        &proof,
    )
    .unwrap_or_else(|err| panic!("{} honest: {err}", case.name));
    let over_chunk_root = commitment_of_root(geometry, &root, case.data_size);
    expect_claim_error(geometry, case, &over_chunk_root, bytes, &proof);
}

/// spec §9 size proof: `n = 20` claimed as `n' = 25`. The honest siblings of
/// padding slot 3 open the re-derived commitment only from `leaf("")`. The
/// claimed rightmost data segment has one byte.
fn check_padding_slot(geometry: &Geometry, case: &NegativeCase) {
    assert_eq!(
        (case.source_data_size, case.data_size),
        (PADDING_SLOT_DATA_SIZE, PADDING_SLOT_CLAIMED_DATA_SIZE),
        "{}: spec §10",
        case.name
    );
    let segment_index = case.segment_index.expect("segment index");
    assert_eq!(
        (case.chunk_index, segment_index),
        rightmost_segment(geometry, case.data_size),
        "{}",
        case.name
    );
    let (data, tree) = build_pattern_with_bytes(geometry, case);
    let chunk = chunk_slice(geometry, &data, case.source_data_size, case.chunk_index);
    let honest = tree
        .segment_proof(case.chunk_index, segment_index, chunk)
        .unwrap_or_else(|err| panic!("{}: {err}", case.name));
    let proof = with_size_witness(geometry, honest, case.data_size);
    let claimed_data = data_bytes(MOD251, case.data_size);
    let bytes = segment_slice(
        geometry,
        &claimed_data,
        case.data_size,
        case.chunk_index,
        segment_index,
    );
    assert_eq!(bytes.len(), 1, "{}: one claimed byte", case.name);
    let rederived = data_commitment(geometry, &tree.data_tree_root(), case.data_size);
    assert_eq!(
        commitment_of_unpinned_proof(
            geometry,
            case.data_size,
            case.chunk_index,
            segment_index,
            &[],
            &proof,
        ),
        rederived,
        "{}: the honest leaf is leaf(\"\")",
        case.name
    );
    expect_claim_error(geometry, case, &rederived, bytes, &proof);
}

fn assert_same_geometry_value(geometry: &Geometry, expected: Geometry, label: &str) {
    assert_eq!(geometry.chunk_size(), expected.chunk_size(), "{label}");
    assert_eq!(
        geometry.segments_per_chunk(),
        expected.segments_per_chunk(),
        "{label}"
    );
    assert_eq!(
        geometry.max_data_tx_chunks(),
        expected.max_data_tx_chunks(),
        "{label}"
    );
}

fn stand_in_hash() -> NodeHash {
    NodeHash::from([STAND_IN_HASH_BYTE; HASH_SIZE])
}

fn mixed_leaf_inputs(
    geometry: &Geometry,
    segment_index: u32,
) -> (DataCommitment, Vec<u8>, Vec<u8>) {
    assert_eq!(geometry.chunk_count(MIXED_LEAF_DATA_SIZE).unwrap(), 1);
    let leaf_index = geometry.segments_per_chunk() - 1;
    assert_eq!(leaf_index, 3, "spec §5: leaf 3");
    let data = data_bytes(MOD251, MIXED_LEAF_DATA_SIZE);
    let chunk_length = geometry.chunk_len(MIXED_LEAF_DATA_SIZE, 0).unwrap();
    let bad_root = subtree_with_stand_in_leaf(
        geometry,
        &data,
        chunk_length,
        0,
        geometry.height(),
        leaf_index,
    );
    // One chunk: `data_tree_root = chunk_leaf(0, R_0)` (spec §4).
    let commitment = commitment_of_root(geometry, &chunk_leaf(0, &bad_root), MIXED_LEAF_DATA_SIZE);
    let segment = segment_slice(geometry, &data, MIXED_LEAF_DATA_SIZE, 0, segment_index).to_vec();
    let mut proof = Vec::new();
    for level in 0..geometry.height() {
        let start = sibling_start(segment_index, level);
        let sibling =
            subtree_with_stand_in_leaf(geometry, &data, chunk_length, start, level, leaf_index);
        proof.extend_from_slice(sibling.as_bytes());
    }
    proof.extend_from_slice(size_witness(geometry, MIXED_LEAF_DATA_SIZE).as_bytes());
    (commitment, proof, segment)
}

/// `rightmost_segment(n) = (C − 1, s_last − 1)` (spec §5).
fn rightmost_segment(geometry: &Geometry, data_size: u64) -> (u32, u32) {
    let chunk_index = geometry.chunk_count(data_size).expect("data size in range") - 1;
    (
        chunk_index,
        data_segment_count(geometry, data_size, chunk_index) - 1,
    )
}

fn data_segment_count(geometry: &Geometry, data_size: u64, chunk_index: u32) -> u32 {
    let chunk_length = geometry
        .chunk_len(data_size, chunk_index)
        .expect("chunk in range");
    u32::try_from(chunk_length.div_ceil(geometry.segment_size())).expect("segment count fits")
}

/// `start = ((j >> ℓ) ^ 1) << ℓ` (spec §5).
fn sibling_start(segment_index: u32, level: u32) -> u32 {
    debug_assert!(level < u32::BITS);
    ((segment_index >> level) ^ 1) << level
}

fn sibling_hash(proof: &[u8], level: u32) -> NodeHash {
    let offset = usize::try_from(level).expect("level fits") * HASH_SIZE;
    let bytes = <[u8; HASH_SIZE]>::try_from(&proof[offset..offset + HASH_SIZE])
        .expect("the sibling is one hash");
    NodeHash::from(bytes)
}

fn replace_sibling(proof: &mut [u8], level: u32, sibling: &NodeHash) {
    let offset = usize::try_from(level).expect("level fits") * HASH_SIZE;
    proof[offset..offset + HASH_SIZE].copy_from_slice(sibling.as_bytes());
}

fn commitment_of_root(geometry: &Geometry, root: &NodeHash, data_size: u64) -> DataCommitment {
    data_commitment(geometry, &DataTreeRoot::from(*root.as_bytes()), data_size)
}

/// Commitment of a segment proof climbed with no empty-sibling pin (spec §5).
fn commitment_of_unpinned_proof(
    geometry: &Geometry,
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
    segment: &[u8],
    proof: &[u8],
) -> DataCommitment {
    let chunk_count = geometry.chunk_count(data_size).expect("data size in range");
    let siblings = &proof[..proof.len() - HASH_SIZE];
    let in_chunk_len = usize::try_from(geometry.height()).expect("height fits") * HASH_SIZE;
    let (in_chunk, chunk_siblings) = siblings
        .split_at_checked(in_chunk_len)
        .expect("the proof holds the in-chunk siblings");
    let mut root = leaf(segment);
    let mut walked = segment_index;
    for sibling in in_chunk.chunks_exact(HASH_SIZE) {
        let bytes = <[u8; HASH_SIZE]>::try_from(sibling).expect("one hash");
        let sibling = NodeHash::from(bytes);
        root = if walked & 1 == 0 {
            node(&root, &sibling)
        } else {
            node(&sibling, &root)
        };
        walked >>= 1;
    }
    // The chunk root takes its index before the chunk level (spec §5).
    let climbed = climb_without_empty_pin(
        chunk_leaf(chunk_index, &root),
        chunk_index,
        chunk_count,
        chunk_siblings,
    );
    commitment_of_root(geometry, &climbed, data_size)
}

/// spec §5 `climb`. No empty-sibling pin.
fn climb_without_empty_pin(
    mut root: NodeHash,
    chunk_index: u32,
    chunk_count: u32,
    siblings: &[u8],
) -> NodeHash {
    let mut node_index = chunk_index;
    let mut last_index = chunk_count - 1;
    for sibling in siblings.chunks_exact(HASH_SIZE) {
        assert_ne!(last_index, 0, "the chunk-level sibling count is depth");
        let bytes = <[u8; HASH_SIZE]>::try_from(sibling).expect("one hash");
        let sibling = NodeHash::from(bytes);
        if node_index & 1 == 1 || node_index == last_index {
            root = node(&sibling, &root);
            while node_index & 1 == 0 && node_index != 0 {
                node_index >>= 1;
                last_index >>= 1;
            }
        } else {
            root = node(&root, &sibling);
        }
        node_index >>= 1;
        last_index >>= 1;
    }
    assert_eq!(last_index, 0, "the chunk-level sibling count is depth");
    root
}

/// Spec §4 subtree. Leaf `stand_in_leaf` is the stand-in hash instead of `empty(0)`.
fn subtree_with_stand_in_leaf(
    geometry: &Geometry,
    data: &[u8],
    chunk_length: u64,
    start_leaf: u32,
    subtree_height: u32,
    stand_in_leaf: u32,
) -> NodeHash {
    let width = 1_u32 << subtree_height;
    let end_leaf = start_leaf.checked_add(width).expect("subtree end fits");
    let contains = stand_in_leaf >= start_leaf && stand_in_leaf < end_leaf;
    let range_start = u64::from(start_leaf) * geometry.segment_size();
    if !contains && range_start >= chunk_length {
        return empty_at_height(subtree_height);
    }
    if subtree_height == 0 {
        if contains {
            return stand_in_hash();
        }
        let end = (range_start + geometry.segment_size()).min(chunk_length);
        let start = usize::try_from(range_start).expect("range start fits");
        let end = usize::try_from(end).expect("range end fits");
        return leaf(&data[start..end]);
    }
    let left_width = width / 2;
    let left = subtree_with_stand_in_leaf(
        geometry,
        data,
        chunk_length,
        start_leaf,
        subtree_height - 1,
        stand_in_leaf,
    );
    let right = subtree_with_stand_in_leaf(
        geometry,
        data,
        chunk_length,
        start_leaf + left_width,
        subtree_height - 1,
        stand_in_leaf,
    );
    node(&left, &right)
}

/// `empty(k)` at a height the caller knows is at most `MAX_SUBTREE_HEIGHT`.
fn empty_at_height(height: u32) -> NodeHash {
    empty(SubtreeHeight::new(height).expect("height is at most MAX_SUBTREE_HEIGHT"))
}
