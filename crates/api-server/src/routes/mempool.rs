use crate::ApiState;
use actix_web::{web, HttpResponse, Result};

/// GET /v1/mempool/status
pub async fn get_mempool_status(state: web::Data<ApiState>) -> Result<HttpResponse> {
    let status = state
        .mempool_guard
        .atomic_state()
        .get_status(&state.config.node_config, &state.chunk_ingress_state)
        .await;
    Ok(HttpResponse::Ok().json(status))
}
