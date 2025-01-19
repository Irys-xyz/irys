use crate::{
    generate_data_root, generate_leaves, resolve_proofs, Address, Base64, IrysBlockHeader,
    IrysSignature, IrysTransaction, IrysTransactionHeader, Signature, CONFIG, H256, MAX_CHUNK_SIZE,
};
use alloy_core::primitives::keccak256;

use alloy_signer::utils::secret_key_to_address;
use alloy_signer_local::LocalSigner;
use eyre::Result;
use k256::ecdsa::SigningKey;
use rand::rngs::OsRng;

#[derive(Debug, Clone)]

pub struct IrysSigner {
    pub signer: SigningKey,
    pub chain_id: u64,
    pub chunk_size: usize,
}

/// Encapsulates an Irys API for doing client type things, making transactions,
/// signing them, posting them etc.
impl IrysSigner {
    pub fn mainnet_from_slice(key_slice: &[u8]) -> Self {
        IrysSigner {
            signer: k256::ecdsa::SigningKey::from_slice(key_slice).unwrap(),
            chain_id: CONFIG.irys_chain_id,
            chunk_size: CONFIG.chunk_size.try_into().unwrap(),
        }
    }

    // DO NOT USE IN PROD
    pub fn random_signer() -> Self {
        IrysSigner {
            signer: k256::ecdsa::SigningKey::random(&mut OsRng),
            chain_id: CONFIG.irys_chain_id,
            chunk_size: MAX_CHUNK_SIZE,
        }
    }

    pub fn random_signer_with_chunk_size<T>(chunk_size: T) -> Self
    where
        T: TryInto<usize>,
        <T as TryInto<usize>>::Error: std::fmt::Debug,
    {
        IrysSigner {
            signer: k256::ecdsa::SigningKey::random(&mut OsRng),
            chain_id: CONFIG.irys_chain_id,
            chunk_size: chunk_size.try_into().unwrap(),
        }
    }

    /// Returns the address associated with the signer's signing key
    pub fn address(&self) -> Address {
        secret_key_to_address(&self.signer)
    }

    /// Creates a transaction from a data buffer, optional anchor hash for the
    /// transaction is supported. The txid will not be set until the transaction
    /// is signed with [sign_transaction]
    pub fn create_transaction(
        &self,
        data: Vec<u8>,
        anchor: Option<H256>, //TODO!: more parameters as they are implemented
    ) -> Result<IrysTransaction> {
        let mut transaction = self.merklize(data, self.chunk_size)?;

        // TODO: These should be calculated from some pricing params passed in
        // as a parameter
        transaction.header.perm_fee = Some(1);
        transaction.header.term_fee = 1;

        // Fetch and set last_tx if not provided (primarily for testing).
        let anchor = if let Some(anchor) = anchor {
            anchor
        } else {
            // TODO: Retrieve an acceptable block_hash anchor
            H256::default()
        };
        transaction.header.anchor = anchor;

        Ok(transaction)
    }

    /// signs and sets signature and id.
    pub fn sign_transaction(&self, mut transaction: IrysTransaction) -> Result<IrysTransaction> {
        // Store the signer address
        transaction.header.signer = Address::from_public_key(self.signer.verifying_key());

        // Create the signature hash and sign it
        let prehash = transaction.signature_hash();

        let signature: Signature = self.signer.sign_prehash_recoverable(&prehash)?.into();

        transaction.header.signature = IrysSignature::new(signature);
        // Derive the txid by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        transaction.header.id = H256::from(id);
        Ok(transaction)
    }

    pub fn sign_block_header(&self, mut block_header: IrysBlockHeader) -> Result<IrysBlockHeader> {
        // Store the signer address
        block_header.miner_address = Address::from_public_key(self.signer.verifying_key());

        // Create the signature hash and sign it
        let prehash = block_header.signature_hash()?;

        let signature: Signature = self.signer.sign_prehash_recoverable(&prehash)?.into();

        block_header.signature = IrysSignature::new(signature);
        // Derive the block hash by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        block_header.block_hash = H256::from(id);
        Ok(block_header)
    }

    /// Builds a merkle tree, with a root, including all the proofs for each
    /// chunk.
    fn merklize(&self, data: Vec<u8>, chunk_size: usize) -> Result<IrysTransaction> {
        let chunks = generate_leaves(&data, chunk_size)?;
        let root = generate_data_root(chunks.clone())?;
        let data_root = H256(root.id);
        let proofs = resolve_proofs(root, None)?;

        // Error if the last chunk or proof is zero length.
        let last_chunk = chunks.last().unwrap();
        if last_chunk.max_byte_range == last_chunk.min_byte_range {
            return Err(eyre::eyre!("Last chunk cannot be zero length"));
        }

        Ok(IrysTransaction {
            header: IrysTransactionHeader {
                data_size: data.len() as u64,
                data_root,
                ..Default::default()
            },
            data: Base64(data),
            chunks,
            proofs,
            ..Default::default()
        })
    }
}

impl From<IrysSigner> for LocalSigner<SigningKey> {
    fn from(val: IrysSigner) -> Self {
        LocalSigner::from_signing_key(val.signer)
    }
}

#[cfg(test)]
mod tests {
    use crate::{hash_sha256, validate_chunk, MAX_CHUNK_SIZE};
    use assert_matches::assert_matches;
    use rand::Rng;
    use reth_primitives::transaction::recover_signer;

    use super::IrysSigner;

    #[tokio::test]
    async fn create_and_sign_transaction() {
        // Create 2.5 chunks worth of data *  fill the data with random bytes
        let data_size = (MAX_CHUNK_SIZE as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Create a new Irys API instance
        let irys = IrysSigner::random_signer();

        // Create a transaction from the random bytes
        let mut tx = irys.create_transaction(data_bytes.clone(), None).unwrap();

        // Sign the transaction
        tx = irys.sign_transaction(tx).unwrap();

        assert_eq!(tx.chunks.len(), 3);

        for chunk in &tx.chunks {
            println!(
                "min: {}, max: {}",
                chunk.min_byte_range, chunk.max_byte_range
            );
        }

        println!("{}", serde_json::to_string_pretty(&tx.header).unwrap());

        // Make sure the size of the last chunk is just whatever is left over
        // after chunking the rest of the data at MAX_CHUNK_SIZE intervals.
        let last_chunk = tx.chunks.last().unwrap();
        assert_eq!(
            data_size % MAX_CHUNK_SIZE,
            last_chunk.max_byte_range - last_chunk.min_byte_range
        );

        // Validate the chunk proofs
        for (index, chunk_node) in tx.chunks.iter().enumerate() {
            let min = chunk_node.min_byte_range;
            let max = chunk_node.max_byte_range;

            // Ensure max is within bounds of data_bytes
            if max > data_bytes.len() {
                panic!("Max byte range exceeds the data_bytes length!");
            }

            // Ensure every chunk proof (data_path) is valid
            let root_id = tx.header.data_root.0;
            let proof = tx.proofs[index].clone();
            let proof_result = validate_chunk(root_id, chunk_node, &proof);
            assert_matches!(proof_result, Ok(_));

            // Ensure the data_hash is valid by hashing the chunk data
            let chunk_bytes: &[u8] = &data_bytes[min..max];
            let computed_hash = hash_sha256(chunk_bytes).unwrap();
            let data_hash = chunk_node.data_hash.unwrap();

            assert_eq!(data_hash, computed_hash);
        }

        // Recover the signer as a way to verify the signature
        let prehash = tx.header.signature_hash();
        let sig = tx.header.signature.as_bytes();

        let signer = recover_signer(&sig[..].try_into().unwrap(), prehash.into()).unwrap();

        assert_eq!(signer, tx.header.signer);
    }
}
