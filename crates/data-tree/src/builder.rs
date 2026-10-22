//! Streaming chunk tree (spec §4).
//!
//! Layer 0 holds chunk roots. Layer `l + 1` holds pairs of layer `l`, left to
//! right. An unpaired last node is carried up unchanged: the same `NodeHash`,
//! not a new hash. After each push, a layer whose length just became even
//! appends `node(left, right)` to the next layer. `finish` carries odd tails
//! until one root remains.
//!
//! The stack is `build_layer` kept between pushes, not a fresh recursive `path`
//! (spec §4). A `path` recomputes each sibling subtree; this cache stores those
//! subtrees, so `layers[l][i ^ 1]` is the sibling `path` would append. A missing
//! slot is a node that was carried up. The sibling count is `depth(c, C)`. The
//! proof is those siblings and then the size witness, never the height of the
//! stack (spec §4, §5).

use crate::chunk::{chunk_root, segment_siblings};
use crate::error::{BuildError, ChunkLengthError, ProofError, SizeError};
use crate::geometry::Geometry;
use crate::hash::{self, node};
use crate::shape::depth;
use crate::types::{ChunkKey, DataCommitment, DataTreeRoot, HASH_SIZE, NodeHash, SizeWitness};

/// Accumulates chunk roots into a data tree.
#[derive(Debug, Clone)]
pub struct DataTreeBuilder {
    geometry: Geometry,
    layers: Vec<Vec<NodeHash>>,
    /// Final length of each layer for a `with_capacity` hint (spec §4).
    /// Empty after `new`: layers then grow on demand.
    layer_capacities: Vec<usize>,
    data_size: u64,
    last_chunk_short: bool,
}

impl DataTreeBuilder {
    /// An empty builder. `finish` fails until at least one chunk is pushed.
    pub fn new(geometry: Geometry) -> Self {
        Self {
            geometry,
            layers: Vec::new(),
            layer_capacities: Vec::new(),
            data_size: 0,
            last_chunk_short: false,
        }
    }

    /// Reserve each cache layer for an upload of `data_size` bytes.
    ///
    /// `data_size` is only a capacity hint. It never changes the tree. Pushing
    /// a different amount still works, and those pushes are checked exactly as
    /// with [`Self::new`]. A `data_size` of 0, or one above the geometry
    /// maximum, is the same [`crate::SizeError`] as [`Geometry::chunk_count`].
    ///
    /// Layer 0 holds the chunk roots. Each next layer holds `ceil(len / 2)`
    /// nodes, the `build_layer` shape with the carried node included (spec §4),
    /// until one node remains. The reservation runs when that layer is created.
    /// A failed reservation is ignored, and the layer then grows on demand, the
    /// same as [`Self::new`].
    ///
    /// On Linux with glibc the hint does not measurably change resident memory
    /// or time for a large upload: a 5 TiB tree used about 1.31 GB peak RSS and
    /// about 5–6 s both with the hint and without it, because a large
    /// allocation grows without touching the unused pages.
    pub fn with_capacity(geometry: Geometry, data_size: u64) -> Result<Self, SizeError> {
        let chunk_count = geometry.chunk_count(data_size)?;
        let layer_capacities = layer_capacities(chunk_count);
        Ok(Self {
            geometry,
            layers: Vec::with_capacity(layer_capacities.len()),
            layer_capacities,
            data_size: 0,
            last_chunk_short: false,
        })
    }

    /// Hash `bytes` with `chunk_root` and append that root.
    ///
    /// The length is checked before hashing (spec §10).
    pub fn push_chunk(&mut self, bytes: &[u8]) -> Result<(), BuildError> {
        // A slice length fits `u64` on every supported target.
        let chunk_length = u64::try_from(bytes.len()).expect("byte length fits u64");
        self.prepare_push(chunk_length)?;
        let root = chunk_root(&self.geometry, bytes)?;
        self.commit_push(root, chunk_length);
        Ok(())
    }

    /// Append a chunk root hashed by the caller, with the chunk's byte length.
    ///
    /// Callers that hash chunks in parallel use this. `chunk_length` is 0 or
    /// above `chunk_size`, a push after a short chunk, and a chunk count above
    /// `max_data_tx_chunks` are errors.
    pub fn push_chunk_root(&mut self, root: NodeHash, chunk_length: u64) -> Result<(), BuildError> {
        self.prepare_push(chunk_length)?;
        self.commit_push(root, chunk_length);
        Ok(())
    }

    /// Hash a batch of consecutive chunks, then append those roots in order.
    ///
    /// `bytes` splits on `chunk_size`. Every piece but the last is one full
    /// chunk. A short last piece is the upload's last chunk; a later push
    /// fails, as with [`Self::push_chunk`].
    ///
    /// The batch is checked before any hashing, and a failed check does not
    /// change the builder (spec §10). The appended roots match
    /// [`Self::push_chunk`] on each piece in order. Chunk roots do not depend
    /// on each other (spec §9), but the tree must stay deterministic (spec §10):
    /// the piece index selects the slice, and the collected vector is that
    /// index order, not the order workers finish.
    ///
    /// Hashing uses the rayon global pool. A caller may run this inside their
    /// own `ThreadPool::install` to bound the threads.
    #[cfg(feature = "parallel")]
    pub fn push_chunks_parallel(&mut self, bytes: &[u8]) -> Result<(), BuildError> {
        let (split, piece_count) = self.validate_chunk_batch(bytes)?;
        let roots = hash_chunk_pieces(&self.geometry, bytes, split, piece_count)?;
        for (piece_index, root) in roots.into_iter().enumerate() {
            let (start, end) = piece_bounds(bytes.len(), split, piece_index);
            let chunk_length = u64::try_from(end - start).expect("piece length fits u64");
            // Lengths, the short-chunk rule, and the chunk limit were checked
            // for every piece. This is the success path of `push_chunk_root`.
            self.push_chunk_root(root, chunk_length)
                .expect("chunk batch was validated");
        }
        Ok(())
    }

    /// Carry odd tails and return the tree. Fails when no chunk was pushed.
    pub fn finish(mut self) -> Result<DataTree, BuildError> {
        if self.pushed_chunk_count() == 0 {
            return Err(BuildError::Empty);
        }
        self.carry_odd_tails();
        debug_assert_eq!(self.layers.last().map(Vec::len), Some(1));
        Ok(DataTree {
            geometry: self.geometry,
            layers: self.layers,
            data_size: self.data_size,
        })
    }

    fn prepare_push(&self, chunk_length: u64) -> Result<(), BuildError> {
        if self.last_chunk_short {
            return Err(BuildError::AfterShortChunk);
        }
        if chunk_length == 0 {
            return Err(BuildError::ChunkLength(ChunkLengthError::Empty));
        }
        if chunk_length > self.geometry.chunk_size() {
            return Err(BuildError::ChunkLength(ChunkLengthError::TooLong {
                chunk_length,
                chunk_size: self.geometry.chunk_size(),
            }));
        }
        if self.pushed_chunk_count() >= self.geometry.max_data_tx_chunks() {
            return Err(BuildError::TooManyChunks {
                max_data_tx_chunks: self.geometry.max_data_tx_chunks(),
            });
        }
        Ok(())
    }

    /// Check every piece before hashing. On error the builder is unchanged (spec §10).
    ///
    /// Returns the split width and the number of pieces. A `chunk_size` that
    /// does not fit `usize` cannot repeat in this slice, so the split is the
    /// whole slice and there is one piece.
    #[cfg(feature = "parallel")]
    fn validate_chunk_batch(&self, bytes: &[u8]) -> Result<(usize, usize), BuildError> {
        if bytes.is_empty() {
            // Same rejection as `push_chunk(&[])`. `prepare_push(0)` always fails.
            self.prepare_push(0)?;
            unreachable!("prepare_push rejects an empty chunk");
        }

        let chunk_size = self.geometry.chunk_size();
        let split = usize::try_from(chunk_size).unwrap_or(bytes.len());
        debug_assert!(split >= 1, "geometry rejects a chunk size of 0");

        let mut offset = 0_usize;
        let mut pushed = self.pushed_chunk_count();
        let mut previous_piece_short = self.last_chunk_short;
        let mut piece_count = 0_usize;
        while offset < bytes.len() {
            if previous_piece_short {
                return Err(BuildError::AfterShortChunk);
            }
            let end = match offset.checked_add(split) {
                Some(end) => end.min(bytes.len()),
                None => bytes.len(),
            };
            let chunk_length = u64::try_from(end - offset).expect("piece length fits u64");
            // Same length rule as `prepare_push`. The split makes every piece
            // but the last full, and the last piece `1..=chunk_size`.
            if chunk_length == 0 {
                return Err(BuildError::ChunkLength(ChunkLengthError::Empty));
            }
            if chunk_length > chunk_size {
                return Err(BuildError::ChunkLength(ChunkLengthError::TooLong {
                    chunk_length,
                    chunk_size,
                }));
            }
            if pushed >= self.geometry.max_data_tx_chunks() {
                return Err(BuildError::TooManyChunks {
                    max_data_tx_chunks: self.geometry.max_data_tx_chunks(),
                });
            }
            pushed = pushed.checked_add(1).expect("chunk count fits u32");
            previous_piece_short = chunk_length < chunk_size;
            piece_count = piece_count.checked_add(1).expect("piece count fits usize");
            offset = end;
        }
        debug_assert_eq!(offset, bytes.len());
        Ok((split, piece_count))
    }

    fn commit_push(&mut self, root: NodeHash, chunk_length: u64) {
        self.push_node_at(0, root);
        // `max_data_tx_chunks * chunk_size` fits `u64`, and each push is at most
        // one chunk, so the running total fits.
        self.data_size = self
            .data_size
            .checked_add(chunk_length)
            .expect("data_size fits u64");
        self.last_chunk_short = chunk_length < self.geometry.chunk_size();
    }

    fn pushed_chunk_count(&self) -> u32 {
        let layer_length = self.layers.first().map_or(0, Vec::len);
        // Pushes stop at `max_data_tx_chunks`, which fits `u32`.
        u32::try_from(layer_length).expect("chunk count fits u32")
    }

    fn push_node_at(&mut self, level: usize, hash: NodeHash) {
        if self.layers.len() == level {
            // Apply the hint when the layer is created, not before. Empty higher
            // layers would make `finish` carry a shorter upload through them
            // (spec §4). A missing hint, or a hint that is too small, grows the
            // layer on demand.
            //
            // Fail soft. `with_capacity` aborts the process when the allocator
            // refuses the reservation or the byte size exceeds `isize::MAX`.
            // A hint is not a checked allocation: `try_reserve_exact` returns
            // that failure, and the empty layer grows on demand, the same as
            // `new`.
            let capacity = self.layer_capacities.get(level).copied().unwrap_or(0);
            let mut layer = Vec::new();
            let _ = layer.try_reserve_exact(capacity);
            self.layers.push(layer);
        }
        self.layers[level].push(hash);
        if self.layers[level].len().is_multiple_of(2) {
            let layer_length = self.layers[level].len();
            let left = self.layers[level][layer_length - 2];
            let right = self.layers[level][layer_length - 1];
            let parent = node(&left, &right);
            self.push_node_at(level + 1, parent);
        }
    }

    fn carry_odd_tails(&mut self) {
        let mut level = 0;
        while level < self.layers.len() {
            let layer_length = self.layers[level].len();
            if !layer_length.is_multiple_of(2) {
                // A single node on the highest layer is the root. Carrying it
                // again would hash the root with itself.
                if layer_length == 1 && level + 1 == self.layers.len() {
                    break;
                }
                let tail = self.layers[level][layer_length - 1];
                self.push_node_at(level + 1, tail);
            }
            level += 1;
        }
    }
}

/// A finished data tree: chunk roots, internal nodes, and `data_size`.
#[derive(Debug, Clone)]
pub struct DataTree {
    geometry: Geometry,
    layers: Vec<Vec<NodeHash>>,
    data_size: u64,
}

impl DataTree {
    /// Split `data` into chunks and build the tree.
    ///
    /// Empty `data` is `BuildError::Empty`. A length above the geometry maximum
    /// is `BuildError::TooManyChunks`. With the `parallel` feature the chunk
    /// roots are hashed as one batch (spec §9). The tree matches the sequential
    /// pushes either way (spec §10).
    pub fn from_bytes(geometry: Geometry, data: &[u8]) -> Result<Self, BuildError> {
        let mut builder = DataTreeBuilder::new(geometry);
        if data.is_empty() {
            return builder.finish();
        }
        #[cfg(feature = "parallel")]
        builder.push_chunks_parallel(data)?;
        #[cfg(not(feature = "parallel"))]
        {
            let mut offset = 0_usize;
            while offset < data.len() {
                let remaining = data.len() - offset;
                let chunk_length = match usize::try_from(geometry.chunk_size()) {
                    Ok(chunk_size) => chunk_size.min(remaining),
                    // `chunk_size` does not fit `usize`, so the whole slice is one chunk.
                    Err(_) => remaining,
                };
                // `chunk_size >= 1` and `remaining >= 1`, so this slice is non-empty.
                debug_assert!(chunk_length > 0);
                builder.push_chunk(&data[offset..offset + chunk_length])?;
                offset += chunk_length;
            }
        }
        builder.finish()
    }

    /// Byte length covered by the tree.
    pub fn data_size(&self) -> u64 {
        self.data_size
    }

    /// Number of chunk roots.
    pub fn chunk_count(&self) -> u32 {
        self.layers.first().map_or(0, |layer| {
            // `finish` only returns a tree whose chunk count fits `u32`.
            u32::try_from(layer.len()).expect("chunk count fits u32")
        })
    }

    /// Chunk root stored at index `c`.
    ///
    /// `c` is `chunk_index`.
    pub fn chunk_root(&self, chunk_index: u32) -> Result<NodeHash, SizeError> {
        let layer = self.layers.first();
        let layer_length = layer.map_or(0, Vec::len);
        let chunk_count = u32::try_from(layer_length).expect("chunk count fits u32");
        let Some(layer) = layer else {
            return Err(SizeError::ChunkIndexOutOfRange {
                chunk_index,
                chunk_count,
            });
        };
        let Some(index) = usize::try_from(chunk_index)
            .ok()
            .filter(|index| *index < layer.len())
        else {
            return Err(SizeError::ChunkIndexOutOfRange {
                chunk_index,
                chunk_count,
            });
        };
        Ok(layer[index])
    }

    /// Root over the data only (spec §4).
    pub fn data_tree_root(&self) -> DataTreeRoot {
        DataTreeRoot::from_hash(self.root_hash())
    }

    /// Size witness for this tree's `data_size`.
    pub fn size_witness(&self) -> SizeWitness {
        hash::size_witness(self.data_size)
    }

    /// `node(data_tree_root, size_witness)` (spec §4).
    pub fn data_commitment(&self) -> DataCommitment {
        hash::data_commitment(&self.data_tree_root(), self.data_size)
    }

    /// Chunk identity key for index `c` (spec §7).
    ///
    /// `c` is `chunk_index`.
    pub fn chunk_key(&self, chunk_index: u32) -> Result<ChunkKey, SizeError> {
        self.ensure_chunk(chunk_index)?;
        Ok(hash::chunk_key(&self.data_commitment(), chunk_index))
    }

    /// Chunk proof for index `c`: siblings, then `size_witness` (spec §5).
    ///
    /// `32 * (depth(c, C) + 1)` bytes, leaf to root. For one chunk the proof is
    /// the witness alone.
    ///
    /// `c` is `chunk_index`.
    pub fn chunk_proof(&self, chunk_index: u32) -> Result<Vec<u8>, ProofError> {
        let chunk_count = self.ensure_chunk(chunk_index)?;
        let mut proof = Vec::new();
        for (level, layer) in self.layers.iter().enumerate() {
            if let Some(sibling) = sibling_at(layer, chunk_index, level) {
                proof.extend_from_slice(sibling.as_bytes());
            }
        }
        proof.extend_from_slice(self.size_witness().as_bytes());
        debug_assert_eq!(
            proof.len(),
            usize::try_from(
                depth(chunk_index, chunk_count)
                    .expect("ensure_chunk requires chunk_index < chunk_count")
                    + 1
            )
            .expect("depth fits usize")
                * HASH_SIZE
        );
        Ok(proof)
    }

    /// Segment proof: `h` in-chunk siblings, then the chunk proof (spec §4, §5).
    ///
    /// The chunk proof ends with `size_witness`, so this concatenation is the
    /// whole segment proof. That includes a one-chunk upload, whose chunk proof
    /// is only the witness.
    ///
    /// `chunk_bytes` must be exactly `chunk_len` and must hash to the stored
    /// chunk root. Those checks run before the sibling hashes are built.
    ///
    /// `c` is `chunk_index`. `j` is `segment_index`.
    pub fn segment_proof(
        &self,
        chunk_index: u32,
        segment_index: u32,
        chunk_bytes: &[u8],
    ) -> Result<Vec<u8>, ProofError> {
        let chunk_count = self.ensure_chunk(chunk_index)?;
        if segment_index >= self.geometry.segments_per_chunk() {
            return Err(ProofError::Size(SizeError::SegmentIndexOutOfRange {
                segment_index,
                segments_per_chunk: self.geometry.segments_per_chunk(),
            }));
        }
        let expected = self.geometry.chunk_len(self.data_size, chunk_index)?;
        let actual = u64::try_from(chunk_bytes.len()).expect("byte length fits u64");
        if actual != expected {
            return Err(ProofError::InvalidChunkLength { expected, actual });
        }
        let computed = chunk_root(&self.geometry, chunk_bytes)
            .map_err(|_| ProofError::InvalidChunkLength { expected, actual })?;
        if computed != self.chunk_root(chunk_index)? {
            return Err(ProofError::ChunkRootMismatch);
        }
        let mut proof = Vec::new();
        for sibling in segment_siblings(&self.geometry, chunk_bytes, segment_index) {
            proof.extend_from_slice(sibling.as_bytes());
        }
        proof.extend(self.chunk_proof(chunk_index)?);
        debug_assert_eq!(
            proof.len() / HASH_SIZE,
            // `h + depth(c, C) + 1` fits `u32`: `h` is at most `MAX_SUBTREE_HEIGHT`.
            usize::try_from(
                self.geometry.height()
                    + depth(chunk_index, chunk_count)
                        .expect("ensure_chunk requires chunk_index < chunk_count")
                    + 1
            )
            .expect("proof hash count fits usize")
        );
        Ok(proof)
    }

    fn ensure_chunk(&self, chunk_index: u32) -> Result<u32, SizeError> {
        let layer_length = self.layers.first().map_or(0, Vec::len);
        // `finish` only returns a tree whose chunk count fits `u32`.
        let chunk_count = u32::try_from(layer_length).expect("chunk count fits u32");
        if chunk_index >= chunk_count {
            return Err(SizeError::ChunkIndexOutOfRange {
                chunk_index,
                chunk_count,
            });
        }
        Ok(chunk_count)
    }

    fn root_hash(&self) -> NodeHash {
        // `finish` carries tails until one node remains.
        let layer = self
            .layers
            .last()
            .filter(|layer| !layer.is_empty())
            .expect("finish leaves a root");
        debug_assert_eq!(layer.len(), 1, "finish leaves one root");
        layer[layer.len() - 1]
    }

    #[cfg(test)]
    pub(crate) fn layers(&self) -> &[Vec<NodeHash>] {
        &self.layers
    }
}

/// Final length of each cache layer for `chunk_count` chunk roots.
///
/// Layer 0 is the chunk roots. Each next layer is `ceil(previous / 2)`: one
/// slot per pair, plus the carried unpaired node (spec §4 `build_layer`). Stop
/// when one node remains. The loop is that rule; it is not a closed form.
fn layer_capacities(chunk_count: u32) -> Vec<usize> {
    let mut layer_length = usize::try_from(chunk_count).expect("chunk count fits usize");
    debug_assert!(layer_length >= 1, "chunk_count rejects an empty upload");
    let mut capacities = Vec::new();
    capacities.push(layer_length);
    while layer_length > 1 {
        layer_length = layer_length.div_ceil(2);
        capacities.push(layer_length);
    }
    capacities
}

/// Byte range of piece `piece_index` when `bytes` is split into `split`-byte pieces.
#[cfg(feature = "parallel")]
fn piece_bounds(byte_length: usize, split: usize, piece_index: usize) -> (usize, usize) {
    let start = piece_index.checked_mul(split).expect("piece start fits");
    let end = match start.checked_add(split) {
        Some(end) => end.min(byte_length),
        None => byte_length,
    };
    (start, end)
}

/// Chunk root of each piece. The output index is the piece index (spec §9, §10).
#[cfg(feature = "parallel")]
fn hash_chunk_pieces(
    geometry: &Geometry,
    bytes: &[u8],
    split: usize,
    piece_count: usize,
) -> Result<Vec<NodeHash>, BuildError> {
    use rayon::prelude::*;

    let byte_length = bytes.len();
    (0..piece_count)
        .into_par_iter()
        .map(|piece_index| {
            let (start, end) = piece_bounds(byte_length, split, piece_index);
            chunk_root(geometry, &bytes[start..end]).map_err(BuildError::from)
        })
        .collect()
}

fn sibling_at(layer: &[NodeHash], chunk_index: u32, level: usize) -> Option<NodeHash> {
    // At level `l`, the node index is `c >> l` and the sibling flips that bit
    // (`i ^ 1`). A missing slot is a carried unpaired node (spec §4).
    //
    // A level at or above `u32::BITS` is the root layer of a tree with more than
    // 2^31 chunks, and the root has no sibling.
    let shift = u32::try_from(level)
        .ok()
        .filter(|shift| *shift < u32::BITS)?;
    let index = chunk_index >> shift;
    let sibling_index = index ^ 1;
    let sibling_index = usize::try_from(sibling_index).expect("sibling index fits usize");
    layer.get(sibling_index).copied()
}

#[cfg(test)]
mod tests {
    use super::sibling_at;
    use crate::types::{HASH_SIZE, NodeHash};

    fn node_hash(byte: u8) -> NodeHash {
        NodeHash::from([byte; HASH_SIZE])
    }

    /// A level at or above `u32::BITS` has no sibling. The one-node layer is that
    /// root and builds no tree. A second node sits at index 1 so a rewrite of the
    /// index to 0 would return that node.
    #[test]
    fn root_layer_at_or_above_u32_bits_has_no_sibling() {
        let root_level = usize::try_from(u32::BITS).expect("bit width fits usize");
        let level_above_the_root = root_level + 1;
        let chunk_index = u32::MAX;
        let one_node_layer = [node_hash(1)];
        assert_eq!(sibling_at(&one_node_layer, chunk_index, root_level), None);
        assert_eq!(
            sibling_at(&one_node_layer, chunk_index, level_above_the_root),
            None
        );

        let layer_with_index_one = [node_hash(1), node_hash(2)];
        assert_eq!(
            sibling_at(&layer_with_index_one, chunk_index, root_level),
            None
        );
        assert_eq!(
            sibling_at(&layer_with_index_one, chunk_index, level_above_the_root),
            None
        );
    }

    /// Level `u32::BITS - 1` is still a valid shift. A sibling that is present is returned.
    #[test]
    fn sibling_at_the_highest_valid_shift_is_still_found() {
        let left = node_hash(1);
        let right = node_hash(2);
        let layer = [left, right];
        let highest_valid_level = usize::try_from(u32::BITS - 1).expect("bit width fits usize");
        // The high bit is clear: the node index is 0, so the sibling is index 1.
        assert_eq!(sibling_at(&layer, 0, highest_valid_level), Some(right));
        let high_chunk_index = 1_u32 << (u32::BITS - 1);
        // The high bit is set: the node index is 1, so the sibling is index 0.
        assert_eq!(
            sibling_at(&layer, high_chunk_index, highest_valid_level),
            Some(left)
        );
    }
}
