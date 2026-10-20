//! Streaming chunk tree (spec §4).
//!
//! Layer 0 holds chunk roots. Layer `l + 1` holds pairs of layer `l`, left to
//! right. An unpaired last node is carried up unchanged: the same `NodeHash`,
//! not a new hash. After each push, a layer whose length just became even
//! appends `node(left, right)` to the next layer. `finish` carries odd tails
//! until one root remains.
//!
//! A proof reads siblings from this cache. At level `l`, `i = c >> l`; the
//! sibling is `layers[l][i ^ 1]` when that index exists, and nothing when the
//! node was carried. The proof length is `depth(c, C)`, never the height of
//! the builder's stack (spec §4).

use crate::chunk::{chunk_root, segment_siblings};
use crate::error::{BuildError, ChunkLengthError, ProofError, SizeError};
use crate::geometry::Geometry;
use crate::hash::{self, node};
use crate::shape::depth;
use crate::types::{ChunkKey, DataCommitment, DataTreeRoot, NodeHash, SizeWitness};

/// Accumulates chunk roots into a data tree.
#[derive(Debug, Clone)]
pub struct DataTreeBuilder {
    geom: Geometry,
    layers: Vec<Vec<NodeHash>>,
    data_size: u64,
    last_chunk_short: bool,
}

impl DataTreeBuilder {
    /// An empty builder. `finish` fails until at least one chunk is pushed.
    pub fn new(geom: Geometry) -> Self {
        Self {
            geom,
            layers: Vec::new(),
            data_size: 0,
            last_chunk_short: false,
        }
    }

    /// Hash `bytes` with `chunk_root` and append that root.
    ///
    /// The length is checked before hashing (spec §10).
    pub fn push_chunk(&mut self, bytes: &[u8]) -> Result<(), BuildError> {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        self.prepare_push(len)?;
        let root = chunk_root(&self.geom, bytes)?;
        self.commit_push(root, len);
        Ok(())
    }

    /// Append a chunk root hashed by the caller, with the chunk's byte length.
    ///
    /// Callers that hash chunks in parallel use this. `len` is 0 or above
    /// `chunk_size`, a push after a short chunk, and a chunk count above
    /// `max_data_tx_chunks` are errors.
    pub fn push_chunk_root(&mut self, root: NodeHash, len: u64) -> Result<(), BuildError> {
        self.prepare_push(len)?;
        self.commit_push(root, len);
        Ok(())
    }

    /// Carry odd tails and return the tree. Fails when no chunk was pushed.
    pub fn finish(mut self) -> Result<DataTree, BuildError> {
        if self.chunk_len_pushed() == 0 {
            return Err(BuildError::Empty);
        }
        self.carry_odd_tails();
        debug_assert_eq!(self.layers.last().map(Vec::len), Some(1));
        Ok(DataTree {
            geom: self.geom,
            layers: self.layers,
            data_size: self.data_size,
        })
    }

    fn prepare_push(&self, len: u64) -> Result<(), BuildError> {
        if self.last_chunk_short {
            return Err(BuildError::AfterShortChunk);
        }
        if len == 0 {
            return Err(BuildError::ChunkLength(ChunkLengthError::Empty));
        }
        if len > self.geom.chunk_size() {
            return Err(BuildError::ChunkLength(ChunkLengthError::TooLong {
                len,
                chunk_size: self.geom.chunk_size(),
            }));
        }
        if self.chunk_len_pushed() >= self.geom.max_data_tx_chunks() {
            return Err(BuildError::TooManyChunks {
                max: self.geom.max_data_tx_chunks(),
            });
        }
        // `max_data_tx_chunks * chunk_size` fits `u64`, and each push is at most
        // one chunk, so the running total fits. Refuse if it does not.
        if self.data_size.checked_add(len).is_none() {
            return Err(BuildError::TooManyChunks {
                max: self.geom.max_data_tx_chunks(),
            });
        }
        Ok(())
    }

    fn commit_push(&mut self, root: NodeHash, len: u64) {
        self.push_node_at(0, root);
        // `prepare_push` rejected a sum that does not fit.
        self.data_size = self.data_size.checked_add(len).expect("data_size fits u64");
        self.last_chunk_short = len < self.geom.chunk_size();
    }

    fn chunk_len_pushed(&self) -> u32 {
        let len = self.layers.first().map_or(0, Vec::len);
        u32::try_from(len).unwrap_or(u32::MAX)
    }

    fn push_node_at(&mut self, level: usize, hash: NodeHash) {
        if self.layers.len() == level {
            self.layers.push(Vec::new());
        }
        self.layers[level].push(hash);
        if self.layers[level].len().is_multiple_of(2) {
            let len = self.layers[level].len();
            let left = self.layers[level][len - 2];
            let right = self.layers[level][len - 1];
            let parent = node(&left, &right);
            self.push_node_at(level + 1, parent);
        }
    }

    fn carry_odd_tails(&mut self) {
        let mut level = 0;
        while level < self.layers.len() {
            let len = self.layers[level].len();
            if !len.is_multiple_of(2) {
                // A single node on the highest layer is the root. Carrying it
                // again would hash the root with itself.
                if len == 1 && level + 1 == self.layers.len() {
                    break;
                }
                let tail = self.layers[level][len - 1];
                self.push_node_at(level + 1, tail);
            }
            level += 1;
        }
    }
}

/// A finished data tree: chunk roots, internal nodes, and `data_size`.
#[derive(Debug, Clone)]
pub struct DataTree {
    geom: Geometry,
    layers: Vec<Vec<NodeHash>>,
    data_size: u64,
}

impl DataTree {
    /// Split `data` into chunks and build the tree.
    ///
    /// Empty `data` is `BuildError::Empty`. A length above the geometry maximum
    /// is `BuildError::TooManyChunks`.
    pub fn from_bytes(geom: Geometry, data: &[u8]) -> Result<Self, BuildError> {
        let mut builder = DataTreeBuilder::new(geom);
        if data.is_empty() {
            return builder.finish();
        }
        let mut offset = 0_usize;
        while offset < data.len() {
            let remaining = data.len() - offset;
            let step = match usize::try_from(geom.chunk_size()) {
                Ok(chunk_size) => chunk_size.min(remaining),
                // `chunk_size` does not fit `usize`, so the whole slice is one chunk.
                Err(_) => remaining,
            };
            if step == 0 {
                return Err(BuildError::ChunkLength(ChunkLengthError::Empty));
            }
            builder.push_chunk(&data[offset..offset + step])?;
            offset += step;
        }
        builder.finish()
    }

    /// Byte length covered by the tree.
    pub fn data_size(&self) -> u64 {
        self.data_size
    }

    /// Number of chunk roots.
    pub fn chunk_count(&self) -> u32 {
        self.layers
            .first()
            .map_or(0, |layer| u32::try_from(layer.len()).unwrap_or(u32::MAX))
    }

    /// Chunk root stored at index `c`.
    pub fn chunk_root(&self, c: u32) -> Result<NodeHash, SizeError> {
        let layer = self.layers.first();
        let len = layer.map_or(0, Vec::len);
        let chunk_count = u32::try_from(len).unwrap_or(u32::MAX);
        let Some(layer) = layer else {
            return Err(SizeError::ChunkIndexOutOfRange { c, chunk_count });
        };
        let Some(idx) = usize::try_from(c).ok().filter(|idx| *idx < layer.len()) else {
            return Err(SizeError::ChunkIndexOutOfRange { c, chunk_count });
        };
        Ok(layer[idx])
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
    pub fn chunk_key(&self, c: u32) -> Result<ChunkKey, SizeError> {
        self.ensure_chunk(c)?;
        Ok(hash::chunk_key(&self.data_commitment(), c))
    }

    /// Chunk proof for index `c`: `32 * depth(c, C)` bytes, leaf to root (spec §5).
    pub fn chunk_proof(&self, c: u32) -> Result<Vec<u8>, ProofError> {
        let chunk_count = self.ensure_chunk(c)?;
        let mut proof = Vec::new();
        for (level, layer) in self.layers.iter().enumerate() {
            if let Some(sibling) = sibling_at(layer, c, level) {
                proof.extend_from_slice(sibling.as_bytes());
            }
        }
        debug_assert_eq!(
            proof.len(),
            usize::try_from(depth(c, chunk_count)).unwrap_or(0) * 32
        );
        Ok(proof)
    }

    /// Segment proof: `h` in-chunk siblings, then the chunk proof (spec §4, §5).
    ///
    /// `chunk_bytes` must be exactly `chunk_len` and must hash to the stored
    /// chunk root. Those checks run before the sibling hashes are built.
    pub fn segment_proof(&self, c: u32, j: u32, chunk_bytes: &[u8]) -> Result<Vec<u8>, ProofError> {
        let chunk_count = self.ensure_chunk(c)?;
        if j >= self.geom.segments_per_chunk() {
            return Err(ProofError::Size(SizeError::SegmentIndexOutOfRange {
                j,
                spc: self.geom.segments_per_chunk(),
            }));
        }
        let expected = self.geom.chunk_len(self.data_size, c)?;
        let actual = u64::try_from(chunk_bytes.len()).unwrap_or(u64::MAX);
        if actual != expected {
            return Err(ProofError::InvalidChunkLength { expected, actual });
        }
        let computed = chunk_root(&self.geom, chunk_bytes)
            .map_err(|_| ProofError::InvalidChunkLength { expected, actual })?;
        if computed != self.chunk_root(c)? {
            return Err(ProofError::ChunkRootMismatch);
        }
        let mut proof = Vec::new();
        for sibling in segment_siblings(&self.geom, chunk_bytes, j) {
            proof.extend_from_slice(sibling.as_bytes());
        }
        proof.extend(self.chunk_proof(c)?);
        debug_assert_eq!(
            proof.len() / 32,
            usize::try_from(self.geom.height().saturating_add(depth(c, chunk_count))).unwrap_or(0)
        );
        Ok(proof)
    }

    fn ensure_chunk(&self, c: u32) -> Result<u32, SizeError> {
        let len = self.layers.first().map_or(0, Vec::len);
        let Ok(chunk_count) = u32::try_from(len) else {
            return Err(SizeError::ChunkIndexOutOfRange {
                c,
                chunk_count: u32::MAX,
            });
        };
        if c >= chunk_count {
            return Err(SizeError::ChunkIndexOutOfRange { c, chunk_count });
        }
        Ok(chunk_count)
    }

    fn root_hash(&self) -> NodeHash {
        // `finish` carries tails until one node remains. A missing root is a
        // broken builder; return a digest that no honest commitment uses.
        let Some(layer) = self.layers.last().filter(|layer| !layer.is_empty()) else {
            return NodeHash([0; 32]);
        };
        debug_assert_eq!(layer.len(), 1, "finish leaves one root");
        layer[layer.len() - 1]
    }

    #[cfg(test)]
    pub(crate) fn layers(&self) -> &[Vec<NodeHash>] {
        &self.layers
    }
}

fn sibling_at(layer: &[NodeHash], c: u32, level: usize) -> Option<NodeHash> {
    let index = if level >= usize::try_from(u32::BITS).unwrap_or(32) {
        0
    } else {
        c >> level
    };
    let sibling = index ^ 1;
    layer.get(usize::try_from(sibling).ok()?).copied()
}
