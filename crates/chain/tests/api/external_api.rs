#[cfg(test)]
mod tests {
    use ::irys_database::open_or_create_db;
    use actix::Actor;
    use irys_actors::mempool::{GetBestMempoolTxs, MempoolActor};
    use irys_api_server::{run_server, ApiState};
    use irys_database::tables::{IngressProofs, IrysTables};
    use irys_storage::ChunkProvider;
    use irys_testing_utils::utils::setup_tracing_and_temp_dir;
    use irys_types::{irys::IrysSigner, DatabaseProvider, StorageConfig};

    use reth::tasks::TaskManager;
    use reth_db::transaction::DbTx;
    use reth_db::Database as _;
    use std::sync::Arc;
    use tokio::{
        task,
        time::{sleep, Duration},
    };
    use tracing::info;

    const DEV_PRIVATE_KEY: &str =
        "db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0";
    const DEV_ADDRESS: &str = "64f1a2829e0e698c18e7792d6e74f67d89aa0a32";

    #[actix_web::test]
    async fn post_external_tx_and_chunks_golden_path() {
        // std::env::set_var("RUST_LOG", "actix_web=trace");
        // std::env::set_var("RUST_LOG", "trace");

        let path =
            setup_tracing_and_temp_dir(Some("post_external_tx_and_chunks_golden_path"), false);
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
        let arc_db = Arc::new(db);

        let task_manager = TaskManager::current();
        let storage_config = StorageConfig::default();

        // TODO Fixup this test, maybe with some stubs
        let mempool_actor = MempoolActor::new(
            irys_types::app_state::DatabaseProvider(arc_db.clone()),
            task_manager.executor(),
            IrysSigner::random_signer(),
            storage_config.clone(),
            Arc::new(Vec::new()).to_vec(),
        );
        let mempool_actor_addr = mempool_actor.start();

        let chunk_provider = ChunkProvider::new(
            storage_config.clone(),
            Arc::new(Vec::new()).to_vec(),
            DatabaseProvider(arc_db.clone()),
        );

        let app_state = ApiState {
            db: DatabaseProvider(arc_db.clone()),
            mempool: mempool_actor_addr.clone(),
            chunk_provider: Arc::new(chunk_provider),
        };

        let address = "http://127.0.0.1:8080";

        // spawn server in a seperate thread
        task::spawn(run_server(app_state));
        // TODO: remove this delay and use proper probing to check if the server is active
        sleep(Duration::from_millis(500)).await;

        // server should be running
        // check with request to `/v1/info`
        let client = awc::Client::default();
        let response = client
            .get(format!("{}/v1/info", address))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), 200);
        info!("HTTP server started");

        // for future debugging: https://github.com/Irys-xyz/irys/blob/e19cb4aa63fd155d140c87556b0c5c8db020c219/crates/chain/tests/api/external_api.rs#L87

        info!("waiting for tx header...");

        let recv_tx = loop {
            let txs = mempool_actor_addr.send(GetBestMempoolTxs).await;
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
            let ro_tx = &arc_db.tx().unwrap();
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
    }
}
