//! endpoint tests
use std::sync::Arc;

use crate::{
    api::{
        block_index_endpoint_request, chunk_endpoint_request, info_endpoint_request,
        network_config_endpoint_request, peer_list_endpoint_request, price_endpoint_request,
        version_endpoint_request,
    },
    utils::{mine_block, IrysNodeTest},
};
use actix_web::{http::header::ContentType, HttpMessage};
use irys_actors::BlockFinalizedMessage;
use irys_api_server::routes::index::NodeInfo;
use irys_database::{BlockIndexItem, DataLedger};
use irys_types::{Address, IrysTransactionHeader, Signature, H256};
use tokio::time::{sleep, Duration};
use tracing::info;

#[test_log::test(actix::test)]
async fn pricing_endpoint() -> eyre::Result<()> {
    let ctx = IrysNodeTest::default().start().await;
    let address = format!("http://127.0.0.1:{}", ctx.node_ctx.config.port);
    let data_size_bytes = 1024_u64;

    let response = price_endpoint_request(&address, DataLedger::Publish, data_size_bytes).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.content_type(), ContentType::json().to_string());

    ctx.node_ctx.stop().await;
    Ok(())
}
