use irys_config::submodules::StorageSubmodulesConfig;
use irys_types::{
    CommitmentTransaction, Config, H256, H256List, IrysBlockHeader, SystemLedger,
    SystemTransactionLedger, U256, irys::IrysSigner, transaction::PledgeDataProvider,
};

/// Creates a signed pledge commitment transaction
///
/// Constructs a new Pledge-type commitment transaction with the provided anchor
/// value and signs it using the given signer.
///
/// # Arguments
/// * `signer` - The signer to use for transaction signing
/// * `anchor` - The anchor value to include in the commitment
/// * `config` - The configuration containing consensus parameters
/// * `provider` - The pledge data provider for fee calculation
///
/// # Returns
/// The signed commitment transaction
///
/// # Panics
/// Panics if signing the commitment transaction fails
async fn create_pledge_commitment_transaction(
    signer: &IrysSigner,
    anchor: H256,
    config: &Config,
    provider: &impl PledgeDataProvider,
) -> CommitmentTransaction {
    let mut pledge_commitment =
        CommitmentTransaction::new_pledge(&config.consensus, anchor, provider, signer.address())
            .await;

    signer
        .sign_commitment(&mut pledge_commitment)
        .expect("commitment transaction to be signable");
    pledge_commitment
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
/// This function has the same configuration dependency as EpochSnapshot::map_storage_modules_to_partition_assignments.
/// When updating configuration related to StorageModule/submodule functionality, both functions
/// will need corresponding updates.
///
/// # Panics
/// Panics if fewer than 3 storage submodules are configured, as this is below
/// the minimum required for network operation
pub async fn get_genesis_commitments(config: &Config) -> Vec<CommitmentTransaction> {
    let base_dir = config.node_config.base_directory.clone();

    // Load the submodule paths from the storage_submodules.toml config
    let storage_submodule_config = StorageSubmodulesConfig::load(base_dir).unwrap();
    let num_submodules = storage_submodule_config.submodule_paths.len();

    if num_submodules < 3 {
        panic!("There must be at least 3 submodules paths to initiate network genesis");
    }

    let signer = config.irys_signer();

    // Create a stake commitment tx for the genesis block producer.
    let mut stake_commitment = CommitmentTransaction::new_stake(&config.consensus, H256::default());

    signer
        .sign_commitment(&mut stake_commitment)
        .expect("commitment transaction to be signable");

    let mut commitments = vec![stake_commitment.clone()];

    // Gap in configuration vs. functionality: StorageModules can compose multiple
    // submodules for a single partition, but the config doesn't yet express this
    // many-to-one relationship. For testnet, each submodule path is treated as
    // a complete HDD capable of providing all storage for a StorageModule.
    // When the configuration catches up to the StorageModule functionality,
    // this method as well as [`epoch_serve::map_storage_modules_to_partition_assignments()`]
    // will have to be updated.
    let mut anchor = stake_commitment.id();
    for i in 0..num_submodules {
        let pledge_tx =
            create_pledge_commitment_transaction(&signer, anchor, config, &(i as u64)).await;

        // We have to rotate the anchors on these TX so they produce unique signatures
        // and unique txids
        anchor = pledge_tx.id();

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
    genesis_block
        .system_ledgers
        .iter_mut()
        .find(|e| e.ledger_id == SystemLedger::Commitment)
        .expect("Commitment ledger should exist at this point")
}

/// Adds genesis commitment transaction IDs to the block header
///
/// Mutates the provided genesis_block by adding commitment IDs
/// to the Commitments system ledger.
///
/// Returns the list of commitment transactions and the total value locked in commitments.
pub async fn add_genesis_commitments(
    genesis_block: &mut IrysBlockHeader,
    config: &Config,
) -> (Vec<CommitmentTransaction>, U256) {
    let commitments = get_genesis_commitments(config).await;
    let commitment_ledger = get_or_create_commitment_ledger(genesis_block);

    // Calculate total value of all commitments (initial treasury)
    let mut total_value = U256::zero();

    // Add the commitment txids to the commitment ledger one by one
    for commitment in commitments.iter() {
        commitment_ledger.tx_ids.push(commitment.id());
        // Add commitment value to total (this represents locked funds)
        total_value = total_value.saturating_add(commitment.value());
    }

    (commitments, total_value)
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
pub async fn add_test_commitments(
    block_header: &mut IrysBlockHeader,
    pledge_count: u8,
    config: &Config,
) -> (Vec<CommitmentTransaction>, U256) {
    let signer = config.irys_signer();
    add_test_commitments_for_signer(block_header, &signer, pledge_count, config).await
}

#[cfg(any(test, feature = "test-utils"))]
pub async fn add_test_commitments_for_signer(
    block_header: &mut IrysBlockHeader,
    signer: &IrysSigner,
    pledge_count: u8,
    config: &Config,
) -> (Vec<CommitmentTransaction>, U256) {
    let mut commitments: Vec<CommitmentTransaction> = Vec::new();
    let mut anchor = H256::random();
    if block_header.is_genesis() {
        // Create a stake commitment tx for the genesis block producer.
        let mut stake_commitment =
            CommitmentTransaction::new_stake(&config.consensus, H256::default());

        signer
            .sign_commitment(&mut stake_commitment)
            .expect("commitment transaction to be signable");

        anchor = stake_commitment.id();
        commitments.push(stake_commitment);
    }

    for i in 0..(pledge_count as usize) {
        let pledge_tx =
            create_pledge_commitment_transaction(signer, anchor, config, &(i as u64)).await;
        // We have to rotate the anchors on these TX so they produce unique signatures
        // and unique txids
        anchor = pledge_tx.id();
        commitments.push(pledge_tx);
    }

    // Get a reference to the Commitment Ledger
    let commitment_ledger = get_or_create_commitment_ledger(block_header);

    // Calculate total value of all commitments
    let mut total_value = U256::zero();

    // Add the pledge commitment txids to the system ledger one by one
    for commitment in commitments.iter() {
        commitment_ledger.tx_ids.push(commitment.id());
        total_value = total_value.saturating_add(commitment.value());
    }

    (commitments, total_value)
}
