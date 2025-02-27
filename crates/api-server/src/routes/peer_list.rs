use crate::ApiState;
use actix_web::{web, HttpResponse};
use serde_json::to_string;

pub async fn peer_list_route(state: web::Data<ApiState>) -> HttpResponse {
    // Fetch the list of known peers
    let ips_result = state.get_known_peers();

    // Handle the Result from get_known_peers
    let ips = match ips_result {
        Ok(ips) => ips,
        Err(e) => {
            return HttpResponse::InternalServerError()
                .body(format!("Failed to fetch peers: {}", e));
        }
    };

    // Serialize IPs to JSON and return as HTTP response
    match to_string(&ips) {
        Ok(json_body) => HttpResponse::Ok()
            .content_type("application/json")
            .body(json_body),
        Err(e) => HttpResponse::InternalServerError().body(format!("Serialization error: {}", e)),
    }
}
