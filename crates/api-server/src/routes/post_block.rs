use actix_web::{
    http::header::ContentType,
    web::{self, Json},
    HttpResponse,
};
use awc::http::StatusCode;
use irys_actors::mempool_service::MempoolServiceMessage;
use irys_types::IrysBlockHeader;
use std::sync::Arc;
use tracing::info;

use crate::ApiState;

/// Handles the HTTP POST request for adding a block to the mempool.
/// This function takes in a JSON payload of a `Block` type, encapsulates it
/// into a `ChunkIngressMessage` for further processing by the mempool actor,
/// and manages error handling based on the results of message delivery and validation.
pub async fn post_block(
    state: web::Data<ApiState>,
    body: Json<IrysBlockHeader>,
) -> actix_web::Result<HttpResponse> {
    let block = Arc::new(body.into_inner());
    info!("Received block");

    // Create a message and send it
    let tx_ingress_msg = MempoolServiceMessage::IngestBlocks {
        prevalidated_blocks: vec![block],
    };

    // Handle failure to deliver the message (e.g., channel closed)
    if let Err(err) = state.mempool_service.send(tx_ingress_msg) {
        tracing::error!("Failed to send to mempool channel: {:?}", err);
        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body(format!("Failed to send to mempool channel: {:?}", err)));
    }

    // We don't know if everything succeeded, return an HTTP 200 OK response anyway
    Ok(HttpResponse::Ok()
        .content_type(ContentType::json())
        .finish())
}
