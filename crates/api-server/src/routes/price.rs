use actix_web::{web::Path, HttpResponse};
use eyre::Error;
use irys_config::{ANNUALIZED_COST_OF_STORING_1GB, PRICE_PER_CHUNK_5_EPOCH, PRICE_PER_CHUNK_PERM};
use irys_database::Ledger;
use irys_types::CONFIG;

// This will eventually come from Rob's work from another crate
fn get_usd_to_irys_conversion_rate() -> f64 {
    1.0
}

fn calc_perm_storage_price(number_of_bytes_to_store: u128) -> Result<f64, Error> {
    const CHUNKS_PER_GB: u32 = 4*1024;  // 256 KiB chunk size
    
    // where are these two params coming from / living?
    let safe_minimum_number_of_years = 200;
    let annualized_decay_rate = 0.01;

    // calculate the fees / GB
    let perm_cost = calc_perm_cost_per_gb(safe_minimum_number_of_years, annualized_decay_rate)?;
    let perm_fee = calc_perm_fee_per_gb(perm_cost);

    let approximate_usd_irys_price = get_usd_to_irys_conversion_rate();
    let chunks = get_chunks_from_bytes(number_of_bytes_to_store as u128);
    Ok(chunks as f64 * perm_fee * approximate_usd_irys_price / CHUNKS_PER_GB as f64)
}

fn get_chunks_from_bytes(number_of_bytes_to_store: u128) -> u64 {
    const MIN_CHUNK: u128 = 256 * 1024;    // 256 KiB

    if number_of_bytes_to_store == 0 {
        return 0;
    }
    let chunks =  number_of_bytes_to_store.checked_div(MIN_CHUNK).expect("RHS is const");

    if number_of_bytes_to_store % MIN_CHUNK != 0 {
        u64::try_from(chunks).unwrap() + 1
    } else {
        u64::try_from(chunks).unwrap()
    }
}

// The Irys pricing model. Expected to change periodically in a hard fork.
fn calc_perm_cost_per_gb(safe_minimum_number_of_years: u32, annualized_decay_rate: f64) -> Result<f64, Error> {
    // https://docs.google.com/spreadsheets/d/14q8kKiDU2075zofQa8FRQv5iNiOCHyjEa8KRpMoshjg/edit?gid=0#gid=0
    const ANNUALIZED_COST_OF_STORING_16TB: f64 = 44.0;
    const NUMBER_OF_REPLICAS: u32 = 10;
    const TB_PER_DRIVE: f64 = 16.0;
    const ANNUALIZED_COST_OF_STORING_1TB: f64 = ANNUALIZED_COST_OF_STORING_16TB / TB_PER_DRIVE;
    const GB_PER_TB: f64 = 1024.0;

    if safe_minimum_number_of_years == 0 {
        todo!("Error here")
    }
    if annualized_decay_rate <= 0.0 {
        todo!("Error here")
    }

    let total_cost = (ANNUALIZED_COST_OF_STORING_1TB / GB_PER_TB) * (1.0 - f64::powi(1.0 - annualized_decay_rate, safe_minimum_number_of_years as i32)) / annualized_decay_rate; 
    Ok(total_cost * NUMBER_OF_REPLICAS as f64)
}

fn calc_perm_fee_per_gb(perm_cost: f64) -> f64 {
    // https://docs.google.com/document/d/1APuymt4TPqu3_oDGXHRtnSD8A_OOjDH0HzteY2ae_q4/edit?tab=t.0#heading=h.k00jfweqzvbx
    const INGRESS_FEE: f64 = 0.01;   // TODO require a ticket to determine what this will eventually be
    const NUM_INGRESS_PROOFS: u32 = 10;

    ANNUALIZED_COST_OF_STORING_1GB + (INGRESS_FEE * NUM_INGRESS_PROOFS as f64) + perm_cost
}

pub async fn get_perm_storage_pricing(path: Path<u128>) -> actix_web::Result<HttpResponse> {
    // - Hardcode the annualized cost of storing 1GB as a config parameter.
    // - Use the $IRYS/USD price calculated in
    // - $IRYS/USD Price Approximation #180 as a pricing parameter.
    // - Set 200 years and annual cost decline as consensus-configurable parameters.
    // - Implement a function that takes the number of bytes to be stored and returns the $IRYS-denominated price for that storage.
    // - Enforce a minimum storage price equivalent to 256KiB of permanent data (1 chunk, the smallest verifiable storage unit in Irys).
    // - Update the HTTP endpoint to expose the storage pricing function to users.


    // These are protocol-managed parameters that adjust pricing over time.
    // Annualized Cost of Storing 1GB:
    // The current annualized storage cost per GB, denominated in USD, is maintained as a protocol parameter. It is periodically adjusted via hard forks.
    // Approximate USD/$IRYS Price:
    // The protocol maintains an approximate USD/$IRYS exchange rate as a parameter. This rate is updated dynamically using the price adjustment interval mechanism, ensuring that storage prices remain stable in USD terms.
    let number_of_bytes_to_store = path.into_inner();
    let perm_storage_price = calc_perm_storage_price(number_of_bytes_to_store).unwrap();
    Ok(HttpResponse::Ok().body(perm_storage_price.to_string()))
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
    use awc::{body::to_bytes, http::StatusCode};
    use super::*;

    #[actix_web::test]
    async fn test_get_perm_storage_pricing_1_gib() {
        let number_of_bytes_to_store = 1024*1024*1024;  // 1 GiB
        let path:Path<u128> = Path::from(number_of_bytes_to_store);
        let response = get_perm_storage_pricing(path).await.unwrap();
        let result = response.into_body();
        let body = to_bytes(result).await.unwrap();
        let result = String::from_utf8_lossy(&body);
        assert_eq!("2.4368465763116856".to_string(), result)
    }

    #[actix_web::test]
    async fn test_get_perm_storage_pricing_1_chunk() {
        let number_of_bytes_to_store = 1;  // 1 B
        let path:Path<u128> = Path::from(number_of_bytes_to_store);
        let response = get_perm_storage_pricing(path).await.unwrap();
        let result = response.into_body();
        let body = to_bytes(result).await.unwrap();
        let result = String::from_utf8_lossy(&body);
        // The price/GiB / (1024 MiB/GiB * 4 chunks/MiB) = price/chunk
        assert_eq!("0.0005949332461698451".to_string(), result)
    }

    #[test]
    fn test_calc_perm_cost_per_gb_10_years() {
        let safe_minimum_number_of_years = 10;
        let annualized_decay_rate = 0.01;
        let res = calc_perm_cost_per_gb(safe_minimum_number_of_years, annualized_decay_rate).unwrap();
        assert_eq!(0.25678641965409, res)
    }

    #[test]
    fn test_calc_perm_cost_per_gb_200_years() {
        let safe_minimum_number_of_years = 200;
        let annualized_decay_rate = 0.01;
        let res = calc_perm_cost_per_gb(safe_minimum_number_of_years, annualized_decay_rate).unwrap();
        assert_eq!(2.3257381778716857, res)
    }

    #[test]
    fn test_get_perm_fee() {
        let res = calc_perm_fee_per_gb(2.33);
        assert_eq!(2.44110839844, res)
    }

    #[test]
    fn test_calc_chunks() {
        const ONE_KB: u128 = 1024;

        let number_of_bytes_to_store= 0;    // 0 B
        let res = get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(0, res);
        let number_of_bytes_to_store= 1;    // 1 B
        let res = get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(1, res);
        let number_of_bytes_to_store= 256 * ONE_KB;   // 256 KiB
        let res = get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(1, res);
        let number_of_bytes_to_store= 257 * ONE_KB;
        let res = get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(2, res);
        let number_of_bytes_to_store= 1_024 * ONE_KB;    // 1 MiB
        let res = get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(4, res);
        let number_of_bytes_to_store= 1_025 * ONE_KB;
        let res = get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(5, res);
        let number_of_bytes_to_store= 1_073_741_824;    // 1 GiB
        let res = get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(4096, res);
        let number_of_bytes_to_store= 1_073_741_825;    // 1 GiB + 1 B
        let res = get_chunks_from_bytes(number_of_bytes_to_store);
        assert_eq!(4097, res);
    }
}
