use crate::{error::ApiError, ApiState};
use actix_web::{http::header::ContentType, web, HttpResponse, ResponseError as _};
use awc::http::StatusCode;
use serde_json::to_string;

pub async fn peer_list_route(state: web::Data<ApiState>) -> HttpResponse {
    // Fetch the list of known peers
    let ips = state.get_known_peers();

    // Serialize IPs to JSON and return as HTTP response
    match to_string(&ips) {
        Ok(json_body) => HttpResponse::Ok()
            .content_type(ContentType::json())
            .body(json_body),
        Err(e) => ApiError::CustomWithStatus(
            format!("Serialization error: {e}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
        .error_response(),
    }
}
