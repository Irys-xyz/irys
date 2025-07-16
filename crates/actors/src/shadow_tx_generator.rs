use eyre::Result;
use irys_reth::shadow_tx::{
    BalanceDecrement, BalanceIncrement, BlockRewardIncrement, ShadowTransaction, TransactionPacket,
};
use irys_types::{
    Address, CommitmentTransaction, DataTransactionHeader, IrysBlockHeader,
    IrysTransactionCommon as _,
};
use reth::revm::primitives::ruint::Uint;

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
    ) -> impl std::iter::Iterator<Item = Result<ShadowTransaction>> + use<'a> {
        self.generate_shadow_tx_header()
            .chain(self.generate_commitment_shadow_transactions(commitment_txs))
            .chain(self.generate_data_storage_shadow_transactions(submit_txs))
    }

    /// Generates the expected header shadow transactions for a given block
    pub fn generate_shadow_tx_header(
        &self,
    ) -> impl std::iter::Iterator<Item = Result<ShadowTransaction>> {
        std::iter::once(Ok(ShadowTransaction::new_v1(
            TransactionPacket::BlockReward(BlockRewardIncrement {
                amount: (*self.reward_amount).into(),
                target: *self.reward_address,
            }),
        )))
    }

    /// Generates the expected data shadow transactions for a given block
    pub fn generate_data_storage_shadow_transactions(
        &'a self,
        submit_txs: &'a [DataTransactionHeader],
    ) -> impl std::iter::Iterator<Item = Result<ShadowTransaction>> + use<'a> {
        // create a storage fee shadow txs
        submit_txs.iter().map(move |tx| {
            Ok(ShadowTransaction::new_v1(TransactionPacket::StorageFees(
                BalanceDecrement {
                    amount: Uint::from(tx.total_fee()),
                    target: tx.signer,
                    irys_ref: tx.id.into(),
                },
            )))
        })
    }

    /// Generates the expected commitment transactions for a given block
    pub fn generate_commitment_shadow_transactions(
        &'a self,
        commitment_txs: &'a [CommitmentTransaction],
    ) -> impl std::iter::Iterator<Item = Result<ShadowTransaction>> + use<'a> {
        commitment_txs.iter().map(move |tx| {
            let commitment_value = Uint::from_le_bytes(tx.commitment_value().to_le_bytes());
            let total_fee = Uint::from(tx.total_fee());

            match tx.commitment_type {
                irys_primitives::CommitmentType::Stake => {
                    let amount = total_fee
                        .checked_add(commitment_value)
                        .ok_or_else(|| eyre::eyre!("Overflow when calculating stake amount"))?;
                    Ok(ShadowTransaction::new_v1(TransactionPacket::Stake(
                        BalanceDecrement {
                            amount,
                            target: tx.signer,
                            irys_ref: tx.id.into(),
                        },
                    )))
                }
                irys_primitives::CommitmentType::Pledge => {
                    let amount = total_fee
                        .checked_add(commitment_value)
                        .ok_or_else(|| eyre::eyre!("Overflow when calculating pledge amount"))?;
                    Ok(ShadowTransaction::new_v1(TransactionPacket::Pledge(
                        BalanceDecrement {
                            amount,
                            target: tx.signer,
                            irys_ref: tx.id.into(),
                        },
                    )))
                }
                irys_primitives::CommitmentType::Unpledge => {
                    let amount = commitment_value
                        .checked_sub(total_fee)
                        .ok_or_else(|| eyre::eyre!("Underflow when calculating unpledge amount"))?;
                    Ok(ShadowTransaction::new_v1(TransactionPacket::Unpledge(
                        BalanceIncrement {
                            amount,
                            target: tx.signer,
                            irys_ref: tx.id.into(),
                        },
                    )))
                }
                irys_primitives::CommitmentType::Unstake => {
                    let amount = commitment_value
                        .checked_sub(total_fee)
                        .ok_or_else(|| eyre::eyre!("Underflow when calculating unstake amount"))?;
                    Ok(ShadowTransaction::new_v1(TransactionPacket::Unstake(
                        BalanceIncrement {
                            amount,
                            target: tx.signer,
                            irys_ref: tx.id.into(),
                        },
                    )))
                }
            }
        })
    }
}
