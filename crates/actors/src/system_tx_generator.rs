use eyre::Result;
use irys_reth::system_tx::{
    BalanceDecrement, BalanceIncrement, SystemTransaction, TransactionPacket,
};
use irys_types::{
    Address, CommitmentTransaction, IrysBlockHeader, IrysTransactionCommon, IrysTransactionHeader,
    H256,
};
use reth::revm::primitives::ruint::Uint;

pub struct SystemTxGenerator<'a> {
    pub block_height: &'a u64,
    pub reward_address: &'a Address,
    pub reward_amount: &'a irys_types::U256,
    pub parent_block: &'a IrysBlockHeader,
}

impl<'a> SystemTxGenerator<'a> {
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
        submit_txs: &'a [IrysTransactionHeader],
    ) -> impl std::iter::Iterator<Item = Result<SystemTransaction>> + use<'a> {
        self.generate_system_tx_header()
            .chain(self.generate_commitment_system_transactions(commitment_txs))
            .chain(self.generate_data_storage_system_transactions(submit_txs))
    }

    /// Generates the expected header system transactions for a given block
    pub fn generate_system_tx_header(
        &self,
    ) -> impl std::iter::Iterator<Item = Result<SystemTransaction>> {
        std::iter::once(Ok(SystemTransaction::new_v1(
            *self.block_height,
            H256::from_slice(&*self.parent_block.evm_block_hash).into(),
            TransactionPacket::BlockReward(BalanceIncrement {
                amount: (*self.reward_amount).into(),
                target: *self.reward_address,
            }),
        )))
    }

    /// Generates the expected data system transactions for a given block
    pub fn generate_data_storage_system_transactions(
        &'a self,
        submit_txs: &'a [IrysTransactionHeader],
    ) -> impl std::iter::Iterator<Item = Result<SystemTransaction>> + use<'a> {
        // create a storage fee system txs
        submit_txs.iter().map(move |tx| {
            Ok(SystemTransaction::new_v1(
                *self.block_height,
                H256::from_slice(&*self.parent_block.evm_block_hash).into(),
                TransactionPacket::StorageFees(BalanceDecrement {
                    amount: Uint::from(tx.total_fee()),
                    target: tx.signer,
                }),
            ))
        })
    }

    /// Generates the expected commitment transactions for a given block
    pub fn generate_commitment_system_transactions(
        &'a self,
        commitment_txs: &'a [CommitmentTransaction],
    ) -> impl std::iter::Iterator<Item = Result<SystemTransaction>> + use<'a> {
        commitment_txs.iter().map(move |tx| {
            let commitment_value = Uint::from_le_bytes(tx.commitment_value().to_le_bytes());
            let total_fee = Uint::from(tx.total_fee());

            match tx.commitment_type {
                irys_primitives::CommitmentType::Stake => {
                    let amount = total_fee
                        .checked_add(commitment_value)
                        .ok_or_else(|| eyre::eyre!("Overflow when calculating stake amount"))?;
                    Ok(SystemTransaction::new_v1(
                        *self.block_height,
                        H256::from_slice(&*self.parent_block.evm_block_hash).into(),
                        TransactionPacket::Stake(BalanceDecrement {
                            amount,
                            target: tx.signer,
                        }),
                    ))
                }
                irys_primitives::CommitmentType::Pledge => {
                    let amount = total_fee
                        .checked_add(commitment_value)
                        .ok_or_else(|| eyre::eyre!("Overflow when calculating pledge amount"))?;
                    Ok(SystemTransaction::new_v1(
                        *self.block_height,
                        H256::from_slice(&*self.parent_block.evm_block_hash).into(),
                        TransactionPacket::Pledge(BalanceDecrement {
                            amount,
                            target: tx.signer,
                        }),
                    ))
                }
                irys_primitives::CommitmentType::Unpledge => {
                    let amount = commitment_value
                        .checked_sub(total_fee)
                        .ok_or_else(|| eyre::eyre!("Underflow when calculating unpledge amount"))?;
                    Ok(SystemTransaction::new_v1(
                        *self.block_height,
                        H256::from_slice(&*self.parent_block.evm_block_hash).into(),
                        TransactionPacket::Unpledge(BalanceIncrement {
                            amount,
                            target: tx.signer,
                        }),
                    ))
                }
                irys_primitives::CommitmentType::Unstake => {
                    let amount = commitment_value
                        .checked_sub(total_fee)
                        .ok_or_else(|| eyre::eyre!("Underflow when calculating unstake amount"))?;
                    Ok(SystemTransaction::new_v1(
                        *self.block_height,
                        H256::from_slice(&*self.parent_block.evm_block_hash).into(),
                        TransactionPacket::Unstake(BalanceIncrement {
                            amount,
                            target: tx.signer,
                        }),
                    ))
                }
            }
        })
    }
}
