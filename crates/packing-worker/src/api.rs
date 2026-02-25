use actix_web::{
    App, HttpResponse, HttpServer,
    dev::{HttpServiceFactory, Server},
    error::InternalError,
    middleware,
    web::{self, Data, Json, JsonConfig, Redirect},
};
use futures::StreamExt as _;
use irys_packing::{PACKING_TYPE, PackingType};
use serde::{Deserialize, Serialize};

use std::net::TcpListener;
use tracing::{debug, info};

use crate::types::RemotePackingRequest;
use crate::worker::PackingWorkerState;

pub fn routes() -> impl HttpServiceFactory {
    web::scope("v1")
        .wrap(middleware::Logger::default())
        .route("/pack", web::post().to(process_packing_job))
        .route("/info", web::get().to(info_route))
}

pub fn run_server(state: PackingWorkerState, listener: TcpListener) -> Server {
    let port = listener.local_addr().expect("listener to work").port();
    info!(custom.port = ?port, "Starting API server");

    HttpServer::new(move || {
        App::new()
            .wrap(middleware::Logger::new("%r %s %D ms"))
            .app_data(Data::new(state.clone()))
            .app_data(
                JsonConfig::default()
                    .limit(1024 * 1024) // Set JSON payload limit to 1MB
                    .error_handler(|err, req| {
                        debug!("JSON decode error for req {:?} - {:?}", &req.path(), &err);
                        InternalError::from_response(err, HttpResponse::BadRequest().finish())
                            .into()
                    }),
            )
            // not a permanent redirect, so we can redirect to the highest API version
            .route("/", web::get().to(|| async { Redirect::to("/v1/info") }))
            .service(routes())
        // .wrap(Cors::permissive())
    })
    .shutdown_timeout(5)
    // .keep_alive(actix_web::http::KeepAlive::Os)
    .listen(listener)
    .unwrap()
    .run()
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
