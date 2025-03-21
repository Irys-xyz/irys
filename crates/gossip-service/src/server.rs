use std::{net::IpAddr, sync::Arc};

use actix_web::{
    middleware,
    web::{self, Data},
    App, HttpResponse, HttpServer,
};
use actix_web::dev::Server;
use tokio::sync::mpsc;

use crate::{
    cache::GossipCache,
    types::{GossipData, GossipError, GossipResult, PeerInfo},
};

pub struct GossipServer {
    cache: Arc<GossipCache>,
    message_tx: mpsc::Sender<(IpAddr, GossipData)>,
    peer_list: Arc<dyn PeerList>,
}

#[async_trait::async_trait]
pub trait PeerList: Send + Sync {
    async fn is_peer_allowed(&self, ip: &IpAddr) -> bool;
    async fn get_peer_info(&self, ip: &IpAddr) -> Option<PeerInfo>;
}

impl GossipServer {
    pub fn new(
        cache: Arc<GossipCache>,
        message_tx: mpsc::Sender<(IpAddr, GossipData)>,
        peer_list: Arc<dyn PeerList>,
    ) -> Self {
        Self {
            cache,
            message_tx,
            peer_list,
        }
    }

    pub fn run(self, bind_address: &str, port: u16) -> GossipResult<Server> {
        let server = Arc::new(self);

        Ok(HttpServer::new(move || {
            App::new()
                .app_data(Data::new(server.clone()))
                .wrap(middleware::Logger::default())
                .service(
                    web::scope("/gossip")
                        .route("/data", web::post().to(handle_gossip_data))
                        .route("/health", web::get().to(handle_health_check)),
                )
        })
        .bind((bind_address, port))
        .map_err(|e| GossipError::Internal(e.to_string()))?
        .run())
    }
}

async fn handle_gossip_data(
    server: Data<Arc<GossipServer>>,
    data: web::Json<GossipData>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let peer_ip = match req.peer_addr() {
        Some(addr) => addr.ip(),
        None => return HttpResponse::BadRequest().finish(),
    };

    // Check if peer is allowed
    if !server.peer_list.is_peer_allowed(&peer_ip).await {
        return HttpResponse::Forbidden().finish();
    }

    // Record that this peer has seen this data
    if let Err(e) = server.cache.record_seen(peer_ip, &data) {
        tracing::error!("Failed to record data in cache: {}", e);
        return HttpResponse::InternalServerError().finish();
    }

    // Forward the data to the message bus
    if let Err(e) = server.message_tx.send((peer_ip, data.into_inner())).await {
        tracing::error!("Failed to forward data to message bus: {}", e);
        return HttpResponse::InternalServerError().finish();
    }

    HttpResponse::Ok().finish()
}

async fn handle_health_check(
    server: Data<Arc<GossipServer>>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let peer_ip = match req.peer_addr() {
        Some(addr) => addr.ip(),
        None => return HttpResponse::BadRequest().finish(),
    };

    match server.peer_list.get_peer_info(&peer_ip).await {
        Some(info) => HttpResponse::Ok().json(info),
        None => HttpResponse::NotFound().finish(),
    }
} 