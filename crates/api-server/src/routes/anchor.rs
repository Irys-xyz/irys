use crate::ApiState;
use actix_web::{http::header::ContentType, web, HttpResponse};
use irys_types::BlockHash;
use serde::{Deserialize, Serialize};

pub async fn anchor_route(state: web::Data<ApiState>) -> HttpResponse {
    if let Some(tip) = { state.block_index.read().get_latest_item() } {
        let resp = AnchorResponse {
            block_hash: tip.block_hash,
        };
        HttpResponse::Ok()
            .content_type(ContentType::json())
            .body(serde_json::to_string(&resp).unwrap())
    } else {
        HttpResponse::NoContent().finish()
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AnchorResponse {
    block_hash: BlockHash,
}
