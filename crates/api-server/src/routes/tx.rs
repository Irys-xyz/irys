use actix_web::{
    web::{self, Json},
    HttpResponse,
};
use irys_actors::{
    mempool::{TxIngressError, TxIngressMessage},
    ActorAddresses,
};
use awc::http::StatusCode;
use irys_types::IrysTransactionHeader;

/// Handles the HTTP POST request for adding a chunk to the mempool.
/// This function takes in a JSON payload of a `IrysTransactionHeader` type,
/// encapsulates it into a `TxIngressMessage` for further processing by the
/// mempool actor, and manages error handling based on the results of message
/// delivery and transaction validation.
pub async fn post_tx(
    state: web::Data<ActorAddresses>,
    body: Json<IrysTransactionHeader>,
) -> actix_web::Result<HttpResponse> {
    let tx = body.into_inner();

    // Validate transaction is valid. Check balances etc etc.
    let tx_ingress_msg = TxIngressMessage { 0: tx };
    let msg_result = state.mempool.send(tx_ingress_msg).await;

    // Handle failure to deliver the message (e.g., actor unresponsive or unavailable)
    if let Err(err) = msg_result {
        return Ok(HttpResponse::build(StatusCode::INTERNAL_SERVER_ERROR)
            .body(format!("Failed to deliver transaction: {:?}", err)));
    }

    // If message delivery succeeded, check for validation errors within the response
    let inner_result = msg_result.unwrap();
    if let Err(err) = inner_result {
        return match err {
            TxIngressError::InvalidSignature => Ok(HttpResponse::build(StatusCode::BAD_REQUEST)
                .body(format!("Invalid Signature: {:?}", err))),
            TxIngressError::Unfunded => Ok(HttpResponse::build(StatusCode::PAYMENT_REQUIRED)
                .body(format!("Unfunded: {:?}", err))),
            TxIngressError::Skipped => Ok(HttpResponse::Ok()
                .body("Already processed: the transaction was previously handled")),
        };
    }

    // If everything succeeded, return an HTTP 200 OK response
    Ok(HttpResponse::Ok().finish())
}
