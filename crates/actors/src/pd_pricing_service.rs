use crate::block_tree_service::BlockTreeServiceMessage;
use crate::block_tree_service::PdCanonicalUpdate;
use crate::mempool_service::MempoolServiceMessage;
use crate::services::ServiceSenders;
use irys_reth::pd_pricing::state::PdPricingHandle;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::DatabaseProvider;
use irys_types::{ConsensusConfig, EvmBlockHash, TokioServiceHandle, U256};
use reth::tasks::shutdown::Shutdown;
use tokio::sync::broadcast;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, error, info, Instrument as _};

/// Service responsible for updating PD pricing parameters when canonical chain changes.
#[derive(Debug)]
pub struct PdPricingService {
    shutdown: Shutdown,
    pd_rx: broadcast::Receiver<PdCanonicalUpdate>,
    // No reorg branch needed; handled via canonical updates
    reth: IrysRethNodeAdapter,
    pd_handle: PdPricingHandle,
    db: DatabaseProvider,
    mempool: UnboundedSender<MempoolServiceMessage>,
    consensus: ConsensusConfig,
    block_tree: UnboundedSender<BlockTreeServiceMessage>,
}

impl PdPricingService {
    pub fn spawn_service(
        service_senders: ServiceSenders,
        reth: IrysRethNodeAdapter,
        pd_handle: PdPricingHandle,
        db: DatabaseProvider,
        mempool: UnboundedSender<MempoolServiceMessage>,
        consensus: ConsensusConfig,
        runtime: tokio::runtime::Handle,
    ) -> TokioServiceHandle {
        let pd_rx = service_senders.subscribe_pd_canonical();
        let (shutdown_signal, shutdown) = reth::tasks::shutdown::signal();

        let service = Self {
            shutdown,
            pd_rx,
            reth,
            pd_handle,
            db,
            mempool,
            consensus,
            block_tree: service_senders.block_tree.clone(),
        };
        let handle = runtime.spawn(
            async move {
                if let Err(err) = service.run().await {
                    error!(%err, "PD pricing service terminated with error");
                }
            }
            .in_current_span(),
        );

        TokioServiceHandle {
            name: "pd_pricing_service".to_string(),
            handle,
            shutdown_signal,
        }
    }

    async fn run(mut self) -> eyre::Result<()> {
        info!("Starting PD pricing service");
        // Initialize from latest canonical head
        self.initialize_from_canonical_head().await?;
        loop {
            tokio::select! {
                biased;
                _ = &mut self.shutdown => {
                    info!("Shutdown PD pricing service");
                    break;
                }
                Ok(update) = self.pd_rx.recv() => {
                    if let Err(e) = self.on_pd_canonical_update(update).await {
                        error!(error=%e, "failed PD canonical update");
                    }
                }
            }
        }
        Ok(())
    }

    async fn on_pd_canonical_update(&self, update: PdCanonicalUpdate) -> eyre::Result<()> {
        let header = update.head;
        // Try PD extraction from EVM block access lists
        let chunks_used = self
            .sum_pd_chunks_in_evm_block(header.evm_block_hash)
            .await
            .unwrap_or(0);
        // Compute next base rate and min fee using U256 math
        let old_base_rate = self.read_current_base_rate();
        let new_base_rate: U256 =
            irys_reth::pd_tx::step_base_rate_u256(old_base_rate.into(), chunks_used).into();
        let price = header.ema_irys_price; // use EMA price for conversion
        let min_fee_tokens: U256 = {
            let tokens = irys_reth::pd_pricing::usd_to_tokens_floor(
                irys_reth::constants::USD_CENT_SCALED_ALLOY,
                price.amount.into(),
            );
            tokens.into()
        }; // $0.01 scaled

        // Atomically update PD state
        self.pd_handle
            .set_price_and_min_base(price.amount.into(), min_fee_tokens.into());
        self.pd_handle
            .set_base_rate_usd_per_mb(new_base_rate.into());

        debug!(head = %header.block_hash, chunks_used, "PD pricing updated");
        Ok(())
    }

    fn read_current_base_rate(&self) -> U256 {
        // Snapshot the current base_rate_usd_per_mb (scaled 1e18); if poisoned, default to $0.01
        match self.pd_handle.0.read() {
            Ok(g) => g.base_rate_usd_per_mb_scaled.into(),
            Err(_) => irys_reth::constants::USD_CENT_SCALED_ALLOY.into(),
        }
    }

    async fn sum_pd_chunks_in_evm_block(&self, _evm_block_hash: EvmBlockHash) -> eyre::Result<u64> {
        // TODO(pd): implement RPC-based PD chunk counting from transaction access lists.
        Ok(0)
    }
}

// -------- Pricing helpers (U256 math) --------

impl PdPricingService {
    async fn initialize_from_canonical_head(&self) -> eyre::Result<()> {
        use irys_domain::BlockTreeReadGuard;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.block_tree
            .send(BlockTreeServiceMessage::GetBlockTreeReadGuard { response: tx })
            .map_err(|_| eyre::eyre!("failed to request block tree guard"))?;
        let guard: BlockTreeReadGuard = rx.await?;
        // Limit the scope of the tree read guard to avoid holding it across await.
        let header = {
            let tree = guard.read();
            let (canonical, _) = tree.get_canonical_chain();
            let Some(head_entry) = canonical.last() else {
                return Ok(());
            };
            tree.get_block(&head_entry.block_hash)
                .ok_or_else(|| eyre::eyre!("head block not in block tree"))?
                .clone()
        };

        // Derive PD chunks from EVM block
        let chunks_used = self
            .sum_pd_chunks_in_evm_block(header.evm_block_hash)
            .await
            .unwrap_or(0);

        // Compute next base rate and min fee using U256 math
        let old_base_rate = self.read_current_base_rate();
        let new_base_rate: U256 =
            irys_reth::pd_tx::step_base_rate_u256(old_base_rate.into(), chunks_used).into();
        let price = header.ema_irys_price; // EMA conversion
        let min_fee_tokens: U256 = {
            let tokens = irys_reth::pd_pricing::usd_to_tokens_floor(
                irys_reth::constants::USD_CENT_SCALED_ALLOY,
                price.amount.into(),
            );
            tokens.into()
        }; // $0.01

        // Atomically update PD state
        self.pd_handle
            .set_price_and_min_base(price.amount.into(), min_fee_tokens.into());
        self.pd_handle
            .set_base_rate_usd_per_mb(new_base_rate.into());
        Ok(())
    }
}
