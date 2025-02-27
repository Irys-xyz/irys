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
        Ok(Ledger::Submit) => Ok(HttpResponse::BadRequest().body("term not yet implemented")),
        Err(_) => Ok(HttpResponse::BadRequest().body("Ledger type not supported")),
    }
}
