//! Checks the crate against a reference written from `docs/spec.md` only.
//!
//! `reference` imports nothing from the crate. Three groups of tests:
//!
//! - A pins the SHA-256 primitive with FIPS 180-4 known answers.
//! - B re-derives every committed vector file with the reference alone. These
//!   tests never call the crate, so a systematic hash-layer bug in the crate
//!   cannot hide behind expected values built by the crate itself.
//! - C compares the crate with the reference over many geometries, sizes,
//!   indexes and mutated inputs (module `differential`, the only user of the crate).

#[path = "spec_reference/reference.rs"]
mod reference;

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use reference::{Geometry, Hash, Rejection};
use serde::Deserialize;

/// spec §10: `b[i] = i mod 251`.
const MOD251_MODULUS: u64 = 251;
const MOD251: &str = "mod251";
const ZEROS: &str = "zeros";

/// Vector files in `vectors/` (spec §10).
const VECTOR_FILE_NAMES: [&str; 7] = [
    "chunk_proofs.json",
    "multi_segment.json",
    "negative.json",
    "primitives.json",
    "roots_mainnet.json",
    "roots_testing.json",
    "segment_proofs.json",
];

/// Bytes of `pattern` from byte offset `start`, `length` bytes (spec §10).
fn pattern_bytes(pattern: &str, start: u64, length: usize) -> Vec<u8> {
    match pattern {
        MOD251 => (0..length as u64)
            .map(|byte_index| {
                u8::try_from((start + byte_index) % MOD251_MODULUS).expect("below 251 fits u8")
            })
            .collect(),
        ZEROS => vec![0; length],
        _ => panic!("unknown data pattern {pattern}"),
    }
}

fn hex_of(hash: &[u8]) -> String {
    hex::encode(hash)
}

// ---------------------------------------------------------------------------
// A. SHA-256 known answers
// ---------------------------------------------------------------------------

/// FIPS 180-4 digest of the empty message.
const SHA256_OF_EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
/// FIPS 180-4 digest of "abc".
const SHA256_OF_ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

#[test]
fn sha256_matches_fips_known_answers() {
    assert_eq!(hex_of(&reference::sha256(b"")), SHA256_OF_EMPTY);
    assert_eq!(hex_of(&reference::sha256(b"abc")), SHA256_OF_ABC);
}

// ---------------------------------------------------------------------------
// B. The reference against the committed vector files
// ---------------------------------------------------------------------------

fn vector_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vectors")
}

fn read_vector_file<T: for<'de> Deserialize<'de>>(file_name: &str) -> VectorFile<T> {
    let path = vector_directory().join(file_name);
    let text = fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {file_name}: {err}"));
    serde_json::from_str(&text).unwrap_or_else(|err| panic!("parse {file_name}: {err}"))
}

#[derive(Deserialize)]
struct VectorFile<T> {
    #[serde(default)]
    geometry: Option<GeometryJson>,
    cases: Vec<T>,
}

#[derive(Deserialize, Clone, Copy)]
struct GeometryJson {
    chunk_size: u64,
    segments_per_chunk: u32,
    max_data_tx_chunks: u32,
}

impl GeometryJson {
    fn to_reference(self) -> Geometry {
        Geometry::new(
            self.chunk_size,
            self.segments_per_chunk,
            self.max_data_tx_chunks,
        )
        .unwrap_or_else(|err| panic!("vector geometry: {err}"))
    }
}

#[derive(Deserialize)]
struct LeafInput {
    pattern: String,
    data_size: u64,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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

#[derive(Deserialize)]
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

#[derive(Deserialize)]
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

#[derive(Deserialize)]
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
    /// Set when one empty leaf is replaced by a stand-in hash (spec §5).
    #[serde(default)]
    replaced_leaf: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct MultiSegmentCase {
    pattern: String,
    data_size: u64,
    data_commitment: String,
    chunks: Vec<MultiSegmentChunk>,
}

#[derive(Deserialize)]
struct MultiSegmentChunk {
    chunk_index: u32,
    chunk_proof: String,
    segments: Vec<MultiSegmentSegment>,
}

#[derive(Deserialize)]
struct MultiSegmentSegment {
    segment_index: u32,
    in_chunk_siblings: String,
}

#[derive(Deserialize)]
struct NegativeCase {
    name: String,
    check: String,
    #[serde(default)]
    geometry: Option<GeometryJson>,
    #[serde(default)]
    pattern: Option<String>,
    source_data_size: u64,
    data_size: u64,
    chunk_index: u32,
    #[serde(default)]
    segment_index: Option<u32>,
    mutation: String,
    #[serde(default)]
    proof_chunk_index: Option<u32>,
    #[serde(default)]
    left: Option<String>,
    #[serde(default)]
    right: Option<String>,
    commitment: String,
    proof: String,
    expected_error: String,
}

/// Chunk root of an upload made from a pattern, with one memo per distinct chunk.
///
/// A chunk root depends only on the chunk bytes (spec §4). Chunks of one
/// pattern repeat, so a 1 GiB upload has few distinct chunks. The memo calls
/// the reference `chunk_root` once per distinct chunk.
#[derive(Default)]
struct ChunkRootMemo {
    roots: HashMap<(u64, u32, bool, u64, usize), Hash>,
}

impl ChunkRootMemo {
    fn chunk_root(
        &mut self,
        geometry: &Geometry,
        pattern: &str,
        chunk_start: u64,
        chunk_length: usize,
    ) -> Hash {
        let phase = if pattern == MOD251 {
            chunk_start % MOD251_MODULUS
        } else {
            0
        };
        let key = (
            geometry.chunk_size,
            geometry.segments_per_chunk,
            pattern == MOD251,
            phase,
            chunk_length,
        );
        *self.roots.entry(key).or_insert_with(|| {
            let bytes = pattern_bytes(pattern, chunk_start, chunk_length);
            reference::chunk_root(geometry, &bytes)
        })
    }
}

/// An upload of `data_size` pattern bytes, kept as its reference chunk roots and chunk leaves.
struct PatternUpload {
    geometry: Geometry,
    pattern: String,
    data_size: u64,
    chunk_roots: Vec<Hash>,
    chunk_leaves: Vec<Hash>,
}

impl PatternUpload {
    fn build(memo: &mut ChunkRootMemo, geometry: Geometry, pattern: &str, data_size: u64) -> Self {
        let chunk_count = geometry.chunk_count(data_size);
        let chunk_roots = (0..chunk_count)
            .map(|chunk_index| {
                let chunk_start = u64::from(chunk_index) * geometry.chunk_size;
                let chunk_length = geometry.chunk_len(data_size, chunk_index);
                memo.chunk_root(
                    &geometry,
                    pattern,
                    chunk_start,
                    usize::try_from(chunk_length).expect("chunk length fits"),
                )
            })
            .collect::<Vec<Hash>>();
        let chunk_leaves = reference::chunk_leaves(&chunk_roots);
        Self {
            geometry,
            pattern: pattern.to_string(),
            data_size,
            chunk_roots,
            chunk_leaves,
        }
    }

    fn chunk_count(&self) -> u32 {
        u32::try_from(self.chunk_roots.len()).expect("chunk count fits")
    }

    fn data_tree_root(&self) -> Hash {
        reference::data_tree_root(&self.chunk_leaves)
    }

    fn size_witness(&self) -> Hash {
        reference::size_witness(&self.geometry, self.data_size)
    }

    fn commitment(&self) -> Hash {
        reference::data_commitment(&self.geometry, &self.data_tree_root(), self.data_size)
    }

    fn chunk_bytes(&self, chunk_index: u32) -> Vec<u8> {
        let chunk_start = u64::from(chunk_index) * self.geometry.chunk_size;
        let chunk_length = self.geometry.chunk_len(self.data_size, chunk_index);
        pattern_bytes(
            &self.pattern,
            chunk_start,
            usize::try_from(chunk_length).expect("chunk length fits"),
        )
    }

    fn segment_bytes(&self, chunk_index: u32, segment_index: u32) -> Vec<u8> {
        let segment_start = u64::from(chunk_index) * self.geometry.chunk_size
            + u64::from(segment_index) * self.geometry.segment_size();
        let segment_length = self
            .geometry
            .segment_len(self.data_size, chunk_index, segment_index);
        pattern_bytes(
            &self.pattern,
            segment_start,
            usize::try_from(segment_length).expect("segment length fits"),
        )
    }

    fn chunk_proof(&self, chunk_index: u32) -> Vec<u8> {
        reference::chunk_proof(
            &self.geometry,
            self.data_size,
            &self.chunk_leaves,
            chunk_index,
        )
    }

    fn segment_proof(&self, chunk_index: u32, segment_index: u32) -> Vec<u8> {
        reference::segment_proof(
            &self.geometry,
            self.data_size,
            &self.chunk_leaves,
            chunk_index,
            segment_index,
            &self.chunk_bytes(chunk_index),
        )
    }
}

/// Memoised uploads, keyed by geometry, pattern and size.
#[derive(Default)]
struct UploadSet {
    memo: ChunkRootMemo,
    uploads: HashMap<(u64, u32, u32, String, u64), PatternUpload>,
}

impl UploadSet {
    fn get(&mut self, geometry: GeometryJson, pattern: &str, data_size: u64) -> &PatternUpload {
        let key = (
            geometry.chunk_size,
            geometry.segments_per_chunk,
            geometry.max_data_tx_chunks,
            pattern.to_string(),
            data_size,
        );
        let Self { memo, uploads } = self;
        uploads.entry(key).or_insert_with(|| {
            PatternUpload::build(memo, geometry.to_reference(), pattern, data_size)
        })
    }
}

fn hash_of(text: &str) -> Hash {
    Hash::try_from(hex::decode(text).expect("hex").as_slice()).expect("a hash is 32 bytes")
}

fn rejection_of_name(name: &str) -> Rejection {
    match name {
        "DataSizeOutOfRange" | "ChunkIndexOutOfRange" | "SegmentIndexOutOfRange" => {
            Rejection::Range
        }
        "InvalidChunkLength" | "InvalidSegmentLength" => Rejection::ByteLength,
        "InvalidProofLength" => Rejection::ProofLength,
        "SizeWitnessMismatch" => Rejection::SizeWitness,
        "CommitmentMismatch" => Rejection::Commitment,
        _ => panic!("unknown expected error {name}"),
    }
}

#[test]
fn vector_directory_holds_exactly_the_checked_files() {
    let mut names: Vec<String> = fs::read_dir(vector_directory())
        .expect("vectors directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf-8 name")
        })
        .filter(|name| name.ends_with(".json"))
        .collect();
    names.sort();
    assert_eq!(names, VECTOR_FILE_NAMES);
}

#[test]
fn reference_matches_primitive_vectors() {
    let file = read_vector_file::<PrimitiveCase>("primitives.json");
    let mut uploads = UploadSet::default();
    for case in &file.cases {
        match case {
            PrimitiveCase::Leaf {
                pattern,
                data_size,
                hash,
            } => {
                let bytes = pattern_bytes(pattern, 0, usize::try_from(*data_size).unwrap());
                assert_eq!(hex_of(&reference::leaf(&bytes)), *hash, "leaf {data_size}");
            }
            PrimitiveCase::Node { left, right, hash } => {
                let left_leaf = reference::leaf(&pattern_bytes(
                    &left.pattern,
                    0,
                    usize::try_from(left.data_size).unwrap(),
                ));
                let right_leaf = reference::leaf(&pattern_bytes(
                    &right.pattern,
                    0,
                    usize::try_from(right.data_size).unwrap(),
                ));
                assert_eq!(hex_of(&reference::node(&left_leaf, &right_leaf)), *hash);
            }
            PrimitiveCase::SizeWitness {
                data_size,
                geometry,
                hash,
            } => {
                let witness = reference::size_witness(&geometry.to_reference(), *data_size);
                assert_eq!(hex_of(&witness), *hash, "size_witness {data_size}");
            }
            PrimitiveCase::Empty { empty_height, hash } => {
                assert_eq!(
                    hex_of(&reference::empty(*empty_height)),
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
                let upload = uploads.get(*geometry, pattern, *data_size);
                assert_eq!(hex_of(&upload.commitment()), *commitment);
                // The index is a hash input: it need not be a chunk of this upload.
                let key = reference::chunk_key(&upload.commitment(), *chunk_index);
                assert_eq!(hex_of(&key), *hash, "chunk_key {chunk_index}");
            }
            PrimitiveCase::ChunkLeaf {
                chunk_index,
                chunk_root,
                hash,
            } => {
                let leaf = reference::chunk_leaf(*chunk_index, &hash_of(chunk_root));
                assert_eq!(hex_of(&leaf), *hash, "chunk_leaf {chunk_index}");
            }
        }
    }
}

fn check_root_file(file_name: &str) {
    let file = read_vector_file::<RootCase>(file_name);
    let geometry = file.geometry.expect("root file geometry");
    let mut uploads = UploadSet::default();
    for case in &file.cases {
        let label = format!("{file_name} {} {}", case.pattern, case.data_size);
        let upload = uploads.get(geometry, &case.pattern, case.data_size);
        assert_eq!(upload.chunk_count(), case.chunk_count, "{label}");
        let chunk_roots: Vec<String> = upload.chunk_roots.iter().map(|root| hex_of(root)).collect();
        assert_eq!(chunk_roots, case.chunk_roots, "{label} chunk roots");
        let chunk_leaves: Vec<String> = upload
            .chunk_leaves
            .iter()
            .map(|leaf| hex_of(leaf))
            .collect();
        assert_eq!(chunk_leaves, case.chunk_leaves, "{label} chunk leaves");
        assert_eq!(
            hex_of(&upload.data_tree_root()),
            case.data_tree_root,
            "{label}"
        );
        assert_eq!(hex_of(&upload.size_witness()), case.size_witness, "{label}");
        assert_eq!(
            hex_of(&upload.commitment()),
            case.data_commitment,
            "{label}"
        );
    }
}

#[test]
fn reference_matches_testing_root_vectors() {
    check_root_file("roots_testing.json");
}

#[test]
fn reference_matches_mainnet_root_vectors() {
    check_root_file("roots_mainnet.json");
}

#[test]
fn reference_matches_chunk_proof_vectors() {
    let file = read_vector_file::<ChunkProofCase>("chunk_proofs.json");
    let mut uploads = UploadSet::default();
    for case in &file.cases {
        let label = format!(
            "chunk proof n={} c={} chunk_size={}",
            case.data_size, case.chunk_index, case.geometry.chunk_size
        );
        assert_eq!(case.pattern, MOD251, "{label}");
        let upload = uploads.get(case.geometry, &case.pattern, case.data_size);
        assert_eq!(upload.chunk_count(), case.chunk_count, "{label}");
        assert_eq!(
            reference::depth(case.chunk_index, case.chunk_count),
            case.depth,
            "{label}"
        );
        let commitment = upload.commitment();
        assert_eq!(hex_of(&commitment), case.data_commitment, "{label}");
        assert_eq!(
            case.size_proof,
            case.chunk_index == reference::rightmost_chunk(&upload.geometry, case.data_size),
            "{label} size_proof"
        );
        let proof = upload.chunk_proof(case.chunk_index);
        assert_eq!(hex_of(&proof), case.proof, "{label}");
        reference::verify_chunk(
            &upload.geometry,
            &commitment,
            case.data_size,
            case.chunk_index,
            &upload.chunk_bytes(case.chunk_index),
            &proof,
        )
        .unwrap_or_else(|rejection| panic!("{label}: {rejection:?}"));
    }
}

#[test]
fn reference_matches_segment_proof_vectors() {
    let file = read_vector_file::<SegmentProofCase>("segment_proofs.json");
    let mut uploads = UploadSet::default();
    for case in &file.cases {
        let label = format!(
            "segment proof n={} c={} j={} chunk_size={}",
            case.data_size, case.chunk_index, case.segment_index, case.geometry.chunk_size
        );
        assert_eq!(case.pattern, MOD251, "{label}");
        let upload = uploads.get(case.geometry, &case.pattern, case.data_size);
        assert_eq!(upload.chunk_count(), case.chunk_count, "{label}");
        let segment_length =
            upload
                .geometry
                .segment_len(case.data_size, case.chunk_index, case.segment_index);
        assert_eq!(segment_length, case.segment_length, "{label}");
        assert_eq!(
            case.size_proof,
            (case.chunk_index, case.segment_index)
                == reference::rightmost_segment(&upload.geometry, case.data_size),
            "{label} size_proof"
        );
        let segment = upload.segment_bytes(case.chunk_index, case.segment_index);
        if case.replaced_leaf.is_some() {
            // spec §5: a bad hash in an empty leaf of a mixed sibling does not
            // fail these data segments. The commitment is the malicious one.
            assert_ne!(
                hex_of(&upload.commitment()),
                case.data_commitment,
                "{label}"
            );
        } else {
            assert_eq!(
                hex_of(&upload.commitment()),
                case.data_commitment,
                "{label}"
            );
            let proof = upload.segment_proof(case.chunk_index, case.segment_index);
            assert_eq!(hex_of(&proof), case.proof, "{label}");
        }
        reference::verify_segment(
            &upload.geometry,
            &hash_of(&case.data_commitment),
            case.data_size,
            case.chunk_index,
            case.segment_index,
            &segment,
            &hex::decode(&case.proof).expect("hex"),
        )
        .unwrap_or_else(|rejection| panic!("{label}: {rejection:?}"));
    }
}

#[test]
fn reference_matches_multi_segment_vectors() {
    let file = read_vector_file::<MultiSegmentCase>("multi_segment.json");
    let geometry = file.geometry.expect("multi-segment geometry");
    let mut uploads = UploadSet::default();
    for case in &file.cases {
        let upload = uploads.get(geometry, &case.pattern, case.data_size);
        assert_eq!(hex_of(&upload.commitment()), case.data_commitment);
        for chunk in &case.chunks {
            let label = format!("multi-segment chunk {}", chunk.chunk_index);
            let chunk_proof = upload.chunk_proof(chunk.chunk_index);
            assert_eq!(hex_of(&chunk_proof), chunk.chunk_proof, "{label}");
            for segment in &chunk.segments {
                let label = format!("{label} segment {}", segment.segment_index);
                let full_proof = upload.segment_proof(chunk.chunk_index, segment.segment_index);
                let siblings_length =
                    reference::HASH_SIZE * upload.geometry.subtree_height() as usize;
                assert_eq!(
                    hex_of(&full_proof[..siblings_length]),
                    segment.in_chunk_siblings,
                    "{label}"
                );
                // spec §5: the full proof is the siblings, then the shared chunk proof.
                assert_eq!(
                    &full_proof[siblings_length..],
                    chunk_proof.as_slice(),
                    "{label}"
                );
                reference::verify_segment(
                    &upload.geometry,
                    &upload.commitment(),
                    case.data_size,
                    chunk.chunk_index,
                    segment.segment_index,
                    &upload.segment_bytes(chunk.chunk_index, segment.segment_index),
                    &full_proof,
                )
                .unwrap_or_else(|rejection| panic!("{label}: {rejection:?}"));
            }
        }
    }
}

/// Mutations whose commitment in the file is the honest commitment of the source upload.
fn commitment_is_honest(mutation: &str) -> bool {
    !matches!(
        mutation,
        "segment_equals_left_and_right"
            | "pinned_sibling_not_empty"
            | "bad_empty_leaf"
            | "data_size_above_max"
            | "index_out_of_range"
            | "data_size_zero"
            | "chunk_proof_at_moved_index"
            | "rightmost_proof_of_claimed_size"
            | "commitment_over_chunk_root"
            | "padding_slot_as_rightmost_segment"
    )
}

/// Replace the last hash of a proof (the size witness) with `witness`.
fn replace_last_hash(proof: &mut [u8], witness: &Hash) {
    let witness_start = proof.len() - reference::HASH_SIZE;
    proof[witness_start..].copy_from_slice(witness);
}

/// Inputs of the cases that claim another size, another index, or another
/// commitment (spec §6, §9). The reference rebuilds the file's proof and
/// commitment and returns the verifier bytes. `None` for every other mutation.
fn claimed_inputs(
    uploads: &mut UploadSet,
    geometry: GeometryJson,
    case: &NegativeCase,
) -> Option<Vec<u8>> {
    let name = case.name.as_str();
    let pattern = case.pattern.as_deref().unwrap_or(MOD251);
    let (bytes, proof, commitment) = match case.mutation.as_str() {
        "chunk_proof_at_moved_index" => {
            // The honest bytes and siblings of another chunk, the claimed size's witness.
            let source = uploads.get(geometry, pattern, case.source_data_size);
            let proof_chunk_index = case.proof_chunk_index.expect("proof chunk index");
            let mut proof = source.chunk_proof(proof_chunk_index);
            replace_last_hash(
                &mut proof,
                &reference::size_witness(&source.geometry, case.data_size),
            );
            let commitment = reference::data_commitment(
                &source.geometry,
                &source.data_tree_root(),
                case.data_size,
            );
            (source.chunk_bytes(proof_chunk_index), proof, commitment)
        }
        "rightmost_proof_of_claimed_size" => {
            // The claimed upload's own rightmost proof against the source root.
            let source_root = uploads
                .get(geometry, pattern, case.source_data_size)
                .data_tree_root();
            let claimed = uploads.get(geometry, pattern, case.data_size);
            let (chunk_index, segment_index) =
                reference::rightmost_segment(&claimed.geometry, case.data_size);
            assert_eq!(case.chunk_index, chunk_index, "{name}: rightmost chunk");
            let (bytes, proof) = match case.segment_index {
                None => (
                    claimed.chunk_bytes(chunk_index),
                    claimed.chunk_proof(chunk_index),
                ),
                Some(claimed_segment_index) => {
                    assert_eq!(
                        claimed_segment_index, segment_index,
                        "{name}: rightmost segment"
                    );
                    (
                        claimed.segment_bytes(chunk_index, segment_index),
                        claimed.segment_proof(chunk_index, segment_index),
                    )
                }
            };
            let commitment =
                reference::data_commitment(&claimed.geometry, &source_root, case.data_size);
            (bytes, proof, commitment)
        }
        "commitment_over_chunk_root" => {
            // A commitment whose root is the chunk root, not the chunk leaf.
            let source = uploads.get(geometry, pattern, case.source_data_size);
            assert_eq!(source.chunk_count(), 1, "{name}: one chunk");
            let bytes = source.chunk_bytes(case.chunk_index);
            let commitment = reference::node(
                &reference::chunk_root(&source.geometry, &bytes),
                &source.size_witness(),
            );
            (bytes, source.chunk_proof(case.chunk_index), commitment)
        }
        "padding_slot_as_rightmost_segment" => {
            // The source siblings of a padding slot, the claimed upload's bytes there.
            let segment_index = case.segment_index.expect("segment index");
            let source = uploads.get(geometry, pattern, case.source_data_size);
            let mut proof = source.segment_proof(case.chunk_index, segment_index);
            replace_last_hash(
                &mut proof,
                &reference::size_witness(&source.geometry, case.data_size),
            );
            let commitment = reference::data_commitment(
                &source.geometry,
                &source.data_tree_root(),
                case.data_size,
            );
            let claimed = uploads.get(geometry, pattern, case.data_size);
            assert_eq!(
                (case.chunk_index, segment_index),
                reference::rightmost_segment(&claimed.geometry, case.data_size),
                "{name}: rightmost segment of the claimed size"
            );
            (
                claimed.segment_bytes(case.chunk_index, segment_index),
                proof,
                commitment,
            )
        }
        _ => return None,
    };
    assert_eq!(hex_of(&proof), case.proof, "{name} proof");
    assert_eq!(hex_of(&commitment), case.commitment, "{name} commitment");
    Some(bytes)
}

#[test]
fn reference_matches_negative_vectors() {
    let file = read_vector_file::<NegativeCase>("negative.json");
    let file_geometry = file.geometry.expect("negative file geometry");
    let mut uploads = UploadSet::default();
    for case in &file.cases {
        let name = case.name.as_str();
        let geometry_json = case.geometry.unwrap_or(file_geometry);
        let geometry = geometry_json.to_reference();
        let expected = Err(rejection_of_name(&case.expected_error));
        let commitment = hash_of(&case.commitment);
        let proof = hex::decode(&case.proof).expect("hex");
        let is_segment_check = case.check == "verify_segment";
        let segment_index = case.segment_index.unwrap_or(0);

        // Range cases hand the verifier bytes that cannot matter: the range
        // check runs first (spec §5). The two-digest case carries its own bytes.
        let mut bytes = Vec::new();
        if case.mutation == "segment_equals_left_and_right" {
            bytes.extend_from_slice(&hash_of(case.left.as_deref().expect("left")));
            bytes.extend_from_slice(&hash_of(case.right.as_deref().expect("right")));
        } else if let Some(claimed_bytes) = claimed_inputs(&mut uploads, geometry_json, case) {
            bytes = claimed_bytes;
        } else if !matches!(
            case.mutation.as_str(),
            "index_out_of_range" | "data_size_zero" | "data_size_above_max"
        ) {
            let pattern = case.pattern.as_deref().expect("pattern");
            let upload = uploads.get(geometry_json, pattern, case.source_data_size);
            if commitment_is_honest(&case.mutation) {
                assert_eq!(hex_of(&upload.commitment()), case.commitment, "{name}");
            }
            bytes = if is_segment_check {
                upload.segment_bytes(case.chunk_index, segment_index)
            } else {
                upload.chunk_bytes(case.chunk_index)
            };
            match case.mutation.as_str() {
                "drop_last_chunk_byte" => {
                    bytes.pop();
                }
                "append_zero_chunk_byte" => bytes.push(0),
                "zero_pad_segment" => {
                    bytes.resize(usize::try_from(geometry.segment_size()).unwrap(), 0);
                }
                _ => {}
            }
            // Where the file proof is an honest proof, the reference builds the same bytes.
            let honest_proof = match case.mutation.as_str() {
                "honest_chunk" | "drop_last_chunk_byte" | "append_zero_chunk_byte" => {
                    Some(upload.chunk_proof(case.chunk_index))
                }
                "proof_from_other_chunk" => {
                    Some(upload.chunk_proof(case.proof_chunk_index.expect("proof chunk index")))
                }
                "zero_pad_segment" => Some(upload.segment_proof(case.chunk_index, segment_index)),
                _ => None,
            };
            if let Some(honest_proof) = honest_proof {
                assert_eq!(hex_of(&honest_proof), case.proof, "{name}");
            }
        }

        let outcome = if is_segment_check {
            reference::verify_segment(
                &geometry,
                &commitment,
                case.data_size,
                case.chunk_index,
                segment_index,
                &bytes,
                &proof,
            )
        } else {
            reference::verify_chunk(
                &geometry,
                &commitment,
                case.data_size,
                case.chunk_index,
                &bytes,
                &proof,
            )
        };
        assert_eq!(outcome, expected, "{name}");
    }
}

// ---------------------------------------------------------------------------
// C. The crate against the reference
// ---------------------------------------------------------------------------

mod differential {
    use super::pattern_bytes;
    use super::{BTreeSet, Geometry, Hash, MOD251, Rejection, reference};
    use irys_data_tree::{
        DataCommitment, DataTree, Geometry as CrateGeometry, MAX_SUBTREE_HEIGHT, SubtreeHeight,
        VerifyError, depth, empty,
    };

    /// spec §2: the maximum chunk count of every geometry here.
    const MAX_DATA_TX_CHUNKS: u32 = 20_971_520;

    /// spec §2 mainnet geometry.
    const MAINNET_CHUNK_SIZE: u64 = 262_144;
    const MAINNET_SEGMENTS_PER_CHUNK: u32 = 64;
    /// spec §2 testing geometry.
    const TESTING_CHUNK_SIZE: u64 = 32;
    const TESTING_SEGMENTS_PER_CHUNK: u32 = 4;

    /// Chunk counts for which `depth(c, C)` is compared.
    const DEPTH_CHUNK_COUNT_LIMIT: u32 = 130;

    /// Chunks in the largest uploads of the small geometries (at least 40, so
    /// the chunk level has 5 or 6 levels and several carried nodes).
    const SMALL_GEOMETRY_CHUNK_LIMIT: u64 = 40;

    /// Chunks whose every size is compared in the geometries that are not the
    /// testing geometry. Their larger uploads are compared at chunk boundaries.
    const DENSE_CHUNK_LIMIT: u64 = 10;

    /// Chunks of the boundary sizes in the maximum-segments geometry.
    const MAX_SEGMENT_GEOMETRY_CHUNK_LIMIT: u64 = 12;

    /// Bit flipped in a proof or byte string: bit 0 of the byte.
    const FLIPPED_BIT: u8 = 1;

    /// A geometry as the reference and as the crate build it.
    struct Pair {
        reference: Geometry,
        crate_geometry: CrateGeometry,
    }

    impl Pair {
        fn new(chunk_size: u64, segments_per_chunk: u32) -> Self {
            Self {
                reference: Geometry::new(chunk_size, segments_per_chunk, MAX_DATA_TX_CHUNKS)
                    .expect("reference geometry"),
                crate_geometry: CrateGeometry::new(
                    chunk_size,
                    segments_per_chunk,
                    MAX_DATA_TX_CHUNKS,
                )
                .expect("crate geometry"),
            }
        }
    }

    /// Which sizes, chunks and segments one geometry covers.
    struct Plan {
        chunk_size: u64,
        segments_per_chunk: u32,
        /// Sizes whose roots, keys, proofs and honest verification are compared.
        sizes: Vec<u64>,
        /// Sizes that also run the mutated-input comparison. A subset of `sizes`.
        mutated_sizes: BTreeSet<u64>,
        /// Compare every segment index of every chunk in the honest checks.
        every_segment: bool,
    }

    /// Every size from 1 through `chunk_limit` chunks.
    fn all_sizes(chunk_size: u64, chunk_limit: u64) -> Vec<u64> {
        (1..=chunk_size * chunk_limit).collect()
    }

    /// Sizes at and around every chunk boundary, up to `chunk_limit` chunks.
    ///
    /// The chunk count, the last chunk length and the data segment count change
    /// at `k × chunk_size`, so the sizes just below, at and just above it are
    /// the cases where the shape changes.
    fn chunk_boundary_sizes(chunk_size: u64, chunk_limit: u64) -> BTreeSet<u64> {
        let mut sizes = BTreeSet::new();
        for chunk_number in 1..=chunk_limit {
            let boundary = chunk_number * chunk_size;
            sizes.extend([boundary - 1, boundary, boundary + 1]);
        }
        sizes.remove(&0);
        sizes.retain(|&size| size <= chunk_limit * chunk_size);
        sizes
    }

    /// Sizes just below, at and just above segment edges inside the first two chunks.
    ///
    /// The data segment count of the last chunk changes there, and so does the
    /// set of in-chunk siblings that spec §5 pins to `empty(level)`.
    fn segment_edge_sizes(chunk_size: u64, segments_per_chunk: u32, edges: &[u32]) -> Vec<u64> {
        let segment_size = chunk_size / u64::from(segments_per_chunk);
        let mut sizes = BTreeSet::new();
        for last_chunk_start in [0, chunk_size] {
            for &edge in edges {
                let size = last_chunk_start + u64::from(edge) * segment_size;
                sizes.extend([size - 1, size, size + 1]);
            }
        }
        sizes.remove(&0);
        sizes.into_iter().collect()
    }

    fn rejection_of(error: VerifyError) -> Rejection {
        match error {
            VerifyError::Size(_) => Rejection::Range,
            VerifyError::InvalidChunkLength { .. } | VerifyError::InvalidSegmentLength { .. } => {
                Rejection::ByteLength
            }
            VerifyError::InvalidProofLength { .. } => Rejection::ProofLength,
            VerifyError::SizeWitnessMismatch => Rejection::SizeWitness,
            VerifyError::CommitmentMismatch => Rejection::Commitment,
        }
    }

    /// Which outcomes the comparisons produced, to show that none is vacuous.
    #[derive(Default)]
    struct Tally {
        accepted: usize,
        range: usize,
        byte_length: usize,
        proof_length: usize,
        size_witness: usize,
        commitment: usize,
    }

    impl Tally {
        fn record(&mut self, outcome: Result<(), Rejection>) {
            match outcome {
                Ok(()) => self.accepted += 1,
                Err(Rejection::Range) => self.range += 1,
                Err(Rejection::ByteLength) => self.byte_length += 1,
                Err(Rejection::ProofLength) => self.proof_length += 1,
                Err(Rejection::SizeWitness) => self.size_witness += 1,
                Err(Rejection::Commitment) => self.commitment += 1,
            }
        }

        fn assert_every_rejection_seen(&self) {
            assert!(self.accepted > 0, "no honest case accepted");
            assert!(self.range > 0, "no range rejection");
            assert!(self.byte_length > 0, "no byte-length rejection");
            assert!(self.proof_length > 0, "no proof-length rejection");
            assert!(self.size_witness > 0, "no size-witness rejection");
            assert!(self.commitment > 0, "no commitment rejection");
        }
    }

    /// One input to both verifiers. `what` names the change made to an honest input.
    struct Input {
        what: &'static str,
        /// Which hash, size or index `what` changed.
        detail: u64,
        data_size: u64,
        chunk_index: u32,
        /// `Some(j)` for a segment check, `None` for a chunk check.
        segment_index: Option<u32>,
        bytes: Vec<u8>,
        proof: Vec<u8>,
    }

    /// Run both verifiers, require the same outcome and the same rejection kind.
    fn compare_verifiers(
        pair: &Pair,
        commitment: &Hash,
        input: &Input,
        tally: &mut Tally,
    ) -> Result<(), Rejection> {
        let anchored = DataCommitment::from(*commitment);
        let (oracle, actual) = match input.segment_index {
            None => (
                reference::verify_chunk(
                    &pair.reference,
                    commitment,
                    input.data_size,
                    input.chunk_index,
                    &input.bytes,
                    &input.proof,
                ),
                irys_data_tree::verify_chunk(
                    &pair.crate_geometry,
                    &anchored,
                    input.data_size,
                    input.chunk_index,
                    &input.bytes,
                    &input.proof,
                ),
            ),
            Some(segment_index) => (
                reference::verify_segment(
                    &pair.reference,
                    commitment,
                    input.data_size,
                    input.chunk_index,
                    segment_index,
                    &input.bytes,
                    &input.proof,
                ),
                irys_data_tree::verify_segment(
                    &pair.crate_geometry,
                    &anchored,
                    input.data_size,
                    input.chunk_index,
                    segment_index,
                    &input.bytes,
                    &input.proof,
                ),
            ),
        };
        let actual = actual.map_err(rejection_of);
        assert_eq!(
            oracle,
            actual,
            "chunk_size {} segments_per_chunk {}: {} {}: n {} c {} j {:?}",
            pair.reference.chunk_size,
            pair.reference.segments_per_chunk,
            input.what,
            input.detail,
            input.data_size,
            input.chunk_index,
            input.segment_index
        );
        tally.record(oracle);
        oracle
    }

    fn flip_bit(bytes: &mut [u8], index: usize) {
        bytes[index] ^= FLIPPED_BIT;
    }

    /// Every mutation of one honest input, one change at a time.
    fn mutated_inputs(pair: &Pair, honest: &Input) -> Vec<Input> {
        let geometry = &pair.reference;
        let chunk_count = geometry.chunk_count(honest.data_size);
        let make = |what: &'static str, detail: u64| Input {
            what,
            detail,
            data_size: honest.data_size,
            chunk_index: honest.chunk_index,
            segment_index: honest.segment_index,
            bytes: honest.bytes.clone(),
            proof: honest.proof.clone(),
        };
        let mut inputs = Vec::new();

        // Flip one bit in each proof hash in turn (the last one is the size witness).
        let hash_count = honest.proof.len() / reference::HASH_SIZE;
        for hash_index in 0..hash_count {
            let mut input = make("flip a bit in proof hash", hash_index as u64);
            flip_bit(&mut input.proof, hash_index * reference::HASH_SIZE);
            inputs.push(input);
        }

        // Wrong data size: one with the same chunk count, others with another count.
        let data_size = honest.data_size;
        let in_range = |size: &u64| (1..=geometry.max_data_size()).contains(size);
        let same_count = [data_size + 1, data_size - 1]
            .into_iter()
            .filter(in_range)
            .find(|&size| geometry.chunk_count(size) == chunk_count);
        let other_count = [
            data_size.checked_add(geometry.chunk_size),
            data_size.checked_sub(geometry.chunk_size),
        ]
        .into_iter()
        .flatten()
        .filter(in_range);
        for wrong_size in same_count.into_iter().chain(other_count) {
            let mut input = make("wrong data_size", wrong_size);
            input.data_size = wrong_size;
            inputs.push(input);
        }

        // Wrong chunk index: one past the end, and the next chunk (wrapping).
        let mut wrong_chunk_indexes = vec![chunk_count];
        if chunk_count > 1 {
            wrong_chunk_indexes.push((honest.chunk_index + 1) % chunk_count);
        }
        for wrong_chunk in wrong_chunk_indexes {
            let mut input = make("wrong chunk_index", u64::from(wrong_chunk));
            input.chunk_index = wrong_chunk;
            inputs.push(input);
        }

        // Wrong segment index: one past the end, and the next segment (wrapping).
        if let Some(segment_index) = honest.segment_index {
            let mut wrong_segment_indexes = vec![geometry.segments_per_chunk];
            if geometry.segments_per_chunk > 1 {
                wrong_segment_indexes.push((segment_index + 1) % geometry.segments_per_chunk);
            }
            for wrong_segment in wrong_segment_indexes {
                let mut input = make("wrong segment_index", u64::from(wrong_segment));
                input.segment_index = Some(wrong_segment);
                inputs.push(input);
            }
        }

        // Proof one hash short (a sibling, or the witness if it is alone) and one long.
        let mut short = make("proof one hash short", 0);
        if hash_count >= 2 {
            let sibling_start = (hash_count - 2) * reference::HASH_SIZE;
            short
                .proof
                .drain(sibling_start..sibling_start + reference::HASH_SIZE);
        } else {
            short.proof.clear();
        }
        inputs.push(short);
        let mut long_before_witness = make("proof one hash long before the witness", 0);
        let witness_start = (hash_count - 1) * reference::HASH_SIZE;
        long_before_witness
            .proof
            .splice(witness_start..witness_start, [0; reference::HASH_SIZE]);
        inputs.push(long_before_witness);
        let mut long_at_end = make("proof one hash long at the end", 0);
        long_at_end
            .proof
            .extend_from_slice(&[0; reference::HASH_SIZE]);
        inputs.push(long_at_end);

        // Bytes one short, one long, and one changed byte (first and last).
        let mut short_bytes = make("bytes one short", 0);
        short_bytes.bytes.pop();
        inputs.push(short_bytes);
        let mut long_bytes = make("bytes one long", 0);
        long_bytes.bytes.push(0);
        inputs.push(long_bytes);
        if !honest.bytes.is_empty() {
            let mut changed_first = make("first byte changed", 0);
            flip_bit(&mut changed_first.bytes, 0);
            inputs.push(changed_first);
            let last_index = honest.bytes.len() - 1;
            let mut changed_last = make("last byte changed", last_index as u64);
            flip_bit(&mut changed_last.bytes, last_index);
            inputs.push(changed_last);
        }
        inputs
    }

    /// Segment indexes at the data edge (the spec §5 pin boundary), the middle and both ends.
    fn selected_segment_indexes(geometry: &Geometry, data_size: u64, chunk_index: u32) -> Vec<u32> {
        let data_segment_count = geometry
            .chunk_len(data_size, chunk_index)
            .div_ceil(geometry.segment_size());
        let data_segment_count = u32::try_from(data_segment_count).expect("segment count fits");
        let last = geometry.segments_per_chunk - 1;
        [
            0,
            data_segment_count.saturating_sub(1),
            data_segment_count,
            geometry.segments_per_chunk / 2,
            last,
        ]
        .into_iter()
        .filter(|&index| index <= last)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
    }

    /// Chunk indexes at both ends and the middle.
    fn selected_chunk_indexes(chunk_count: u32) -> Vec<u32> {
        [0, 1, chunk_count / 2, chunk_count - 1]
            .into_iter()
            .filter(|&index| index < chunk_count)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn compare_upload(pair: &Pair, plan: &Plan, data_size: u64, tally: &mut Tally) {
        let geometry = &pair.reference;
        let data = pattern_bytes(MOD251, 0, usize::try_from(data_size).expect("size fits"));
        let chunk_count = geometry.chunk_count(data_size);
        let chunk_slice = |chunk_index: u32| -> &[u8] {
            let start = usize::try_from(u64::from(chunk_index) * geometry.chunk_size).unwrap();
            let length = usize::try_from(geometry.chunk_len(data_size, chunk_index)).unwrap();
            &data[start..start + length]
        };

        let chunk_roots: Vec<Hash> = (0..chunk_count)
            .map(|chunk_index| reference::chunk_root(geometry, chunk_slice(chunk_index)))
            .collect();
        let chunk_leaves = reference::chunk_leaves(&chunk_roots);
        let root = reference::data_tree_root(&chunk_leaves);
        let witness = reference::size_witness(geometry, data_size);
        let commitment = reference::data_commitment(geometry, &root, data_size);

        let tree = DataTree::from_bytes(pair.crate_geometry, &data).expect("crate builds");
        assert_eq!(tree.chunk_count(), chunk_count, "n {data_size}");
        assert_eq!(
            tree.data_tree_root().as_bytes(),
            &root,
            "n {data_size} root"
        );
        assert_eq!(
            tree.size_witness().as_bytes(),
            &witness,
            "n {data_size} witness"
        );
        assert_eq!(
            tree.data_commitment().as_bytes(),
            &commitment,
            "n {data_size} commitment"
        );

        let mutate_this_size = plan.mutated_sizes.contains(&data_size);
        let mutated_chunks = selected_chunk_indexes(chunk_count);
        for chunk_index in 0..chunk_count {
            let chunk_bytes = chunk_slice(chunk_index);
            assert_eq!(
                tree.chunk_leaf(chunk_index).unwrap().as_bytes(),
                &chunk_leaves[chunk_index as usize],
                "n {data_size} c {chunk_index} chunk leaf"
            );
            assert_eq!(
                irys_data_tree::chunk_root(&pair.crate_geometry, chunk_bytes)
                    .unwrap()
                    .as_bytes(),
                &chunk_roots[chunk_index as usize],
                "n {data_size} c {chunk_index} chunk root"
            );
            assert_eq!(
                tree.chunk_key(chunk_index).unwrap().as_bytes(),
                &reference::chunk_key(&commitment, chunk_index),
                "n {data_size} c {chunk_index} chunk key"
            );
            assert_eq!(
                depth(chunk_index, chunk_count),
                Some(reference::depth(chunk_index, chunk_count)),
                "n {data_size} c {chunk_index} depth"
            );
            let chunk_proof =
                reference::chunk_proof(geometry, data_size, &chunk_leaves, chunk_index);
            assert_eq!(
                tree.chunk_proof(chunk_index).unwrap(),
                chunk_proof,
                "n {data_size} c {chunk_index} chunk proof"
            );
            let honest_chunk = Input {
                what: "honest chunk",
                detail: 0,
                data_size,
                chunk_index,
                segment_index: None,
                bytes: chunk_bytes.to_vec(),
                proof: chunk_proof,
            };
            assert_eq!(
                compare_verifiers(pair, &commitment, &honest_chunk, tally),
                Ok(()),
                "honest chunk n {data_size} c {chunk_index}"
            );
            let mutate_this_chunk = mutate_this_size && mutated_chunks.contains(&chunk_index);
            if mutate_this_chunk {
                for input in mutated_inputs(pair, &honest_chunk) {
                    let _ = compare_verifiers(pair, &commitment, &input, tally);
                }
            }

            let selected_segments = selected_segment_indexes(geometry, data_size, chunk_index);
            let segment_indexes: Vec<u32> = if plan.every_segment {
                (0..geometry.segments_per_chunk).collect()
            } else {
                selected_segments.clone()
            };
            for segment_index in segment_indexes {
                let segment_proof = reference::segment_proof(
                    geometry,
                    data_size,
                    &chunk_leaves,
                    chunk_index,
                    segment_index,
                    chunk_bytes,
                );
                assert_eq!(
                    tree.segment_proof(chunk_index, segment_index, chunk_bytes)
                        .unwrap(),
                    segment_proof,
                    "n {data_size} c {chunk_index} j {segment_index} segment proof"
                );
                let segment_start =
                    usize::try_from(u64::from(segment_index) * geometry.segment_size()).unwrap();
                let segment_length =
                    usize::try_from(geometry.segment_len(data_size, chunk_index, segment_index))
                        .unwrap();
                let honest_segment = Input {
                    what: "honest segment",
                    detail: 0,
                    data_size,
                    chunk_index,
                    segment_index: Some(segment_index),
                    bytes: chunk_bytes[segment_start.min(chunk_bytes.len())..][..segment_length]
                        .to_vec(),
                    proof: segment_proof,
                };
                assert_eq!(
                    compare_verifiers(pair, &commitment, &honest_segment, tally),
                    Ok(()),
                    "honest segment n {data_size} c {chunk_index} j {segment_index}"
                );
                if mutate_this_chunk && selected_segments.contains(&segment_index) {
                    for input in mutated_inputs(pair, &honest_segment) {
                        let _ = compare_verifiers(pair, &commitment, &input, tally);
                    }
                }
            }
        }
    }

    fn run(plan: &Plan) {
        let pair = Pair::new(plan.chunk_size, plan.segments_per_chunk);
        let mut tally = Tally::default();
        for &data_size in &plan.sizes {
            compare_upload(&pair, plan, data_size, &mut tally);
        }
        tally.assert_every_rejection_seen();
    }

    /// Every size up to `dense_chunk_limit` chunks, and the chunk boundaries up to the limit.
    fn dense_then_boundary_sizes(chunk_size: u64, dense_chunk_limit: u64) -> BTreeSet<u64> {
        let mut sizes: BTreeSet<u64> = (1..=dense_chunk_limit * chunk_size).collect();
        sizes.extend(chunk_boundary_sizes(chunk_size, SMALL_GEOMETRY_CHUNK_LIMIT));
        sizes
    }

    #[test]
    fn reference_matches_crate_at_testing_geometry() {
        run(&Plan {
            chunk_size: TESTING_CHUNK_SIZE,
            segments_per_chunk: TESTING_SEGMENTS_PER_CHUNK,
            sizes: all_sizes(TESTING_CHUNK_SIZE, SMALL_GEOMETRY_CHUNK_LIMIT),
            mutated_sizes: dense_then_boundary_sizes(TESTING_CHUNK_SIZE, 1),
            every_segment: true,
        });
    }

    #[test]
    fn reference_matches_crate_at_one_segment_per_chunk() {
        let (chunk_size, segments_per_chunk) = (8, 1);
        run(&Plan {
            chunk_size,
            segments_per_chunk,
            sizes: all_sizes(chunk_size, SMALL_GEOMETRY_CHUNK_LIMIT),
            mutated_sizes: dense_then_boundary_sizes(chunk_size, 1),
            every_segment: true,
        });
    }

    #[test]
    fn reference_matches_crate_at_two_segments_per_chunk() {
        let (chunk_size, segments_per_chunk) = (64, 2);
        run(&Plan {
            chunk_size,
            segments_per_chunk,
            sizes: dense_then_boundary_sizes(chunk_size, DENSE_CHUNK_LIMIT)
                .into_iter()
                .collect(),
            mutated_sizes: dense_then_boundary_sizes(chunk_size, 1),
            every_segment: true,
        });
    }

    #[test]
    fn reference_matches_crate_at_eight_segments_per_chunk() {
        // A chunk size that is not a power of two: 8 segments of 3 bytes.
        let (chunk_size, segments_per_chunk) = (24, 8);
        run(&Plan {
            chunk_size,
            segments_per_chunk,
            sizes: dense_then_boundary_sizes(chunk_size, DENSE_CHUNK_LIMIT)
                .into_iter()
                .collect(),
            mutated_sizes: dense_then_boundary_sizes(chunk_size, 1),
            every_segment: true,
        });
    }

    #[test]
    fn reference_matches_crate_at_the_maximum_segments_per_chunk() {
        // 256 segments of 1 byte. Segment edges near both ends and the middle of the
        // first two chunks, and every chunk boundary up to 40 chunks.
        let (chunk_size, segments_per_chunk) = (256, 256);
        let mut sizes = chunk_boundary_sizes(chunk_size, MAX_SEGMENT_GEOMETRY_CHUNK_LIMIT);
        sizes.extend(segment_edge_sizes(
            chunk_size,
            segments_per_chunk,
            &[1, 2, 127, 128, 129, 254, 255],
        ));
        run(&Plan {
            chunk_size,
            segments_per_chunk,
            mutated_sizes: sizes.iter().copied().step_by(5).collect(),
            sizes: sizes.into_iter().collect(),
            every_segment: false,
        });
    }

    #[test]
    fn reference_matches_crate_at_mainnet_geometry() {
        // 1 byte; one segment; one full chunk; 3.5 chunks (spec §4); and 3 full
        // chunks plus a short last chunk that holds one segment and one byte.
        let sizes = vec![
            1,
            4_096,
            MAINNET_CHUNK_SIZE,
            917_504,
            3 * MAINNET_CHUNK_SIZE + 4_097,
        ];
        run(&Plan {
            chunk_size: MAINNET_CHUNK_SIZE,
            segments_per_chunk: MAINNET_SEGMENTS_PER_CHUNK,
            mutated_sizes: sizes.iter().copied().collect(),
            sizes,
            every_segment: false,
        });
        let pair = Pair::new(MAINNET_CHUNK_SIZE, MAINNET_SEGMENTS_PER_CHUNK);
        assert_eq!(pair.crate_geometry, CrateGeometry::MAINNET);
    }

    #[test]
    fn reference_matches_crate_on_empty_subtrees_and_depth() {
        for height in 0..=MAX_SUBTREE_HEIGHT {
            let crate_empty = empty(SubtreeHeight::new(height).expect("height in bound"));
            assert_eq!(
                crate_empty.as_bytes(),
                &reference::empty(height),
                "empty({height})"
            );
        }
        assert_eq!(
            CrateGeometry::TESTING,
            CrateGeometry::new(
                TESTING_CHUNK_SIZE,
                TESTING_SEGMENTS_PER_CHUNK,
                MAX_DATA_TX_CHUNKS
            )
            .unwrap()
        );
        for chunk_count in 1..=DEPTH_CHUNK_COUNT_LIMIT {
            for chunk_index in 0..chunk_count {
                assert_eq!(
                    depth(chunk_index, chunk_count),
                    Some(reference::depth(chunk_index, chunk_count)),
                    "depth({chunk_index}, {chunk_count})"
                );
            }
        }
    }
}
