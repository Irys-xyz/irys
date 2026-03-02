use actix_web::{http::header::ContentType, web, HttpResponse};
use irys_types::canonical::Canonical;

use crate::ApiState;

pub async fn get_config(state: web::Data<ApiState>) -> HttpResponse {
    HttpResponse::Ok()
        .content_type(ContentType::json())
        .json(Canonical(state.config.consensus.clone()))
}
