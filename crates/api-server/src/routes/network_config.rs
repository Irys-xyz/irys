use actix_web::{web, HttpResponse};
use irys_types::PublicStorageConfig;

use crate::ApiState;

pub async fn get_network_config(state: web::Data<ApiState>) -> HttpResponse {
    HttpResponse::Ok().json(PublicStorageConfig::from(
        state.chunk_provider.storage_config.clone(),
    ))
}
