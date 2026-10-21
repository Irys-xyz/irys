//! Shared vectors for the data tree (spec §11 step 2, §13).
//!
//! One JSON file per group under `<out_dir>`. Data bytes are not stored: a case
//! names `mod251` (`b[i] = i mod 251`) or `zeros`, and the byte length.
//!
//! Multi-segment (option B, spec §5 and §15 Q2) is generated.
//! Spec §13 groups not generated here: receipt header binding (gateway formats),
//! `tx_root` (waits on the deferred §14 Q3), and wire (L1 and gateway message formats).
//!
//! Run: `cargo run -p irys-data-tree --example gen_vectors -- <out_dir>`.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use irys_data_tree::{
    DataTree, DataTreeRoot, Geometry, HASH_SIZE, NodeHash, SizeError, VerifyError, chunk_key,
    data_commitment, depth, empty, leaf, node, size_witness, verify_chunk, verify_segment,
};
use serde::Serialize;

/// spec §13: `b[i] = i mod 251`.
const MOD251_MODULUS: usize = 251;
const MOD251: &str = "mod251";
const ZEROS: &str = "zeros";

/// spec §13: `size_witness` of `2^40`.
const TWO_TO_THE_40: u64 = 1 << 40;

/// spec §13: 1 GiB is `2^30` bytes, 4,096 mainnet chunks.
const ONE_GIBIBYTE: u64 = 1 << 30;

/// spec §13: every chunk for chunk counts through 17.
const EVERY_CHUNK_UP_TO: u32 = 17;

/// spec §13: every segment for chunk counts through 2.
const EVERY_SEGMENT_UP_TO: u32 = 2;

/// spec §13: in one chunk, `m` = 1, 2, 4, and 64.
const ONE_CHUNK_SEGMENT_COUNTS: [u32; 4] = [1, 2, 4, 64];

/// spec §13: chunk counts listed beside the mainnet root sizes.
const MAINNET_SHAPE_CHUNK_COUNTS: [u32; 4] = [8, 9, 16, 17];

/// spec §13: chunks 0, 2,047, and 4,095 of the 1 GiB upload.
const ONE_GIBIBYTE_CHUNK_INDEXES: [u32; 3] = [0, 2_047, 4_095];

/// Printed testing upload (spec §13). Chunk count 4.
const PRINTED_DATA_SIZE: u64 = 112;

const PRIMITIVES_DESCRIPTION: &str = concat!(
    "Domain-separated hashes (spec §3, §13). ",
    "Leaves hash mod251 bytes of length 0, 1, 4095, and 4096. ",
    "node hashes the 0-byte leaf with the 1-byte leaf. ",
    "size_witness covers 1, 4096, 262144, 917504, 2^40, and 5 TiB. ",
    "empty covers heights 0 through 6. ",
    "chunk_key uses the testing-geometry commitment of 112 mod251 bytes. ",
    "Chunk indexes are 0, 1, and 2^24, plus 3 so the printed chunk_key is in this file. ",
    "2^24 is an input of the hash, not a chunk of that 112-byte tree."
);

const ROOTS_MAINNET_DESCRIPTION: &str = concat!(
    "Mainnet roots (spec §4, §13). Each data_size is stored twice: mod251 bytes, then zero bytes. ",
    "The case holds the chunk roots, data_tree_root, size_witness, and data_commitment. ",
    "Chunk counts 8, 9, 16, and 17 are full uploads (data_size = chunk_count * chunk_size). ",
    "1 GiB is 2^30 bytes (4096 chunks). 1 GiB + 1 is the next byte."
);

const ROOTS_TESTING_DESCRIPTION: &str = concat!(
    "Testing-geometry roots (spec §13): chunk_size 32, segments_per_chunk 4. ",
    "Bytes are mod251, the pattern of the printed root table. ",
    "The case holds the chunk roots, data_tree_root, size_witness, and data_commitment."
);

const CHUNK_PROOFS_DESCRIPTION: &str = concat!(
    "Chunk proofs, leaf to root, one lowercase hex string (spec §5, §13). ",
    "Mainnet geometry, mod251: every chunk of each full upload with chunk count 1 through 17 ",
    "(chunk count 5, chunk index 4 is the one-sibling proof, spec §4), ",
    "and chunks 0, 2047, and 4095 of 1 GiB. ",
    "Testing geometry, mod251: every chunk of the printed 112-byte upload, including chunk 2."
);

const SEGMENT_PROOFS_DESCRIPTION: &str = concat!(
    "Segment proofs: in-chunk siblings, then the chunk proof, one hex string (spec §5, §13). ",
    "Mainnet geometry, mod251: every segment of every chunk for chunk count 1 and 2 (full uploads); ",
    "segments (3, 5), (3, 31), (3, 32), and (3, 63) of the 917504-byte upload; ",
    "and segment (0, 1) of 4097 bytes, which is 1 byte. ",
    "Testing geometry, mod251: the printed proof at data_size 112, chunk 3, segment 1."
);

const MULTI_SEGMENT_DESCRIPTION: &str = concat!(
    "Multi-segment proofs, option B (spec §5, §13, §15 Q2). ",
    "Each segment stores its h in-chunk siblings, level 0 first. ",
    "The chunk proof is one hex string, leaf to root, shared by the segments of that chunk. ",
    "The upload is mainnet geometry and mod251 bytes, data_size 917504: ",
    "the spec §4 example of 3.5 chunks, so chunk 3 is partial ",
    "(segments 0 to 31 hold bytes, segments 32 to 63 are empty). ",
    "Cases on that upload: m = 1, 2, 4, and 64, the first m segments of chunk 0; ",
    "an adjacent pair in chunk 3, segments 31 and 32 ",
    "(the last full segment and the first empty segment); ",
    "a pair in two chunks, segment 0 of chunk 2 and segment 0 of chunk 3, two chunk proofs."
);

const NEGATIVE_DESCRIPTION: &str = concat!(
    "Rejected inputs (spec §13). expected_error is the VerifyError or SizeError variant name. ",
    "A size failure is the SizeError name. source_data_size rebuilds the tree. ",
    "data_size, chunk_index, segment_index, commitment, and proof are the verifier inputs. ",
    "The mutation names how those differ from the honest proof. ",
    "The 64-byte segment is left concatenated with right, checked against the commitment of ",
    "node(left, right). That segment is a leaf (spec §3). ",
    "The unanchored case records the re-derived commitment, which verifies, ",
    "and the anchored commitment, which does not (spec §6)."
);

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("gen_vectors: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let out_dir = out_dir_from_args()?;
    fs::create_dir_all(&out_dir)?;

    // Small groups first. A rejected case then fails before the 1 GiB uploads are hashed.
    let primitives = primitive_cases();
    write_group(
        &out_dir,
        "primitives.json",
        &group(PRIMITIVES_DESCRIPTION, None, primitives),
    )?;
    let testing_roots = testing_root_cases();
    write_group(
        &out_dir,
        "roots_testing.json",
        &group(
            ROOTS_TESTING_DESCRIPTION,
            Some(Geometry::TESTING.into()),
            testing_roots,
        ),
    )?;
    let negative = negative_cases();
    write_group(
        &out_dir,
        "negative.json",
        &group(
            NEGATIVE_DESCRIPTION,
            Some(Geometry::TESTING.into()),
            negative,
        ),
    )?;
    let segment_proofs = segment_proof_cases();
    write_group(
        &out_dir,
        "segment_proofs.json",
        &group(SEGMENT_PROOFS_DESCRIPTION, None, segment_proofs),
    )?;
    let multi_segment = multi_segment_cases();
    write_group(
        &out_dir,
        "multi_segment.json",
        &group(
            MULTI_SEGMENT_DESCRIPTION,
            Some(Geometry::MAINNET.into()),
            multi_segment,
        ),
    )?;
    let chunk_proofs = chunk_proof_cases();
    write_group(
        &out_dir,
        "chunk_proofs.json",
        &group(CHUNK_PROOFS_DESCRIPTION, None, chunk_proofs),
    )?;
    let mainnet_roots = mainnet_root_cases();
    write_group(
        &out_dir,
        "roots_mainnet.json",
        &group(
            ROOTS_MAINNET_DESCRIPTION,
            Some(Geometry::MAINNET.into()),
            mainnet_roots,
        ),
    )?;
    Ok(())
}

fn out_dir_from_args() -> Result<PathBuf, Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let Some(out_dir) = args.next() else {
        return Err("usage: gen_vectors <out_dir>".into());
    };
    if args.next().is_some() {
        return Err("usage: gen_vectors <out_dir>".into());
    }
    Ok(PathBuf::from(out_dir))
}

fn group<T: Serialize>(
    description: &'static str,
    geometry: Option<GeometryJson>,
    cases: Vec<T>,
) -> VectorFile<T> {
    VectorFile {
        description,
        geometry,
        cases,
    }
}

fn write_group<T: Serialize>(
    dir: &Path,
    file_name: &str,
    value: &VectorFile<T>,
) -> Result<(), Box<dyn Error>> {
    let path = dir.join(file_name);
    let mut text = serde_json::to_string_pretty(value)?;
    // A trailing newline keeps the file a POSIX text file. Pretty JSON has no other variance.
    text.push('\n');
    fs::write(&path, text)?;
    println!("{file_name}: {} cases", value.cases.len());
    Ok(())
}

fn primitive_cases() -> Vec<PrimitiveCase> {
    let mut cases = Vec::new();
    for data_size in leaf_lengths() {
        let bytes = data_bytes(MOD251, data_size);
        cases.push(PrimitiveCase::Leaf {
            pattern: MOD251,
            data_size,
            hash: hex_of(leaf(&bytes).as_bytes()),
        });
    }

    // spec §3: node is one hash of the two children. The inputs are the first two leaves.
    let left = leaf(&data_bytes(MOD251, 0));
    let right = leaf(&data_bytes(MOD251, 1));
    cases.push(PrimitiveCase::Node {
        left: LeafInput {
            pattern: MOD251,
            data_size: 0,
        },
        right: LeafInput {
            pattern: MOD251,
            data_size: 1,
        },
        hash: hex_of(node(&left, &right).as_bytes()),
    });

    let geometry = Geometry::MAINNET;
    // 5 TiB is the mainnet maximum (spec §2, §13).
    assert_eq!(
        geometry.max_data_size(),
        5 * TWO_TO_THE_40,
        "spec §2: 5 TiB"
    );
    for data_size in size_witness_data_sizes(geometry) {
        cases.push(PrimitiveCase::SizeWitness {
            data_size,
            hash: hex_of(size_witness(data_size).as_bytes()),
        });
    }

    // spec §13 stops at empty(6), the mainnet chunk height. Taller empties are out of this set.
    for empty_height in 0..=6 {
        cases.push(PrimitiveCase::Empty {
            empty_height,
            hash: hex_of(empty(empty_height).as_bytes()),
        });
    }

    let testing = Geometry::TESTING;
    let (_data, tree) = build_upload(testing, MOD251, PRINTED_DATA_SIZE);
    let commitment = tree.data_commitment();
    // 0, 1, and 2^24 are the spec list. 3 is the printed testing chunk_key (spec §13).
    for chunk_index in [0_u32, 1, 1 << 24, 3] {
        let key = chunk_key(&commitment, chunk_index);
        cases.push(PrimitiveCase::ChunkKey {
            pattern: MOD251,
            data_size: PRINTED_DATA_SIZE,
            geometry: testing.into(),
            chunk_index,
            commitment: hex_of(commitment.as_bytes()),
            hash: hex_of(key.as_bytes()),
        });
    }
    cases
}

fn leaf_lengths() -> [u64; 4] {
    // 4095 and 4096 are one below and equal to the mainnet segment size (spec §13).
    let segment_size = Geometry::MAINNET.segment_size();
    [0, 1, segment_size - 1, segment_size]
}

fn size_witness_data_sizes(geometry: Geometry) -> [u64; 6] {
    let chunk_size = geometry.chunk_size();
    [
        1,
        geometry.segment_size(),
        chunk_size,
        3 * chunk_size + chunk_size / 2,
        TWO_TO_THE_40,
        geometry.max_data_size(),
    ]
}

fn testing_root_cases() -> Vec<RootCase> {
    let geometry = Geometry::TESTING;
    assert_eq!(geometry.chunk_size(), 32, "spec §2");
    assert_eq!(geometry.segment_size(), 8, "spec §2");
    let mut cases = Vec::new();
    for data_size in testing_root_data_sizes(geometry) {
        let (_data, tree) = build_upload(geometry, MOD251, data_size);
        cases.push(root_case(MOD251, data_size, &tree));
    }
    cases
}

fn testing_root_data_sizes(geometry: Geometry) -> [u64; 7] {
    let chunk_size = geometry.chunk_size();
    let segment_size = geometry.segment_size();
    // 112 and 157 are the printed rows that are not a one-byte step off a full chunk (spec §13).
    let data_sizes = [
        1,
        segment_size,
        segment_size + 1,
        chunk_size,
        chunk_size + 1,
        PRINTED_DATA_SIZE,
        157,
    ];
    assert_eq!(
        geometry.chunk_count(PRINTED_DATA_SIZE).unwrap(),
        4,
        "spec §13"
    );
    assert_eq!(geometry.chunk_count(157).unwrap(), 5, "spec §13");
    data_sizes
}

fn mainnet_root_cases() -> Vec<RootCase> {
    let geometry = Geometry::MAINNET;
    assert_mainnet_numbers(geometry);
    let mut cases = Vec::new();
    for data_size in mainnet_root_data_sizes(geometry) {
        for pattern in [MOD251, ZEROS] {
            let (_data, tree) = build_upload(geometry, pattern, data_size);
            cases.push(root_case(pattern, data_size, &tree));
        }
    }
    cases
}

fn assert_mainnet_numbers(geometry: Geometry) {
    let chunk_size = geometry.chunk_size();
    let segment_size = geometry.segment_size();
    assert_eq!(chunk_size, 262_144, "spec §2");
    assert_eq!(segment_size, 4_096, "spec §2");
    assert_eq!(2 * chunk_size, 524_288, "spec §13");
    assert_eq!(3 * chunk_size, 786_432, "spec §13");
    assert_eq!(3 * chunk_size + chunk_size / 2, 917_504, "spec §13");
    assert_eq!(5 * chunk_size, 1_310_720, "spec §13");
    assert_eq!(
        ONE_GIBIBYTE / chunk_size,
        4_096,
        "spec §13: 1 GiB is 4096 chunks"
    );
    assert_eq!(geometry.chunk_count(ONE_GIBIBYTE).unwrap(), 4_096);
    assert_eq!(geometry.chunk_count(ONE_GIBIBYTE + 1).unwrap(), 4_097);
}

fn mainnet_root_data_sizes(geometry: Geometry) -> Vec<u64> {
    let chunk_size = geometry.chunk_size();
    let segment_size = geometry.segment_size();
    let mut data_sizes = vec![
        1,
        segment_size - 1,
        segment_size,
        segment_size + 1,
        chunk_size - 1,
        chunk_size,
        chunk_size + 1,
        2 * chunk_size,
        3 * chunk_size,
        3 * chunk_size + chunk_size / 2,
        5 * chunk_size,
    ];
    for chunk_count in MAINNET_SHAPE_CHUNK_COUNTS {
        data_sizes.push(u64::from(chunk_count) * chunk_size);
    }
    data_sizes.push(ONE_GIBIBYTE);
    data_sizes.push(ONE_GIBIBYTE + 1);
    data_sizes
}

fn root_case(pattern: &'static str, data_size: u64, tree: &DataTree) -> RootCase {
    let chunk_count = tree.chunk_count();
    let mut chunk_roots =
        Vec::with_capacity(usize::try_from(chunk_count).expect("chunk count fits"));
    for chunk_index in 0..chunk_count {
        let chunk_root = tree
            .chunk_root(chunk_index)
            .unwrap_or_else(|err| panic!("chunk root {chunk_index}: {err}"));
        chunk_roots.push(hex_of(chunk_root.as_bytes()));
    }
    RootCase {
        pattern,
        data_size,
        chunk_count,
        chunk_roots,
        data_tree_root: hex_of(tree.data_tree_root().as_bytes()),
        size_witness: hex_of(tree.size_witness().as_bytes()),
        data_commitment: hex_of(tree.data_commitment().as_bytes()),
    }
}

fn chunk_proof_cases() -> Vec<ChunkProofCase> {
    let mut cases = Vec::new();
    let mainnet = Geometry::MAINNET;
    for chunk_count in 1..=EVERY_CHUNK_UP_TO {
        let data_size = u64::from(chunk_count) * mainnet.chunk_size();
        push_every_chunk(&mut cases, mainnet, data_size);
    }
    push_chunk_indexes(
        &mut cases,
        mainnet,
        ONE_GIBIBYTE,
        &ONE_GIBIBYTE_CHUNK_INDEXES,
    );
    // The printed proof is testing geometry, chunk count 4, which the mainnet sweep does not cover.
    push_every_chunk(&mut cases, Geometry::TESTING, PRINTED_DATA_SIZE);
    cases
}

fn push_every_chunk(cases: &mut Vec<ChunkProofCase>, geometry: Geometry, data_size: u64) {
    let chunk_count = geometry
        .chunk_count(data_size)
        .unwrap_or_else(|err| panic!("data_size {data_size}: {err}"));
    let mut indexes = Vec::with_capacity(usize::try_from(chunk_count).expect("chunk count fits"));
    for chunk_index in 0..chunk_count {
        indexes.push(chunk_index);
    }
    push_chunk_indexes(cases, geometry, data_size, &indexes);
}

fn push_chunk_indexes(
    cases: &mut Vec<ChunkProofCase>,
    geometry: Geometry,
    data_size: u64,
    chunk_indexes: &[u32],
) {
    let (data, tree) = build_upload(geometry, MOD251, data_size);
    let chunk_count = tree.chunk_count();
    let commitment = tree.data_commitment();
    for &chunk_index in chunk_indexes {
        let proof = chunk_proof_bytes(&tree, chunk_index);
        let sibling_count = depth(chunk_index, chunk_count);
        assert_eq!(
            proof.len(),
            (usize::try_from(sibling_count).expect("depth fits") + 1) * HASH_SIZE,
            "proof length is 32 * (depth + 1) (spec §5)"
        );
        if chunk_count == 5 && chunk_index == 4 {
            assert_eq!(
                sibling_count, 1,
                "spec §4: chunk 4 of chunk count 5 has one sibling"
            );
        }
        let chunk = chunk_slice(&geometry, &data, data_size, chunk_index);
        verify_chunk(
            &geometry,
            &commitment,
            data_size,
            chunk_index,
            chunk,
            &proof,
        )
        .unwrap_or_else(|err| panic!("chunk proof {data_size} chunk {chunk_index}: {err}"));
        cases.push(ChunkProofCase {
            geometry: geometry.into(),
            pattern: MOD251,
            data_size,
            chunk_index,
            chunk_count,
            depth: sibling_count,
            data_commitment: hex_of(commitment.as_bytes()),
            proof: hex_of(&proof),
        });
    }
}

fn segment_proof_cases() -> Vec<SegmentProofCase> {
    let mut cases = Vec::new();
    let mainnet = Geometry::MAINNET;
    for chunk_count in 1..=EVERY_SEGMENT_UP_TO {
        push_every_segment(&mut cases, mainnet, chunk_count);
    }
    push_mainnet_boundary_segments(&mut cases, mainnet);
    // Printed segment proof: testing geometry, outside the chunk-count-2 sweep (spec §13).
    push_segment_pairs(&mut cases, Geometry::TESTING, PRINTED_DATA_SIZE, &[(3, 1)]);
    cases
}

fn push_every_segment(cases: &mut Vec<SegmentProofCase>, geometry: Geometry, chunk_count: u32) {
    let data_size = u64::from(chunk_count) * geometry.chunk_size();
    let (data, tree) = build_upload(geometry, MOD251, data_size);
    assert_eq!(tree.chunk_count(), chunk_count);
    let mut pairs = Vec::new();
    for chunk_index in 0..chunk_count {
        for segment_index in 0..geometry.segments_per_chunk() {
            pairs.push((chunk_index, segment_index));
        }
    }
    push_segment_pairs_with_tree(cases, geometry, data_size, &tree, &data, &pairs);
}

fn push_mainnet_boundary_segments(cases: &mut Vec<SegmentProofCase>, geometry: Geometry) {
    let three_and_a_half = 3 * geometry.chunk_size() + geometry.chunk_size() / 2;
    assert_eq!(three_and_a_half, 917_504, "spec §13");
    // (3, 31) is the last full segment, (3, 32) the first empty (spec §4, §13).
    assert_eq!(
        geometry.segment_len(three_and_a_half, 3, 5).unwrap(),
        geometry.segment_size()
    );
    assert_eq!(
        geometry.segment_len(three_and_a_half, 3, 31).unwrap(),
        geometry.segment_size()
    );
    assert_eq!(geometry.segment_len(three_and_a_half, 3, 32).unwrap(), 0);
    assert_eq!(geometry.segment_len(three_and_a_half, 3, 63).unwrap(), 0);
    push_segment_pairs(
        cases,
        geometry,
        three_and_a_half,
        &[(3, 5), (3, 31), (3, 32), (3, 63)],
    );

    let one_past_segment = geometry.segment_size() + 1;
    assert_eq!(one_past_segment, 4_097, "spec §13");
    assert_eq!(geometry.segment_len(one_past_segment, 0, 1).unwrap(), 1);
    push_segment_pairs(cases, geometry, one_past_segment, &[(0, 1)]);
}

fn push_segment_pairs(
    cases: &mut Vec<SegmentProofCase>,
    geometry: Geometry,
    data_size: u64,
    pairs: &[(u32, u32)],
) {
    let (data, tree) = build_upload(geometry, MOD251, data_size);
    push_segment_pairs_with_tree(cases, geometry, data_size, &tree, &data, pairs);
}

fn push_segment_pairs_with_tree(
    cases: &mut Vec<SegmentProofCase>,
    geometry: Geometry,
    data_size: u64,
    tree: &DataTree,
    data: &[u8],
    pairs: &[(u32, u32)],
) {
    let chunk_count = tree.chunk_count();
    let commitment = tree.data_commitment();
    for &(chunk_index, segment_index) in pairs {
        let chunk = chunk_slice(&geometry, data, data_size, chunk_index);
        let proof = tree
            .segment_proof(chunk_index, segment_index, chunk)
            .unwrap_or_else(|err| {
                panic!(
                    "segment proof {data_size} chunk {chunk_index} segment {segment_index}: {err}"
                )
            });
        let hash_count = geometry.height() + depth(chunk_index, chunk_count) + 1;
        assert_eq!(
            proof.len(),
            usize::try_from(hash_count).expect("hash count fits") * HASH_SIZE,
            "segment proof length is 32 * (h + depth + 1) (spec §5)"
        );
        let segment_length = geometry
            .segment_len(data_size, chunk_index, segment_index)
            .unwrap_or_else(|err| panic!("segment length: {err}"));
        let segment = segment_slice(&geometry, data, data_size, chunk_index, segment_index);
        assert_eq!(u64::try_from(segment.len()).unwrap(), segment_length);
        verify_segment(
            &geometry,
            &commitment,
            data_size,
            chunk_index,
            segment_index,
            segment,
            &proof,
        )
        .unwrap_or_else(|err| {
            panic!("verify segment {data_size} chunk {chunk_index} segment {segment_index}: {err}")
        });
        cases.push(SegmentProofCase {
            geometry: geometry.into(),
            pattern: MOD251,
            data_size,
            chunk_index,
            segment_index,
            segment_length,
            chunk_count,
            data_commitment: hex_of(commitment.as_bytes()),
            proof: hex_of(&proof),
        });
    }
}

fn multi_segment_cases() -> Vec<MultiSegmentCase> {
    let geometry = Geometry::MAINNET;
    let chunk_size = geometry.chunk_size();
    // spec §4: 3.5 chunks. The last chunk is partial, so one case can use it.
    let data_size = 3 * chunk_size + chunk_size / 2;
    assert_eq!(data_size, 917_504, "spec §4");
    let (data, tree) = build_upload(geometry, MOD251, data_size);
    let chunk_count = tree.chunk_count();
    assert_eq!(chunk_count, 4, "spec §4");
    let last_chunk_index = chunk_count - 1;
    let last_chunk_length = geometry
        .chunk_len(data_size, last_chunk_index)
        .expect("last chunk is in range");
    assert!(last_chunk_length < chunk_size, "the last chunk is partial");
    assert_eq!(
        *ONE_CHUNK_SEGMENT_COUNTS.last().expect("four counts"),
        geometry.segments_per_chunk(),
        "spec §13: m = 64 is every segment of one mainnet chunk"
    );

    let mut shapes = Vec::new();
    // Chunk 0 is full. The m cases share it: the first m segments.
    for segment_count in ONE_CHUNK_SEGMENT_COUNTS {
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
    assert_eq!(
        geometry
            .segment_len(data_size, last_chunk_index, last_full_segment)
            .expect("segment in range"),
        geometry.segment_size(),
        "spec §4: the last occupied segment is full"
    );
    assert_eq!(
        geometry
            .segment_len(data_size, last_chunk_index, first_empty_segment)
            .expect("segment in range"),
        0,
        "spec §4: the next segment is empty"
    );
    shapes.push(vec![(
        last_chunk_index,
        vec![last_full_segment, first_empty_segment],
    )]);

    let previous_chunk_index = last_chunk_index - 1;
    shapes.push(vec![
        (previous_chunk_index, vec![0]),
        (last_chunk_index, vec![0]),
    ]);

    let mut cases = Vec::with_capacity(shapes.len());
    for chunks in shapes {
        cases.push(multi_segment_case(
            geometry, data_size, &tree, &data, &chunks,
        ));
    }
    cases
}

fn multi_segment_case(
    geometry: Geometry,
    data_size: u64,
    tree: &DataTree,
    data: &[u8],
    chunks: &[(u32, Vec<u32>)],
) -> MultiSegmentCase {
    let commitment = tree.data_commitment();
    let mut chunk_cases = Vec::with_capacity(chunks.len());
    for (chunk_index, segment_indexes) in chunks {
        let chunk_proof = chunk_proof_bytes(tree, *chunk_index);
        let chunk_bytes = chunk_slice(&geometry, data, data_size, *chunk_index);
        let mut segments = Vec::with_capacity(segment_indexes.len());
        for &segment_index in segment_indexes {
            let segment_proof = tree
                .segment_proof(*chunk_index, segment_index, chunk_bytes)
                .unwrap_or_else(|err| {
                    panic!(
                        "segment proof {data_size} chunk {chunk_index} segment {segment_index}: {err}"
                    )
                });
            // spec §5: in-chunk siblings concatenated with the chunk proof. The
            // chunk proof ends with the size witness, so this is the whole proof
            // in every case, including a one-chunk upload.
            let in_chunk = in_chunk_sibling_bytes(&geometry, &segment_proof, &chunk_proof);
            let segment = segment_slice(&geometry, data, data_size, *chunk_index, segment_index);
            let mut proof = in_chunk.to_vec();
            proof.extend_from_slice(&chunk_proof);
            verify_segment(
                &geometry,
                &commitment,
                data_size,
                *chunk_index,
                segment_index,
                segment,
                &proof,
            )
            .unwrap_or_else(|err| {
                panic!(
                    "verify multi-segment {data_size} chunk {chunk_index} segment {segment_index}: {err}"
                )
            });
            segments.push(MultiSegmentSegment {
                segment_index,
                in_chunk_siblings: hex_of(in_chunk),
            });
        }
        chunk_cases.push(MultiSegmentChunk {
            chunk_index: *chunk_index,
            chunk_proof: hex_of(&chunk_proof),
            segments,
        });
    }
    if chunk_cases.len() == 2 {
        assert_ne!(
            chunk_cases[0].chunk_proof, chunk_cases[1].chunk_proof,
            "a pair in two chunks is two chunk proofs"
        );
    }
    MultiSegmentCase {
        pattern: MOD251,
        data_size,
        data_commitment: hex_of(commitment.as_bytes()),
        chunks: chunk_cases,
    }
}

/// The `h` in-chunk siblings: the prefix before the chunk proof (spec §5).
fn in_chunk_sibling_bytes<'a>(
    geometry: &Geometry,
    segment_proof: &'a [u8],
    chunk_proof: &[u8],
) -> &'a [u8] {
    let in_chunk_len = usize::try_from(geometry.height()).expect("height fits usize") * HASH_SIZE;
    let (in_chunk, rest) = segment_proof
        .split_at_checked(in_chunk_len)
        .expect("segment proof holds the h in-chunk siblings");
    assert_eq!(
        rest, chunk_proof,
        "a segment proof is the in-chunk siblings, then the chunk proof (spec §5)"
    );
    in_chunk
}

fn negative_cases() -> Vec<NegativeCase> {
    let geometry = Geometry::TESTING;
    assert_negative_shape(geometry);
    let chunk_size = geometry.chunk_size();
    let three_chunks = 3 * chunk_size;
    let drafts = [
        draft(
            "wrong_data_size_same_chunk_count",
            Check::VerifyChunk,
            three_chunks,
            2 * chunk_size + chunk_size / 2,
            0,
            Mutation::HonestChunk,
            "SizeWitnessMismatch",
        ),
        draft(
            "wrong_data_size_other_chunk_count",
            Check::VerifyChunk,
            three_chunks,
            4 * chunk_size,
            2,
            Mutation::HonestChunk,
            "InvalidProofLength",
        ),
        draft(
            "chunk_index_out_of_range",
            Check::VerifyChunk,
            chunk_size,
            chunk_size,
            1,
            Mutation::IndexOutOfRange,
            "ChunkIndexOutOfRange",
        ),
        draft(
            "proof_one_hash_too_long",
            Check::VerifyChunk,
            2 * chunk_size,
            2 * chunk_size,
            0,
            Mutation::AppendOneZeroHash,
            "InvalidProofLength",
        ),
        draft(
            "proof_one_hash_too_short",
            Check::VerifyChunk,
            three_chunks,
            three_chunks,
            0,
            Mutation::DropLastHash,
            "InvalidProofLength",
        ),
        draft(
            "empty_proof_for_chunk_count_above_one",
            Check::VerifyChunk,
            2 * chunk_size,
            2 * chunk_size,
            0,
            Mutation::EmptyProof,
            "InvalidProofLength",
        ),
        draft(
            "chunk_length_minus_one",
            Check::VerifyChunk,
            chunk_size,
            chunk_size,
            0,
            Mutation::DropLastChunkByte,
            "InvalidChunkLength",
        ),
        draft(
            "chunk_length_plus_one",
            Check::VerifyChunk,
            chunk_size,
            chunk_size,
            0,
            Mutation::AppendZeroChunkByte,
            "InvalidChunkLength",
        ),
        draft(
            "data_size_zero",
            Check::VerifyChunk,
            chunk_size,
            0,
            0,
            Mutation::DataSizeZero,
            "DataSizeOutOfRange",
        ),
        NegativeDraft {
            proof_chunk_index: Some(0),
            ..draft(
                "proof_at_another_index",
                Check::VerifyChunk,
                three_chunks,
                three_chunks,
                1,
                Mutation::ProofFromOtherChunk,
                "CommitmentMismatch",
            )
        },
        NegativeDraft {
            record_unanchored: true,
            ..draft(
                "unanchored_commitment",
                Check::VerifyChunk,
                three_chunks,
                three_chunks + 1,
                0,
                Mutation::HonestChunk,
                "CommitmentMismatch",
            )
        },
        NegativeDraft {
            segment_index: Some(0),
            ..draft(
                "zero_padded_segment",
                Check::VerifySegment,
                chunk_size + 1,
                chunk_size + 1,
                1,
                Mutation::ZeroPadSegment,
                "InvalidSegmentLength",
            )
        },
    ];
    let mut cases: Vec<NegativeCase> = drafts.iter().copied().map(realize).collect();
    cases.extend(flip_sibling_cases(geometry));
    cases.push(two_digest_case());
    let mut names = std::collections::BTreeSet::new();
    for case in &cases {
        assert!(names.insert(case.name), "duplicate case {}", case.name);
    }
    cases
}

fn assert_negative_shape(geometry: Geometry) {
    let chunk_size = geometry.chunk_size();
    let three_chunks = 3 * chunk_size;
    let same_count = 2 * chunk_size + chunk_size / 2;
    // Same chunk count, and a chunk count whose proof length differs (spec §5, §13).
    assert_eq!(geometry.chunk_count(three_chunks).unwrap(), 3);
    assert_eq!(geometry.chunk_count(same_count).unwrap(), 3);
    assert_eq!(geometry.chunk_count(4 * chunk_size).unwrap(), 4);
    assert_eq!(geometry.chunk_count(three_chunks + 1).unwrap(), 4);
    assert_eq!(
        depth(0, 3),
        depth(0, 4),
        "spec §6: the left chunk keeps its depth"
    );
    assert_eq!(depth(0, 3), 2, "spec §5");
    assert_eq!(depth(2, 3), 1, "spec §5");
    assert_eq!(depth(2, 4), 2, "spec §5");
    assert_eq!(depth(0, 2), 1, "spec §5");
}

fn draft(
    name: &'static str,
    check: Check,
    source_data_size: u64,
    data_size: u64,
    chunk_index: u32,
    mutation: Mutation,
    expected_error: &'static str,
) -> NegativeDraft {
    NegativeDraft {
        name,
        check,
        geometry: None,
        pattern: Some(MOD251),
        source_data_size,
        data_size,
        chunk_index,
        segment_index: None,
        mutation,
        sibling_index: None,
        proof_chunk_index: None,
        expected_error,
        record_unanchored: false,
    }
}

fn flip_sibling_cases(geometry: Geometry) -> Vec<NegativeCase> {
    let (_data, tree) = build_upload(geometry, MOD251, PRINTED_DATA_SIZE);
    let chunk = chunk_slice_from_pattern(geometry, PRINTED_DATA_SIZE, 3);
    let proof = tree
        .segment_proof(3, 1, &chunk)
        .expect("printed segment proof");
    // Four reference siblings, then the size witness (spec §13). One flipped bit
    // per sibling. The witness flip is a unit test, not one of these cases.
    let hash_count = proof.len() / HASH_SIZE;
    assert_eq!(hash_count, 5, "spec §13 printed segment proof");
    let sibling_count = hash_count - 1;
    assert_eq!(sibling_count, 4, "spec §13 printed siblings");
    (0..sibling_count)
        .map(|sibling_index| {
            let sibling_index = u32::try_from(sibling_index).expect("sibling count fits");
            let name = flip_case_name(sibling_index);
            realize(NegativeDraft {
                sibling_index: Some(sibling_index),
                segment_index: Some(1),
                name,
                ..draft(
                    name,
                    Check::VerifySegment,
                    PRINTED_DATA_SIZE,
                    PRINTED_DATA_SIZE,
                    3,
                    Mutation::FlipSiblingBit0,
                    "CommitmentMismatch",
                )
            })
        })
        .collect()
}

fn flip_case_name(sibling_index: u32) -> &'static str {
    match sibling_index {
        0 => "flip_bit_0_of_sibling_0",
        1 => "flip_bit_0_of_sibling_1",
        2 => "flip_bit_0_of_sibling_2",
        3 => "flip_bit_0_of_sibling_3",
        _ => panic!("printed segment proof has 4 siblings"),
    }
}

fn chunk_slice_from_pattern(geometry: Geometry, data_size: u64, chunk_index: u32) -> Vec<u8> {
    let data = data_bytes(MOD251, data_size);
    chunk_slice(&geometry, &data, data_size, chunk_index).to_vec()
}

fn realize(draft: NegativeDraft) -> NegativeCase {
    if draft.mutation == Mutation::SegmentEqualsLeftAndRight {
        return two_digest_case();
    }
    let geometry = draft.geometry.unwrap_or(Geometry::TESTING);
    let pattern = draft.pattern.expect("data pattern");
    let (data, tree) = build_upload(geometry, pattern, draft.source_data_size);
    let (bytes, mut proof) = mutated_input(&geometry, &data, &tree, &draft);
    if draft.record_unanchored {
        // spec §6: the re-derived commitment repeats size_witness of the claimed
        // size. The proof's last hash is that witness, so the climb can pass.
        // The true witness is rejected before the climb (spec §5).
        let witness = size_witness(draft.data_size);
        let start = proof.len() - HASH_SIZE;
        proof[start..].copy_from_slice(witness.as_bytes());
    }
    let commitment = tree.data_commitment();
    let unanchored_commitment = unanchored_commitment(&geometry, &tree, &draft, &bytes, &proof);
    reject(&geometry, &draft, &commitment, &bytes, &proof);
    NegativeCase {
        name: draft.name,
        check: draft.check,
        geometry: draft.geometry.map(GeometryJson::from),
        pattern: draft.pattern,
        source_data_size: draft.source_data_size,
        data_size: draft.data_size,
        chunk_index: draft.chunk_index,
        segment_index: draft.segment_index,
        mutation: draft.mutation,
        sibling_index: draft.sibling_index,
        proof_chunk_index: draft.proof_chunk_index,
        left: None,
        right: None,
        leaf_hash: None,
        node_hash: None,
        commitment: hex_of(commitment.as_bytes()),
        unanchored_commitment,
        proof: hex_of(&proof),
        expected_error: draft.expected_error,
    }
}

fn mutated_input(
    geometry: &Geometry,
    data: &[u8],
    tree: &DataTree,
    draft: &NegativeDraft,
) -> (Vec<u8>, Vec<u8>) {
    match draft.mutation {
        Mutation::IndexOutOfRange => {
            assert_eq!(
                draft.chunk_index,
                tree.chunk_count(),
                "{} is one past the last chunk",
                draft.name
            );
            (Vec::new(), Vec::new())
        }
        Mutation::DataSizeZero => (Vec::new(), Vec::new()),
        Mutation::ProofFromOtherChunk => other_chunk_input(geometry, data, tree, draft),
        Mutation::ZeroPadSegment | Mutation::FlipSiblingBit0 => {
            segment_input(geometry, data, tree, draft)
        }
        Mutation::HonestChunk
        | Mutation::AppendOneZeroHash
        | Mutation::DropLastHash
        | Mutation::EmptyProof
        | Mutation::DropLastChunkByte
        | Mutation::AppendZeroChunkByte => chunk_input(geometry, data, tree, draft),
        Mutation::SegmentEqualsLeftAndRight => unreachable!("handled before the upload is built"),
    }
}

fn chunk_input(
    geometry: &Geometry,
    data: &[u8],
    tree: &DataTree,
    draft: &NegativeDraft,
) -> (Vec<u8>, Vec<u8>) {
    let chunk = chunk_slice(geometry, data, draft.source_data_size, draft.chunk_index);
    let honest = chunk_proof_bytes(tree, draft.chunk_index);
    let proof = mutate_chunk_proof(&honest, draft.mutation);
    let bytes = mutate_chunk_bytes(chunk, draft.mutation);
    (bytes, proof)
}

fn segment_input(
    geometry: &Geometry,
    data: &[u8],
    tree: &DataTree,
    draft: &NegativeDraft,
) -> (Vec<u8>, Vec<u8>) {
    let segment_index = draft.segment_index.expect("segment index");
    let chunk = chunk_slice(geometry, data, draft.source_data_size, draft.chunk_index);
    let segment = segment_slice(
        geometry,
        data,
        draft.source_data_size,
        draft.chunk_index,
        segment_index,
    );
    let honest = tree
        .segment_proof(draft.chunk_index, segment_index, chunk)
        .unwrap_or_else(|err| panic!("segment proof {}: {err}", draft.name));
    let proof = mutate_segment_proof(&honest, draft);
    let bytes = mutate_segment_bytes(geometry, segment, draft.mutation);
    (bytes, proof)
}

fn other_chunk_input(
    geometry: &Geometry,
    data: &[u8],
    tree: &DataTree,
    draft: &NegativeDraft,
) -> (Vec<u8>, Vec<u8>) {
    let proof_chunk_index = draft.proof_chunk_index.expect("proof chunk index");
    let bytes = chunk_slice(geometry, data, draft.source_data_size, draft.chunk_index).to_vec();
    let other = chunk_slice(geometry, data, draft.source_data_size, proof_chunk_index);
    // spec §13: a valid proof shown at another index, with different bytes.
    assert_ne!(bytes.as_slice(), other, "{}", draft.name);
    assert_eq!(bytes.len(), other.len(), "{}", draft.name);
    let proof = chunk_proof_bytes(tree, proof_chunk_index);
    let honest = chunk_proof_bytes(tree, draft.chunk_index);
    assert_ne!(proof, honest, "{}", draft.name);
    (bytes, proof)
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
                "spec §13: the empty proof is for a chunk count above 1"
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
        | Mutation::SegmentEqualsLeftAndRight => {
            panic!("not a chunk-proof mutation")
        }
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
        | Mutation::SegmentEqualsLeftAndRight => {
            panic!("not a chunk-byte mutation")
        }
    }
}

fn mutate_segment_proof(honest: &[u8], draft: &NegativeDraft) -> Vec<u8> {
    match draft.mutation {
        Mutation::FlipSiblingBit0 => {
            flip_sibling_bit_0(honest, draft.sibling_index.expect("sibling"))
        }
        Mutation::ZeroPadSegment => honest.to_vec(),
        Mutation::HonestChunk
        | Mutation::AppendOneZeroHash
        | Mutation::DropLastHash
        | Mutation::EmptyProof
        | Mutation::DropLastChunkByte
        | Mutation::AppendZeroChunkByte
        | Mutation::ProofFromOtherChunk
        | Mutation::IndexOutOfRange
        | Mutation::DataSizeZero
        | Mutation::SegmentEqualsLeftAndRight => {
            panic!("not a segment-proof mutation")
        }
    }
}

fn mutate_segment_bytes(geometry: &Geometry, segment: &[u8], mutation: Mutation) -> Vec<u8> {
    match mutation {
        Mutation::ZeroPadSegment => {
            let segment_size = usize::try_from(geometry.segment_size()).expect("segment size fits");
            // Padding must change the length. The verifier rejects that before hashing (spec §5).
            assert!(
                segment.len() < segment_size,
                "zero padding extends a short segment"
            );
            let mut bytes = segment.to_vec();
            bytes.resize(segment_size, 0);
            bytes
        }
        Mutation::FlipSiblingBit0 => segment.to_vec(),
        Mutation::HonestChunk
        | Mutation::AppendOneZeroHash
        | Mutation::DropLastHash
        | Mutation::EmptyProof
        | Mutation::DropLastChunkByte
        | Mutation::AppendZeroChunkByte
        | Mutation::ProofFromOtherChunk
        | Mutation::IndexOutOfRange
        | Mutation::DataSizeZero
        | Mutation::SegmentEqualsLeftAndRight => {
            panic!("not a segment-byte mutation")
        }
    }
}

/// Bit 0 of the sibling's first byte. One flipped bit per sibling (spec §13).
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

fn unanchored_commitment(
    geometry: &Geometry,
    tree: &DataTree,
    draft: &NegativeDraft,
    bytes: &[u8],
    proof: &[u8],
) -> Option<String> {
    if !draft.record_unanchored {
        return None;
    }
    // spec §6: a commitment derived from the real root and the wrong size passes
    // when the path shape matches. The anchored commitment does not.
    let rederived = data_commitment(&tree.data_tree_root(), draft.data_size);
    verify_chunk(
        geometry,
        &rederived,
        draft.data_size,
        draft.chunk_index,
        bytes,
        proof,
    )
    .expect("spec §6: re-derived commitment verifies");
    Some(hex_of(rederived.as_bytes()))
}

fn reject(
    geometry: &Geometry,
    draft: &NegativeDraft,
    commitment: &irys_data_tree::DataCommitment,
    bytes: &[u8],
    proof: &[u8],
) {
    let result = match draft.check {
        Check::VerifyChunk => verify_chunk(
            geometry,
            commitment,
            draft.data_size,
            draft.chunk_index,
            bytes,
            proof,
        ),
        Check::VerifySegment => verify_segment(
            geometry,
            commitment,
            draft.data_size,
            draft.chunk_index,
            draft.segment_index.expect("segment index"),
            bytes,
            proof,
        ),
    };
    match result {
        Ok(()) => panic!("{} verified", draft.name),
        Err(err) => assert_eq!(error_kind(&err), draft.expected_error, "{}", draft.name),
    }
}

fn two_digest_case() -> NegativeCase {
    // spec §3, §13: a 64-byte segment equal to `l ‖ r` is a leaf, never `node(l, r)`.
    let geometry = two_digest_geometry();
    let left = NodeHash([1_u8; HASH_SIZE]);
    let right = NodeHash([2_u8; HASH_SIZE]);
    let mut bytes = Vec::with_capacity(HASH_SIZE * 2);
    bytes.extend_from_slice(left.as_bytes());
    bytes.extend_from_slice(right.as_bytes());
    let leaf_hash = leaf(&bytes);
    let node_hash = node(&left, &right);
    assert_ne!(
        leaf_hash.as_bytes(),
        node_hash.as_bytes(),
        "a segment of two digests is not an internal node"
    );
    let data_size = u64::try_from(bytes.len()).expect("two digests fit");
    let tree = DataTree::from_bytes(geometry, &bytes).expect("two digests are one chunk");
    // `h = 0` and `C = 1`: the segment proof is the size witness (spec §5).
    let proof = tree
        .segment_proof(0, 0, &bytes)
        .expect("one segment of one chunk");
    assert_eq!(proof.as_slice(), size_witness(data_size).as_bytes());
    verify_segment(
        &geometry,
        &tree.data_commitment(),
        data_size,
        0,
        0,
        &bytes,
        &proof,
    )
    .expect("the segment verifies as a leaf");
    let mistaken = data_commitment(&DataTreeRoot(*node_hash.as_bytes()), data_size);
    let draft = NegativeDraft {
        name: "segment_equal_to_two_hashes_is_a_leaf",
        check: Check::VerifySegment,
        geometry: Some(geometry),
        pattern: None,
        source_data_size: data_size,
        data_size,
        chunk_index: 0,
        segment_index: Some(0),
        mutation: Mutation::SegmentEqualsLeftAndRight,
        sibling_index: None,
        proof_chunk_index: None,
        expected_error: "CommitmentMismatch",
        record_unanchored: false,
    };
    reject(&geometry, &draft, &mistaken, &bytes, &proof);
    NegativeCase {
        name: draft.name,
        check: draft.check,
        geometry: Some(geometry.into()),
        pattern: None,
        source_data_size: data_size,
        data_size,
        chunk_index: 0,
        segment_index: Some(0),
        mutation: draft.mutation,
        sibling_index: None,
        proof_chunk_index: None,
        left: Some(hex_of(left.as_bytes())),
        right: Some(hex_of(right.as_bytes())),
        leaf_hash: Some(hex_of(leaf_hash.as_bytes())),
        node_hash: Some(hex_of(node_hash.as_bytes())),
        commitment: hex_of(mistaken.as_bytes()),
        unanchored_commitment: None,
        proof: hex_of(&proof),
        expected_error: draft.expected_error,
    }
}

fn two_digest_geometry() -> Geometry {
    let chunk_size = u64::try_from(HASH_SIZE).expect("hash width fits") * 2;
    Geometry::new(chunk_size, 1, 1).expect("two digests are one segment")
}

fn build_upload(geometry: Geometry, pattern: &str, data_size: u64) -> (Vec<u8>, DataTree) {
    let data = data_bytes(pattern, data_size);
    let tree = DataTree::from_bytes(geometry, &data)
        .unwrap_or_else(|err| panic!("build {pattern} data_size {data_size}: {err}"));
    (data, tree)
}

fn data_bytes(pattern: &str, data_size: u64) -> Vec<u8> {
    let byte_length = usize::try_from(data_size).expect("data size fits usize");
    match pattern {
        MOD251 => mod251_bytes(byte_length),
        ZEROS => vec![0; byte_length],
        _ => panic!("unknown data pattern {pattern}"),
    }
}

fn mod251_bytes(byte_length: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(byte_length);
    for byte_index in 0..byte_length {
        let reduced = byte_index % MOD251_MODULUS;
        data.push(u8::try_from(reduced).expect("mod 251 fits in a byte"));
    }
    data
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

fn chunk_proof_bytes(tree: &DataTree, chunk_index: u32) -> Vec<u8> {
    tree.chunk_proof(chunk_index)
        .unwrap_or_else(|err| panic!("chunk proof {chunk_index}: {err}"))
}

fn hex_of(bytes: impl AsRef<[u8]>) -> String {
    hex::encode(bytes)
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

#[derive(Serialize)]
struct VectorFile<T> {
    description: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    geometry: Option<GeometryJson>,
    cases: Vec<T>,
}

#[derive(Serialize, Clone, Copy)]
struct GeometryJson {
    chunk_size: u64,
    segments_per_chunk: u32,
    max_data_tx_chunks: u32,
}

impl From<Geometry> for GeometryJson {
    fn from(geometry: Geometry) -> Self {
        Self {
            chunk_size: geometry.chunk_size(),
            segments_per_chunk: geometry.segments_per_chunk(),
            max_data_tx_chunks: geometry.max_data_tx_chunks(),
        }
    }
}

#[derive(Serialize)]
struct LeafInput {
    pattern: &'static str,
    data_size: u64,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PrimitiveCase {
    Leaf {
        pattern: &'static str,
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
        pattern: &'static str,
        data_size: u64,
        geometry: GeometryJson,
        chunk_index: u32,
        commitment: String,
        hash: String,
    },
}

#[derive(Serialize)]
struct RootCase {
    pattern: &'static str,
    data_size: u64,
    chunk_count: u32,
    chunk_roots: Vec<String>,
    data_tree_root: String,
    size_witness: String,
    data_commitment: String,
}

#[derive(Serialize)]
struct ChunkProofCase {
    geometry: GeometryJson,
    pattern: &'static str,
    data_size: u64,
    chunk_index: u32,
    chunk_count: u32,
    depth: u32,
    data_commitment: String,
    proof: String,
}

#[derive(Serialize)]
struct SegmentProofCase {
    geometry: GeometryJson,
    pattern: &'static str,
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
    segment_length: u64,
    chunk_count: u32,
    data_commitment: String,
    proof: String,
}

#[derive(Serialize)]
struct MultiSegmentCase {
    pattern: &'static str,
    data_size: u64,
    data_commitment: String,
    chunks: Vec<MultiSegmentChunk>,
}

#[derive(Serialize)]
struct MultiSegmentChunk {
    chunk_index: u32,
    chunk_proof: String,
    segments: Vec<MultiSegmentSegment>,
}

#[derive(Serialize)]
struct MultiSegmentSegment {
    segment_index: u32,
    in_chunk_siblings: String,
}

#[derive(Serialize)]
struct NegativeCase {
    name: &'static str,
    check: Check,
    #[serde(skip_serializing_if = "Option::is_none")]
    geometry: Option<GeometryJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pattern: Option<&'static str>,
    source_data_size: u64,
    data_size: u64,
    chunk_index: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    segment_index: Option<u32>,
    mutation: Mutation,
    #[serde(skip_serializing_if = "Option::is_none")]
    sibling_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proof_chunk_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    left: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    right: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    leaf_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    node_hash: Option<String>,
    commitment: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    unanchored_commitment: Option<String>,
    proof: String,
    expected_error: &'static str,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Check {
    VerifyChunk,
    VerifySegment,
}

#[derive(Serialize, Clone, Copy, PartialEq, Eq)]
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

#[derive(Clone, Copy)]
struct NegativeDraft {
    name: &'static str,
    check: Check,
    geometry: Option<Geometry>,
    pattern: Option<&'static str>,
    source_data_size: u64,
    data_size: u64,
    chunk_index: u32,
    segment_index: Option<u32>,
    mutation: Mutation,
    sibling_index: Option<u32>,
    proof_chunk_index: Option<u32>,
    expected_error: &'static str,
    record_unanchored: bool,
}
