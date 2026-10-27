use crate::chunked::ChunkedIterator;
use crate::data_tree::Geometry;
use crate::{
    Base64, BoundedFee, CommitmentTransaction, DataLedger, DataTransaction, DataTransactionHeader,
    H256, HandshakeRequest, HandshakeRequestV2, IngressProof, IrysAddress, IrysBlockHeader,
    IrysSignature, Signature, versioning::Signable as _,
};
use alloy_core::primitives::keccak256;

use alloy_primitives::Address;
use alloy_signer::utils::secret_key_to_address;
use alloy_signer_local::LocalSigner;
use eyre::Result;
use irys_data_tree::DataTreeBuilder;
use k256::ecdsa::SigningKey;

#[derive(Debug, Clone)]

pub struct IrysSigner {
    pub signer: SigningKey,
    pub chain_id: u64,
    /// The geometry the signer builds data trees with.
    pub data_tree_geometry: Geometry,
}

/// Encapsulates an Irys API for doing client type things, making transactions,
/// signing them, posting them etc.
impl IrysSigner {
    pub fn random_signer(config: &crate::ConsensusConfig) -> Self {
        use rand::rngs::OsRng;

        Self {
            signer: k256::ecdsa::SigningKey::random(&mut OsRng),
            chain_id: config.chain_id,
            data_tree_geometry: config
                .data_tree_geometry()
                .expect("data tree geometry is validated at load"),
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
        let mut transaction = self.merklize(data, store_data)?;

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

    /// Builds the data tree and its root. The `data` iterator does NOT need to
    /// yield correctly sized `Vec` chunks: it is cut into `chunk_size` pieces.
    /// Empty data is an error.
    fn merklize(
        &self,
        data: impl Iterator<Item = eyre::Result<Vec<u8>>>,
        store_data: bool,
    ) -> Result<DataTransaction> {
        let chunk_size = self.data_tree_geometry.chunk_size();
        let mut builder = DataTreeBuilder::new(self.data_tree_geometry);
        // TODO: fix the `data` field so we can use "streaming" data sources
        let mut stored = store_data.then(Vec::new);
        for chunk in ChunkedIterator::new(data, usize::try_from(chunk_size)?) {
            let chunk = chunk?;
            builder.push_chunk(&chunk)?;
            if let Some(stored) = stored.as_mut() {
                stored.extend_from_slice(&chunk);
            }
        }
        let tree = builder.finish()?;

        let mut header = DataTransactionHeader::default();
        header.data_size = tree.data_size();
        header.data_tree_root = tree.data_tree_root().into();
        // Stamp the signer's chain_id so builder-produced txs target the configured chain.
        // `DataTransactionHeader::default()` leaves chain_id = 0, and consensus/ingress now
        // reject a foreign chain_id, so an unstamped tx would be refused on its own chain.
        header.chain_id = self.chain_id;

        Ok(DataTransaction {
            header,
            data: stored.map(Base64),
            tree: Some(tree),
            chunk_size,
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
    use super::IrysSigner;
    use crate::data_tree::verify_data_chunk;
    use crate::versioning::Signable as _;
    use crate::{DataTreeRoot, H256};
    use rand::Rng as _;

    #[tokio::test]
    async fn create_and_sign_transaction() {
        // Create 2.5 chunks worth of data *  fill the data with random bytes
        let config = crate::ConsensusConfig::testing();
        let geometry = config.data_tree_geometry().unwrap();
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

        let chunks = tx.data_chunks().unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(tx.header.data_size, data_size as u64);

        // Make sure the size of the last chunk is just whatever is left over
        // after chunking the rest of the data at chunk_size intervals.
        assert_eq!(
            data_size % config.chunk_size as usize,
            chunks.last().unwrap().bytes.0.len()
        );

        // Every chunk is its slice of the data, and its proof opens it.
        for (chunk_index, chunk) in chunks.iter().enumerate() {
            let start = chunk_index * config.chunk_size as usize;
            let end = (start + config.chunk_size as usize).min(data_size);
            assert_eq!(chunk.bytes.0, data_bytes[start..end]);
            assert_eq!(
                verify_data_chunk(
                    &geometry,
                    tx.header.data_tree_root,
                    tx.header.data_size,
                    chunk.tx_offset,
                    &chunk.bytes.0,
                    &chunk.data_path.0,
                ),
                Ok(())
            );
        }

        // Recover the signer as a way to verify the signature
        let prehash = tx.header.signature_hash();
        let signer = tx.header.signature.recover_signer(prehash).unwrap();

        assert_eq!(signer, tx.header.signer);
    }

    #[test]
    fn empty_data_is_an_error() {
        let config = crate::ConsensusConfig::testing();
        let signer = IrysSigner::random_signer(&config);
        assert!(signer.create_transaction(Vec::new(), H256::zero()).is_err());
    }

    /// Parse one lowercase hex string from a vector file.
    fn hex_bytes(value: &serde_json::Value) -> Vec<u8> {
        hex::decode(value.as_str().expect("hex string")).expect("valid hex")
    }

    fn read_vectors(file_name: &str) -> serde_json::Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../data-tree/vectors")
            .join(file_name);
        serde_json::from_slice(&std::fs::read(path).expect("vector file")).expect("vector JSON")
    }

    /// `merklize` builds the tree the crate's vectors fix: the testing
    /// geometry (`chunk_size` 32, 4 segments), bytes `i mod 251`, 112 bytes.
    /// The data is fed in uneven pieces, so the cut into chunks is checked too.
    #[test]
    fn merklize_matches_the_crate_vectors() {
        const DATA_SIZE: u64 = 112;
        const PATTERN_MODULUS: u64 = 251;
        let config = crate::ConsensusConfig::testing();
        let geometry = config.data_tree_geometry().unwrap();
        assert_eq!(
            geometry.chunk_size(),
            irys_data_tree::Geometry::TESTING.chunk_size()
        );
        assert_eq!(
            geometry.segments_per_chunk(),
            irys_data_tree::Geometry::TESTING.segments_per_chunk()
        );
        let data: Vec<u8> = (0..DATA_SIZE)
            .map(|byte| (byte % PATTERN_MODULUS) as u8)
            .collect();
        let pieces = [&data[..5], &data[5..70], &data[70..]]
            .map(|piece| Ok(piece.to_vec()))
            .into_iter();
        let signer = IrysSigner::random_signer(&config);
        let tx = signer
            .create_transaction_from_iter(pieces, H256::zero(), true)
            .unwrap();
        assert_eq!(tx.header.data_size, DATA_SIZE);

        let roots = read_vectors("roots_testing.json");
        let root_case = roots["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|case| case["pattern"] == "mod251" && case["data_size"] == DATA_SIZE)
            .expect("root vector for 112 bytes");
        let expected_root = H256::from_slice(&hex_bytes(&root_case["data_tree_root"]));
        assert_eq!(tx.header.data_tree_root, DataTreeRoot(expected_root));

        let proofs = read_vectors("chunk_proofs.json");
        let proof_cases: Vec<_> = proofs["cases"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|case| {
                case["geometry"]["chunk_size"] == geometry.chunk_size()
                    && case["pattern"] == "mod251"
                    && case["data_size"] == DATA_SIZE
            })
            .collect();
        let chunks = tx.data_chunks().unwrap();
        assert_eq!(proof_cases.len(), chunks.len());
        for case in proof_cases {
            let chunk_index = u32::try_from(case["chunk_index"].as_u64().unwrap()).unwrap();
            let expected_proof = hex_bytes(&case["proof"]);
            assert_eq!(
                tx.chunk_proof(crate::TxChunkOffset(chunk_index)).unwrap().0,
                expected_proof,
                "chunk {chunk_index}"
            );
            assert_eq!(chunks[chunk_index as usize].data_path.0, expected_proof);
        }
    }
}
