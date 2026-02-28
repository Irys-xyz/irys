use eyre::{Result, eyre};
use irys_domain::EpochSnapshot;
use irys_reth::shadow_tx::{
    BalanceDecrement, BalanceIncrement, BlockRewardIncrement, IrysUsdPriceUpdate, PdBaseFeeUpdate,
    ShadowTransaction, TransactionPacket, TreasuryDeposit, UnstakeDebit,
};
use irys_types::{
    BoundedFee, CommitmentTransaction, ConsensusConfig, DataTransactionHeader, H256,
    IngressProofsList, IrysAddress, IrysBlockHeader, IrysTokenPrice, U256, UnixTimestamp,
    storage_pricing::{
        Amount,
        phantoms::{CostPerChunk, Irys},
    },
    transaction::fee_distribution::{PublishFeeCharges, TermFeeCharges},
};
use reth::revm::primitives::ruint::Uint;
use std::collections::BTreeMap;

use crate::block_producer::ledger_expiry::LedgerExpiryBalanceDelta;
use crate::block_producer::{UnpledgeRefundEvent, UnstakeRefundEvent};

/// Structure holding publish ledger transactions with their proofs
#[derive(Debug, Clone, Default)]
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
    pub reward_address: &'a IrysAddress,
    pub reward_amount: &'a U256,
    pub parent_block: &'a IrysBlockHeader,
    pub solution_hash: &'a H256,
    pub config: &'a ConsensusConfig,

    // Transaction slices
    commitment_txs: &'a [CommitmentTransaction],
    submit_txs: &'a [DataTransactionHeader],

    // PD base fee per chunk (None for pre-Sprite blocks, Some for Sprite blocks)
    pd_base_fee_per_chunk: Option<Amount<(CostPerChunk, Irys)>>,

    // IRYS/USD price for IrysUsdPriceUpdate shadow tx
    irys_usd_price: IrysTokenPrice,

    // Block timestamp for hardfork checks
    block_timestamp: UnixTimestamp,

    // Hardfork mode flags
    is_sprite_active: bool,
    is_first_sprite_block: bool,

    // Initial treasury balance (used for TreasuryDeposit on first Sprite block)
    initial_treasury_balance: U256,

    // Iterator state
    treasury_balance: U256,
    phase: Phase,
    index: usize,
    // Current publish ledger iterator
    current_publish_iter: std::vec::IntoIter<Result<ShadowMetadata>>,
    // Current expired ledger fees iterator
    current_expired_ledger_iter: std::vec::IntoIter<Result<ShadowMetadata>>,
    // Current commitment refunds iterator (epoch-only)
    current_commitment_refunds_iter: std::vec::IntoIter<Result<ShadowMetadata>>,
}

impl Iterator for ShadowTxGenerator<'_> {
    type Item = Result<ShadowMetadata>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.phase {
                Phase::Header => {
                    self.phase = Phase::TreasuryDeposit;
                    self.index = 0;
                    // Block reward has no treasury impact
                    return Some(Ok(ShadowMetadata {
                        shadow_tx: ShadowTransaction::new_v1(
                            TransactionPacket::BlockReward(BlockRewardIncrement {
                                amount: (*self.reward_amount).into(),
                            }),
                            (*self.solution_hash).into(),
                        ),
                        transaction_fee: 0,
                    }));
                }

                Phase::TreasuryDeposit => {
                    self.phase = Phase::PdBaseFee;
                    // Only emit TreasuryDeposit on the first Sprite block
                    // This initializes the EVM's TREASURY_ACCOUNT with the pre-Sprite tracked balance
                    if self.is_first_sprite_block {
                        return Some(Ok(ShadowMetadata {
                            shadow_tx: ShadowTransaction::new_v1(
                                TransactionPacket::TreasuryDeposit(TreasuryDeposit {
                                    amount: self.initial_treasury_balance.into(),
                                }),
                                (*self.solution_hash).into(),
                            ),
                            transaction_fee: 0,
                        }));
                    }
                    // Not first Sprite block: skip to next phase
                }

                Phase::PdBaseFee => {
                    self.phase = Phase::IrysUsdPrice;
                    self.index = 0;
                    // Only emit PdBaseFeeUpdate if we have a base fee (Sprite active)
                    if let Some(base_fee) = &self.pd_base_fee_per_chunk {
                        // PD base fee update has no treasury impact
                        return Some(Ok(ShadowMetadata {
                            shadow_tx: ShadowTransaction::new_v1(
                                TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                                    per_chunk: base_fee.amount.into(),
                                }),
                                (*self.solution_hash).into(),
                            ),
                            transaction_fee: 0,
                        }));
                    }
                    // None: skip PdBaseFeeUpdate and continue to next phase
                }

                Phase::IrysUsdPrice => {
                    self.phase = Phase::Commitments;
                    self.index = 0;
                    // Only emit IrysUsdPriceUpdate if Sprite hardfork is active
                    if self.config.hardforks.is_sprite_active(self.block_timestamp) {
                        // Convert IrysTokenPrice to reth Uint<256, 4> at emission time
                        let reth_price =
                            Uint::from_be_bytes(self.irys_usd_price.amount.to_be_bytes());
                        return Some(Ok(ShadowMetadata {
                            shadow_tx: ShadowTransaction::new_v1(
                                TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                                    price: reth_price,
                                }),
                                (*self.solution_hash).into(),
                            ),
                            transaction_fee: 0,
                        }));
                    }
                    // Pre-Sprite: skip IrysUsdPriceUpdate and continue to next phase
                }

                Phase::Commitments => {
                    // Check if we have more commitments to process
                    if self.index < self.commitment_txs.len() {
                        let result = self.try_process_commitment_at_index(self.index);
                        self.index += 1;
                        return Some(result);
                    }
                    // Move to next phase
                    self.phase = Phase::SubmitLedger;
                    self.index = 0;
                }

                Phase::SubmitLedger => {
                    // Check if we have more submit transactions to process
                    if self.index < self.submit_txs.len() {
                        let result = self.try_process_submit_at_index(self.index);
                        self.index += 1;
                        return Some(result);
                    }
                    // Move to next phase
                    self.phase = Phase::ExpiredLedgerFees;
                    self.index = 0;
                }

                Phase::ExpiredLedgerFees => {
                    // Process expired ledger fees with treasury updates
                    if let Some(result) = self.try_process_expired_ledger().transpose() {
                        return Some(result);
                    }
                    // Move to next phase
                    self.phase = Phase::PublishLedger;
                }

                Phase::PublishLedger => {
                    // Process publish ledger with treasury updates
                    if let Some(result) = self.try_process_publish_ledger().transpose() {
                        return Some(result);
                    }
                    // Move to commitment refunds phase (epoch only; otherwise empty)
                    self.phase = Phase::CommitmentRefunds;
                }

                Phase::CommitmentRefunds => {
                    if let Some(result) = self.try_process_commitment_refunds().transpose() {
                        return Some(result);
                    }
                    // Move to done
                    self.phase = Phase::Done;
                }

                Phase::Done => return None,
            }
        }
    }
}

impl<'a> ShadowTxGenerator<'a> {
    #[tracing::instrument(level = "trace", skip_all, err)]
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        block_height: &'a u64,
        reward_address: &'a IrysAddress,
        reward_amount: &'a U256,
        parent_block: &'a IrysBlockHeader,
        solution_hash: &'a H256,
        config: &'a ConsensusConfig,
        commitment_txs: &'a [CommitmentTransaction],
        submit_txs: &'a [DataTransactionHeader],
        publish_ledger: &'a PublishLedgerWithTxs,
        initial_treasury_balance: U256,
        pd_base_fee_per_chunk: Option<Amount<(CostPerChunk, Irys)>>,
        irys_usd_price: IrysTokenPrice,
        block_timestamp: UnixTimestamp,
        ledger_expiry_balance_delta: &'a LedgerExpiryBalanceDelta,
        refund_events: &[UnpledgeRefundEvent],
        unstake_refund_events: &[UnstakeRefundEvent],
        epoch_snapshot: &EpochSnapshot,
    ) -> Result<Self> {
        // Validate pd_base_fee_per_chunk: if Sprite is active, it must be provided
        let is_sprite_active = config.hardforks.is_sprite_active(block_timestamp);
        if is_sprite_active && pd_base_fee_per_chunk.is_none() {
            return Err(eyre!(
                "pd_base_fee_per_chunk must be provided when Sprite hardfork is active"
            ));
        }

        // Determine if this is the first Sprite block that needs to emit TreasuryDeposit
        // This initializes the EVM TREASURY_ACCOUNT with the pre-Sprite treasury balance.
        // Two cases:
        // 1. Block 1 (parent is genesis at height 0): if Sprite is active, this is the first
        //    "real" block that can emit TreasuryDeposit to seed the EVM treasury.
        // 2. Subsequent blocks: emit only on the transition from pre-Sprite to Sprite
        //    (i.e., parent was NOT Sprite-active AND current IS Sprite-active).
        let parent_sprite_active = config
            .hardforks
            .is_sprite_active(parent_block.timestamp_secs());
        let is_first_sprite_block = if parent_block.height == 0 {
            // Block 1: emit TreasuryDeposit if Sprite is active (seeds EVM treasury from genesis)
            is_sprite_active
        } else {
            // Subsequent blocks: emit only on the transition
            !parent_sprite_active && is_sprite_active
        };

        // Validate that no transaction in publish ledger has a refund
        // (promoted transactions should not get perm_fee refunds)
        for tx in &publish_ledger.txs {
            for (refund_tx_id, _, _) in &ledger_expiry_balance_delta.user_perm_fee_refunds {
                if tx.id == *refund_tx_id {
                    return Err(eyre!(
                        "Transaction {} is in publish ledger but also has a perm_fee refund scheduled. \
                        Promoted transactions should not receive refunds.",
                        tx.id
                    ));
                }
            }
        }

        tracing::debug!(
            "ShadowTxGenerator initialized with {} miner fee increments and {} user refund addresses",
            ledger_expiry_balance_delta.reward_balance_increment.len(),
            ledger_expiry_balance_delta.user_perm_fee_refunds.len()
        );

        // Create a temporary generator to initialize the iterators
        let generator = Self {
            block_height,
            reward_address,
            reward_amount,
            parent_block,
            solution_hash,
            config,
            commitment_txs,
            submit_txs,
            pd_base_fee_per_chunk,
            irys_usd_price,
            block_timestamp,
            is_sprite_active,
            is_first_sprite_block,
            initial_treasury_balance,
            treasury_balance: initial_treasury_balance,
            phase: Phase::Header,
            index: 0,
            current_publish_iter: Vec::new().into_iter(),
            current_expired_ledger_iter: Vec::new().into_iter(),
            current_commitment_refunds_iter: Vec::new().into_iter(),
        };

        // Initialize expired ledger iterator with all fee rewards and refunds
        let expired_ledger_txs = if !ledger_expiry_balance_delta
            .reward_balance_increment
            .is_empty()
            || !ledger_expiry_balance_delta.user_perm_fee_refunds.is_empty()
        {
            generator.create_expired_ledger_shadow_txs(ledger_expiry_balance_delta)?
        } else {
            Vec::new()
        };
        let current_expired_ledger_iter = expired_ledger_txs
            .into_iter()
            .map(Ok)
            .collect::<Vec<_>>()
            .into_iter();

        // Initialize publish ledger iterator with aggregated ingress proof rewards
        // Use parent block's timestamp for hardfork params (convert millis to seconds)
        let parent_block_timestamp_secs = parent_block.timestamp_secs();
        let aggregated_rewards = Self::accumulate_ingress_rewards_for_init(
            publish_ledger,
            config,
            parent_block_timestamp_secs,
            epoch_snapshot,
        )?;
        let publish_ledger_txs = if !aggregated_rewards.is_empty() {
            generator.create_publish_shadow_txs(aggregated_rewards)?
        } else {
            Vec::new()
        };
        let current_publish_iter = publish_ledger_txs
            .into_iter()
            .map(Ok)
            .collect::<Vec<_>>()
            .into_iter();

        // Initialize commitment refunds iterator (epoch only -> may be empty)
        let commitment_refund_txs =
            generator.create_commitment_refund_shadow_txs(refund_events, unstake_refund_events)?;
        let current_commitment_refunds_iter = commitment_refund_txs
            .into_iter()
            .map(Ok)
            .collect::<Vec<_>>()
            .into_iter();

        Ok(Self {
            block_height,
            reward_address,
            reward_amount,
            parent_block,
            solution_hash,
            config,
            commitment_txs,
            submit_txs,
            pd_base_fee_per_chunk,
            irys_usd_price,
            block_timestamp,
            is_sprite_active,
            is_first_sprite_block,
            initial_treasury_balance,
            treasury_balance: initial_treasury_balance,
            phase: Phase::Header,
            index: 0,
            current_publish_iter,
            current_expired_ledger_iter,
            current_commitment_refunds_iter,
        })
    }

    // Static helper methods for initialization
    #[tracing::instrument(level = "trace", skip_all, err)]
    fn create_expired_ledger_shadow_txs(
        &self,
        balance_delta: &LedgerExpiryBalanceDelta,
    ) -> Result<Vec<ShadowMetadata>> {
        let mut shadow_txs = Vec::new();

        // First process miner rewards for storing the expired data
        for (address, (amount, rolling_hash)) in balance_delta.reward_balance_increment.iter() {
            // Convert the rolling hash to FixedBytes<32> for irys_ref
            let hash_bytes = rolling_hash.to_bytes();
            let h256 = irys_types::H256::from(hash_bytes);
            let irys_ref = h256.into();

            shadow_txs.push(ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::TermFeeReward(BalanceIncrement {
                        amount: Uint::from_le_bytes(amount.to_le_bytes()),
                        target: (*address).into(),
                        irys_ref,
                    }),
                    (*self.solution_hash).into(),
                ),
                transaction_fee: 0, // No block producer reward for term fee rewards
            });
        }

        // Then process user refunds for non-promoted transactions (already sorted by tx_id)
        tracing::debug!(
            "Processing {} user perm fee refunds",
            balance_delta.user_perm_fee_refunds.len()
        );
        for (tx_id, refund_amount, user_address) in balance_delta.user_perm_fee_refunds.iter() {
            shadow_txs.push(ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::PermFeeRefund(BalanceIncrement {
                        amount: Uint::from_le_bytes(refund_amount.to_le_bytes()),
                        target: (*user_address).into(),
                        irys_ref: (*tx_id).into(),
                    }),
                    (*self.solution_hash).into(),
                ),
                transaction_fee: 0, // No block producer reward for refunds
            });
        }

        Ok(shadow_txs)
    }

    fn accumulate_ingress_rewards_for_init(
        publish_ledger: &PublishLedgerWithTxs,
        config: &ConsensusConfig,
        timestamp_secs: UnixTimestamp,
        epoch_snapshot: &EpochSnapshot,
    ) -> Result<BTreeMap<IrysAddress, (RewardAmount, RollingHash)>> {
        let mut rewards_map: BTreeMap<IrysAddress, (RewardAmount, RollingHash)> = BTreeMap::new();

        // Get ingress proofs if available
        let proofs = publish_ledger
            .proofs
            .as_ref()
            .map(|p| &p.0[..])
            .unwrap_or(&[]);

        // Skip processing if no proofs (nothing to reward)
        if proofs.is_empty() {
            return Ok(BTreeMap::new());
        }

        // Get ingress proof params for this timestamp
        let number_of_ingress_proofs_total = config
            .hardforks
            .number_of_ingress_proofs_total_at(timestamp_secs);

        // Process all transactions (MUST BE SORTED)
        for (index, tx) in publish_ledger.txs.iter().enumerate() {
            // CRITICAL: All publish ledger txs MUST have perm_fee
            let perm_fee = tx
                .perm_fee
                .ok_or_else(|| eyre::eyre!("publish ledger tx missing perm_fee {}", tx.id))?;

            // Calculate fee distribution using PublishFeeCharges
            let publish_charges = PublishFeeCharges::new(
                perm_fee,
                tx.term_fee,
                config,
                number_of_ingress_proofs_total,
            )?;

            // Get all the ingress proofs for the transaction
            let start_index = index * number_of_ingress_proofs_total as usize;
            let end_index = start_index + number_of_ingress_proofs_total as usize;
            let ingress_proofs = &proofs[start_index..end_index];

            // Get fee charges for all ingress proofs
            let fee_charges = publish_charges.ingress_proof_rewards(ingress_proofs)?;

            // Aggregate rewards by address and update rolling hash
            for charge in fee_charges {
                // Resolve to reward_address (falls back to miner address if no stake entry)
                let reward_addr = epoch_snapshot.resolve_reward_address(charge.address);

                let entry = rewards_map
                    .entry(reward_addr)
                    .or_insert((RewardAmount::zero(), RollingHash::zero()));
                entry.0.add_assign(charge.amount); // Add to total amount
                // XOR the rolling hash with the transaction ID
                entry.1.xor_assign(U256::from_be_bytes(tx.id.0));
            }
        }

        Ok(rewards_map)
    }

    /// Get the current treasury balance
    pub fn treasury_balance(&self) -> U256 {
        self.treasury_balance
    }

    /// Update treasury balance for expired ledger fee payments
    fn deduct_from_treasury_for_payout(&mut self, amount: U256) -> Result<()> {
        self.treasury_balance = self.treasury_balance.checked_sub(amount).ok_or_else(|| {
            eyre!(
                "Treasury balance underflow: cannot pay {} from balance {}",
                amount,
                self.treasury_balance
            )
        })?;
        Ok(())
    }

    /// Processed on block inclusion (different fees applied immediately to the user for having the tx included)
    fn process_commitment_transaction(&self, tx: &CommitmentTransaction) -> Result<ShadowMetadata> {
        match tx.commitment_type() {
            irys_types::CommitmentTypeV2::Stake => Ok(ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::Stake(BalanceDecrement {
                        amount: tx.value().into(),
                        target: tx.signer().into(),
                        irys_ref: tx.id().into(),
                    }),
                    (*self.solution_hash).into(),
                ),
                transaction_fee: tx.fee() as u128,
            }),
            irys_types::CommitmentTypeV2::Pledge { .. } => Ok(ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::Pledge(BalanceDecrement {
                        amount: tx.value().into(),
                        target: tx.signer().into(),
                        irys_ref: tx.id().into(),
                    }),
                    (*self.solution_hash).into(),
                ),
                transaction_fee: tx.fee() as u128,
            }),
            irys_types::CommitmentTypeV2::Unpledge { .. } => Ok(ShadowMetadata {
                // Inclusion-time behavior: fee-only via priority fee; no treasury movement here
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::Unpledge(irys_reth::shadow_tx::UnpledgeDebit {
                        target: tx.signer().into(),
                        irys_ref: tx.id().into(),
                    }),
                    (*self.solution_hash).into(),
                ),
                transaction_fee: tx.fee() as u128,
            }),
            irys_types::CommitmentTypeV2::Unstake => Ok(ShadowMetadata {
                // Inclusion-time behavior: fee-only via priority fee; no treasury movement here
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::UnstakeDebit(UnstakeDebit {
                        target: tx.signer().into(),
                        irys_ref: tx.id().into(),
                    }),
                    (*self.solution_hash).into(),
                ),
                transaction_fee: tx.fee() as u128,
            }),
            irys_types::CommitmentTypeV2::UpdateRewardAddress { new_reward_address } => {
                Ok(ShadowMetadata {
                    // Inclusion-time behavior: fee-only via priority fee; no treasury movement here
                    shadow_tx: ShadowTransaction::new_v1(
                        TransactionPacket::UpdateRewardAddress(
                            irys_reth::shadow_tx::UpdateRewardAddressDebit {
                                target: tx.signer().into(),
                                irys_ref: tx.id().into(),
                                new_reward_address: new_reward_address.into(),
                            },
                        ),
                        (*self.solution_hash).into(),
                    ),
                    transaction_fee: tx.fee() as u128,
                })
            }
        }
    }

    /// Creates shadow transactions from aggregated rewards
    fn create_publish_shadow_txs(
        &self,
        rewards_map: BTreeMap<IrysAddress, (RewardAmount, RollingHash)>,
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
                    shadow_tx: ShadowTransaction::new_v1(
                        TransactionPacket::IngressProofReward(BalanceIncrement {
                            amount: Uint::from_le_bytes(total_amount.to_le_bytes()),
                            target: address.into(),
                            irys_ref,
                        }),
                        (*self.solution_hash).into(),
                    ),
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
        // Calculate the amount to be deducted and sent to treasury
        // This includes:
        // - term_fee_treasury (95% of term_fee)
        // - perm_fee (if present, for permanent storage)
        // The block producer reward (5% of term_fee) is paid separately via transaction_fee
        let treasury_amount = term_charges
            .term_fee_treasury
            .saturating_add(tx.perm_fee.unwrap_or(BoundedFee::zero()).get());

        Ok(ShadowMetadata {
            shadow_tx: ShadowTransaction::new_v1(
                TransactionPacket::StorageFees(BalanceDecrement {
                    amount: Uint::from_le_bytes(treasury_amount.to_le_bytes()),
                    target: tx.signer.into(),
                    irys_ref: tx.id.into(),
                }),
                (*self.solution_hash).into(),
            ),
            // Block producer gets their reward (5% of term_fee) via transaction_fee
            // This becomes a priority fee in the EVM layer
            transaction_fee: term_charges
                .block_producer_reward
                .try_into()
                .map_err(|_| eyre!("Block producer reward exceeds u128 max"))?,
        })
    }

    /// Process a submit ledger transaction at a specific index
    #[tracing::instrument(skip_all, err)]
    fn try_process_submit_at_index(&mut self, index: usize) -> Result<ShadowMetadata> {
        let tx = &self.submit_txs[index];

        // Construct term fee charges
        let term_charges = TermFeeCharges::new(tx.term_fee, self.config)?;

        // Construct perm fee charges if applicable
        // Use parent block's timestamp for hardfork params (convert millis to seconds)
        let parent_block_timestamp_secs = self.parent_block.timestamp_secs();
        let number_of_ingress_proofs_total = self
            .config
            .hardforks
            .number_of_ingress_proofs_total_at(parent_block_timestamp_secs);
        let perm_charges = tx
            .perm_fee
            .map(|perm_fee| {
                PublishFeeCharges::new(
                    perm_fee,
                    tx.term_fee,
                    self.config,
                    number_of_ingress_proofs_total,
                )
            })
            .transpose()?;

        // Create shadow transaction
        let shadow_metadata = self.create_submit_shadow_tx(tx, &term_charges)?;

        // Update treasury with checked arithmetic (only pre-Sprite)
        // Post-Sprite: EVM handles treasury accounting via handle_balance_decrement
        if !self.is_sprite_active {
            self.treasury_balance = self
                .treasury_balance
                .checked_add(term_charges.term_fee_treasury)
                .ok_or_else(|| eyre!("Treasury balance overflow when adding term fee treasury"))?;

            if let Some(ref charges) = perm_charges {
                self.treasury_balance = self
                    .treasury_balance
                    .checked_add(charges.perm_fee_treasury)
                    .ok_or_else(|| {
                        eyre!("Treasury balance overflow when adding perm fee treasury")
                    })?;
            }
        }

        Ok(shadow_metadata)
    }

    /// Process a commitment transaction at a specific index
    #[tracing::instrument(skip_all, err)]
    fn try_process_commitment_at_index(&mut self, index: usize) -> Result<ShadowMetadata> {
        let tx = &self.commitment_txs[index];

        // Process commitment transaction
        let shadow_metadata = self.process_commitment_transaction(tx)?;

        // Update treasury based on commitment type (only pre-Sprite)
        // Post-Sprite: EVM handles treasury accounting via handle_balance_decrement
        if !self.is_sprite_active {
            match tx.commitment_type() {
                irys_types::CommitmentTypeV2::Stake
                | irys_types::CommitmentTypeV2::Pledge { .. } => {
                    // Stake and Pledge lock funds in the treasury
                    self.treasury_balance = self
                        .treasury_balance
                        .checked_add(tx.value())
                        .ok_or_else(|| {
                            eyre!("Treasury balance overflow when adding commitment value")
                        })?;
                }
                irys_types::CommitmentTypeV2::Unstake => {
                    // Unstake handled on epoch boundary
                }
                irys_types::CommitmentTypeV2::Unpledge { .. } => {
                    // Unpledge handled on epoch boundary
                }
                irys_types::CommitmentTypeV2::UpdateRewardAddress { .. } => {
                    // No treasury movement - fee only
                }
            }
        }

        Ok(shadow_metadata)
    }

    /// Process expired ledger fees - handles treasury updates and validation
    #[tracing::instrument(skip_all, err)]
    fn try_process_expired_ledger(&mut self) -> Result<Option<ShadowMetadata>> {
        self.current_expired_ledger_iter
            .next()
            .map(|result| {
                // Propagate any errors from the iterator
                let metadata = result?;

                // Validate this is the correct shadow tx type and update treasury (only pre-Sprite)
                // Post-Sprite: EVM handles treasury accounting via handle_balance_increment
                match &metadata.shadow_tx {
                    ShadowTransaction::V1 {
                        packet: TransactionPacket::TermFeeReward(increment),
                        ..
                    } => {
                        // Deduct miner reward from treasury (only pre-Sprite)
                        if !self.is_sprite_active {
                            self.deduct_from_treasury_for_payout(U256::from(increment.amount))?;
                        }
                    }
                    ShadowTransaction::V1 {
                        packet: TransactionPacket::PermFeeRefund(increment),
                        ..
                    } => {
                        // Deduct user refund from treasury (only pre-Sprite)
                        if !self.is_sprite_active {
                            self.deduct_from_treasury_for_payout(U256::from(increment.amount))?;
                        }
                    }
                    _ => {
                        return Err(eyre!(
                            "Unexpected shadow transaction type in expired ledger phase: {:?}",
                            metadata.shadow_tx
                        ));
                    }
                }

                Ok(metadata)
            })
            .transpose()
    }

    /// Process publish ledger - handles treasury updates and validation
    #[tracing::instrument(skip_all, err)]
    fn try_process_publish_ledger(&mut self) -> Result<Option<ShadowMetadata>> {
        self.current_publish_iter
            .next()
            .map(|result| {
                // Propagate any errors from the iterator
                let metadata = result?;

                // Validate this is the correct shadow tx type and update treasury (only pre-Sprite)
                // Post-Sprite: EVM handles treasury accounting via handle_balance_increment
                match &metadata.shadow_tx {
                    ShadowTransaction::V1 {
                        packet: TransactionPacket::IngressProofReward(increment),
                        ..
                    } => {
                        // Deduct ingress proof reward from treasury (only pre-Sprite)
                        if !self.is_sprite_active {
                            self.treasury_balance = self
                                .treasury_balance
                                .checked_sub(U256::from(increment.amount))
                                .ok_or_else(|| {
                                    eyre!(
                                        "Treasury balance underflow when paying ingress proof reward"
                                    )
                                })?;
                        }
                    }
                    _ => {
                        return Err(eyre!(
                            "Unexpected shadow transaction type in publish ledger phase: {:?}",
                            metadata.shadow_tx
                        ));
                    }
                }

                Ok(metadata)
            })
            .transpose()
    }

    /// Process commitment refunds (epoch-only) - handles treasury updates and validation
    #[tracing::instrument(skip_all, err)]
    fn try_process_commitment_refunds(&mut self) -> Result<Option<ShadowMetadata>> {
        self.current_commitment_refunds_iter
            .next()
            .map(|result| {
                let metadata = result?;
                // Update treasury (only pre-Sprite)
                // Post-Sprite: EVM handles treasury accounting via handle_balance_increment
                if !self.is_sprite_active {
                    match &metadata.shadow_tx {
                        ShadowTransaction::V1 { packet, .. } => match packet {
                            TransactionPacket::UnpledgeRefund(increment) => {
                                self.deduct_from_treasury_for_payout(U256::from(increment.amount))?;
                            }
                            TransactionPacket::UnstakeRefund(increment) => {
                                self.deduct_from_treasury_for_payout(U256::from(increment.amount))?;
                            }
                            _ => {
                                unreachable!(
                                    "commitment refund iterator contains only refund packets"
                                )
                            }
                        },
                        _ => {
                            unreachable!("commitment refund iterator contains only refund packets")
                        }
                    }
                }
                Ok(metadata)
            })
            .transpose()
    }

    fn create_commitment_refund_shadow_txs(
        &self,
        unpledge_events: &[UnpledgeRefundEvent],
        unstake_events: &[UnstakeRefundEvent],
    ) -> Result<Vec<ShadowMetadata>> {
        let mut out = Vec::new();
        // Unpledge refunds first (lower priority number in Ord)
        for event in unpledge_events.iter().copied() {
            out.push(ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::UnpledgeRefund(BalanceIncrement {
                        amount: event.amount.into(),
                        target: event.account.into(),
                        irys_ref: event.irys_ref_txid.into(),
                    }),
                    (*self.solution_hash).into(),
                ),
                transaction_fee: 0, // zero-priority fee for refunds
            });
        }
        // Unstake refunds after
        for event in unstake_events.iter().copied() {
            out.push(ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::UnstakeRefund(BalanceIncrement {
                        amount: event.amount.into(),
                        target: event.account.into(),
                        irys_ref: event.irys_ref_txid.into(),
                    }),
                    (*self.solution_hash).into(),
                ),
                transaction_fee: 0,
            });
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    Header,
    TreasuryDeposit, // Only emits on first Sprite block
    PdBaseFee,
    IrysUsdPrice,
    Commitments,
    SubmitLedger,
    ExpiredLedgerFees,
    PublishLedger,
    CommitmentRefunds,
    Done,
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
pub struct RollingHash(pub U256);

impl RollingHash {
    fn zero() -> Self {
        Self(U256::zero())
    }

    pub fn xor_assign(&mut self, value: U256) {
        self.0 ^= value;
    }

    pub fn to_bytes(self) -> [u8; 32] {
        self.0.to_be_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use irys_types::ingress::IngressProofV1;
    use irys_types::{BlockHash, CommitmentStatus, CommitmentTransactionV2, CommitmentTypeV1};
    use irys_types::{
        ConsensusConfig, H256, IrysBlockHeader, IrysSignature, Signature, ingress::IngressProof,
        irys::IrysSigner,
    };
    use itertools::Itertools as _;

    fn test_epoch_snapshot() -> EpochSnapshot {
        EpochSnapshot::default()
    }

    /// Creates an epoch snapshot with stake entries where reward_address can differ from signer.
    /// miners_with_rewards is a slice of (signer, reward_address) tuples.
    fn test_epoch_snapshot_with_reward_addresses(
        miners_with_rewards: &[(IrysAddress, IrysAddress)],
    ) -> EpochSnapshot {
        use irys_domain::StakeEntry;
        let mut snapshot = EpochSnapshot::default();
        for (signer, reward_address) in miners_with_rewards {
            snapshot.commitment_state.stake_commitments.insert(
                *signer,
                StakeEntry {
                    id: H256::random(),
                    commitment_status: CommitmentStatus::Active,
                    signer: *signer,
                    amount: U256::from(1000),
                    reward_address: *reward_address,
                },
            );
        }
        snapshot
    }

    fn create_test_commitment(
        commitment_type: CommitmentTypeV1,
        value: U256,
        fee: u64,
    ) -> CommitmentTransaction {
        let config = ConsensusConfig::testing();
        let signer = IrysSigner::random_signer(&config);
        CommitmentTransaction::V2(irys_types::CommitmentV2WithMetadata {
            tx: CommitmentTransactionV2 {
                id: H256::from([7_u8; 32]),
                commitment_type: commitment_type.into(),
                anchor: H256::from([8_u8; 32]),
                signer: signer.address(),
                value,
                fee,
                signature: IrysSignature::new(Signature::try_from([0_u8; 65].as_slice()).unwrap()),
                chain_id: config.chain_id,
            },
            metadata: Default::default(),
        })
    }

    fn create_data_tx_header(
        signer: &IrysSigner,
        term_fee: U256,
        perm_fee: Option<U256>,
    ) -> DataTransactionHeader {
        let data = vec![0_u8; 1024];
        let anchor = H256::from([9_u8; 32]);

        // Always create with perm_fee for publish ledger (ledger_id = 0)
        // The tests simulate the actual usage where submit txs have been promoted to publish
        let actual_perm_fee = perm_fee.unwrap_or_else(|| {
            // If no perm_fee specified, calculate minimum required for ingress proofs
            let config = ConsensusConfig::testing();
            let number_of_ingress_proofs_total = config
                .hardforks
                .number_of_ingress_proofs_total_at(UnixTimestamp::from_secs(0));
            let ingress_reward_per_proof = (term_fee
                * config.immediate_tx_inclusion_reward_percent.amount)
                / U256::from(10000);
            let total_ingress_reward =
                ingress_reward_per_proof * U256::from(number_of_ingress_proofs_total);
            U256::from(1000000) + total_ingress_reward
        });

        let tx = signer
            .create_publish_transaction(data, anchor, actual_perm_fee.into(), term_fee.into())
            .expect("Failed to create publish transaction");

        // Modify the header to reflect the original perm_fee intent
        let mut header = tx.header;
        header.perm_fee = perm_fee.map(Into::into);
        header
    }

    fn create_test_ingress_proof(
        signer: &IrysSigner,
        data_root: H256,
        anchor: BlockHash,
    ) -> IngressProof {
        let mut proof = IngressProof::V1(IngressProofV1 {
            signature: Default::default(),
            data_root,
            proof: H256::from([12_u8; 32]),
            chain_id: 1_u64,
            anchor,
        });

        signer
            .sign_ingress_proof(&mut proof)
            .expect("Test Ingress proof generation failed");
        proof
    }

    #[test]
    fn test_header_only() {
        let config = ConsensusConfig::testing();
        let parent_block = IrysBlockHeader::new_mock_header();
        let block_height = 101;
        let reward_address = IrysAddress::from([20_u8; 20]);
        let reward_amount = U256::from(5000);
        let initial_treasury = U256::from(2000000);
        let publish_ledger = PublishLedgerWithTxs {
            txs: vec![],
            proofs: None,
        };

        let solution_hash = H256::zero();

        // Create expected shadow transactions
        let expected_shadow_txs: Vec<ShadowMetadata> = vec![
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::BlockReward(BlockRewardIncrement {
                        amount: reward_amount.into(),
                    }),
                    solution_hash.into(),
                ),
                transaction_fee: 0,
            },
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                        per_chunk: U256::from(1000000_u64).into(),
                    }),
                    solution_hash.into(),
                ),
                transaction_fee: 0,
            },
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                        price: Uint::from(1000000000000000000_u128),
                    }),
                    solution_hash.into(),
                ),
                transaction_fee: 0,
            },
        ];

        let empty_fees = LedgerExpiryBalanceDelta {
            reward_balance_increment: BTreeMap::new(),
            user_perm_fee_refunds: Vec::new(),
        };
        let solution_hash = H256::zero();
        let epoch_snapshot = test_epoch_snapshot();
        let generator = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &solution_hash,
            &config,
            &[],
            &[],
            &publish_ledger,
            initial_treasury,
            Some(Amount::new(U256::from(1000000_u64))),
            IrysTokenPrice::new(U256::from(1000000000000000000_u128)),
            UnixTimestamp::from_secs(0), // Sprite active from genesis in testing config
            &empty_fees,
            &[],
            &[],
            &epoch_snapshot,
        )
        .expect("Should create generator");

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
        let reward_address = IrysAddress::from([20_u8; 20]);
        let reward_amount = U256::from(5000);
        let initial_treasury = U256::from(2000000);

        let stake_value = U256::from(100000);
        let stake_fee = 1000;
        let pledge_value = U256::from(100000);
        let pledge_fee = 500;
        let commitments = vec![
            create_test_commitment(CommitmentTypeV1::Stake, stake_value, stake_fee),
            create_test_commitment(
                CommitmentTypeV1::Pledge {
                    pledge_count_before_executing: 2,
                },
                pledge_value,
                pledge_fee,
            ),
            create_test_commitment(CommitmentTypeV1::Unstake, stake_value, stake_fee),
            create_test_commitment(
                CommitmentTypeV1::Unpledge {
                    pledge_count_before_executing: 1,
                    partition_hash: [0_u8; 32].into(),
                },
                pledge_value,
                pledge_fee,
            ),
        ];
        let stake_fee = stake_fee as u128;
        let pledge_fee = pledge_fee as u128;

        let publish_ledger = PublishLedgerWithTxs {
            txs: vec![],
            proofs: None,
        };

        // Create expected shadow transactions for all commitment types
        let expected_shadow_txs: Vec<ShadowMetadata> = vec![
            // Block reward
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::BlockReward(BlockRewardIncrement {
                        amount: reward_amount.into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // PD Base Fee Update
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                        per_chunk: U256::from(1000000_u64).into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // IRYS/USD Price Update
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                        price: Uint::from(1000000000000000000_u128),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // Stake
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::Stake(BalanceDecrement {
                        amount: stake_value.into(),
                        target: commitments[0].signer().into(),
                        irys_ref: commitments[0].id().into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: stake_fee,
            },
            // Pledge
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::Pledge(BalanceDecrement {
                        amount: pledge_value.into(),
                        target: commitments[1].signer().into(),
                        irys_ref: commitments[1].id().into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: pledge_fee,
            },
            // Unstake inclusion: fee-only via priority fee
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::UnstakeDebit(UnstakeDebit {
                        target: commitments[2].signer().into(),
                        irys_ref: commitments[2].id().into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: stake_fee,
            },
            // Unpledge: fee-only via priority fee
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::Unpledge(irys_reth::shadow_tx::UnpledgeDebit {
                        target: commitments[3].signer().into(),
                        irys_ref: commitments[3].id().into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: pledge_fee,
            },
        ];

        let empty_fees = LedgerExpiryBalanceDelta {
            reward_balance_increment: BTreeMap::new(),
            user_perm_fee_refunds: Vec::new(),
        };
        let solution_hash = H256::zero();
        let epoch_snapshot = test_epoch_snapshot();
        let generator = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &solution_hash,
            &config,
            &commitments,
            &[],
            &publish_ledger,
            initial_treasury,
            Some(Amount::new(U256::from(1000000_u64))),
            IrysTokenPrice::new(U256::from(1000000000000000000_u128)),
            UnixTimestamp::from_secs(0), // Sprite active from genesis in testing config
            &empty_fees,
            &[],
            &[],
            &epoch_snapshot,
        )
        .expect("Should create generator");

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
        let reward_address = IrysAddress::from([20_u8; 20]);
        let reward_amount = U256::from(5000);
        let initial_treasury = U256::from(2000000);
        let publish_ledger = PublishLedgerWithTxs {
            txs: vec![],
            proofs: None,
        };

        // Calculate expected values
        let term_charges = TermFeeCharges::new(term_fee.into(), &config).unwrap();

        // Create expected shadow transactions directly
        let expected_shadow_txs: Vec<ShadowMetadata> = vec![
            // Block reward
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::BlockReward(BlockRewardIncrement {
                        amount: reward_amount.into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // PD Base Fee Update
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                        per_chunk: U256::from(1000000_u64).into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // IRYS/USD Price Update
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                        price: Uint::from(1000000000000000000_u128),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // Storage fee for the submit transaction (treasury amount only)
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::StorageFees(BalanceDecrement {
                        amount: term_charges.term_fee_treasury.into(),
                        target: submit_tx.signer.into(),
                        irys_ref: submit_tx.id.into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: term_charges
                    .block_producer_reward
                    .try_into()
                    .expect("Block producer reward should fit in u128"),
            },
        ];

        let empty_fees = LedgerExpiryBalanceDelta {
            reward_balance_increment: BTreeMap::new(),
            user_perm_fee_refunds: Vec::new(),
        };
        let solution_hash = H256::zero();
        let epoch_snapshot = test_epoch_snapshot();
        let mut generator = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &solution_hash,
            &config,
            &[],
            &submit_txs,
            &publish_ledger,
            initial_treasury,
            Some(Amount::new(U256::from(1000000_u64))),
            IrysTokenPrice::new(U256::from(1000000000000000000_u128)),
            UnixTimestamp::from_secs(0), // Sprite active from genesis in testing config
            &empty_fees,
            &[],
            &[],
            &epoch_snapshot,
        )
        .expect("Should create generator");

        // Compare actual with expected
        generator
            .by_ref()
            .zip_eq(expected_shadow_txs)
            .for_each(|(actual, expected)| {
                let actual = actual.expect("Should be Ok");
                assert_eq!(actual, expected);
            });

        // Verify treasury increased by the expected amount (only pre-Sprite)
        // Post-Sprite: treasury is read from EVM state, not tracked in generator
        let block_timestamp = UnixTimestamp::from_secs(0);
        if !config.hardforks.is_sprite_active(block_timestamp) {
            let expected_treasury = initial_treasury + term_charges.term_fee_treasury;
            assert_eq!(generator.treasury_balance(), expected_treasury);
        }
    }

    #[test]
    fn test_btreemap_maintains_sorted_order() {
        // Quick test to verify BTreeMap maintains sorted order
        let mut rewards_map: BTreeMap<IrysAddress, u32> = BTreeMap::new();

        // Insert addresses in random order
        let addr1 = IrysAddress::from([5_u8; 20]);
        let addr2 = IrysAddress::from([1_u8; 20]);
        let addr3 = IrysAddress::from([9_u8; 20]);

        rewards_map.insert(addr3, 3);
        rewards_map.insert(addr1, 1);
        rewards_map.insert(addr2, 2);

        // Verify they come out sorted
        let addresses: Vec<IrysAddress> = rewards_map.keys().copied().collect();
        assert_eq!(addresses[0], addr2); // Smallest address first
        assert_eq!(addresses[1], addr1);
        assert_eq!(addresses[2], addr3); // Largest address last
    }

    #[test]
    fn test_publish_tx_with_reward_address_redirection() {
        let mut config = ConsensusConfig::testing();
        // Use custom hardfork params with 4 proofs for this test
        config.hardforks.frontier.number_of_ingress_proofs_total = 4;
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
            create_test_ingress_proof(
                &proof_signer1,
                H256::from([10_u8; 32]),
                H256::from([14_u8; 32]),
            ),
            create_test_ingress_proof(
                &proof_signer2,
                H256::from([11_u8; 32]),
                H256::from([15_u8; 32]),
            ),
            create_test_ingress_proof(
                &proof_signer3,
                H256::from([12_u8; 32]),
                H256::from([16_u8; 32]),
            ),
            create_test_ingress_proof(
                &proof_signer2,
                H256::from([13_u8; 32]),
                H256::from([17_u8; 32]),
            ), // Extra proof for signer2
        ];

        let block_height = 101;
        let reward_address = IrysAddress::from([20_u8; 20]);
        let reward_amount = U256::from(5000);
        let initial_treasury = U256::from(20000000);
        let publish_ledger = PublishLedgerWithTxs {
            txs: submit_txs.clone(),
            proofs: Some(IngressProofsList(proofs)),
        };

        // Calculate expected values
        let term_charges = TermFeeCharges::new(term_fee.into(), &config).unwrap();

        // Since perm_fee was calculated with 4 proofs in mind
        let number_of_ingress_proofs_total = 4;
        let publish_charges = PublishFeeCharges::new(
            perm_fee.into(),
            term_fee.into(),
            &config,
            number_of_ingress_proofs_total,
        )
        .unwrap();

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

        // Key difference: Create separate reward addresses for signers 1 and 2
        let reward_dest1 = IrysAddress::random(); // Different from signer1
        let reward_dest2 = IrysAddress::random(); // Different from signer2
        // signer3 keeps reward_address = signer (tests mixed scenario)

        // Create epoch snapshot with reward redirection
        let epoch_snapshot = test_epoch_snapshot_with_reward_addresses(&[
            (proof_signer1.address(), reward_dest1),
            (proof_signer2.address(), reward_dest2),
            (proof_signer3.address(), proof_signer3.address()), // No redirection
        ]);

        // Build expected shadow txs with redirected addresses:
        // - signer1's reward → reward_dest1
        // - signer2's reward → reward_dest2
        // - signer3's reward → proof_signer3.address()

        // Sort by reward destination address for deterministic ordering
        let mut signer_rewards = [
            (reward_dest1, signer1_reward),
            (reward_dest2, signer2_reward),
            (proof_signer3.address(), signer3_reward),
        ];
        signer_rewards.sort_by_key(|(addr, _)| *addr);

        // Create expected shadow transactions directly
        let expected_shadow_txs: Vec<ShadowMetadata> = vec![
            // Block reward
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::BlockReward(BlockRewardIncrement {
                        amount: reward_amount.into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // PD Base Fee Update
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                        per_chunk: U256::from(1000000_u64).into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // IRYS/USD Price Update
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                        price: Uint::from(1000000000000000000_u128),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // Storage fee for the publish transaction (treasury amount + perm_fee)
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::StorageFees(BalanceDecrement {
                        amount: (term_charges.term_fee_treasury + perm_fee).into(),
                        target: publish_tx.signer.into(),
                        irys_ref: publish_tx.id.into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: term_charges
                    .block_producer_reward
                    .try_into()
                    .expect("Block producer reward should fit in u128"),
            },
            // Ingress proof rewards (aggregated by reward_address, sorted by address)
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::IngressProofReward(BalanceIncrement {
                        amount: signer_rewards[0].1.into(),
                        target: signer_rewards[0].0.into(),
                        irys_ref: H256::from(publish_tx.id.0).into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::IngressProofReward(BalanceIncrement {
                        amount: signer_rewards[1].1.into(),
                        target: signer_rewards[1].0.into(),
                        irys_ref: H256::from(publish_tx.id.0).into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::IngressProofReward(BalanceIncrement {
                        amount: signer_rewards[2].1.into(),
                        target: signer_rewards[2].0.into(),
                        irys_ref: H256::from(publish_tx.id.0).into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
        ];

        let empty_fees = LedgerExpiryBalanceDelta {
            reward_balance_increment: BTreeMap::new(),
            user_perm_fee_refunds: Vec::new(),
        };
        let solution_hash = H256::zero();
        let generator = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &solution_hash,
            &config,
            &[],
            &submit_txs,
            &publish_ledger,
            initial_treasury,
            Some(Amount::new(U256::from(1000000_u64))),
            IrysTokenPrice::new(U256::from(1000000000000000000_u128)),
            UnixTimestamp::from_secs(0), // Sprite active from genesis in testing config
            &empty_fees,
            &[],
            &[],
            &epoch_snapshot,
        )
        .expect("Should create generator");

        // Compare actual with expected
        generator
            .zip_eq(expected_shadow_txs)
            .for_each(|(actual, expected)| {
                let actual = actual.expect("Should be Ok");
                assert_eq!(actual, expected);
            });
    }

    #[test]
    fn test_expired_ledger_miner_rewards() {
        let config = ConsensusConfig::testing();
        let parent_block = IrysBlockHeader::new_mock_header();
        let block_height = 101;
        let reward_address = IrysAddress::from([20_u8; 20]);
        let reward_amount = U256::from(5000);
        let initial_treasury = U256::from(10_000_000);

        // Create miners and their rewards
        let miner1 = IrysAddress::from([10_u8; 20]);
        let miner2 = IrysAddress::from([11_u8; 20]);
        let miner3 = IrysAddress::from([12_u8; 20]);

        let miner1_reward = U256::from(1000);
        let miner2_reward = U256::from(2000);
        let miner3_reward = U256::from(1500);
        let total_miner_rewards = miner1_reward + miner2_reward + miner3_reward;

        // Create rolling hashes for each miner (simulating aggregated tx IDs)
        let miner1_hash = RollingHash(U256::from_be_bytes([1_u8; 32]));
        let miner2_hash = RollingHash(U256::from_be_bytes([2_u8; 32]));
        let miner3_hash = RollingHash(U256::from_be_bytes([3_u8; 32]));

        let mut reward_balance_increment = BTreeMap::new();
        reward_balance_increment.insert(miner1, (miner1_reward, miner1_hash));
        reward_balance_increment.insert(miner2, (miner2_reward, miner2_hash));
        reward_balance_increment.insert(miner3, (miner3_reward, miner3_hash));

        let expired_fees = LedgerExpiryBalanceDelta {
            reward_balance_increment,
            user_perm_fee_refunds: Vec::new(),
        };

        let publish_ledger = PublishLedgerWithTxs {
            txs: vec![],
            proofs: None,
        };

        // Create expected shadow transactions (sorted by miner address)
        let mut expected_shadow_txs = vec![
            // Block reward
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::BlockReward(BlockRewardIncrement {
                        amount: reward_amount.into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // PD Base Fee Update
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                        per_chunk: U256::from(1000000_u64).into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // IRYS/USD Price Update
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                        price: Uint::from(1000000000000000000_u128),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
        ];

        // Add miner rewards in sorted order (BTreeMap guarantees this)
        for (miner, (reward, hash)) in expired_fees.reward_balance_increment.iter() {
            expected_shadow_txs.push(ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::TermFeeReward(BalanceIncrement {
                        amount: Uint::from_le_bytes(reward.to_le_bytes()),
                        target: (*miner).into(),
                        irys_ref: H256::from(hash.to_bytes()).into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            });
        }

        let solution_hash = H256::zero();
        let epoch_snapshot = test_epoch_snapshot();
        let mut generator = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &solution_hash,
            &config,
            &[],
            &[],
            &publish_ledger,
            initial_treasury,
            Some(Amount::new(U256::from(1000000_u64))),
            IrysTokenPrice::new(U256::from(1000000000000000000_u128)),
            UnixTimestamp::from_secs(0), // Sprite active from genesis in testing config
            &expired_fees,
            &[],
            &[],
            &epoch_snapshot,
        )
        .expect("Should create generator");

        // Compare actual with expected
        generator
            .by_ref()
            .zip_eq(expected_shadow_txs)
            .for_each(|(actual, expected)| {
                let actual = actual.expect("Should be Ok");
                assert_eq!(actual, expected);
            });

        // Treasury should decrease by total miner rewards (only pre-Sprite)
        // Post-Sprite: treasury is read from EVM state, not tracked in generator
        let block_timestamp = UnixTimestamp::from_secs(0);
        if !config.hardforks.is_sprite_active(block_timestamp) {
            let expected_treasury = initial_treasury - total_miner_rewards;
            assert_eq!(generator.treasury_balance(), expected_treasury);
        }
    }

    #[test]
    fn test_user_perm_fee_refunds() {
        let config = ConsensusConfig::testing();
        let parent_block = IrysBlockHeader::new_mock_header();
        let block_height = 101;
        let reward_address = IrysAddress::from([20_u8; 20]);
        let reward_amount = U256::from(5000);
        let initial_treasury = U256::from(10_000_000);

        // Create users and their refunds
        let user1 = IrysAddress::from([30_u8; 20]);
        let user2 = IrysAddress::from([31_u8; 20]);

        let tx1_id = H256::from([40_u8; 32]);
        let tx2_id = H256::from([41_u8; 32]);
        let tx3_id = H256::from([42_u8; 32]);

        let refund1 = U256::from(500);
        let refund2 = U256::from(700);
        let refund3 = U256::from(300);
        let total_refunds = refund1 + refund2 + refund3;

        // Create refunds sorted by tx_id
        let mut user_perm_fee_refunds = vec![
            (tx1_id, refund1, user1), // User1's first refund
            (tx2_id, refund2, user1), // User1's second refund
            (tx3_id, refund3, user2), // User2's refund
        ];
        user_perm_fee_refunds.sort_by_key(|(tx_id, _, _)| *tx_id);

        let expired_fees = LedgerExpiryBalanceDelta {
            reward_balance_increment: BTreeMap::new(),
            user_perm_fee_refunds,
        };

        let publish_ledger = PublishLedgerWithTxs {
            txs: vec![],
            proofs: None,
        };

        // Create expected shadow transactions
        let mut expected_shadow_txs = vec![
            // Block reward
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::BlockReward(BlockRewardIncrement {
                        amount: reward_amount.into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // PD Base Fee Update
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                        per_chunk: U256::from(1000000_u64).into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            // IRYS/USD Price Update
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                        price: Uint::from(1000000000000000000_u128),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
        ];

        // Add user refunds in sorted order (already sorted by tx_id)
        for (tx_id, refund_amount, user) in expired_fees.user_perm_fee_refunds.iter() {
            expected_shadow_txs.push(ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::PermFeeRefund(BalanceIncrement {
                        amount: Uint::from_le_bytes(refund_amount.to_le_bytes()),
                        target: (*user).into(),
                        irys_ref: (*tx_id).into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            });
        }

        let solution_hash = H256::zero();
        let epoch_snapshot = test_epoch_snapshot();
        let mut generator = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &solution_hash,
            &config,
            &[],
            &[],
            &publish_ledger,
            initial_treasury,
            Some(Amount::new(U256::from(1000000_u64))),
            IrysTokenPrice::new(U256::from(1000000000000000000_u128)),
            UnixTimestamp::from_secs(0), // Sprite active from genesis in testing config
            &expired_fees,
            &[],
            &[],
            &epoch_snapshot,
        )
        .expect("Should create generator");

        // Compare actual with expected
        generator
            .by_ref()
            .zip_eq(expected_shadow_txs)
            .for_each(|(actual, expected)| {
                let actual = actual.expect("Should be Ok");
                assert_eq!(actual, expected);
            });

        // Treasury should decrease by total refunds (only pre-Sprite)
        // Post-Sprite: treasury is read from EVM state, not tracked in generator
        let block_timestamp = UnixTimestamp::from_secs(0);
        if !config.hardforks.is_sprite_active(block_timestamp) {
            let expected_treasury = initial_treasury - total_refunds;
            assert_eq!(generator.treasury_balance(), expected_treasury);
        }
    }

    #[test]
    fn test_empty_expired_ledger_fees() {
        let config = ConsensusConfig::testing();
        let parent_block = IrysBlockHeader::new_mock_header();
        let block_height = 101;
        let reward_address = IrysAddress::from([20_u8; 20]);
        let reward_amount = U256::from(5000);
        let initial_treasury = U256::from(10_000_000);

        // Empty expired fees
        let expired_fees = LedgerExpiryBalanceDelta {
            reward_balance_increment: BTreeMap::new(),
            user_perm_fee_refunds: Vec::new(),
        };

        let publish_ledger = PublishLedgerWithTxs {
            txs: vec![],
            proofs: None,
        };

        // Only expect block reward, PD base fee update, and IRYS/USD price update
        let expected_shadow_txs = vec![
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::BlockReward(BlockRewardIncrement {
                        amount: reward_amount.into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::PdBaseFeeUpdate(PdBaseFeeUpdate {
                        per_chunk: U256::from(1000000_u64).into(),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
            ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(
                    TransactionPacket::IrysUsdPriceUpdate(IrysUsdPriceUpdate {
                        price: Uint::from(1000000000000000000_u128),
                    }),
                    H256::zero().into(),
                ),
                transaction_fee: 0,
            },
        ];

        let solution_hash = H256::zero();
        let epoch_snapshot = test_epoch_snapshot();
        let mut generator = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &solution_hash,
            &config,
            &[],
            &[],
            &publish_ledger,
            initial_treasury,
            Some(Amount::new(U256::from(1000000_u64))),
            IrysTokenPrice::new(U256::from(1000000000000000000_u128)),
            UnixTimestamp::from_secs(0), // Sprite active from genesis in testing config
            &expired_fees,
            &[],
            &[],
            &epoch_snapshot,
        )
        .expect("Should create generator");

        // Compare actual with expected
        generator
            .by_ref()
            .zip_eq(expected_shadow_txs)
            .for_each(|(actual, expected)| {
                let actual = actual.expect("Should be Ok");
                assert_eq!(actual, expected);
            });

        // Treasury should remain unchanged (no expired fees to pay)
        assert_eq!(generator.treasury_balance(), initial_treasury);
    }

    #[test]
    fn test_error_when_sprite_active_but_pd_base_fee_none() {
        let config = ConsensusConfig::testing(); // Sprite active from genesis
        let parent_block = IrysBlockHeader::new_mock_header();
        let block_height = 1;
        let reward_address = IrysAddress::from([1_u8; 20]);
        let reward_amount = U256::from(1000);
        let initial_treasury = U256::from(1000000);

        let publish_ledger = PublishLedgerWithTxs {
            txs: vec![],
            proofs: None,
        };
        let empty_fees = LedgerExpiryBalanceDelta {
            reward_balance_increment: BTreeMap::new(),
            user_perm_fee_refunds: Vec::new(),
        };
        let solution_hash = H256::zero();

        // Sprite is active (timestamp 0 with testing config), but pd_base_fee is None
        let result = ShadowTxGenerator::new(
            &block_height,
            &reward_address,
            &reward_amount,
            &parent_block,
            &solution_hash,
            &config,
            &[],
            &[],
            &publish_ledger,
            initial_treasury,
            None, // This should cause an error
            IrysTokenPrice::new(U256::from(1000000000000000000_u128)),
            UnixTimestamp::from_secs(0), // Sprite active from genesis in testing config
            &empty_fees,
            &[],
            &[],
            &test_epoch_snapshot(),
        );

        assert!(result.is_err());
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg
                .contains("pd_base_fee_per_chunk must be provided when Sprite hardfork is active"),
            "Unexpected error message: {}",
            err_msg
        );
    }
}
