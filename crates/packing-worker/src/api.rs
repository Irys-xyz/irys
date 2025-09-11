use actix_web::{
    dev::{HttpServiceFactory, Server},
    error::InternalError,
    middleware,
    web::{self, Json, JsonConfig},
    App, HttpResponse, HttpServer,
};
use futures::StreamExt as _;
use irys_packing::{PackingType, PACKING_TYPE};
use serde::{Deserialize, Serialize};

use std::net::{SocketAddr, TcpListener};
use tracing::{debug, info};

use crate::worker::{PackingWorkerState, RemotePackingRequest};

pub fn routes() -> impl HttpServiceFactory {
    web::scope("v1")
        .wrap(middleware::Logger::default())
        .route("/pack", web::post().to(process_packing_job))
        .route("/", web::get().to(info_route))
}

pub fn run_server(state: PackingWorkerState, listener: TcpListener) -> Server {
    let port = listener.local_addr().expect("listener to work").port();
    info!(?port, "Starting API server");

    HttpServer::new(move || {
        App::new()
            .wrap(middleware::Logger::new("%r %s %D ms"))
            .app_data(state.clone())
            .app_data(
                JsonConfig::default()
                    .limit(1024 * 1024) // Set JSON payload limit to 1MB
                    .error_handler(|err, req| {
                        debug!("JSON decode error for req {:?} - {:?}", &req.path(), &err);
                        InternalError::from_response(err, HttpResponse::BadRequest().finish())
                            .into()
                    }),
            )
            .service(routes())
        // .wrap(Cors::permissive())
    })
    .shutdown_timeout(5)
    // .keep_alive(actix_web::http::KeepAlive::Os)
    .listen(listener)
    .unwrap()
    .run()
}

// Adapted from /actix-web-4.9.0/src/server.rs create_listener
// This is required as we need to access the TcpListener directly to figure out what port we've been assigned
// if randomisation (requested port 0) is used.
pub fn create_listener(addr: SocketAddr) -> eyre::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let backlog = 1024;
    let domain = Domain::for_address(addr);
    let socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    // need this so application restarts can pick back up the same port without suffering from time-wait
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    // clamp backlog to max u32 that fits in i32 range
    let backlog = core::cmp::min(backlog, i32::MAX as u32) as i32;
    socket.listen(backlog)?;
    let listener = TcpListener::from(socket);
    Ok(listener)
}

pub async fn process_packing_job(
    state: web::Data<PackingWorkerState>,
    body: Json<RemotePackingRequest>,
) -> actix_web::Result<HttpResponse> {
    // note: we don't restrict the max number of simultaneous requests, as we yield packing in a stream
    match state.pack(body.0) {
        Ok(stream) => {
            let mstream = stream.map(|r| r.map(std::convert::Into::into));

            Ok(HttpResponse::Ok().streaming(mstream))
        }
        Err(e) => Ok(HttpResponse::InternalServerError().body(e.to_string())),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackingWorkerInfo {
    available_capacity: usize,
    packing_type: PackingType,
}

pub async fn info_route(
    state: web::Data<PackingWorkerState>,
) -> actix_web::Result<Json<PackingWorkerInfo>> {
    Ok(web::Json(PackingWorkerInfo {
        available_capacity: state.0.packing_semaphore.available_permits(),
        packing_type: PACKING_TYPE,
    }))
}
