use std::time::{SystemTime, UNIX_EPOCH};

use crate::ApiState;
use actix_web::{
    web::{self, Json},
    HttpResponse,
};

use irys_database::insert_peer_list_item;
use irys_types::{
    parse_user_agent, AcceptedResponse, PeerListItem, PeerResponse, ProtocolVersion,
    RejectedResponse, RejectionReason, VersionRequest,
};
use reth_db::Database;
use semver::Version;
pub async fn post_version(
    state: web::Data<ApiState>,
    body: Json<VersionRequest>,
) -> actix_web::Result<HttpResponse> {
    let version_request = body.into_inner();

    // Validate the request
    if version_request.protocol_version != ProtocolVersion::V1 {
        let response = PeerResponse::Rejected(RejectedResponse {
            reason: RejectionReason::ProtocolMismatch,
            message: Some("Unsupported protocol version".to_string()),
            retry_after: None,
        });
        return Ok(HttpResponse::BadRequest().json(response));
    }

    // Fetch peers and handle potential errors
    let peers = match state.get_known_peers() {
        Ok(peers) => peers,
        Err(e) => {
            let response = PeerResponse::Rejected(RejectedResponse {
                reason: RejectionReason::InternalError,
                message: Some(format!("Failed to fetch peers: {}", e)),
                retry_after: Some(5000),
            });
            return Ok(HttpResponse::ServiceUnavailable().json(response));
        }
    };

    let socket_addr = version_request.address;
    let mining_addr = version_request.mining_address;
    let peer_list_entry = PeerListItem {
        address: socket_addr,
        ..Default::default()
    };

    // Check if peer already exists in the list
    let is_new_peer = !peers.iter().any(|peer| peer == &socket_addr);

    // Only update if it's a new peer
    if is_new_peer {
        if state
            .db
            .update(|tx| insert_peer_list_item(tx, &mining_addr, &peer_list_entry))
            .is_err()
        {
            let response = PeerResponse::Rejected(RejectedResponse {
                reason: RejectionReason::InternalError,
                message: Some("Could not update peer list".to_string()),
                retry_after: Some(5000),
            });
            return Ok(HttpResponse::ServiceUnavailable().json(response));
        }
    }

    let node_name = version_request
        .user_agent
        .and_then(|ua| parse_user_agent(&ua))
        .map(|(name, _, _, _)| name)
        .unwrap_or_default();

    // Process accepted request
    let response = PeerResponse::Accepted(AcceptedResponse {
        version: Version::new(1, 2, 0),
        protocol_version: ProtocolVersion::V1,
        peers,
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        message: Some(format!("Welcome to the network {}", node_name)),
    });

    Ok(HttpResponse::Ok().json(response))
}
