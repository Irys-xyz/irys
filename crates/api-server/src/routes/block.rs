use crate::error::ApiError;
use crate::ApiState;
use actix_web::{
    web::{self, Json},
    Result,
};
use base58::{FromBase58 as _, ToBase58 as _};
use irys_actors::block_header_lookup;
use irys_types::{CombinedBlockHeader, ExecutionHeader, H256};
use reth::{providers::BlockReader as _, revm::primitives::alloy_primitives::TxHash};
use serde::{Deserialize, Serialize};
use std::{fmt::Display, str::FromStr};

pub async fn get_block_with_poa(
    state: web::Data<ApiState>,
    path: web::Path<String>,
) -> Result<Json<CombinedBlockHeader>, ApiError> {
    get_block(state, path, true).await
}

pub async fn get_block_without_poa(
    state: web::Data<ApiState>,
    path: web::Path<String>,
) -> Result<Json<CombinedBlockHeader>, ApiError> {
    get_block(state, path, false).await
}

#[expect(clippy::unused_async, reason = "actix-web handlers must be async")]
pub async fn get_block(
    state: web::Data<ApiState>,
    path: web::Path<String>,
    with_poa: bool,
) -> Result<Json<CombinedBlockHeader>, ApiError> {
    let tag_param = BlockParam::from_str(&path).map_err(|_| ApiError::ErrNoId {
        id: path.to_string(),
        err: String::from("Invalid block tag"),
    })?;

    let block_hash: H256 = match tag_param {
        BlockParam::Latest => state.block_tree.read().tip,
        BlockParam::BlockHeight(height) => 'outer: {
            let in_block_tree = state
                .block_tree
                .read()
                .get_canonical_chain()
                .0
                .iter()
                .find_map(|e| {
                    if e.height() == height {
                        Some(e.block_hash())
                    } else {
                        None
                    }
                });
            if let Some(hash) = in_block_tree {
                break 'outer hash;
            }
            state
                .block_index
                .read()
                .get_item(height)
                .ok_or(ApiError::ErrNoId {
                    id: path.to_string(),
                    err: String::from("Invalid block height"),
                })
                .map(|b| b.block_hash)?
        }
        BlockParam::Finalized | BlockParam::Pending => {
            return Err(ApiError::NotImplemented {
                feature: format!("Block tag '{tag_param}' is not yet supported"),
            });
        }
        BlockParam::Hash(hash) => hash,
    };
    get_block_by_hash(&state, block_hash, with_poa)
}

fn get_block_by_hash(
    state: &web::Data<ApiState>,
    block_hash: H256,
    with_poa: bool,
) -> Result<Json<CombinedBlockHeader>, ApiError> {
    let irys_header =
        block_header_lookup::get_block_header(&state.block_tree, &state.db, block_hash, with_poa)
            .map_err(|e| {
                tracing::error!("Error looking up block header: {}", e);
                ApiError::Internal {
                    err: "DB error".to_string(),
                }
            })?
            .ok_or_else(|| {
                tracing::warn!("No block header found for hash {}", block_hash);
                ApiError::ErrNoId {
                    id: block_hash.to_string(),
                    err: "block hash not found".to_string(),
                }
            })?;

    let reth_block = match state
        .reth_provider
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
        .map(|tx| *tx.hash())
        .collect::<Vec<TxHash>>();

    let cbh = CombinedBlockHeader {
        irys: irys_header,
        execution: ExecutionHeader {
            header: reth_block.header,
            transactions: exec_txs,
        },
    };

    Ok(web::Json(cbh))
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
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
            "latest" => Ok(Self::Latest),
            "pending" => Ok(Self::Pending),
            "finalized" => Ok(Self::Finalized),
            _ => {
                if let Ok(block_height) = s.parse::<u64>() {
                    return Ok(Self::BlockHeight(block_height));
                }
                match s.from_base58() {
                    Ok(v) => {
                        let arr: [u8; 32] = match v.as_slice().try_into() {
                            Ok(arr) => arr,
                            Err(_) => {
                                return Err(format!(
                                    "Invalid block tag parameter: expected 32 bytes, got {}",
                                    v.len()
                                ));
                            }
                        };
                        Ok(Self::Hash(H256(arr)))
                    }
                    Err(_) => Err("Invalid block tag parameter".to_string()),
                }
            }
        }
    }
}

impl Display for BlockParam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Latest => write!(f, "latest"),
            Self::Pending => write!(f, "pending"),
            Self::Finalized => write!(f, "finalized"),
            Self::BlockHeight(height) => write!(f, "{height}"),
            Self::Hash(hash) => write!(f, "{}", hash.0.to_base58()),
        }
    }
}

// #[cfg(test)]
// mod tests {
//     use crate::routes;

//     use super::*;
//     use actix::{Actor, SystemRegistry, SystemService as _};
//     use actix_web::{middleware::Logger, test, App, Error};
//     use awc::http::StatusCode;
//     use base58::ToBase58;
//     use database::open_or_create_db;
//     use irys_actors::mempool_service::MempoolService;
//     use irys_database::tables::IrysTables;
//     use irys_storage::ChunkProvider;
//     use irys_types::{app_state::DatabaseProvider, irys::IrysSigner, StorageConfig};
//     use log::{error, info};
//     use reth::tasks::TaskManager;
//     use std::sync::Arc;
//     use tempfile::tempdir;

//     #[ignore = "broken due to reth provider/block tree dependency"]
//     #[tokio::test]
//     async fn test_get_block() -> eyre::Result<()> {
//         //std::env::set_var("RUST_LOG", "debug");
//         let _ = env_logger::try_init();

//         //let path = get_data_dir();
//         let path = tempdir().unwrap();
//         let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
//         let blk = IrysBlockHeader::default();

//         let res =
//             db.update(|tx| -> eyre::Result<()> { database::insert_block_header(tx, &blk) })?;
//         res.unwrap();

//         match db.view_eyre(|tx| database::block_header_by_hash(tx, &blk.block_hash))? {
//             None => error!("block not found, test db error!"),
//             Some(blk_get) => {
//                 info!("block found!");
//                 assert_eq!(blk, blk_get, "retrieved another block");
//             }
//         };

//         let db_arc = Arc::new(db);
//         let task_manager = TaskManager::current();
//         let storage_config = StorageConfig::default();

//         let mempool_service = MempoolService::new(
//             irys_types::app_state::DatabaseProvider(db_arc.clone()),
//             task_manager.executor(),
//             IrysSigner::random_signer(),
//             storage_config.clone(),
//             Arc::new(Vec::new()).to_vec(),
//         );
//         SystemRegistry::set(mempool_service.start());
//         let mempool_addr = MempoolService::from_registry();
//         let chunk_provider = ChunkProvider::new(
//             storage_config.clone(),
//             Arc::new(Vec::new()).to_vec(),
//             DatabaseProvider(db_arc.clone()),
//         );

//         let app_state = ApiState {
//             reth_provider: None,
//             reth_http_url: None,
//             block_index: None,
//             block_tree: None,
//             db: DatabaseProvider(db_arc.clone()),
//             mempool: mempool_addr,
//             chunk_provider: Arc::new(chunk_provider),
//         };

//         let app = test::init_service(
//             App::new()
//                 .wrap(Logger::default())
//                 .app_data(web::Data::new(app_state))
//                 .service(routes()),
//         )
//         .await;

//         let blk_hash: String = blk.block_hash.as_bytes().to_base58();
//         let req = test::TestRequest::get()
//             .uri(&format!("/v1/block/{}", &blk_hash))
//             .to_request();

//         let resp = test::call_service(&app, req).await;
//         assert_eq!(resp.status(), StatusCode::OK);
//         let result: IrysBlockHeader = test::read_body_json(resp).await;
//         assert_eq!(blk, result);
//         Ok(())
//     }

//     #[ignore = "broken due to reth provider/block tree dependency"]
//     #[tokio::test]
//     async fn test_get_non_existent_block() -> Result<(), Error> {
//         let path = tempdir().unwrap();
//         let db = open_or_create_db(path, IrysTables::ALL, None).unwrap();
//         let blk = IrysBlockHeader::default();

//         let db_arc = Arc::new(db);

//         let task_manager = TaskManager::current();
//         let storage_config = StorageConfig::default();

//         let mempool_service = MempoolService::new(
//             irys_types::app_state::DatabaseProvider(db_arc.clone()),
//             task_manager.executor(),
//             IrysSigner::random_signer(),
//             storage_config.clone(),
//             Arc::new(Vec::new()).to_vec(),
//         );
//         SystemRegistry::set(mempool_service.start());
//         let mempool_addr = MempoolService::from_registry();
//         let chunk_provider = ChunkProvider::new(
//             storage_config.clone(),
//             Arc::new(Vec::new()).to_vec(),
//             DatabaseProvider(db_arc.clone()),
//         );

//         let app_state = ApiState {
//             reth_provider: None,
//             reth_http_url: None,
//             block_index: None,
//             block_tree: None,
//             db: DatabaseProvider(db_arc.clone()),
//             mempool: mempool_addr,
//             chunk_provider: Arc::new(chunk_provider),
//         };

//         let app = test::init_service(
//             App::new()
//                 .app_data(web::Data::new(app_state))
//                 .service(routes()),
//         )
//         .await;

//         let id: String = blk.block_hash.as_bytes().to_base58();
//         let req = test::TestRequest::get()
//             .uri(&format!("/v1/block/{}", &id))
//             .to_request();

//         let resp = test::call_service(&app, req).await;
//         assert_eq!(resp.status(), StatusCode::NOT_FOUND);
//         let result: ApiError = test::read_body_json(resp).await;
//         let blk_error = ApiError::ErrNoId {
//             id: blk.block_hash.to_string(),
//             err: String::from("block hash not found"),
//         };
//         assert_eq!(blk_error, result);
//         Ok(())
//     }
// }
