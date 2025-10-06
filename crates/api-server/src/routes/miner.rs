use actix_web::{
    error::ErrorBadRequest,
    web::{self, Path},
    HttpResponse, Result as ActixResult,
};
use irys_types::Address;
use std::str::FromStr as _;

use crate::ApiState;

#[tracing::instrument(skip(state), fields(address = %path.as_str()))]
pub async fn get_miner_pledge_state(
    path: Path<String>,
    state: web::Data<ApiState>,
) -> ActixResult<HttpResponse> {
    let address_str = path.into_inner();
    let address =
        Address::from_str(&address_str).map_err(|_| ErrorBadRequest("Invalid address format"))?;

    // Return the canonical epoch snapshot's view of all partition assignments for this miner.
    let tree = state.block_tree.read();
    let epoch_snapshot = tree.canonical_epoch_snapshot();
    let assignments = epoch_snapshot.get_partition_assignments(address);

    Ok(HttpResponse::Ok().json(assignments))
}
