use actix::{
    Actor, ActorTryFutureExt as _, AtomicResponse, Context, Handler, Message,
    Supervised, SystemService, WrapFuture,
};
use eyre::{eyre, OptionExt};
use futures::TryFutureExt;
use irys_database::database;
use irys_reth_node_bridge::{adapter::node::RethNodeContext, node::RethNodeProvider};
use irys_types::{DatabaseProvider, H256};
use reth::revm::primitives::B256;
use reth_db::Database as _;
use tracing::{error, info};

#[derive(Debug, Default)]
pub struct RethServiceActor {
    pub handle: Option<RethNodeProvider>,
    pub db: Option<DatabaseProvider>,
}

#[derive(Debug, Clone, Copy)]
pub struct ForkChoiceUpdate {
    pub head_hash: B256,
    pub confirmed_hash: Option<B256>,
    pub finalized_hash: Option<B256>,
}

// todo: move the entire reth process in here
impl RethServiceActor {
    pub fn new(handle: RethNodeProvider, database_provider: DatabaseProvider) -> Self {
        Self {
            handle: Some(handle),
            db: Some(database_provider),
        }
    }

    pub fn resolve_irys_block_hashes(
        db: DatabaseProvider,
        fcu: ForkChoiceUpdateMessage,
    ) -> eyre::Result<ForkChoiceUpdate> {
        let ForkChoiceUpdateMessage {
            head_hash,
            confirmed_hash,
            finalized_hash,
        } = fcu;

        let evm_head_hash = match head_hash {
            BlockHashType::Irys(irys_hash) => {
                let irys_header =
                    db.view_eyre(|tx| database::block_header_by_hash(tx, &irys_hash))?;
                let h = irys_header
                    .ok_or(eyre!("Missing irys block {} in DB!", &irys_hash))?
                    .evm_block_hash;
                h
            }
            BlockHashType::Evm(v) => v,
        };

        let evm_confirmed_hash = match confirmed_hash {
            Some(confirmed_hash) => Some(match confirmed_hash {
                BlockHashType::Irys(irys_hash) => {
                    let irys_header =
                        db.view_eyre(|tx| database::block_header_by_hash(tx, &irys_hash))?;
                    irys_header
                        .ok_or(eyre!("Missing irys block {} in DB!", &irys_hash))?
                        .evm_block_hash
                }
                BlockHashType::Evm(v) => v,
            }),
            None => None,
        };

        let evm_finalized_hash = match finalized_hash {
            Some(finalized_hash) => Some(match finalized_hash {
                BlockHashType::Irys(irys_hash) => {
                    let irys_header =
                        db.view_eyre(|tx| database::block_header_by_hash(tx, &irys_hash))?;
                    irys_header
                        .ok_or(eyre!("Missing irys block {} in DB!", &irys_hash))?
                        .evm_block_hash
                }
                BlockHashType::Evm(v) => v,
            }),
            None => None,
        };

        Ok(ForkChoiceUpdate {
            head_hash: evm_head_hash,
            confirmed_hash: evm_confirmed_hash,
            finalized_hash: evm_finalized_hash,
        })
    }
}

impl Actor for RethServiceActor {
    type Context = Context<Self>;
}

impl Supervised for RethServiceActor {}

impl SystemService for RethServiceActor {
    fn service_started(&mut self, _ctx: &mut Context<Self>) {
        println!("RethServiceActor started");
    }
}

#[derive(Message, Debug, Clone)]
#[rtype(result = "Option<RethNodeProvider>")]
pub struct GetRethProvider(pub Option<RethNodeProvider>);

impl Handler<GetRethProvider> for RethServiceActor {
    type Result = Option<RethNodeProvider>;

    fn handle(&mut self, _msg: GetRethProvider, _ctx: &mut Self::Context) -> Self::Result {
        self.handle.clone()
    }
}

#[derive(Debug, Copy, Clone)]
pub enum BlockHashType {
    Irys(H256),
    Evm(B256),
}

#[derive(Message, Debug, Clone, Copy)]
#[rtype(result = "eyre::Result<()>")]
pub struct ForkChoiceUpdateMessage {
    pub head_hash: BlockHashType,
    pub confirmed_hash: Option<BlockHashType>,
    pub finalized_hash: Option<BlockHashType>,
}

impl Handler<ForkChoiceUpdateMessage> for RethServiceActor {
    type Result = AtomicResponse<Self, eyre::Result<()>>;

    fn handle(&mut self, msg: ForkChoiceUpdateMessage, _ctx: &mut Self::Context) -> Self::Result {
        let handle = self.handle.clone();
        let db = self.db.clone();
        AtomicResponse::new(Box::pin(
            async move {
                let handle = handle.ok_or_eyre("Reth service is uninitialized!")?;
                let db = db.ok_or_eyre("Reth service is uninitialized!")?;

                let ForkChoiceUpdate {
                    head_hash,
                    confirmed_hash,
                    finalized_hash,
                } = RethServiceActor::resolve_irys_block_hashes(db, msg).map_err(|e| {
                    error!("Error updating reth with forkchoice {:?} - {}", &msg, &e);
                    e
                })?;

                info!(
                    "Updating reth forkchoice: head: {:?}, conf: {:?}, final: {:?}",
                    &head_hash, &confirmed_hash, &finalized_hash
                );

                let context = RethNodeContext::new(handle.into())
                    .await
                    .map_err(|e| eyre!("Error connecting to Reth: {}", e))?;

                context
                    .engine_api
                    .update_forkchoice_full(head_hash, confirmed_hash, finalized_hash)
                    .await
                    .map_err(|e| {
                        eyre!("Error updating reth with forkchoice {:?} - {}", &msg, &e)
                    })?;
                Ok(())
            }
            .into_actor(self)
            .map_err(|e: eyre::Error, _, _| {
                error!(
                    "Error processing RethServiceActor ForkChoiceUpdateMessage: {}",
                    &e
                );
                std::process::abort();
            }),
        ))
    }
}
