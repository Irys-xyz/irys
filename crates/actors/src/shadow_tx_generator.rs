use eyre::Result;
use irys_reth::shadow_tx::{
    BalanceDecrement, BalanceIncrement, BlockRewardIncrement, EitherIncrementOrDecrement,
    ShadowTransaction, TransactionPacket,
};
use irys_types::{
    Address, CommitmentTransaction, DataTransactionHeader, IrysBlockHeader,
    IrysTransactionCommon as _,
};
use reth::revm::primitives::ruint::Uint;

#[derive(Debug)]
pub struct ShadowMetadata {
    pub shadow_tx: ShadowTransaction,
    pub transaction_fee: u128,
}

pub struct ShadowTxGenerator<'a> {
    pub block_height: &'a u64,
    pub reward_address: &'a Address,
    pub reward_amount: &'a irys_types::U256,
    pub parent_block: &'a IrysBlockHeader,
}

impl<'a> ShadowTxGenerator<'a> {
    pub fn new(
        block_height: &'a u64,
        reward_address: &'a Address,
        reward_amount: &'a irys_types::U256,
        parent_block: &'a IrysBlockHeader,
    ) -> Self {
        Self {
            block_height,
            reward_address,
            reward_amount,
            parent_block,
        }
    }

    pub fn generate_all(
        &'a self,
        commitment_txs: &'a [CommitmentTransaction],
        submit_txs: &'a [DataTransactionHeader],
    ) -> impl std::iter::Iterator<Item = Result<ShadowMetadata>> + use<'a> {
        self.generate_shadow_tx_header()
            .chain(self.generate_commitment_shadow_transactions(commitment_txs))
            .chain(self.generate_data_storage_shadow_transactions(submit_txs))
    }

    /// Generates the expected header shadow transactions for a given block
    pub fn generate_shadow_tx_header(
        &self,
    ) -> impl std::iter::Iterator<Item = Result<ShadowMetadata>> {
        std::iter::once(Ok(ShadowMetadata {
            shadow_tx: ShadowTransaction::new_v1(TransactionPacket::BlockReward(
                BlockRewardIncrement {
                    amount: (*self.reward_amount).into(),
                },
            )),
            transaction_fee: 0, // Block rewards have no fee
        }))
    }

    /// Generates the expected data shadow transactions for a given block
    pub fn generate_data_storage_shadow_transactions(
        &'a self,
        submit_txs: &'a [DataTransactionHeader],
    ) -> impl std::iter::Iterator<Item = Result<ShadowMetadata>> + use<'a> {
        // create a storage fee shadow txs
        submit_txs.iter().map(move |tx| {
            let total_cost = tx.total_cost();
            Ok(ShadowMetadata {
                shadow_tx: ShadowTransaction::new_v1(TransactionPacket::StorageFees(
                    BalanceDecrement {
                        amount: Uint::from_le_bytes(total_cost.to_le_bytes()),
                        target: tx.signer,
                        irys_ref: tx.id.into(),
                    },
                )),
                // todo: We need to update DataTransactionHeader - separate out the fee that
                // the miner receiver from the fee that gets burned from users account
                transaction_fee: 0,
            })
        })
    }

    /// Generates the expected commitment transactions for a given block
    pub fn generate_commitment_shadow_transactions(
        &'a self,
        commitment_txs: &'a [CommitmentTransaction],
    ) -> impl std::iter::Iterator<Item = Result<ShadowMetadata>> + use<'a> {
        commitment_txs.iter().map(move |tx| {
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
                    shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Stake(
                        BalanceDecrement {
                            amount: total_cost,
                            target: tx.signer,
                            irys_ref: tx.id.into(),
                        },
                    )),
                    transaction_fee,
                }),
                irys_primitives::CommitmentType::Pledge => Ok(ShadowMetadata {
                    shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Pledge(
                        BalanceDecrement {
                            amount: total_cost,
                            target: tx.signer,
                            irys_ref: tx.id.into(),
                        },
                    )),
                    transaction_fee,
                }),
                irys_primitives::CommitmentType::Unpledge => {
                    create_increment_or_decrement("unpledge").map(|result| ShadowMetadata {
                        shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Unpledge(result)),
                        transaction_fee,
                    })
                }
                irys_primitives::CommitmentType::Unstake => {
                    create_increment_or_decrement("unstake").map(|result| ShadowMetadata {
                        shadow_tx: ShadowTransaction::new_v1(TransactionPacket::Unstake(result)),
                        transaction_fee,
                    })
                }
            }
        })
    }
}
