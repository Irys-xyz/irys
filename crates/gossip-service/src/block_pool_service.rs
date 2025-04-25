use actix::{
    Actor, Addr, AsyncContext, Context, Handler, Message, ResponseActFuture, Supervised,
    SystemService, WrapFuture,
};
use irys_actors::block_discovery::{BlockDiscoveredMessage, BlockDiscoveryActor};
use irys_actors::peer_list_service::{PeerListService, PeerListServiceWithClient};
use irys_actors::reth_service::RethServiceActor;
use irys_api_client::{ApiClient, IrysApiClient};
use irys_database::block_header_by_hash;
use irys_database::reth_db::Database;
use irys_types::{BlockHash, DatabaseProvider, IrysBlockHeader, RethPeerInfo};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::debug;

#[derive(Debug)]
pub enum BlockPoolError {
    DatabaseError(eyre::Error),
    OtherInternal(String),
    BlockError(eyre::Error),
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
    pub block_hashes: HashSet<BlockHash>,

    pub block_producer_addr: Option<Addr<B>>,
    pub peer_list_addr: Option<Addr<PeerListServiceWithClient<A, R>>>,
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
            block_hashes: HashSet::new(),
            block_producer_addr: None,
            peer_list_addr: None,
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

impl BlockPoolService<IrysApiClient, RethServiceActor, BlockDiscoveryActor> {
    pub fn new(
        db: DatabaseProvider,
        peer_list_addr: Addr<PeerListService>,
        block_discovery_addr: Addr<BlockDiscoveryActor>,
    ) -> Self {
        Self::new_with_client(
            db,
            IrysApiClient::default(),
            peer_list_addr,
            block_discovery_addr,
        )
    }
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
        peer_list_addr: Addr<PeerListServiceWithClient<A, R>>,
        block_producer_addr: Addr<B>,
    ) -> Self {
        Self {
            db: Some(db),
            irys_api_client,
            orphaned_blocks_by_parent: HashMap::new(),
            block_hashes: HashSet::new(),
            peer_list_addr: Some(peer_list_addr),
            block_producer_addr: Some(block_producer_addr),
        }
    }

    fn process_block(
        self: &mut Self,
        block_header: IrysBlockHeader,
        ctx: &mut <BlockPoolService<A, R, B> as Actor>::Context,
    ) -> ResponseActFuture<Self, Result<(), BlockPoolError>> {
        debug!("Processing block {:?}", block_header.block_hash);
        let prev_block_hash = block_header.previous_block_hash;
        let current_block_hash = block_header.block_hash;
        let self_addr = ctx.address();
        let block_producer_addr = self.block_producer_addr.clone();
        let db = self.db.clone();

        Box::pin(
            async move {
                debug!(
                    "Searching for parent block {:?} for block {:?} in the db",
                    prev_block_hash, current_block_hash
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
                    debug!("Found parent block for block {:?}", current_block_hash);
                    block_producer_addr
                        .as_ref()
                        .ok_or(BlockPoolError::OtherInternal(
                            "Block producer address is not connected".to_string(),
                        ))?
                        .send(BlockDiscoveredMessage(Arc::new(block_header.clone())))
                        .await
                        .map_err(|mailbox_error| {
                            BlockPoolError::OtherInternal(format!(
                                "Can't send block to block producer: {:?}",
                                mailbox_error
                            ))
                        })?
                        .map_err(|block_error| BlockPoolError::BlockError(block_error))?;

                    self_addr.do_send(RemoveBlockFromPool {
                        parent_block_hash: previous_block_header.block_hash,
                        block_hash: block_header.block_hash,
                    });

                    // Check if the currently processed block has any ancestors in the orphaned blocks pool
                    self_addr.do_send(ProcessOrphanedAncestor {
                        block_hash: block_header.block_hash,
                    });

                    return Ok(());
                }

                debug!(
                    "Parent block for block {:?} not found in db",
                    current_block_hash
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
            self.block_hashes.insert(current_block_hash);
        }

        Box::pin(
            async move {
                if !already_in_cache {
                    // If the parent is also in the cache it's likely that processing has already started
                    if !parent_is_also_in_cache {
                        self_addr
                            .send(FetchAndProcessBlock {
                                block_hash: previous_block_hash,
                            })
                            .await
                            .map_err(|mailbox| {
                                BlockPoolError::OtherInternal(format!(
                                    "Can't send block to block producer: {:?}",
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
        self.block_hashes.remove(&msg.block_hash);
    }
}

/// Adds a block to the block pool for processing.
#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<(), BlockPoolError>")]
struct FetchAndProcessBlock {
    pub block_hash: BlockHash,
}

impl<A, R, B> Handler<FetchAndProcessBlock> for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    type Result = ResponseActFuture<Self, Result<(), BlockPoolError>>;

    fn handle(&mut self, msg: FetchAndProcessBlock, ctx: &mut Self::Context) -> Self::Result {
        use irys_actors::peer_list_service::TopActivePeersRequest;
        use std::collections::HashSet;

        let block_hash = msg.block_hash;
        let peer_list_addr = self.peer_list_addr.clone();
        let api_client = self.irys_api_client.clone();
        let self_addr = ctx.address();

        let fut = async move {
            // Handle case where peer list address is not set
            let peer_list_addr = peer_list_addr.ok_or(BlockPoolError::OtherInternal(
                "Peer list address not set".to_string(),
            ))?;

            // Get top 5 active peers
            let peers = peer_list_addr
                .send(TopActivePeersRequest {
                    truncate: Some(5),
                    exclude_peers: HashSet::new(),
                })
                .await
                .map_err(|err| {
                    BlockPoolError::OtherInternal(format!("Failed to get active peers: {}", err))
                })?;

            if peers.is_empty() {
                return Err(BlockPoolError::OtherInternal(
                    "No active peers available to fetch block".to_string(),
                ));
            }

            // Try up to 5 peers to get the block
            let mut last_error = None;

            for peer in peers {
                for attempt in 1..=5 {
                    tracing::debug!(
                        "Attempting to fetch block {} from peer {} (attempt {}/5)",
                        block_hash,
                        peer.address.api,
                        attempt
                    );

                    match api_client
                        .get_block_by_hash(peer.address.api, block_hash)
                        .await
                    {
                        Ok(Some(block)) => {
                            tracing::info!(
                                "Successfully fetched block {} from peer {}",
                                block_hash,
                                peer.address.api
                            );

                            // Process the block through the BlockPoolService itself
                            self_addr
                                .send(ProcessBlock { header: block.irys })
                                .await
                                .map_err(|err| {
                                    BlockPoolError::OtherInternal(format!(
                                        "Failed to send block to block pool: {:?}",
                                        err
                                    ))
                                })??;

                            return Ok(());
                        }
                        Ok(None) => {
                            // Peer doesn't have this block, try another peer
                            tracing::debug!(
                                "Peer {} doesn't have block {}",
                                peer.address.api,
                                block_hash
                            );
                            break;
                        }
                        Err(err) => {
                            last_error = Some(err);
                            tracing::warn!(
                                "Failed to fetch block {} from peer {} (attempt {}/5): {}",
                                block_hash,
                                peer.address.api,
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
                block_hash, last_error
            )))
        };

        Box::pin(fut.into_actor(self))
    }
}

#[derive(Message, Debug, Clone)]
#[rtype(result = "()")]
struct ProcessOrphanedAncestor {
    pub block_hash: BlockHash,
}

impl<A, R, B> Handler<ProcessOrphanedAncestor> for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    type Result = ();

    fn handle(&mut self, msg: ProcessOrphanedAncestor, ctx: &mut Self::Context) -> Self::Result {
        let address = ctx.address();

        let maybe_orphaned_block = self.orphaned_blocks_by_parent.get(&msg.block_hash).cloned();
        if let Some(orphaned_block) = maybe_orphaned_block {
            address.do_send(ProcessBlock {
                header: orphaned_block,
            });
        }
    }
}
