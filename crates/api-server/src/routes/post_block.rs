use actix_web::{
    http::header::ContentType,
    web::{self, Json},
    HttpResponse,
};
use awc::http::StatusCode;
use eyre::eyre;
use irys_actors::block_discovery::BlockDiscoveredMessage;
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
    let irys_block = Arc::new(body.into_inner());
    info!("Received block");

    let validation_result = match state
        .block_discovery_service
        .send(BlockDiscoveredMessage(irys_block.clone()))
        .await
    {
        Ok(_) => Ok(()),
        Err(res) => {
            tracing::error!(
                "Received block {:?} ({}) failed pre-validation: {:?}",
                &irys_block.block_hash.0,
                &irys_block.height,
                res
            );
            Err(eyre!(
                "Received block {:?} ({}) failed pre-validation: {:?}",
                &irys_block.block_hash.0,
                &irys_block.height,
                res
            ))
        }
    };

    // handle block validation failure
    if let Err(e) = validation_result {
        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body(format!("Block failed validation: {:?}", e)));
    }

    // We don't know if everything succeeded, return an HTTP 200 OK response anyway
    Ok(HttpResponse::Ok()
        .content_type(ContentType::json())
        .finish())
}
