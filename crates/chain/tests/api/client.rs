//! api client tests

use crate::utils::{mine_block, IrysNodeTest};
use irys_api_client::{ApiClient, IrysApiClient};
use irys_chain::IrysNodeCtx;
use irys_types::{AcceptedResponse, PeerResponse, ProtocolVersion, VersionRequest};
use semver::Version;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

async fn check_post_version_endpoint(api_client: &IrysApiClient, api_address: SocketAddr) {
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
        peers: vec![],
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
}

async fn check_transaction_endpoints(
    api_client: &IrysApiClient,
    api_address: SocketAddr,
    ctx: &IrysNodeTest<IrysNodeCtx>,
) {
    // advance one block
    let (_previous_header, _payload) = mine_block(&ctx.node_ctx).await.unwrap().unwrap();
    // advance one block, finalizing the previous block
    let (_header, _payload) = mine_block(&ctx.node_ctx).await.unwrap().unwrap();

    let tx = ctx
        .create_signed_data_tx(&ctx.node_ctx.config.irys_signer(), vec![1, 2, 3])
        .unwrap();
    let tx_id = tx.header.id;
    let tx_2 = ctx
        .create_signed_data_tx(&ctx.node_ctx.config.irys_signer(), vec![4, 5, 6])
        .unwrap();
    let tx_2_id = tx_2.header.id;

    // This method doesn't return anything if there's no error
    api_client
        .post_transaction(api_address, tx.header.clone())
        .await
        .expect("valid post transaction response");
    api_client
        .post_transaction(api_address, tx_2.header.clone())
        .await
        .expect("valid post transaction response");

    // advance one block to add the transaction to the block
    let (_header, _payload) = mine_block(&ctx.node_ctx).await.unwrap().unwrap();

    let retrieved_tx = api_client
        .get_transaction(api_address, tx_id)
        .await
        .expect("valid get transaction response")
        .expect("transaction not found");

    assert_eq!(retrieved_tx, tx.header);

    let txs = api_client
        .get_transactions(api_address, &[tx_id, tx_2_id])
        .await
        .expect("valid get transactions response");

    assert_eq!(txs.len(), 2);
    assert!(txs.contains(&Some(tx.header)));
    assert!(txs.contains(&Some(tx_2.header)));
}

#[actix_rt::test]
async fn heavy_api_client_all_endpoints_should_work() {
    let ctx = IrysNodeTest::default_async().await.start().await;

    let api_address = SocketAddr::new(
        IpAddr::from_str("127.0.0.1").unwrap(),
        ctx.node_ctx.config.node_config.http.port,
    );
    let api_client = IrysApiClient::new();

    check_post_version_endpoint(&api_client, api_address).await;
    check_transaction_endpoints(&api_client, api_address, &ctx).await;

    ctx.node_ctx.stop().await;
}
