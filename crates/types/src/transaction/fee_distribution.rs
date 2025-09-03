/// Consensus Pricing Mechanism
/// The process of promoting data from the submit ledger to the permanent ledger involves multiple phases, resulting in a staged payment model for permanent data. All transactions, whether intended for permanent (perm) or temporary (term) data, initially enter the submit ledger. The payment process for term data is consistent across all transactions, while permanent data incurs additional payments to incentivize the complete publishing process.
///
/// ## Term Data Payment Distribution
/// 1. User posts a transaction including the term_fee.
/// 2. Block producer transaction inclusion:
///   - Block producer includes the transaction in a block.
///   - Block producer's balance increases by 5% of the term_fee.
///   - Remaining 95% of term_fee is added to the treasury (tracked in block headers).
/// 3. The user uploads data chunks associated with their transaction.
/// 4. Miners assigned to store chunks gossip them amongst themselves.
/// 5. Term ledger expiration payout:
///   - When the transaction expires from the submit ledger (when the partitions containing its chunks are reset at an epoch boundary), each miner is paid their portion (term_fee / num_chunks_in_partition) for all assigned chunks expiring in their partition.
///   - For a full 16TB partition, this payout is approximately $0.60 per miner.
///   - Miners continue to earn full inflation/block rewards from any blocks they produce while mining these partitions.
///
/// ## Permanent Data Payment Distribution
/// Users pay the following fees for permanent data storage:
/// - term_fee: Standard fee for term storage (distributed as with regular term data)
/// - perm_fee: Total fee for permanent storage which includes:
///   - Base permanent storage cost (200 years × 10 replicas with 1% annual decline)
///   - PLUS ingress proof rewards (5% of term_fee × number_of_ingress_proofs)
///
/// ### Fee Distribution
/// - term_fee: Processed identically to regular term data transactions.
/// - 5% of term_fee for each ingress-proof provided. These rewards are included in the transaction's `perm_fee` field along with the base permanent storage cost.
/// - perm_fee total value: Prepaid amount covering 200 years x 10 replicas with 1% annual decline in storage costs PLUS ingress proof rewards (5% of term_fee × number_of_ingress_proofs)
use crate::ingress::IngressProof;
use crate::storage_pricing::{mul_div, PRECISION_SCALE};
pub use crate::{
    address_base58_stringify, optional_string_u64, string_u64, Address, Arbitrary, Base64, Compact,
    ConsensusConfig, IrysSignature, Node, Proof, Signature, H256, U256,
};
use eyre::{ensure, eyre, OptionExt as _};
pub use irys_primitives::CommitmentType;
use serde::{Deserialize, Serialize};

pub enum FeeDistribution {
    PublishTransactions(PublishFeeCharges, TermFeeCharges),
}

/// Represents a single fee charge to or from an address
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeeCharge {
    /// The address involved in the charge
    pub address: Address,
    /// The amount of the charge
    pub amount: U256,
}

/// Represents the complete term fee distribution for a data transaction from a term ledger
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermFeeCharges {
    /// Reward to the block producer who includes this tx
    pub block_producer_reward: U256,

    /// Remaining term_fee that goes to treasury
    pub term_fee_treasury: U256,
}

impl TermFeeCharges {
    pub fn new(term_fee: U256, config: &ConsensusConfig) -> eyre::Result<Self> {
        // Calculate block producer reward using immediate_tx_inclusion_reward_percent from config
        let block_producer_reward = mul_div(
            term_fee,
            config.immediate_tx_inclusion_reward_percent.amount,
            PRECISION_SCALE,
        )?;

        // The rest of the fee goes to the treasury
        let term_fee_treasury = term_fee
            .checked_sub(block_producer_reward)
            .ok_or_eyre("block producer reward larger than term fee")?;

        // Validate that the sum of all the fields equals term_fee
        ensure!(
            block_producer_reward.saturating_add(term_fee_treasury) == term_fee,
            "Fee distribution must equal total term_fee: {} + {} != {}",
            block_producer_reward,
            term_fee_treasury,
            term_fee
        );

        Ok(Self {
            block_producer_reward,
            term_fee_treasury,
        })
    }

    /// Returns the reward for each miner, distributing any remainder to the first miner
    pub fn distribution_on_expiry(&self, miners: &[Address]) -> eyre::Result<Vec<U256>> {
        // Check for empty miners array
        ensure!(
            !miners.is_empty(),
            "Cannot distribute fees to empty miners list"
        );

        // Check for duplicate addresses
        let unique_count = miners
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len();
        ensure!(
            unique_count == miners.len(),
            "Duplicate mining addresses not allowed in fee distribution: found {} unique addresses in list of {}",
            unique_count,
            miners.len()
        );

        // When the partition expires we distribute the remainder of the term_fee
        // (which is located in term_fee_treasury) over to all the miners
        let num_miners = U256::from(miners.len());
        let base_per_miner_reward = self
            .term_fee_treasury
            .checked_div(num_miners)
            .ok_or_else(|| eyre!("Failed to calculate per-miner reward"))?;

        // Calculate remainder from integer division
        let total_base_distribution = base_per_miner_reward.saturating_mul(num_miners);
        let remainder = self
            .term_fee_treasury
            .saturating_sub(total_base_distribution);

        // Create distribution vector
        let mut rewards = vec![base_per_miner_reward; miners.len()];

        // Give the remainder to the first miner (if any)
        if remainder > U256::from(0) {
            rewards[0] = rewards[0].saturating_add(remainder);
        }

        // Validate that the sum of all the fees we're distributing equals term_fee_treasury exactly
        let total_distributed: U256 = rewards
            .iter()
            .fold(U256::from(0), |acc, reward| acc.saturating_add(*reward));

        ensure!(
            total_distributed == self.term_fee_treasury,
            "Total distributed must equal term_fee_treasury exactly: {} != {}",
            total_distributed,
            self.term_fee_treasury
        );

        Ok(rewards)
    }
}

/// Represents the complete perm fee distribution for a data transaction
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishFeeCharges {
    /// Total rewards to be distributed among ingress proof providers
    pub ingress_proof_reward: U256,

    /// Full perm_fee that goes to treasury for permanent storage
    pub perm_fee_treasury: U256,
}

impl PublishFeeCharges {
    pub fn new(perm_fee: U256, term_fee: U256, config: &ConsensusConfig) -> eyre::Result<Self> {
        // Calculate the reward for each ingress proof (x% of term_fee)
        let per_ingress_reward = mul_div(
            term_fee,
            config.immediate_tx_inclusion_reward_percent.amount,
            PRECISION_SCALE,
        )
        .unwrap_or(U256::from(0));

        // Number of ingress proofs required from config
        let num_ingress_proofs = config.number_of_ingress_proofs_total;

        // Calculate total ingress rewards for all proofs
        let ingress_proof_reward =
            per_ingress_reward.saturating_mul(U256::from(num_ingress_proofs));

        // Validate that perm_fee is sufficient to cover ingress rewards
        // According to business rules, perm_fee = base_permanent_storage_cost + ingress_proof_rewards
        ensure!(
            perm_fee >= ingress_proof_reward,
            "Permanent fee ({}) is insufficient to cover ingress proof rewards ({})",
            perm_fee,
            ingress_proof_reward
        );

        // The perm_fee should include base perm storage cost + ingress rewards
        // Treasury gets the base permanent storage cost portion (perm_fee - ingress_rewards)
        let perm_fee_treasury = perm_fee.saturating_sub(ingress_proof_reward);

        // Validate that treasury amount is non-zero (there should be base storage cost)
        ensure!(
            perm_fee_treasury > U256::from(0),
            "Permanent fee ({}) must include base storage cost in addition to ingress proof rewards ({})",
            perm_fee,
            ingress_proof_reward
        );

        // Validate that the sum of all fields equals perm_fee
        ensure!(
            ingress_proof_reward.saturating_add(perm_fee_treasury) == perm_fee,
            "Fee distribution must equal total perm_fee: {} + {} != {}",
            ingress_proof_reward,
            perm_fee_treasury,
            perm_fee
        );

        Ok(Self {
            ingress_proof_reward,
            perm_fee_treasury,
        })
    }

    /// Provided a list of ingress proofs, figure out the fee allocations for each of them
    pub fn ingress_proof_rewards(
        &self,
        ingress_proofs: &[IngressProof],
    ) -> eyre::Result<Vec<FeeCharge>> {
        // Check for empty proofs
        ensure!(
            !ingress_proofs.is_empty(),
            "Cannot distribute rewards to empty ingress proofs list"
        );

        // Calculate base reward per proof
        let num_proofs = U256::from(ingress_proofs.len());
        let base_per_proof_reward = self
            .ingress_proof_reward
            .checked_div(num_proofs)
            .ok_or_else(|| eyre!("Failed to calculate per-proof reward"))?;

        // Calculate remainder from integer division
        let total_base_distribution = base_per_proof_reward.saturating_mul(num_proofs);
        let remainder = self
            .ingress_proof_reward
            .saturating_sub(total_base_distribution);

        // Extract addresses and create fee charges
        let mut fee_charges = Vec::with_capacity(ingress_proofs.len());

        for (i, proof) in ingress_proofs.iter().enumerate() {
            // Recover the address from the ingress proof signature
            let address = proof.recover_signer()?;

            // First proof gets base + remainder, others get base
            let amount = if i == 0 && remainder > U256::from(0) {
                base_per_proof_reward.saturating_add(remainder)
            } else {
                base_per_proof_reward
            };

            fee_charges.push(FeeCharge { address, amount });
        }

        // Validate that we're distributing exactly what was allocated
        let total_distributed: U256 = fee_charges.iter().map(|fc| fc.amount).fold(
            U256::from(0),
            super::super::serialization::U256::saturating_add,
        );

        ensure!(
            total_distributed == self.ingress_proof_reward,
            "Total distributed fees must equal total allocated rewards: {} != {}",
            total_distributed,
            self.ingress_proof_reward
        );

        Ok(fee_charges)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage_pricing::phantoms::Percentage;
    use crate::storage_pricing::Amount;
    use rust_decimal_macros::dec;

    #[test]
    fn test_term_fee_charges_new() {
        let mut config = ConsensusConfig::testing();
        // Set block producer fee to 5%
        config.immediate_tx_inclusion_reward_percent =
            Amount::<Percentage>::percentage(dec!(0.05)).unwrap();

        let term_fee = U256::from(1000);
        let charges = TermFeeCharges::new(term_fee, &config).unwrap();

        // Block producer should get 5% = 50
        assert_eq!(charges.block_producer_reward, U256::from(50));
        // Treasury should get 95% = 950
        assert_eq!(charges.term_fee_treasury, U256::from(950));
        // Total should equal term_fee
        assert_eq!(
            charges.block_producer_reward + charges.term_fee_treasury,
            term_fee
        );
    }

    #[test]
    fn test_term_fee_distribution_on_expiry() {
        let config = ConsensusConfig::testing();
        let term_fee = U256::from(1000);
        let charges = TermFeeCharges::new(term_fee, &config).unwrap();

        // Test with 10 miners
        let miners: Vec<Address> = (0..10).map(|_| Address::random()).collect();

        let rewards = charges.distribution_on_expiry(&miners).unwrap();

        // Check we have the right number of rewards
        assert_eq!(rewards.len(), 10);

        // Each miner should get approximately treasury / 10
        let base_per_miner = charges.term_fee_treasury / U256::from(10);

        // First 9 miners should get the base amount (or first miner gets base + remainder)
        for (i, reward) in rewards.iter().enumerate().skip(1) {
            assert_eq!(
                *reward, base_per_miner,
                "Miner {} should get base amount",
                i
            );
        }

        // Total should equal treasury exactly
        let total: U256 = rewards.iter().fold(U256::from(0), |acc, r| acc + *r);
        assert_eq!(total, charges.term_fee_treasury);
    }

    #[test]
    fn test_term_fee_distribution_with_remainder() {
        let config = ConsensusConfig::testing();
        let term_fee = U256::from(1003); // Not evenly divisible by 10
        let charges = TermFeeCharges::new(term_fee, &config).unwrap();

        // Test with 10 miners
        let miners: Vec<Address> = (0..10).map(|_| Address::random()).collect();

        let rewards = charges.distribution_on_expiry(&miners).unwrap();

        // Check we have the right number of rewards
        assert_eq!(rewards.len(), 10);

        // Calculate expected values
        let base_per_miner = charges.term_fee_treasury / U256::from(10);
        let remainder = charges.term_fee_treasury % U256::from(10);

        // First miner should get base + remainder
        assert_eq!(rewards[0], base_per_miner + remainder);

        // Rest should get base amount
        for reward in &rewards[1..] {
            assert_eq!(*reward, base_per_miner);
        }

        // Total should equal treasury exactly
        let total: U256 = rewards.iter().fold(U256::from(0), |acc, r| acc + *r);
        assert_eq!(total, charges.term_fee_treasury);
    }

    #[test]
    fn test_term_fee_distribution_empty_miners() {
        let config = ConsensusConfig::testing();
        let term_fee = U256::from(1000);
        let charges = TermFeeCharges::new(term_fee, &config).unwrap();

        let miners: Vec<Address> = vec![];
        let result = charges.distribution_on_expiry(&miners);

        // Should fail with empty miners
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("empty miners"));
    }

    #[test]
    fn test_term_fee_distribution_rejects_duplicates() {
        let config = ConsensusConfig::testing();
        let term_fee = U256::from(1000);
        let charges = TermFeeCharges::new(term_fee, &config).unwrap();

        // Create miners list with duplicates
        let addr1 = Address::random();
        let addr2 = Address::random();
        let miners_with_duplicates = vec![addr1, addr2, addr1]; // addr1 appears twice

        let result = charges.distribution_on_expiry(&miners_with_duplicates);

        // Should fail due to duplicates
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Duplicate mining addresses not allowed"));
    }

    #[test]
    fn test_publish_fee_charges_new() {
        let mut config = ConsensusConfig::testing();
        // Set ingress proof fee to 5%
        config.immediate_tx_inclusion_reward_percent =
            Amount::<Percentage>::percentage(dec!(0.05)).unwrap();
        config.number_of_ingress_proofs_total = 3;

        let term_fee = U256::from(1000);
        let perm_fee = U256::from(10000);

        let charges = PublishFeeCharges::new(perm_fee, term_fee, &config).unwrap();

        // Total ingress reward should be 3 * (5% of term_fee) = 3 * 50 = 150
        assert_eq!(charges.ingress_proof_reward, U256::from(150));

        // Treasury should get perm_fee - 150 = 10000 - 150 = 9850
        assert_eq!(charges.perm_fee_treasury, U256::from(9850));

        // Total should equal perm_fee
        assert_eq!(
            charges.ingress_proof_reward + charges.perm_fee_treasury,
            perm_fee
        );
    }

    #[test]
    fn test_publish_fee_ingress_proof_rewards() {
        use crate::ingress::generate_ingress_proof;
        use crate::irys::IrysSigner;

        let mut config = ConsensusConfig::testing();
        config.immediate_tx_inclusion_reward_percent =
            Amount::<Percentage>::percentage(dec!(0.05)).unwrap();

        let term_fee = U256::from(1000);
        let perm_fee = U256::from(10000);
        let charges = PublishFeeCharges::new(perm_fee, term_fee, &config).unwrap();

        // Create some test ingress proofs
        let signer1 = IrysSigner::random_signer(&config);
        let signer2 = IrysSigner::random_signer(&config);
        let data_root = H256::random();

        // Generate actual ingress proofs
        let proof1 = generate_ingress_proof(
            &signer1,
            data_root,
            vec![vec![0_u8; 32]].into_iter().map(Ok),
            config.chain_id,
        )
        .unwrap();

        let proof2 = generate_ingress_proof(
            &signer2,
            data_root,
            vec![vec![0_u8; 32]].into_iter().map(Ok),
            config.chain_id,
        )
        .unwrap();

        let proofs = vec![proof1, proof2];
        let fee_charges = charges.ingress_proof_rewards(&proofs).unwrap();

        // Should have 2 fee charges
        assert_eq!(fee_charges.len(), 2);

        // Each should get half of total (75 each)
        let expected_per_proof = charges.ingress_proof_reward / U256::from(2);
        assert_eq!(fee_charges[0].amount, expected_per_proof);
        assert_eq!(fee_charges[1].amount, expected_per_proof);

        // Addresses should match signers
        assert_eq!(fee_charges[0].address, signer1.address());
        assert_eq!(fee_charges[1].address, signer2.address());

        // Total should equal ingress_proof_reward
        let total: U256 = fee_charges
            .iter()
            .map(|fc| fc.amount)
            .fold(U256::from(0), |acc, amt| acc + amt);
        assert_eq!(total, charges.ingress_proof_reward);
    }

    #[test]
    fn test_publish_fee_insufficient_perm_fee() {
        let mut config = ConsensusConfig::testing();
        config.immediate_tx_inclusion_reward_percent =
            Amount::<Percentage>::percentage(dec!(0.05)).unwrap();
        config.number_of_ingress_proofs_total = 3;

        let term_fee = U256::from(1000);
        let perm_fee = U256::from(100); // Too small to cover ingress rewards (150)

        let result = PublishFeeCharges::new(perm_fee, term_fee, &config);

        // Should fail with insufficient fee error
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("insufficient to cover ingress proof rewards"));
    }

    #[test]
    fn test_publish_fee_no_base_storage_cost() {
        let mut config = ConsensusConfig::testing();
        config.immediate_tx_inclusion_reward_percent =
            Amount::<Percentage>::percentage(dec!(0.05)).unwrap();
        config.number_of_ingress_proofs_total = 3;

        let term_fee = U256::from(1000);
        // Exactly equal to ingress rewards (150), no base storage cost
        let perm_fee = U256::from(150);

        let result = PublishFeeCharges::new(perm_fee, term_fee, &config);

        // Should fail because there's no base storage cost
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must include base storage cost"));
    }

    #[test]
    fn test_publish_fee_ingress_proof_rewards_with_remainder() {
        use crate::ingress::generate_ingress_proof;
        use crate::irys::IrysSigner;

        let mut config = ConsensusConfig::testing();
        config.immediate_tx_inclusion_reward_percent =
            Amount::<Percentage>::percentage(dec!(0.05)).unwrap();

        let term_fee = U256::from(1003); // Not evenly divisible
        let perm_fee = U256::from(10000);
        let charges = PublishFeeCharges::new(perm_fee, term_fee, &config).unwrap();

        // Create 3 test ingress proofs
        let signers: Vec<_> = (0..3).map(|_| IrysSigner::random_signer(&config)).collect();
        let data_root = H256::random();

        let proofs: Vec<_> = signers
            .iter()
            .map(|signer| {
                generate_ingress_proof(
                    signer,
                    data_root,
                    vec![vec![0_u8; 32]].into_iter().map(Ok),
                    config.chain_id,
                )
                .unwrap()
            })
            .collect();

        let fee_charges = charges.ingress_proof_rewards(&proofs).unwrap();

        // Should have 3 fee charges
        assert_eq!(fee_charges.len(), 3);

        // Calculate expected values
        let base_per_proof = charges.ingress_proof_reward / U256::from(3);
        let remainder = charges.ingress_proof_reward % U256::from(3);

        // First proof should get base + remainder
        assert_eq!(fee_charges[0].amount, base_per_proof + remainder);

        // Others should get base
        assert_eq!(fee_charges[1].amount, base_per_proof);
        assert_eq!(fee_charges[2].amount, base_per_proof);

        // Total should equal ingress_proof_reward exactly
        let total: U256 = fee_charges
            .iter()
            .map(|fc| fc.amount)
            .fold(U256::from(0), |acc, amt| acc + amt);
        assert_eq!(total, charges.ingress_proof_reward);
    }
}
