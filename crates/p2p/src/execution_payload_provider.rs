use crate::types::GossipDataRequest;
use crate::PeerList;
use irys_reth_node_bridge::IrysRethNodeAdapter;
use lru::LruCache;
use reth::builder::Block as _;
use reth::primitives::{Block, Header, Receipt, Transaction};
use reth::revm::primitives::B256;
use reth::rpc::api::EthApiClient;
use reth::rpc::types::engine::ExecutionPayload;
#[cfg(test)]
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, warn};

const PAYLOAD_CACHE_CAPACITY: usize = 1000;
const PAYLOAD_RECEIVERS_CAPACITY: usize = 1000;
const PAYLOAD_REQUESTS_CACHE_CAPACITY: usize = 1000;

#[derive(Debug, Clone)]
pub enum RethPayloadProvider {
    IrysRethAdapter(IrysRethNodeAdapter),
    #[cfg(test)]
    Mock(Arc<RwLock<HashMap<B256, ExecutionPayload>>>),
}

impl RethPayloadProvider {
    pub fn new(irys_reth_node_adapter: IrysRethNodeAdapter) -> Self {
        Self::IrysRethAdapter(irys_reth_node_adapter)
    }

    #[cfg(test)]
    pub fn new_mock() -> Self {
        Self::Mock(Arc::new(RwLock::new(HashMap::new())))
    }

    /// Fetches the execution payload for a given EVM block hash. You can get the EVM block hash
    /// from the Irys block header like this:
    /// ```rust
    /// use irys_types::IrysBlockHeader;
    /// let irys_block: IrysBlockHeader = IrysBlockHeader::new_mock_header(); // Obtain the Irys block header
    /// let evm_block_hash = irys_block.evm_block_hash; // Get the EVM block hash
    /// ```
    pub async fn payload(&self, evm_block_hash: B256) -> Option<ExecutionPayload> {
        let ctx = match self {
            Self::IrysRethAdapter(adapter) => &adapter.reth_node,
            #[cfg(test)]
            Self::Mock(_) => {
                return self.payload_mock(evm_block_hash).await;
            }
        };

        let rpc = ctx.rpc_client().unwrap();
        let evm_block = EthApiClient::<Transaction, Block, Receipt, Header>::block_by_hash(
            &rpc,
            evm_block_hash,
            true,
        )
        .await
        .ok()??;

        let sealed_block = evm_block.seal_slow();

        let payload =
            <<irys_reth_node_bridge::irys_reth::IrysEthereumNode as reth::api::NodeTypes>::Payload as reth::api::PayloadTypes>::block_to_payload(sealed_block);

        Some(payload.payload)
    }

    #[cfg(test)]
    pub async fn payload_mock(&self, evm_block_hash: B256) -> Option<ExecutionPayload> {
        if let Self::Mock(payloads) = self {
            let payloads = payloads.read().await;
            payloads.get(&evm_block_hash).cloned()
        } else {
            panic!("Tried to get payload from mock provider, but it is not a mock provider");
        }
    }
}

impl From<IrysRethNodeAdapter> for RethPayloadProvider {
    fn from(irys_adapter: IrysRethNodeAdapter) -> Self {
        Self::new(irys_adapter)
    }
}

#[derive(Clone, Debug)]
pub struct ExecutionPayloadProvider<TPeerList: PeerList> {
    cache: Arc<RwLock<ExecutionPayloadCache>>,
    reth_payload_provider: RethPayloadProvider,
    payload_senders:
        Arc<RwLock<LruCache<B256, Vec<tokio::sync::oneshot::Sender<ExecutionPayload>>>>>,
    peer_list: TPeerList,
}

impl<TPeerList> ExecutionPayloadProvider<TPeerList>
where
    TPeerList: PeerList,
{
    pub fn new(peer_list: TPeerList, reth_payload_provider: RethPayloadProvider) -> Self {
        Self {
            cache: Arc::new(RwLock::new(ExecutionPayloadCache {
                payloads: LruCache::new(NonZeroUsize::new(PAYLOAD_CACHE_CAPACITY).expect("payload capacity is not a non-zero usize")),
                payloads_currently_requested_from_the_network: LruCache::new(NonZeroUsize::new(PAYLOAD_REQUESTS_CACHE_CAPACITY).expect("payloads currently requested from the network capacity is not a non-zero usize")),
            })),
            // TODO: fix this to use a real RPC client
            reth_payload_provider,
            payload_senders: Arc::new(RwLock::new(LruCache::new(NonZeroUsize::new(PAYLOAD_RECEIVERS_CAPACITY).expect("payload senders capacity is not a non-zero usize")))),
            peer_list
        }
    }

    pub async fn add_payload_to_cache(&self, payload: ExecutionPayload) {
        {
            let mut cache = self.cache.write().await;
            cache.payloads.put(payload.block_hash(), payload.clone());
            cache
                .payloads_currently_requested_from_the_network
                .pop(&payload.block_hash());
        }
        if let Some(senders) = self
            .payload_senders
            .write()
            .await
            .pop(&payload.block_hash())
        {
            for sender in senders {
                if let Err(returned_payload) = sender.send(payload.clone()) {
                    warn!(
                        "Failed to send execution payload to receiver: {:?}",
                        returned_payload.block_hash()
                    );
                }
            }
        }
    }

    /// DO NOT USE THIS METHOD ANYWHERE WHERE YOU NEED TO RELIABLY GET THE PAYLOAD!
    /// Use [ExecutionPayloadProvider::wait_for_payload] instead.
    /// This method is used to retrieve the payload from the local cache or EVM node.
    pub(crate) async fn get_locally_stored_payload(
        &self,
        evm_block_hash: &B256,
    ) -> Option<ExecutionPayload> {
        if let Some(payload) = self.cache.write().await.payloads.get(evm_block_hash) {
            return Some(payload.clone());
        }
        if let Some(payload) = self.reth_payload_provider.payload(*evm_block_hash).await {
            return Some(payload);
        }
        None
    }

    /// Waits for the execution payload to arrive over gossip. This method will first check the local
    /// cache, then try to retrieve the payload from the network if it is not found locally.
    /// There's a limit on how many payloads can be requested at once, so if the limit is reached,
    /// the method will return `None`. This should not be a problem in practice, as the limit
    /// is currently set to 1000, which should not be reached in normal operation - there should
    /// be no case where 1000 blocks are validated at once.
    ///
    /// You can get the EVM block hash from the Irys block header like this:
    /// ```rust
    /// use irys_types::IrysBlockHeader;
    /// let irys_block = IrysBlockHeader::new_mock_header();
    /// let evm_block_hash = irys_block.evm_block_hash;
    /// ```
    pub async fn wait_for_payload(&self, evm_block_hash: &B256) -> Option<ExecutionPayload> {
        if let Some(payload) = self.get_locally_stored_payload(evm_block_hash).await {
            return Some(payload);
        }

        let (sender, receiver) = tokio::sync::oneshot::channel();

        {
            let mut payload_senders = self.payload_senders.write().await;
            if let Some(senders) = payload_senders.get_mut(evm_block_hash) {
                senders.push(sender)
            } else {
                payload_senders.push(*evm_block_hash, vec![sender]);
            }
        }

        self.request_payload_from_the_network(*evm_block_hash).await;

        let payload = receiver.await.ok()?;

        Some(payload)
    }

    pub async fn request_payload_from_the_network(&self, evm_block_hash: B256) {
        self.cache
            .write()
            .await
            .payloads_currently_requested_from_the_network
            .put(evm_block_hash, ());
        if let Err(peer_list_error) = self
            .peer_list
            .request_data_from_the_network(GossipDataRequest::ExecutionPayload(evm_block_hash))
            .await
        {
            self.cache
                .write()
                .await
                .payloads_currently_requested_from_the_network
                .pop(&evm_block_hash);
            error!(
                "Failed to request execution payload from the network: {:?}",
                peer_list_error
            );
        }
    }

    pub(crate) async fn is_waiting_for_payload(&self, evm_block_hash: &B256) -> bool {
        self.cache
            .read()
            .await
            .payloads_currently_requested_from_the_network
            .contains(evm_block_hash)
    }
}

#[derive(Debug)]
struct ExecutionPayloadCache {
    payloads: LruCache<B256, ExecutionPayload>,
    payloads_currently_requested_from_the_network: LruCache<B256, ()>,
}
