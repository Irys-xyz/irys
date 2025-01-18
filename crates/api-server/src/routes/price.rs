use std::cmp::max;

use actix_web::{web::Path, HttpResponse};
use irys_config::{PRICE_PER_CHUNK_5_EPOCH, PRICE_PER_CHUNK_PERM};
use irys_database::Ledger;
use irys_types::CONFIG;

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
            Ledger::Submit => PRICE_PER_CHUNK_5_EPOCH
        } * num_of_chunks;

        Ok(HttpResponse::Ok().body(final_price.to_string()))
    } else {
        Ok(HttpResponse::BadRequest().body("Ledger type not support"))
    }
}
