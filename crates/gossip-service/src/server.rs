use crate::types::InternalGossipError;
use crate::{
    cache::GossipCache,
    types::{GossipError, GossipResult},
    PeerListProvider,
};
use actix_web::dev::Server;
use actix_web::{
    middleware,
    web::{self, Data},
    App, HttpResponse, HttpServer,
};
use irys_types::GossipData;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct GossipServer {
    cache: Arc<GossipCache>,
    message_tx: mpsc::Sender<(SocketAddr, GossipData)>,
    peer_list: PeerListProvider,
}

impl GossipServer {
    pub fn new(
        cache: Arc<GossipCache>,
        message_tx: mpsc::Sender<(SocketAddr, GossipData)>,
        peer_list: PeerListProvider,
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
        .map_err(|e| GossipError::Internal(InternalGossipError::Unknown(e.to_string())))?
        .run())
    }
}

async fn handle_gossip_data(
    server: Data<Arc<GossipServer>>,
    data: web::Json<GossipData>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    tracing::debug!("Gossip data received: {:?}", data);
    let peer_address = match req.peer_addr() {
        Some(addr) => addr,
        None => {
            tracing::debug!("Failed to get peer address from gossip post request");
            return HttpResponse::BadRequest().finish();
        }
    };

    tracing::debug!("Gossip origin address is {}", peer_address);
    // Check if peer is allowed
    match server.peer_list.is_peer_allowed(&peer_address) {
        Ok(is_peer_allowed) => {
            if !is_peer_allowed {
                tracing::debug!("Peer address is not allowed");
                return HttpResponse::Forbidden().finish();
            }
        }
        Err(e) => {
            tracing::error!("Failed to check if peer is allowed: {}", e);
            return HttpResponse::InternalServerError().finish();
        }
    }

    // Record that this peer has seen this data
    if let Err(e) = server.cache.record_seen(peer_address, &data) {
        tracing::error!("Failed to record data in cache: {}", e);
        return HttpResponse::InternalServerError().finish();
    }

    // Forward the data to the message bus
    if let Err(e) = server
        .message_tx
        .send((peer_address, data.into_inner()))
        .await
    {
        tracing::error!("Failed to forward data to message bus: {}", e);
        return HttpResponse::InternalServerError().finish();
    }

    tracing::debug!("Gossip data sent");
    HttpResponse::Ok().finish()
}

async fn handle_health_check(
    server: Data<Arc<GossipServer>>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let peer_addr = match req.peer_addr() {
        Some(addr) => addr,
        None => return HttpResponse::BadRequest().finish(),
    };

    match server.peer_list.get_peer_info(&peer_addr) {
        Ok(info) => match info {
            Some(info) => HttpResponse::Ok().json(info),
            None => HttpResponse::NotFound().finish(),
        },
        Err(_) => HttpResponse::InternalServerError().finish(),
    }
}
