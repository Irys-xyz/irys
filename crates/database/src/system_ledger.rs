use std::collections::HashMap;

use irys_config::submodules::StorageSubmodulesConfig;
use irys_types::{
    irys::IrysSigner, transaction::PledgeDataProvider, CommitmentTransaction, Config, H256List,
    IrysBlockHeader, SystemLedger, SystemTransactionLedger, H256, U256,
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

    // tracing::error!(
    //     "JESSEDEBUG GENESIS COMMITMENTS {:#?}",
    //     &serde_json::to_string(&commitments).unwrap(),
    // );

    // tracing::error!(
    //     "BEF {:?}",
    //     &commitments.iter().fold(vec![], |mut acc, c| {
    //         acc.push(c.id);
    //         acc
    //     })
    // );

    // spellchecker:off

    let json = "[{\"id\":\"SpDAfdVyX6Mp8dNkckqjcAJa1xthPmiTnb27FqZkMoW\",\"anchor\":\"FhpWSWBP4WmwUiamGHh2rTJwADdqGraZEnaiJAR69bav\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":12},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"94444047706363085335\",\"signature\":\"7GNSYvjbe78GxidsnqDjSNip2JbjtbR2Yhxt8WYFa3RtC1quKrDGNPJJ2dkMKRa5oajiw2t5nogLU7EMUAMaCEL82\"},{\"id\":\"2DyovVioBZofArMo9R1mmoRQ5QMyawu6mdHtfmBRDiqJ\",\"anchor\":\"Dudn8G6gohPcEcXg69Ps539ULgYKNyuaSK5vDC7E1WUG\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":14},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"83052022158719152905\",\"signature\":\"4XRD9Xv15BHDpJ7ifAjy7fdLgTtAtC6eQqNQRQ5QPzWbMK3zUASHK4B71Xyywq66W7GernL3nwis7CcuvUMEaDVCW\"},{\"id\":\"2NLPhcVL5zXfXXrpXsMxpTUQaGrfM2ntNpRKnptBjqKk\",\"anchor\":\"7Y4iTWrbKTxPDe6fGmp9Rf1q9ReUwtTKXGfFdpBN7bSr\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":18},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"67118982548105240123\",\"signature\":\"LGwBJZ3Z6sREURfmrPUSpAcAx5ZbDbw2YqZ4wtoURzGWeMMwTXRjasyE1c2x68iQ979Bx5R2YbQwENUqUHpAhmDMd\"},{\"id\":\"36yxmMxTwUAesf9KKMyYj2g5LsuRMbevw98yTJ3Dg1vU\",\"anchor\":\"G2Suvs8MY8hGhipAcBsyavy5PQNuLPKFbZxKWbpbWVeg\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"7GsfKC5t7SUJwyojsSrrM3iE7ZetaPANmbCenXqhBzhRqeAAW69CdJHf8TT6Dfd5yV6ssKrv43HHL1UaphuycrGrE\"},{\"id\":\"67kjWMtBXH41m7CFhsoZKFefW4PRhKao43ncu3dNXitL\",\"anchor\":\"EnXfCVf2juGJpNbi53StK6p3NKRRu7gnWCC5M6hLPL1r\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":16},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"74185593596711366744\",\"signature\":\"bWzsiGbs4aASKRXP8YavYoWQtz9fTGDG8gWQr7X5HhFBoDrpj7ctueGtP8xQ93wsoEk7pxYzPvcuqYeJTHEHfrmp\"},{\"id\":\"7Y4iTWrbKTxPDe6fGmp9Rf1q9ReUwtTKXGfFdpBN7bSr\",\"anchor\":\"67kjWMtBXH41m7CFhsoZKFefW4PRhKao43ncu3dNXitL\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":17},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"70465794134072072120\",\"signature\":\"345xBYfAb3YfzjYZmeGAwtRB9ChDDuNUNVgJorLCq9ABZKobbb5VEHsZJ559q3UJjrKeQ6MHfHUwS3zVv1TR52VKq\"},{\"id\":\"A5hezjXTX9qm1rp5TQtkGZXxGfdXTojaibsZVZTWPCWe\",\"anchor\":\"36yxmMxTwUAesf9KKMyYj2g5LsuRMbevw98yTJ3Dg1vU\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"3JHoxx3U2j5r8V4NHAJCKedVYAA3Ujv71VaiaLRNpkThYj2zykuLgvh6nmVZzix9FXcw7fsKFG63kLJx45Qy2Ewxv\"},{\"id\":\"AKXXAa24BdnC4jHrJhKpMQXNB2U741nuzSQLsm6u7Tjx\",\"anchor\":\"AxWvnZLhf2eZkXkh2BZ7PKcSXME9NJK1eZx9X9stQkCR\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"PDJahEB5HhMLqcyLbogE2UDRCodwWd8dBgwxiWkKKeGhawFmakq11PLFD6cCMdzXqPRZ3KNfTSmoKCTfJbaX9Xpi6\"},{\"id\":\"AUefGtwBe9XZgio2Ye6emyXwhuPaCVm8qhHJibyjUPQx\",\"anchor\":\"AxAbJqPwVeJfMRCDsYsDkq1iqGAyiGgy4WU7R6eiJx86\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":10},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"109766594040875928379\",\"signature\":\"LLhG24HHJLa71Pd2GAr4FdBNTtrSShBGkMBmjFZ9eLnbsE1UQmQkNPRjJYnwniUTaPJucTErwXko7U9qtXhcVERTq\"},{\"id\":\"AVXVNyNJeD7RwaSQ3Znqwt1mZQJLbVYQ6bs57Yd9M3Wx\",\"anchor\":\"AKXXAa24BdnC4jHrJhKpMQXNB2U741nuzSQLsm6u7Tjx\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"5adWFQkgebt9uxq5978eYxAF3Z8NkVrbB8yz77gwMpsPAxogH5H6kWiAqkzzWMTdwpsy6SuGUjSvHwUVtCV8tG3Qv\"},{\"id\":\"AxAbJqPwVeJfMRCDsYsDkq1iqGAyiGgy4WU7R6eiJx86\",\"anchor\":\"HBXmxvd5DJce1fc3oWQpe5bkMGHifamX9DuhZw9bdqz3\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":9},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"119597914120445885436\",\"signature\":\"46SsEq9iTxaVgvUa3GDd9nUD6m7jKAQnkD5q8qQ7mNyYg73ZnQR848Ts4JdDDuG2tgPHC4uCxNRZTjNz9VGkV1Zhm\"},{\"id\":\"AxWvnZLhf2eZkXkh2BZ7PKcSXME9NJK1eZx9X9stQkCR\",\"anchor\":\"ApBC5ymcJi2NeFACvvPWjdQU2VPMuhCvVc15Wda1hWhG\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"GCDcExuCig8HLcNwkSUGuLNBySrnfAo7NTMpSFKvastmecYNEUWik8B1QYWEf7DainLpwpVb9UTuvMnKFYGzVChEr\"},{\"id\":\"Br9MxJwBLbCNvESSQfyUtqYFMFzWpixbdZxaWukvv3Ki\",\"anchor\":\"A5hezjXTX9qm1rp5TQtkGZXxGfdXTojaibsZVZTWPCWe\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"KeGVnLpVizHECUE9FjPbwEqAHeuHh9xMedmnSLYMiWUZ6hfCsEC7RoKt4recjYsTMZq5hUG2ZExNN72yJndRH727k\"},{\"id\":\"DGhPf99kRXhsThUDDhTEKbSAymN33ioLgJegwVeTzddM\",\"anchor\":\"2NLPhcVL5zXfXXrpXsMxpTUQaGrfM2ntNpRKnptBjqKk\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":19},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"64090935264494257686\",\"signature\":\"4W4tkZs8rvvEP3f961x4t3CmjaC99os6Yo7kakHqERSTC5Wb6bQmD6mCppWG2XWSyt3q1fDmxkcDJFsob9Y5E5eFC\"},{\"id\":\"Dudn8G6gohPcEcXg69Ps539ULgYKNyuaSK5vDC7E1WUG\",\"anchor\":\"SpDAfdVyX6Mp8dNkckqjcAJa1xthPmiTnb27FqZkMoW\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":13},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"88350569128394093046\",\"signature\":\"PRya5oPAdojQTfCEQxJpDhx81izqqmc6VDJZBQYDG4Qri3Vx82ZBCvXDHQaTPNVtViuCG68DMC7n1uANpWAmeg3UE\"},{\"id\":\"EnXfCVf2juGJpNbi53StK6p3NKRRu7gnWCC5M6hLPL1r\",\"anchor\":\"2DyovVioBZofArMo9R1mmoRQ5QMyawu6mdHtfmBRDiqJ\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":15},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"78345782202140596913\",\"signature\":\"G8tHoBHUwzvxwY8QqVsV6wubwSXugCDxFWhgVYdbCMDWji4a9hkS9KTxwCWrJyQcJJdV5oK1Sryd9aaXN8YS36KyU\"},{\"id\":\"FhpWSWBP4WmwUiamGHh2rTJwADdqGraZEnaiJAR69bav\",\"anchor\":\"AUefGtwBe9XZgio2Ye6emyXwhuPaCVm8qhHJibyjUPQx\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":11},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"101498700941547404261\",\"signature\":\"HGaF1Tc5KDhmzVwZPQchFxtgJrB2FjXcoyNnmcG1fsUp3UemCwCxQe9eidAYmCvTDZkaTmL72bknwoNhgg3KVsAmH\"},{\"id\":\"FuB56YKCHo4CKPZBWTNbnmVjbyC5qGNCCzXeugPsuhsm\",\"anchor\":\"Br9MxJwBLbCNvESSQfyUtqYFMFzWpixbdZxaWukvv3Ki\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"AhiU9pCJ3nV35VjDkSf57SCE5fsa86TJpKQgT9iohHymqoApRnKPNkxxgpUsobTjHLTADvePAewhWbSgVYZNWxcEw\"},{\"id\":\"G2Suvs8MY8hGhipAcBsyavy5PQNuLPKFbZxKWbpbWVeg\",\"anchor\":\"AVXVNyNJeD7RwaSQ3Znqwt1mZQJLbVYQ6bs57Yd9M3Wx\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"KsGDUjyph3rPT6y8iM4g2kpHgmsyaS2gLehw8Wq8Ur9mYc3nv6NAhqTjr8tek5hpJgmnMsptAtXDZ1inP5nktjemG\"},{\"id\":\"HBXmxvd5DJce1fc3oWQpe5bkMGHifamX9DuhZw9bdqz3\",\"anchor\":\"FuB56YKCHo4CKPZBWTNbnmVjbyC5qGNCCzXeugPsuhsm\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":8},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"131493821403860162655\",\"signature\":\"3fF7ir9iRhuCw57K9dEdpMHqZwgxYGnjJRcq57zM5J6pYfAqaefnBfvQPBAsN4VZHRHtvuXTfj6vkYffuQxh9U8AX\"},{\"id\":\"6JjcbuCBuZJmYjTxx2SM9z4faXaMJAPvzidXCWmAtNSd\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"FFveD5NypotY6nwTG7bF3PqMFzpn4kgS2GvUwejD36c7er9CPK7K3nj7B5JaQP3ozyt8qNJ4TKWC4WQNmxf8gSZbp\"},{\"id\":\"7wqxPkTh6fDc2PBkJ13jRLfefihMLF4LXtKhNeFnNjhp\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"7EmMdhsZmSnY1gdLfceSeE7CfWxegdsTV8Q66vZQB7dvTo1uExDJqVs7obTPZhg9YtKBi3ejXxQydyoVieds17GwM\"},{\"id\":\"8VyrTRVX9hphixtGfzJikg4hTcvrQfNSbzyUpSPz4RTy\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"Gr1K1zWaZFVmLmjNPYG9tG2K5iMn8tyZWo5816Wm3crfB6GbNGiCCNbHcfvV2Kv6TWCU6F2G3k5Wztxzf423wgkjL\"},{\"id\":\"A1mYRuhLxBAQF18MxWs9qgYQ8ymM8oFyorSenQUU9SYS\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"BhBphprvvCbiXTCVMVbWopYmfcUBEW5UvTbTuXN8fQUxkQWYXW8H6CRRh2cBAheMiEDyoDCmTa27F75uVKX61PmDc\"},{\"id\":\"BHSF1ZEB1SjP9vchQM24ZcML9Pa64JTTVK1SXjupjFBC\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"KhAHkzghKr6stCrAUSkZ3xgf8oXGSK6zennrvuwxizDdgraTQgAHxrrozZ1YxpVdtwfU8CYP67UArJz1kvDBw445p\"},{\"id\":\"C9SnHfYJNN9dg6omPgoEEY2RjcVrWVVE9NnSjkJKkwcn\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"99AGoKUzZHxp7zg6oG1qpcS1VeKyR48TicMetA3faTbyu3SAC1Jac7YFN3DYyYAiWxGbAPjFu8ZbFdKJGPuD4uaxr\"},{\"id\":\"CKdYa7RiPt1efGpMWmv2x9DUAv6mmXociFzVwBGoQ19e\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"LsKE6TFBPrLtJHw3SDTFkwGeuapN8WA4vSLS46d1zuFDHE5R2ZNso3YW4kdQHKFGguth77MP5aNUrixUGpyBeZn8b\"},{\"id\":\"HP4aTGmVQZY2DgFEpw2RKzUFh6k7aMZHtxgDbWqL8AR2\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"BEZNDBusSgH2m5y5WAwNtzdQ6AZk1yHJ1nETDCyAiCq4kZh793kepmFdrhdZ6MYnpYfP26T6CrznwaMuGhDtvLiKV\"},{\"id\":\"7zwUcgcynrqG7uMwjWw8SN888PYYXwftSspUit8UvytC\",\"anchor\":\"7QV4x5TKCPisgSHyC3szEQWtRhqjUBbmK4q1copSSkVN\",\"signer\":\"2orJDPQr81Pw9NuWtsKrGT1WqaWg\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"BbDUAej9j9LewyVcvbX35byoKsS87Q2vaKMSjsyKagAxn6xG8t4apt2RaCMkY6W7DUhgW7qyxD9ooHMzvXsguLgCB\"},{\"id\":\"H9tKi9LXbdMpm33UEbDrXk7wECUP2BwqaMrJoffC44uA\",\"anchor\":\"7QV4x5TKCPisgSHyC3szEQWtRhqjUBbmK4q1copSSkVN\",\"signer\":\"2orJDPQr81Pw9NuWtsKrGT1WqaWg\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"Acg8iAQxRQZP5GaFpr4dtf7D41rZG9w9n5S6mBdHuRxGChzaBZZBbyYjyqatbWiunAd91WVWXxS6Pq35djgbuGBvS\"},{\"id\":\"HggC7vSAAY72u9vQdz4UbCwwtuhqC4CnstpTvku2McQZ\",\"anchor\":\"7QV4x5TKCPisgSHyC3szEQWtRhqjUBbmK4q1copSSkVN\",\"signer\":\"2orJDPQr81Pw9NuWtsKrGT1WqaWg\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"HQrb3jVmTh9uUfcYwzGC3xi3jhToJSxST7hcWWH6ooZb1gjEnygPAyUo79mAMwZPhtjztL2unwBahWSz2fUAAHeR8\"},{\"id\":\"ApBC5ymcJi2NeFACvvPWjdQU2VPMuhCvVc15Wda1hWhG\",\"anchor\":\"11111111111111111111111111111111\",\"signer\":\"2Z7NNbu2hgdx9qzoYbLX8YTAAJtR\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"K6qchga5BxLnp6B2CsWtzdJ3ftbXbsDpjwiT7AR7YBMbTdjwBqBh9B2W18FveQPyDMFYiHZDr4RXLrBLpohehGekr\"},{\"id\":\"qjfu2ewgiD4Hv7dhq7aAqQUFQJHrWoyBG9QrATxtmGF\",\"anchor\":\"8kbXKmR1W8SrpuZqYRW5VVz2ajWiMtcVt6Bvfrrmrwf3\",\"signer\":\"Ffeh9bwhoiCcnDQdfxGY1HeyQPG\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"HfRJUmuZEFyZXKghDTujeCKdr1pdUtvvZDknnot84UatVC8aJgzbycJCFH1bQdQjZY7Cwxu42eLqj39f1AMCNvX54\"},{\"id\":\"BtNzkyCUgqoH3i4pKk2nWMVNemY4w33Uv2P3K1S8qdeH\",\"anchor\":\"C7Ym3dvAQsqXaEsiCKhGYq7NqsKj8VgaHVWdMkZtY7zB\",\"signer\":\"2orJDPQr81Pw9NuWtsKrGT1WqaWg\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"FAp6hZ9ZbThURW6yxGQ5QmkfLxFgNAGkngfWvw21c4tRwcx7P9HEhWSiweGREddSnJWD3vMq5TwNDHyHdnW1YpsY7\"},{\"id\":\"f9H4nnnp17G1FfXqy4RyGEdEfVyEe6NDprP5ddnripL\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"6ie8XKuXkXijTTaMDwcuy7CPMf1JfeAn8Vzrr4TYQPToq2pYsK2LLt3fPUBvsX4AMhwhjPf6VBiNJz6x2iMdRXoLf\"},{\"id\":\"EHeQmQhdKqUCVng3T2HM2P4NqXXfVmSSA1TS5mrUekaK\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"FS8Aj7ua5oNUkGL2N9x5UQJa9JW51NpG6MynPeJdfvPorwN5JW1GtPFgMPFpA3kvMue6mQBZe2rqmDnFpexghzwir\"},{\"id\":\"5vGQ93p4yS2pTvAEgeBpvBBMRSdkzZgjB7Jf8eNCaxDZ\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"BEBP7XEHykc7eoWdHnpodFWoC9idjs5LzHA5CsKpN93R6F8m92MRoQfeskWkiCp2kmh7Lab5E5FpLv1bYuq5kGS1G\"},{\"id\":\"8Me5N5E13LziHYWH7HHJwvWg5a18avDd42xovrBotMs5\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"N5SRQFFJd4rxiwRCphKpgatdAXLtvsHW4kgH43j22DfZU5DSfjdFb5AVoJDTRRQyhausKEgoHJ1vWJQtPipVLypbV\"},{\"id\":\"2owKvCvgZr4qGaAeccYpnW5w18WwwYusLRfHBwjFSGNQ\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"5xao1QRQtt1mPritCbiPoAZ3FJhybA5ZQL9JaoAC3M5i1zLnJfn9qNSGDdz9YcqJwAqU3rLmetAFCdA1gAkvYq8rw\"},{\"id\":\"8hqjqHtFKBHwG7Kh4K665Do54e7mwHJUGw7W75HNSPGG\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"7wiAv5qWddWsCGdqoDRaJNeyoruUM9duxTvYGFWxFqCV4DSJ7jjLy1X1ek2PiJDc1vrq6unD4UxWDxdNbQLsj6em8\"},{\"id\":\"BYcYN62VYSeY3TgAcY9i4otnCqxxEe5sRuyNFwCh8Exc\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"8KBXNrB3ArC1HEUtoApwBg97E5eB1tm2Ar8ndyHFkAwvLsTTiCbQ5dvTDjofNEi1xQPKhutShfAYRLJLQPE21ZGhG\"},{\"id\":\"CHGY5aRqBcMgfMjQPd1GgJaEyRhshJXiUxZ6vhngxr6h\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"NvafDG6p3MTvmL9YSDUEtZNRhPHXmxAfMGKdBdoNys6FkRmtSyFSmqXCoN3C3qY4K6MkGRcgFrNDjzZN52YYsLkdp\"},{\"id\":\"CvL2kiexFtzvcwdpsZu6kNJJeniztC6nQUFy3STkbbux\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"CCg1vKa8yBuVzt4WdMDEJ2sceH5Jmt6sDBVb1w9Ge6T33yN2UtPbcz6Fvnbz5Pug4WpCmjemQWy51cwMEmtPtT5NA\"},{\"id\":\"f9H4nnnp17G1FfXqy4RyGEdEfVyEe6NDprP5ddnripL\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"6ie8XKuXkXijTTaMDwcuy7CPMf1JfeAn8Vzrr4TYQPToq2pYsK2LLt3fPUBvsX4AMhwhjPf6VBiNJz6x2iMdRXoLf\"},{\"id\":\"EHeQmQhdKqUCVng3T2HM2P4NqXXfVmSSA1TS5mrUekaK\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"FS8Aj7ua5oNUkGL2N9x5UQJa9JW51NpG6MynPeJdfvPorwN5JW1GtPFgMPFpA3kvMue6mQBZe2rqmDnFpexghzwir\"},{\"id\":\"5vGQ93p4yS2pTvAEgeBpvBBMRSdkzZgjB7Jf8eNCaxDZ\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"BEBP7XEHykc7eoWdHnpodFWoC9idjs5LzHA5CsKpN93R6F8m92MRoQfeskWkiCp2kmh7Lab5E5FpLv1bYuq5kGS1G\"},{\"id\":\"8Me5N5E13LziHYWH7HHJwvWg5a18avDd42xovrBotMs5\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"N5SRQFFJd4rxiwRCphKpgatdAXLtvsHW4kgH43j22DfZU5DSfjdFb5AVoJDTRRQyhausKEgoHJ1vWJQtPipVLypbV\"},{\"id\":\"2owKvCvgZr4qGaAeccYpnW5w18WwwYusLRfHBwjFSGNQ\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"5xao1QRQtt1mPritCbiPoAZ3FJhybA5ZQL9JaoAC3M5i1zLnJfn9qNSGDdz9YcqJwAqU3rLmetAFCdA1gAkvYq8rw\"},{\"id\":\"8hqjqHtFKBHwG7Kh4K665Do54e7mwHJUGw7W75HNSPGG\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"7wiAv5qWddWsCGdqoDRaJNeyoruUM9duxTvYGFWxFqCV4DSJ7jjLy1X1ek2PiJDc1vrq6unD4UxWDxdNbQLsj6em8\"},{\"id\":\"BYcYN62VYSeY3TgAcY9i4otnCqxxEe5sRuyNFwCh8Exc\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"8KBXNrB3ArC1HEUtoApwBg97E5eB1tm2Ar8ndyHFkAwvLsTTiCbQ5dvTDjofNEi1xQPKhutShfAYRLJLQPE21ZGhG\"},{\"id\":\"CHGY5aRqBcMgfMjQPd1GgJaEyRhshJXiUxZ6vhngxr6h\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"NvafDG6p3MTvmL9YSDUEtZNRhPHXmxAfMGKdBdoNys6FkRmtSyFSmqXCoN3C3qY4K6MkGRcgFrNDjzZN52YYsLkdp\"},{\"id\":\"CvL2kiexFtzvcwdpsZu6kNJJeniztC6nQUFy3STkbbux\",\"anchor\":\"HXkvgvugURdAKrK4fgJuTtagLyXbkjRBC2iR2mvefws5\",\"signer\":\"bC6mUCQYB38H5jj7JkCB452wvsV\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"CCg1vKa8yBuVzt4WdMDEJ2sceH5Jmt6sDBVb1w9Ge6T33yN2UtPbcz6Fvnbz5Pug4WpCmjemQWy51cwMEmtPtT5NA\"},{\"id\":\"4z331DK3KDdFEMygg61qQGevK75p3mqamUi7N5RwEFgv\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"qkEoMrkniz3v68ooiAwxTPpjZLP8tiEWt3RvZ6yPccvvykw8zUWBTjbefWD7KguGEzP4kr5NXirWTRgeJfDBC7G7\"},{\"id\":\"hQ3LK3C3p9RqPDnjdM9h43ujRWMfpBxDn82v8b23bbp\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"KpLBBSCQ5XZQUfeiykKccuJyaU6JC5FufXuYeaoQT3P7gNJvsKA9eb5XpG932d1Ps3N1uZcEaDrQXuxosSwP8qAEv\"},{\"id\":\"7AKhdRufwMpEUJWs9S53ZdcD8FAZqZLhs7NnKYtvhbNj\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"FmBAJU3pp7cTRAE21T7E7KPS3YM1SXfpL5nZNb3BhSaVJEd5gnjrsWDeUtZ7euqqqjNGUGexpxkpZRVhhGgwM7Snw\"},{\"id\":\"Ea9Xo4hwpoWfqaUYtyD3P6YkgzzZYVccvNN4nB5FgXEw\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"BRrDiLqqV5HJqPasfPVKvutJhnSGd9shk8NcAHpNv4GXD8bgFZ1jWZzxR43s8qbk2cbaF1ANMYrfKFVzVsNuDt9Vk\"},{\"id\":\"3NJ1xeU4THy39mwQQmvdMerJephNk5akHnqaNGBonuMW\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"Bo8Gd4RwBBTqoMgwVrjjf2Uo3xrWmD8MtqF8bXe54P6c9M8pZ1MCzrm1tjeSd5sYMvM4A8vHd68u4Wi2wfVspQeKD\"},{\"id\":\"FydMpm1PZddNDK312zDERbZuLrAtAVRE6YUgtQmZofV2\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"52dJc9ry4BTzQFjuitLj18ZDjWkV9exmuprtzqxKWCKuMaWxBGcHLDGLxq6UJyVQ2fqL6osQAvMeJ3Fi8y1QHnDEK\"},{\"id\":\"EMGgEm1HSxrYWpLVbaJcRQb1g3iu4e43hYhs38UNcbxk\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"85LqaLSGDYpvGgFY5TMSCG8gipVk3XkCP98uPid72b7xTh7wYeLHStMdJnoyoyzYsUQAE7SNecU1wFo6Tv7pc7zaX\"},{\"id\":\"Bpc3ThhGeRkeAM1JVshf5rhVvqkdaZ1yHAP3yVS5ru3e\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"2ADCKGqMLo4yo2sJBFiEjanoV2XoTX6y8KBLyLhMsqn6RN1JY1sMJpVpyafGeC5y5K8gGaFNtGSAAQtwdBL6ej923\"},{\"id\":\"87jnLze5qyprmoPATVWiWetWLerfFv9QPZBxdKvpi1se\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"DL3U41PTEEzVB643pBaDSM6PXRtAj6XpynZrJegnAMqbUUdfbPhHQm4uWhqMXRfcbALYoWho9P3tgj1FAh9cAWKRk\"},{\"id\":\"4z331DK3KDdFEMygg61qQGevK75p3mqamUi7N5RwEFgv\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"qkEoMrkniz3v68ooiAwxTPpjZLP8tiEWt3RvZ6yPccvvykw8zUWBTjbefWD7KguGEzP4kr5NXirWTRgeJfDBC7G7\"},{\"id\":\"hQ3LK3C3p9RqPDnjdM9h43ujRWMfpBxDn82v8b23bbp\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"KpLBBSCQ5XZQUfeiykKccuJyaU6JC5FufXuYeaoQT3P7gNJvsKA9eb5XpG932d1Ps3N1uZcEaDrQXuxosSwP8qAEv\"},{\"id\":\"7AKhdRufwMpEUJWs9S53ZdcD8FAZqZLhs7NnKYtvhbNj\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"FmBAJU3pp7cTRAE21T7E7KPS3YM1SXfpL5nZNb3BhSaVJEd5gnjrsWDeUtZ7euqqqjNGUGexpxkpZRVhhGgwM7Snw\"},{\"id\":\"Ea9Xo4hwpoWfqaUYtyD3P6YkgzzZYVccvNN4nB5FgXEw\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"BRrDiLqqV5HJqPasfPVKvutJhnSGd9shk8NcAHpNv4GXD8bgFZ1jWZzxR43s8qbk2cbaF1ANMYrfKFVzVsNuDt9Vk\"},{\"id\":\"3NJ1xeU4THy39mwQQmvdMerJephNk5akHnqaNGBonuMW\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"Bo8Gd4RwBBTqoMgwVrjjf2Uo3xrWmD8MtqF8bXe54P6c9M8pZ1MCzrm1tjeSd5sYMvM4A8vHd68u4Wi2wfVspQeKD\"},{\"id\":\"FydMpm1PZddNDK312zDERbZuLrAtAVRE6YUgtQmZofV2\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"52dJc9ry4BTzQFjuitLj18ZDjWkV9exmuprtzqxKWCKuMaWxBGcHLDGLxq6UJyVQ2fqL6osQAvMeJ3Fi8y1QHnDEK\"},{\"id\":\"EMGgEm1HSxrYWpLVbaJcRQb1g3iu4e43hYhs38UNcbxk\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"85LqaLSGDYpvGgFY5TMSCG8gipVk3XkCP98uPid72b7xTh7wYeLHStMdJnoyoyzYsUQAE7SNecU1wFo6Tv7pc7zaX\"},{\"id\":\"Bpc3ThhGeRkeAM1JVshf5rhVvqkdaZ1yHAP3yVS5ru3e\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"2ADCKGqMLo4yo2sJBFiEjanoV2XoTX6y8KBLyLhMsqn6RN1JY1sMJpVpyafGeC5y5K8gGaFNtGSAAQtwdBL6ej923\"},{\"id\":\"87jnLze5qyprmoPATVWiWetWLerfFv9QPZBxdKvpi1se\",\"anchor\":\"3iQegRegNm7fpkfuaofEf7wd7fbnNsm6vTk7WC9GhA6i\",\"signer\":\"2LGG9Am8JE1STqLbx8ttzkvPgzW9\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"DL3U41PTEEzVB643pBaDSM6PXRtAj6XpynZrJegnAMqbUUdfbPhHQm4uWhqMXRfcbALYoWho9P3tgj1FAh9cAWKRk\"},{\"id\":\"FSwTCYnMv1HVy6SjD4LX6mhUpD5XCdtjz9GkKtxPeny4\",\"anchor\":\"AAK1XFunwGHxHHAj1JGgnHepEedpWY2W2jJNcQQEA5Lv\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"7E4ScqrpDsjFcNXR7q6nEdTT3B6tqyYGbQKpDFjE1emwLcroWcp3899Fp3DVmWUdJubrnSxS3L5N1w9np6stxvun3\"},{\"id\":\"6gw9xoxWBVKzUBMPiwZophze52oGdLAymCqtt5wNtm1D\",\"anchor\":\"AAK1XFunwGHxHHAj1JGgnHepEedpWY2W2jJNcQQEA5Lv\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"84yat8GYur64iG8wkKyrLT82n5cKqrgN3tR2X8RoQLG4s998q9MvQ1Vu8MZ66C8U6yqyoTxCCjz8gbwwdAtSmV2JX\"},{\"id\":\"ACDDa8KQToPchsMrjPnxXYHQebh1AgXFSJsjSoeR2Wap\",\"anchor\":\"AAK1XFunwGHxHHAj1JGgnHepEedpWY2W2jJNcQQEA5Lv\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"JE9f81sdDqmoYuz5CuhkALvP51DZrwT4meyeKVGFCGREno2bQnLuDpaFDUxVra5yyCLhnmQgmLctbTEKRBoYJrYhQ\"},{\"id\":\"D2yipqvvUiF4692tS5WyQVEqvqEs93AX1UARZrPLuGf9\",\"anchor\":\"AAK1XFunwGHxHHAj1JGgnHepEedpWY2W2jJNcQQEA5Lv\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"C7phLX3ioARC5fG3UeYv5yMzfjnsAbEpSpum8eXPeDZamHDbkccrNGMJpqBoHzdXPZkFRHZUtBCiQQCbS8abiZLDh\"},{\"id\":\"8H1pNoWZ8KHXPNHQn6ccML5xZ1fx8Nvst8kw6j8NWzu2\",\"anchor\":\"AAK1XFunwGHxHHAj1JGgnHepEedpWY2W2jJNcQQEA5Lv\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"Gp77Z2cUTgNnDDuVdCxc5qo8HgTFwoMHihVSUupskisg4aLSrM7VE1GEnUo5rpWpP8xH4git9RLewU5ppDgeCwfSS\"},{\"id\":\"5LZFQJfupn8aUr2cnTJsj2YwL5vMHzSAoLxxjUX5UtPa\",\"anchor\":\"CWHhyHP2hK92GtKugm5MHqaFCa7NSZXUGAn1RuTfpsrU\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"PNX8P2LvN7FAGuzMKJQKzSUcRvJia7E9hFNgVnK9Zn4U9JgtHe6t7nWRhXd2MLQd3JyeKUN3BYnB4RhMyvdAySfnA\"},{\"id\":\"HtDLRd6uqJrZ2bib1U2Q9PXWKmGsSo8fFmV5vEH8r6K3\",\"anchor\":\"CWHhyHP2hK92GtKugm5MHqaFCa7NSZXUGAn1RuTfpsrU\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"7RHULQ9ZuWcfzgBARErqF282wpgoqaH6Ekkm8DR3oZS44jqT1EvpSm8xxzx1HQzUrPdnbFP5XAswE5jmGfrQaCDmC\"},{\"id\":\"7JD8hjLcpaKwe8Qqm37CqQywuA8BDCLAyuF5jrs3HHf3\",\"anchor\":\"CWHhyHP2hK92GtKugm5MHqaFCa7NSZXUGAn1RuTfpsrU\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"K95TSPEdjodQpVAXF2Nm2Zjg1CWYUCasjrQ4eYZK9CPdWkbUuhznN59ny8w3xmdcku5hfcXRm26XnRUtoV7tP7Yh1\"},{\"id\":\"8aEU45BnanuUGY379rRYmbNjdenvMqdSVPVWmyPXXAw5\",\"anchor\":\"CWHhyHP2hK92GtKugm5MHqaFCa7NSZXUGAn1RuTfpsrU\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"Lh8J7RwRtVpoaGVVSUeBgtovzn49f2jW2BMroo9MtYLSyvtH8AnSzJZBjK5tXjqYbJWi2hMU5U7H3FkSf4j9LiDkN\"},{\"id\":\"FSwTCYnMv1HVy6SjD4LX6mhUpD5XCdtjz9GkKtxPeny4\",\"anchor\":\"AAK1XFunwGHxHHAj1JGgnHepEedpWY2W2jJNcQQEA5Lv\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"7E4ScqrpDsjFcNXR7q6nEdTT3B6tqyYGbQKpDFjE1emwLcroWcp3899Fp3DVmWUdJubrnSxS3L5N1w9np6stxvun3\"},{\"id\":\"6gw9xoxWBVKzUBMPiwZophze52oGdLAymCqtt5wNtm1D\",\"anchor\":\"AAK1XFunwGHxHHAj1JGgnHepEedpWY2W2jJNcQQEA5Lv\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"84yat8GYur64iG8wkKyrLT82n5cKqrgN3tR2X8RoQLG4s998q9MvQ1Vu8MZ66C8U6yqyoTxCCjz8gbwwdAtSmV2JX\"},{\"id\":\"ACDDa8KQToPchsMrjPnxXYHQebh1AgXFSJsjSoeR2Wap\",\"anchor\":\"AAK1XFunwGHxHHAj1JGgnHepEedpWY2W2jJNcQQEA5Lv\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"JE9f81sdDqmoYuz5CuhkALvP51DZrwT4meyeKVGFCGREno2bQnLuDpaFDUxVra5yyCLhnmQgmLctbTEKRBoYJrYhQ\"},{\"id\":\"D2yipqvvUiF4692tS5WyQVEqvqEs93AX1UARZrPLuGf9\",\"anchor\":\"AAK1XFunwGHxHHAj1JGgnHepEedpWY2W2jJNcQQEA5Lv\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"C7phLX3ioARC5fG3UeYv5yMzfjnsAbEpSpum8eXPeDZamHDbkccrNGMJpqBoHzdXPZkFRHZUtBCiQQCbS8abiZLDh\"},{\"id\":\"8H1pNoWZ8KHXPNHQn6ccML5xZ1fx8Nvst8kw6j8NWzu2\",\"anchor\":\"AAK1XFunwGHxHHAj1JGgnHepEedpWY2W2jJNcQQEA5Lv\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"Gp77Z2cUTgNnDDuVdCxc5qo8HgTFwoMHihVSUupskisg4aLSrM7VE1GEnUo5rpWpP8xH4git9RLewU5ppDgeCwfSS\"},{\"id\":\"5LZFQJfupn8aUr2cnTJsj2YwL5vMHzSAoLxxjUX5UtPa\",\"anchor\":\"CWHhyHP2hK92GtKugm5MHqaFCa7NSZXUGAn1RuTfpsrU\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"PNX8P2LvN7FAGuzMKJQKzSUcRvJia7E9hFNgVnK9Zn4U9JgtHe6t7nWRhXd2MLQd3JyeKUN3BYnB4RhMyvdAySfnA\"},{\"id\":\"HtDLRd6uqJrZ2bib1U2Q9PXWKmGsSo8fFmV5vEH8r6K3\",\"anchor\":\"CWHhyHP2hK92GtKugm5MHqaFCa7NSZXUGAn1RuTfpsrU\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"7RHULQ9ZuWcfzgBARErqF282wpgoqaH6Ekkm8DR3oZS44jqT1EvpSm8xxzx1HQzUrPdnbFP5XAswE5jmGfrQaCDmC\"},{\"id\":\"7JD8hjLcpaKwe8Qqm37CqQywuA8BDCLAyuF5jrs3HHf3\",\"anchor\":\"CWHhyHP2hK92GtKugm5MHqaFCa7NSZXUGAn1RuTfpsrU\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"K95TSPEdjodQpVAXF2Nm2Zjg1CWYUCasjrQ4eYZK9CPdWkbUuhznN59ny8w3xmdcku5hfcXRm26XnRUtoV7tP7Yh1\"},{\"id\":\"8aEU45BnanuUGY379rRYmbNjdenvMqdSVPVWmyPXXAw5\",\"anchor\":\"CWHhyHP2hK92GtKugm5MHqaFCa7NSZXUGAn1RuTfpsrU\",\"signer\":\"35ESwVduvSMRkGhREDng8EDbQ7XJ\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"Lh8J7RwRtVpoaGVVSUeBgtovzn49f2jW2BMroo9MtYLSyvtH8AnSzJZBjK5tXjqYbJWi2hMU5U7H3FkSf4j9LiDkN\"},{\"id\":\"5k2nktM6V3gF4hG5BkHbuBLh1BzLRdK6GU4kga2yNcW4\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"2rx5puonYTeTh3jn4EQLTSk5fas32ZceFCksJD8FLLfb9RYePHYPu2XW6XbBu6HuLzEiZVJFXLZQ9p1Y4Rn68Fvp3\"},{\"id\":\"2ZTT15y9mQpaPU6XFPGpXeKuA39963mcU7iPan3ub7Y1\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"gw8qukYx17vis3NXXhohB7XwwTfWSNxLvzAszByE3sqL3UyCNjubJh2o5N6sG48Rk9AMJgspPzDJZ15ZpY1LPEsM\"},{\"id\":\"8LkCLc5KZGzraSNdVLSAAhMP74vMr5TFwnyNqbnq2ToJ\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"EbbQopLxnxTwXv7Pf4nXZAJgjh772L1DFaNpqEggLN1fjPthRoJHJp31xvaqEuXvaUo4QQH6pmETgqJKoqx9mRzK\"},{\"id\":\"6Tdi2SBn9sc7HwDoJJqej86zbtFHEaSRVF9g7CjDbgtE\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"52ZYRHjRZsMrbYvygEYqNKFz6ZZRJU4CXw14QMmpXAnGJCXFS3uGZe7jPza48bdNFjSCweJEngQY2o4h8SeZromXY\"},{\"id\":\"35EWT2k1BZoa9xq6Cu6HGSmcG1k3KxGMTsKQgNjMNDnR\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"MxvHnUhGttpoUzfHed8o6ra2NZLJXiYwiEXeRVHa2yQhgXyG89beZvmjEYgTPjkJTC72cEwhRaetRjeYhWncrVzmG\"},{\"id\":\"DnWrWuAhFJFTSUXY7PFXRzvQQF5rdweDRUjkPaHPcSZw\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"PUv3vD2aVQS8xLPfVxjjoJ78A5wwdAD4bWyPZGNngowjvGNbT895qq72utPF313xXEMtnnxdDKbL1nsQrVaXMoz3u\"},{\"id\":\"7nQWwpTiF9Ui8gpcKKxuRWEFyohqYtrmHC8RVZKzQ9Wj\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"JVb1EB5JPzi2TdTVMfYV6eTSDAL8JQS9RmDqL8ACZK2Gg2fxpvD4YZF7aD4S4enQ6C9XsPtLZ6YurBtgKD9d9jbSS\"},{\"id\":\"DVEZiYKq5cFxWoZBsxnwQEVDQuBrmitJETX8rBk22h67\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"3VWmQ9wgfQen2zEe4YCY2kwLebCbLhgsnTi5YwikjtFa9M2kvU8Gy5hw9hZyifZytSzEcEsFo9oiE9kanRWXSBiQ3\"},{\"id\":\"Ga1oWyjEDc2kLbUVASz5pnq6XMqjL2GqkQFtM5Pt3Nhb\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"H2tQaPyHuhgJGSLKy1T68tpTrZqzBk6t8Sobr6y8RJWEVnqjjAVjirv7mckbZyFxFxD1eaGtjae1B18JiYvorQFqV\"},{\"id\":\"5k2nktM6V3gF4hG5BkHbuBLh1BzLRdK6GU4kga2yNcW4\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"2rx5puonYTeTh3jn4EQLTSk5fas32ZceFCksJD8FLLfb9RYePHYPu2XW6XbBu6HuLzEiZVJFXLZQ9p1Y4Rn68Fvp3\"},{\"id\":\"2ZTT15y9mQpaPU6XFPGpXeKuA39963mcU7iPan3ub7Y1\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"gw8qukYx17vis3NXXhohB7XwwTfWSNxLvzAszByE3sqL3UyCNjubJh2o5N6sG48Rk9AMJgspPzDJZ15ZpY1LPEsM\"},{\"id\":\"8LkCLc5KZGzraSNdVLSAAhMP74vMr5TFwnyNqbnq2ToJ\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"EbbQopLxnxTwXv7Pf4nXZAJgjh772L1DFaNpqEggLN1fjPthRoJHJp31xvaqEuXvaUo4QQH6pmETgqJKoqx9mRzK\"},{\"id\":\"6Tdi2SBn9sc7HwDoJJqej86zbtFHEaSRVF9g7CjDbgtE\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"52ZYRHjRZsMrbYvygEYqNKFz6ZZRJU4CXw14QMmpXAnGJCXFS3uGZe7jPza48bdNFjSCweJEngQY2o4h8SeZromXY\"},{\"id\":\"35EWT2k1BZoa9xq6Cu6HGSmcG1k3KxGMTsKQgNjMNDnR\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"MxvHnUhGttpoUzfHed8o6ra2NZLJXiYwiEXeRVHa2yQhgXyG89beZvmjEYgTPjkJTC72cEwhRaetRjeYhWncrVzmG\"},{\"id\":\"DnWrWuAhFJFTSUXY7PFXRzvQQF5rdweDRUjkPaHPcSZw\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"PUv3vD2aVQS8xLPfVxjjoJ78A5wwdAD4bWyPZGNngowjvGNbT895qq72utPF313xXEMtnnxdDKbL1nsQrVaXMoz3u\"},{\"id\":\"7nQWwpTiF9Ui8gpcKKxuRWEFyohqYtrmHC8RVZKzQ9Wj\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"JVb1EB5JPzi2TdTVMfYV6eTSDAL8JQS9RmDqL8ACZK2Gg2fxpvD4YZF7aD4S4enQ6C9XsPtLZ6YurBtgKD9d9jbSS\"},{\"id\":\"DVEZiYKq5cFxWoZBsxnwQEVDQuBrmitJETX8rBk22h67\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"3VWmQ9wgfQen2zEe4YCY2kwLebCbLhgsnTi5YwikjtFa9M2kvU8Gy5hw9hZyifZytSzEcEsFo9oiE9kanRWXSBiQ3\"},{\"id\":\"Ga1oWyjEDc2kLbUVASz5pnq6XMqjL2GqkQFtM5Pt3Nhb\",\"anchor\":\"9VdxRnvmLCxGRVFoQHcqAUNnFakQVMUtWvuxE5RFmJTc\",\"signer\":\"28T56M4hn9F8UJ4C9xqsr7ccw238\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"H2tQaPyHuhgJGSLKy1T68tpTrZqzBk6t8Sobr6y8RJWEVnqjjAVjirv7mckbZyFxFxD1eaGtjae1B18JiYvorQFqV\"}]";

    let json2 = "[{\"id\":\"83rs4Pkz9o7udk8Xf6tz8hS4YvGNobHhfbHgvGkHurdC\",\"anchor\":\"11111111111111111111111111111111\",\"signer\":\"2DgvStAbNv9tUiyhYEg27728aVu6\",\"commitmentType\":{\"type\":\"stake\"},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"20000000000000000000000\",\"signature\":\"C6iEmZkxmVgyc8taG6wCHLnV6GHwrPTQeFiW4uf4HRfmQDdqXJPGBgyVsU4ZLE49fkyaRwLgBpdMPAad791Q3mZhk\"},{\"id\":\"pyik8Jv9f3YdUDzo6jQ6Z5ePmvb7nnak1R7o2EUd6uT\",\"anchor\":\"83rs4Pkz9o7udk8Xf6tz8hS4YvGNobHhfbHgvGkHurdC\",\"signer\":\"2DgvStAbNv9tUiyhYEg27728aVu6\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":0},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"950000000000000000000\",\"signature\":\"8oavtLLpY59agErQuzrtcscRvDqjoMEAHhwRgVUZibyQP7rmgaZuT4jqzRXfApWXba2QtrL1r3kn4HUHTsJ4fNrVR\"},{\"id\":\"517dP1pw9WGBLFX3qNe8xLjdgV9HFP3Nf7AUK1V5RhLh\",\"anchor\":\"pyik8Jv9f3YdUDzo6jQ6Z5ePmvb7nnak1R7o2EUd6uT\",\"signer\":\"2DgvStAbNv9tUiyhYEg27728aVu6\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":1},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"509092394704739255719\",\"signature\":\"MUjAu65WJ976hpm8RKfzryLDhmkKrWtvoNJVFbkfrtNi21PHjhASP56sbR799WMyJyunj8wBEPS2SPrkh84VQv2Vh\"},{\"id\":\"7LAt8hGx63eQVuMyqGxDctSTP5ZKWg9br2eg73qJgPo9\",\"anchor\":\"517dP1pw9WGBLFX3qNe8xLjdgV9HFP3Nf7AUK1V5RhLh\",\"signer\":\"2DgvStAbNv9tUiyhYEg27728aVu6\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":2},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"353439005113955754002\",\"signature\":\"FynjanyMFEpW6F6XMrtK7JPTeeiryJNMa87scM82DzRtEjAj2GuRveLJqnBDANWLfKNciqi4M8BnGjqzGfA9T2bdc\"},{\"id\":\"DdgxQ5BhUQ52tcFV6Pwq4AiDZtR44KypcepMhH2EB6B9\",\"anchor\":\"7LAt8hGx63eQVuMyqGxDctSTP5ZKWg9br2eg73qJgPo9\",\"signer\":\"2DgvStAbNv9tUiyhYEg27728aVu6\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":3},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"272815859311795815254\",\"signature\":\"8tjdvSSk669G8QQuScC81XAstmVyFY677vG1djRfH7Kwijk1beEeTkC3CZ8HouabTb8Fkd9RQnkWcYBCSCefN3AHQ\"},{\"id\":\"He9N44udjsXzAvfk1BFkX5UQWuzLsTS9EhAB9ouUCnf5\",\"anchor\":\"DdgxQ5BhUQ52tcFV6Pwq4AiDZtR44KypcepMhH2EB6B9\",\"signer\":\"2DgvStAbNv9tUiyhYEg27728aVu6\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":4},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"223177599186723612055\",\"signature\":\"Cjh2Z7smt1pbFSgn8wrxEAMXZKmr2pAkYuHb4kDwSGF15TPnMHSmXECsHzNVBmiVPTvF4XnzWeHDE3XsRintcQgFD\"},{\"id\":\"AexPMP3FUPURCVCQWwJNxcn5qbWRY5sP29ADC4JUcYBV\",\"anchor\":\"He9N44udjsXzAvfk1BFkX5UQWuzLsTS9EhAB9ouUCnf5\",\"signer\":\"2DgvStAbNv9tUiyhYEg27728aVu6\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":5},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"189403273153183492508\",\"signature\":\"PXiWfn6DAeA6hvqnoF4CEbe6RtvHqV8Bu3riiYMuzjCufRESYKiD3DiqtDmBZV54mGA858rFGxdRH7J7ZxdaXfdYP\"},{\"id\":\"FvTgoKsp1AqRWXccHw7KajvvcTKFd34Fzux9XbPMFs9\",\"anchor\":\"AexPMP3FUPURCVCQWwJNxcn5qbWRY5sP29ADC4JUcYBV\",\"signer\":\"2DgvStAbNv9tUiyhYEg27728aVu6\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":6},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"164867991635690088752\",\"signature\":\"9N8QFZdmKtV59bGZsJcNPnWgiR1awsgqNJTbJQZArRBqzSX9oFVWWxQHriYrtBNmAF2NqoF6vSQML6sNZmiuBHf4v\"},{\"id\":\"5tMNMeVSjFcFf2m5RMa9pitVFyAe4wcg5JnBdsSBciuP\",\"anchor\":\"FvTgoKsp1AqRWXccHw7KajvvcTKFd34Fzux9XbPMFs9\",\"signer\":\"2DgvStAbNv9tUiyhYEg27728aVu6\",\"commitmentType\":{\"type\":\"pledge\",\"pledgeCountBeforeExecuting\":7},\"version\":1,\"chainId\":\"1270\",\"fee\":\"100\",\"value\":\"146198399084708809276\",\"signature\":\"Gxi1An9eAymTDYKj4EW7QuutmQV1Z3bUjikxxiqMofkrQCFiPF2x6drgJkyVpaGGEYrTunc75aHbx7XHUhjbhbx91\"}]";

    let mut commitments: Vec<CommitmentTransaction> = serde_json::from_str(json).unwrap();
    let commitments2: Vec<CommitmentTransaction> = serde_json::from_str(json2).unwrap();

    commitments.extend(commitments2);

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
        // 4 (?)
        "2owKvCvgZr4qGaAeccYpnW5w18WwwYusLRfHBwjFSGNQ",
        "5vGQ93p4yS2pTvAEgeBpvBBMRSdkzZgjB7Jf8eNCaxDZ",
        "8Me5N5E13LziHYWH7HHJwvWg5a18avDd42xovrBotMs5",
        "8hqjqHtFKBHwG7Kh4K665Do54e7mwHJUGw7W75HNSPGG",
        "BYcYN62VYSeY3TgAcY9i4otnCqxxEe5sRuyNFwCh8Exc",
        "CHGY5aRqBcMgfMjQPd1GgJaEyRhshJXiUxZ6vhngxr6h",
        "CvL2kiexFtzvcwdpsZu6kNJJeniztC6nQUFy3STkbbux",
        "EHeQmQhdKqUCVng3T2HM2P4NqXXfVmSSA1TS5mrUekaK",
        // 5 (?)
        "hQ3LK3C3p9RqPDnjdM9h43ujRWMfpBxDn82v8b23bbp",
        "3NJ1xeU4THy39mwQQmvdMerJephNk5akHnqaNGBonuMW",
        "7AKhdRufwMpEUJWs9S53ZdcD8FAZqZLhs7NnKYtvhbNj",
        "87jnLze5qyprmoPATVWiWetWLerfFv9QPZBxdKvpi1se",
        "Bpc3ThhGeRkeAM1JVshf5rhVvqkdaZ1yHAP3yVS5ru3e",
        "EMGgEm1HSxrYWpLVbaJcRQb1g3iu4e43hYhs38UNcbxk",
        "Ea9Xo4hwpoWfqaUYtyD3P6YkgzzZYVccvNN4nB5FgXEw",
        "FydMpm1PZddNDK312zDERbZuLrAtAVRE6YUgtQmZofV2",
        // 6 (?)
        "5LZFQJfupn8aUr2cnTJsj2YwL5vMHzSAoLxxjUX5UtPa",
        "6gw9xoxWBVKzUBMPiwZophze52oGdLAymCqtt5wNtm1D",
        "7JD8hjLcpaKwe8Qqm37CqQywuA8BDCLAyuF5jrs3HHf3",
        "8H1pNoWZ8KHXPNHQn6ccML5xZ1fx8Nvst8kw6j8NWzu2",
        "8aEU45BnanuUGY379rRYmbNjdenvMqdSVPVWmyPXXAw5",
        "ACDDa8KQToPchsMrjPnxXYHQebh1AgXFSJsjSoeR2Wap",
        "D2yipqvvUiF4692tS5WyQVEqvqEs93AX1UARZrPLuGf9",
        "HtDLRd6uqJrZ2bib1U2Q9PXWKmGsSo8fFmV5vEH8r6K3",
        // 7 (?)
        "2ZTT15y9mQpaPU6XFPGpXeKuA39963mcU7iPan3ub7Y1",
        "35EWT2k1BZoa9xq6Cu6HGSmcG1k3KxGMTsKQgNjMNDnR",
        "6Tdi2SBn9sc7HwDoJJqej86zbtFHEaSRVF9g7CjDbgtE",
        "7nQWwpTiF9Ui8gpcKKxuRWEFyohqYtrmHC8RVZKzQ9Wj",
        "8LkCLc5KZGzraSNdVLSAAhMP74vMr5TFwnyNqbnq2ToJ",
        "DVEZiYKq5cFxWoZBsxnwQEVDQuBrmitJETX8rBk22h67",
        "DnWrWuAhFJFTSUXY7PFXRzvQQF5rdweDRUjkPaHPcSZw",
        "Ga1oWyjEDc2kLbUVASz5pnq6XMqjL2GqkQFtM5Pt3Nhb",
        // new genesis
        "83rs4Pkz9o7udk8Xf6tz8hS4YvGNobHhfbHgvGkHurdC",
        "pyik8Jv9f3YdUDzo6jQ6Z5ePmvb7nnak1R7o2EUd6uT",
        "517dP1pw9WGBLFX3qNe8xLjdgV9HFP3Nf7AUK1V5RhLh",
        "7LAt8hGx63eQVuMyqGxDctSTP5ZKWg9br2eg73qJgPo9",
        "DdgxQ5BhUQ52tcFV6Pwq4AiDZtR44KypcepMhH2EB6B9",
        "He9N44udjsXzAvfk1BFkX5UQWuzLsTS9EhAB9ouUCnf5",
        "AexPMP3FUPURCVCQWwJNxcn5qbWRY5sP29ADC4JUcYBV",
        "FvTgoKsp1AqRWXccHw7KajvvcTKFd34Fzux9XbPMFs9",
        "5tMNMeVSjFcFf2m5RMa9pitVFyAe4wcg5JnBdsSBciuP",
    ]
    .map(H256::from_base58);

    // spellchecker:on

    let position_map: HashMap<_, _> = ordered_ids
        .iter()
        .enumerate()
        .map(|(pos, id)| (*id, pos))
        .collect();

    let mut seen_ids = std::collections::HashSet::new();
    commitments.retain(|s| seen_ids.insert(s.id()));

    // tracing::error!(
    //     "BEF {:?}",
    //     &commitments.iter().fold(vec![], |mut acc, c| {
    //         acc.push(c.id);
    //         acc
    //     })
    // );

    commitments.sort_by_key(|s| position_map.get(&s.id()).copied().unwrap_or(usize::MAX));

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
