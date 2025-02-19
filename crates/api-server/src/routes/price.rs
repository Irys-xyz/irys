use actix_web::{web::Path, HttpResponse};
use irys_config::{PRICE_PER_CHUNK_5_EPOCH, PRICE_PER_CHUNK_PERM};
use irys_database::Ledger;
use irys_types::{perm_storage_price::PriceCalc, CONFIG};

pub async fn get_perm_storage_pricing(path: Path<u64>) -> actix_web::Result<HttpResponse> {
    match PriceCalc::calc_perm_storage_price(path.into_inner()) {
        Ok(perm_storage_price) => Ok(HttpResponse::Ok().body(perm_storage_price.to_string())),
        Err(e) => Ok(HttpResponse::BadRequest().body(format!("{e:?}"))),
    }
}

pub async fn get_price(path: Path<(String, u64)>) -> actix_web::Result<HttpResponse> {
    let size = path.1;
    let ledger = Ledger::from_url(&path.0);

    let num_of_chunks = if size < CONFIG.chunk_size {
        1u128
    } else {
        // Safe because u128 > u64
        (size % CONFIG.chunk_size + 1) as u128
    };

    if let Ok(l) = ledger {
        let final_price = match l {
            Ledger::Publish => PRICE_PER_CHUNK_PERM,
            Ledger::Submit => PRICE_PER_CHUNK_5_EPOCH,
        } * num_of_chunks;

        Ok(HttpResponse::Ok().body(final_price.to_string()))
    } else {
        Ok(HttpResponse::BadRequest().body("Ledger type not support"))
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use awc::{body::to_bytes, http::StatusCode};

    #[actix_web::test]
    async fn test_bad_url() {
        let path = Path::from((String::default(), u64::default()));
        let res = get_price(path).await.unwrap();
        assert_eq!(StatusCode::BAD_REQUEST, res.status())
    }

    #[actix_web::test]
    async fn test_perm() {
        let storage_type = String::from("perm");
        let path = Path::from((storage_type, u64::default()));
        let res = get_price(path).await.unwrap();
        assert_eq!(StatusCode::OK, res.status())
    }

    #[actix_web::test]
    async fn test_term() {
        let storage_type = String::from("5days");
        let path = Path::from((storage_type, u64::default()));
        let res = get_price(path).await.unwrap();
        assert_eq!(StatusCode::OK, res.status())
    }

    #[actix_web::test]
    async fn test_get_price_one_chunk_perm() {
        let additional_chunks = 0;
        let expected_price = PRICE_PER_CHUNK_PERM;
        let storage_type = String::from("perm");
        let path: Path<(String, u64)> =
            Path::from((storage_type, CONFIG.chunk_size + additional_chunks));
        let response = get_price(path).await.unwrap().into_body();
        let body = to_bytes(response).await.unwrap();
        let result = String::from_utf8_lossy(&body);
        assert_eq!(expected_price.to_string(), result)
    }

    #[actix_web::test]
    async fn test_get_price_one_chunk_term() {
        let additional_chunks = 0;
        let expected_price = PRICE_PER_CHUNK_5_EPOCH;
        let storage_type = String::from("5days");
        let path: Path<(String, u64)> =
            Path::from((storage_type, CONFIG.chunk_size + additional_chunks));
        let response = get_price(path).await.unwrap().into_body();
        let body = to_bytes(response).await.unwrap();
        let result = String::from_utf8_lossy(&body);
        assert_eq!(expected_price.to_string(), result)
    }

    #[actix_web::test]
    async fn test_get_price_n_chunks_perm() {
        let additional_chunks = 17;
        let expected_price = PRICE_PER_CHUNK_PERM * (additional_chunks + 1);
        let storage_type = String::from("perm");
        let path: Path<(String, u64)> =
            Path::from((storage_type, CONFIG.chunk_size + additional_chunks as u64));
        let response = get_price(path).await.unwrap().into_body();
        let body = to_bytes(response).await.unwrap();
        let result = String::from_utf8_lossy(&body);
        assert_eq!(expected_price.to_string(), result)
    }

    #[actix_web::test]
    async fn test_get_price_n_chunks_term() {
        let additional_chunks = 15;
        let expected_price = (additional_chunks + 1) * PRICE_PER_CHUNK_5_EPOCH;
        let storage_type = String::from("5days");
        let path: Path<(String, u64)> =
            Path::from((storage_type, CONFIG.chunk_size + additional_chunks as u64));
        let response = get_price(path).await.unwrap().into_body();
        let body = to_bytes(response).await.unwrap();
        let result = String::from_utf8_lossy(&body);
        assert_eq!(expected_price.to_string(), result)
    }

    #[actix_web::test]
    async fn test_get_perm_storage_pricing_1_gib() {
        let number_of_bytes_to_store = 1024 * 1024 * 1024; // 1 GiB
        let path: Path<u64> = Path::from(number_of_bytes_to_store);
        let response = get_perm_storage_pricing(path).await.unwrap();
        let result = response.into_body();
        let body = to_bytes(result).await.unwrap();
        let result = String::from_utf8_lossy(&body);
        assert_eq!("0.34368221622716855".to_string(), result)
    }

    #[actix_web::test]
    async fn test_get_perm_storage_pricing_1_chunk() {
        let number_of_bytes_to_store = 1; // 1 B
        let path: Path<u64> = Path::from(number_of_bytes_to_store);
        let response = get_perm_storage_pricing(path).await.unwrap();
        let result = response.into_body();
        let body = to_bytes(result).await.unwrap();
        let result = String::from_utf8_lossy(&body);
        // The price/GiB / (1024 MiB/GiB * 4 chunks/MiB) = price/chunk
        assert_eq!("0.00008390679107108607".to_string(), result)
    }

    #[actix_web::test]
    async fn test_get_perm_storage_pricing_no_chunks() {
        let number_of_bytes_to_store = 0; // 0 B
        let path: Path<u64> = Path::from(number_of_bytes_to_store);
        let response = get_perm_storage_pricing(path).await.unwrap();
        let result = response.into_body();
        let body = to_bytes(result).await.unwrap();
        let result = String::from_utf8_lossy(&body);
        // The price/GiB / (1024 MiB/GiB * 4 chunks/MiB) = price/chunk
        assert_eq!("0".to_string(), result)
    }
}
