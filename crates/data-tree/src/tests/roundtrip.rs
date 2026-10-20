//! Every chunk proof and every segment proof for `n` in `1..=199` (spec §13).

use super::pattern;
use crate::DataTree;
use crate::geometry::Geometry;
use crate::verify::{verify_chunk, verify_segment, verify_size};

#[test]
fn every_proof_verifies_for_n_up_to_199() {
    let geom = Geometry::TESTING;
    for n in 1_u64..=199 {
        let data = pattern(usize::try_from(n).unwrap());
        let tree = DataTree::from_bytes(geom, &data).unwrap();
        let commitment = tree.data_commitment();
        verify_size(&commitment, &tree.data_tree_root(), n)
            .unwrap_or_else(|err| panic!("verify_size n={n}: {err}"));
        assert_eq!(tree.chunk_count(), geom.chunk_count(n).unwrap());
        for c in 0..tree.chunk_count() {
            let chunk = chunk_slice(&geom, &data, n, c);
            let chunk_proof = tree.chunk_proof(c).unwrap();
            verify_chunk(&geom, &commitment, n, c, &chunk, &chunk_proof)
                .unwrap_or_else(|err| panic!("chunk n={n} c={c}: {err}"));
            for j in 0..geom.segments_per_chunk() {
                // A segment past `n` is an empty slice. Do not read past the data.
                let segment = segment_slice(&geom, &data, n, c, j);
                let segment_proof = tree.segment_proof(c, j, &chunk).unwrap();
                verify_segment(&geom, &commitment, n, c, j, &segment, &segment_proof)
                    .unwrap_or_else(|err| panic!("segment n={n} c={c} j={j}: {err}"));
            }
        }
    }
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
