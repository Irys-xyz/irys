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
//! slot is a node that was carried up. The proof length is `depth(c, C)`, never
//! the height of the stack (spec §4).

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
    data_size: u64,
    last_chunk_short: bool,
}

impl DataTreeBuilder {
    /// An empty builder. `finish` fails until at least one chunk is pushed.
    pub fn new(geometry: Geometry) -> Self {
        Self {
            geometry,
            layers: Vec::new(),
            data_size: 0,
            last_chunk_short: false,
        }
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
            self.layers.push(Vec::new());
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
    /// is `BuildError::TooManyChunks`.
    pub fn from_bytes(geometry: Geometry, data: &[u8]) -> Result<Self, BuildError> {
        let mut builder = DataTreeBuilder::new(geometry);
        if data.is_empty() {
            return builder.finish();
        }
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

    /// Chunk proof for index `c`: `32 * depth(c, C)` bytes, leaf to root (spec §5).
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
        debug_assert_eq!(
            proof.len(),
            usize::try_from(depth(chunk_index, chunk_count)).expect("depth fits usize") * HASH_SIZE
        );
        Ok(proof)
    }

    /// Segment proof: `h` in-chunk siblings, then the chunk proof (spec §4, §5).
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
            // `h + depth(c, C)` fits `u32`: `h` is at most `MAX_SUBTREE_HEIGHT`.
            usize::try_from(self.geometry.height() + depth(chunk_index, chunk_count))
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

fn sibling_at(layer: &[NodeHash], chunk_index: u32, level: usize) -> Option<NodeHash> {
    // At level `l`, the node index is `c >> l` and the sibling flips that bit
    // (`i ^ 1`). A missing slot is a carried unpaired node (spec §4).
    //
    // A shift of a `u32` by `u32::BITS` or more is 0. Rust panics on that shift.
    // Only a tree of `u32::MAX` chunks reaches that level, and the level is the
    // root, which has no sibling.
    let index = match u32::try_from(level) {
        Ok(shift) if shift < u32::BITS => chunk_index >> shift,
        _ => 0,
    };
    let sibling_index = index ^ 1;
    let sibling_index = usize::try_from(sibling_index).expect("sibling index fits usize");
    layer.get(sibling_index).copied()
}
