use actix::{
    Actor, ActorTryFutureExt as _, AtomicResponse, Context, Handler, Message, Supervised,
    SystemService, WrapFuture as _,
};
use eyre::eyre;
use irys_database::{database, db::IrysDatabaseExt as _};
use irys_reth_node_bridge::IrysRethNodeAdapter;
use irys_types::{DatabaseProvider, RethPeerInfo, H256};
use reth::{
    network::{NetworkInfo as _, Peers as _},
    revm::primitives::B256,
    rpc::{eth::EthApiServer as _, types::BlockNumberOrTag},
};
use tracing::{debug, error, info};

#[derive(Debug)]
pub struct RethServiceActor {
    pub handle: IrysRethNodeAdapter,
    pub db: DatabaseProvider,
    // we store a copy of the latest FCU so we can always provide reth with a "full" FCU, as the finalized field is used to control the block persistence mechanism.
    pub latest_fcu: ForkChoiceUpdate,
}

impl Default for RethServiceActor {
    fn default() -> Self {
        panic!("Don't rely on the default constructor for RethServiceActor, use the `new` method instead");
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ForkChoiceUpdate {
    pub head_hash: B256,
    pub confirmed_hash: Option<B256>,
    pub finalized_hash: Option<B256>,
}

// todo: move the entire reth process in here
impl RethServiceActor {
    pub fn new(handle: IrysRethNodeAdapter, database_provider: DatabaseProvider) -> Self {
        Self {
            handle,
            db: database_provider,
            latest_fcu: ForkChoiceUpdate::default(),
        }
    }

    // computes a new FCU, resolving any Irys block hashes into EVM block hashes, as well as backfilling the safe (confirmed) and finalized fields.
    pub fn resolve_new_fcu(
        db: DatabaseProvider,
        new_fcu: ForkChoiceUpdateMessage,
        prev_fcu: ForkChoiceUpdate,
    ) -> eyre::Result<ForkChoiceUpdate> {
        let ForkChoiceUpdateMessage {
            head_hash,
            confirmed_hash,
            finalized_hash,
        } = new_fcu;

        let evm_head_hash = match head_hash {
            BlockHashType::Irys(irys_hash) => {
                let irys_header =
                    db.view_eyre(|tx| database::block_header_by_hash(tx, &irys_hash, false))?;
                let h = irys_header.ok_or(eyre!("Missing irys block {} in DB!", &irys_hash))?;

                debug!("EVM block {} is height {}", &h.evm_block_hash, &h.height);
                h.evm_block_hash
            }
            BlockHashType::Evm(v) => v,
        };

        let evm_confirmed_hash = match confirmed_hash {
            Some(confirmed_hash) => Some(match confirmed_hash {
                BlockHashType::Irys(irys_hash) => {
                    let irys_header =
                        db.view_eyre(|tx| database::block_header_by_hash(tx, &irys_hash, false))?;
                    let h = irys_header.ok_or(eyre!("Missing irys block {} in DB!", &irys_hash))?;

                    debug!("EVM block {} is height {}", &h.evm_block_hash, &h.height);
                    h.evm_block_hash
                }
                BlockHashType::Evm(v) => v,
            }),
            None => prev_fcu.confirmed_hash,
        };

        let evm_finalized_hash = match finalized_hash {
            Some(finalized_hash) => Some(match finalized_hash {
                BlockHashType::Irys(irys_hash) => {
                    let irys_header =
                        db.view_eyre(|tx| database::block_header_by_hash(tx, &irys_hash, false))?;
                    let h = irys_header.ok_or(eyre!("Missing irys block {} in DB!", &irys_hash))?;

                    debug!("EVM block {} is height {}", &h.evm_block_hash, &h.height);
                    h.evm_block_hash
                }
                BlockHashType::Evm(v) => v,
            }),
            None => prev_fcu.finalized_hash,
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

#[derive(Debug, Copy, Clone)]
pub enum BlockHashType {
    Irys(H256),
    Evm(B256),
}

#[derive(Message, Debug, Clone, Copy)]
#[rtype(result = "eyre::Result<ForkChoiceUpdate>")]
pub struct ForkChoiceUpdateMessage {
    pub head_hash: BlockHashType,
    pub confirmed_hash: Option<BlockHashType>,
    pub finalized_hash: Option<BlockHashType>,
}

impl Handler<ForkChoiceUpdateMessage> for RethServiceActor {
    type Result = AtomicResponse<Self, eyre::Result<ForkChoiceUpdate>>;

    fn handle(&mut self, msg: ForkChoiceUpdateMessage, _ctx: &mut Self::Context) -> Self::Result {
        let handle = self.handle.clone();
        let db = self.db.clone();
        let prev_fcu = self.latest_fcu;
        AtomicResponse::new(Box::pin(
            async move {
                let fcu = Self::resolve_new_fcu(db, msg, prev_fcu).inspect_err(|e| {
                    error!(error = ?e, ?msg, "Error updating reth with forkchoice");
                })?;

                let ForkChoiceUpdate {
                    head_hash,
                    confirmed_hash,
                    finalized_hash,
                } = fcu;

                info!(
                    "Updating reth forkchoice: head: {:?}, conf: {:?}, final: {:?}",
                    &head_hash, &confirmed_hash, &finalized_hash
                );

                handle
                    .update_forkchoice_full(head_hash, confirmed_hash, finalized_hash)
                    .await
                    .map_err(|e| {
                        eyre!("Error updating reth with forkchoice {:?} - {}", &msg, &e)
                    })?;

                let latest = handle
                    .inner
                    .eth_api()
                    .block_by_number(BlockNumberOrTag::Latest, false)
                    .await;

                let safe = handle
                    .inner
                    .eth_api()
                    .block_by_number(BlockNumberOrTag::Safe, false)
                    .await;

                let finalized = handle
                    .inner
                    .eth_api()
                    .block_by_number(BlockNumberOrTag::Finalized, false)
                    .await;

                debug!(
                    "JESSEDEBUG FCU S latest {:?}, safe {:?}, finalized {:?}",
                    &latest, &safe, &finalized
                );
                Ok(fcu)
            }
            .into_actor(self)
            .map_ok(|fcu, a, _| {
                a.latest_fcu = fcu; // update the latest FCU so we can provide reth a consistent FCU state
                fcu
            })
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

type ConnectToPeerMessage = RethPeerInfo;

impl Handler<ConnectToPeerMessage> for RethServiceActor {
    type Result = eyre::Result<()>;

    fn handle(&mut self, msg: ConnectToPeerMessage, _ctx: &mut Self::Context) -> Self::Result {
        debug!("Connecting to {:?}", &msg);
        self.handle
            .inner
            .network
            .add_peer(msg.peer_id, msg.peering_tcp_addr);
        Ok(())
    }
}

#[derive(Message, Debug, Clone, Copy)]
#[rtype(result = "eyre::Result<RethPeerInfo>")]
pub struct GetPeeringInfoMessage {}

impl Handler<GetPeeringInfoMessage> for RethServiceActor {
    type Result = eyre::Result<RethPeerInfo>;

    fn handle(&mut self, _: GetPeeringInfoMessage, _ctx: &mut Self::Context) -> Self::Result {
        let handle = self.handle.clone();
        // TODO: we need to store the external socketaddr somewhere and use that instead
        Ok(RethPeerInfo {
            peer_id: *handle.inner.network.peer_id(),
            peering_tcp_addr: handle.inner.network.local_addr(),
        })
    }
}
