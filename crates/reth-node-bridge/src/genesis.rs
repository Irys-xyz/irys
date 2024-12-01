use foldhash::fast::RandomState;
use irys_primitives::shadow::Shadows;
use irys_primitives::{Address, Genesis, GenesisAccount};
use jsonrpsee::core::RpcResult;
use jsonrpsee_types::ErrorObjectOwned;
use reth::primitives::StaticFileSegment;
use reth::revm::database::StateProviderDatabase;
use reth::revm::state_change::apply_shadow;
use reth_db::database::Database;
use reth_db::static_file::iter_static_files;
use reth_db::table::Table;
use reth_db::transaction::{DbTx, DbTxMut};
use reth_db::{DatabaseEnv, TableViewer, Tables};
use reth_node_builder::NodeTypesWithDBAdapter;
use reth_node_core::irys_ext::{IrysExt, ReloadPayload};
use reth_node_ethereum::EthereumNode;
use reth_provider::providers::BlockchainProvider2;
use reth_provider::{ChainSpecProvider, StateProviderFactory, StaticFileProviderFactory};
use revm::db::CacheDB;
use revm::{DatabaseCommit, JournaledState};
use revm_primitives::ruint::Uint;
use revm_primitives::{AccountInfo, Bytecode, FixedBytes, HashSet, SpecId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use strum::IntoEnumIterator;
use tracing::{debug, error, warn};

pub struct ClearViewer<'a, DB: Database> {
    db: &'a DB,
}

impl<DB: Database> TableViewer<()> for ClearViewer<'_, DB> {
    type Error = eyre::Report;

    fn view<T: Table>(&self) -> Result<(), Self::Error> {
        let tx = self.db.tx_mut()?;
        tx.clear::<T>()?;
        tx.commit()?;
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GenesisInfo {
    pub accounts: Vec<(Address, GenesisAccount)>,
    pub shadows: Option<Shadows>,
    pub timestamp: u64,
}

pub fn add_genesis_block(
    provider: &BlockchainProvider2<NodeTypesWithDBAdapter<EthereumNode, Arc<DatabaseEnv>>>,
    ext: &IrysExt,
    info: GenesisInfo,
) -> RpcResult<Genesis> {
    warn!("genesis reloading via reth RPC is deprecated");
    let GenesisInfo {
        accounts,
        shadows,
        timestamp,
    } = info;

    // clean database & static files
    // TODO: gate this fn so that it only works/shows up in dev mode
    let static_file_provider = provider.static_file_provider();
    for segment in StaticFileSegment::iter() {
        let static_files = iter_static_files(static_file_provider.directory()).map_err(|e| {
            ErrorObjectOwned::owned::<String>(
                -32073,
                "error getting database provider",
                Some(e.to_string()),
            )
        })?;

        if let Some(segment_static_files) = static_files.get(&segment) {
            for (block_range, _) in segment_static_files {
                static_file_provider
                    .delete_jar(segment, block_range.start())
                    .map_err(|e| {
                        //         .map_err(|e| {
                        ErrorObjectOwned::owned::<String>(
                            -32073,
                            "error getting database provider",
                            Some(e.to_string()),
                        )
                    })?;
            }
        }
    }

    for table in Tables::ALL {
        table
            .view(&ClearViewer {
                db: provider.database.db_ref(),
            })
            .map_err(|e| {
                ErrorObjectOwned::owned::<String>(
                    -32073,
                    "error getting database provider",
                    Some(e.to_string()),
                )
            })?;
    }

    // write the new genesis json, boot from it.
    // code taken from reth/crates/storage/db-common/src/init.rs:init_genesis
    let mut chain = (*provider.chain_spec()).clone();
    chain.genesis.timestamp = timestamp;

    // StateProviderDatabase implements the required DatabaseRef trait
    let db = StateProviderDatabase::new(provider.latest().unwrap());
    let mut cache_db = CacheDB::new(db);
    // let random_state = RandomState::default();
    let mut journaled_state = JournaledState::new(
        SpecId::LATEST,
        HashSet::<Address, RandomState>::with_hasher(RandomState::default()),
    );

    // TODO: inhereting alloc from the loaded chain requires that we add reth state resets between runs in a single erlang shell
    // otherwise the 'new' genesis alloc will have different values
    // see the commented out reth_node ! {'EXIT', self(), normal} call in ar_weave:init/3
    // let mut genesis_accounts: BTreeMap<Address, GenesisAccount> = chain.genesis.alloc.clone();

    let mut genesis_accounts: BTreeMap<Address, GenesisAccount> = BTreeMap::new();
    accounts.iter().for_each(|(k, v)| {
        genesis_accounts.insert(k.clone(), v.clone());
    });

    // load genesis accounts into cache (memory) db, then apply shadows
    // in future we can extend this to evaluate EVM txs for the genesis block, if needed.
    for (address, account) in genesis_accounts
        .iter()
        .map(|(addr, acc)| (addr.clone(), acc.clone()))
    {
        let bytecode = Bytecode::new_raw(account.code.unwrap_or_default());
        cache_db.insert_account_info(
            address,
            AccountInfo {
                balance: account.balance,
                nonce: account.nonce.unwrap_or(0),
                code_hash: bytecode.clone().hash_slow(),
                code: Some(bytecode),
                stake: account.stake,
                commitments: account.commitments,
                last_tx: account.last_tx,
                mining_permission: account.mining_permission,
            },
        );
        for (slot, value) in account.storage.unwrap_or_default() {
            let res = cache_db.insert_account_storage(address, slot.into(), value.into());
        }
    }
    // apply all shadows
    for shadow in shadows.unwrap_or(Shadows::new(vec![])) {
        let res = apply_shadow(shadow, &mut journaled_state, &mut cache_db);
    }

    // commit state changes
    let (state, _logs) = journaled_state.finalize();
    debug!("Evaluated genesis with state: {:#?}", &state);
    cache_db.commit(state);

    let mut after_btree: BTreeMap<Address, GenesisAccount> = BTreeMap::new();

    cache_db.accounts.iter().for_each(|(addr, acc)| {
        let info = &acc.info;
        let genesis_account = GenesisAccount {
            nonce: Some(info.nonce),
            balance: info.balance,
            code: info.code.clone().map(|c| c.bytes()),
            storage: {
                let mut btree: BTreeMap<FixedBytes<32>, FixedBytes<32>> = BTreeMap::new();
                acc.storage.iter().for_each(|(k, v)| {
                    // todo: fix this, not even sure why this is required.
                    let nk = <Uint<256, 4> as Into<FixedBytes<32>>>::into(*k);
                    let nv = <Uint<256, 4> as Into<FixedBytes<32>>>::into(*v);
                    btree.insert(nk, nv);
                });
                Some(btree)
            },
            private_key: genesis_accounts.get(addr).unwrap().private_key,
            commitments: info.commitments.clone(),
            stake: info.stake,
            mining_permission: info.mining_permission,
            last_tx: info.last_tx,
        };
        after_btree.insert(addr.clone(), genesis_account);
    });

    // overwrite alloc
    chain.genesis.alloc = after_btree;

    // trigger reload
    // todo: redo this Arc<Mutex<...>> nonsense, ideally with a oneshot channel
    // let v = self.irys_ext.0.lock().map_err(|e| {
    //     ErrorObjectOwned::owned::<String>(
    //         -32080,
    //         "error locking reload channel",
    //         Some(e.to_string()),
    //     )
    // })?;

    let sender = ext.reload.write().map_err(|e| {
        ErrorObjectOwned::owned::<String>(
            -32080,
            "error locking reload channel",
            Some(e.to_string()),
        )
    })?;

    let hash = chain.genesis_hash();
    let header = serde_json::to_string(&chain.sealed_genesis_header())
        .expect("Unable to serialize genesis header");
    error!("Header: {}", &header);
    error!("Written genesis block (hash: {}), reloading...", &hash);

    // WARNING: RACE CONDITION
    // the reload operation doesn't wait for the reponse response of this method/RPC call to finish, it *does* have a 500ms delay, but that might not be
    // sufficient in certain conditions
    let _res = sender.send(ReloadPayload::ReloadConfig(chain.clone()));

    Ok(chain.genesis)
}
