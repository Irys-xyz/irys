use alloy_rpc_types::BlockId;
use alloy_signer_local::LocalWallet;
use foldhash::fast::RandomState;
use irys_primitives::{Address, Genesis, ShadowReceipt};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use jsonrpsee_core::async_trait;
use jsonrpsee_types::ErrorObjectOwned;

use reth::network::{NetworkHandle, PeersHandleProvider};

use reth::payload::database::CachedReads;
use reth::primitives::Account;
use reth::revm::database::StateProviderDatabase;
use reth::revm::state_change::simulate_apply_shadow_thin;
use reth::transaction_pool::{PeerId, TransactionPool};

use alloy_eips::eip2718::Encodable2718;

use reth_db::DatabaseEnv;
use reth_e2e_test_utils::transaction::{tx, TransactionTestContext};
use reth_node_builder::NodeTypesWithDBAdapter;
use reth_node_core::irys_ext::IrysExt;

use reth_node_ethereum::EthereumNode;
use reth_provider::providers::BlockchainProvider2;

use reth_provider::{BlockIdReader, StateProviderFactory};
use revm::db::State;
use revm::JournaledState;

use irys_primitives::shadow::ShadowTx;
use revm_primitives::hex::ToHexExt;
use revm_primitives::{Bytes, HashSet, SpecId, B256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use strum::IntoEnumIterator;

use tracing::debug;

use crate::genesis::GenesisInfo;

// // We use jemalloc for performance reasons.
// #[cfg(all(feature = "jemalloc", unix))]
// #[global_allocator]
// static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

// #[rpc(server, client, namespace = "irys")]
// #[cfg_attr(not(test), rpc(server, namespace = "irys"))]
// #[cfg_attr(test, rpc(server, client, namespace = "irys"))]
#[rpc(server, client, namespace = "irys")]
pub trait AccountStateExtApi {
    // #[method(name = "updateBasicAccount")]
    // fn update_basic_account(&self, address: Address, new_balance: U256) -> RpcResult<bool>;
    #[method(name = "getAccount")]
    fn get_account(
        &self,
        address: Address,
        block_hash: Option<BlockId>,
    ) -> RpcResult<Option<Account>>;

    #[method(name = "getAccounts")]
    fn get_accounts(
        &self,
        addresses: Vec<Address>,
        block_id: Option<BlockId>,
    ) -> RpcResult<HashMap<Address, Option<Account>>>;

    // #[method(name = "getAccount2")]
    // fn get_account2(
    //     &self,
    //     address: Address,
    //     block_hash: Option<BlockId>,
    // ) -> RpcResult<Option<Account>>;

    #[method(name = "ping")]
    fn ping(&self) -> RpcResult<String>;

    #[method(name = "genesisBlock")]
    fn add_genesis_block(
        &self,
        // accounts: Vec<(Address, GenesisAccount)>,
        // shadows: Option<Shadows>,
        info: GenesisInfo,
    ) -> RpcResult<Genesis>;

    #[method(name = "testApplyShadow")]
    fn test_apply_shadow(&self, parent: BlockId, shadow: ShadowTx) -> RpcResult<ShadowReceipt>;

    #[method(name = "addPeer")]
    fn add_peer(&self, peer_id: PeerId, addr: String) -> RpcResult<()>;

    #[method(name = "removePeer")]
    fn remove_peer(&self, peer_id: PeerId) -> RpcResult<()>;

    #[method(name = "getPeerId")]
    fn get_peer_id(&self) -> RpcResult<PeerId>;

    #[method(name = "createEthTx")]
    async fn create_eth_tx(&self, private_key: B256) -> RpcResult<Bytes>;

    #[method(name = "toEthAddress")]
    fn to_address(&self, private_key: B256) -> RpcResult<Address>;

    // #[method(name = "getAccounts2")]
    // fn get_accounts2(
    //     &self,
    //     addresses: Vec<Address>,
    //     block_id: Option<BlockId>,
    // ) -> RpcResult<HashMap<Address, Option<Account>>>;
}

pub struct AccountStateExt {
    pub provider: BlockchainProvider2<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
    pub irys_ext: Option<IrysExt>,
    pub network: NetworkHandle,
}

pub enum Either<L, R> {
    Left(L),
    Right(R),
}

#[async_trait]
// impl<DB> AccountStateExtApiServer for AccountStateExt<DB> where DB: NodeTypesWithDB + StateProviderFactory  {
impl AccountStateExtApiServer for AccountStateExt {
    fn add_peer(&self, peer_id: PeerId, addr: String) -> RpcResult<()> {
        dbg!(&addr);
        let socket_addr = SocketAddr::from_str(addr.as_str()).map_err(|e| {
            ErrorObjectOwned::owned::<String>(
                -32073, // TOOD @JesseTheRobot add a RPC error registry so we don't overlap with error codes/numbers
                "unable to convert address into SocketAddr",
                Some(e.to_string()),
            )
        })?;
        // TODO @JesseTheRobot - these don't confirm if the operation was successful
        self.network.peers_handle().add_peer(peer_id, socket_addr);
        Ok(())
    }

    fn remove_peer(&self, peer_id: PeerId) -> RpcResult<()> {
        self.network.peers_handle().remove_peer(peer_id);
        Ok(())
    }

    fn get_peer_id(&self) -> RpcResult<PeerId> {
        Ok(self.network.peer_id().clone())
    }

    fn get_account(
        &self,
        address: Address,
        block_id: Option<BlockId>,
    ) -> RpcResult<Option<Account>> {
        // get provider for latest state
        let state = match block_id {
            Some(block_id) => match block_id {
                BlockId::Number(n) => self.provider.state_by_block_number_or_tag(n),
                BlockId::Hash(h) => self.provider.state_by_block_hash(h.into()),
            },
            None => self.provider.latest(),
        }
        .map_err(|e| {
            ErrorObjectOwned::owned::<String>(
                -32071,
                "error getting state provider",
                Some(e.to_string()),
            )
        })?;
        // TODO: replace with proper errors/error codes
        let r2 = state.basic_account(address).map_err(|e| {
            ErrorObjectOwned::owned::<String>(
                -32072,
                "error getting account info",
                Some(e.to_string()),
            )
        });

        return r2;
    }

    // fn get_account2(
    //     &self,
    //     address: Address,
    //     block_id: Option<BlockId>,
    // ) -> RpcResult<Option<Account>> {
    //     // get provider for latest state
    //     self.provider.pending_state_by_hash(block_hash)
    //     let state_provider: Either<Box<dyn StateProvider>,Box<dyn FullExecutionDataProvider>> =
    //         match block_id {
    //             Some(block_id) => match block_id {
    //                 BlockId::Number(n) => self
    //                     .provider
    //                     .state_by_block_number_or_tag(n)
    //                     .map(|p| Either::Left(p)),
    //                 BlockId::Hash(h) => match self.provider.history_by_block_hash(h.into()) {
    //                     Ok(s) => Ok(Either::Left(s)),
    //                     Err(_) => self
    //                         .provider
    //                         .tree
    //                         .pending_state_provider(h.into())
    //                         .map(|p| Either::Right(p)),
    //                 },
    //             },
    //             None => self.provider.latest().map(|p| Either::Left(p)),
    //         }
    //         .map_err(|e: ProviderError| {
    //             ErrorObjectOwned::owned::<String>(
    //                 -32071,
    //                 "error getting state provider",
    //                 Some(e.to_string()),
    //             )
    //         })?;

    //     let acc = match state_provider {
    //         Either::Left(ref provider) => provider.basic_account(address.clone()).map_err(|e| {
    //             ErrorObjectOwned::owned::<String>(
    //                 -32072,
    //                 "error getting account info",
    //                 Some(e.to_string()),
    //             )
    //         })?,
    //         Either::Right(provider) => provider.execution_outcome().state().account(&address).map(|a| a.account_info().map(|a2| a2.into())).flatten(),
    //     };

    //     return Ok(acc);
    // }

    fn get_accounts(
        &self,
        addresses: Vec<Address>,
        block_id: Option<BlockId>,
    ) -> RpcResult<HashMap<Address, Option<Account>>> {
        // get provider for latest state
        let state = match block_id {
            Some(block_id) => match block_id {
                BlockId::Number(n) => self.provider.state_by_block_number_or_tag(n),
                BlockId::Hash(h) => self.provider.state_by_block_hash(h.into()),
            },
            None => self.provider.latest(),
        }
        .map_err(|e| {
            ErrorObjectOwned::owned::<String>(
                -32071,
                "error getting state provider",
                Some(e.to_string()),
            )
        })?;
        let mut hm = HashMap::new();
        for address in addresses.iter() {
            hm.insert(
                address.clone(),
                state.basic_account(address.clone()).map_err(|e| {
                    ErrorObjectOwned::owned::<String>(
                        -32072,
                        "error getting account info",
                        Some(e.to_string()),
                    )
                })?,
            );
        }

        Ok(hm)
    }

    // fn get_accounts2(
    //     &self,
    //     addresses: Vec<Address>,
    //     block_id: Option<BlockId>,
    // ) -> RpcResult<HashMap<Address, Option<Account>>> {
    // get state provider
    //     let state_provider: Either<Box<dyn StateProvider>, Box<dyn FullExecutionDataProvider>> =
    //         match block_id {
    //             Some(block_id) => match block_id {
    //                 BlockId::Number(n) => self
    //                     .provider
    //                     .state_by_block_number_or_tag(n)
    //                     .map(|p| Either::Left(p)),
    //                 BlockId::Hash(h) => match self.provider.history_by_block_hash(h.into()) {
    //                     Ok(s) => Ok(Either::Left(s)),
    //                     Err(_) => self
    //                         .provider
    //                         .tree
    //                         .pending_state_provider(h.into())
    //                         .map(|p| Either::Right(p)),
    //                 },
    //             },
    //             None => self.provider.latest().map(|p| Either::Left(p)),
    //         }
    //         .map_err(|e: ProviderError| {
    //             ErrorObjectOwned::owned::<String>(
    //                 -32071,
    //                 "error getting state provider",
    //                 Some(e.to_string()),
    //             )
    //         })?;

    //     let mut hm = HashMap::new();
    //     for address in addresses.iter() {
    //         hm.insert(
    //             address.clone(),
    //             match state_provider {
    //                 Either::Left(ref provider) => {
    //                     provider.basic_account(address.clone()).map_err(|e| {
    //                         ErrorObjectOwned::owned::<String>(
    //                             -32072,
    //                             "error getting account info",
    //                             Some(e.to_string()),
    //                         )
    //                     })?
    //                 }
    //                 Either::Right(ref provider) => {
    //                     provider.execution_outcome().state().account(&address).map(|a| a.account_info().map(|a2| a2.into())).flatten()
    //                     // provider.state().account(address).flatten()
    //                      /* .ok_or_else(|| {
    //                                                                     ErrorObjectOwned::owned::<String>(
    //                                                                         -32072,
    //                                                                         "error getting account info from pending state provider",
    //                                                                         Some(address.to_string()),
    //                                                                     )
    //                                                                 })? */
    //                 }
    //             },
    //         );
    //     }

    //     Ok(hm)
    // }

    fn ping(&self) -> RpcResult<String> {
        Ok("pong".to_string())
    }

    fn add_genesis_block(&self, info: GenesisInfo) -> RpcResult<Genesis> {
        crate::genesis::add_genesis_block(&self.provider, &self.irys_ext.clone().unwrap(), info)
    }

    fn test_apply_shadow(&self, parent: BlockId, shadow: ShadowTx) -> RpcResult<ShadowReceipt> {
        let block_hash = &self
            .provider
            .block_hash_for_id(parent)
            .map_err(|e| {
                ErrorObjectOwned::owned::<String>(
                    -32090,
                    "error acquiring database state for block",
                    Some(e.to_string()),
                )
            })?
            .ok_or::<ErrorObjectOwned>(ErrorObjectOwned::owned::<String>(
                -32091,
                "Invalid block ID",
                None,
            ))?;

        let state_provider = &self
            .provider
            .state_by_block_hash(*block_hash)
            .map_err(|e| {
                ErrorObjectOwned::owned::<String>(
                    -32090,
                    "error acquiring database state for block",
                    Some(e.to_string()),
                )
            })?;
        let state = StateProviderDatabase::new(state_provider);
        let mut cached_reads = CachedReads::default();
        let mut db = State::builder()
            .with_database_ref(cached_reads.as_db(state))
            .with_bundle_update()
            .build();
        // TODO @JesseTheRobot - fix this (it seems like it's a dep & feature re-export issue)
        let mut journaled_state = JournaledState::new(
            SpecId::LATEST,
            HashSet::<Address, RandomState>::with_hasher(RandomState::default()),
        );
        // let res = apply_shadow(shadow, &mut journaled_state, &mut db);
        let res = simulate_apply_shadow_thin(shadow, &mut journaled_state, &mut db);
        return Ok(res.map_err(|e| {
            ErrorObjectOwned::owned::<String>(-32091, "error executing shadow", Some(e.to_string()))
        })?);
        // OLD - used to build a full EVM env (lol) to execute the shadow //

        // let block_hash = parent
        //     .as_block_hash()
        //     .expect("unable to get blockId as hash");
        // let state_provider = &self.provider.state_by_block_hash(block_hash)?;
        // let state = StateProviderDatabase::new(state_provider);
        // let mut cached_reads = CachedReads::default();
        // let mut block_env = BlockEnv::default();
        // let mut cfg = CfgEnvWithHandlerCfg::default()
        //     & self
        //         .provider
        //         .fill_block_env_at(&mut block_env, BlockHashOrNumber::Hash(block_hash));
        // // &self
        // //     .provider
        // //     .fill_cfg_env_at(cfg, BlockHashOrNumber::Hash(block_hash), evm_config);

        // let hash = self
        //     .convert_number(at)?
        //     .ok_or(ProviderError::HeaderNotFound(at))?;
        // let header = self
        //     .header(&hash)?
        //     .ok_or(ProviderError::HeaderNotFound(at))?;
        // let total_difficulty = &self
        //     .provider
        //     .header_td_by_number(header.number)?
        //     .ok_or_else(|| ProviderError::HeaderNotFound(header.number.into()))?;
        // EvmConfig::fill_cfg_env(cfg, &self.chain_spec, header, total_difficulty);

        // let mut db = State::builder()
        //     .with_database_ref(cached_reads.as_db(state))
        //     .with_bundle_update()
        //     .build();
        // let mut evm = revm::Evm::builder()
        //     .with_db(&mut db)
        //     .with_env_with_handler_cfg(EnvWithHandlerCfg::new_with_cfg_env(
        //         initialized_cfg.clone(),
        //         block_env.clone(),
        //         // tx_env_with_recovered(&tx),
        //         Default::default(),
        //     ))
        //     .build();
    }

    async fn create_eth_tx(&self, private_key: B256) -> RpcResult<Bytes> {
        // create a new tx
        // let wallet = LocalWallet::from(private_key);
        let wallet = LocalWallet::from_bytes(&private_key).map_err(|e| {
            ErrorObjectOwned::owned::<String>(-32091, "error executing shadow", Some(e.to_string()))
        })?;
        debug!("Creating tx with owner: {}", wallet.address());

        let account = self
            .get_account(wallet.address(), Some(BlockId::latest()))?
            .ok_or(ErrorObjectOwned::owned::<String>(
                -32091,
                "error executing shadow",
                None,
            ))?;
        let tx = tx(4096, 210000, None, None, account.nonce);

        let signed = TransactionTestContext::sign_tx(wallet, tx).await;
        let raw_tx: Bytes = signed.encoded_2718().into();
        return Ok(raw_tx);
    }

    fn to_address(&self, private_key: B256) -> RpcResult<Address> {
        dbg!(&private_key.encode_hex());
        let wallet = LocalWallet::from_bytes(&private_key).map_err(|e| {
            ErrorObjectOwned::owned::<String>(
                -32091,
                "error resolving address for wallet",
                Some(e.to_string()),
            )
        })?;
        return Ok(wallet.address());
    }
}

// #[derive(Debug, Clone, Copy, Default, clap::Args)]
// pub struct RethCliIrysExt {
//     #[arg(long, default_value_t = true)]
//     pub enable_irys_ext: bool,
// }

/// trait interface for a custom rpc namespace: `txpool`
///
/// This defines an additional namespace where all methods are configured as trait functions.
#[cfg_attr(not(test), rpc(server, namespace = "txpoolExt"))]
#[cfg_attr(test, rpc(server, client, namespace = "txpoolExt"))]
pub trait TxpoolExtApi {
    /// Returns the number of transactions in the pool.
    #[method(name = "transactionCount")]
    fn transaction_count(&self) -> RpcResult<usize>;
}

/// The type that implements the `txpool` rpc namespace trait
pub struct TxpoolExt<Pool> {
    pub pool: Pool,
}

impl<Pool> TxpoolExtApiServer for TxpoolExt<Pool>
where
    Pool: TransactionPool + Clone + 'static,
{
    fn transaction_count(&self) -> RpcResult<usize> {
        Ok(self.pool.pool_size().total)
    }
}
