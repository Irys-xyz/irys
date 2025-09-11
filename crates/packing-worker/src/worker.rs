use irys_types::{Address, PartitionChunkRange, H256};
use irys_utils::signal::run_until_ctrl_c_or_channel_message;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use tokio::{
    runtime::Handle,
    sync::{mpsc::channel, Semaphore},
};
use tracing::info;

use crate::{
    api::{create_listener, run_server},
    config::PackingWorkerConfig,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemotePackingRequest {
    pub mining_address: Address,
    pub partition_hash: H256,
    pub chunk_range: PartitionChunkRange,
    pub chain_id: u64,
    pub chunk_size: u64,
    pub entropy_packing_iterations: u32,
}

pub struct PackingWorkerStateInner {
    pub config: PackingWorkerConfig,
    pub runtime_handle: Handle,
    pub packing_semaphore: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct PackingWorkerState(pub Arc<PackingWorkerStateInner>);

#[derive(PartialEq, Debug)]
pub enum PackingType {
    CPU,
    CUDA,
    // AMD,
}

#[cfg(not(feature = "nvidia"))]
pub const PACKING_TYPE: PackingType = PackingType::CPU;

#[cfg(feature = "nvidia")]
pub const PACKING_TYPE: PackingType = PackingType::CUDA;

pub async fn start_worker(config: PackingWorkerConfig) -> eyre::Result<()> {
    let addr: SocketAddr = format!("{}:{}", &config.bind_addr, &config.bind_port).parse()?;
    let listener = create_listener(addr)?;
    // this limits the concurrency across *all* requests
    let packing_semaphore = Semaphore::new(if matches!(PACKING_TYPE, PackingType::CUDA) {
        1
    } else {
        config.cpu_packing_concurrency.into()
    });

    info!("Starting packing worker on {addr:?} with packing type {PACKING_TYPE:?}");
    let state: PackingWorkerState = PackingWorkerState(Arc::new(PackingWorkerStateInner {
        config,
        packing_semaphore: packing_semaphore.into(),
        runtime_handle: Handle::current(),
    }));

    let server = run_server(state, listener);
    // dummy
    let (_tx, rx) = channel(1);

    use futures_util::TryFutureExt as _;
    run_until_ctrl_c_or_channel_message(server.map_err(Into::into), rx).await?;

    Ok(())
}
