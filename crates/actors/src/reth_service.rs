use crate::mempool_service::MempoolServiceMessage;
use eyre::eyre;
use irys_database::{database, db::IrysDatabaseExt as _};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::{BlockHash, DatabaseProvider, RethPeerInfo, TokioServiceHandle, H256};
use reth::{
    network::{NetworkInfo as _, Peers as _},
    revm::primitives::B256,
    rpc::{eth::EthApiServer as _, types::BlockNumberOrTag},
    tasks::shutdown::Shutdown,
};
use tokio::sync::{mpsc::UnboundedReceiver, mpsc::UnboundedSender, oneshot};
use tracing::{debug, error, info, Instrument as _};

#[derive(Debug)]
pub struct RethService {
    shutdown: Shutdown,
    cmd_rx: UnboundedReceiver<RethServiceMessage>,
    handle: IrysRethNodeAdapter,
    db: DatabaseProvider,
    mempool: UnboundedSender<MempoolServiceMessage>,
}

#[derive(Debug, Clone, Copy)]
pub struct ForkChoiceUpdateMessage {
    pub head_hash: BlockHash,
    pub confirmed_hash: BlockHash,
    pub finalized_hash: BlockHash,
}

#[derive(Debug)]
pub enum RethServiceMessage {
    ForkChoice {
        update: ForkChoiceUpdateMessage,
        response: oneshot::Sender<()>,
    },
    ConnectToPeer {
        peer: RethPeerInfo,
        response: oneshot::Sender<eyre::Result<()>>,
    },
    GetPeeringInfo {
        response: oneshot::Sender<eyre::Result<RethPeerInfo>>,
    },
}

// Represents the fork-choice hashes we feed to Reth. Each field is an ancestor of `head`:
// - `head_hash`: current canonical tip (latest block we want Reth to follow).
// - `confirmed_hash`: migration/safe block, roughly `migration_depth` behind head.
// - `finalized_hash`: prune/finalized block, `block_tree_depth` behind the confirmed block.
#[derive(Debug, Clone, Copy, Default)]
pub struct ForkChoiceUpdate {
    pub head_hash: B256,
    pub confirmed_hash: B256,
    pub finalized_hash: B256,
}

async fn evm_block_hash_from_block_hash(
    mempool_service: &UnboundedSender<MempoolServiceMessage>,
    db: &DatabaseProvider,
    irys_hash: H256,
) -> eyre::Result<B256> {
    debug!(irys_hash = %irys_hash, "Resolving EVM block hash for Irys block");

    let irys_header = {
        let (tx, rx) = oneshot::channel();
        mempool_service
            .send(MempoolServiceMessage::GetBlockHeader(irys_hash, true, tx))
            .expect("expected send to mempool to succeed");
        let mempool_response = rx.await?;
        match mempool_response {
            Some(h) => {
                debug!(irys_hash = %irys_hash, "Found block in mempool");
                h
            }
            None => {
                debug!(irys_hash = %irys_hash, "Block not in mempool, checking database");
                db
                    .view_eyre(|tx| database::block_header_by_hash(tx, &irys_hash, false))?
                    .ok_or_else(|| {
                        error!(irys_hash = %irys_hash, "Irys block not found in mempool or database");
                        eyre!("Missing irys block {} in DB!", irys_hash)
                    })?
            }
        }
    };
    debug!(
        irys_hash = %irys_hash,
        evm_block_hash = %irys_header.evm_block_hash,
        height = irys_header.height,
        "Resolved Irys block to EVM block"
    );
    Ok(irys_header.evm_block_hash)
}

impl RethService {
    pub fn spawn_service(
        handle: IrysRethNodeAdapter,
        database_provider: DatabaseProvider,
        mempool: UnboundedSender<MempoolServiceMessage>,
        cmd_rx: UnboundedReceiver<RethServiceMessage>,
        runtime_handle: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        let (shutdown_signal, shutdown) = reth::tasks::shutdown::signal();

        let service = Self {
            shutdown,
            cmd_rx,
            handle,
            db: database_provider,
            mempool,
        };

        let join_handle = runtime_handle.spawn(
            async move {
                if let Err(err) = service.run().await {
                    error!(error = %err, "Reth service terminated with error");
                }
            }
            .in_current_span(),
        );

        TokioServiceHandle {
            name: "reth_service".to_string(),
            handle: join_handle,
            shutdown_signal,
        }
    }

    #[tracing::instrument(skip_all, err)]
    async fn run(mut self) -> eyre::Result<()> {
        info!("Starting Reth service");

        loop {
            tokio::select! {
                biased;

                _ = &mut self.shutdown => {
                    info!("Shutdown signal received for Reth service");
                    break;
                }

                command = self.cmd_rx.recv() => {
                    match command {
                        Some(command) => self.handle_command(command).await?,
                        None => {
                            info!("Reth service command channel closed");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }

    async fn handle_command(&mut self, command: RethServiceMessage) -> eyre::Result<()> {
        match command {
            RethServiceMessage::ForkChoice { update, response } => {
                self.handle_forkchoice(update).await?;
                let _ = response.send(());
            }
            RethServiceMessage::ConnectToPeer { peer, response } => {
                let result = self.connect_to_peer(peer);
                let _ = response.send(result);
            }
            RethServiceMessage::GetPeeringInfo { response } => {
                let result = self.get_peering_info();
                let _ = response.send(result);
            }
        }
        Ok(())
    }

    async fn handle_forkchoice(&mut self, update: ForkChoiceUpdateMessage) -> eyre::Result<()> {
        debug!(?update, "Received fork choice update command");

        let resolved = self.resolve_new_fcu(update).await?;
        self.process_fcu(resolved).await?;
        Ok(())
    }

    #[tracing::instrument(skip(self), err, ret)]
    async fn resolve_new_fcu(
        &self,
        new_fcu: ForkChoiceUpdateMessage,
    ) -> eyre::Result<ForkChoiceUpdate> {
        debug!("Resolving new fork choice update");

        let ForkChoiceUpdateMessage {
            head_hash,
            confirmed_hash,
            finalized_hash,
        } = new_fcu;

        let evm_head_hash =
            evm_block_hash_from_block_hash(&self.mempool, &self.db, head_hash).await?;

        let evm_confirmed_hash =
            evm_block_hash_from_block_hash(&self.mempool, &self.db, confirmed_hash).await?;

        let evm_finalized_hash =
            evm_block_hash_from_block_hash(&self.mempool, &self.db, finalized_hash).await?;

        Ok(ForkChoiceUpdate {
            head_hash: evm_head_hash,
            confirmed_hash: evm_confirmed_hash,
            finalized_hash: evm_finalized_hash,
        })
    }

    async fn process_fcu(&self, fcu: ForkChoiceUpdate) -> eyre::Result<ForkChoiceUpdate> {
        let ForkChoiceUpdate {
            head_hash,
            confirmed_hash,
            finalized_hash,
        } = fcu;

        tracing::debug!(
            head = %head_hash,
            confirmed = %confirmed_hash,
            finalized = %finalized_hash,
            "Updating Reth fork choice"
        );
        let handle = self.handle.clone();
        let eth_api = handle.inner.eth_api();

        let get_blocks = async || {
            let latest_before = eth_api.block_by_number(BlockNumberOrTag::Latest, false);
            let safe_before = eth_api.block_by_number(BlockNumberOrTag::Safe, false);
            let finalized_before = eth_api.block_by_number(BlockNumberOrTag::Finalized, false);
            futures::try_join!(latest_before, safe_before, finalized_before)
        };
        let (latest_before, safe_before, finalized_before) = get_blocks().await?;

        tracing::debug!(
            latest_block = ?latest_before.as_ref().map(|b| (b.header.number, b.header.hash)),
            safe_block = ?safe_before.as_ref().map(|b| (b.header.number, b.header.hash)),
            finalized_block = ?finalized_before.as_ref().map(|b| (b.header.number, b.header.hash)),
            "Reth state before fork choice update"
        );

        handle
            .update_forkchoice_full(head_hash, Some(confirmed_hash), Some(finalized_hash))
            .await
            .map_err(|e| {
                error!(error = %e, ?fcu, "Failed to update Reth fork choice");
                eyre!("Error updating reth with forkchoice {:?} - {}", &fcu, &e)
            })?;

        debug!("Fork choice update sent to Reth, fetching current state");

        let (latest_after, safe_after, finalized_after) = get_blocks().await?;
        tracing::debug!(
            latest_block = ?latest_after.as_ref().map(|b| (b.header.number, b.header.hash)),
            safe_block = ?safe_after.as_ref().map(|b| (b.header.number, b.header.hash)),
            finalized_block = ?finalized_after.as_ref().map(|b| (b.header.number, b.header.hash)),
            "Reth state after fork choice update"
        );

        eyre::ensure!(
            head_hash == latest_after.unwrap().header.hash,
            "head hashes don't match post FCU"
        );
        eyre::ensure!(
            confirmed_hash == safe_after.unwrap().header.hash,
            "safe/confirmed hashes don't match post FCU"
        );
        eyre::ensure!(
            finalized_hash == finalized_after.unwrap().header.hash,
            "finalized hashes don't match post FCU"
        );

        Ok(fcu)
    }

    fn connect_to_peer(&self, peer: RethPeerInfo) -> eyre::Result<()> {
        info!(
            peer_id = %peer.peer_id,
            address = %peer.peering_tcp_addr,
            "Connecting to peer"
        );
        self.handle
            .inner
            .network
            .add_peer(peer.peer_id, peer.peering_tcp_addr);
        debug!(peer_id = %peer.peer_id, "Peer connection initiated");
        Ok(())
    }

    fn get_peering_info(&self) -> eyre::Result<RethPeerInfo> {
        let handle = self.handle.clone();
        let peer_id = *handle.inner.network.peer_id();
        let local_addr = handle.inner.network.local_addr();

        debug!(
            peer_id = %peer_id,
            local_address = %local_addr,
            "Returning peering info"
        );

        Ok(RethPeerInfo {
            peer_id,
            peering_tcp_addr: local_addr,
        })
    }
}
