#[cfg(test)]
mod tests {
    use ::irys_database::open_or_create_db;
    use actix::Actor;
    use irys_actors::{
        block_producer::SolutionFoundMessage,
        mempool::{GetBestMempoolTxs, MempoolActor},
    };
    use irys_api_server::{run_server, ApiState};
    use irys_chain::start_for_testing;
    use irys_config::IrysNodeConfig;
    use irys_database::tables::{IngressProofs, IrysTables};
    use irys_reth_node_bridge::adapter::node::RethNodeContext;
    use irys_storage::ChunkProvider;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{
        generate_data_root, generate_leaves, irys::IrysSigner, resolve_proofs, Address, Base64,
        DatabaseProvider, IrysTransaction, IrysTransactionHeader, StorageConfig, UnpackedChunk,
        H256, IRYS_CHAIN_ID, MAX_CHUNK_SIZE,
    };
    use k256::ecdsa::SigningKey;
    use rand::Rng as _;
    use reth::tasks::TaskManager;
    use reth_db::transaction::DbTx;
    use reth_db::Database as _;
    use std::sync::Arc;
    use tokio::{
        task,
        time::{sleep, timeout, Duration},
    };
    use tracing::info;

    use crate::block_production::block_production::capacity_chunk_solution;

    const DEV_PRIVATE_KEY: &str =
        "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
    const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

    #[actix_web::test]
    async fn post_external_tx_and_chunks_golden_path() -> eyre::Result<()> {
        // std::env::set_var("RUST_LOG", "actix_web=trace");
        // std::env::set_var("RUST_LOG", "trace");

        let temp_dir =
            setup_tracing_and_temp_dir(Some("post_external_tx_and_chunks_golden_path"), false);

        let mut config = IrysNodeConfig::default();
        config.base_directory = temp_dir.path().to_path_buf();

        let node = start_for_testing(config).await?;

        let reth_context = RethNodeContext::new(node.reth_handle.into()).await?;

        let poa_solution = capacity_chunk_solution(node.config.mining_signer.address());

        // let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
        // let arc_db = Arc::new(db);

        // let task_manager = TaskManager::current();
        // let storage_config = StorageConfig::default();

        // // TODO Fixup this test, maybe with some stubs
        // let mempool_actor = MempoolActor::new(
        //     irys_types::app_state::DatabaseProvider(arc_db.clone()),
        //     task_manager.executor(),
        //     IrysSigner::random_signer(),
        //     storage_config.clone(),
        //     Arc::new(Vec::new()).to_vec(),
        // );
        // let mempool_actor_addr = mempool_actor.start();

        // let chunk_provider = ChunkProvider::new(
        //     storage_config.clone(),
        //     Arc::new(Vec::new()).to_vec(),
        //     DatabaseProvider(arc_db.clone()),
        // );

        // let app_state = ApiState {
        //     db: DatabaseProvider(arc_db.clone()),
        //     mempool: mempool_actor_addr.clone(),
        //     chunk_provider: Arc::new(chunk_provider),
        // };

        // // spawn server in a seperate thread
        // task::spawn(run_server(app_state));

        let address = "http://127.0.0.1:8080";
        // TODO: remove this delay and use proper probing to check if the server is active
        sleep(Duration::from_millis(500)).await;

        println!("going to make web request");

        // server should be running
        // check with request to `/v1/info`
        let client = awc::Client::default();
        println!("made client");
        println!("making web request");

        let response = client
            .get(format!("{}/v1/info", address))
            .send()
            .await
            .unwrap();

        println!("made web request");

        assert_eq!(response.status(), 200);
        info!("HTTP server started");

        // for future debugging: https://github.com/Irys-xyz/irys/blob/e19cb4aa63fd155d140c87556b0c5c8db020c219/crates/chain/tests/api/external_api.rs#L87

        info!("waiting for tx header...");

        let recv_tx = loop {
            let txs = node.actor_addresses.mempool.send(GetBestMempoolTxs).await;
            match txs {
                Ok(transactions) if !transactions.is_empty() => {
                    break transactions[0].clone();
                }
                _ => {
                    sleep(Duration::from_millis(1_000)).await;
                }
            }
        };

        // now we wait for an ingress proof to be generated for this tx (automatic once all chunks have been uploaded)
        info!(
            "got tx {:?}- waiting for chunks & ingress proof generation...",
            &recv_tx.id
        );

        let ingress_proof = loop {
            // don't reuse the tx! it has read isolation (won't see anything commited after it's creation)
            let ro_tx = &node.db.tx().unwrap();
            match ro_tx.get::<IngressProofs>(recv_tx.data_root).unwrap() {
                Some(ip) => break ip,
                None => sleep(Duration::from_millis(100)).await,
            }
        };

        info!(
            "got ingress proof for data root {}",
            &ingress_proof.data_root
        );
        assert_eq!(&ingress_proof.data_root, &recv_tx.data_root);

        // now we mine a couple blocks

        for i in 1..=2 {
            info!("mining block {}", i);
            let fut = node
                .actor_addresses
                .block_producer
                .send(SolutionFoundMessage(poa_solution.clone()));
            let (block, _reth_exec_env) = fut.await?.unwrap();
            assert_eq!(&block.height, &i);
        }

        // now we wait 10s for the client to fetch the chunks
        // TODO: refactor to use traits so we can use `mockall`

        sleep(Duration::from_millis(1000_000)).await;
        Ok(())
    }
}
