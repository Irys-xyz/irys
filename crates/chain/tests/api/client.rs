//! api client tests

use crate::utils::IrysNodeTest;
use irys_api_client::{ApiClient, IrysApiClient};
use irys_types::{AcceptedResponse, PeerAddress, PeerResponse, ProtocolVersion, VersionRequest};
use semver::Version;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

#[actix_rt::test]
async fn heavy_api_client_all_endpoints_should_work() {
    // POST /version
    let ctx = IrysNodeTest::default_async().await.start().await;

    let api_address = SocketAddr::new(
        IpAddr::from_str("127.0.0.1").unwrap(),
        ctx.node_ctx.config.api_port,
    );
    let api_client = IrysApiClient::new();

    let version_request = VersionRequest::default();

    let expected_version_response = AcceptedResponse {
        version: Version {
            major: 1,
            minor: 2,
            patch: 0,
            pre: Default::default(),
            build: Default::default(),
        },
        protocol_version: ProtocolVersion::V1,
        peers: vec![PeerAddress {
            gossip: SocketAddr::from_str("127.0.0.1:8081").unwrap(),
            api: SocketAddr::from_str("127.0.0.1:8080").unwrap(),
        }],
        timestamp: 1744920031378,
        message: Some("Welcome to the network ".to_string()),
    };

    let post_version_response = api_client
        .post_version(api_address, version_request)
        .await
        .expect("valid post version response");

    let response_data = match post_version_response {
        PeerResponse::Accepted(response) => response,
        _ => panic!("Expected Accepted response"),
    };

    assert_eq!(response_data.version, expected_version_response.version);
    assert_eq!(
        response_data.protocol_version,
        expected_version_response.protocol_version
    );
    assert_eq!(response_data.peers, expected_version_response.peers);
    assert_eq!(response_data.message, expected_version_response.message);

    ctx.node_ctx.stop().await;
}
