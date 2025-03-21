use actix_web::{http::header::ContentType, web, HttpResponse};
use irys_types::PublicStorageConfig;

use crate::ApiState;

pub async fn get_network_config(state: web::Data<ApiState>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(ContentType::json())
        .json(PublicStorageConfig::from(
            state.chunk_provider.storage_config.clone(),
        ))
}
