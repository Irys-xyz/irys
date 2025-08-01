//! Validates merkle tree proofs for Irys transaction data and proof chunks

use crate::chunked::ChunkedIterator;
use crate::Base64;
use crate::ChunkBytes;
use crate::H256;
use alloy_primitives::Address;
use borsh::BorshDeserialize as _;
use borsh_derive::BorshDeserialize;
use eyre::eyre;
use eyre::Error;
use eyre::OptionExt as _;
use openssl::sha;
use tracing::debug;

/// Single struct used for original data chunks (Leaves) and branch nodes (hashes of pairs of child nodes).
#[derive(Debug, PartialEq, Clone)]
pub struct Node {
    pub id: [u8; HASH_SIZE],
    pub data_hash: Option<[u8; HASH_SIZE]>,
    pub min_byte_range: usize,
    pub max_byte_range: usize,
    pub left_child: Option<Box<Node>>,
    pub right_child: Option<Box<Node>>,
}

/// Concatenated ids and offsets for full set of nodes for an original data chunk, starting with the root.
#[derive(Debug, PartialEq, Clone)]
pub struct Proof {
    pub offset: usize,
    pub proof: Vec<u8>,
}
/// Populated with data from deserialized [`Proof`] for original data chunk (Leaf [`Node`]).
#[repr(C)]
#[derive(BorshDeserialize, Debug, PartialEq, Clone)]
pub struct LeafProof {
    data_hash: [u8; HASH_SIZE],
    notepad: [u8; NOTE_SIZE - 8],
    offset: [u8; 8],
}

/// Populated with data from deserialized [`Proof`] for branch [`Node`] (hash of pair of child nodes).
#[derive(BorshDeserialize, Debug, PartialEq, Clone)]
pub struct BranchProof {
    left_id: [u8; HASH_SIZE],
    right_id: [u8; HASH_SIZE],
    notepad: [u8; NOTE_SIZE - 8],
    offset: [u8; 8],
}

/// Includes methods to deserialize [`Proof`]s.
pub trait ProofDeserialize<T> {
    fn try_from_proof_slice(slice: &[u8]) -> Result<T, Error>;
    fn offset(&self) -> usize;
    fn hash(&self) -> Option<[u8; HASH_SIZE]>;
}

impl ProofDeserialize<Self> for LeafProof {
    fn try_from_proof_slice(slice: &[u8]) -> Result<Self, Error> {
        let proof = Self::try_from_slice(slice)?;
        Ok(proof)
    }
    fn offset(&self) -> usize {
        usize::from_be_bytes(self.offset)
    }

    fn hash(&self) -> Option<[u8; HASH_SIZE]> {
        Some(self.data_hash)
    }
}

impl ProofDeserialize<Self> for BranchProof {
    fn try_from_proof_slice(slice: &[u8]) -> Result<Self, Error> {
        let proof = Self::try_from_slice(slice)?;
        Ok(proof)
    }
    fn offset(&self) -> usize {
        usize::from_be_bytes(self.offset)
    }
    fn hash(&self) -> Option<[u8; HASH_SIZE]> {
        None
    }
}

pub const MAX_CHUNK_SIZE: usize = 256 * 1024;
pub const MIN_CHUNK_SIZE: usize = 32 * 1024;
pub const HASH_SIZE: usize = 32;
const NOTE_SIZE: usize = 32;

/// Includes a function to convert a number to a Vec of 32 bytes per spec.
pub trait Helpers<T> {
    fn to_note_vec(&self) -> Vec<u8>;
}

impl Helpers<Self> for usize {
    fn to_note_vec(&self) -> Vec<u8> {
        let mut note = vec![0; NOTE_SIZE - 8];
        note.extend((*self as u64).to_be_bytes());
        note
    }
}

#[derive(Debug)]
pub struct ValidatePathResult {
    pub leaf_hash: [u8; HASH_SIZE],
    pub left_bound: u128,
    pub right_bound: u128,
}

pub fn get_leaf_proof(path_buff: &Base64) -> Result<LeafProof, Error> {
    let (_, leaf) = path_buff.split_at(path_buff.len() - HASH_SIZE - NOTE_SIZE);
    let leaf_proof = LeafProof::try_from_proof_slice(leaf)?;
    Ok(leaf_proof)
}

pub fn validate_path(
    root_hash: [u8; HASH_SIZE],
    path_buff: &Base64,
    target_offset: u128,
) -> Result<ValidatePathResult, Error> {
    // Split proof into branches and leaf. Leaf is the final proof and branches
    // are ordered from root to leaf.
    let (branches, leaf) = path_buff.split_at(path_buff.len() - HASH_SIZE - NOTE_SIZE);

    // Deserialize proof.
    let branch_proofs: Vec<BranchProof> = branches
        .chunks(HASH_SIZE * 2 + NOTE_SIZE)
        .map(|b| BranchProof::try_from_proof_slice(b).unwrap())
        .collect();
    let leaf_proof = LeafProof::try_from_proof_slice(leaf)?;

    let mut left_bound: u128 = 0;
    let mut expected_path_hash = root_hash;

    // Validate branches.
    for branch_proof in branch_proofs.iter() {
        // Calculate the path_hash from the proof elements.
        let path_hash = hash_all_sha256(vec![
            &branch_proof.left_id,
            &branch_proof.right_id,
            &branch_proof.offset().to_note_vec(),
        ])?;

        // Proof is invalid if the calculated path_hash doesn't match expected
        if path_hash != expected_path_hash {
            return Err(eyre!("Invalid Branch Proof"));
        }

        let offset = branch_proof.offset() as u128;
        let is_right_of_offset = target_offset >= offset;

        // Choose the next expected_path_hash based on weather the target_offset
        // byte is to the left or right of the branch_proof's "offset" value
        expected_path_hash = if is_right_of_offset {
            branch_proof.right_id
        } else {
            branch_proof.left_id
        };

        // Keep track of left bound as we traverse down the branches
        if is_right_of_offset {
            left_bound = offset;
        }

        println!(
            "BranchProof: left: {}{}, right: {}{},offset: {} => path_hash: {}",
            if is_right_of_offset { "" } else { "✅" },
            base64_url::encode(&branch_proof.left_id),
            if is_right_of_offset { "✅" } else { "" },
            base64_url::encode(&branch_proof.right_id),
            branch_proof.offset(),
            base64_url::encode(&path_hash)
        );
    }

    let leaf_node_id = hash_all_sha256(vec![
        &leaf_proof.data_hash,
        &leaf_proof.offset().to_note_vec(),
    ])?;
    let path_hash_matches_leaf = leaf_node_id == expected_path_hash;

    debug!(
        "  LeafProof: data_hash: {}, offset: {}",
        base64_url::encode(&leaf_proof.data_hash),
        usize::from_be_bytes(leaf_proof.offset)
    );

    if !path_hash_matches_leaf {
        return Err(eyre!(
            "Invalid Leaf Proof: hash mismatch, expected: {:?}, got: {:?}",
            base64_url::encode(&expected_path_hash),
            base64_url::encode(&leaf_proof.data_hash)
        ));
    }

    // Proof nodes (including leaf nodes) always contain their right bound
    let right_bound = leaf_proof.offset() as u128;

    Ok(ValidatePathResult {
        leaf_hash: leaf_proof.data_hash,
        left_bound,
        right_bound,
    })
}

/// Utility method for logging a proof out to the terminal.
pub fn print_debug(proof: &[u8], target_offset: u128) -> Result<([u8; 32], u128, u128), Error> {
    // Split proof into branches and leaf. Leaf is at the end and branches are
    // ordered from root to leaf.
    let (branches, leaf) = proof.split_at(proof.len() - HASH_SIZE - NOTE_SIZE);

    // Deserialize proof.
    let branch_proofs: Vec<BranchProof> = branches
        .chunks(HASH_SIZE * 2 + NOTE_SIZE)
        .map(|b| BranchProof::try_from_proof_slice(b).unwrap())
        .collect();
    let leaf_proof = LeafProof::try_from_proof_slice(leaf)?;

    let mut left_bound: u128 = 0;

    // Validate branches.
    for branch_proof in branch_proofs.iter() {
        // Calculate the id from the proof.
        let path_hash = hash_all_sha256(vec![
            &branch_proof.left_id,
            &branch_proof.right_id,
            &branch_proof.offset().to_note_vec(),
        ])?;

        let offset = branch_proof.offset() as u128;
        let is_right_of_offset = target_offset > offset;

        // Keep track of left and right bounds as we traverse down the proof
        if is_right_of_offset {
            left_bound = offset;
        }

        println!(
            "BranchProof: left: {}{}, right: {}{},offset: {} => path_hash: {}",
            if is_right_of_offset { "" } else { "✅" },
            base64_url::encode(&branch_proof.left_id),
            if is_right_of_offset { "✅" } else { "" },
            base64_url::encode(&branch_proof.right_id),
            branch_proof.offset(),
            base64_url::encode(&path_hash)
        );
    }
    println!(
        "  LeafProof: data_hash: {:?}, offset: {}",
        base64_url::encode(&leaf_proof.data_hash),
        usize::from_be_bytes(leaf_proof.offset)
    );

    let right_bound = leaf_proof.offset() as u128;
    Ok((leaf_proof.data_hash, left_bound, right_bound))
}

/// Validates chunk of data against provided [`Proof`].
pub fn validate_chunk(
    mut root_id: [u8; HASH_SIZE],
    chunk_node: &Node,
    proof: &Proof,
) -> Result<(), Error> {
    match chunk_node {
        Node {
            data_hash: Some(data_hash),
            max_byte_range,
            ..
        } => {
            // Split proof into branches and leaf. Leaf is at the end and branches are ordered
            // from root to leaf.
            let (branches, leaf) = proof
                .proof
                .split_at(proof.proof.len() - HASH_SIZE - NOTE_SIZE);

            // Deserialize proof.
            let branch_proofs: Vec<BranchProof> = branches
                .chunks(HASH_SIZE * 2 + NOTE_SIZE)
                .map(|b| BranchProof::try_from_proof_slice(b).unwrap())
                .collect();
            let leaf_proof = LeafProof::try_from_proof_slice(leaf)?;

            // Validate branches.
            for branch_proof in branch_proofs.iter() {
                // Calculate the id from the proof.
                let id = hash_all_sha256(vec![
                    &branch_proof.left_id,
                    &branch_proof.right_id,
                    &branch_proof.offset().to_note_vec(),
                ])?;

                // Ensure calculated id correct.
                if id != root_id {
                    return Err(eyre!("Invalid Branch Proof"));
                }

                // If the offset from the proof is greater than the offset in the data chunk,
                // then the next id to validate against is from the left.
                root_id = if max_byte_range > &branch_proof.offset() {
                    branch_proof.right_id
                } else {
                    branch_proof.left_id
                }
            }

            // Validate leaf data_hash is correct
            if data_hash != &leaf_proof.data_hash {
                return Err(eyre!("Invalid Leaf Proof: hash mismatch"));
            }

            // Validate leaf root id is correct
            let id = hash_all_sha256(vec![data_hash, &max_byte_range.to_note_vec()])?;
            if id != root_id {
                return Err(eyre!("Invalid Leaf Proof: root mismatch"));
            }
        }
        _ => {
            unreachable!()
        }
    }
    Ok(())
}

/// Generates data chunks from which the calculation of root id starts.
pub fn generate_leaves(
    data: impl Iterator<Item = eyre::Result<ChunkBytes>>,
    chunk_size: usize,
) -> Result<Vec<Node>, Error> {
    let data_chunks = ChunkedIterator::new(data, chunk_size);
    generate_leaves_from_chunks(data_chunks)
}

/// Generates merkle leaves from chunks
pub fn generate_leaves_from_chunks(
    chunks: impl Iterator<Item = eyre::Result<ChunkBytes>>,
) -> Result<Vec<Node>, Error> {
    let mut leaves = Vec::<Node>::new();
    let mut min_byte_range = 0;
    for chunk in chunks {
        let chunk = chunk?;
        let data_hash = hash_sha256(&chunk)?;
        let max_byte_range = min_byte_range + chunk.len();
        let offset = max_byte_range.to_note_vec();
        let id = hash_all_sha256(vec![&data_hash, &offset])?;

        leaves.push(Node {
            id,
            data_hash: Some(data_hash),
            min_byte_range,
            max_byte_range,
            left_child: None,
            right_child: None,
        });
        min_byte_range += &chunk.len();
    }
    Ok(leaves)
}

/// Generates data chunks from which the calculation of root id starts, including the provided address to interleave into the leaf data hash for ingress proofs
/// and_regular can be set to re-use the chunk iterator to produce the "standard" leaves, as well as the ingress proof specific ones for when we validate ingress proofs
pub fn generate_ingress_leaves(
    chunks: impl Iterator<Item = eyre::Result<ChunkBytes>>,
    address: Address,
    and_regular: bool,
) -> Result<(Vec<Node>, Option<Vec<Node>>), Error> {
    let mut leaves = Vec::<Node>::new();
    let mut regular_leaves = Vec::<Node>::new();
    let mut min_byte_range = 0;
    for chunk in chunks {
        let chunk = chunk?;
        let chunk = chunk.as_ref(); // double-binding required
        let data_hash = hash_ingress_sha256(chunk, address)?;
        let max_byte_range = min_byte_range + chunk.len();
        let offset = max_byte_range.to_note_vec();
        let id = hash_all_sha256(vec![&data_hash, &offset])?;

        leaves.push(Node {
            id,
            data_hash: Some(data_hash),
            min_byte_range,
            max_byte_range,
            left_child: None,
            right_child: None,
        });

        if and_regular {
            let data_hash = hash_sha256(chunk)?;
            let id = hash_all_sha256(vec![&data_hash, &offset])?;
            regular_leaves.push(Node {
                id,
                data_hash: Some(data_hash),
                min_byte_range,
                max_byte_range,
                left_child: None,
                right_child: None,
            });
        }

        min_byte_range += &chunk.len();
    }
    Ok((leaves, and_regular.then_some(regular_leaves)))
}

pub struct DataRootLeave {
    pub data_root: H256,
    pub tx_size: usize,
}

/// Generates merkle leaves from data roots
pub fn generate_leaves_from_data_roots(data_roots: &[DataRootLeave]) -> Result<Vec<Node>, Error> {
    let mut leaves = Vec::<Node>::new();
    let mut min_byte_range = 0;
    for data_root in data_roots.iter() {
        let data_root_hash = &data_root.data_root.0;
        let max_byte_range = min_byte_range + data_root.tx_size;
        let offset = max_byte_range.to_note_vec();
        let id = hash_all_sha256(vec![data_root_hash, &offset])?;

        leaves.push(Node {
            id,
            data_hash: Some(*data_root_hash),
            min_byte_range,
            max_byte_range,
            left_child: None,
            right_child: None,
        });

        min_byte_range = max_byte_range;
    }
    Ok(leaves)
}

/// Hashes together a single branch node from a pair of child nodes.
pub fn hash_branch(left: Node, right: Node) -> Result<Node, Error> {
    let max_byte_range = left.max_byte_range.to_note_vec();
    let id = hash_all_sha256(vec![&left.id, &right.id, &max_byte_range])?;
    Ok(Node {
        id,
        data_hash: None,
        min_byte_range: left.max_byte_range,
        max_byte_range: right.max_byte_range,
        left_child: Some(Box::new(left)),
        right_child: Some(Box::new(right)),
    })
}

/// Builds one layer of branch nodes from a layer of child nodes.
pub fn build_layer(nodes: Vec<Node>) -> Result<Vec<Node>, Error> {
    let mut layer = Vec::<Node>::with_capacity(nodes.len() / 2 + (nodes.len() % 2 != 0) as usize);
    let mut nodes_iter = nodes.into_iter();
    while let Some(left) = nodes_iter.next() {
        if let Some(right) = nodes_iter.next() {
            layer.push(hash_branch(left, right).unwrap());
        } else {
            layer.push(left);
        }
    }
    Ok(layer)
}

/// Builds all layers from leaves up to single root node.
pub fn generate_data_root(mut nodes: Vec<Node>) -> Result<Node, Error> {
    while nodes.len() > 1 {
        nodes = build_layer(nodes)?;
    }
    let root = nodes
        .pop()
        .ok_or_eyre("At least one data node is required")?;
    Ok(root)
}

/// Calculates [`Proof`] for each data chunk contained in root [`Node`].
pub fn resolve_proofs(node: Node, proof: Option<Proof>) -> Result<Vec<Proof>, Error> {
    let mut proof = if let Some(proof) = proof {
        proof
    } else {
        Proof {
            offset: 0,
            proof: Vec::new(),
        }
    };
    match node {
        // Leaf
        Node {
            data_hash: Some(data_hash),
            max_byte_range,
            left_child: None,
            right_child: None,
            ..
        } => {
            proof.offset = max_byte_range - 1;
            proof.proof.extend(data_hash);
            proof.proof.extend(max_byte_range.to_note_vec());
            Ok(vec![proof])
        }
        // Branch
        Node {
            data_hash: None,
            min_byte_range,
            left_child: Some(left_child),
            right_child: Some(right_child),
            ..
        } => {
            proof.proof.extend(left_child.id);
            proof.proof.extend(right_child.id);
            proof.proof.extend(min_byte_range.to_note_vec());

            let mut left_proof = resolve_proofs(*left_child, Some(proof.clone()))?;
            let right_proof = resolve_proofs(*right_child, Some(proof))?;
            left_proof.extend(right_proof);
            Ok(left_proof)
        }
        _ => unreachable!(),
    }
}

pub fn hash_sha256(message: &[u8]) -> Result<[u8; 32], Error> {
    let mut hasher = sha::Sha256::new();
    hasher.update(message);
    let result = hasher.finish();
    Ok(result)
}

pub fn hash_ingress_sha256(message: &[u8], address: Address) -> Result<[u8; 32], Error> {
    let mut hasher = sha::Sha256::new();
    hasher.update(message);
    hasher.update(address.as_slice());
    let result = hasher.finish();
    Ok(result)
}

/// Returns a SHA256 hash of the the concatenated SHA256 hashes of a vector of messages.
pub fn hash_all_sha256(messages: Vec<&[u8]>) -> Result<[u8; 32], Error> {
    let hash: Vec<u8> = messages
        .into_iter()
        .flat_map(|m| hash_sha256(m).unwrap())
        .collect();
    let hash = hash_sha256(&hash)?;
    Ok(hash)
}
