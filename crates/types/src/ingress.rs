use std::io::Read;

use alloy_primitives::{Address, Parity, U256};
use alloy_signer::Signature;
use bytes::Buf;
use k256::ecdsa::VerifyingKey;
use rand::Rng;
use reth_codecs::Compact;
use reth_db::DatabaseError;
use reth_db_api::table::{Compress, Decompress};
use reth_primitives::recover_signer_unchecked;
use serde::{Deserialize, Serialize};

use crate::{generate_leaves, hash_sha256, irys::IrysSigner, MAX_CHUNK_SIZE};

use crate::{generate_data_root, generate_interleaved_leaves, Node, H256, IRYS_CHAIN_ID};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Compact)]
pub struct IngressProof {
    pub tx_id: H256,    // IrysTxId
    pub owner: Address, // address
    pub signature: Signature,
    pub block_relative_tx_offset: u128,
    pub data_root: H256,
    pub data_size: u128,
    pub proof: H256,
}

// TODO: either a.) move tables into type or b.) somehow move the ingress proof type into database without causing a circular dep
// for speed I just copied the macro here
macro_rules! impl_compression_for_compact {
	($($name:tt),+) => {
			$(
					impl Compress for $name {
							type Compressed = Vec<u8>;

							fn compress_to_buf<B: bytes::BufMut + AsMut<[u8]>>(self, buf: &mut B) {
									let _ = Compact::to_compact(&self, buf);
							}
					}

					impl Decompress for $name {
							fn decompress(value: &[u8]) -> Result<$name, DatabaseError> {
									let (obj, _) = Compact::from_compact(value, value.len());
									Ok(obj)
							}
					}
			)+
	};
}

impl_compression_for_compact!(IngressProof);

impl Default for IngressProof {
    fn default() -> Self {
        Self {
            tx_id: Default::default(),
            owner: Default::default(),
            signature: Signature::new(U256::ZERO, U256::ZERO, Parity::Parity(false)),
            block_relative_tx_offset: Default::default(),
            data_root: Default::default(),
            data_size: Default::default(),
            proof: Default::default(),
        }
    }
}

// const OWNER_LENGTH: usize = 20;

// impl Compact for IngressProof {
//     fn to_compact<B>(&self, buf: &mut B) -> usize
//     where
//         B: bytes::BufMut + AsMut<[u8]>,
//     {
//         let mut len = 0;
//         len += self.tx_id.to_compact(buf);
//         // buf.put(self.owner.as_slice());
//         // len += 20;
//         len += self.owner.to_compact(buf);
//         len += self.signature.to_compact(buf);
//         len += self.block_relative_tx_offset.to_compact(buf);
//         len += self.data_root.to_compact(buf);
//         len += self.data_size.to_compact(buf);
//         len += self.proof.to_compact(buf);
//         len
//     }

//     fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
//         // todo!()
//             let (tx_id, buf)= H256::from_compact(buf, H256::len_bytes());
//             let mut owner = [0; OWNER_LENGTH];
//             buf.read_exact(&mut owner);
//             // buf.advance(cnt);
//             let (signature)

//         Self {
//             tx_id = H256::from_compact(buf, H256::len_bytes()),
//             owner: todo!(),
//             signature: todo!(),
//             block_relative_tx_offset: todo!(),
//             data_root: todo!(),
//             data_size: todo!(),
//             proof: todo!(),

//         }

//     }
// }

pub fn generate_ingress_proof_tree(data: Vec<u8>, address: Address) -> eyre::Result<Node> {
    let chunks = generate_interleaved_leaves(data.clone(), address.as_slice())?;
    let root = generate_data_root(chunks.clone())?;
    Ok(root)
}

pub fn generate_ingress_proof(signer: IrysSigner, data: Vec<u8>) -> eyre::Result<IngressProof> {
    let root = generate_ingress_proof_tree(data, signer.address())?;
    let proof: [u8; 32] = root.id.clone();
    let mut signature: Signature = signer.signer.sign_prehash_recoverable(&proof)?.into();
    signature = signature.with_chain_id(IRYS_CHAIN_ID);
    Ok(IngressProof {
        tx_id: H256::random(),
        owner: signer.address(),
        signature,
        block_relative_tx_offset: 0,
        data_root: H256::random(),
        data_size: 0,
        proof: H256(root.id.clone()),
    })
}

pub fn verify_ingress_proof(proof: IngressProof, data: Vec<u8>) -> eyre::Result<bool> {
    let prehash = proof.proof.0;
    let sig = proof.signature.as_bytes();

    let recovered_address = recover_signer_unchecked(&sig, &prehash)?;
    if recovered_address != proof.owner {
        return Ok(false);
    }
    // re-compute the proof
    let recomputed = generate_ingress_proof_tree(data, recovered_address)?;
    // make sure they match
    Ok(recomputed.id == prehash)
}

mod tests {
    use rand::Rng;

    use crate::{
        generate_leaves, hash_sha256, ingress::verify_ingress_proof, irys::IrysSigner,
        MAX_CHUNK_SIZE,
    };

    use super::generate_ingress_proof;

    #[test]
    fn interleave_test() -> eyre::Result<()> {
        let data_size = (MAX_CHUNK_SIZE as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);
        let signer = IrysSigner::random_signer();
        let leaves = generate_leaves(data_bytes.clone())?;
        let interleave_value = signer.address();
        let interleave_hash = hash_sha256(&interleave_value.0 .0)?;

        // interleave the interleave hash with the leaves
        // TODO improve
        let mut interleaved = Vec::with_capacity(leaves.len() * 2);
        for leaf in leaves.iter() {
            interleaved.push(interleave_hash);
            interleaved.push(leaf.id)
        }

        let interleaved_hash = hash_sha256(interleaved.concat().as_slice())?;
        Ok(())
    }

    #[test]
    fn basic() -> eyre::Result<()> {
        let data_size = (MAX_CHUNK_SIZE as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);
        let signer = IrysSigner::random_signer();
        let proof = generate_ingress_proof(signer.clone(), data_bytes.clone())?;

        assert!(verify_ingress_proof(proof.clone(), data_bytes.clone())?);
        let mut reversed = data_bytes.clone();
        reversed.reverse();
        assert!(!verify_ingress_proof(proof, reversed)?);

        Ok(())
    }
}
