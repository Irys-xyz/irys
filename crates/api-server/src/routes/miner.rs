use actix_web::{
    error::ErrorBadRequest,
    web::{self, Path},
    HttpResponse, Result as ActixResult,
};
use irys_types::{partition::PartitionHash, Address, CommitmentType, H256};
use serde::Serialize;
use std::collections::HashSet;
use std::str::FromStr as _;

use crate::ApiState;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacityPartitionView {
    pub partition_hash: PartitionHash,
    pub pending_unpledge: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MinerPledgeState {
    pub address: Address,
    pub capacity_partitions: Vec<CapacityPartitionView>,
}

#[tracing::instrument(skip(state), fields(address = %path.as_str()))]
pub async fn get_miner_pledge_state(
    path: Path<String>,
    state: web::Data<ApiState>,
) -> ActixResult<HttpResponse> {
    let address_str = path.into_inner();
    let address =
        Address::from_str(&address_str).map_err(|_| ErrorBadRequest("Invalid address format"))?;

    let tree = state.block_tree.read();
    let epoch_snapshot = tree.canonical_epoch_snapshot();
    let commitment_snapshot = tree.canonical_commitment_snapshot();
    drop(tree);

    // Collect the set of partitions that have a pending unpledge for this address
    let pending_unpledge_hashes: HashSet<H256> = commitment_snapshot
        .commitments
        .get(&address)
        .map(|mc| {
            mc.unpledges
                .iter()
                .filter_map(|tx| match tx.commitment_type {
                    CommitmentType::Unpledge { partition_hash, .. } => Some(partition_hash.into()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // Enumerate capacity partitions owned by address from epoch snapshot
    let out: Vec<CapacityPartitionView> = epoch_snapshot
        .partition_assignments
        .capacity_partitions
        .iter()
        .filter(|(_, a)| a.miner_address == address)
        .map(|(hash, _)| CapacityPartitionView {
            partition_hash: *hash,
            pending_unpledge: pending_unpledge_hashes.contains(hash),
        })
        .collect();

    // Natural ordering by hash is stable (BTreeMap iteration already sorted),
    // keep as-is for deterministic client behavior.

    Ok(HttpResponse::Ok().json(MinerPledgeState {
        address,
        capacity_partitions: out,
    }))
}
