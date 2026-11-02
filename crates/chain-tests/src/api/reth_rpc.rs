use crate::utils::IrysNodeTest;
use irys_types::NodeConfig;
use serde_json::{Value, json};

/// JSON-RPC error code for a method the server does not serve.
const METHOD_NOT_FOUND: i64 = -32601;

async fn reth_rpc_call(url: &str, method: &str, params: Value) -> eyre::Result<Value> {
    let response = reqwest::Client::new()
        .post(url)
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .send()
        .await?
        .json::<Value>()
        .await?;
    Ok(response)
}

/// With the `debug` namespace on, the Reth HTTP RPC serves the read and trace
/// methods and does not serve the methods that change node state.
#[test_log::test(tokio::test)]
async fn heavy_reth_rpc_debug_namespace_serves_only_allowed_methods() -> eyre::Result<()> {
    let mut config = NodeConfig::testing();
    config.reth.rpc_args = vec!["--http.api=eth,debug".to_string()];
    let node = IrysNodeTest::new_genesis(config).start().await;

    let port = node
        .node_ctx
        .reth_node_adapter
        .rpc_server_handle()
        .http_local_addr()
        .expect("reth HTTP RPC is enabled")
        .port();
    let url = format!("http://127.0.0.1:{port}");

    let set_head = reth_rpc_call(&url, "debug_setHead", json!(["0x0"])).await?;
    assert_eq!(
        set_head["error"]["code"].as_i64(),
        Some(METHOD_NOT_FOUND),
        "debug_setHead must not be served: {set_head}"
    );

    let trace = reth_rpc_call(
        &url,
        "debug_traceTransaction",
        json!([format!("0x{}", "00".repeat(32))]),
    )
    .await?;
    assert_ne!(
        trace["error"]["code"].as_i64(),
        Some(METHOD_NOT_FOUND),
        "debug_traceTransaction must be served: {trace}"
    );

    let raw_header = reth_rpc_call(&url, "debug_getRawHeader", json!(["latest"])).await?;
    assert!(
        raw_header["result"].is_string(),
        "debug_getRawHeader must be served: {raw_header}"
    );

    node.stop().await;
    Ok(())
}
