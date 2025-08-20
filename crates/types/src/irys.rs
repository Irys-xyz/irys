use crate::storage_pricing::TERM_FEE;
use crate::{
    generate_data_root, generate_leaves, resolve_proofs, Address, Base64, CommitmentTransaction,
    DataLedger, DataTransaction, DataTransactionHeader, IrysBlockHeader, IrysSignature, Signature,
    VersionRequest, H256, U256,
};
use alloy_core::primitives::keccak256;

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
    ) -> Result<DataTransaction> {
        let mut transaction = self.merklize(data, self.chunk_size as usize)?;

        // Compute realistic fees using config defaults and pricing helpers
        // Term fee: placeholder constant used across pricing until full dynamic pricing is wired
        let term_fee = TERM_FEE;

        // Use consensus defaults and genesis price to approximate perm fee (suitable for tests)
        let config = crate::ConsensusConfig::testing();
        let bytes_to_store = U256::from(transaction.header.data_size);

        // Calculate base network fee for permanent storage
        let cost_per_gb_per_year = config
            .annual_cost_per_gb
            .cost_per_replica(config.safe_minimum_number_of_years, config.decay_rate)?
            .replica_count(config.number_of_ingress_proofs)?;
        let base_network_fee =
            cost_per_gb_per_year.base_network_fee(bytes_to_store, config.genesis_price)?;

        // Add ingress proof rewards to get total perm_fee
        let perm_fee = base_network_fee.add_ingress_proof_rewards(
            term_fee,
            config.number_of_ingress_proofs,
            config.immediate_tx_inclusion_reward_percent,
        )?;

        // Set computed fees on the header
        transaction.header.term_fee = term_fee;
        transaction.header.perm_fee = Some(perm_fee.amount);

        // Fetch and set last_tx if not provided (primarily for testing).
        #[expect(clippy::manual_unwrap_or_default, reason = "TODO")]
        let anchor = if let Some(anchor) = anchor {
            anchor
        } else {
            // TODO: Retrieve an acceptable block_hash anchor
            H256::default()
        };
        transaction.header.anchor = anchor;

        Ok(transaction)
    }

    /// Creates a transaction with explicit fee and ledger parameters
    pub fn create_transaction_with_fees(
        &self,
        data: Vec<u8>,
        anchor: Option<H256>,
        ledger: DataLedger,
        term_fee: U256,
        perm_fee: Option<U256>,
    ) -> Result<DataTransaction> {
        let mut transaction = self.merklize(data, self.chunk_size as usize)?;

        // Set the provided fees directly as U256
        transaction.header.ledger_id = ledger as u32;
        transaction.header.term_fee = term_fee;
        transaction.header.perm_fee = perm_fee;

        // Fetch and set anchor if not provided
        let anchor = anchor.unwrap_or_default();
        transaction.header.anchor = anchor;

        Ok(transaction)
    }

    /// Creates a publish transaction with the provided perm and term fees
    pub fn create_publish_transaction(
        &self,
        data: Vec<u8>,
        anchor: Option<H256>,
        perm_price: U256,
        term_price: U256,
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

    pub fn sign_commitment(
        &self,
        mut commitment: CommitmentTransaction,
    ) -> Result<CommitmentTransaction> {
        // Store the signer address
        commitment.signer = Address::from_public_key(self.signer.verifying_key());

        // Create the signature hash and sign it
        let prehash = commitment.signature_hash();

        let signature: Signature = self.signer.sign_prehash_recoverable(&prehash)?.into();

        commitment.signature = IrysSignature::new(signature);

        // Derive the txid by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        commitment.id = H256::from(id);
        Ok(commitment)
    }

    pub fn sign_block_header(&self, block_header: &mut IrysBlockHeader) -> Result<()> {
        // Store the signer address
        block_header.miner_address = Address::from_public_key(self.signer.verifying_key());

        // Create the signature hash and sign it
        let prehash = block_header.signature_hash();
        let signature: Signature = self.signer.sign_prehash_recoverable(&prehash)?.into();
        block_header.signature = IrysSignature::new(signature);

        // Derive the block hash by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        block_header.block_hash = H256::from(id);
        Ok(())
    }

    pub fn sign_p2p_handshake(&self, handshake_message: &mut VersionRequest) -> Result<()> {
        // Store the signer address
        handshake_message.mining_address = self.address();

        // Create the signature hash and sign it
        let prehash = handshake_message.signature_hash();
        let signature: Signature = self.signer.sign_prehash_recoverable(&prehash)?.into();
        handshake_message.signature = IrysSignature::new(signature);

        Ok(())
    }

    /// Builds a merkle tree, with a root, including all the proofs for each
    /// chunk.
    fn merklize(&self, data: Vec<u8>, chunk_size: usize) -> Result<DataTransaction> {
        // TODO: fix the `data` field so we can use "streaming" data sources & remove the clone
        let chunks = generate_leaves(vec![data.clone()].into_iter().map(Ok), chunk_size)?;
        let root = generate_data_root(chunks.clone())?;
        let data_root = H256(root.id);
        let proofs = resolve_proofs(root, None)?;

        // Error if the last chunk or proof is zero length.
        let last_chunk = chunks.last().unwrap();
        if last_chunk.max_byte_range == last_chunk.min_byte_range {
            return Err(eyre::eyre!("Last chunk cannot be zero length"));
        }

        Ok(DataTransaction {
            header: DataTransactionHeader {
                data_size: data.len() as u64,
                data_root,
                ..Default::default()
            },
            data: Some(Base64(data)),
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

#[cfg(test)]
mod tests {
    use crate::{hash_sha256, validate_chunk};
    use rand::Rng as _;
    use reth_primitives::transaction::recover_signer;

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

        let signer = recover_signer(&sig[..].try_into().unwrap(), prehash.into()).unwrap();

        assert_eq!(signer, tx.header.signer);
    }

    #[test]
    fn create_transaction_sets_realistic_fees() {
        let config = crate::ConsensusConfig::testing();
        let irys = IrysSigner::random_signer(&config);
        let data_bytes = vec![1_u8; 1234];

        let tx = irys.create_transaction(data_bytes, None).unwrap();

        // Term fee should equal the constant TERM_FEE used by pricing helpers
        assert_eq!(tx.header.term_fee, crate::storage_pricing::TERM_FEE);

        // Perm fee should equal base_network_fee + ingress proof rewards
        let bytes_to_store = crate::U256::from(tx.header.data_size);
        let cost_per_gb_per_year = config
            .annual_cost_per_gb
            .cost_per_replica(config.safe_minimum_number_of_years, config.decay_rate)
            .unwrap()
            .replica_count(config.number_of_ingress_proofs)
            .unwrap();
        let base_network_fee = cost_per_gb_per_year
            .base_network_fee(bytes_to_store, config.genesis_price)
            .unwrap();
        let expected_perm = base_network_fee
            .add_ingress_proof_rewards(
                tx.header.term_fee,
                config.number_of_ingress_proofs,
                config.immediate_tx_inclusion_reward_percent,
            )
            .unwrap()
            .amount;

        assert_eq!(tx.header.perm_fee, Some(expected_perm));
    }

    #[test]
    fn publish_fee_charges_accepts_computed_perm_fee() {
        let config = crate::ConsensusConfig::testing();
        let irys = IrysSigner::random_signer(&config);
        let data = vec![7_u8; 1024];

        let tx = irys.create_transaction(data, None).unwrap();
        let perm = tx.header.perm_fee.expect("perm_fee set");
        let term = tx.header.term_fee;

        // Validate perm_fee against distribution logic
        crate::transaction::fee_distribution::PublishFeeCharges::new(perm, term, &config)
            .expect("perm_fee should be sufficient for ingress rewards + base cost");
    }
}
