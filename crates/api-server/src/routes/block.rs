use crate::error::ApiError;
use crate::ApiState;
use actix_web::{
    web::{self, Json},
    Result,
};
use base58::FromBase58 as _;
use irys_database::database;
use irys_types::{IrysBlockHeader, H256};
use reth::{
    primitives::Header, providers::BlockReader, revm::primitives::alloy_primitives::TxHash,
};
use reth_db::Database;
use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub async fn get_block(
    state: web::Data<ApiState>,
    path: web::Path<String>,
) -> Result<Json<CombinedBlockHeader>, ApiError> {
    let tag_param = BlockParam::from_str(&path).map_err(|_| ApiError::ErrNoId {
        id: path.to_string(),
        err: String::from("Invalid block tag"),
    })?;

    // all roads lead to block hash
    let block_hash: H256 = match tag_param {
        BlockParam::Latest => {
            let block_tree_guard = state.block_tree.clone().ok_or(ApiError::Internal {
                err: String::from("block tree error"),
            })?;
            let guard = block_tree_guard.read();
            guard.tip
        }
        BlockParam::BlockHeight(height) => 'outer: {
            let block_tree_guard = state.block_tree.clone().ok_or(ApiError::Internal {
                err: String::from("block tree error"),
            })?;
            let guard = block_tree_guard.read();
            let canon_chain = guard.get_canonical_chain();
            let in_block_tree =
                canon_chain
                    .0
                    .iter()
                    .find_map(|(hash, hght, _, _)| match *hght == height {
                        true => Some(hash),
                        false => None,
                    });
            if let Some(hash) = in_block_tree {
                break 'outer *hash;
            }
            // get from block index
            let block_index_guard = state.block_index.clone().ok_or(ApiError::Internal {
                err: String::from("block index error"),
            })?;
            let guard = block_index_guard.read();
            let r = guard
                .get_item(height.try_into().map_err(|_| ApiError::Internal {
                    err: String::from("Block height out of range"),
                })?)
                .ok_or(ApiError::ErrNoId {
                    id: path.to_string(),
                    err: String::from("Invalid block height"),
                })?;
            r.block_hash
        }
        BlockParam::Finalized | BlockParam::Pending => {
            return Err(ApiError::Internal {
                err: String::from("Unsupported tag"),
            });
        }
        BlockParam::Hash(hash) => hash,
    };
    get_block_by_hash(&state, block_hash)
}

fn get_block_by_hash(
    state: &web::Data<ApiState>,
    block_hash: H256,
) -> Result<Json<CombinedBlockHeader>, ApiError> {
    let irys_header = match state
        .db
        .view_eyre(|tx| database::block_header_by_hash(tx, &block_hash))
    {
        Err(_error) => Err(ApiError::Internal {
            err: String::from("db error"),
        }),
        Ok(None) => Err(ApiError::ErrNoId {
            id: block_hash.to_string(),
            err: String::from("block hash not found"),
        }),
        Ok(Some(tx_header)) => Ok(tx_header),
    }?;

    let reth = match &state.reth_provider {
        Some(r) => r,
        None => {
            return Err(ApiError::Internal {
                err: String::from("db error"),
            })
        }
    };

    let reth_block = match reth
        .provider
        .block_by_hash(irys_header.evm_block_hash)
        .ok()
        .flatten()
    {
        Some(r) => r,
        None => {
            return Err(ApiError::Internal {
                err: String::from("db error"),
            })
        }
    };

    let exec_txs = reth_block
        .body
        .transactions
        .iter()
        .map(|tx| tx.hash())
        .collect::<Vec<TxHash>>();

    let cbh = CombinedBlockHeader {
        irys: irys_header,
        execution: ExecutionHeader {
            header: reth_block.header.clone(),
            transactions: exec_txs,
        },
    };

    Ok(web::Json(cbh))
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]

pub struct CombinedBlockHeader {
    #[serde(flatten)]
    pub irys: IrysBlockHeader,
    pub execution: ExecutionHeader,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]

pub struct ExecutionHeader {
    #[serde(flatten)]
    pub header: Header,
    pub transactions: Vec<TxHash>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum BlockParam {
    Latest,
    Pending,
    Finalized,
    BlockHeight(u64),
    Hash(H256),
}

impl FromStr for BlockParam {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "latest" => Ok(BlockParam::Latest),
            "pending" => Ok(BlockParam::Pending),
            "finalized" => Ok(BlockParam::Finalized),
            _ => {
                if let Ok(block_height) = s.parse::<u64>() {
                    return Ok(BlockParam::BlockHeight(block_height));
                }
                match s.from_base58() {
                    Ok(v) => Ok(BlockParam::Hash(H256::from_slice(v.as_slice()))),
                    Err(_) => Err("Invalid block tag parameter".to_string()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::routes;

    use super::*;
    use actix::{Actor, ArbiterService, SystemRegistry, SystemService as _};
    use actix_web::{middleware::Logger, test, App, Error};
    use awc::http::StatusCode;
    use base58::ToBase58;
    use database::open_or_create_db;
    use irys_actors::mempool_service::MempoolService;
    use irys_database::tables::IrysTables;
    use irys_storage::ChunkProvider;
    use irys_types::{app_state::DatabaseProvider, irys::IrysSigner, StorageConfig};
    use log::{error, info};
    use reth::tasks::TaskManager;
    use std::sync::Arc;
    use tempfile::tempdir;

    #[ignore = "broken due to reth provider/block tree dependency"]
    #[actix_web::test]
    async fn test_get_block() -> eyre::Result<()> {
        //std::env::set_var("RUST_LOG", "debug");
        let _ = env_logger::try_init();

        //let path = get_data_dir();
        let path = tempdir().unwrap();
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
        let blk = IrysBlockHeader::default();

        let res =
            db.update(|tx| -> eyre::Result<()> { database::insert_block_header(tx, &blk) })?;
        res.unwrap();

        match db.view_eyre(|tx| database::block_header_by_hash(tx, &blk.block_hash))? {
            None => error!("block not found, test db error!"),
            Some(blk_get) => {
                info!("block found!");
                assert_eq!(blk, blk_get, "retrived another block");
            }
        };

        let db_arc = Arc::new(db);
        let task_manager = TaskManager::current();
        let storage_config = StorageConfig::default();

        let mempool_service = MempoolService::new(
            irys_types::app_state::DatabaseProvider(db_arc.clone()),
            task_manager.executor(),
            IrysSigner::random_signer(),
            storage_config.clone(),
            Arc::new(Vec::new()).to_vec(),
        );
        SystemRegistry::set(mempool_service.start());
        let mempool_addr = MempoolService::from_registry();
        let chunk_provider = ChunkProvider::new(
            storage_config.clone(),
            Arc::new(Vec::new()).to_vec(),
            DatabaseProvider(db_arc.clone()),
        );

        let app_state = ApiState {
            reth_provider: None,
            block_index: None,
            block_tree: None,
            db: DatabaseProvider(db_arc.clone()),
            mempool: mempool_addr,
            chunk_provider: Arc::new(chunk_provider),
        };

        let app = test::init_service(
            App::new()
                .wrap(Logger::default())
                .app_data(web::Data::new(app_state))
                .service(routes()),
        )
        .await;

        let blk_hash: String = blk.block_hash.as_bytes().to_base58();
        let req = test::TestRequest::get()
            .uri(&format!("/v1/block/{}", &blk_hash))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let result: IrysBlockHeader = test::read_body_json(resp).await;
        assert_eq!(blk, result);
        Ok(())
    }

    #[ignore = "broken due to reth provider/block tree dependency"]
    #[actix_web::test]
    async fn test_get_non_existent_block() -> Result<(), Error> {
        let path = tempdir().unwrap();
        let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
        let blk = IrysBlockHeader::default();

        let db_arc = Arc::new(db);

        let task_manager = TaskManager::current();
        let storage_config = StorageConfig::default();

        let mempool_service = MempoolService::new(
            irys_types::app_state::DatabaseProvider(db_arc.clone()),
            task_manager.executor(),
            IrysSigner::random_signer(),
            storage_config.clone(),
            Arc::new(Vec::new()).to_vec(),
        );
        SystemRegistry::set(mempool_service.start());
        let mempool_addr = MempoolService::from_registry();
        let chunk_provider = ChunkProvider::new(
            storage_config.clone(),
            Arc::new(Vec::new()).to_vec(),
            DatabaseProvider(db_arc.clone()),
        );

        let app_state = ApiState {
            reth_provider: None,
            block_index: None,
            block_tree: None,
            db: DatabaseProvider(db_arc.clone()),
            mempool: mempool_addr,
            chunk_provider: Arc::new(chunk_provider),
        };

        let app = test::init_service(
            App::new()
                .app_data(web::Data::new(app_state))
                .service(routes()),
        )
        .await;

        let id: String = blk.block_hash.as_bytes().to_base58();
        let req = test::TestRequest::get()
            .uri(&format!("/v1/block/{}", &id))
            .to_request();

        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let result: ApiError = test::read_body_json(resp).await;
        let blk_error = ApiError::ErrNoId {
            id: blk.block_hash.to_string(),
            err: String::from("block hash not found"),
        };
        assert_eq!(blk_error, result);
        Ok(())
    }
}
