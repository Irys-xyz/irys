use crate::peer_list::{PeerListFacade, PeerListFacadeError};
use crate::{fast_forward_vdf_steps_from_block, wait_for_vdf_step, SyncState};
use actix::{
    Actor, AsyncContext, Context, Handler, Message, ResponseActFuture, Supervised, SystemService,
    WrapFuture,
};
use base58::ToBase58;
use irys_actors::block_discovery::BlockDiscoveryFacade;
use irys_actors::broadcast_mining_service::BroadcastMiningSeed;
use irys_actors::vdf_service::VdfServiceMessage;
use irys_api_client::ApiClient;
use irys_database::block_header_by_hash;
use irys_database::reth_db::Database;
use irys_types::{BlockHash, DatabaseProvider, IrysBlockHeader, RethPeerInfo};
use std::collections::HashMap;
use tokio::sync::mpsc::{Sender, UnboundedSender};
use tracing::{debug, error, info};

#[derive(Debug, Clone)]
pub enum BlockPoolError {
    DatabaseError(String),
    OtherInternal(String),
    BlockError(String),
}

impl From<PeerListFacadeError> for BlockPoolError {
    fn from(err: PeerListFacadeError) -> Self {
        Self::OtherInternal(format!("Peer list error: {:?}", err))
    }
}

#[derive(Debug)]
pub(crate) struct BlockPoolService<A, R, B>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
{
    /// Database provider for accessing transaction headers and related data.
    pub(crate) db: Option<DatabaseProvider>,

    pub(crate) orphaned_blocks_by_parent: HashMap<BlockHash, IrysBlockHeader>,
    pub(crate) block_hash_to_parent_hash: HashMap<BlockHash, BlockHash>,

    pub(crate) block_producer: Option<B>,
    pub(crate) peer_list: Option<PeerListFacade<A, R>>,
    pub(crate) vdf_sender: Option<Sender<BroadcastMiningSeed>>,
    pub(crate) vdf_service_sender: Option<UnboundedSender<VdfServiceMessage>>,

    sync_state: SyncState,
}

impl<A, R, B> Default for BlockPoolService<A, R, B>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
{
    fn default() -> Self {
        Self {
            db: None,
            orphaned_blocks_by_parent: HashMap::new(),
            block_hash_to_parent_hash: HashMap::new(),
            block_producer: None,
            peer_list: None,
            vdf_sender: None,
            vdf_service_sender: None,
            sync_state: SyncState::default(),
        }
    }
}

impl<A, R, B> Actor for BlockPoolService<A, R, B>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
{
    type Context = actix::Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        let _block_service_addr = ctx.address();
    }
}

impl<A, R, B> Supervised for BlockPoolService<A, R, B>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
{
}

impl<A, R, B> SystemService for BlockPoolService<A, R, B>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
{
}

impl<A, R, B> BlockPoolService<A, R, B>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
{
    pub(crate) fn new_with_client(
        db: DatabaseProvider,
        peer_list: PeerListFacade<A, R>,
        block_producer_addr: B,
        vdf_sender: Option<Sender<BroadcastMiningSeed>>,
        sync_state: SyncState,
        vdf_service_sender: UnboundedSender<VdfServiceMessage>,
    ) -> Self {
        Self {
            db: Some(db),
            orphaned_blocks_by_parent: HashMap::new(),
            block_hash_to_parent_hash: HashMap::new(),
            peer_list: Some(peer_list),
            block_producer: Some(block_producer_addr),
            vdf_sender,
            sync_state,
            vdf_service_sender: Some(vdf_service_sender),
        }
    }

    fn process_block(
        self: &mut Self,
        block_header: IrysBlockHeader,
        ctx: &mut <BlockPoolService<A, R, B> as Actor>::Context,
    ) -> ResponseActFuture<Self, Result<(), BlockPoolError>> {
        debug!(
            "Block pool: Processing block {} (height {})",
            block_header.block_hash.0.to_base58(),
            block_header.height
        );
        let current_block_height = block_header.height;
        let prev_block_hash = block_header.previous_block_hash;
        let current_block_hash = block_header.block_hash;
        let vdf_limiter_info = block_header.vdf_limiter_info.clone();
        let self_addr = ctx.address();
        let block_discovery = self.block_producer.clone();
        let db = self.db.clone();
        let vdf_sender = self.vdf_sender.clone().expect("valid vdf sender");
        let vdf_service_sender = self
            .vdf_service_sender
            .clone()
            .expect("valid vdf service sender");

        // Adding the block to the pool, so if a block depending on that block arrives,
        // this block won't be requested from the network
        self.orphaned_blocks_by_parent
            .insert(prev_block_hash, block_header.clone());
        self.block_hash_to_parent_hash
            .insert(current_block_hash, prev_block_hash);

        let sync_state = self.sync_state.clone();

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
                    .ok_or(BlockPoolError::DatabaseError("Database is not connected".into()))?
                    .view_eyre(|tx| block_header_by_hash(tx, &prev_block_hash, false))
                    .map_err(|db_error| BlockPoolError::DatabaseError(format!("{:?}", db_error)))?;

                // If the parent block is in the db, process it
                if let Some(_previous_block_header) = maybe_previous_block_header {
                    info!(
                        "Found parent block for block {}",
                        current_block_hash.0.to_base58()
                    );

                    // process vdf steps from block
                    fast_forward_vdf_steps_from_block(vdf_limiter_info, vdf_sender).await;

                    info!(
                        "FF VDF Steps for block for block {} completed. Waiting for FF VDF Steps to be saved to VdfState",
                        current_block_hash.0.to_base58()
                    );

                    // wait to be sure the FF steps are saved to VdfState before we try to discover the block that requires them
                    let desired_step = block_header.vdf_limiter_info.global_step_number;
                    if let Err(vdf_error) = wait_for_vdf_step(vdf_service_sender, desired_step).await {
                        self_addr.do_send(RemoveBlockFromPool {
                            block_hash: block_header.block_hash,
                        });
                        return Err(BlockPoolError::OtherInternal(format!("Can't process VDF steps for block: {:?}", vdf_error)))
                    }

                    info!(
                        "VDF Steps for block {} saved. Starting block validation",
                        current_block_hash.0.to_base58()
                    );

                    if let Err(block_discovery_error) = block_discovery
                        .as_ref()
                        .ok_or_else(|| {
                            let error_message =
                                "Block producer address is not connected".to_string();
                            error!(error_message);
                            BlockPoolError::OtherInternal(error_message)
                        })?
                        .handle_block(block_header.clone())
                        .await
                    {
                            error!("Block pool: Block validation error for block {}: {:?}. Removing block from the pool", block_header.block_hash.0.to_base58(), block_discovery_error);
                            self_addr.do_send(RemoveBlockFromPool {
                                block_hash: block_header.block_hash,
                            });
                            return Err(BlockPoolError::BlockError(format!("{:?}", block_discovery_error)))
                    }

                    info!(
                        "Block pool: Block {} has been processed",
                        current_block_hash.0.to_base58()
                    );
                    sync_state.mark_processed(current_block_height as usize);
                    self_addr.do_send(RemoveBlockFromPool {
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
                    .send(TryToFetchParent {
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
pub(crate) struct ProcessBlock {
    pub header: IrysBlockHeader,
}

impl<A, R, B> Handler<ProcessBlock> for BlockPoolService<A, R, B>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
{
    type Result = ResponseActFuture<Self, Result<(), BlockPoolError>>;

    fn handle(&mut self, msg: ProcessBlock, ctx: &mut Self::Context) -> Self::Result {
        self.process_block(msg.header, ctx)
    }
}

#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<(), BlockPoolError>")]
struct TryToFetchParent {
    pub header: IrysBlockHeader,
}

impl<A, R, B> Handler<TryToFetchParent> for BlockPoolService<A, R, B>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
{
    type Result = ResponseActFuture<Self, Result<(), BlockPoolError>>;

    fn handle(&mut self, msg: TryToFetchParent, ctx: &mut Self::Context) -> Self::Result {
        let block_header = msg.header;
        let self_addr = ctx.address();
        let previous_block_hash = block_header.previous_block_hash;
        let parent_is_already_in_the_pool = self
            .block_hash_to_parent_hash
            .contains_key(&previous_block_hash);

        Box::pin(
            async move {
                // If the parent is also in the cache it's likely that processing has already started
                if !parent_is_already_in_the_pool {
                    debug!(
                        "Block pool: Parent block {} not found in the cache, requesting it from the network",
                        previous_block_hash.0.to_base58()
                    );
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
                    debug!(
                        "Parent block {} is already in the cache, skipping get data request",
                        previous_block_hash.0.to_base58()
                    );
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
    pub block_hash: BlockHash,
}

impl<A, R, B> Handler<RemoveBlockFromPool> for BlockPoolService<A, R, B>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
{
    type Result = ();

    fn handle(&mut self, msg: RemoveBlockFromPool, _ctx: &mut Self::Context) -> () {
        if let Some(parent_hash) = self.block_hash_to_parent_hash.remove(&msg.block_hash) {
            self.orphaned_blocks_by_parent.remove(&parent_hash);
        }
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
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
{
    type Result = ResponseActFuture<Self, Result<(), BlockPoolError>>;

    fn handle(&mut self, msg: RequestBlockFromTheNetwork, ctx: &mut Self::Context) -> Self::Result {
        let block_hash = msg.block_hash;
        let peer_list_addr = self.peer_list.clone();
        let self_addr = ctx.address();

        let fut = async move {
            // Handle case where peer list address is not set
            let peer_list_addr = peer_list_addr.ok_or(BlockPoolError::OtherInternal(
                "Peer list address not set".to_string(),
            ))?;

            match peer_list_addr
                .request_block_from_the_network(block_hash)
                .await
            {
                Ok(_) => {
                    debug!(
                        "Block pool: Requested block {} from the network",
                        block_hash.0.to_base58()
                    );
                    Ok(())
                }
                Err(error) => {
                    error!("Error while trying to fetch parent block {}: {:?}. Removing the block from the pool", block_hash.0.to_base58(), error);
                    if let Err(err) = self_addr.send(RemoveBlockFromPool { block_hash }).await {
                        error!(
                            "Error while trying to request the block from the network: {:?}",
                            err
                        );
                    }
                    Err(error.into())
                }
            }
        };

        Box::pin(fut.into_actor(self))
    }
}

/// Get block by its hash
#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<Option<IrysBlockHeader>, BlockPoolError>")]
pub(crate) struct GetBlockByHash {
    pub block_hash: BlockHash,
}

impl<A, R, B> Handler<GetBlockByHash> for BlockPoolService<A, R, B>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
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
            .ok_or(BlockPoolError::DatabaseError(
                "Database is not connected".into(),
            ))?
            .view_eyre(|tx| block_header_by_hash(tx, &block_hash, true))
            .map_err(|db_error| BlockPoolError::DatabaseError(format!("{:?}", db_error)))
    }
}

/// Adds a block to the block pool for processing.
#[derive(Message, Debug, Clone)]
#[rtype(result = "Result<bool, BlockPoolError>")]
pub(crate) struct BlockExists {
    pub block_hash: BlockHash,
}

impl<A, R, B> Handler<BlockExists> for BlockPoolService<A, R, B>
where
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
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
            .ok_or(BlockPoolError::DatabaseError(
                "Database is not connected".into(),
            ))?
            .view_eyre(|tx| block_header_by_hash(tx, &block_hash, true))
            .map_err(|db_error| BlockPoolError::DatabaseError(format!("{:?}", db_error)))?
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
    A: ApiClient,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: BlockDiscoveryFacade,
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
