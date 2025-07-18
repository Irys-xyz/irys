use crate::{
    address_base58_stringify, optional_string_u64, string_u64, Address, Arbitrary, Base64, Compact,
    ConsensusConfig, IrysSignature, Node, Proof, Signature, TxIngressProof, H256, U256,
};
use alloy_primitives::keccak256;
use alloy_rlp::{Encodable as _, RlpDecodable, RlpEncodable};
use irys_primitives::CommitmentType;
use serde::{Deserialize, Serialize};

pub type IrysTransactionId = H256;

#[derive(
    Clone,
    Debug,
    Eq,
    Serialize,
    Default,
    Deserialize,
    PartialEq,
    Arbitrary,
    Compact,
    RlpEncodable,
    RlpDecodable,
)]
#[rlp(trailing)]
/// Stores deserialized fields from a JSON formatted Irys transaction header.
/// will decode from strings or numeric literals for u64 fields, due to JS's max safe int being 2^53-1 instead of 2^64
/// We include the Irys prefix to differentiate from EVM transactions.
#[serde(rename_all = "camelCase", default)]
pub struct DataTransactionHeader {
    /// A SHA-256 hash of the transaction signature.
    #[rlp(skip)]
    #[rlp(default)]
    // NOTE: both rlp skip AND rlp default must be present in order for field skipping to work
    pub id: H256,

    /// The transaction's version
    pub version: u8,

    /// block_hash of a recent (last 50) blocks or the a recent transaction id
    /// from the signer. Multiple transactions can share the same anchor.
    pub anchor: H256,

    /// The ecdsa/secp256k1 public key of the transaction signer
    #[serde(default, with = "address_base58_stringify")]
    pub signer: Address,

    /// The merkle root of the transactions data chunks
    // #[serde(default, with = "address_base58_stringify")]
    pub data_root: H256,

    /// Size of the transaction data in bytes
    #[serde(with = "string_u64")]
    pub data_size: u64,

    /// Funds the storage of the transaction data during the storage term
    #[serde(with = "string_u64")]
    pub term_fee: u64,

    /// Destination ledger for the transaction, default is 0 - Permanent Ledger
    pub ledger_id: u32,

    /// EVM chain ID - used to prevent cross-chain replays
    #[serde(with = "string_u64")]
    pub chain_id: u64,

    /// Transaction signature bytes
    #[rlp(skip)]
    #[rlp(default)]
    pub signature: IrysSignature,

    #[serde(default, with = "optional_string_u64")]
    pub bundle_format: Option<u64>,

    /// Funds the storage of the transaction for the next 200+ years
    #[serde(default, with = "optional_string_u64")]
    pub perm_fee: Option<u64>,

    /// INTERNAL: Signed ingress proofs used to promote this transaction to the Publish ledger
    /// TODO: put these somewhere else?
    #[rlp(skip)]
    #[rlp(default)]
    #[serde(skip)]
    pub ingress_proofs: Option<TxIngressProof>,
}

impl DataTransactionHeader {
    /// RLP Encoding of Transactions for Signing
    ///
    /// When RLP encoding a transaction for signing, an extra byte is included
    /// for the transaction type. This serves to simplify future parsing and
    /// decoding of RLP-encoded headers.
    ///
    /// When signing a transaction, the prehash is formed by RLP encoding the
    /// transaction's header fields. It's important to note that the prehash
    ///
    /// **excludes** certain fields:
    ///
    /// - **Transaction ID**: This is excluded from the prehash.
    /// - **Signature fields**: These are not part of the prehash.
    /// - **Optional fields**: Any optional fields that are `Option::None` are
    ///                        also excluded from the prehash.
    ///
    /// This method ensures that the transaction signature reflects only the
    /// essential data needed for validation and security purposes.
    pub fn encode_for_signing(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.encode(out)
    }

    pub fn signature_hash(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        self.encode_for_signing(&mut bytes);

        keccak256(&bytes).0
    }

    /// Validates the transaction signature by:
    /// 1.) generating the prehash
    /// 2.) recovering the sender address, and comparing it to the tx's sender (sender MUST be part of the prehash)
    pub fn is_signature_valid(&self) -> bool {
        let id: [u8; 32] = keccak256(self.signature.as_bytes()).into();
        let id_matches_signature = self.id.0 == id;
        id_matches_signature
            && self
                .signature
                .validate_signature(self.signature_hash(), self.signer)
    }
}

/// Wrapper for the underlying DataTransactionHeader fields, this wrapper
/// contains the data/chunk/proof info that is necessary for clients to seed
/// a transactions data to the network.
#[derive(Clone, Default, Debug, Serialize, Deserialize, PartialEq)]
pub struct DataTransaction {
    pub header: DataTransactionHeader,
    // TODO: make this compatible with stream/iterator data sources
    pub data: Option<Base64>,
    #[serde(skip)]
    pub chunks: Vec<Node>,
    #[serde(skip)]
    pub proofs: Vec<Proof>,
}

impl DataTransaction {
    pub fn signature_hash(&self) -> [u8; 32] {
        self.header.signature_hash()
    }
}

impl DataTransactionHeader {
    pub fn new(config: &ConsensusConfig) -> Self {
        Self {
            id: H256::zero(),
            anchor: H256::zero(),
            signer: Address::default(),
            data_root: H256::zero(),
            data_size: 0,
            term_fee: 0,
            perm_fee: None,
            ledger_id: 0,
            bundle_format: None,
            version: 0,
            chain_id: config.chain_id,
            signature: Signature::test_signature().into(),
            ingress_proofs: None,
        }
    }
}

pub type TxPath = Vec<u8>;

/// sha256(tx_path)
pub type TxPathHash = H256;

#[derive(
    Clone,
    Debug,
    Eq,
    Serialize,
    Default,
    Deserialize,
    PartialEq,
    Arbitrary,
    Compact,
    RlpEncodable,
    RlpDecodable,
)]
#[rlp(trailing)]
/// Stores deserialized fields from a JSON formatted commitment transaction.
#[serde(rename_all = "camelCase", default)]
pub struct CommitmentTransaction {
    // NOTE: both rlp skip AND rlp default must be present in order for field skipping to work
    #[rlp(skip)]
    #[rlp(default)]
    /// A SHA-256 hash of the transaction signature.
    pub id: H256,

    /// block_hash of a recent (last 50) blocks or the a recent transaction id
    /// from the signer. Multiple transactions can share the same anchor.
    pub anchor: H256,

    /// The ecdsa/secp256k1 public key of the transaction signer
    #[serde(default, with = "address_base58_stringify")]
    pub signer: Address,

    /// The type of commitment Stake/UnStake Pledge/UnPledge
    pub commitment_type: CommitmentType,

    /// The transaction's version
    pub version: u8,

    /// EVM chain ID - used to prevent cross-chain replays
    #[serde(with = "string_u64")]
    pub chain_id: u64,

    /// Pay the fee required to mitigate tx spam
    #[serde(with = "string_u64")]
    pub fee: u64,

    /// The value being staked, pledged, unstaked or unpledged
    pub value: U256,

    /// Transaction signature bytes
    #[rlp(skip)]
    #[rlp(default)]
    pub signature: IrysSignature,
}

impl CommitmentTransaction {
    /// Create a new CommitmentTransaction with default values from config
    pub fn new(config: &ConsensusConfig) -> Self {
        Self {
            id: H256::zero(),
            anchor: H256::zero(),
            signer: Address::default(),
            commitment_type: CommitmentType::default(),
            version: 0,
            chain_id: config.chain_id,
            fee: 0,
            value: U256::zero(),
            signature: IrysSignature::new(Signature::test_signature()),
        }
    }

    /// Create a new stake transaction with the configured stake fee as value
    pub fn new_stake(config: &ConsensusConfig, anchor: H256, fee: u64) -> Self {
        Self {
            commitment_type: CommitmentType::Stake,
            anchor,
            fee,
            value: config.stake_fee.amount,
            ..Self::new(config)
        }
    }

    /// Create a new unstake transaction with the configured stake fee as value
    pub fn new_unstake(config: &ConsensusConfig, anchor: H256, fee: u64) -> Self {
        Self {
            commitment_type: CommitmentType::Unstake,
            anchor,
            fee,
            value: config.stake_fee.amount,
            ..Self::new(config)
        }
    }

    /// Create a new pledge transaction with dynamic fee based on existing pledge count
    pub fn new_pledge(
        config: &ConsensusConfig,
        anchor: H256,
        fee: u64,
        provider: &impl PledgeDataProvider,
        signer_address: Address,
    ) -> Self {
        let count = provider.pledge_count(signer_address);

        // Calculate: pledge_base_fee / ((count + 1) ^ pledge_decay)
        let value = config
            .pledge_base_fee
            .apply_pledge_decay(count, config.pledge_decay)
            .map(|a| a.amount)
            .unwrap_or(config.pledge_base_fee.amount);

        Self {
            commitment_type: CommitmentType::Pledge,
            anchor,
            fee,
            value,
            ..Self::new(config)
        }
    }

    /// Create a new unpledge transaction with dynamic fee based on existing pledge count
    pub fn new_unpledge(
        config: &ConsensusConfig,
        anchor: H256,
        fee: u64,
        provider: &impl PledgeDataProvider,
        signer_address: Address,
    ) -> Self {
        let count = provider.pledge_count(signer_address);

        // Calculate: pledge_base_fee / ((count + 1) ^ pledge_decay)
        // Note: for unpledge, we use count to match the fee of the most recent pledge
        let value = config
            .pledge_base_fee
            .apply_pledge_decay(count, config.pledge_decay)
            .map(|a| a.amount)
            .unwrap_or(config.pledge_base_fee.amount);

        Self {
            commitment_type: CommitmentType::Unpledge,
            anchor,
            fee,
            value,
            ..Self::new(config)
        }
    }

    /// Rely on RLP encoding for signing
    pub fn encode_for_signing(&self, out: &mut dyn alloy_rlp::BufMut) {
        self.encode(out)
    }

    pub fn signature_hash(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        self.encode_for_signing(&mut bytes);

        keccak256(&bytes).0
    }

    /// Returns the value stored in the transaction
    pub fn commitment_value(&self) -> U256 {
        self.value
    }

    /// Validates the transaction signature by:
    /// 1.) generating the prehash (signature_hash)
    /// 2.) recovering the sender address, and comparing it to the tx's sender (sender MUST be part of the prehash)
    pub fn is_signature_valid(&self) -> bool {
        let id: [u8; 32] = keccak256(self.signature.as_bytes()).into();
        let id_matches_signature = self.id.0 == id;
        id_matches_signature
            && self
                .signature
                .validate_signature(self.signature_hash(), self.signer)
    }
}

// Trait to abstract common behavior
pub trait IrysTransactionCommon {
    fn is_signature_valid(&self) -> bool;
    fn id(&self) -> IrysTransactionId;
    fn total_cost(&self) -> U256;
    fn signer(&self) -> Address;
    fn user_fee(&self) -> U256;

    /// Sign this transaction with the provided signer
    fn sign(self, signer: &crate::irys::IrysSigner) -> Result<Self, eyre::Error>
    where
        Self: Sized;
}

impl IrysTransactionCommon for DataTransactionHeader {
    fn is_signature_valid(&self) -> bool {
        self.is_signature_valid()
    }

    fn id(&self) -> IrysTransactionId {
        self.id
    }

    fn total_cost(&self) -> U256 {
        U256::from(self.perm_fee.unwrap_or(0) + self.term_fee)
    }

    fn signer(&self) -> Address {
        self.signer
    }

    fn user_fee(&self) -> U256 {
        U256::from(self.perm_fee.unwrap_or(0) + self.term_fee)
    }

    fn sign(mut self, signer: &crate::irys::IrysSigner) -> Result<Self, eyre::Error> {
        use crate::Address;
        use alloy_primitives::keccak256;

        // Store the signer address
        self.signer = Address::from_public_key(signer.signer.verifying_key());

        // Create the signature hash and sign it
        let prehash = self.signature_hash();
        let signature: Signature = signer.signer.sign_prehash_recoverable(&prehash)?.into();

        self.signature = IrysSignature::new(signature);

        // Derive the txid by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        self.id = H256::from(id);

        Ok(self)
    }
}

impl IrysTransactionCommon for CommitmentTransaction {
    fn is_signature_valid(&self) -> bool {
        self.is_signature_valid()
    }

    fn id(&self) -> IrysTransactionId {
        self.id
    }

    fn total_cost(&self) -> U256 {
        let additional_fee = match self.commitment_type {
            CommitmentType::Stake => self.value,
            CommitmentType::Pledge => self.value,
            CommitmentType::Unpledge => U256::zero(),
            CommitmentType::Unstake => U256::zero(),
        };
        U256::from(self.fee).saturating_add(additional_fee)
    }

    fn signer(&self) -> Address {
        self.signer
    }

    fn user_fee(&self) -> U256 {
        U256::from(self.fee)
    }

    fn sign(mut self, signer: &crate::irys::IrysSigner) -> Result<Self, eyre::Error> {
        use alloy_primitives::keccak256;

        // Store the signer address
        self.signer = signer.address();

        // Create the signature hash and sign it
        let prehash = self.signature_hash();
        let signature: Signature = signer.signer.sign_prehash_recoverable(&prehash)?.into();

        self.signature = IrysSignature::new(signature);

        // Derive the txid by hashing the signature
        let id: [u8; 32] = keccak256(signature.as_bytes()).into();
        self.id = H256::from(id);

        Ok(self)
    }
}

/// Trait for providing pledge count information for dynamic fee calculation
pub trait PledgeDataProvider {
    /// Returns the number of existing pledges for a given user address
    fn pledge_count(&self, user_address: Address) -> usize;
}

#[cfg(test)]
mod test_helpers {
    use super::*;
    use std::collections::HashMap;

    pub(super) struct MockPledgeProvider {
        pub pledge_counts: HashMap<Address, usize>,
    }

    impl MockPledgeProvider {
        pub(super) fn new() -> Self {
            Self {
                pledge_counts: HashMap::new(),
            }
        }

        pub(super) fn with_pledge_count(mut self, address: Address, count: usize) -> Self {
            self.pledge_counts.insert(address, count);
            self
        }
    }

    impl PledgeDataProvider for MockPledgeProvider {
        fn pledge_count(&self, user_address: Address) -> usize {
            self.pledge_counts.get(&user_address).copied().unwrap_or(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::MockPledgeProvider;
    use super::*;
    use crate::irys::IrysSigner;

    use alloy_rlp::Decodable as _;

    use k256::ecdsa::SigningKey;
    use serde_json;

    #[test]
    fn test_irys_transaction_header_rlp_round_trip() {
        // setup
        let config = ConsensusConfig::testnet();
        let mut header = mock_header(&config);

        // action
        let mut buffer = vec![];
        header.encode(&mut buffer);
        let decoded = DataTransactionHeader::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        // zero out the id and signature, those do not get encoded
        header.id = H256::zero();
        header.signature = IrysSignature::new(Signature::try_from([0_u8; 65].as_slice()).unwrap());
        assert_eq!(header, decoded);
    }

    #[test]
    fn test_commitment_transaction_rlp_round_trip() {
        // setup
        let config = ConsensusConfig::testnet();
        let mut header = mock_commitment_tx(&config);

        // action
        let mut buffer = vec![];
        header.encode(&mut buffer);
        let decoded = CommitmentTransaction::decode(&mut buffer.as_slice()).unwrap();

        // Assert
        // zero out the id and signature, those do not get encoded
        header.id = H256::zero();
        header.signature = IrysSignature::new(Signature::try_from([0_u8; 65].as_slice()).unwrap());
        assert_eq!(header, decoded);
    }

    #[test]
    fn test_irys_transaction_header_serde() {
        // Create a sample DataTransactionHeader
        let config = ConsensusConfig::testnet();
        let original_header = mock_header(&config);

        // Serialize the DataTransactionHeader to JSON
        let serialized =
            serde_json::to_string_pretty(&original_header).expect("Failed to serialize");

        println!("{}", &serialized);
        // Deserialize the JSON back to DataTransactionHeader
        let deserialized: DataTransactionHeader =
            serde_json::from_str(&serialized).expect("Failed to deserialize");

        // Ensure the deserialized struct matches the original
        assert_eq!(original_header, deserialized);
    }

    #[test]
    fn test_commitment_transaction_serde() {
        // Create a sample commitment tx
        let config = ConsensusConfig::testnet();
        let original_tx = mock_commitment_tx(&config);

        // Serialize the commitment tx to JSON
        let serialized = serde_json::to_string_pretty(&original_tx).expect("Failed to serialize");

        println!("{}", &serialized);
        // Deserialize the JSON back to a commitment tx
        let deserialized: CommitmentTransaction =
            serde_json::from_str(&serialized).expect("Failed to deserialize");

        // Ensure the deserialized tx matches the original
        assert_eq!(original_tx, deserialized);
    }

    #[test]
    fn test_tx_encode_and_signing() {
        // setup
        let config = ConsensusConfig::testnet();
        let original_header = mock_header(&config);
        let mut sig_data = Vec::new();
        original_header.encode(&mut sig_data);
        let dec: DataTransactionHeader =
            DataTransactionHeader::decode(&mut sig_data.as_slice()).unwrap();

        // action
        let signer = IrysSigner {
            signer: SigningKey::random(&mut rand::thread_rng()),
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        // Test signing the header directly using the trait method
        let signed_header = dec.sign(&signer).unwrap();
        assert!(signed_header.is_signature_valid());

        // Also test the old way for IrysTransaction
        let tx = DataTransaction {
            header: mock_header(&config),
            ..Default::default()
        };

        let signed_tx = signer.sign_transaction(tx).unwrap();
        assert!(signed_tx.header.is_signature_valid());
    }

    #[test]
    fn test_commitment_tx_encode_and_signing() {
        // setup
        let config = ConsensusConfig::testnet();
        let original_tx = mock_commitment_tx(&config);
        let mut sig_data = Vec::new();
        original_tx.encode(&mut sig_data);
        let _dec = CommitmentTransaction::decode(&mut sig_data.as_slice()).unwrap();

        // action
        let signer = IrysSigner {
            signer: SigningKey::random(&mut rand::thread_rng()),
            chain_id: config.chain_id,
            chunk_size: config.chunk_size,
        };

        // Test using the new trait method
        let signed_tx = original_tx.clone().sign(&signer).unwrap();

        println!(
            "{}",
            serde_json::to_string_pretty(&signed_tx).expect("Failed to serialize")
        );

        assert!(signed_tx.is_signature_valid());
    }

    fn mock_header(config: &ConsensusConfig) -> DataTransactionHeader {
        DataTransactionHeader {
            id: H256::from([255_u8; 32]),
            anchor: H256::from([1_u8; 32]),
            signer: Address::default(),
            data_root: H256::from([3_u8; 32]),
            data_size: 1024,
            term_fee: 100,
            perm_fee: Some(200),
            ledger_id: 1,
            bundle_format: None,
            chain_id: config.chain_id,
            version: 0,
            ingress_proofs: None,
            signature: Signature::test_signature().into(),
        }
    }

    fn mock_commitment_tx(config: &ConsensusConfig) -> CommitmentTransaction {
        let mut tx = CommitmentTransaction::new_stake(config, H256::from([1_u8; 32]), 1);
        tx.id = H256::from([255_u8; 32]);
        tx.signer = Address::default();
        tx.signature = Signature::test_signature().into();
        tx
    }

    #[test]
    fn test_pledge_decay_first_pledge() {
        let mut config = ConsensusConfig::testnet();
        config.pledge_base_fee =
            crate::storage_pricing::Amount::token(rust_decimal_macros::dec!(1.0)).unwrap();
        config.pledge_decay =
            crate::storage_pricing::Amount::percentage(rust_decimal_macros::dec!(0.9)).unwrap();

        let provider = MockPledgeProvider::new();
        let signer_address = Address::default();

        let pledge_tx =
            CommitmentTransaction::new_pledge(&config, H256::zero(), 1, &provider, signer_address);

        // First pledge should be at base fee
        assert_eq!(pledge_tx.value, config.pledge_base_fee.amount);
    }

    #[test]
    fn test_pledge_decay_multiple_pledges() {
        let mut config = ConsensusConfig::testnet();
        config.pledge_base_fee =
            crate::storage_pricing::Amount::token(rust_decimal_macros::dec!(1.0)).unwrap();
        config.pledge_decay =
            crate::storage_pricing::Amount::percentage(rust_decimal_macros::dec!(0.3)).unwrap(); // 30% decay rate

        let signer_address = Address::default();

        // Test with 0 existing pledges (first pledge)
        let provider = MockPledgeProvider::new();
        let pledge_tx =
            CommitmentTransaction::new_pledge(&config, H256::zero(), 1, &provider, signer_address);
        let first_pledge_value = pledge_tx.value;

        // Test with 1 existing pledge (second pledge)
        let provider = MockPledgeProvider::new().with_pledge_count(signer_address, 1);
        let pledge_tx =
            CommitmentTransaction::new_pledge(&config, H256::zero(), 1, &provider, signer_address);
        let second_pledge_value = pledge_tx.value;

        // Test with 2 existing pledges (third pledge)
        let provider = MockPledgeProvider::new().with_pledge_count(signer_address, 2);
        let pledge_tx =
            CommitmentTransaction::new_pledge(&config, H256::zero(), 1, &provider, signer_address);
        let third_pledge_value = pledge_tx.value;

        // Each subsequent pledge should be cheaper
        assert!(
            first_pledge_value > second_pledge_value,
            "First pledge should be more expensive than second"
        );
        assert!(
            second_pledge_value > third_pledge_value,
            "Second pledge should be more expensive than third"
        );
    }
}

#[cfg(test)]
mod pledge_decay_parametrized_tests {
    use super::*;
    use rstest::rstest;
    use rust_decimal_macros::dec;

    #[rstest]
    #[case(0, 20000.0)] // Pledge 1
    #[case(1, 10718.0)] // Pledge 2
    #[case(2, 7441.0)] // Pledge 3
    #[case(3, 5743.0)] // Pledge 4
    #[case(4, 4698.0)] // Pledge 5
    #[case(5, 3987.0)] // Pledge 6
    #[case(6, 3471.0)] // Pledge 7
    #[case(7, 3078.0)] // Pledge 8
    #[case(8, 2768.0)] // Pledge 9
    #[case(9, 2518.0)] // Pledge 10
    #[case(10, 2311.0)] // Pledge 11
    #[case(11, 2137.0)] // Pledge 12
    #[case(12, 1988.0)] // Pledge 13
    #[case(13, 1860.0)] // Pledge 14
    #[case(14, 1748.0)] // Pledge 15
    #[case(15, 1649.0)] // Pledge 16
    #[case(16, 1562.0)] // Pledge 17
    #[case(17, 1483.0)] // Pledge 18
    #[case(18, 1413.0)] // Pledge 19
    #[case(19, 1349.0)] // Pledge 20
    #[case(20, 1291.0)] // Pledge 21
    #[case(21, 1238.0)] // Pledge 22
    #[case(22, 1190.0)] // Pledge 23
    #[case(23, 1145.0)] // Pledge 24
    #[case(24, 1104.0)] // Pledge 25
    fn test_pledge_cost_with_decay(#[case] existing_pledges: usize, #[case] expected_cost: f64) {
        use super::test_helpers::MockPledgeProvider;

        // Setup config with $20,000 base fee and 0.9 decay rate
        let mut config = ConsensusConfig::testnet();
        config.pledge_base_fee = crate::storage_pricing::Amount::token(dec!(20000.0)).unwrap();
        config.pledge_decay = crate::storage_pricing::Amount::percentage(dec!(0.9)).unwrap();

        // Create provider with existing pledge count
        let signer_address = Address::default();
        let provider =
            MockPledgeProvider::new().with_pledge_count(signer_address, existing_pledges);

        // Create a new pledge transaction
        let pledge_tx =
            CommitmentTransaction::new_pledge(&config, H256::zero(), 1, &provider, signer_address);

        // Convert the value back to a dollar amount for comparison
        // Value is in wei (10^18), so divide by 10^18 to get token amount
        let actual_cost_tokens = pledge_tx.value.to_string().parse::<f64>().unwrap() / 1e18;

        // Allow for small rounding differences (0.5% tolerance)
        let tolerance = expected_cost * 0.005;
        assert!(
            (actual_cost_tokens - expected_cost).abs() < tolerance,
            "Pledge {} cost mismatch: expected ${:.2}, got ${:.2} (tolerance: ${:.2})",
            existing_pledges + 1,
            expected_cost,
            actual_cost_tokens,
            tolerance
        );
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum IrysTransactionResponse {
    #[serde(rename = "commitment")]
    Commitment(CommitmentTransaction),

    #[serde(rename = "storage")]
    Storage(DataTransactionHeader),
}

impl From<CommitmentTransaction> for IrysTransactionResponse {
    fn from(tx: CommitmentTransaction) -> Self {
        Self::Commitment(tx)
    }
}

impl From<DataTransactionHeader> for IrysTransactionResponse {
    fn from(tx: DataTransactionHeader) -> Self {
        Self::Storage(tx)
    }
}
