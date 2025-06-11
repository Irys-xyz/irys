use irys_reth::system_tx::{
    BalanceDecrement, BalanceIncrement, SystemTransaction, TransactionPacket,
};
use irys_types::{
    Address, CommitmentTransaction, IrysBlockHeader, IrysTransactionCommon, IrysTransactionHeader,
    H256,
};
use reth::revm::primitives::ruint::Uint;

pub struct SystemTxGenerator {
    pub block_height: u64,
    pub reward_address: Address,
    pub reward_amount: reth::revm::primitives::U256,
    pub parent_evm_block_hash: H256,
}

impl SystemTxGenerator {
    pub fn new(
        block_height: u64,
        reward_address: Address,
        reward_amount: reth::revm::primitives::U256,
        parent_block: &IrysBlockHeader,
    ) -> Self {
        Self {
            block_height,
            reward_address,
            reward_amount,
            parent_evm_block_hash: H256::from_slice(&*parent_block.evm_block_hash),
        }
    }

    pub fn generate_all<'a>(
        &'a self,
        commitment_txs: &'a [CommitmentTransaction],
        submit_txs: &'a [IrysTransactionHeader],
    ) -> impl std::iter::Iterator<Item = SystemTransaction> + use<'a> {
        self.generate_system_tx_header()
            .chain(self.generate_commitment_system_transactions(commitment_txs))
            .chain(self.generate_data_storage_system_transactions(submit_txs))
    }

    /// Generates the expected data system transactions for a given block
    pub fn generate_system_tx_header(&self) -> impl std::iter::Iterator<Item = SystemTransaction> {
        std::iter::once(SystemTransaction::new_v1(
            self.block_height,
            self.parent_evm_block_hash.into(),
            TransactionPacket::BlockReward(BalanceIncrement {
                amount: self.reward_amount,
                target: self.reward_address,
            }),
        ))
    }

    /// Generates the expected data system transactions for a given block
    pub fn generate_data_storage_system_transactions<'a>(
        &'a self,
        submit_txs: &'a [IrysTransactionHeader],
    ) -> impl std::iter::Iterator<Item = SystemTransaction> + use<'a> {
        // create a storage fee system txs
        submit_txs.into_iter().map(move |tx| {
            SystemTransaction::new_v1(
                self.block_height,
                self.parent_evm_block_hash.into(),
                TransactionPacket::StorageFees(BalanceDecrement {
                    amount: Uint::from(tx.total_fee()),
                    target: tx.signer,
                }),
            )
        })
    }

    /// Generates the expected commitment transactions for a given block
    pub fn generate_commitment_system_transactions<'a>(
        &'a self,
        commitment_txs: &'a [CommitmentTransaction],
    ) -> impl std::iter::Iterator<Item = SystemTransaction> + use<'a> {
        commitment_txs
            .into_iter()
            .map(move |tx| match tx.commitment_type {
                irys_primitives::CommitmentType::Stake => SystemTransaction::new_v1(
                    self.block_height,
                    self.parent_evm_block_hash.into(),
                    TransactionPacket::Stake(BalanceDecrement {
                        amount: Uint::from(tx.total_fee()),
                        target: tx.signer,
                    }),
                ),
                irys_primitives::CommitmentType::Pledge => SystemTransaction::new_v1(
                    self.block_height,
                    self.parent_evm_block_hash.into(),
                    TransactionPacket::Pledge(BalanceIncrement {
                        amount: Uint::from(tx.total_fee()),
                        target: tx.signer,
                    }),
                ),
                irys_primitives::CommitmentType::Unpledge => SystemTransaction::new_v1(
                    self.block_height,
                    self.parent_evm_block_hash.into(),
                    TransactionPacket::Unpledge(BalanceDecrement {
                        amount: Uint::from(tx.total_fee()),
                        target: tx.signer,
                    }),
                ),
                irys_primitives::CommitmentType::Unstake => SystemTransaction::new_v1(
                    self.block_height,
                    self.parent_evm_block_hash.into(),
                    TransactionPacket::Unstake(BalanceIncrement {
                        amount: Uint::from(tx.total_fee()),
                        target: tx.signer,
                    }),
                ),
            })
    }
}
