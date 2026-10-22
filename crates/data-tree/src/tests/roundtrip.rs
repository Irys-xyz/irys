//! Every chunk proof and every segment proof for `n` in `1..=199` (spec §13).

use super::pattern;
use crate::DataTree;
use crate::geometry::Geometry;
use crate::verify::{verify_chunk, verify_segment, verify_size};

#[test]
fn every_proof_verifies_for_n_up_to_199() {
    let geometry = Geometry::TESTING;
    for data_size in 1_u64..=199 {
        let data = pattern(usize::try_from(data_size).unwrap());
        let tree = DataTree::from_bytes(geometry, &data).unwrap();
        let commitment = tree.data_commitment();
        verify_size(&geometry, &commitment, &tree.data_tree_root(), data_size)
            .unwrap_or_else(|err| panic!("verify_size n={data_size}: {err}"));
        assert_eq!(tree.chunk_count(), geometry.chunk_count(data_size).unwrap());
        for chunk_index in 0..tree.chunk_count() {
            let chunk = chunk_slice(&geometry, &data, data_size, chunk_index);
            let chunk_proof = tree.chunk_proof(chunk_index).unwrap();
            verify_chunk(
                &geometry,
                &commitment,
                data_size,
                chunk_index,
                &chunk,
                &chunk_proof,
            )
            .unwrap_or_else(|err| panic!("chunk n={data_size} c={chunk_index}: {err}"));
            for segment_index in 0..geometry.segments_per_chunk() {
                // A segment past `n` is an empty slice. Do not read past the data.
                let segment =
                    segment_slice(&geometry, &data, data_size, chunk_index, segment_index);
                let segment_proof = tree
                    .segment_proof(chunk_index, segment_index, &chunk)
                    .unwrap();
                verify_segment(
                    &geometry,
                    &commitment,
                    data_size,
                    chunk_index,
                    segment_index,
                    &segment,
                    &segment_proof,
                )
                .unwrap_or_else(|err| {
                    panic!("segment n={data_size} c={chunk_index} j={segment_index}: {err}")
                });
            }
        }
    }
}

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
