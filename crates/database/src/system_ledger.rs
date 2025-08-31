use eyre::eyre;
use irys_config::submodules::StorageSubmodulesConfig;
use irys_types::{
    irys::IrysSigner, transaction::PledgeDataProvider, CommitmentTransaction, Compact, Config,
    H256List, IrysBlockHeader, SystemTransactionLedger, H256, U256,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ops::{Index, IndexMut},
};

/// Names for each of the system ledgers as well as their `ledger_id` discriminant
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Compact, PartialOrd, Ord, Hash,
)]
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
    type Error = eyre::Report;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Commitment),
            _ => Err(eyre!("Invalid ledger number")),
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

impl Index<SystemLedger> for Vec<SystemTransactionLedger> {
    type Output = SystemTransactionLedger;

    fn index(&self, ledger: SystemLedger) -> &Self::Output {
        self.iter()
            .find(|tx_ledger| tx_ledger.ledger_id == ledger as u32)
            .expect("No system transaction ledger found for given ledger type")
    }
}

impl IndexMut<SystemLedger> for Vec<SystemTransactionLedger> {
    fn index_mut(&mut self, ledger: SystemLedger) -> &mut Self::Output {
        self.iter_mut()
            .find(|tx_ledger| tx_ledger.ledger_id == ledger as u32)
            .expect("No system transaction ledger found for given ledger type")
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
    let pledge_commitment =
        CommitmentTransaction::new_pledge(&config.consensus, anchor, provider, signer.address())
            .await;

    signer
        .sign_commitment(pledge_commitment)
        .expect("commitment transaction to be signable")
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
    let stake_commitment = CommitmentTransaction::new_stake(&config.consensus, H256::default());

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
    for i in 0..num_submodules {
        let pledge_tx =
            create_pledge_commitment_transaction(&signer, anchor, config, &(i as u64)).await;

        // We have to rotate the anchors on these TX so they produce unique signatures
        // and unique txids
        anchor = pledge_tx.id;

        commitments.push(pledge_tx);
    }

    let json = "[{\"id\":\"ApBC5ymcJi2NeFACvvPWjdQU2VPMuhCvVc15Wda1hWhG\",\"anchor\":\"11111111111111111111111111111111\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"stake\"},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"K6qchga5BxLnp6B2CsWtzdJ3ftbXbsDpjwiT7AR7YBMbTdjwBqBh9B2W18FveQPyDMFYiHZDr4RXLrBLpohehGekr\"},{\"id\":\"AxWvnZLhf2eZkXkh2BZ7PKcSXME9NJK1eZx9X9stQkCR\",\"anchor\":\"ApBC5ymcJi2NeFACvvPWjdQU2VPMuhCvVc15Wda1hWhG\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"GCDcExuCig8HLcNwkSUGuLNBySrnfAo7NTMpSFKvastmecYNEUWik8B1QYWEf7DainLpwpVb9UTuvMnKFYGzVChEr\"},{\"id\":\"AKXXAa24BdnC4jHrJhKpMQXNB2U741nuzSQLsm6u7Tjx\",\"anchor\":\"AxWvnZLhf2eZkXkh2BZ7PKcSXME9NJK1eZx9X9stQkCR\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"PDJahEB5HhMLqcyLbogE2UDRCodwWd8dBgwxiWkKKeGhawFmakq11PLFD6cCMdzXqPRZ3KNfTSmoKCTfJbaX9Xpi6\"},{\"id\":\"AVXVNyNJeD7RwaSQ3Znqwt1mZQJLbVYQ6bs57Yd9M3Wx\",\"anchor\":\"AKXXAa24BdnC4jHrJhKpMQXNB2U741nuzSQLsm6u7Tjx\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"5adWFQkgebt9uxq5978eYxAF3Z8NkVrbB8yz77gwMpsPAxogH5H6kWiAqkzzWMTdwpsy6SuGUjSvHwUVtCV8tG3Qv\"},{\"id\":\"G2Suvs8MY8hGhipAcBsyavy5PQNuLPKFbZxKWbpbWVeg\",\"anchor\":\"AVXVNyNJeD7RwaSQ3Znqwt1mZQJLbVYQ6bs57Yd9M3Wx\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"KsGDUjyph3rPT6y8iM4g2kpHgmsyaS2gLehw8Wq8Ur9mYc3nv6NAhqTjr8tek5hpJgmnMsptAtXDZ1inP5nktjemG\"},{\"id\":\"36yxmMxTwUAesf9KKMyYj2g5LsuRMbevw98yTJ3Dg1vU\",\"anchor\":\"G2Suvs8MY8hGhipAcBsyavy5PQNuLPKFbZxKWbpbWVeg\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"7GsfKC5t7SUJwyojsSrrM3iE7ZetaPANmbCenXqhBzhRqeAAW69CdJHf8TT6Dfd5yV6ssKrv43HHL1UaphuycrGrE\"},{\"id\":\"A5hezjXTX9qm1rp5TQtkGZXxGfdXTojaibsZVZTWPCWe\",\"anchor\":\"36yxmMxTwUAesf9KKMyYj2g5LsuRMbevw98yTJ3Dg1vU\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"3JHoxx3U2j5r8V4NHAJCKedVYAA3Ujv71VaiaLRNpkThYj2zykuLgvh6nmVZzix9FXcw7fsKFG63kLJx45Qy2Ewxv\"},{\"id\":\"Br9MxJwBLbCNvESSQfyUtqYFMFzWpixbdZxaWukvv3Ki\",\"anchor\":\"A5hezjXTX9qm1rp5TQtkGZXxGfdXTojaibsZVZTWPCWe\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"KeGVnLpVizHECUE9FjPbwEqAHeuHh9xMedmnSLYMiWUZ6hfCsEC7RoKt4recjYsTMZq5hUG2ZExNN72yJndRH727k\"},{\"id\":\"FuB56YKCHo4CKPZBWTNbnmVjbyC5qGNCCzXeugPsuhsm\",\"anchor\":\"Br9MxJwBLbCNvESSQfyUtqYFMFzWpixbdZxaWukvv3Ki\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"AhiU9pCJ3nV35VjDkSf57SCE5fsa86TJpKQgT9iohHymqoApRnKPNkxxgpUsobTjHLTADvePAewhWbSgVYZNWxcEw\"},{\"id\":\"HBXmxvd5DJce1fc3oWQpe5bkMGHifamX9DuhZw9bdqz3\",\"anchor\":\"FuB56YKCHo4CKPZBWTNbnmVjbyC5qGNCCzXeugPsuhsm\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":8},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"131493821403860162655\",\"signature\":\"3fF7ir9iRhuCw57K9dEdpMHqZwgxYGnjJRcq57zM5J6pYfAqaefnBfvQPBAsN4VZHRHtvuXTfj6vkYffuQxh9U8AX\"},{\"id\":\"AxAbJqPwVeJfMRCDsYsDkq1iqGAyiGgy4WU7R6eiJx86\",\"anchor\":\"HBXmxvd5DJce1fc3oWQpe5bkMGHifamX9DuhZw9bdqz3\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":9},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"119597914120445885436\",\"signature\":\"46SsEq9iTxaVgvUa3GDd9nUD6m7jKAQnkD5q8qQ7mNyYg73ZnQR848Ts4JdDDuG2tgPHC4uCxNRZTjNz9VGkV1Zhm\"},{\"id\":\"AUefGtwBe9XZgio2Ye6emyXwhuPaCVm8qhHJibyjUPQx\",\"anchor\":\"AxAbJqPwVeJfMRCDsYsDkq1iqGAyiGgy4WU7R6eiJx86\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":10},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"109766594040875928379\",\"signature\":\"LLhG24HHJLa71Pd2GAr4FdBNTtrSShBGkMBmjFZ9eLnbsE1UQmQkNPRjJYnwniUTaPJucTErwXko7U9qtXhcVERTq\"},{\"id\":\"FhpWSWBP4WmwUiamGHh2rTJwADdqGraZEnaiJAR69bav\",\"anchor\":\"AUefGtwBe9XZgio2Ye6emyXwhuPaCVm8qhHJibyjUPQx\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":11},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"101498700941547404261\",\"signature\":\"HGaF1Tc5KDhmzVwZPQchFxtgJrB2FjXcoyNnmcG1fsUp3UemCwCxQe9eidAYmCvTDZkaTmL72bknwoNhgg3KVsAmH\"},{\"id\":\"SpDAfdVyX6Mp8dNkckqjcAJa1xthPmiTnb27FqZkMoW\",\"anchor\":\"FhpWSWBP4WmwUiamGHh2rTJwADdqGraZEnaiJAR69bav\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":12},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"94444047706363085335\",\"signature\":\"7GNSYvjbe78GxidsnqDjSNip2JbjtbR2Yhxt8WYFa3RtC1quKrDGNPJJ2dkMKRa5oajiw2t5nogLU7EMUAMaCEL82\"},{\"id\":\"Dudn8G6gohPcEcXg69Ps539ULgYKNyuaSK5vDC7E1WUG\",\"anchor\":\"SpDAfdVyX6Mp8dNkckqjcAJa1xthPmiTnb27FqZkMoW\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":13},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"88350569128394093046\",\"signature\":\"PRya5oPAdojQTfCEQxJpDhx81izqqmc6VDJZBQYDG4Qri3Vx82ZBCvXDHQaTPNVtViuCG68DMC7n1uANpWAmeg3UE\"},{\"id\":\"2DyovVioBZofArMo9R1mmoRQ5QMyawu6mdHtfmBRDiqJ\",\"anchor\":\"Dudn8G6gohPcEcXg69Ps539ULgYKNyuaSK5vDC7E1WUG\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":14},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"83052022158719152905\",\"signature\":\"4XRD9Xv15BHDpJ7ifAjy7fdLgTtAtC6eQqNQRQ5QPzWbMK3zUASHK4B71Xyywq66W7GernL3nwis7CcuvUMEaDVCW\"},{\"id\":\"EnXfCVf2juGJpNbi53StK6p3NKRRu7gnWCC5M6hLPL1r\",\"anchor\":\"2DyovVioBZofArMo9R1mmoRQ5QMyawu6mdHtfmBRDiqJ\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":15},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"78345782202140596913\",\"signature\":\"G8tHoBHUwzvxwY8QqVsV6wubwSXugCDxFWhgVYdbCMDWji4a9hkS9KTxwCWrJyQcJJdV5oK1Sryd9aaXN8YS36KyU\"},{\"id\":\"67kjWMtBXH41m7CFhsoZKFefW4PRhKao43ncu3dNXitL\",\"anchor\":\"EnXfCVf2juGJpNbi53StK6p3NKRRu7gnWCC5M6hLPL1r\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":16},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"74185593596711366744\",\"signature\":\"bWzsiGbs4aASKRXP8YavYoWQtz9fTGDG8gWQr7X5HhFBoDrpj7ctueGtP8xQ93wsoEk7pxYzPvcuqYeJTHEHfrmp\"},{\"id\":\"7Y4iTWrbKTxPDe6fGmp9Rf1q9ReUwtTKXGfFdpBN7bSr\",\"anchor\":\"67kjWMtBXH41m7CFhsoZKFefW4PRhKao43ncu3dNXitL\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":17},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"70465794134072072120\",\"signature\":\"345xBYfAb3YfzjYZmeGAwtRB9ChDDuNUNVgJorLCq9ABZKobbb5VEHsZJ559q3UJjrKeQ6MHfHUwS3zVv1TR52VKq\"},{\"id\":\"2NLPhcVL5zXfXXrpXsMxpTUQaGrfM2ntNpRKnptBjqKk\",\"anchor\":\"7Y4iTWrbKTxPDe6fGmp9Rf1q9ReUwtTKXGfFdpBN7bSr\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":18},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"67118982548105240123\",\"signature\":\"LGwBJZ3Z6sREURfmrPUSpAcAx5ZbDbw2YqZ4wtoURzGWeMMwTXRjasyE1c2x68iQ979Bx5R2YbQwENUqUHpAhmDMd\"},{\"id\":\"DGhPf99kRXhsThUDDhTEKbSAymN33ioLgJegwVeTzddM\",\"anchor\":\"2NLPhcVL5zXfXXrpXsMxpTUQaGrfM2ntNpRKnptBjqKk\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":19},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"64090935264494257686\",\"signature\":\"4W4tkZs8rvvEP3f961x4t3CmjaC99os6Yo7kakHqERSTC5Wb6bQmD6mCppWG2XWSyt3q1fDmxkcDJFsob9Y5E5eFC\"},{\"id\":\"qjfu2ewgiD4Hv7dhq7aAqQUFQJHrWoyBG9QrATxtmGF\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"stake\"},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"HfRJUmuZEFyZXKghDTujeCKdr1pdUtvvZDknnot84UatVC8aJgzbycJCFH1bQdQjZY7Cwxu42eLqj39f1AMCNvX54\"},{\"id\":\"A1mYRuhLxBAQF18MxWs9qgYQ8ymM8oFyorSenQUU9SYS\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"BhBphprvvCbiXTCVMVbWopYmfcUBEW5UvTbTuXN8fQUxkQWYXW8H6CRRh2cBAheMiEDyoDCmTa27F75uVKX61PmDc\"},{\"id\":\"8VyrTRVX9hphixtGfzJikg4hTcvrQfNSbzyUpSPz4RTy\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"Gr1K1zWaZFVmLmjNPYG9tG2K5iMn8tyZWo5816Wm3crfB6GbNGiCCNbHcfvV2Kv6TWCU6F2G3k5Wztxzf423wgkjL\"},{\"id\":\"CKdYa7RiPt1efGpMWmv2x9DUAv6mmXociFzVwBGoQ19e\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"LsKE6TFBPrLtJHw3SDTFkwGeuapN8WA4vSLS46d1zuFDHE5R2ZNso3YW4kdQHKFGguth77MP5aNUrixUGpyBeZn8b\"},{\"id\":\"6JjcbuCBuZJmYjTxx2SM9z4faXaMJAPvzidXCWmAtNSd\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"FFveD5NypotY6nwTG7bF3PqMFzpn4kgS2GvUwejD36c7er9CPK7K3nj7B5JaQP3ozyt8qNJ4TKWC4WQNmxf8gSZbp\"},{\"id\":\"BHSF1ZEB1SjP9vchQM24ZcML9Pa64JTTVK1SXjupjFBC\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"KhAHkzghKr6stCrAUSkZ3xgf8oXGSK6zennrvuwxizDdgraTQgAHxrrozZ1YxpVdtwfU8CYP67UArJz1kvDBw445p\"},{\"id\":\"C9SnHfYJNN9dg6omPgoEEY2RjcVrWVVE9NnSjkJKkwcn\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"99AGoKUzZHxp7zg6oG1qpcS1VeKyR48TicMetA3faTbyu3SAC1Jac7YFN3DYyYAiWxGbAPjFu8ZbFdKJGPuD4uaxr\"},{\"id\":\"HP4aTGmVQZY2DgFEpw2RKzUFh6k7aMZHtxgDbWqL8AR2\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"BEZNDBusSgH2m5y5WAwNtzdQ6AZk1yHJ1nETDCyAiCq4kZh793kepmFdrhdZ6MYnpYfP26T6CrznwaMuGhDtvLiKV\"},{\"id\":\"7wqxPkTh6fDc2PBkJ13jRLfefihMLF4LXtKhNeFnNjhp\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"7EmMdhsZmSnY1gdLfceSeE7CfWxegdsTV8Q66vZQB7dvTo1uExDJqVs7obTPZhg9YtKBi3ejXxQydyoVieds17GwM\"},{\"id\":\"BtNzkyCUgqoH3i4pKk2nWMVNemY4w33Uv2P3K1S8qdeH\",\"anchor\":\"C7Ym3dvAQsqXaEsiCKhGYq7NqsKj8VgaHVWdMkZtY7zB\",\"signer\":\"2orJDPQr81Pw9NuWtsKrGT1WqaWg\",\"commitmentType\":{\"type\":\"stake\"},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"FAp6hZ9ZbThURW6yxGQ5QmkfLxFgNAGkngfWvw21c4tRwcx7P9HEhWSiweGREddSnJWD3vMq5TwNDHyHdnW1YpsY7\"},{\"id\":\"HggC7vSAAY72u9vQdz4UbCwwtuhqC4CnstpTvku2McQZ\",\"anchor\":\"7QV4x5TKCPisgSHyC3szEQWtRhqjUBbmK4q1copSSkVN\",\"signer\":\"2orJDPQr81Pw9NuWtsKrGT1WqaWg\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"HQrb3jVmTh9uUfcYwzGC3xi3jhToJSxST7hcWWH6ooZb1gjEnygPAyUo79mAMwZPhtjztL2unwBahWSz2fUAAHeR8\"},{\"id\":\"H9tKi9LXbdMpm33UEbDrXk7wECUP2BwqaMrJoffC44uA\",\"anchor\":\"7QV4x5TKCPisgSHyC3szEQWtRhqjUBbmK4q1copSSkVN\",\"signer\":\"2orJDPQr81Pw9NuWtsKrGT1WqaWg\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"Acg8iAQxRQZP5GaFpr4dtf7D41rZG9w9n5S6mBdHuRxGChzaBZZBbyYjyqatbWiunAd91WVWXxS6Pq35djgbuGBvS\"},{\"id\":\"7zwUcgcynrqG7uMwjWw8SN888PYYXwftSspUit8UvytC\",\"anchor\":\"7QV4x5TKCPisgSHyC3szEQWtRhqjUBbmK4q1copSSkVN\",\"signer\":\"2orJDPQr81Pw9NuWtsKrGT1WqaWg\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":0,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"BbDUAej9j9LewyVcvbX35byoKsS87Q2vaKMSjsyKagAxn6xG8t4apt2RaCMkY6W7DUhgW7qyxD9ooHMzvXsguLgCB\"}]";

    let mut commitments: Vec<CommitmentTransaction> = serde_json::from_str(json).unwrap();

    let ordered_ids = [
        "SpDAfdVyX6Mp8dNkckqjcAJa1xthPmiTnb27FqZkMoW",
        "2DyovVioBZofArMo9R1mmoRQ5QMyawu6mdHtfmBRDiqJ",
        "2NLPhcVL5zXfXXrpXsMxpTUQaGrfM2ntNpRKnptBjqKk",
        "36yxmMxTwUAesf9KKMyYj2g5LsuRMbevw98yTJ3Dg1vU",
        "67kjWMtBXH41m7CFhsoZKFefW4PRhKao43ncu3dNXitL",
        "7Y4iTWrbKTxPDe6fGmp9Rf1q9ReUwtTKXGfFdpBN7bSr",
        "A5hezjXTX9qm1rp5TQtkGZXxGfdXTojaibsZVZTWPCWe",
        "AKXXAa24BdnC4jHrJhKpMQXNB2U741nuzSQLsm6u7Tjx",
        "AUefGtwBe9XZgio2Ye6emyXwhuPaCVm8qhHJibyjUPQx",
        "AVXVNyNJeD7RwaSQ3Znqwt1mZQJLbVYQ6bs57Yd9M3Wx",
        "AxAbJqPwVeJfMRCDsYsDkq1iqGAyiGgy4WU7R6eiJx86",
        "AxWvnZLhf2eZkXkh2BZ7PKcSXME9NJK1eZx9X9stQkCR",
        "Br9MxJwBLbCNvESSQfyUtqYFMFzWpixbdZxaWukvv3Ki",
        "DGhPf99kRXhsThUDDhTEKbSAymN33ioLgJegwVeTzddM",
        "Dudn8G6gohPcEcXg69Ps539ULgYKNyuaSK5vDC7E1WUG",
        "EnXfCVf2juGJpNbi53StK6p3NKRRu7gnWCC5M6hLPL1r",
        "FhpWSWBP4WmwUiamGHh2rTJwADdqGraZEnaiJAR69bav",
        "FuB56YKCHo4CKPZBWTNbnmVjbyC5qGNCCzXeugPsuhsm",
        "G2Suvs8MY8hGhipAcBsyavy5PQNuLPKFbZxKWbpbWVeg",
        "HBXmxvd5DJce1fc3oWQpe5bkMGHifamX9DuhZw9bdqz3",
        // 2
        "6JjcbuCBuZJmYjTxx2SM9z4faXaMJAPvzidXCWmAtNSd",
        "7wqxPkTh6fDc2PBkJ13jRLfefihMLF4LXtKhNeFnNjhp",
        "8VyrTRVX9hphixtGfzJikg4hTcvrQfNSbzyUpSPz4RTy",
        "A1mYRuhLxBAQF18MxWs9qgYQ8ymM8oFyorSenQUU9SYS",
        "BHSF1ZEB1SjP9vchQM24ZcML9Pa64JTTVK1SXjupjFBC",
        "C9SnHfYJNN9dg6omPgoEEY2RjcVrWVVE9NnSjkJKkwcn",
        "CKdYa7RiPt1efGpMWmv2x9DUAv6mmXociFzVwBGoQ19e",
        "HP4aTGmVQZY2DgFEpw2RKzUFh6k7aMZHtxgDbWqL8AR2",
        // 3
        "7zwUcgcynrqG7uMwjWw8SN888PYYXwftSspUit8UvytC",
        "H9tKi9LXbdMpm33UEbDrXk7wECUP2BwqaMrJoffC44uA",
        "HggC7vSAAY72u9vQdz4UbCwwtuhqC4CnstpTvku2McQZ",
    ]
    .map(H256::from_base58);

    let position_map: HashMap<_, _> = ordered_ids
        .iter()
        .enumerate()
        .map(|(pos, id)| (*id, pos))
        .collect();

    let mut seen_ids = std::collections::HashSet::new();
    commitments.retain(|s| seen_ids.insert(s.id));

    // tracing::error!(
    //     "BEF {:?}",
    //     &commitments.iter().fold(vec![], |mut acc, c| {
    //         acc.push(c.id);
    //         acc
    //     })
    // );

    commitments.sort_by_key(|s| position_map.get(&s.id).copied().unwrap_or(usize::MAX));

    // tracing::error!(
    //     "AFT {:?}",
    //     &commitments.iter().fold(vec![], |mut acc, c| {
    //         acc.push(c.id);
    //         acc
    //     })
    // );

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
        commitment_ledger.tx_ids.push(commitment.id);
        // Add commitment value to total (this represents locked funds)
        total_value = total_value.saturating_add(commitment.value);
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
        let stake_commitment = CommitmentTransaction::new_stake(&config.consensus, H256::default());

        let stake_tx = signer
            .sign_commitment(stake_commitment)
            .expect("commitment transaction to be signable");

        anchor = stake_tx.id;
        commitments.push(stake_tx);
    }

    for i in 0..(pledge_count as usize) {
        let pledge_tx =
            create_pledge_commitment_transaction(signer, anchor, config, &(i as u64)).await;
        // We have to rotate the anchors on these TX so they produce unique signatures
        // and unique txids
        anchor = pledge_tx.id;
        commitments.push(pledge_tx);
    }

    // Get a reference to the Commitment Ledger
    let commitment_ledger = get_or_create_commitment_ledger(block_header);

    // Calculate total value of all commitments
    let mut total_value = U256::zero();

    // Add the pledge commitment txids to the system ledger one by one
    for commitment in commitments.iter() {
        commitment_ledger.tx_ids.push(commitment.id);
        total_value = total_value.saturating_add(commitment.value);
    }

    (commitments, total_value)
}
