//! Edge-biased proptest inputs for the whole geometry and integer space.
//!
//! `ProptestConfig::with_cases` writes `cases` after `ProptestConfig::default`
//! has already applied `PROPTEST_CASES` (proptest reads that variable only
//! while building the process-wide default). [`case_config`] keeps the variable
//! when it is set, so a soak raises every property test together:
//!
//! ```text
//! cat > /tmp/data-tree-soak-nextest.toml <<'EOF'
//! [profile.default]
//! slow-timeout = { period = "20m", terminate-after = 2 }
//! EOF
//! PROPTEST_CASES=330000 XTASK_NO_PRUNE=1 cargo nextest run -p irys-data-tree \
//!   --config-file /tmp/data-tree-soak-nextest.toml \
//!   -E 'test(/::panic_freedom::/) | test(/::differential::/) | test(/::builder_props::/) | test(/::geometry_props::/) | test(/::empty_siblings::/) | test(/::props::/)'
//! ```
//!
//! That filter is every property module: `panic_freedom`, `differential`,
//! `builder_props`, `geometry_props`, `empty_siblings`, and `props`. `props`
//! uses bare `proptest!`, so `PROPTEST_CASES` still scales it through the
//! process default. A new property module has to be added to the filter.
//!
//! The repo's default nextest profile kills a test after two 30 s periods.
//! The temporary config raises that for the soak only. Nextest runs those
//! tests in parallel, so the wall clock is the slowest one. The honest-tree
//! test in `differential` is about 4 seconds at 2000 cases on an opt-level 3
//! dev build, so 330000 cases is about ten minutes. An unset variable uses the
//! per-test count passed to [`case_config`]. A value proptest cannot parse as
//! `u32` leaves the built-in default of 256.
//!
//! Failure persistence stays on. Both branches start from `ProptestConfig::default`,
//! which records failures under `crates/data-tree/proptest-regressions/`
//! (proptest's `SourceParallel("proptest-regressions")`, next to this crate's
//! `src/lib.rs`). Check in a file a real failure produces.
//!
//! The harness functions in the other test modules take plain values and
//! `assert`. A later coverage-guided fuzzer can call them without proptest.

use proptest::prelude::*;

use crate::geometry::{Geometry, MAX_SEGMENTS_PER_CHUNK};
use crate::shape::depth;
use crate::types::{DataCommitment, DataTreeRoot, HASH_SIZE};

/// How often to draw `0`, `1`, `2`, `MAX - 1`, or `MAX` against the other arms.
const EXTREME_VALUE_WEIGHT: u32 = 4;

/// How often to draw `2^k - 1`, `2^k`, and `2^k + 1`.
const POWER_OF_TWO_WEIGHT: u32 = 8;

/// How often to draw a uniform value of the whole integer type.
const UNIFORM_VALUE_WEIGHT: u32 = 4;

/// How often to draw [`Geometry::TESTING`] or [`Geometry::MAINNET`].
const PRESET_GEOMETRY_WEIGHT: u32 = 4;

/// How often to draw one edge geometry (one segment, max segments, one-byte
/// segments, one chunk, or the largest product that fits `u64`).
const EDGE_GEOMETRY_WEIGHT: u32 = 3;

/// How often to draw a geometry whose parameters are uniform inside the rules.
const UNIFORM_GEOMETRY_WEIGHT: u32 = 4;

/// How often a length is one of the listed edges rather than uniform in range.
const BIASED_LENGTH_WEIGHT: u32 = 5;

/// Longest `bytes` or `proof` slice the untrusted-input properties allocate.
///
/// Verifiers compare the length before they hash (spec §5, §10). A length above
/// this cap is omitted from the edge list, and the case still passes a short
/// slice, so the length check runs. When the interesting length fits, it is a
/// candidate and the hasher runs.
const UNTRUSTED_INPUT_BYTE_CAP: usize = 4 * 1024;

/// Chunk cap for one honest upload. Past this, the sum check samples instead
/// of walking every chunk.
pub(super) const HONEST_TREE_MAX_CHUNKS: u32 = 64;

/// Byte cap for one honest upload. Hashing stays on a few kilobytes per case
/// (spec §9: the bytes dominate the time).
pub(super) const HONEST_TREE_MAX_BYTES: u64 = 16 * 1024;

/// Case count for one property.
///
/// See the module comment for the soak command. `PROPTEST_CASES` wins when set.
pub(super) fn case_config(default_cases: u32) -> ProptestConfig {
    if std::env::var_os("PROPTEST_CASES").is_some() {
        ProptestConfig::default()
    } else {
        ProptestConfig::with_cases(default_cases)
    }
}

/// `0`, `1`, `2`, `u64::MAX - 1`, `u64::MAX`, every power of two and the values
/// one away from it, plus a uniform `u64`.
pub(super) fn edge_biased_u64() -> impl Strategy<Value = u64> {
    prop_oneof![
        EXTREME_VALUE_WEIGHT => Just(0_u64),
        EXTREME_VALUE_WEIGHT => Just(1_u64),
        EXTREME_VALUE_WEIGHT => Just(2_u64),
        EXTREME_VALUE_WEIGHT => Just(u64::MAX - 1),
        EXTREME_VALUE_WEIGHT => Just(u64::MAX),
        POWER_OF_TWO_WEIGHT => power_of_two_neighborhood_u64(),
        UNIFORM_VALUE_WEIGHT => any::<u64>(),
    ]
}

/// `0`, `1`, `2`, `u32::MAX - 1`, `u32::MAX`, every power of two and the values
/// one away from it, plus a uniform `u32`.
pub(super) fn edge_biased_u32() -> impl Strategy<Value = u32> {
    prop_oneof![
        EXTREME_VALUE_WEIGHT => Just(0_u32),
        EXTREME_VALUE_WEIGHT => Just(1_u32),
        EXTREME_VALUE_WEIGHT => Just(2_u32),
        EXTREME_VALUE_WEIGHT => Just(u32::MAX - 1),
        EXTREME_VALUE_WEIGHT => Just(u32::MAX),
        POWER_OF_TWO_WEIGHT => power_of_two_neighborhood_u32(),
        UNIFORM_VALUE_WEIGHT => any::<u32>(),
    ]
}

fn power_of_two_neighborhood_u64() -> impl Strategy<Value = u64> {
    // Shifts `0..64`. `2^63 + 1` still fits in `u64`.
    (0..u64::BITS).prop_flat_map(|shift| {
        let power = 1_u64 << shift;
        let below = power - 1;
        let above = power + 1;
        prop_oneof![Just(below), Just(power), Just(above)]
    })
}

fn power_of_two_neighborhood_u32() -> impl Strategy<Value = u32> {
    // Shifts `0..32`. `2^31 + 1` still fits in `u32`.
    (0..u32::BITS).prop_flat_map(|shift| {
        let power = 1_u32 << shift;
        let below = power - 1;
        let above = power + 1;
        prop_oneof![Just(below), Just(power), Just(above)]
    })
}

/// Any geometry [`Geometry::new`] accepts, weighted toward the spec edges.
pub(super) fn any_geometry() -> impl Strategy<Value = Geometry> {
    prop_oneof![
        PRESET_GEOMETRY_WEIGHT => Just(Geometry::TESTING),
        PRESET_GEOMETRY_WEIGHT => Just(Geometry::MAINNET),
        EDGE_GEOMETRY_WEIGHT => one_segment_geometry(),
        EDGE_GEOMETRY_WEIGHT => max_segments_geometry(),
        EDGE_GEOMETRY_WEIGHT => one_byte_segment_geometry(),
        EDGE_GEOMETRY_WEIGHT => one_chunk_geometry(),
        EDGE_GEOMETRY_WEIGHT => largest_product_geometry(),
        UNIFORM_GEOMETRY_WEIGHT => uniform_geometry(),
    ]
}

/// Parameters [`Geometry::new`] must reject (spec §2). Each arm breaks one rule.
#[derive(Debug, Clone)]
pub(super) struct RejectedGeometry {
    /// Candidate `chunk_size`. May be 0 or not a multiple of the segment count.
    pub chunk_size: u64,
    /// Candidate `segments_per_chunk`. May be outside the power-of-two range.
    pub segments_per_chunk: u32,
    /// Candidate `max_data_tx_chunks`. May be 0, or too large for `chunk_size`.
    pub max_data_tx_chunks: u32,
}

pub(super) fn rejected_geometry() -> impl Strategy<Value = RejectedGeometry> {
    prop_oneof![
        3 => bad_segments_per_chunk(),
        2 => zero_chunk_size(),
        3 => chunk_size_not_multiple(),
        2 => zero_max_chunks(),
        3 => product_overflows(),
    ]
}

/// `data_size` weighted toward 0, the type extremes, and this geometry's
/// chunk-count boundaries (spec §2).
pub(super) fn edge_biased_data_size(geometry: Geometry) -> impl Strategy<Value = u64> {
    let boundaries = data_size_boundaries(geometry);
    let boundary_count = boundaries.len();
    prop_oneof![
        POWER_OF_TWO_WEIGHT => (0..boundary_count).prop_map(move |index| boundaries[index]),
        UNIFORM_VALUE_WEIGHT => edge_biased_u64(),
    ]
}

/// Chunk index weighted toward 0 and the chunk-count boundary when `data_size`
/// is in range, otherwise the plain `u32` edges.
pub(super) fn edge_biased_chunk_index(geometry: Geometry, data_size: u64) -> BoxedStrategy<u32> {
    match geometry.chunk_count(data_size) {
        Ok(chunk_count) => edge_biased_around(chunk_count).boxed(),
        Err(_) => edge_biased_u32().boxed(),
    }
}

/// Segment index weighted toward 0 and `segments_per_chunk` (the first index
/// the geometry rejects, spec §2).
pub(super) fn edge_biased_segment_index(segments_per_chunk: u32) -> impl Strategy<Value = u32> {
    edge_biased_around(segments_per_chunk)
}

/// `(chunk_index, chunk_count)` with `chunk_count >= 1` and
/// `chunk_index < chunk_count`. That is `depth`'s precondition (spec §5).
pub(super) fn chunk_index_below_count() -> impl Strategy<Value = (u32, u32)> {
    // `edge_biased_u32` draws 0 often. Map it to 1 so `C = 1` (a proof that is
    // only the size witness, spec §5) stays an edge instead of being dropped.
    edge_biased_u32()
        .prop_map(|chunk_count| if chunk_count == 0 { 1 } else { chunk_count })
        .prop_flat_map(|chunk_count| {
            let last = chunk_count - 1;
            prop_oneof![
                EXTREME_VALUE_WEIGHT => Just(0_u32),
                EXTREME_VALUE_WEIGHT => Just(last),
                UNIFORM_VALUE_WEIGHT => 0..chunk_count,
            ]
            .prop_map(move |chunk_index| (chunk_index, chunk_count))
        })
}

/// Untrusted verifier input. Lengths prefer the spec boundaries that fit
/// [`UNTRUSTED_INPUT_BYTE_CAP`].
#[derive(Debug, Clone)]
pub(super) struct UntrustedVerifierInput {
    /// Geometry under test.
    pub geometry: Geometry,
    /// Not derived from a tree. `Ok` from a verifier would be a preimage.
    pub commitment: DataCommitment,
    /// Root passed to `verify_size`. Independent of `commitment`.
    pub data_tree_root: DataTreeRoot,
    /// `n` in spec §2.
    pub data_size: u64,
    /// `c` in spec §2.
    pub chunk_index: u32,
    /// `j` in spec §2.
    pub segment_index: u32,
    /// Candidate chunk or segment bytes.
    pub bytes: Vec<u8>,
    /// Candidate proof bytes.
    pub proof: Vec<u8>,
}

pub(super) fn untrusted_verifier_input() -> impl Strategy<Value = UntrustedVerifierInput> {
    any_geometry().prop_flat_map(|geometry| {
        edge_biased_data_size(geometry)
            .prop_flat_map(move |data_size| {
                (
                    edge_biased_chunk_index(geometry, data_size),
                    edge_biased_segment_index(geometry.segments_per_chunk()),
                    any::<[u8; HASH_SIZE]>(),
                    any::<[u8; HASH_SIZE]>(),
                )
                    .prop_flat_map(
                        move |(chunk_index, segment_index, commitment, root)| {
                            let byte_lengths = byte_length_choices(
                                geometry,
                                data_size,
                                chunk_index,
                                segment_index,
                            );
                            let proof_lengths =
                                proof_length_choices(geometry, data_size, chunk_index);
                            (biased_length(byte_lengths), biased_length(proof_lengths))
                                .prop_flat_map(|(byte_length, proof_length)| {
                                    (
                                        prop::collection::vec(any::<u8>(), byte_length),
                                        prop::collection::vec(any::<u8>(), proof_length),
                                    )
                                })
                                .prop_map(move |(bytes, proof)| UntrustedVerifierInput {
                                    geometry,
                                    commitment: DataCommitment::from(commitment),
                                    data_tree_root: DataTreeRoot::from(root),
                                    data_size,
                                    chunk_index,
                                    segment_index,
                                    bytes,
                                    proof,
                                })
                        },
                    )
            })
            .boxed()
    })
}

/// Small geometry and a non-empty upload of at most [`HONEST_TREE_MAX_BYTES`].
#[derive(Debug, Clone)]
pub(super) struct HonestUpload {
    /// Geometry. `max_data_tx_chunks` is [`HONEST_TREE_MAX_CHUNKS`].
    pub geometry: Geometry,
    /// Upload bytes. Length is at least 1 and at most the byte cap.
    pub bytes: Vec<u8>,
    /// Picks indexes inside the finished tree.
    pub probe: u64,
}

pub(super) fn honest_upload() -> impl Strategy<Value = HonestUpload> {
    small_geometry().prop_flat_map(|geometry| {
        let max_bytes = honest_byte_limit(geometry);
        let chunk_size =
            usize::try_from(geometry.chunk_size()).expect("small chunk size fits usize");
        (honest_bytes(max_bytes, chunk_size), any::<u64>()).prop_map(move |(bytes, probe)| {
            HonestUpload {
                geometry,
                bytes,
                probe,
            }
        })
    })
}

/// Push length for a builder: `0`, `1`, `chunk_size - 1`, `chunk_size`,
/// `chunk_size + 1`, plus two chunks. Two chunks is where a parallel batch
/// splits (spec §4). `chunk_size` fits `usize` for builder geometries.
pub(super) fn edge_biased_push_length(chunk_size: u64) -> impl Strategy<Value = usize> {
    let chunk_size = usize::try_from(chunk_size).expect("builder chunk_size fits usize");
    let two_chunks = chunk_size.saturating_mul(2);
    let mut edges = vec![
        0,
        1,
        chunk_size.saturating_sub(1),
        chunk_size,
        chunk_size.saturating_add(1),
        two_chunks,
        two_chunks.saturating_add(1),
    ];
    edges.retain(|length| *length <= two_chunks.saturating_add(1));
    let longest = two_chunks.saturating_add(1);
    biased_length_up_to(edges, longest)
}

fn edge_biased_around(boundary: u32) -> impl Strategy<Value = u32> {
    let below = boundary.saturating_sub(1);
    let above = boundary.saturating_add(1);
    prop_oneof![
        EXTREME_VALUE_WEIGHT => Just(0_u32),
        EXTREME_VALUE_WEIGHT => Just(below),
        EXTREME_VALUE_WEIGHT => Just(boundary),
        EXTREME_VALUE_WEIGHT => Just(above),
        POWER_OF_TWO_WEIGHT => edge_biased_u32(),
    ]
}

fn any_segments_per_chunk() -> impl Strategy<Value = u32> {
    let powers: Vec<u32> = (0..=MAX_SEGMENTS_PER_CHUNK.trailing_zeros())
        .map(|shift| 1_u32 << shift)
        .collect();
    let power_count = powers.len();
    prop_oneof![
        EDGE_GEOMETRY_WEIGHT => Just(1_u32),
        PRESET_GEOMETRY_WEIGHT => Just(Geometry::TESTING.segments_per_chunk()),
        PRESET_GEOMETRY_WEIGHT => Just(Geometry::MAINNET.segments_per_chunk()),
        EDGE_GEOMETRY_WEIGHT => Just(MAX_SEGMENTS_PER_CHUNK),
        UNIFORM_GEOMETRY_WEIGHT => (0..power_count).prop_map(move |index| powers[index]),
    ]
}

fn accepted_max_chunks() -> impl Strategy<Value = u32> {
    let mainnet_max = Geometry::MAINNET.max_data_tx_chunks();
    prop_oneof![
        EDGE_GEOMETRY_WEIGHT => Just(1_u32),
        PRESET_GEOMETRY_WEIGHT => Just(mainnet_max),
        EDGE_GEOMETRY_WEIGHT => Just(u32::MAX),
        UNIFORM_GEOMETRY_WEIGHT => power_of_two_neighborhood_u32()
            .prop_map(|value| if value == 0 { 1 } else { value }),
        UNIFORM_VALUE_WEIGHT => 1_u32..=u32::MAX,
    ]
}

fn one_segment_geometry() -> impl Strategy<Value = Geometry> {
    // `segments_per_chunk = 1`. Height is `log2(1) = 0` (spec §2).
    const ONE_SEGMENT: u32 = 1;
    accepted_max_chunks().prop_flat_map(move |max_data_tx_chunks| {
        chunk_size_choices(ONE_SEGMENT, max_data_tx_chunks).prop_map(move |chunk_size| {
            Geometry::new(chunk_size, ONE_SEGMENT, max_data_tx_chunks)
                .expect("one segment and a fitting chunk size")
        })
    })
}

fn max_segments_geometry() -> impl Strategy<Value = Geometry> {
    accepted_max_chunks().prop_flat_map(|max_data_tx_chunks| {
        chunk_size_choices(MAX_SEGMENTS_PER_CHUNK, max_data_tx_chunks).prop_map(move |chunk_size| {
            Geometry::new(chunk_size, MAX_SEGMENTS_PER_CHUNK, max_data_tx_chunks)
                .expect("max segments and a fitting chunk size")
        })
    })
}

fn one_byte_segment_geometry() -> impl Strategy<Value = Geometry> {
    // `chunk_size == segments_per_chunk`, so each segment is one byte (spec §2).
    (any_segments_per_chunk(), accepted_max_chunks()).prop_map(
        |(segments_per_chunk, max_data_tx_chunks)| {
            let chunk_size = u64::from(segments_per_chunk);
            Geometry::new(chunk_size, segments_per_chunk, max_data_tx_chunks)
                .expect("one-byte segments fit: segments_per_chunk <= 256 and the product fits u64")
        },
    )
}

fn one_chunk_geometry() -> impl Strategy<Value = Geometry> {
    const ONE_CHUNK: u32 = 1;
    any_segments_per_chunk().prop_flat_map(|segments_per_chunk| {
        chunk_size_choices(segments_per_chunk, ONE_CHUNK).prop_map(move |chunk_size| {
            Geometry::new(chunk_size, segments_per_chunk, ONE_CHUNK)
                .expect("one chunk of a multiple of segments_per_chunk fits u64")
        })
    })
}

fn largest_product_geometry() -> impl Strategy<Value = Geometry> {
    // `chunk_size` is the greatest multiple of `segments_per_chunk` whose
    // product with `max_data_tx_chunks` still fits `u64` (spec §2).
    (any_segments_per_chunk(), accepted_max_chunks()).prop_map(
        |(segments_per_chunk, max_data_tx_chunks)| {
            let chunk_size = largest_chunk_size(segments_per_chunk, max_data_tx_chunks);
            Geometry::new(chunk_size, segments_per_chunk, max_data_tx_chunks)
                .expect("largest fitting chunk size is accepted")
        },
    )
}

fn uniform_geometry() -> impl Strategy<Value = Geometry> {
    (any_segments_per_chunk(), accepted_max_chunks()).prop_flat_map(
        |(segments_per_chunk, max_data_tx_chunks)| {
            let segment_count = u64::from(segments_per_chunk);
            let largest = largest_chunk_size(segments_per_chunk, max_data_tx_chunks);
            let max_factor = largest / segment_count;
            (1_u64..=max_factor).prop_map(move |factor| {
                Geometry::new(
                    factor * segment_count,
                    segments_per_chunk,
                    max_data_tx_chunks,
                )
                .expect("factor is at most the largest that fits")
            })
        },
    )
}

fn chunk_size_choices(
    segments_per_chunk: u32,
    max_data_tx_chunks: u32,
) -> impl Strategy<Value = u64> {
    let segment_count = u64::from(segments_per_chunk);
    let largest = largest_chunk_size(segments_per_chunk, max_data_tx_chunks);
    let max_factor = largest / segment_count;
    prop_oneof![
        EDGE_GEOMETRY_WEIGHT => Just(segment_count),
        EDGE_GEOMETRY_WEIGHT => Just(largest),
        UNIFORM_GEOMETRY_WEIGHT => {
            (1_u64..=max_factor).prop_map(move |factor| factor * segment_count)
        },
    ]
}

/// Greatest `chunk_size` that is a multiple of `segments_per_chunk` and whose
/// product with `max_data_tx_chunks` fits `u64`.
///
/// Every power-of-two segment count in `1..=MAX_SEGMENTS_PER_CHUNK` has one:
/// `u64::MAX / u32::MAX` is greater than 256.
fn largest_chunk_size(segments_per_chunk: u32, max_data_tx_chunks: u32) -> u64 {
    let segment_count = u64::from(segments_per_chunk);
    let max_chunks = u64::from(max_data_tx_chunks);
    let largest_that_fits = u64::MAX / max_chunks;
    let chunk_size = largest_that_fits - (largest_that_fits % segment_count);
    debug_assert!(chunk_size >= segment_count);
    debug_assert!(chunk_size.checked_mul(max_chunks).is_some());
    chunk_size
}

fn bad_segments_per_chunk() -> impl Strategy<Value = RejectedGeometry> {
    (
        rejected_segments_per_chunk(),
        edge_biased_u64(),
        edge_biased_u32(),
    )
        .prop_map(
            |(segments_per_chunk, chunk_size, max_data_tx_chunks)| RejectedGeometry {
                chunk_size,
                segments_per_chunk,
                max_data_tx_chunks,
            },
        )
}

fn rejected_segments_per_chunk() -> impl Strategy<Value = u32> {
    // Inside `1..=MAX`, the values that are not powers of two. The range is
    // 256 wide, so the filter rejects only the nine legal powers.
    let in_range_not_power = (1_u32..=MAX_SEGMENTS_PER_CHUNK)
        .prop_filter("power of two", |value| !value.is_power_of_two());
    prop_oneof![
        EXTREME_VALUE_WEIGHT => Just(0_u32),
        EXTREME_VALUE_WEIGHT => Just(MAX_SEGMENTS_PER_CHUNK + 1),
        EXTREME_VALUE_WEIGHT => Just(u32::MAX),
        POWER_OF_TWO_WEIGHT => in_range_not_power,
        UNIFORM_VALUE_WEIGHT => (MAX_SEGMENTS_PER_CHUNK + 1)..=u32::MAX,
    ]
}

fn zero_chunk_size() -> impl Strategy<Value = RejectedGeometry> {
    (any_segments_per_chunk(), edge_biased_u32()).prop_map(
        |(segments_per_chunk, max_data_tx_chunks)| RejectedGeometry {
            chunk_size: 0,
            segments_per_chunk,
            max_data_tx_chunks,
        },
    )
}

fn chunk_size_not_multiple() -> impl Strategy<Value = RejectedGeometry> {
    (
        segments_per_chunk_at_least_two(),
        edge_biased_u32(),
        edge_biased_u64(),
    )
        .prop_map(|(segments_per_chunk, max_data_tx_chunks, quotient)| {
            let segment_count = u64::from(segments_per_chunk);
            // Remainder is in `1..segment_count`, so `chunk_size` is not a multiple.
            let remainder = 1 + (quotient % (segment_count - 1));
            let chunk_size = match quotient.checked_mul(segment_count) {
                Some(base) => base.checked_add(remainder).unwrap_or(remainder),
                None => remainder,
            };
            debug_assert_ne!(chunk_size % segment_count, 0);
            debug_assert_ne!(chunk_size, 0);
            RejectedGeometry {
                chunk_size,
                segments_per_chunk,
                max_data_tx_chunks,
            }
        })
}

fn segments_per_chunk_at_least_two() -> impl Strategy<Value = u32> {
    // Shifts `1..=log2(MAX)` are `2, 4, ..., MAX_SEGMENTS_PER_CHUNK`.
    let max_shift = MAX_SEGMENTS_PER_CHUNK.trailing_zeros();
    (1..=max_shift).prop_map(|shift| 1_u32 << shift)
}

fn zero_max_chunks() -> impl Strategy<Value = RejectedGeometry> {
    (any_segments_per_chunk(), edge_biased_u64()).prop_map(|(segments_per_chunk, factor)| {
        let segment_count = u64::from(segments_per_chunk);
        let max_factor = u64::MAX / segment_count;
        let factor = factor.clamp(1, max_factor);
        RejectedGeometry {
            chunk_size: factor * segment_count,
            segments_per_chunk,
            max_data_tx_chunks: 0,
        }
    })
}

fn product_overflows() -> impl Strategy<Value = RejectedGeometry> {
    (
        any_segments_per_chunk(),
        max_chunks_at_least_two(),
        edge_biased_u64(),
    )
        .prop_map(|(segments_per_chunk, max_data_tx_chunks, seed)| {
            let chunk_size = overflowing_chunk_size(segments_per_chunk, max_data_tx_chunks, seed);
            RejectedGeometry {
                chunk_size,
                segments_per_chunk,
                max_data_tx_chunks,
            }
        })
}

fn max_chunks_at_least_two() -> impl Strategy<Value = u32> {
    let mainnet_max = Geometry::MAINNET.max_data_tx_chunks();
    prop_oneof![
        EDGE_GEOMETRY_WEIGHT => Just(2_u32),
        PRESET_GEOMETRY_WEIGHT => Just(mainnet_max),
        EDGE_GEOMETRY_WEIGHT => Just(u32::MAX),
        UNIFORM_VALUE_WEIGHT => 2_u32..=u32::MAX,
    ]
}

/// A positive multiple of `segments_per_chunk` whose product with
/// `max_data_tx_chunks` does not fit `u64`. `max_data_tx_chunks >= 2`.
fn overflowing_chunk_size(segments_per_chunk: u32, max_data_tx_chunks: u32, seed: u64) -> u64 {
    let segment_count = u64::from(segments_per_chunk);
    let max_chunks = u64::from(max_data_tx_chunks);
    let largest_that_fits = u64::MAX / max_chunks;
    // Smallest factor whose multiple is strictly above `largest_that_fits`.
    let first_factor = largest_that_fits / segment_count + 1;
    let max_factor = u64::MAX / segment_count;
    debug_assert!(first_factor <= max_factor);
    let span = max_factor - first_factor;
    let factor = first_factor + if span == 0 { 0 } else { seed % (span + 1) };
    let chunk_size = factor * segment_count;
    debug_assert!(chunk_size > largest_that_fits);
    debug_assert!(chunk_size.checked_mul(max_chunks).is_none());
    chunk_size
}

fn data_size_boundaries(geometry: Geometry) -> Vec<u64> {
    let chunk_size = geometry.chunk_size();
    let max_data_size = geometry.max_data_size();
    let mut boundaries = vec![0, 1, chunk_size, max_data_size];
    if chunk_size > 1 {
        boundaries.push(chunk_size - 1);
    }
    if let Some(above_chunk) = chunk_size.checked_add(1) {
        boundaries.push(above_chunk);
    }
    if max_data_size > 1 {
        boundaries.push(max_data_size - 1);
    }
    if let Some(above_max) = max_data_size.checked_add(1) {
        boundaries.push(above_max);
    }
    if let Some(two_chunks) = chunk_size.checked_mul(2) {
        boundaries.push(two_chunks.saturating_sub(1));
        boundaries.push(two_chunks);
    }
    boundaries
}

fn byte_length_choices(
    geometry: Geometry,
    data_size: u64,
    chunk_index: u32,
    segment_index: u32,
) -> Vec<usize> {
    let mut lengths = vec![0, 1];
    push_length_near(&mut lengths, geometry.chunk_size());
    if let Ok(chunk_length) = geometry.chunk_len(data_size, chunk_index) {
        push_length_near(&mut lengths, chunk_length);
    }
    if let Ok(segment_length) = geometry.segment_len(data_size, chunk_index, segment_index) {
        push_length_near(&mut lengths, segment_length);
    }
    lengths.retain(|length| *length <= UNTRUSTED_INPUT_BYTE_CAP);
    if lengths.is_empty() {
        lengths.push(0);
    }
    lengths
}

fn proof_length_choices(geometry: Geometry, data_size: u64, chunk_index: u32) -> Vec<usize> {
    // One byte off a single hash, and the hash width itself (spec §5: `32 × k`).
    let mut lengths = vec![0, HASH_SIZE - 1, HASH_SIZE, HASH_SIZE + 1];
    // `depth` is `None` when `chunk_index >= chunk_count` (spec §5).
    if let Ok(chunk_count) = geometry.chunk_count(data_size)
        && chunk_index < chunk_count
    {
        let chunk_depth =
            usize::try_from(depth(chunk_index, chunk_count).expect("chunk_index < chunk_count"))
                .expect("depth fits usize");
        // The proof is the siblings plus the size witness (spec §5).
        push_hash_count_near(&mut lengths, chunk_depth + 1);
        let segment_hashes =
            usize::try_from(geometry.height()).expect("height fits usize") + chunk_depth + 1;
        push_hash_count_near(&mut lengths, segment_hashes);
    }
    lengths.retain(|length| *length <= UNTRUSTED_INPUT_BYTE_CAP);
    lengths
}

fn push_length_near(lengths: &mut Vec<usize>, length: u64) {
    for candidate in [length.saturating_sub(1), length, length.saturating_add(1)] {
        if let Ok(candidate) = usize::try_from(candidate) {
            lengths.push(candidate);
        }
    }
}

fn push_hash_count_near(lengths: &mut Vec<usize>, hash_count: usize) {
    // `HASH_SIZE × (k - 1)`, `HASH_SIZE × k`, and `HASH_SIZE × (k + 1)`.
    for count in [
        hash_count.saturating_sub(1),
        hash_count,
        hash_count.saturating_add(1),
    ] {
        if let Some(length) = count.checked_mul(HASH_SIZE) {
            lengths.push(length);
        }
    }
}

fn biased_length(lengths: Vec<usize>) -> impl Strategy<Value = usize> {
    biased_length_up_to(lengths, UNTRUSTED_INPUT_BYTE_CAP)
}

fn biased_length_up_to(lengths: Vec<usize>, uniform_max: usize) -> impl Strategy<Value = usize> {
    let lengths = if lengths.is_empty() { vec![0] } else { lengths };
    let last = lengths.len() - 1;
    prop_oneof![
        BIASED_LENGTH_WEIGHT => (0..=last).prop_map(move |index| lengths[index]),
        UNIFORM_VALUE_WEIGHT => 0..=uniform_max,
    ]
}

fn small_geometry() -> impl Strategy<Value = Geometry> {
    let testing_shape = Geometry::new(
        Geometry::TESTING.chunk_size(),
        Geometry::TESTING.segments_per_chunk(),
        HONEST_TREE_MAX_CHUNKS,
    )
    .expect("testing chunk size with the honest chunk cap");
    prop_oneof![
        PRESET_GEOMETRY_WEIGHT => Just(testing_shape),
        UNIFORM_GEOMETRY_WEIGHT => varied_small_geometry(),
    ]
}

fn varied_small_geometry() -> impl Strategy<Value = Geometry> {
    any_segments_per_chunk().prop_flat_map(|segments_per_chunk| {
        let segment_count = u64::from(segments_per_chunk);
        let max_segment_size = HONEST_TREE_MAX_BYTES / segment_count;
        debug_assert!(max_segment_size >= 1);
        prop_oneof![
            EDGE_GEOMETRY_WEIGHT => Just(1_u64),
            EDGE_GEOMETRY_WEIGHT => Just(max_segment_size),
            UNIFORM_GEOMETRY_WEIGHT => 1_u64..=max_segment_size,
        ]
        .prop_map(move |segment_size| {
            Geometry::new(
                segment_count * segment_size,
                segments_per_chunk,
                HONEST_TREE_MAX_CHUNKS,
            )
            .expect("small geometry fits the byte cap and u64")
        })
    })
}

fn honest_byte_limit(geometry: Geometry) -> usize {
    let by_chunks = geometry
        .chunk_size()
        .saturating_mul(u64::from(HONEST_TREE_MAX_CHUNKS));
    let capped = by_chunks
        .min(HONEST_TREE_MAX_BYTES)
        .min(geometry.max_data_size());
    usize::try_from(capped.max(1)).expect("honest uploads fit usize")
}

fn honest_bytes(max_bytes: usize, chunk_size: usize) -> BoxedStrategy<Vec<u8>> {
    let lengths = honest_lengths(max_bytes, chunk_size);
    prop_oneof![
        4 => owned_bytes(lengths.clone(), max_bytes, None),
        2 => any::<u8>()
            .prop_flat_map(move |byte| owned_bytes(lengths.clone(), max_bytes, Some(byte))),
        2 => repeated_full_chunks(max_bytes, chunk_size),
    ]
    .boxed()
}

fn honest_lengths(max_bytes: usize, chunk_size: usize) -> Vec<usize> {
    let mut lengths = vec![1];
    for candidate in [
        chunk_size.saturating_sub(1),
        chunk_size,
        chunk_size.saturating_add(1),
        chunk_size.saturating_mul(2),
        chunk_size.saturating_mul(2).saturating_add(1),
        max_bytes,
    ] {
        if (1..=max_bytes).contains(&candidate) {
            lengths.push(candidate);
        }
    }
    lengths
}

/// `fixed` sets every byte. `None` draws each byte independently.
/// Empty uploads are rejected by the builder (spec §4), so the length is at least 1.
fn owned_bytes(
    lengths: Vec<usize>,
    max_bytes: usize,
    fixed: Option<u8>,
) -> impl Strategy<Value = Vec<u8>> {
    biased_length_up_to(lengths, max_bytes).prop_flat_map(move |length| {
        let length = length.clamp(1, max_bytes.max(1));
        match fixed {
            Some(byte) => prop::collection::vec(Just(byte), length).boxed(),
            None => prop::collection::vec(any::<u8>(), length).boxed(),
        }
    })
}

fn repeated_full_chunks(max_bytes: usize, chunk_size: usize) -> BoxedStrategy<Vec<u8>> {
    // Two copies of one full chunk, when they fit. Equal bytes at two indexes
    // are the case spec §10 allows: the claim at the other index is true.
    let pair = chunk_size.saturating_mul(2);
    if chunk_size == 0 || pair > max_bytes {
        let length = chunk_size.clamp(1, max_bytes.max(1));
        return prop::collection::vec(any::<u8>(), length).boxed();
    }
    prop::collection::vec(any::<u8>(), chunk_size)
        .prop_map(move |chunk| {
            let mut bytes = Vec::with_capacity(pair);
            bytes.extend_from_slice(&chunk);
            bytes.extend_from_slice(&chunk);
            bytes
        })
        .boxed()
}
