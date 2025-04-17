//! api client tests

use crate::utils::IrysNodeTest;
use irys_api_client::{ApiClient, IrysApiClient};
use irys_types::VersionRequest;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use tracing::debug;

#[actix_rt::test]
async fn heavy_api_client_all_endpoints_should_work() {
    let ctx = IrysNodeTest::default_async().await.start().await;

    let api_address = SocketAddr::new(
        IpAddr::from_str("127.0.0.1").unwrap(),
        ctx.node_ctx.config.api_port,
    );
    let api_client = IrysApiClient::new();

    let version_request = VersionRequest::default();

    let post_version_response = api_client
        .post_version(api_address, version_request)
        .await
        .expect("valid post version response");

    debug!("Post version response: {:?}", post_version_response);

    ctx.node_ctx.stop().await;
}
