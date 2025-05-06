use crate::service::fast_forward_vdf_steps_from_block;
use crate::types::GossipDataRequest;
use crate::GossipClient;
use actix::{
    Actor, Addr, AsyncContext, Context, Handler, Message, ResponseActFuture, Supervised,
    SystemService, WrapFuture,
};
use base58::ToBase58;
use irys_actors::block_discovery::BlockDiscoveredMessage;
use irys_actors::broadcast_mining_service::BroadcastMiningSeed;
use irys_actors::peer_list_service::{PeerListFacade, PeerListFacadeError};
use irys_api_client::ApiClient;
use irys_database::block_header_by_hash;
use irys_database::reth_db::Database;
use irys_types::{Address, BlockHash, DatabaseProvider, IrysBlockHeader, RethPeerInfo};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

#[derive(Debug)]
pub enum BlockPoolError {
    DatabaseError(eyre::Error),
    OtherInternal(String),
    BlockError(eyre::Error),
}

impl From<PeerListFacadeError> for BlockPoolError {
    fn from(err: PeerListFacadeError) -> Self {
        match err {
            PeerListFacadeError::InternalError(error) => {
                Self::OtherInternal(format!("Peer list error: {:?}", error))
            }
        }
    }
}

#[derive(Debug)]
pub struct BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    /// Database provider for accessing transaction headers and related data.
    pub db: Option<DatabaseProvider>,
    pub irys_api_client: A,

    pub orphaned_blocks_by_parent: HashMap<BlockHash, IrysBlockHeader>,
    pub block_hash_to_parent_hash: HashMap<BlockHash, BlockHash>,

    pub block_producer_addr: Option<Addr<B>>,
    pub peer_list: Option<PeerListFacade<A, R>>,
    pub gossip_client: GossipClient,
    pub vdf_sender: Option<tokio::sync::mpsc::Sender<BroadcastMiningSeed>>,
}

impl<A, R, B> Default for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    fn default() -> Self {
        Self {
            db: None,
            irys_api_client: A::default(),
            orphaned_blocks_by_parent: HashMap::new(),
            block_hash_to_parent_hash: HashMap::new(),
            block_producer_addr: None,
            peer_list: None,
            gossip_client: GossipClient::new(Duration::from_secs(5), Address::default()),
            vdf_sender: None,
        }
    }
}

impl<A, R, B> Actor for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    type Context = actix::Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        let _block_service_addr = ctx.address();
    }
}

impl<A, R, B> Supervised for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
}

impl<A, R, B> SystemService for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
}

impl<A, R, B> BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    pub fn new_with_client(
        db: DatabaseProvider,
        irys_api_client: A,
        peer_list: PeerListFacade<A, R>,
        block_producer_addr: Addr<B>,
        gossip_client: GossipClient,
        vdf_sender: Option<tokio::sync::mpsc::Sender<BroadcastMiningSeed>>,
    ) -> Self {
        Self {
            db: Some(db),
            irys_api_client,
            orphaned_blocks_by_parent: HashMap::new(),
            block_hash_to_parent_hash: HashMap::new(),
            peer_list: Some(peer_list),
            block_producer_addr: Some(block_producer_addr),
            gossip_client,
            vdf_sender,
        }
    }

    fn process_block(
        self: &mut Self,
        block_header: IrysBlockHeader,
        ctx: &mut <BlockPoolService<A, R, B> as Actor>::Context,
    ) -> ResponseActFuture<Self, Result<(), BlockPoolError>> {
        debug!("Processing block {}", block_header.block_hash.0.to_base58());
        let prev_block_hash = block_header.previous_block_hash;
        let current_block_hash = block_header.block_hash;
        let vdf_limiter_info = block_header.vdf_limiter_info.clone();
        let self_addr = ctx.address();
        let block_producer_addr = self.block_producer_addr.clone();
        let db = self.db.clone();
        let vdf_sender = self.vdf_sender.clone().expect("valid vdf sender");

        error!(
            "GOSSIP process_block() BLOCK HEIGHT: {}",
            block_header.height
        );

        Box::pin(
            async move {
                debug!(
                    "Searching for parent block {} for block {} in the db",
                    prev_block_hash.0.to_base58(),
                    current_block_hash.0.to_base58()
                );
                // Check if the previous block is in the db
                let maybe_previous_block_header = db
                    .as_ref()
                    .ok_or(BlockPoolError::DatabaseError(eyre::eyre!(
                        "Database is not connected"
                    )))?
                    .view_eyre(|tx| block_header_by_hash(tx, &prev_block_hash, false))
                    .map_err(|db_error| BlockPoolError::DatabaseError(db_error))?;

                // If the parent block is in the db, process it
                if let Some(previous_block_header) = maybe_previous_block_header {
                    warn!(
                        "Found parent block for block {}",
                        current_block_hash.0.to_base58()
                    );

                    // process vdf steps from block
                    fast_forward_vdf_steps_from_block(vdf_limiter_info, vdf_sender).await;
                    warn!(
                        "FF VDF Steps for block for block {}",
                        current_block_hash.0.to_base58()
                    );

                    block_producer_addr
                        .as_ref()
                        .ok_or_else(|| {
                            let error_message =
                                "Block producer address is not connected".to_string();
                            error!(error_message);
                            BlockPoolError::OtherInternal(error_message)
                        })?
                        .send(BlockDiscoveredMessage(Arc::new(block_header.clone())))
                        .await
                        .map_err(|mailbox_error| {
                            let error_message =
                                format!("Can't send block to block producer: {:?}", mailbox_error);
                            error!(error_message);
                            BlockPoolError::OtherInternal(error_message)
                        })?
                        .map_err(|block_error| {
                            error!("{:?}", block_error);
                            BlockPoolError::BlockError(block_error)
                        })?;

                    info!("Block {} processed", current_block_hash.0.to_base58());
                    self_addr.do_send(RemoveBlockFromPool {
                        parent_block_hash: previous_block_header.block_hash,
                        block_hash: block_header.block_hash,
                    });

                    // Check if the currently processed block has any ancestors in the orphaned blocks pool
                    self_addr
                        .send(ProcessOrphanedAncestor {
                            block_hash: block_header.block_hash,
                        })
                        .await
                        .map_err(|mailbox_error| {
                            error!(
                                "Can't send ProcessOrphanedAncestor to block pool: {:?}",
                                mailbox_error
                            );
                            BlockPoolError::OtherInternal(format!(
                                "Can't send block to block pool: {:?}",
                                mailbox_error
                            ))
                        })??;

                    return Ok(());
                }

                debug!(
                    "Parent block for block {} not found in db",
                    current_block_hash.0.to_base58()
                );

                self_addr
                    .send(AddBlockToPoolAndTryToFetchParent {
                        header: block_header,
                    })
                    .await
                    .map_err(|mailbox_error| {
                        BlockPoolError::OtherInternal(format!(
                            "Can't send block to block pool: {:?}",
                            mailbox_error
                        ))
                    })?
            }
            .into_actor(self),
        )
    }
}

/// Adds a block to the block pool for processing.
#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<(), BlockPoolError>")]
pub struct ProcessBlock {
    pub header: IrysBlockHeader,
}

impl<A, R, B> Handler<ProcessBlock> for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    type Result = ResponseActFuture<Self, Result<(), BlockPoolError>>;

    fn handle(&mut self, msg: ProcessBlock, ctx: &mut Self::Context) -> Self::Result {
        self.process_block(msg.header, ctx)
    }
}

#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<(), BlockPoolError>")]
pub struct AddBlockToPoolAndTryToFetchParent {
    pub header: IrysBlockHeader,
}

impl<A, R, B> Handler<AddBlockToPoolAndTryToFetchParent> for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    type Result = ResponseActFuture<Self, Result<(), BlockPoolError>>;

    fn handle(
        &mut self,
        msg: AddBlockToPoolAndTryToFetchParent,
        ctx: &mut Self::Context,
    ) -> Self::Result {
        let block_header = msg.header;
        let self_addr = ctx.address();
        let current_block_hash = block_header.block_hash;
        let previous_block_hash = block_header.previous_block_hash;
        let parent_is_also_in_cache = self
            .orphaned_blocks_by_parent
            .contains_key(&previous_block_hash);

        let already_in_cache = self
            .orphaned_blocks_by_parent
            .contains_key(&block_header.previous_block_hash);

        if !already_in_cache {
            self.orphaned_blocks_by_parent
                .insert(previous_block_hash, block_header);
            self.block_hash_to_parent_hash
                .insert(current_block_hash, previous_block_hash);
        }

        Box::pin(
            async move {
                if !already_in_cache {
                    // If the parent is also in the cache it's likely that processing has already started
                    if !parent_is_also_in_cache {
                        self_addr
                            .send(RequestBlockFromTheNetwork {
                                block_hash: previous_block_hash,
                            })
                            .await
                            .map_err(|mailbox| {
                                BlockPoolError::OtherInternal(format!(
                                    "Can't request the block from the network: {:?}",
                                    mailbox
                                ))
                            })?
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                }
            }
            .into_actor(self),
        )
    }
}

/// Adds a block to the block pool for processing.
#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
struct RemoveBlockFromPool {
    pub parent_block_hash: BlockHash,
    pub block_hash: BlockHash,
}

impl<A, R, B> Handler<RemoveBlockFromPool> for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    type Result = ();

    fn handle(&mut self, msg: RemoveBlockFromPool, _ctx: &mut Self::Context) -> () {
        self.orphaned_blocks_by_parent
            .remove(&msg.parent_block_hash);
        self.block_hash_to_parent_hash.remove(&msg.block_hash);
    }
}

/// Adds a block to the block pool for processing.
#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<(), BlockPoolError>")]
struct RequestBlockFromTheNetwork {
    pub block_hash: BlockHash,
}

impl<A, R, B> Handler<RequestBlockFromTheNetwork> for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    type Result = ResponseActFuture<Self, Result<(), BlockPoolError>>;

    fn handle(
        &mut self,
        msg: RequestBlockFromTheNetwork,
        _ctx: &mut Self::Context,
    ) -> Self::Result {
        let block_hash = msg.block_hash;
        let peer_list_addr = self.peer_list.clone();
        let gossip_client = self.gossip_client.clone();

        let fut = async move {
            // Handle case where peer list address is not set
            let peer_list_addr = peer_list_addr.ok_or(BlockPoolError::OtherInternal(
                "Peer list address not set".to_string(),
            ))?;

            // Get top 5 active peers
            let peers = peer_list_addr.top_active_peers(Some(5), None).await?;

            if peers.is_empty() {
                return Err(BlockPoolError::OtherInternal(
                    "No active peers available to fetch block".to_string(),
                ));
            }

            // Try up to 5 peers to get the block
            let mut last_error = None;

            for (address, peer_item) in peers {
                for attempt in 1..=5 {
                    debug!(
                        "Attempting to fetch block {} from peer {} (attempt {}/5)",
                        block_hash.0.to_base58(),
                        address,
                        attempt
                    );

                    match gossip_client
                        .get_data_request(&peer_item, GossipDataRequest::Block(block_hash))
                        .await
                    {
                        Ok(true) => {
                            tracing::info!(
                                "Successfully requested block {} from peer {}",
                                block_hash.0.to_base58(),
                                address
                            );

                            return Ok(());
                        }
                        Ok(false) => {
                            // Peer doesn't have this block, try another peer
                            debug!(
                                "Peer {} doesn't have block {}",
                                address,
                                block_hash.0.to_base58()
                            );
                            continue;
                        }
                        Err(err) => {
                            last_error = Some(err);
                            tracing::warn!(
                                "Failed to fetch block {} from peer {} (attempt {}/5): {}",
                                block_hash.0.to_base58(),
                                address,
                                attempt,
                                last_error.as_ref().unwrap()
                            );

                            // Short delay before retrying
                            tokio::time::sleep(std::time::Duration::from_millis(100 * attempt))
                                .await;

                            // Continue trying with the same peer if not the last attempt
                            if attempt < 5 {
                                continue;
                            }
                        }
                    }
                }
            }

            Err(BlockPoolError::OtherInternal(format!(
                "Failed to fetch block {} after trying 5 peers: {:?}",
                block_hash.0.to_base58(),
                last_error
            )))
        };

        Box::pin(fut.into_actor(self))
    }
}

/// Get block by its hash
#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<Option<IrysBlockHeader>, BlockPoolError>")]
pub struct GetBlockByHash {
    pub block_hash: BlockHash,
}

impl<A, R, B> Handler<GetBlockByHash> for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    type Result = Result<Option<IrysBlockHeader>, BlockPoolError>;

    fn handle(&mut self, msg: GetBlockByHash, _ctx: &mut Self::Context) -> Self::Result {
        let block_hash = msg.block_hash;

        if let Some(parent_hash) = self.block_hash_to_parent_hash.get(&block_hash) {
            if let Some(header) = self.orphaned_blocks_by_parent.get(parent_hash) {
                return Ok(Some(header.clone()));
            }
        }

        self.db
            .as_ref()
            .ok_or(BlockPoolError::DatabaseError(eyre::eyre!(
                "Database is not connected"
            )))?
            .view_eyre(|tx| block_header_by_hash(tx, &block_hash, true))
            .map_err(|db_error| BlockPoolError::DatabaseError(db_error))
    }
}

/// Adds a block to the block pool for processing.
#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<bool, BlockPoolError>")]
pub struct BlockExists {
    pub block_hash: BlockHash,
}

impl<A, R, B> Handler<BlockExists> for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    type Result = Result<bool, BlockPoolError>;

    fn handle(&mut self, msg: BlockExists, _ctx: &mut Self::Context) -> Self::Result {
        let block_hash = msg.block_hash;

        if let Some(parent_hash) = self.block_hash_to_parent_hash.get(&block_hash) {
            return Ok(self.orphaned_blocks_by_parent.contains_key(parent_hash));
        }

        Ok(self
            .db
            .as_ref()
            .ok_or(BlockPoolError::DatabaseError(eyre::eyre!(
                "Database is not connected"
            )))?
            .view_eyre(|tx| block_header_by_hash(tx, &block_hash, true))
            .map_err(|db_error| BlockPoolError::DatabaseError(db_error))?
            .is_some())
    }
}

#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<(), BlockPoolError>")]
struct ProcessOrphanedAncestor {
    pub block_hash: BlockHash,
}

impl<A, R, B> Handler<ProcessOrphanedAncestor> for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    type Result = ResponseActFuture<Self, Result<(), BlockPoolError>>;

    fn handle(&mut self, msg: ProcessOrphanedAncestor, ctx: &mut Self::Context) -> Self::Result {
        let address = ctx.address();
        let maybe_orphaned_block = self.orphaned_blocks_by_parent.get(&msg.block_hash).cloned();

        Box::pin(
            async move {
                if let Some(orphaned_block) = maybe_orphaned_block {
                    let block_hash_string = orphaned_block.block_hash.0.to_base58();
                    info!(
                        "Start processing orphaned ancestor block: {:?}",
                        block_hash_string
                    );

                    address
                        .send(ProcessBlock {
                            header: orphaned_block,
                        })
                        .await
                        .map_err(|mailbox_error| {
                            let message = format!(
                                "Can't send block {:?} to pool: {:?}",
                                block_hash_string, mailbox_error
                            );
                            error!(message);
                            BlockPoolError::OtherInternal(message)
                        })?
                        .map_err(|block_pool_error| {
                            error!(
                                "Error while processing block {:?}: {:?}",
                                block_hash_string, block_pool_error
                            );
                            block_pool_error
                        })
                } else {
                    info!(
                        "No orphaned ancestor block found for block: {:?}",
                        msg.block_hash
                    );
                    Ok(())
                }
            }
            .into_actor(self),
        )
    }
}
