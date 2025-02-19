use crate::ApiState;
use actix_web::{
    web::{self, Path},
    HttpResponse,
};
use irys_database::Ledger;
use irys_types::perm_storage_price::PriceCalc;

pub async fn get_price(
    path: Path<(String, u64)>,
    state: web::Data<ApiState>,
) -> actix_web::Result<HttpResponse> {
    let (ledger, size) = path.into_inner();

    match Ledger::try_from(ledger.as_str()) {
        Ok(Ledger::Publish) => match PriceCalc::calc_perm_storage_price(size, &state.config) {
            Ok(perm_storage_price) => Ok(HttpResponse::Ok().body(perm_storage_price.to_string())),
            Err(e) => Ok(HttpResponse::BadRequest().body(format!("{e:?}"))),
        },
        Ok(Ledger::Submit) => match PriceCalc::calc_term_storage_price(size, &state.config) {
            Ok(term_storage_price) => Ok(HttpResponse::Ok().body(term_storage_price.to_string())),
            Err(e) => Ok(HttpResponse::BadRequest().body(format!("{e:?}"))),
        },
        Err(_) => Ok(HttpResponse::BadRequest().body("Ledger type not supported")),
    }
}

// #[cfg(test)]
// mod test {
//     use super::*;
//     use awc::{body::to_bytes, http::StatusCode};
//     use irys_config::PRICE_PER_CHUNK_5_EPOCH;

//     fn get_expected_chunk_price() -> Option<f64> {
//         // These values come from 200 years, 1% decay rate, n partitions, 5% miner fee
//         const PRICE_FOR_1_PARTITION: f64 = 8.674582693274584e-5;
//         const PRICE_FOR_10_PARTITIONS: f64 = 0.0006233236047864428;

//         match get_config().num_partitions_per_slot {
//             1 => Some(PRICE_FOR_1_PARTITION),
//             10 => Some(PRICE_FOR_10_PARTITIONS),
//             _ => None, // todo: if number of replicas, or partitions_per_slot are added, append them here
//         }
//     }

//     fn get_config() -> Config {
//         Config {
//             block_time: u64::default(),
//                 max_data_txs_per_block: u64::default(),
//                 difficulty_adjustment_interval: u64::default(),
//                 max_difficulty_adjustment_factor: rust_decimal_macros::dec!(4),
//                 min_difficulty_adjustment_factor: rust_decimal_macros::dec!(0.25),
//                 chunk_size: 256 * 1024,
//                 num_chunks_in_partition: 10,
//                 num_chunks_in_recall_range: u64::default(),
//                 vdf_reset_frequency: usize::default(),
//                 vdf_parallel_verification_thread_limit: usize::default(),
//                 num_checkpoints_in_vdf_step: usize::default(),
//                 vdf_sha_1s: u64::default(),
//                 entropy_packing_iterations: u32::default(),
//                 chain_id: u64::default(),
//                 capacity_scalar: u64::default(),
//                 num_blocks_in_epoch: u64::default(),
//                 submit_ledger_epoch_length: u64::default(),
//                 num_partitions_per_slot: 1,
//                 num_writes_before_sync: u64::default(),
//                 reset_state_on_restart: bool::default(),
//                 chunk_migration_depth: u32::default(),
//                 mining_key: SigningKey::from_slice(
//                     &hex::decode(b"db793353b633df950842415065f769699541160845d73db902eadee6bc5042d0")
//                         .expect("valid hex"),
//                 )
//                 .expect("valid key"),
//                 num_capacity_partitions: None,
//                 port: u16::default(),
//                 anchor_expiry_depth: u8::default(),
//                 genesis_price_valid_for_n_epochs: u8::default(),
//                 genesis_token_price: Amount::token(rust_decimal_macros::dec!(1)).expect("valid token amount"),
//                 token_price_safe_range: Amount::percentage(rust_decimal_macros::dec!(1)).expect("valid percentage"),
//                 decay_params: DecayParams {
//                     safe_minimum_number_of_years: 200,
//                     annualized_decay_rate: rust_decimal_macros::dec!(0.01),
//                 },
//                 storage_fees: StorageFees {
//                     number_of_ingress_proofs: 10,
//                     ingress_fee: rust_decimal_macros::dec!(0.01)
//                 }
//             }
//         }

//     #[actix_web::test]
//     async fn test_bad_url() {
//         let ledger = "8".to_string(); // 8 is not a valid ledger number
//         let path = Path::from((ledger, u64::default()));
//         let res = get_price(path).await.unwrap();
//         assert_eq!(StatusCode::BAD_REQUEST, res.status())
//     }

//     #[actix_web::test]
//     async fn test_perm() {
//         let ledger = "0".to_string();
//         let path = Path::from((ledger, u64::default()));
//         let res = get_price(path).await.unwrap();
//         assert_eq!(StatusCode::OK, res.status())
//     }

//     #[ignore = "implement with term_price ticket"]
//     #[actix_web::test]
//     async fn test_term() {
//         let ledger = "1".to_string();
//         let path = Path::from((ledger, u64::default()));
//         let res = get_price(path).await.unwrap();
//         assert_eq!(StatusCode::OK, res.status())
//     }

//     #[ignore = "implement with term_price ticket"]
//     #[actix_web::test]
//     async fn test_get_price_one_chunk_term() {
//         let additional_chunks = 0;
//         let expected_price = PRICE_PER_CHUNK_5_EPOCH;
//         let ledger = "1".to_string();
//         let path = Path::from((ledger, CONFIG.chunk_size + additional_chunks));
//         let response = get_price(path).await.unwrap().into_body();
//         let body = to_bytes(response).await.unwrap();
//         let result = String::from_utf8_lossy(&body);
//         assert_eq!(expected_price.to_string(), result)
//     }

//     #[ignore = "implement with term_price ticket"]
//     #[actix_web::test]
//     async fn test_get_price_n_chunks_term() {
//         let additional_chunks = 15;
//         let expected_price = (additional_chunks + 1) * PRICE_PER_CHUNK_5_EPOCH;
//         let ledger = "1".to_string();
//         let path = Path::from((ledger, CONFIG.chunk_size + additional_chunks as u64));
//         let response = get_price(path).await.unwrap().into_body();
//         let body = to_bytes(response).await.unwrap();
//         let result = String::from_utf8_lossy(&body);
//         assert_eq!(expected_price.to_string(), result)
//     }

//     #[actix_web::test]
//     async fn test_get_perm_storage_pricing_1_gib() {
//         let number_of_bytes_to_store = 1024 * 1024 * 1024; // 1 GiB
//         let ledger = "0".to_string();
//         let path = Path::from((ledger, number_of_bytes_to_store));
//         let response = get_price(path).await.unwrap();
//         let result = response.into_body();
//         let body = to_bytes(result).await.unwrap();
//         let result = String::from_utf8_lossy(&body);
//         let expected_price = get_expected_chunk_price().unwrap() * 1024.0 * 4.0;
//         assert_eq!(expected_price.to_string(), result)
//     }

//     #[actix_web::test]
//     async fn test_get_perm_storage_pricing_17_chunks() {
//         let number_of_chunks = 17;
//         let number_of_bytes_to_store = CONFIG.chunk_size * number_of_chunks;
//         let ledger = "0".to_string();
//         let path = Path::from((ledger, number_of_bytes_to_store));
//         let response = get_price(path).await.unwrap();
//         let result = response.into_body();
//         let body = to_bytes(result).await.unwrap();
//         let result = String::from_utf8_lossy(&body);
//         let expected_price = get_expected_chunk_price().unwrap() * number_of_chunks as f64;
//         assert_eq!(expected_price.to_string(), result)
//     }

//     #[actix_web::test]
//     async fn test_get_perm_storage_pricing_1_chunk() {
//         let number_of_bytes_to_store = 1; // 1 B < 1 chunk, Min price is 1 chunk.
//         let ledger = "0".to_string();
//         let path = Path::from((ledger, number_of_bytes_to_store));
//         let response = get_price(path).await.unwrap();
//         let result = response.into_body();
//         let body = to_bytes(result).await.unwrap();
//         let result = String::from_utf8_lossy(&body);
//         // The price/GiB / (1024 MiB/GiB * 4 chunks/MiB) = price/chunk
//         let expected_price = get_expected_chunk_price().unwrap();
//         assert_eq!(expected_price.to_string(), result)
//     }

//     #[actix_web::test]
//     async fn test_get_perm_storage_pricing_no_chunks() {
//         let number_of_bytes_to_store = 0; // 0 B
//         let ledger = "0".to_string();
//         let path = Path::from((ledger, number_of_bytes_to_store));
//         let response = get_price(path).await.unwrap();
//         let result = response.into_body();
//         let body = to_bytes(result).await.unwrap();
//         let result = String::from_utf8_lossy(&body);
//         // The price/GiB / (1024 MiB/GiB * 4 chunks/MiB) = price/chunk
//         assert_eq!("0".to_string(), result)
//     }
// }
