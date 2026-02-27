use irys_packing::PackingType;
use irys_utils::signal::run_until_ctrl_c_or_channel_message;
use std::{net::TcpListener, sync::Arc};
use tokio::{
    runtime::Handle,
    sync::{Semaphore, mpsc::Receiver},
};
use tracing::info;

use crate::{api::run_server, types::PackingWorkerConfig};

pub struct PackingWorkerStateInner {
    pub config: PackingWorkerConfig,
    pub runtime_handle: Handle,
    pub packing_semaphore: Arc<Semaphore>,
}

#[derive(Clone)]
pub struct PackingWorkerState(pub Arc<PackingWorkerStateInner>);

#[cfg(not(feature = "nvidia"))]
pub const PACKING_TYPE: PackingType = PackingType::CPU;

#[cfg(feature = "nvidia")]
pub const PACKING_TYPE: PackingType = PackingType::CUDA;

pub async fn start_worker(
    config: PackingWorkerConfig,
    listener: TcpListener,
    stop_rx: Receiver<irys_types::ShutdownReason>,
) -> eyre::Result<()> {
    // this limits the concurrency across *all* requests
    let packing_semaphore = Semaphore::new(if matches!(PACKING_TYPE, PackingType::CUDA) {
        1
    } else {
        config.cpu_packing_concurrency.into()
    });

    info!("Starting packing worker on {listener:?} with packing type {PACKING_TYPE:?}");
    let state: PackingWorkerState = PackingWorkerState(Arc::new(PackingWorkerStateInner {
        config,
        packing_semaphore: packing_semaphore.into(),
        runtime_handle: Handle::current(),
    }));

    let server = run_server(state, listener);

    use futures_util::TryFutureExt as _;
    run_until_ctrl_c_or_channel_message(server.map_err(Into::into), stop_rx, "packing-worker")
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use irys_testing_utils::initialize_tracing;
    use irys_types::{
        H256, InclusiveInterval as _, IrysAddress, PartitionChunkOffset, PartitionChunkRange, ii,
        partition_chunk_offset_ii,
        remote_packing::{PackingWorkerConfig, RemotePackingRequest},
    };
    use irys_utils::listener::create_listener;
    use std::{net::SocketAddr, num::NonZero};
    use tokio::sync::mpsc::channel;
    use tracing::debug;

    use crate::{api::PackingWorkerInfo, worker::start_worker};

    #[tokio::test]
    async fn heavy4_packing_worker_standalone_test() -> eyre::Result<()> {
        initialize_tracing();
        let config = PackingWorkerConfig {
            bind_port: 0,
            bind_addr: "127.0.0.1".to_string(),
            cpu_packing_concurrency: 2,
            gpu_packing_batch_size: 0,
            max_pending: NonZero::new(1).unwrap(),
        };

        let addr: SocketAddr = format!("{}:{}", &config.bind_addr, &config.bind_port).parse()?;
        let listener = create_listener(addr)?;

        let local_addr = listener.local_addr()?;

        let (tx, rx) = channel(1);
        let exit_handle = tokio::spawn(start_worker(config, listener, rx));

        // create a fake packing request
        let request = RemotePackingRequest {
            mining_address: IrysAddress::ZERO,
            partition_hash: H256::zero(),
            chunk_range: PartitionChunkRange(partition_chunk_offset_ii!(0, 100)),
            chain_id: 1234,
            chunk_size: 262_144,
            entropy_packing_iterations: 1_000,
        };

        let client = reqwest::Client::new();

        let base_url = format!("http://{}:{}/v1/", local_addr.ip(), local_addr.port());
        debug!("GOT URL {:?}", &base_url);

        // sleep(Duration::from_secs(10000)).await;

        let info = loop {
            // ensure we can GET /v1/
            if let Ok(info_res) = client.get(format!("{}info", &base_url)).send().await {
                break info_res.json::<PackingWorkerInfo>().await?;
            }
        };

        debug!("Got info! {:?}", &info);

        // now post a request to pack, and then ingest the body as bytes

        let moved_req = request.clone();
        let handle = tokio::task::spawn_blocking(|| {
            let request = moved_req;
            let chunk_count: u32 = request.chunk_range.0.width().0;
            let mut out = Vec::with_capacity(request.chunk_size as usize * chunk_count as usize);
            let start: u32 = request.chunk_range.start();
            let end: u32 = request.chunk_range.end();

            let mut inner_out = Vec::with_capacity(request.chunk_size as usize);
            for i in start..=end {
                irys_packing::capacity_single::compute_entropy_chunk(
                    request.mining_address,
                    i as u64,
                    request.partition_hash.0,
                    request.entropy_packing_iterations,
                    request.chunk_size as usize,
                    &mut inner_out,
                    request.chain_id,
                );
                out.extend_from_slice(&inner_out);
            }
            out
        });

        // while we're doing that, manually pack to make sure the returned values are correct

        let packed_bytes = client
            .post(format!("{}pack", &base_url))
            .json(&request)
            .send()
            .await?
            .bytes()
            .await?;

        let remote_packed = packed_bytes.to_vec();
        let local_packed = handle.await?;

        assert_eq!(remote_packed, local_packed);

        tx.send(irys_types::ShutdownReason::TestComplete).await?;
        exit_handle.await??;
        Ok(())
    }
}
