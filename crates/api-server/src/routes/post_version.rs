use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ApiState;
use actix_web::{
    http::header::ContentType,
    web::{self, Json},
    HttpRequest, HttpResponse,
};

use irys_types::{
    parse_user_agent, AcceptedResponse, PeerListItem, PeerResponse, ProtocolVersion,
    RejectedResponse, RejectionReason, VersionRequest,
};
use semver::Version;

pub async fn post_version(
    req: HttpRequest,
    state: web::Data<ApiState>,
    body: Json<VersionRequest>,
) -> actix_web::Result<HttpResponse> {
    let connection_info = req.connection_info();
    let Some(source_addr_str) = connection_info.peer_addr() else {
        let response = PeerResponse::Rejected(RejectedResponse {
            reason: RejectionReason::InvalidCredentials,
            message: Some("Can't determine source ip".to_string()),
            retry_after: None,
        });
        return Ok(HttpResponse::BadRequest().json(response));
    };
    let Ok(source_addr) = source_addr_str.parse::<IpAddr>() else {
        let response = PeerResponse::Rejected(RejectedResponse {
            reason: RejectionReason::InvalidCredentials,
            message: Some("Invalid source ip address".to_string()),
            retry_after: None,
        });
        return Ok(HttpResponse::BadRequest().json(response));
    };

    let version_request = body.into_inner();

    // TODO: this is not a perfect way to validate the source ip address,
    //  but generally handshake comes from the same process that is running the gossip server.
    if source_addr != version_request.address.gossip.ip() {
        let response = PeerResponse::Rejected(RejectedResponse {
            reason: RejectionReason::InvalidCredentials,
            message: Some("The source address does not match the request address".to_string()),
            retry_after: None,
        });
        return Ok(HttpResponse::BadRequest().json(response));
    }

    // Validate the request
    if version_request.protocol_version != ProtocolVersion::V1 {
        let response = PeerResponse::Rejected(RejectedResponse {
            reason: RejectionReason::ProtocolMismatch,
            message: Some("Unsupported protocol version".to_string()),
            retry_after: None,
        });
        return Ok(HttpResponse::BadRequest().json(response));
    }

    if !version_request.verify_signature() {
        let response = PeerResponse::Rejected(RejectedResponse {
            reason: RejectionReason::InvalidCredentials,
            message: Some("Signature verification failed".to_string()),
            retry_after: None,
        });
        return Ok(HttpResponse::BadRequest().json(response));
    }

    // Fetch peers and handle potential errors
    let peers = state.get_known_peers();

    let peer_address = version_request.address;
    let mining_addr = version_request.mining_address;
    let peer_list_entry = PeerListItem {
        address: peer_address,
        ..Default::default()
    };

    let is_staked = state
        .block_tree
        .read()
        .canonical_epoch_snapshot()
        .is_staked(mining_addr);
    state
        .peer_list
        .add_or_update_peer(mining_addr, peer_list_entry, is_staked);

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

    Ok(HttpResponse::Ok()
        .content_type(ContentType::json())
        .json(response))
}
