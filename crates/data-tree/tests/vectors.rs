//! Shared vectors (spec §13).
//!
//! Each JSON file is regenerated from its pattern. Positive proofs must verify.
//! Negative cases must fail with the recorded error kind. Printed spec §13 hex is
//! compared to the files and is not adjusted to match a mismatch.
//!
//! Mutations match `examples/gen_vectors.rs`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use irys_data_tree::{
    DataCommitment, DataTree, DataTreeRoot, Geometry, HASH_SIZE, NodeHash, SizeError, VerifyError,
    chunk_key, data_commitment, depth, empty, leaf, node, size_witness, verify_chunk,
    verify_segment,
};
use serde::Deserialize;

/// spec §13: `b[i] = i mod 251`.
const MOD251_MODULUS: usize = 251;
const MOD251: &str = "mod251";
const ZEROS: &str = "zeros";

/// spec §13 primitive group: 4 leaves, node, 6 size witnesses, empty(0..=6), 4 chunk keys.
const PRIMITIVE_CASES: usize = 22;
/// spec §13 testing root rows.
const TESTING_ROOT_CASES: usize = 7;
/// spec §13 mainnet sizes, each as mod251 and as zeros.
const MAINNET_ROOT_CASES: usize = 34;
/// Chunk counts 1..=17, plus 3 chunks of 1 GiB, plus the 4 chunks of the printed upload.
const CHUNK_PROOF_CASES: usize = 160;
/// Mainnet segments for chunk counts 1 and 2, four boundary segments, one partial, one printed.
const SEGMENT_PROOF_CASES: usize = 198;
/// spec §13 multi-segment group: m = 1, 2, 4, 64, one adjacent pair, one two-chunk pair.
const MULTI_SEGMENT_CASES: usize = 6;
/// spec §13 negative list, with one case per sibling bit of the printed segment proof.
const NEGATIVE_CASES: usize = 17;

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
    for description in [
        &primitives.description,
        &roots_testing.description,
        &roots_mainnet.description,
        &chunk_proofs.description,
        &segment_proofs.description,
        &multi_segment.description,
        &negative.description,
    ] {
        assert!(description.contains("spec"), "{description}");
    }
    assert!(primitives.geometry.is_none(), "primitives have no geometry");
    assert!(chunk_proofs.geometry.is_none());
    assert!(segment_proofs.geometry.is_none());
    assert_same_geometry(roots_testing.geometry.as_ref().unwrap(), Geometry::TESTING);
    assert_same_geometry(roots_mainnet.geometry.as_ref().unwrap(), Geometry::MAINNET);
    assert_same_geometry(multi_segment.geometry.as_ref().unwrap(), Geometry::MAINNET);
    assert_same_geometry(negative.geometry.as_ref().unwrap(), Geometry::TESTING);
    assert!(
        multi_segment.description.contains("917504"),
        "the description states the upload"
    );
    assert!(
        multi_segment.description.contains("partial"),
        "the description states why this upload"
    );

    // Printed hex from spec §13. A mismatch is a bug in the crate or the generator.
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
    assert_size_witness(
        primitives,
        1,
        "f83f60940c1ec44c0f1e90f694c6cae7c99b4b6d1507d60ad5a3282a7750d0ee",
    );
    assert_size_witness(
        primitives,
        4_096,
        "719e72af15abf7c6f95ea297c02cd24b41b6b3467a78583a7103dbb19d45571f",
    );
    assert_size_witness(
        primitives,
        262_144,
        "313eba86534a86e9270cfa6cb7cee900b753358a0c5ef61330202481653d22eb",
    );
    assert_size_witness(
        primitives,
        917_504,
        "42bb7029ceeb50156a1b044c1af62a0d78207b52708ff5c31a78ed81c2bd595b",
    );
    assert_spec_roots(roots_testing, TESTING_ROOT_ROWS);
    assert_spec_roots(roots_mainnet, MAINNET_ROOT_ROWS);
    assert_spec_chunk_proof(chunk_proofs);
    assert_spec_segment_proof(segment_proofs);
    assert_spec_chunk_key(primitives);
}

/// `(data_size, chunk_count, data_tree_root, data_commitment)` from spec §13. mod251.
const TESTING_ROOT_ROWS: &[(u64, u32, &str, &str)] = &[
    (
        1,
        1,
        "8521b87ff724236722cfb6d6306ad59bceb4aee3e35ae73c41326cd31915e13e",
        "a107920f86fd22b8f786cace824308964a955e019625b0246a2caf3cdd122eb6",
    ),
    (
        8,
        1,
        "2239889b93ac82ead2db9605f2584a25255c90206c1fccaddf54caa75b7fc1a1",
        "87bbfb2c56ebf02b55bd51d32633445fe986f4031be1a3d04d09fac9d447106e",
    ),
    (
        9,
        1,
        "3ba2878285288e004dcd35a73e9a3df4a19f5fe5acccaf8d84b50fc7254e6ddb",
        "55100cd7d0c2bdbc9d03808eebcde5d5d144f27e021e3477c8a84834d5a0de94",
    ),
    (
        32,
        1,
        "d3e81f198d6bd3782d1941172c89e49721d8f5e33cc709d4f6ba3f54b9e35f82",
        "00f0535265357690ad8c75c88235490e7340e4cef3a81157c2c473861866d8d7",
    ),
    (
        33,
        2,
        "bf98e27c471bfda3de1d44b4eb420f528238aa75e6e002fe1636c04a431713a9",
        "ca888a5334e649c2c8a6ddece5b132464e67b1f59f0b5b5d02433070cce02fcf",
    ),
    (
        112,
        4,
        "7fd698c547deb0bbb8f8ab7db08dd34616a496f141167252487509cb74824564",
        "cf1fcae00134fbb7e7317da641c092740b22136c469ae46b6d1db03c4f01dbe5",
    ),
    (
        157,
        5,
        "81e7a996cd2ba141815aa48a1afe4f9c3d5c0ca450f12c9a605c20a0c3037c95",
        "f8ba5215b628deec40d735d6cdf825016fca443f0f8cbff3eb5361de59e8ab03",
    ),
];

/// `(data_size, chunk_count, data_tree_root, data_commitment)` from spec §13. mod251.
const MAINNET_ROOT_ROWS: &[(u64, u32, &str, &str)] = &[
    (
        1,
        1,
        "2438dc9c444ab818a41197dfd3aa6502bc3e2fb03895b7ed79ab275539b7972b",
        "719863e12c1e714b1df22d0600ed93ad96486cbc7920f565a5e434574463d013",
    ),
    (
        4_095,
        1,
        "851d7b0214e05f85e581de8dd408f5a4b0c31d8baad3f42986f056949ee1d575",
        "e773c7e0780a33fca2cbb91aae23fc0ba476ea86eb156ee79e93df35d1dda5be",
    ),
    (
        4_096,
        1,
        "be58572dcfba33df3201cdbeda2799dd0c5b508a0df41a47912cb008896f9195",
        "be251ef52dcd4fd580211d8fb5eb451be18a3e02c16d4512b56953d9a3c1e748",
    ),
    (
        4_097,
        1,
        "f516d84903ce88d9d1d80d4071ce945dc53707bd4ed60346a3ba6392412c9134",
        "ce4a935a089dd36beb84501e6813bf4e328bb0ed15307ac3c4a6ab246d04c3c6",
    ),
    (
        262_143,
        1,
        "b6dd65dde53181e3443d128d05b7cbf57c5f4352caa8216ed365d716896335bd",
        "7db8ca90c9fab87dc8577252f4ac084bfbc12f491ff049318352623ae715a83c",
    ),
    (
        262_144,
        1,
        "8e2d2d85eb5fd8e66d4f4b874b5ad6b5aa46cad790fbe64f61c374d8f8243d5f",
        "97c9ecd15caca57f3f74198a5a456d583fedd22ed267f951cce1cd14998b6e4f",
    ),
    (
        262_145,
        2,
        "dcc4ac982c84bdf0d6df5c16d2728b65bb4bae355b078b5a22e47a6a3e518825",
        "75f80cf9008e5f4c65a847f036fccd0c10176cc9840f82a4f0d6b846404d6326",
    ),
    (
        917_504,
        4,
        "3d86c811acf3356576cbe00b28814ad89ee42bd5aa36b755cea977620366de67",
        "472cfc752f2aacd4662235abe756da1e2079db705736ad22a6db9db6f6d6cc9c",
    ),
    (
        1_310_720,
        5,
        "1d5501730df0426a6f2b223d3fc31703d5eb6e64bae166736c920ccd23594ac0",
        "027fba531187d84813dd203d3e03dda3a3898cde1bbfd2675b54e46c230d6e89",
    ),
];

fn assert_spec_roots(file: &VectorFile<RootCase>, rows: &[(u64, u32, &str, &str)]) {
    for &(data_size, chunk_count, root, commitment) in rows {
        let case = find_root(file, MOD251, data_size);
        assert_eq!(
            case.chunk_count, chunk_count,
            "spec §13 chunk count {data_size}"
        );
        assert_eq!(
            case.data_tree_root, root,
            "spec §13 data_tree_root {data_size}"
        );
        assert_eq!(
            case.data_commitment, commitment,
            "spec §13 data_commitment {data_size}"
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
        .expect("spec §13 chunk proof");
    let proof = concat!(
        "dbc3c2433981a503f8ffa38629a9ec5d41501618b2afd860af52d6a93e06e8b6",
        "50cb073b3d437e037c34dc0d757489f56fd63b9122dd4edfbe8bc42812a5bd84",
    );
    assert_eq!(case.proof, proof, "spec §13 n=112 chunk proof");
    assert_eq!(
        case.data_commitment,
        "cf1fcae00134fbb7e7317da641c092740b22136c469ae46b6d1db03c4f01dbe5"
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
        .expect("spec §13 segment proof");
    let proof = concat!(
        "82bdff7cd7950d248431b84dd588f1777777ee141ebc86ac8f72080aaf11e0e5",
        "fe43d66afa4a9a5c4f9c9da89f4ffb52635c8f342e7ffb731d68e36c5982072a",
        "f8e87ba8c906d8391fb1182e6c1e7d123392e8770d510b547edb3bbffe7cfb65",
        "50cb073b3d437e037c34dc0d757489f56fd63b9122dd4edfbe8bc42812a5bd84",
    );
    assert_eq!(case.proof, proof, "spec §13 n=112 segment proof");
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
        panic!("spec §13 chunk_key");
    };
    assert_eq!(*data_size, 112);
    assert_eq!(*chunk_index, 3);
    assert_eq!(
        *commitment,
        "cf1fcae00134fbb7e7317da641c092740b22136c469ae46b6d1db03c4f01dbe5"
    );
    assert_eq!(
        *hash,
        "b541f01b947453085184d77f79e23e48acd6d3f0ff773843392ab4dafad1b425"
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
        panic!("spec §13 leaf {data_size}");
    };
    assert_eq!(pattern, MOD251);
    assert_eq!(actual, hash, "spec §13 leaf {data_size}");
}

fn assert_empty(file: &VectorFile<PrimitiveCase>, empty_height: u32, hash: &str) {
    let Some(PrimitiveCase::Empty { hash: actual, .. }) = file.cases.iter().find(
        |case| matches!(case, PrimitiveCase::Empty { empty_height: height, .. } if *height == empty_height),
    ) else {
        panic!("spec §13 empty {empty_height}");
    };
    assert_eq!(actual, hash, "spec §13 empty {empty_height}");
}

fn assert_size_witness(file: &VectorFile<PrimitiveCase>, data_size: u64, hash: &str) {
    let Some(PrimitiveCase::SizeWitness { hash: actual, .. }) = file.cases.iter().find(
        |case| matches!(case, PrimitiveCase::SizeWitness { data_size: size, .. } if *size == data_size),
    ) else {
        panic!("spec §13 size_witness {data_size}");
    };
    assert_eq!(actual, hash, "spec §13 size_witness {data_size}");
}

fn find_root<'a>(file: &'a VectorFile<RootCase>, pattern: &str, data_size: u64) -> &'a RootCase {
    file.cases
        .iter()
        .find(|case| case.pattern == pattern && case.data_size == data_size)
        .unwrap_or_else(|| panic!("missing root {pattern} {data_size}"))
}

fn check_primitives(file: &VectorFile<PrimitiveCase>) {
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
            PrimitiveCase::SizeWitness { data_size, hash } => {
                assert_eq!(
                    hex_of(size_witness(*data_size).as_bytes()),
                    *hash,
                    "size_witness {data_size}"
                );
            }
            PrimitiveCase::Empty { empty_height, hash } => {
                assert_eq!(
                    hex_of(empty(*empty_height).as_bytes()),
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
                let geometry = geometry_of(geometry);
                let data = data_bytes(pattern, *data_size);
                let tree = DataTree::from_bytes(geometry, &data)
                    .unwrap_or_else(|err| panic!("chunk_key tree: {err}"));
                assert_eq!(hex_of(tree.data_commitment().as_bytes()), *commitment);
                // `2^24` is a hash input. It is not an index in the 112-byte tree (spec §3, §13).
                let key = chunk_key(&tree.data_commitment(), *chunk_index);
                assert_eq!(hex_of(key.as_bytes()), *hash, "chunk_key {chunk_index}");
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
        let tree = build_upload(&key);
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
            for (chunk_index, expected) in case.chunk_roots.iter().enumerate() {
                let chunk_index = u32::try_from(chunk_index).expect("chunk index fits");
                let root = tree
                    .chunk_root(chunk_index)
                    .unwrap_or_else(|err| panic!("{label}: {err}"));
                assert_eq!(
                    hex_of(root.as_bytes()),
                    *expected,
                    "{label} chunk {chunk_index}"
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
            assert_eq!(case.pattern, MOD251, "{label}");
            assert_eq!(tree.chunk_count(), case.chunk_count, "{label}");
            assert_eq!(
                depth(case.chunk_index, case.chunk_count),
                case.depth,
                "{label}"
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
    let groups = group_uploads(&file.cases, |case| {
        UploadKey::new(&case.geometry, &case.pattern, case.data_size)
    });
    for (key, cases) in groups {
        let geometry = key.geometry();
        let (data, tree) = build_upload_with_bytes(&key);
        for case in cases {
            let label = format!(
                "segment {} chunk {} segment {}",
                case.data_size, case.chunk_index, case.segment_index
            );
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

/// The six spec §13 shapes, in generator order.
fn multi_segment_shapes(geometry: &Geometry, data_size: u64) -> Vec<Vec<(u32, Vec<u32>)>> {
    assert_eq!(
        geometry.segments_per_chunk(),
        64,
        "spec §13: m = 64 is every segment of one mainnet chunk"
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
            // spec §5: h in-chunk siblings, then the chunk proof.
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
    let file_geometry = file.geometry.as_ref().expect("negative file geometry");
    for case in &file.cases {
        let geometry = geometry_of(case.geometry.as_ref().unwrap_or(file_geometry));
        match case.mutation {
            Mutation::SegmentEqualsLeftAndRight => check_two_digest_segment(&geometry, case),
            Mutation::IndexOutOfRange | Mutation::DataSizeZero => check_range_case(&geometry, case),
            Mutation::ProofFromOtherChunk => check_other_index(&geometry, case),
            Mutation::ZeroPadSegment | Mutation::FlipSiblingBit0 => {
                check_segment_mutation(&geometry, case)
            }
            Mutation::HonestChunk
            | Mutation::AppendOneZeroHash
            | Mutation::DropLastHash
            | Mutation::EmptyProof
            | Mutation::DropLastChunkByte
            | Mutation::AppendZeroChunkByte => check_chunk_mutation(&geometry, case),
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
    let proof = mutate_chunk_proof(&honest, case.mutation);
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
        let rederived = data_commitment(&tree.data_tree_root(), case.data_size);
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
        | Mutation::AppendOneZeroHash
        | Mutation::DropLastHash
        | Mutation::EmptyProof
        | Mutation::DropLastChunkByte
        | Mutation::AppendZeroChunkByte
        | Mutation::ProofFromOtherChunk
        | Mutation::IndexOutOfRange
        | Mutation::DataSizeZero
        | Mutation::SegmentEqualsLeftAndRight => panic!("{} is not a segment mutation", case.name),
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
    let left = NodeHash(decode_hash(case.left.as_deref().expect("left")));
    let right = NodeHash(decode_hash(case.right.as_deref().expect("right")));
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
    assert_eq!(data_size, case.data_size, "{}", case.name);
    let mistaken = data_commitment(&DataTreeRoot(*node_hash.as_bytes()), data_size);
    assert_eq!(
        hex_of(mistaken.as_bytes()),
        case.commitment,
        "{}",
        case.name
    );
    assert!(case.proof.is_empty(), "{}", case.name);
    let tree = DataTree::from_bytes(*geometry, &bytes).expect("two digests are one chunk");
    verify_segment(
        geometry,
        &tree.data_commitment(),
        data_size,
        0,
        0,
        &bytes,
        &[],
    )
    .unwrap_or_else(|err| panic!("{} honest leaf: {err}", case.name));
    expect_segment_error(geometry, &mistaken, case, &bytes, &[]);
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
            proof.extend_from_slice(&[0_u8; HASH_SIZE]);
            proof
        }
        Mutation::DropLastHash => {
            assert!(
                honest.len() > HASH_SIZE,
                "one hash too short leaves a proof"
            );
            honest[..honest.len() - HASH_SIZE].to_vec()
        }
        Mutation::EmptyProof => {
            assert!(
                !honest.is_empty(),
                "empty proof is for a chunk count above 1"
            );
            Vec::new()
        }
        Mutation::HonestChunk | Mutation::DropLastChunkByte | Mutation::AppendZeroChunkByte => {
            honest.to_vec()
        }
        Mutation::ZeroPadSegment
        | Mutation::FlipSiblingBit0
        | Mutation::ProofFromOtherChunk
        | Mutation::IndexOutOfRange
        | Mutation::DataSizeZero
        | Mutation::SegmentEqualsLeftAndRight => panic!("not a chunk-proof mutation"),
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
        | Mutation::AppendOneZeroHash
        | Mutation::DropLastHash
        | Mutation::EmptyProof => chunk.to_vec(),
        Mutation::ZeroPadSegment
        | Mutation::FlipSiblingBit0
        | Mutation::ProofFromOtherChunk
        | Mutation::IndexOutOfRange
        | Mutation::DataSizeZero
        | Mutation::SegmentEqualsLeftAndRight => panic!("not a chunk-byte mutation"),
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

/// Bit 0 of the sibling's first byte (spec §13).
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

fn build_upload(key: &UploadKey) -> DataTree {
    build_upload_with_bytes(key).1
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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootCase {
    pattern: String,
    data_size: u64,
    chunk_count: u32,
    chunk_roots: Vec<String>,
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
}
