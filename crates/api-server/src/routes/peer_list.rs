use actix_web::HttpResponse;

pub async fn peer_list_route() -> HttpResponse {
    HttpResponse::Ok().body("Peer List")
}
