use crate::{error::ApiError, ApiState};
use actix_web::{
    http::header::ContentType,
    web::{self},
    HttpResponse, ResponseError as _,
};
use awc::http::StatusCode;
use irys_domain::get_node_info;

pub async fn info_route(state: web::Data<ApiState>) -> HttpResponse {
    match get_node_info(
        &state.block_index,
        &state.block_tree,
        &state.peer_list,
        &state.sync_state,
        state.started_at,
        state.mining_address,
        state.config.consensus.chain_id,
    )
    .await
    {
        Ok(node_info) => HttpResponse::Ok()
            .content_type(ContentType::json())
            .json(node_info),
        Err(e) => {
            tracing::error!(error = %e, "Failed to retrieve node info");
            ApiError::from((e.to_string(), StatusCode::INTERNAL_SERVER_ERROR)).error_response()
        }
    }
}

pub async fn ready_route() -> HttpResponse {
    HttpResponse::Ok().finish()
}

pub async fn genesis_route(state: web::Data<ApiState>) -> HttpResponse {
    let genesis_hash = state
        .block_index
        .read()
        .get_item(0)
        .map(|item| item.block_hash);

    if let Some(hash) = genesis_hash {
        let genesis_info = serde_json::json!({
            "genesis_block_hash": hash,
            "height": 0
        });

        HttpResponse::Ok()
            .content_type(ContentType::json())
            .body(serde_json::to_string_pretty(&genesis_info).unwrap())
    } else {
        std::convert::Into::<ApiError>::into((
            "Genesis block not found in block index".to_owned(),
            StatusCode::INTERNAL_SERVER_ERROR,
        ))
        .error_response()
    }
}
