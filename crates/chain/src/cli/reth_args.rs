use clap::Parser;

#[derive(Debug, Parser)]
pub struct RethArgs {
    // -- Network --
    /// Reth network bind port
    #[arg(id = "reth.port", long = "reth.port", value_name = "PORT")]
    pub port: Option<u16>,

    /// Reth network bind IP address
    #[arg(id = "reth.bind_ip", long = "reth.bind-ip", value_name = "IP")]
    pub bind_ip: Option<String>,

    /// Reth network public port
    #[arg(
        id = "reth.public_port",
        long = "reth.public-port",
        value_name = "PORT"
    )]
    pub public_port: Option<u16>,

    // -- RPC --
    /// Enable Reth HTTP RPC server
    #[arg(id = "reth.rpc-http", long = "reth.rpc-http", value_name = "BOOL")]
    pub rpc_http: Option<bool>,

    /// Reth HTTP RPC port
    #[arg(
        id = "reth.rpc-http-port",
        long = "reth.rpc-http-port",
        value_name = "PORT"
    )]
    pub rpc_http_port: Option<u16>,

    /// Reth HTTP RPC modules (comma-separated, e.g. "eth,debug,net,trace")
    #[arg(
        id = "reth.rpc-http-api",
        long = "reth.rpc-http-api",
        value_name = "MODULES"
    )]
    pub rpc_http_api: Option<String>,

    /// Reth HTTP RPC CORS domain ("*" for any)
    #[arg(id = "reth.rpc-cors", long = "reth.rpc-cors", value_name = "DOMAIN")]
    pub rpc_corsdomain: Option<String>,

    /// Enable Reth WebSocket RPC server
    #[arg(id = "reth.rpc-ws", long = "reth.rpc-ws", value_name = "BOOL")]
    pub rpc_ws: Option<bool>,

    /// Reth WebSocket RPC port
    #[arg(
        id = "reth.rpc-ws-port",
        long = "reth.rpc-ws-port",
        value_name = "PORT"
    )]
    pub rpc_ws_port: Option<u16>,

    /// Reth WebSocket RPC modules (comma-separated)
    #[arg(
        id = "reth.rpc-ws-api",
        long = "reth.rpc-ws-api",
        value_name = "MODULES"
    )]
    pub rpc_ws_api: Option<String>,

    /// Max RPC request size in MB
    #[arg(
        id = "reth.rpc-max-request-size",
        long = "reth.rpc-max-request-size",
        value_name = "MB"
    )]
    pub rpc_max_request_size_mb: Option<u32>,

    /// Max RPC response size in MB
    #[arg(
        id = "reth.rpc-max-response-size",
        long = "reth.rpc-max-response-size",
        value_name = "MB"
    )]
    pub rpc_max_response_size_mb: Option<u32>,

    /// Max concurrent RPC connections
    #[arg(
        id = "reth.rpc-max-connections",
        long = "reth.rpc-max-connections",
        value_name = "N"
    )]
    pub rpc_max_connections: Option<u32>,

    /// Gas cap for eth_call and estimateGas
    #[arg(id = "reth.rpc-gas-cap", long = "reth.rpc-gas-cap", value_name = "GAS")]
    pub rpc_gas_cap: Option<u64>,

    /// Max transaction fee cap in wei
    #[arg(
        id = "reth.rpc-tx-fee-cap",
        long = "reth.rpc-tx-fee-cap",
        value_name = "WEI"
    )]
    pub rpc_tx_fee_cap: Option<u64>,

    // -- Transaction Pool --
    /// Max pending transactions
    #[arg(
        id = "reth.txpool-pending-max-count",
        long = "reth.txpool-pending-max-count",
        value_name = "N"
    )]
    pub txpool_pending_max_count: Option<usize>,

    /// Max pending pool size in MB
    #[arg(
        id = "reth.txpool-pending-max-size",
        long = "reth.txpool-pending-max-size",
        value_name = "MB"
    )]
    pub txpool_pending_max_size_mb: Option<usize>,

    /// Max basefee transactions
    #[arg(
        id = "reth.txpool-basefee-max-count",
        long = "reth.txpool-basefee-max-count",
        value_name = "N"
    )]
    pub txpool_basefee_max_count: Option<usize>,

    /// Max basefee pool size in MB
    #[arg(
        id = "reth.txpool-basefee-max-size",
        long = "reth.txpool-basefee-max-size",
        value_name = "MB"
    )]
    pub txpool_basefee_max_size_mb: Option<usize>,

    /// Max queued transactions
    #[arg(
        id = "reth.txpool-queued-max-count",
        long = "reth.txpool-queued-max-count",
        value_name = "N"
    )]
    pub txpool_queued_max_count: Option<usize>,

    /// Max queued pool size in MB
    #[arg(
        id = "reth.txpool-queued-max-size",
        long = "reth.txpool-queued-max-size",
        value_name = "MB"
    )]
    pub txpool_queued_max_size_mb: Option<usize>,

    /// Additional txpool validation tasks
    #[arg(
        id = "reth.txpool-validation-tasks",
        long = "reth.txpool-validation-tasks",
        value_name = "N"
    )]
    pub txpool_additional_validation_tasks: Option<usize>,

    /// Max txpool slots per account
    #[arg(
        id = "reth.txpool-max-account-slots",
        long = "reth.txpool-max-account-slots",
        value_name = "N"
    )]
    pub txpool_max_account_slots: Option<usize>,

    /// Txpool price bump percentage
    #[arg(
        id = "reth.txpool-price-bump",
        long = "reth.txpool-price-bump",
        value_name = "PCT"
    )]
    pub txpool_price_bump: Option<u64>,

    // -- Engine --
    /// Engine persistence threshold (blocks before flush)
    #[arg(
        id = "reth.engine-persistence-threshold",
        long = "reth.engine-persistence-threshold",
        value_name = "N"
    )]
    pub engine_persistence_threshold: Option<u64>,

    /// Engine memory block buffer target
    #[arg(
        id = "reth.engine-memory-block-buffer",
        long = "reth.engine-memory-block-buffer",
        value_name = "N"
    )]
    pub engine_memory_block_buffer_target: Option<u64>,

    // -- Metrics --
    /// Prometheus metrics port (0 for random)
    #[arg(
        id = "reth.metrics-port",
        long = "reth.metrics-port",
        value_name = "PORT"
    )]
    pub metrics_port: Option<u16>,
}
