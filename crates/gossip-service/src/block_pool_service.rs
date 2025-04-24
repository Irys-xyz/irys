use actix::{Actor, Addr, AsyncContext, Context, Handler, Message, Supervised, SystemService};
use irys_actors::block_discovery::{BlockDiscoveredMessage, BlockDiscoveryActor};
use irys_actors::peer_list_service::{PeerListService, PeerListServiceWithClient};
use irys_actors::reth_service::RethServiceActor;
use irys_api_client::{ApiClient, IrysApiClient};
use irys_database::block_header_by_hash;
use irys_database::reth_db::Database;
use irys_types::{BlockHash, DatabaseProvider, IrysBlockHeader, RethPeerInfo};
use std::collections::HashMap;
use std::sync::Arc;

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
    pub orphan_blocks_pool: HashMap<BlockHash, IrysBlockHeader>,

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
            orphan_blocks_pool: HashMap::new(),
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
            orphan_blocks_pool: HashMap::new(),
            peer_list_addr: Some(peer_list_addr),
            block_producer_addr: Some(block_producer_addr),
        }
    }

    fn handle_new_block(&self, block_header: &IrysBlockHeader) -> eyre::Result<bool> {
        let prev_block_hash = block_header.previous_block_hash;

        // Check if the previous block is in the db
        let maybe_previous_block_header = self
            .db
            .as_ref()
            .ok_or(eyre::eyre!("Database is not connected"))?
            .view_eyre(|tx| block_header_by_hash(tx, &prev_block_hash, false))?;

        if let Some(_previous_block_header) = maybe_previous_block_header {
            let _fut = self
                .block_producer_addr
                .as_ref()
                .ok_or(eyre::eyre!("Block producer is not connected"))?
                .send(BlockDiscoveredMessage(Arc::new(block_header.clone())));
            // Here we need to spawn a message, wait for returned result, if the result
            // is successful:
            // 1. remove the block from the orphan cache
            // 2. check if we have an orphan block that depends on the current one,
            //  send a message to re-process it.
        }

        // If it's not in db, check if it's in the pool already. If it's in the pool,
        // do nothing and wait till parent gets processed as well.

        // If not in pool and not in db, request it from a random peer

        Ok(false)
    }
}

/// Adds a block to the block pool for processing.
#[derive(Message, Debug, Clone)]
#[rtype(result = "eyre::Result<()>")]
pub struct AddBlock {
    pub header: IrysBlockHeader,
}

impl<A, R, B> Handler<AddBlock> for BlockPoolService<A, R, B>
where
    A: ApiClient + 'static + Unpin + Default,
    R: Handler<RethPeerInfo, Result = eyre::Result<()>> + Actor<Context = Context<R>>,
    B: Handler<BlockDiscoveredMessage> + Actor<Context = Context<B>>,
{
    type Result = eyre::Result<()>;

    fn handle(&mut self, msg: AddBlock, _ctx: &mut Self::Context) -> Self::Result {
        // TODO: implement
        match self.handle_new_block(&msg.header) {
            Ok(true) => (),
            Ok(false) => (),
            Err(_error) => {
                // TODO: handle error
            },
        };

        Ok(())
    }
}
