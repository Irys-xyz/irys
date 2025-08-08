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
/// - term_fee: Standard fee for term storage
/// - perm_fee: Fee for permanent storage
/// - 5% of term_fee for block inclusion
/// - 5% of term_fee for each ingress-proof
///
/// ### Fee Distribution
/// - term_fee: Processed identically to regular term data transactions.
/// - 5% of term_fee for each ingress-proof provided. The value is tracked on transactions `prem_fee` field.
/// - perm_fee base value: Prepaid amount covering 200 years x 10 replicas with 1% annual decline in storage costs. This is added to the treasury
use crate::ingress::IngressProof;
pub use crate::{
    address_base58_stringify, optional_string_u64, string_u64, Address, Arbitrary, Base64, Compact,
    ConsensusConfig, IrysSignature, Node, Proof, Signature, TxIngressProof, H256, U256,
};
use alloy_primitives::keccak256;
use alloy_rlp::{Encodable as _, RlpDecodable, RlpEncodable};
pub use irys_primitives::CommitmentType;
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
    pub fn new(term_fee: U256, config: &ConsensusConfig) -> Self {
        todo!("x% fee percent (from config) goes as the block reward ");
        todo!("The rest of the fee goes to the treasury");
        todo!("assert that the sum of all the fields in Self is equal to `term_fee`")
    }

    pub fn distribution_on_expiry(&self, miners: &[Address]) -> eyre::Result<U256> {
        todo!("When the partition expires we distribute the remainder of the `term_fee` (which is located in `term_fee_treasury` over to all the miners)");
        todo!("assert that the sum of all the fees we're distributing is equal to `term_fee_treasury`")
    }
}

/// Represents the complete perm fee distribution for a data transaction
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishFeeCharges {
    /// Rewards to each ingress proof provider
    pub ingress_proof_reward: Vec<U256>,

    /// Full perm_fee that goes to treasury for permanent storage
    pub perm_fee_treasury: U256,
}

impl PublishFeeCharges {
    pub fn new(prem_fee: U256, term_fee: U256, config: &ConsensusConfig) -> Self {
        todo!("x% fee percent (from config) must be allocated for each ingress proof (number of ingress proofs required must be read from the config)");
        todo!("The prem_fee already includes `base perm fee + ((x% * term_fee) * ingress_proof_count)`");
        todo!("The rest of the perm_fee goes to the treasury");
        todo!("assert that the sum of all the fields in Self is equal to `perm_fee`")
    }

    /// Provided a list of ingress proofs, figure out the fee allocations for each of them
    pub fn ingress_proof_rewards(
        &self,
        ingress_proofs: &[IngressProof],
    ) -> eyre::Result<Vec<FeeCharge>> {
        todo!("get the address from the ingress proof; use .zip_longest and raise an error if there's a len mismatch between the proofs and reward len")
    }
}
