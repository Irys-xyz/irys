use actix_web::{
    web::{self, Path},
    HttpResponse,
};
use irys_database::Ledger;
use irys_types::perm_storage_price::PriceCalc;
use crate::ApiState;

pub async fn get_price(path: Path<(String, u64)>, state: web::Data<ApiState>) -> actix_web::Result<HttpResponse> {
    let (ledger, size) = path.into_inner();

    match Ledger::try_from(ledger.as_str()) {
        Ok(Ledger::Publish) => match PriceCalc::calc_perm_storage_price(size, &state.config) {
            Ok(perm_storage_price) => Ok(HttpResponse::Ok().body(perm_storage_price.to_string())),
            Err(e) => Ok(HttpResponse::BadRequest().body(format!("{e:?}"))),
        },
        Ok(Ledger::Submit) => Ok(HttpResponse::BadRequest().body("term not yet implemented")),
        Err(_) => Ok(HttpResponse::BadRequest().body("Ledger type not supported")),
    }
}

#[cfg(test)]
mod test {
    // use super::*;
    // use awc::{body::to_bytes, http::StatusCode};
//     use irys_config::PRICE_PER_CHUNK_5_EPOCH;
//     use irys_types::CONFIG;

//     fn get_expected_chunk_price() -> Option<f64> {
//         // These values come from 200 years, 1% decay rate, n partitions, 5% miner fee
//         const PRICE_FOR_1_PARTITION: f64 = 8.674582693274584e-5;
//         const PRICE_FOR_10_PARTITIONS: f64 = 0.0006233236047864428;

//         match CONFIG.num_partitions_per_slot {
//             1 => Some(PRICE_FOR_1_PARTITION),
//             10 => Some(PRICE_FOR_10_PARTITIONS),
//             _ => None, // todo: if number of replicas, or partitions_per_slot are added, append them here
//         }
//     }

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
}
