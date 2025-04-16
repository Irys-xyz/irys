use irys_config::StorageSubmodulesConfig;
use irys_primitives::CommitmentType;
use irys_types::{
    irys::IrysSigner, CommitmentTransaction, Compact, ConsensusConfig, H256List, IrysBlockHeader,
    NodeConfig, SystemTransactionLedger, H256,
};
use serde::{Deserialize, Serialize};

/// Names for each of the system ledgers as well as their `ledger_id` discriminant
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Compact, PartialOrd, Ord)]
#[repr(u32)]
pub enum SystemLedger {
    /// The commitments ledger, for pledging and staking related transactions
    Commitment = 0,
}

impl Default for SystemLedger {
    fn default() -> Self {
        Self::Commitment
    }
}

impl SystemLedger {
    /// An array of all the System Ledgers, suitable for enumeration
    pub const ALL: [Self; 1] = [Self::Commitment];

    /// Make it possible to iterate over all the System ledgers in order
    pub fn iter() -> impl Iterator<Item = Self> {
        Self::ALL.iter().copied()
    }
    /// get the associated numeric SystemLedger ID
    pub const fn get_id(&self) -> u32 {
        *self as u32
    }
}

impl From<SystemLedger> for u32 {
    fn from(system_ledger: SystemLedger) -> Self {
        system_ledger as Self
    }
}

impl TryFrom<u32> for SystemLedger {
    type Error = &'static str;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Commitment),
            _ => Err("Invalid ledger number"),
        }
    }
}

impl PartialEq<u32> for SystemLedger {
    fn eq(&self, other: &u32) -> bool {
        self.get_id() == *other
    }
}

impl PartialEq<SystemLedger> for u32 {
    fn eq(&self, other: &SystemLedger) -> bool {
        *self == other.get_id()
    }
}

/// Creates a signed pledge commitment transaction
///
/// Constructs a new Pledge-type commitment transaction with the provided anchor
/// value and signs it using the given signer.
///
/// # Arguments
/// * `signer` - The signer to use for transaction signing
/// * `anchor` - The anchor value to include in the commitment
///
/// # Returns
/// The signed commitment transaction
///
/// # Panics
/// Panics if signing the commitment transaction fails
fn create_pledge_commitment_transaction(
    signer: &IrysSigner,
    anchor: H256,
) -> CommitmentTransaction {
    let pledge_commitment = CommitmentTransaction {
        anchor,
        commitment_type: CommitmentType::Pledge,
        ..Default::default()
    };

    let pledge_tx = signer
        .sign_commitment(pledge_commitment)
        .expect("commitment transaction to be signable");

    pledge_tx
}

/// Generates commitment transactions for genesis block
///
/// Creates a stake commitment for the genesis block producer, followed by pledge
/// commitments for each storage submodule configured in the node. This establishes
/// the initial network state with the necessary storage capacity.
///
/// # Arguments
/// * `config` - The node configuration containing signing keys and storage settings
///
/// # Returns
/// A vector of commitment transactions (one stake + multiple pledges)
///
/// # Note
/// This function has the same configuration dependency as [`EpochServiceActor::map_storage_modules_to_partition_assignments`].
/// When updating configuration related to StorageModule/submodule functionality, both functions
/// will need corresponding updates.
///
/// # Panics
/// Panics if fewer than 3 storage submodules are configured, as this is below
/// the minimum required for network operation
pub fn get_genesis_commitments(
    node_config: &NodeConfig,
    consensus_config: &ConsensusConfig,
) -> Vec<CommitmentTransaction> {
    let base_dir = node_config.base_directory.clone();

    // Load the submodule paths from the storage_submodules.toml config
    let storage_submodule_config = StorageSubmodulesConfig::load(base_dir.clone()).unwrap();
    let num_submodules = storage_submodule_config.submodule_paths.len();

    if num_submodules < 3 {
        panic!("There must be at least 3 submodules paths to initiate network genesis");
    }

    let signer = node_config.irys_signer(&consensus_config);

    // Create a stake commitment tx for the genesis block producer.
    let stake_commitment = CommitmentTransaction {
        anchor: H256::default(),
        commitment_type: CommitmentType::Stake,
        ..Default::default()
    };

    let stake_tx = signer
        .sign_commitment(stake_commitment)
        .expect("commitment transaction to be signable");

    let mut commitments = vec![stake_tx.clone()];

    // Gap in configuration vs. functionality: StorageModules can compose multiple
    // submodules for a single partition, but the config doesn't yet express this
    // many-to-one relationship. For testnet, each submodule path is treated as
    // a complete HDD capable of providing all storage for a StorageModule.
    // When the configuration catches up to the StorageModule functionality,
    // this method as well as [`epoch_serve::map_storage_modules_to_partition_assignments()`]
    // will have to be updated.
    let mut anchor = stake_tx.id;
    for _i in 0..num_submodules {
        let pledge_tx = create_pledge_commitment_transaction(&signer, anchor);

        // We have to rotate the anchors on these TX so they produce unique signatures
        // and unique txids
        anchor = pledge_tx.id.clone();

        commitments.push(pledge_tx);
    }

    commitments
}

fn get_or_create_commitment_ledger(
    genesis_block: &mut IrysBlockHeader,
) -> &mut SystemTransactionLedger {
    // Find the commitment ledger or create it if it doesn't exist
    let commitment_ledger_index = genesis_block
        .system_ledgers
        .iter()
        .position(|e| e.ledger_id == SystemLedger::Commitment);

    // If the commitment ledger doesn't exist, create it
    if commitment_ledger_index.is_none() {
        genesis_block.system_ledgers.push(SystemTransactionLedger {
            ledger_id: SystemLedger::Commitment.into(),
            tx_ids: H256List::new(),
        });
    }

    // Get a mutable reference to the commitment ledger
    let commitment_ledger = genesis_block
        .system_ledgers
        .iter_mut()
        .find(|e| e.ledger_id == SystemLedger::Commitment)
        .expect("Commitment ledger should exist at this point");

    commitment_ledger
}

/// Adds genesis commitment transaction IDs to the block header
///
/// Mutates the provided genesis_block by adding commitment IDs
/// to the Commitments system ledger.
///
/// Returns the list of commitment transactions.
pub fn add_genesis_commitments(
    genesis_block: &mut IrysBlockHeader,
    node_config: &NodeConfig,
    consensus_config: &ConsensusConfig,
) -> Vec<CommitmentTransaction> {
    let commitments = get_genesis_commitments(&node_config, &consensus_config);
    let commitment_ledger = get_or_create_commitment_ledger(genesis_block);

    // Add the commitment txids to the commitment ledger one by one
    for txid in commitments.iter().map(|commitment| commitment.id) {
        commitment_ledger.tx_ids.push(txid);
    }

    commitments
}

/// Adds test pledge commitments to the genesis block for testing purposes
///
/// This function creates a specified number of pledge commitments and adds them
/// to the genesis block's commitment ledger. Unlike production pledges, these
/// test pledges are not based on the actual storage_submodules.toml config file,
/// but are simply generated in the requested quantity for testing.
///
/// The function:
/// 1. Creates a single stake commitment
/// 2. Creates the requested number of pledge commitments
/// 3. Adds all commitment IDs to the genesis block's commitment ledger
///
/// # Arguments
/// * `genesis_block` - Mutable reference to the genesis block to modify
/// * `pledge_count` - Number of pledge commitments to create
/// * `config` - Configuration to use for signing commitments
///
/// # Returns
/// A vector containing all commitment transactions (one stake + multiple pledges)
///
/// # Note
/// This function is only available when compiled with test or test-utils features
#[cfg(any(test, feature = "test-utils"))]
pub fn add_test_commitments(
    block_header: &mut IrysBlockHeader,
    pledge_count: u8,
    node_config: &NodeConfig,
    consensus_config: &ConsensusConfig,
) -> Vec<CommitmentTransaction> {
    let signer = node_config.irys_signer(&consensus_config);
    let mut commitments: Vec<CommitmentTransaction> = Vec::new();
    let mut anchor = H256::random();

    if block_header.is_genesis() {
        // Create a stake commitment tx for the genesis block producer.
        let stake_commitment = CommitmentTransaction {
            anchor: H256::default(),
            commitment_type: CommitmentType::Stake,
            ..Default::default()
        };

        let stake_tx = signer
            .sign_commitment(stake_commitment)
            .expect("commitment transaction to be signable");

        anchor = stake_tx.id;
        commitments.push(stake_tx);
    }

    for _i in 0..pledge_count {
        let pledge_tx = create_pledge_commitment_transaction(&signer, anchor);
        // We have to rotate the anchors on these TX so they produce unique signatures
        // and unique txids
        anchor = pledge_tx.id.clone();
        commitments.push(pledge_tx);
    }

    // Get a reference to the Commitment Ledger
    let commitment_ledger = get_or_create_commitment_ledger(block_header);

    // Add the pledge commitment txids to the system ledger one by one
    for commitment_id in commitments.iter().map(|commitment| commitment.id) {
        commitment_ledger.tx_ids.push(commitment_id);
    }

    commitments
}
