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
use std::collections::BTreeMap;

/// Structure holding publish ledger transactions with their proofs
#[derive(Debug, Clone)]
pub struct PublishLedgerWithTxs {
    pub txs: Vec<DataTransactionHeader>,
    pub proofs: Option<IngressProofsList>,
}

#[derive(Debug, PartialEq)]
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
    publish_ledger: &'a mut PublishLedgerWithTxs,

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
        publish_ledger: &'a mut PublishLedgerWithTxs,
        initial_treasury_balance: U256,
    ) -> Self {
        // Sort publish ledger transactions by id for deterministic processing
        publish_ledger.txs.sort();

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
                    if let Some(result) = self.try_process_commitments().transpose() {
                        return Some(result);
                    }
                    // Continue to next iteration if phase completed
                }

                Phase::SubmitLedger => {
                    if let Some(result) = self.try_process_submit_ledger().transpose() {
                        return Some(result);
                    }
                    // Continue to next iteration if phase completed
                }

                Phase::PublishLedger => {
                    if let Some(result) = self.try_process_publish_ledger().transpose() {
                        return Some(result);
                    }
                    // Continue to next iteration if phase completed
                }

                Phase::Done => return None,
            }
        }
    }
}

impl ShadowTxGenerator<'_> {
    /// Accumulates all rewards from ingress proofs into a map
    ///
    /// We accumulate the ingress rewards per miner address,
    /// meaning, that a single miner will only have a single ingress proof shadow tx
    /// for n ingress proofs. This is to save up space on the evm block.
    ///
    /// Rolling hash is a deterministic hash used by summing up all the tx ids that were a
    /// part of computing the total ingress reward.
    /// This rolling hash is later used as `irys_ref` variable for the shadow tx.
    fn accumulate_ingress_rewards(&self) -> Result<BTreeMap<Address, (RewardAmount, RollingHash)>> {
        // BTreeMap to aggregate rewards by provider address and rolling hash (sorted by address)
        let mut rewards_map: BTreeMap<Address, (RewardAmount, RollingHash)> = BTreeMap::new();

        // Get ingress proofs if available
        let proofs = self
            .publish_ledger
            .proofs
            .as_ref()
            .map(|p| &p.0[..])
            .unwrap_or(&[]);

        // Skip processing if no proofs (nothing to reward)
        if proofs.is_empty() {
            return Ok(BTreeMap::new());
        }

        // Process all transactions (MUST BE SORTED)
        for tx in &self.publish_ledger.txs {
            // CRITICAL: All publish ledger txs MUST have perm_fee
            let perm_fee = tx
                .perm_fee
                .ok_or_else(|| eyre::eyre!("publish ledger tx missing perm_fee {}", tx.id))?;

            // Calculate fee distribution using PublishFeeCharges
            // PublishFeeCharges::new will return an error if perm_fee is insufficient
            let publish_charges = PublishFeeCharges::new(perm_fee, tx.term_fee, self.config)?;

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
                            "Underflow when calculating {} decrement amount for {}",
                            operation_type,
                            tx.id
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
                        eyre::eyre!(
                            "Underflow when calculating {} amount for {}",
                            operation_type,
                            tx.id
                        )
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
        rewards_map: BTreeMap<Address, (RewardAmount, RollingHash)>,
    ) -> Result<Vec<ShadowMetadata>> {
        // BTreeMap already maintains sorted order by address
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
            transaction_fee: term_charges
                .block_producer_reward
                .try_into()
                .map_err(|_| eyre!("Block producer reward exceeds u128 max"))?,
        })
    }

    /// Process a single submit ledger transaction with clean error handling
    #[tracing::instrument(skip_all, err)]
    fn try_process_submit_ledger(&mut self) -> Result<Option<ShadowMetadata>> {
        if self.index >= self.submit_txs.len() {
            self.phase = Phase::PublishLedger;
            self.index = 0;
            return Ok(None);
        }

        let tx = &self.submit_txs[self.index];
        self.index += 1;

        // Construct term fee charges
        let term_charges = TermFeeCharges::new(tx.term_fee, self.config)?;

        // Construct perm fee charges if applicable
        let perm_charges = tx
            .perm_fee
            .map(|perm_fee| PublishFeeCharges::new(perm_fee, tx.term_fee, self.config))
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
    #[tracing::instrument(skip_all, err)]
    fn try_process_commitments(&mut self) -> Result<Option<ShadowMetadata>> {
        if self.index >= self.commitment_txs.len() {
            self.phase = Phase::SubmitLedger;
            self.index = 0;
            return Ok(None);
        }

        let tx = &self.commitment_txs[self.index];
        self.index += 1;

        // Process commitment transaction
        let shadow_metadata = self.process_commitment_transaction(tx)?;

        // Update treasury based on commitment type
        match tx.commitment_type {
            irys_primitives::CommitmentType::Stake
            | irys_primitives::CommitmentType::Pledge { .. } => {
                // Stake and Pledge lock funds in the treasury
                self.treasury_balance =
                    self.treasury_balance.checked_add(tx.value).ok_or_else(|| {
                        eyre!("Treasury balance overflow when adding commitment value")
                    })?;
            }
            irys_primitives::CommitmentType::Unstake
            | irys_primitives::CommitmentType::Unpledge { .. } => {
                self.treasury_balance =
                    self.treasury_balance.checked_sub(tx.value).ok_or_else(|| {
                        eyre!("Treasury balance underflow when releasing commitment value")
                    })?;
            }
        }

        Ok(Some(shadow_metadata))
    }

    /// Process publish ledger phase with clean error handling
    #[tracing::instrument(skip_all, err)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use irys_primitives::CommitmentType;
    use irys_types::{
        ingress::IngressProof, irys::IrysSigner, ConsensusConfig, IrysBlockHeader, IrysSignature,
        Signature, H256,
    };
    use itertools::Itertools as _;
    use openssl::sha;

    fn create_test_commitment(
        commitment_type: CommitmentType,
        value: U256,
        fee: u64,
    ) -> CommitmentTransaction {
        let config = ConsensusConfig::testing();
        let signer = IrysSigner::random_signer(&config);
        CommitmentTransaction {
            id: H256::from([7_u8; 32]),
            commitment_type,
            anchor: H256::from([8_u8; 32]),
            signer: signer.address(),
            value,
            fee,
            signature: IrysSignature::new(Signature::try_from([0_u8; 65].as_slice()).unwrap()),
            version: 1,
            chain_id: config.chain_id,
        }
    }

    fn create_data_tx_header(
        signer: &IrysSigner,
        term_fee: U256,
        perm_fee: Option<U256>,
    ) -> DataTransactionHeader {
        let data = vec![0_u8; 1024];
        let anchor = Some(H256::from([9_u8; 32]));

        // Always create with perm_fee for publish ledger (ledger_id = 0)
        // The tests simulate the actual usage where submit txs have been promoted to publish
        let actual_perm_fee = perm_fee.unwrap_or_else(|| {
            // If no perm_fee specified, calculate minimum required for ingress proofs
            let config = ConsensusConfig::testing();
            let ingress_reward_per_proof = (term_fee
                * config.immediate_tx_inclusion_reward_percent.amount)
                / U256::from(10000);
            let total_ingress_reward =
                ingress_reward_per_proof * U256::from(config.number_of_ingress_proofs);
            U256::from(1000000) + total_ingress_reward
        });

        let tx = signer
            .create_publish_transaction(data, anchor, actual_perm_fee, term_fee)
            .expect("Failed to create publish transaction");

        // Modify the header to reflect the original perm_fee intent
        let mut header = tx.header;
        header.perm_fee = perm_fee;
        header
    }

    fn create_test_ingress_proof(signer: &IrysSigner, data_root: H256) -> IngressProof {
        // Create proof hash
        let proof = H256::from([12_u8; 32]);
        let chain_id = 1_u64;

        // Create the message that would be signed
        let mut hasher = sha::Sha256::new();
        hasher.update(&proof.0);
        hasher.update(&data_root.0);
        hasher.update(&chain_id.to_be_bytes());
        let prehash = hasher.finish();

        // Sign the message with the signer's internal signing key
        // Note: sign_prehash_recoverable is a method on k256::ecdsa::SigningKey
        let signature: Signature = signer
            .signer
            .sign_prehash_recoverable(&prehash)
            .unwrap()
            .into();

        IngressProof {
            signature: IrysSignature::new(signature),
            data_root,
            proof,
            chain_id,
        }
    }

    #[test]
    fn test_header_only() {
        let config = ConsensusConfig::testing();
        let parent_block = IrysBlockHeader::new_mock_header();
        let block_height = 101;
        let reward_address = Address::from([20_u8; 20]);
        let reward_amount = U256::from(5000);
        let initial_treasury = U256::from(2000000);
        let mut publish_ledger = PublishLedgerWithTxs {
            txs: vec![],
            proofs: None,
        };

        // Create expected shadow transactions
        let expected_shadow_txs: Vec<ShadowMetadata> = vec![ShadowMetadata {
            shadow_tx: ShadowTransaction::new_v1(TransactionPacket::BlockReward(
                BlockRewardIncrement {
                    amount: reward_amount.into(),
                },
            )),
            transaction_fee: 0,
        }];

        let generator = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &config,
            &[],
            &[],
            &mut publish_ledger,
            initial_treasury,
        );

        // Compare actual with expected
        generator
            .zip_eq(expected_shadow_txs)
            .for_each(|(actual, expected)| {
                let actual = actual.expect("Should be Ok");
                assert_eq!(actual, expected);
            });
    }

    #[test]
    fn test_three_commitments() {
        let config = ConsensusConfig::testing();
        let parent_block = IrysBlockHeader::new_mock_header();
        let block_height = 101;
        let reward_address = Address::from([20_u8; 20]);
        let reward_amount = U256::from(5000);
        let initial_treasury = U256::from(2000000);

        let commitments = vec![
            create_test_commitment(CommitmentType::Stake, U256::from(100000), 1000),
            create_test_commitment(
                CommitmentType::Pledge {
                    pledge_count_before_executing: 2,
                },
                U256::from(200000),
                2000,
            ),
            create_test_commitment(CommitmentType::Unstake, U256::from(150000), 500),
            create_test_commitment(
                CommitmentType::Unpledge {
                    pledge_count_before_executing: 1,
                },
                U256::from(180000),
                1500,
            ),
        ];

        let mut publish_ledger = PublishLedgerWithTxs {
            txs: vec![],
            proofs: None,
        };

        // Create expected shadow transactions for all commitment types
        let expected_shadow_txs: Vec<ShadowMetadata> = vec![
            // Block reward
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::BlockReward(
                    BlockRewardIncrement {
                        amount: reward_amount.into(),
                    },
                )),
                transaction_fee: 0,
            },
            // Stake
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Stake(BalanceDecrement {
                    amount: U256::from(101000).into(), // 100000 + 1000 fee
                    target: commitments[0].signer,
                    irys_ref: commitments[0].id.into(),
                })),
                transaction_fee: 1000,
            },
            // Pledge
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Pledge(BalanceDecrement {
                    amount: U256::from(202000).into(), // 200000 + 2000 fee
                    target: commitments[1].signer,
                    irys_ref: commitments[1].id.into(),
                })),
                transaction_fee: 2000,
            },
            // Unstake (150000 - 500 fee = 149500 increment)
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Unstake(
                    EitherIncrementOrDecrement::BalanceIncrement(BalanceIncrement {
                        amount: U256::from(149500).into(), // 150000 - 500 fee
                        target: commitments[2].signer,
                        irys_ref: commitments[2].id.into(),
                    }),
                )),
                transaction_fee: 500,
            },
            // Unpledge (180000 - 1500 fee = 178500 increment)
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Unpledge(
                    EitherIncrementOrDecrement::BalanceIncrement(BalanceIncrement {
                        amount: U256::from(178500).into(), // 180000 - 1500 fee
                        target: commitments[3].signer,
                        irys_ref: commitments[3].id.into(),
                    }),
                )),
                transaction_fee: 1500,
            },
        ];

        let generator = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &config,
            &commitments,
            &[],
            &mut publish_ledger,
            initial_treasury,
        );

        // Compare actual with expected
        generator
            .zip_eq(expected_shadow_txs)
            .for_each(|(actual, expected)| {
                let actual = actual.expect("Should be Ok");
                assert_eq!(actual, expected);
            });
    }

    #[test]
    fn test_one_submit_tx() {
        let config = ConsensusConfig::testing();
        let parent_block = IrysBlockHeader::new_mock_header();
        let signer = IrysSigner::random_signer(&config);

        let term_fee = U256::from(20000);
        let submit_tx = create_data_tx_header(&signer, term_fee, None);
        let submit_txs = vec![submit_tx.clone()];

        let block_height = 101;
        let reward_address = Address::from([20_u8; 20]);
        let reward_amount = U256::from(5000);
        let initial_treasury = U256::from(2000000);
        let mut publish_ledger = PublishLedgerWithTxs {
            txs: vec![],
            proofs: None,
        };

        // Calculate expected values
        let term_charges = TermFeeCharges::new(term_fee, &config).unwrap();

        // Create expected shadow transactions directly
        let expected_shadow_txs: Vec<ShadowMetadata> = vec![
            // Block reward
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::BlockReward(
                    BlockRewardIncrement {
                        amount: reward_amount.into(),
                    },
                )),
                transaction_fee: 0,
            },
            // Storage fee for the submit transaction
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::StorageFees(
                    BalanceDecrement {
                        amount: submit_tx.total_cost().into(),
                        target: submit_tx.signer,
                        irys_ref: submit_tx.id.into(),
                    },
                )),
                transaction_fee: term_charges
                    .block_producer_reward
                    .try_into()
                    .expect("Block producer reward should fit in u128"),
            },
        ];

        let mut generator = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &config,
            &[],
            &submit_txs,
            &mut publish_ledger,
            initial_treasury,
        );

        // Compare actual with expected
        generator
            .by_ref()
            .zip_eq(expected_shadow_txs)
            .for_each(|(actual, expected)| {
                let actual = actual.expect("Should be Ok");
                assert_eq!(actual, expected);
            });

        // Verify treasury increased by the expected amount
        let expected_treasury = initial_treasury + term_charges.term_fee_treasury;
        assert_eq!(generator.treasury_balance(), expected_treasury);
    }

    #[test]
    fn test_btreemap_maintains_sorted_order() {
        // Quick test to verify BTreeMap maintains sorted order
        let mut rewards_map: BTreeMap<Address, u32> = BTreeMap::new();

        // Insert addresses in random order
        let addr1 = Address::from([5_u8; 20]);
        let addr2 = Address::from([1_u8; 20]);
        let addr3 = Address::from([9_u8; 20]);

        rewards_map.insert(addr3, 3);
        rewards_map.insert(addr1, 1);
        rewards_map.insert(addr2, 2);

        // Verify they come out sorted
        let addresses: Vec<Address> = rewards_map.keys().copied().collect();
        assert_eq!(addresses[0], addr2); // Smallest address first
        assert_eq!(addresses[1], addr1);
        assert_eq!(addresses[2], addr3); // Largest address last
    }

    #[test]
    fn test_one_publish_tx_with_aggregated_proofs() {
        let config = ConsensusConfig::testing();
        let parent_block = IrysBlockHeader::new_mock_header();

        // Calculate proper fees for publish transaction
        let term_fee = U256::from(30000);
        // We need to account for 4 proofs now
        let ingress_reward_per_proof =
            (term_fee * config.immediate_tx_inclusion_reward_percent.amount) / U256::from(10000);
        let total_ingress_reward = ingress_reward_per_proof * U256::from(4); // 4 proofs total
        let perm_fee = U256::from(1000000) + total_ingress_reward;

        // Create transaction signer
        let tx_signer = IrysSigner::random_signer(&config);
        let publish_tx = create_data_tx_header(&tx_signer, term_fee, Some(perm_fee));
        let submit_txs = vec![publish_tx.clone()];

        // Create three different proof signers
        let proof_signer1 = IrysSigner::random_signer(&config);
        let proof_signer2 = IrysSigner::random_signer(&config);
        let proof_signer3 = IrysSigner::random_signer(&config);

        // Create 4 proofs - signer2 has 2 proofs to test aggregation
        let proofs = vec![
            create_test_ingress_proof(&proof_signer1, H256::from([10_u8; 32])),
            create_test_ingress_proof(&proof_signer2, H256::from([11_u8; 32])),
            create_test_ingress_proof(&proof_signer3, H256::from([12_u8; 32])),
            create_test_ingress_proof(&proof_signer2, H256::from([13_u8; 32])), // Extra proof for signer2
        ];

        let block_height = 101;
        let reward_address = Address::from([20_u8; 20]);
        let reward_amount = U256::from(5000);
        let initial_treasury = U256::from(20000000);
        let mut publish_ledger = PublishLedgerWithTxs {
            txs: submit_txs.clone(),
            proofs: Some(IngressProofsList(proofs)),
        };

        // Calculate expected values
        let term_charges = TermFeeCharges::new(term_fee, &config).unwrap();

        // Since perm_fee was calculated with 4 proofs in mind
        let publish_charges = PublishFeeCharges::new(perm_fee, term_fee, &config).unwrap();

        // Calculate individual ingress rewards (4 proofs total)
        let base_reward_per_proof = publish_charges.ingress_proof_reward / U256::from(4);
        let remainder = publish_charges.ingress_proof_reward % U256::from(4);

        // Calculate aggregated rewards per signer
        // signer1: 1 proof = base_reward + remainder (first proof gets remainder)
        // signer2: 2 proofs = base_reward * 2
        // signer3: 1 proof = base_reward
        let signer1_reward = base_reward_per_proof + remainder;
        let signer2_reward = base_reward_per_proof * U256::from(2);
        let signer3_reward = base_reward_per_proof;

        // Sort signers by address for deterministic ordering
        let mut signer_rewards = [
            (proof_signer1.address(), signer1_reward),
            (proof_signer2.address(), signer2_reward),
            (proof_signer3.address(), signer3_reward),
        ];
        signer_rewards.sort_by_key(|(addr, _)| *addr);

        // Create expected shadow transactions directly
        let expected_shadow_txs: Vec<ShadowMetadata> = vec![
            // Block reward
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::BlockReward(
                    BlockRewardIncrement {
                        amount: reward_amount.into(),
                    },
                )),
                transaction_fee: 0,
            },
            // Storage fee for the publish transaction
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::StorageFees(
                    BalanceDecrement {
                        amount: publish_tx.total_cost().into(),
                        target: publish_tx.signer,
                        irys_ref: publish_tx.id.into(),
                    },
                )),
                transaction_fee: term_charges
                    .block_producer_reward
                    .try_into()
                    .expect("Block producer reward should fit in u128"),
            },
            // Ingress proof rewards (aggregated by signer, sorted by address)
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::IngressProofReward(
                    BalanceIncrement {
                        amount: signer_rewards[0].1.into(),
                        target: signer_rewards[0].0,
                        irys_ref: H256::from(publish_tx.id.0).into(),
                    },
                )),
                transaction_fee: 0,
            },
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::IngressProofReward(
                    BalanceIncrement {
                        amount: signer_rewards[1].1.into(),
                        target: signer_rewards[1].0,
                        irys_ref: H256::from(publish_tx.id.0).into(),
                    },
                )),
                transaction_fee: 0,
            },
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::IngressProofReward(
                    BalanceIncrement {
                        amount: signer_rewards[2].1.into(),
                        target: signer_rewards[2].0,
                        irys_ref: H256::from(publish_tx.id.0).into(),
                    },
                )),
                transaction_fee: 0,
            },
        ];

        let generator = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &config,
            &[],
            &submit_txs,
            &mut publish_ledger,
            initial_treasury,
        );

        // Compare actual with expected
        generator
            .zip_eq(expected_shadow_txs)
            .for_each(|(actual, expected)| {
                let actual = actual.expect("Should be Ok");
                assert_eq!(actual, expected);
            });
    }
}
