use eyre::{eyre, Result};
use irys_reth::shadow_tx::{
    BalanceDecrement, BalanceIncrement, BlockRewardIncrement, EitherIncrementOrDecrement,
    ShadowTransaction, TransactionPacket,
};
use irys_types::{
    transaction::fee_distribution::{PublishFeeCharges, TermFeeCharges},
    Address, CommitmentTransaction, ConsensusConfig, DataTransactionHeader, IngressProofsList,
    IrysBlockHeader, IrysTransactionCommon as _, U256,
};
use reth::revm::primitives::ruint::Uint;
use std::collections::HashMap;

/// Structure holding publish ledger transactions with their proofs
pub struct PublishLedgerWithTxs {
    pub txs: Vec<DataTransactionHeader>,
    pub proofs: Option<IngressProofsList>,
}

#[derive(Debug)]
pub struct ShadowMetadata {
    pub shadow_tx: ShadowTransaction,
    pub transaction_fee: u128,
}

pub struct ShadowTxGenerator<'a> {
    pub block_height: &'a u64,
    pub reward_address: &'a Address,
    pub reward_amount: &'a U256,
    pub parent_block: &'a IrysBlockHeader,
    pub config: &'a ConsensusConfig,

    // Transaction slices
    commitment_txs: &'a [CommitmentTransaction],
    submit_txs: &'a [DataTransactionHeader],
    publish_ledger: &'a PublishLedgerWithTxs,

    // Iterator state
    treasury_balance: U256,
    phase: Phase,
    index: usize,
    // Current publish ledger iterator (if processing publish ledger)
    current_publish_iter: Option<std::vec::IntoIter<Result<ShadowMetadata>>>,
}

impl<'a> ShadowTxGenerator<'a> {
    pub fn new(
        block_height: &'a u64,
        reward_address: &'a Address,
        reward_amount: &'a U256,
        parent_block: &'a IrysBlockHeader,
        config: &'a ConsensusConfig,
        commitment_txs: &'a [CommitmentTransaction],
        submit_txs: &'a [DataTransactionHeader],
        publish_ledger: &'a PublishLedgerWithTxs,
        initial_treasury_balance: U256,
    ) -> Self {
        Self {
            block_height,
            reward_address,
            reward_amount,
            parent_block,
            config,
            commitment_txs,
            submit_txs,
            publish_ledger,
            treasury_balance: initial_treasury_balance,
            phase: Phase::Header,
            index: 0,
            current_publish_iter: None,
        }
    }

    /// Get the current treasury balance
    pub fn treasury_balance(&self) -> U256 {
        self.treasury_balance
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Header,
    Commitments,
    SubmitLedger,
    PublishLedger,
    Done,
}

impl Iterator for ShadowTxGenerator<'_> {
    type Item = Result<ShadowMetadata>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.phase {
                Phase::Header => {
                    self.phase = Phase::Commitments;
                    // Block reward has no treasury impact
                    return Some(Ok(ShadowMetadata {
                        shadow_tx: ShadowTransaction::new_v1(TransactionPacket::BlockReward(
                            BlockRewardIncrement {
                                amount: (*self.reward_amount).into(),
                            },
                        )),
                        transaction_fee: 0,
                    }));
                }

                Phase::Commitments => {
                    return self.try_process_commitments().transpose();
                }

                Phase::SubmitLedger => {
                    return self.try_process_submit_ledger().transpose();
                }

                Phase::PublishLedger => {
                    return self.try_process_publish_ledger().transpose();
                }

                Phase::Done => return None,
            }
        }
    }
}

impl ShadowTxGenerator<'_> {
    /// Accumulates all rewards from ingress proofs into a map
    fn accumulate_ingress_rewards(&self) -> Result<HashMap<Address, (RewardAmount, RollingHash)>> {
        // HashMap to aggregate rewards by provider address and rolling hash
        let mut rewards_map: HashMap<Address, (RewardAmount, RollingHash)> = HashMap::new();

        // Get ingress proofs if available
        let proofs = self
            .publish_ledger
            .proofs
            .as_ref()
            .map(|p| &p.0[..])
            .unwrap_or(&[]);

        // Skip processing if no proofs (nothing to reward)
        if proofs.is_empty() {
            return Ok(HashMap::new());
        }

        // Process all transactions and aggregate rewards
        for tx in &self.publish_ledger.txs {
            // CRITICAL: All publish ledger txs MUST have perm_fee
            let perm_fee = match tx.perm_fee {
                Some(fee) => fee,
                None => {
                    return Err(eyre!(
                        "Critical: publish ledger tx {} missing perm_fee",
                        tx.id
                    ));
                }
            };

            // Calculate fee distribution using PublishFeeCharges
            // PublishFeeCharges::new will return an error if perm_fee is insufficient
            let publish_charges =
                PublishFeeCharges::new(U256::from(perm_fee), U256::from(tx.term_fee), self.config)?;

            // Get fee charges for all ingress proofs
            let fee_charges = publish_charges.ingress_proof_rewards(proofs)?;

            // Aggregate rewards by address and update rolling hash
            for charge in fee_charges {
                let entry = rewards_map
                    .entry(charge.address)
                    .or_insert((RewardAmount::zero(), RollingHash::zero()));
                entry.0.add_assign(charge.amount); // Add to total amount
                                                   // XOR the rolling hash with the transaction ID
                entry.1.xor_assign(U256::from_be_bytes(tx.id.0));
            }
        }

        Ok(rewards_map)
    }

    fn process_commitment_transaction(&self, tx: &CommitmentTransaction) -> Result<ShadowMetadata> {
        // Keep existing commitment transaction logic unchanged
        let commitment_value = Uint::from_le_bytes(tx.commitment_value().to_le_bytes());
        let fee = Uint::from(tx.fee);
        let total_cost = Uint::from_le_bytes(tx.total_cost().to_le_bytes());

        let create_increment_or_decrement =
            |operation_type: &str| -> Result<EitherIncrementOrDecrement> {
                if fee > commitment_value {
                    let amount = fee.checked_sub(commitment_value).ok_or_else(|| {
                        eyre::eyre!(
                            "Underflow when calculating {} decrement amount",
                            operation_type
                        )
                    })?;
                    Ok(EitherIncrementOrDecrement::BalanceDecrement(
                        BalanceDecrement {
                            amount,
                            target: tx.signer,
                            irys_ref: tx.id.into(),
                        },
                    ))
                } else {
                    let amount = commitment_value.checked_sub(fee).ok_or_else(|| {
                        eyre::eyre!("Underflow when calculating {} amount", operation_type)
                    })?;
                    Ok(EitherIncrementOrDecrement::BalanceIncrement(
                        BalanceIncrement {
                            amount,
                            target: tx.signer,
                            irys_ref: tx.id.into(),
                        },
                    ))
                }
            };

        let transaction_fee = tx.fee as u128;

        match tx.commitment_type {
            irys_primitives::CommitmentType::Stake => Ok(ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Stake(BalanceDecrement {
                    amount: total_cost,
                    target: tx.signer,
                    irys_ref: tx.id.into(),
                })),
                transaction_fee,
            }),
            irys_primitives::CommitmentType::Pledge { .. } => Ok(ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Pledge(BalanceDecrement {
                    amount: total_cost,
                    target: tx.signer,
                    irys_ref: tx.id.into(),
                })),
                transaction_fee,
            }),
            irys_primitives::CommitmentType::Unpledge { .. } => {
                create_increment_or_decrement("unpledge").map(|result| ShadowMetadata {
                    shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Unpledge(result)),
                    transaction_fee,
                })
            }
            irys_primitives::CommitmentType::Unstake => create_increment_or_decrement("unstake")
                .map(|result| ShadowMetadata {
                    shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Unstake(result)),
                    transaction_fee,
                }),
        }
    }

    /// Creates shadow transactions from aggregated rewards
    fn create_publish_shadow_txs(
        &self,
        rewards_map: HashMap<Address, (RewardAmount, RollingHash)>,
    ) -> Result<Vec<ShadowMetadata>> {
        let shadow_txs: Vec<ShadowMetadata> = rewards_map
            .into_iter()
            .map(|(address, (reward_amount, rolling_hash))| {
                // Convert the rolling hash to FixedBytes<32> for irys_ref
                let hash_bytes = rolling_hash.to_bytes();
                let h256 = irys_types::H256::from(hash_bytes);
                let irys_ref = h256.into();

                // Extract the inner U256 from RewardAmount
                let total_amount = reward_amount.into_inner();

                ShadowMetadata {
                    shadow_tx: ShadowTransaction::new_v1(TransactionPacket::IngressProofReward(
                        BalanceIncrement {
                            amount: Uint::from_le_bytes(total_amount.to_le_bytes()),
                            target: address,
                            irys_ref,
                        },
                    )),
                    transaction_fee: 0, // No block producer reward for ingress proofs
                }
            })
            .collect();

        Ok(shadow_txs)
    }

    /// Creates a shadow transaction for a submit ledger transaction
    fn create_submit_shadow_tx(
        &self,
        tx: &DataTransactionHeader,
        term_charges: &TermFeeCharges,
    ) -> Result<ShadowMetadata> {
        // Create shadow transaction for total cost deduction
        let total_cost = tx.total_cost();
        Ok(ShadowMetadata {
            shadow_tx: ShadowTransaction::new_v1(TransactionPacket::StorageFees(
                BalanceDecrement {
                    amount: Uint::from_le_bytes(total_cost.to_le_bytes()),
                    target: tx.signer,
                    irys_ref: tx.id.into(),
                },
            )),
            // Block producer gets their reward via transaction_fee
            transaction_fee: term_charges.block_producer_reward.low_u128(),
        })
    }

    /// Process a single submit ledger transaction with clean error handling
    fn try_process_submit_ledger(&mut self) -> Result<Option<ShadowMetadata>> {
        if self.index >= self.submit_txs.len() {
            self.phase = Phase::PublishLedger;
            self.index = 0;
            return Ok(None);
        }

        let tx = &self.submit_txs[self.index];
        self.index += 1;

        // Construct term fee charges
        let term_charges = TermFeeCharges::new(U256::from(tx.term_fee), self.config)?;

        // Construct perm fee charges if applicable
        let perm_charges = tx
            .perm_fee
            .map(|perm_fee| {
                PublishFeeCharges::new(U256::from(perm_fee), U256::from(tx.term_fee), self.config)
            })
            .transpose()?;

        // Create shadow transaction
        let shadow_metadata = self.create_submit_shadow_tx(tx, &term_charges)?;

        // Update treasury with checked arithmetic
        self.treasury_balance = self
            .treasury_balance
            .checked_add(term_charges.term_fee_treasury)
            .ok_or_else(|| eyre!("Treasury balance overflow when adding term fee treasury"))?;

        if let Some(ref charges) = perm_charges {
            self.treasury_balance = self
                .treasury_balance
                .checked_add(charges.perm_fee_treasury)
                .ok_or_else(|| eyre!("Treasury balance overflow when adding perm fee treasury"))?;
        }

        Ok(Some(shadow_metadata))
    }

    /// Process commitments phase with clean error handling
    fn try_process_commitments(&mut self) -> Result<Option<ShadowMetadata>> {
        if self.index >= self.commitment_txs.len() {
            self.phase = Phase::SubmitLedger;
            self.index = 0;
            return Ok(None);
        }

        let tx = &self.commitment_txs[self.index];
        self.index += 1;

        // Process commitment transaction (no treasury impact currently)
        Ok(Some(self.process_commitment_transaction(tx)?))
    }

    /// Process publish ledger phase with clean error handling
    fn try_process_publish_ledger(&mut self) -> Result<Option<ShadowMetadata>> {
        // On first entry to PublishLedger phase, prepare all rewards
        if self.current_publish_iter.is_none() {
            // Accumulate all rewards from ingress proofs
            let aggregated_rewards = self.accumulate_ingress_rewards()?;

            if aggregated_rewards.is_empty() {
                // No rewards to process, move to Done
                self.phase = Phase::Done;
                return Ok(None);
            }

            // Construct all shadow txs
            let shadow_txs = self.create_publish_shadow_txs(aggregated_rewards)?;

            self.current_publish_iter = Some(
                shadow_txs
                    .into_iter()
                    .map(Ok)
                    .collect::<Vec<_>>()
                    .into_iter(),
            );
        }

        // Yield shadow txs and update treasury balance
        if let Some(ref mut iter) = self.current_publish_iter {
            if let Some(result) = iter.next() {
                // Update treasury balance with checked arithmetic
                if let Ok(ref metadata) = result {
                    if let ShadowTransaction::V1 {
                        packet: TransactionPacket::IngressProofReward(increment),
                        ..
                    } = &metadata.shadow_tx
                    {
                        self.treasury_balance = self
                            .treasury_balance
                            .checked_sub(U256::from(increment.amount))
                            .ok_or_else(|| {
                                eyre!("Treasury balance underflow when paying ingress proof reward")
                            })?;
                    }
                }
                // result is already Result<ShadowMetadata, _>, wrap in Some
                return Ok(Some(result?));
            }
        }

        // All rewards processed, move to Done
        self.phase = Phase::Done;
        Ok(None)
    }
}

/// Newtype for reward amounts to prevent mixing with other U256 values
#[derive(Debug, Clone, Copy, Default)]
struct RewardAmount(U256);

impl RewardAmount {
    fn zero() -> Self {
        Self(U256::zero())
    }

    fn add_assign(&mut self, amount: U256) {
        self.0 += amount;
    }

    fn into_inner(self) -> U256 {
        self.0
    }
}

/// Newtype for rolling hash to prevent mixing with other U256 values
#[derive(Debug, Clone, Copy, Default)]
struct RollingHash(U256);

impl RollingHash {
    fn zero() -> Self {
        Self(U256::zero())
    }

    fn xor_assign(&mut self, value: U256) {
        self.0 ^= value;
    }

    fn to_bytes(self) -> [u8; 32] {
        self.0.to_be_bytes()
    }
}
