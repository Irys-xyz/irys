use crate::{
    generate_data_root, generate_leaves, resolve_proofs, versioning::Signable as _, Base64,
    BoundedFee, CommitmentTransaction, DataLedger, DataTransaction, DataTransactionHeader,
    HandshakeRequest, HandshakeRequestV2, IngressProof, IrysAddress, IrysBlockHeader,
    IrysSignature, Signature, H256,
};
use alloy_core::primitives::keccak256;

use alloy_primitives::Address;
use alloy_signer::utils::secret_key_to_address;
use alloy_signer_local::LocalSigner;
use eyre::Result;
use k256::ecdsa::SigningKey;

#[derive(Debug, Clone)]

pub struct IrysSigner {
    pub signer: SigningKey,
    pub chain_id: u64,
    pub chunk_size: u64,
}

/// Encapsulates an Irys API for doing client type things, making transactions,
/// signing them, posting them etc.
impl IrysSigner {
    pub fn random_signer(config: &crate::ConsensusConfig) -> Self {
        use rand::rngs::OsRng;

        Self {
            signer: k256::ecdsa::SigningKey::random(&mut OsRng),
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        }
    }

    /// Returns the address associated with the signer's signing key
    pub fn address(&self) -> IrysAddress {
        secret_key_to_address(&self.signer).into()
    }

    pub fn alloy_address(&self) -> Address {
        secret_key_to_address(&self.signer)
    }

    /// Creates a transaction from a data iterator (which can yield any size Vec), with an optional anchor and flag for if the input data should be stored in the `data` field.
    /// The txid will not be set until the transaction is signed with [`IrysSigner::sign_transaction`]
    pub fn create_transaction_from_iter(
        &self,
        data: impl Iterator<Item = eyre::Result<Vec<u8>>>,
        anchor: H256,     //TODO!: more parameters as they are implemented
        store_data: bool, // whether we should store the data from the iterator in the tx's `data` field
    ) -> Result<DataTransaction> {
        let mut transaction = self.merklize(data, self.chunk_size as usize, store_data)?;

        // TODO: These should be calculated from some pricing params passed in
        // as a parameter
        transaction.header.perm_fee = Some(BoundedFee::from(1_u64));
        transaction.header.term_fee = BoundedFee::from(1_u64);

        transaction.header.anchor = anchor;

        Ok(transaction)
    }

    /// Creates a transaction from a data buffer - which it stores in .data -  with an optional anchor.
    /// The txid will not be set until the transaction is signed with [`IrysSigner::sign_transaction`]
    pub fn create_transaction(&self, data: Vec<u8>, anchor: H256) -> Result<DataTransaction> {
        self.create_transaction_from_iter(vec_to_chunk_iter(data), anchor, true)
    }

    /// Creates a transaction from a data buffer - which it DOES NOT store in .data - with an optional anchor.
    /// The txid will not be set until the transaction is signed with [`IrysSigner::sign_transaction`]
    pub fn create_transaction_discard_data(
        &self,
        data: Vec<u8>,
        anchor: H256,
    ) -> Result<DataTransaction> {
        self.create_transaction_from_iter(vec_to_chunk_iter(data), anchor, false)
    }
    /// Creates a transaction with explicit fee and ledger parameters
    pub fn create_transaction_with_fees(
        &self,
        data: Vec<u8>,
        anchor: H256,
        ledger: DataLedger,
        term_fee: BoundedFee,
        perm_fee: Option<BoundedFee>,
    ) -> Result<DataTransaction> {
        let mut transaction = self.create_transaction(data, anchor)?;

        // Set the provided fees
        transaction.header.ledger_id = ledger as u32;
        transaction.header.term_fee = term_fee;
        transaction.header.perm_fee = perm_fee;

        Ok(transaction)
    }

    /// Creates a publish transaction with the provided perm and term fees
    pub fn create_publish_transaction(
        &self,
        data: Vec<u8>,
        anchor: H256,
        perm_price: BoundedFee,
        term_price: BoundedFee,
    ) -> Result<DataTransaction> {
        self.create_transaction_with_fees(
            data,
            anchor,
            DataLedger::Publish,
            term_price,       // Term storage fee
            Some(perm_price), // Permanent storage fee
        )
    }

    /// signs and sets signature and id.
    pub fn sign_transaction(&self, mut transaction: DataTransaction) -> Result<DataTransaction> {
        // Store the signer address
        transaction.header.signer = IrysAddress::from_public_key(self.signer.verifying_key());

        // Create the signature hash and sign it
        let prehash = transaction.signature_hash();

        let signature: Signature = self.signer.sign_prehash_recoverable(&prehash)?.into();

        transaction.header.signature = IrysSignature::new(signature);
        // Derive the txid by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        transaction.header.id = H256::from(id);
        Ok(transaction)
    }

    pub fn sign_commitment(&self, commitment: &mut CommitmentTransaction) -> Result<()> {
        // Store the signer address
        commitment.set_signer(IrysAddress::from_public_key(self.signer.verifying_key()));

        // Create the signature hash and sign it
        let prehash = commitment.signature_hash();
        let signature: Signature = self.signer.sign_prehash_recoverable(&prehash)?.into();

        commitment.set_signature(IrysSignature::new(signature));

        // Derive the txid by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        commitment.set_id(H256::from(id));
        Ok(())
    }

    pub fn sign_block_header(&self, block_header: &mut IrysBlockHeader) -> Result<()> {
        // Store the signer address
        block_header.miner_address = IrysAddress::from_public_key(self.signer.verifying_key());

        // Create the signature hash and sign it
        let prehash = block_header.signature_hash();
        let signature: Signature = self.signer.sign_prehash_recoverable(&prehash)?.into();
        block_header.signature = IrysSignature::new(signature);

        // Derive the block hash by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        block_header.block_hash = H256::from(id);
        Ok(())
    }

    pub fn sign_p2p_handshake_v1(&self, handshake_message: &mut HandshakeRequest) -> Result<()> {
        // Store the signer address
        handshake_message.mining_address = self.address();

        // Create the signature hash and sign it
        let prehash = handshake_message.signature_hash();
        let signature: Signature = self.signer.sign_prehash_recoverable(&prehash)?.into();
        handshake_message.signature = IrysSignature::new(signature);

        Ok(())
    }

    pub fn sign_p2p_handshake_v2(&self, handshake_message: &mut HandshakeRequestV2) -> Result<()> {
        // Store the signer address
        handshake_message.mining_address = self.address();

        // Create the signature hash and sign it
        let prehash = handshake_message.signature_hash();
        let signature: Signature = self.signer.sign_prehash_recoverable(&prehash)?.into();
        handshake_message.signature = IrysSignature::new(signature);

        Ok(())
    }

    pub fn sign_ingress_proof(&self, proof: &mut IngressProof) -> Result<()> {
        let prehash = proof.signature_hash();
        let signature: Signature = self.signer.sign_prehash_recoverable(&prehash)?.into();
        proof.signature = IrysSignature::new(signature);

        Ok(())
    }

    /// Builds a merkle tree, with a root, including all the proofs for each
    /// chunk. The `data` vec does NOT need to yield correctly sized `Vec` chunks.
    fn merklize(
        &self,
        data: impl Iterator<Item = eyre::Result<Vec<u8>>>,
        chunk_size: usize,
        store_data: bool,
    ) -> Result<DataTransaction> {
        // TODO: fix the `data` field so we can use "streaming" data sources
        let (data, chunks) = if store_data {
            let data: eyre::Result<Vec<Vec<u8>>> = data.collect();
            let data = data?;
            let chunks = generate_leaves(data.clone().into_iter().map(Ok), chunk_size)?;
            (Some(data.into_iter().flatten().collect()), chunks)
        } else {
            let chunks = generate_leaves(data, chunk_size)?;
            (None, chunks)
        };
        let root = generate_data_root(chunks.clone())?;
        let data_root = H256(root.id);
        let proofs = resolve_proofs(root, None)?;

        // Error if the last chunk or proof is zero length.
        let last_chunk = chunks.last().expect("Unable to get last chunk");
        if last_chunk.max_byte_range == last_chunk.min_byte_range {
            return Err(eyre::eyre!("Last chunk cannot be zero length"));
        }

        let mut header = DataTransactionHeader::default();
        header.data_size = chunks
            .last()
            .expect("Unable to get last chunk")
            .max_byte_range as u64;
        header.data_root = data_root;

        Ok(DataTransaction {
            header,
            data: data.map(Base64),
            chunks,
            proofs,
        })
    }
}

impl From<IrysSigner> for LocalSigner<SigningKey> {
    fn from(val: IrysSigner) -> Self {
        Self::from_signing_key(val.signer)
    }
}

pub fn vec_to_chunk_iter(data: Vec<u8>) -> std::iter::Once<eyre::Result<Vec<u8>>> {
    std::iter::once(Ok(data))
}

#[cfg(test)]
mod tests {
    use crate::versioning::Signable as _;
    use crate::{hash_sha256, validate_chunk, IrysAddress, H256};
    use rand::Rng as _;
    use reth_primitives_traits::crypto::secp256k1::recover_signer;

    use super::IrysSigner;

    #[tokio::test]
    async fn create_and_sign_transaction() {
        // Create 2.5 chunks worth of data *  fill the data with random bytes
        let config = crate::ConsensusConfig::testing();
        let data_size = (config.chunk_size as f64 * 2.5).round() as usize;
        let mut data_bytes = vec![0_u8; data_size];
        rand::thread_rng().fill(&mut data_bytes[..]);

        // Create a new Irys API instance
        let irys = IrysSigner::random_signer(&config);

        // Create a transaction from the random bytes
        let mut tx = irys
            .create_transaction(data_bytes.clone(), H256::zero())
            .unwrap();

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
            data_size % config.chunk_size as usize,
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
            assert!(proof_result.is_ok());

            // Ensure the data_hash is valid by hashing the chunk data
            let chunk_bytes: &[u8] = &data_bytes[min..max];
            let computed_hash = hash_sha256(chunk_bytes);
            let data_hash = chunk_node.data_hash.unwrap();

            assert_eq!(data_hash, computed_hash);
        }

        // Recover the signer as a way to verify the signature
        let prehash = tx.header.signature_hash();
        let sig = tx.header.signature.as_bytes();

        let signer: IrysAddress = recover_signer(&sig[..].try_into().unwrap(), prehash.into())
            .unwrap()
            .into();

        assert_eq!(signer, tx.header.signer);
    }
}
