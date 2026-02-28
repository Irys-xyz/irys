use crate::block_validation::calculate_perm_storage_total_fee;
use crate::mempool_service::TxIngressError;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_reth_node_bridge::ext::IrysRethRpcTestContextExt as _;
use irys_types::storage_pricing::{
    Amount, calculate_term_fee,
    phantoms::{Irys, NetworkFee},
};
use irys_types::{Config, IrysAddress, IrysTransactionCommon, U256, UnixTimestamp};
use reth::rpc::types::BlockId;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::debug;

/// Calculate the expected protocol fee for permanent storage
/// This includes base network fee + ingress proof rewards
#[tracing::instrument(level = "trace", skip_all, err)]
pub(crate) fn calculate_perm_storage_fee(
    config: &Config,
    bytes_to_store: u64,
    term_fee: U256,
    ema: &Arc<irys_domain::EmaSnapshot>,
    timestamp_secs: UnixTimestamp,
) -> Result<Amount<(NetworkFee, Irys)>, TxIngressError> {
    calculate_perm_storage_total_fee(bytes_to_store, term_fee, ema, config, timestamp_secs)
        .map_err(TxIngressError::other_display)
}

/// Calculate the expected term fee for temporary storage
/// This matches the calculation in the pricing API and uses dynamic epoch count
#[tracing::instrument(level = "trace", skip_all, fields(bytes_to_store = bytes_to_store, block_height = block_height))]
pub(crate) fn calculate_term_storage_fee(
    config: &Config,
    bytes_to_store: u64,
    ema: &Arc<irys_domain::EmaSnapshot>,
    block_height: u64,
    timestamp: UnixTimestamp,
) -> Result<U256, TxIngressError> {
    let epochs_for_storage = irys_types::ledger_expiry::calculate_submit_ledger_expiry(
        block_height,
        config.consensus.epoch.num_blocks_in_epoch,
        config.consensus.epoch.submit_ledger_epoch_length,
    );
    let number_of_ingress_proofs_total = config.number_of_ingress_proofs_total_at(timestamp);
    calculate_term_fee(
        bytes_to_store,
        epochs_for_storage,
        &config.consensus,
        number_of_ingress_proofs_total,
        ema.ema_for_public_pricing(),
    )
    .map_err(|e| TxIngressError::Other(format!("Failed to calculate term fee: {}", e)))
}

pub(crate) async fn fetch_balances_for_transactions<T: IrysTransactionCommon>(
    reth_adapter: &IrysRethNodeAdapter,
    block_id: Option<BlockId>,
    txs: &[T],
) -> HashMap<IrysAddress, U256> {
    let signers: Vec<IrysAddress> = txs
        .iter()
        .map(irys_types::IrysTransactionCommon::signer)
        .collect();
    reth_adapter
        .reth_node
        .rpc
        .get_balances_irys(&signers, block_id)
        .await
}

// Helper function that verifies transaction funding and tracks cumulative fees
// Returns true if the transaction can be funded based on current account balance
// and previously included transactions in this block
pub(crate) fn check_funding<T: IrysTransactionCommon>(
    tx: &T,
    balances: &HashMap<IrysAddress, U256>,
    unfunded_address: &mut HashSet<IrysAddress>,
    fees_spent_per_address: &mut HashMap<IrysAddress, U256>,
) -> bool {
    let signer = tx.signer();

    // Skip transactions from addresses with previously unfunded transactions
    // This ensures we don't include any transactions (including pledges) from
    // addresses that couldn't afford their stake commitments
    if unfunded_address.contains(&signer) {
        return false;
    }

    let fee = tx.total_cost();
    let current_spent = *fees_spent_per_address.get(&signer).unwrap_or(&U256::zero());

    // Calculate total required balance including previously selected transactions

    // get balance state for the block we're building off of
    let balance = balances.get(&signer).copied().unwrap_or_else(U256::zero);

    let has_funds = balance >= current_spent + fee;

    // Track fees for this address regardless of whether this specific transaction is included
    fees_spent_per_address
        .entry(signer)
        .and_modify(|val| *val += fee)
        .or_insert(fee);

    // If transaction cannot be funded, mark the entire address as unfunded
    // Since stakes are processed before pledges, this prevents inclusion of
    // pledge commitments when their associated stake commitment is unfunded
    if !has_funds {
        debug!(
        tx.signer = ?signer,
        account.balance = ?balance,
        "Transaction funding check failed"
        );
        unfunded_address.insert(signer);
        return false;
    }

    has_funds
}
