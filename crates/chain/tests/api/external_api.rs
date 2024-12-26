#[cfg(test)]
mod tests {
    use ::irys_database::open_or_create_db;
    use actix::Actor;
    use irys_actors::mempool::{GetBestMempoolTxs, MempoolActor};
    use irys_api_server::{run_server, ApiState};
    use irys_database::tables::{IngressProofs, IrysTables};
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

        // for future debugging:

        // let chunk_size = MAX_CHUNK_SIZE;

        // // Create 2.5 chunks worth of data *  fill the data with random bytes
        // let data_size = (chunk_size as f64 * 2.5).round() as usize;
        // let data = vec![69u8; data_size];
        // // rand::thread_rng().fill(&mut data[..]);

        // // Create a new Irys API instance & a signed transaction
        // // let irys = IrysSigner::random_signer();

        // let signer = IrysSigner {
        //     signer: SigningKey::from_slice(hex::decode(DEV_PRIVATE_KEY).unwrap().as_slice())
        //         .unwrap(),
        //     chain_id: IRYS_CHAIN_ID,
        //     chunk_size,
        // };

        // let mut tx = IrysTransaction {
        //     header: IrysTransactionHeader {
        //         id: Default::default(),
        //         anchor: H256::from([1u8; 32]),
        //         signer: Default::default(),
        //         data_root: H256::from([3u8; 32]),
        //         data_size: 1024,
        //         term_fee: 100,
        //         perm_fee: None,
        //         ledger_num: 0,
        //         chain_id: IRYS_CHAIN_ID,
        //         bundle_format: None,
        //         version: 0,
        //         signature: Default::default(),
        //     },
        //     data: Base64(vec![]),
        //     chunks: vec![],
        //     proofs: vec![],
        // };

        // // let mut tx_header =
        // // let mer = signer.merklize(data_bytes, MAX_CHUNK_SIZE).unwrap();

        // let mut chunks = generate_leaves(data.clone(), chunk_size).unwrap();
        // let root = generate_data_root(chunks.clone()).unwrap();
        // let data_root = H256(root.id.clone());
        // let mut proofs = resolve_proofs(root, None).unwrap();

        // // Discard the last chunk & proof if it's zero length.
        // let last_chunk = chunks.last().unwrap();
        // if last_chunk.max_byte_range == last_chunk.min_byte_range {
        //     chunks.pop();
        //     proofs.pop();
        // }

        // tx.header.data_root = data_root;
        // tx.header.data_size = data.len() as u64;
        // tx.data = Base64(data.clone());
        // tx.chunks = chunks;
        // tx.proofs = proofs;

        // // let mut merkleized = signer.create_transaction(data.clone(), None).unwrap();
        // // tx_header.data
        // let tx = signer.sign_transaction(tx).unwrap();
        // info!("tx: {:?}", &tx.header);
        // info!("tx data_root {:?}", &tx.header.data_root.0);
        // info!("tx data_size {:?}", &tx.header.data_size);

        // // Loop though each of the transaction chunks
        // for (tx_chunk_offset, chunk_node) in tx.chunks.iter().enumerate() {
        //     let data_root = tx.header.data_root;
        //     let data_size = tx.header.data_size;
        //     let min = chunk_node.min_byte_range;
        //     let max = chunk_node.max_byte_range;
        //     let data_path = Base64(tx.proofs[tx_chunk_offset].proof.to_vec());

        //     let chunk = UnpackedChunk {
        //         data_root,
        //         data_size,
        //         data_path: data_path.clone(),
        //         bytes: Base64(data[min..max].to_vec()),
        //         tx_offset: tx_chunk_offset as u32,
        //     };
        //     info!(
        //         "chunk {}, path: {:?}, enc_path {:?}, bytes: {}",
        //         &tx_chunk_offset,
        //         &data_path.0,
        //         base64_url::encode(&data_path.0),
        //         &chunk.bytes
        //     );
        // }

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
