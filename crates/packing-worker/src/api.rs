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
                    .limit(1024 * 1024)
                    .error_handler(|err, req| {
                        debug!("JSON decode error for req {:?} - {:?}", &req.path(), &err);
                        InternalError::from_response(err, HttpResponse::BadRequest().finish())
                            .into()
                    }),
            )
            .route("/", web::get().to(|| async { Redirect::to("/v1/info") }))
            .service(routes())
    })
    .shutdown_timeout(5)
    .listen(listener)
    .unwrap()
    .run()
}

pub async fn process_packing_job(
    state: web::Data<PackingWorkerState>,
    body: Json<RemotePackingRequest>,
) -> actix_web::Result<HttpResponse> {
    match state.pack(body.0) {
        Ok(stream) => {
            let mstream = stream.map(|r| r.map(std::convert::Into::into));

            Ok(HttpResponse::Ok().streaming(mstream))
        }
        Err(e) => Ok(HttpResponse::InternalServerError().body(e.to_string())),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    fn packing_type_strategy() -> impl Strategy<Value = PackingType> {
        prop_oneof![
            Just(PackingType::CPU),
            Just(PackingType::CUDA),
            Just(PackingType::AMD),
        ]
    }

    proptest! {
        #[test]
        fn prop_packing_worker_info_serde_roundtrip(
            capacity in any::<usize>(),
            packing_type in packing_type_strategy(),
        ) {
            let info = PackingWorkerInfo {
                available_capacity: capacity,
                packing_type,
            };
            let json = serde_json::to_string(&info).unwrap();
            let deserialized: PackingWorkerInfo = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(info, deserialized);
        }
    }

    #[rstest]
    #[case(
        0,
        PackingType::CPU,
        r#"{"available_capacity":0,"packing_type":"CPU"}"#
    )]
    #[case(
        42,
        PackingType::CUDA,
        r#"{"available_capacity":42,"packing_type":"CUDA"}"#
    )]
    #[case(usize::MAX, PackingType::AMD, &format!(r#"{{"available_capacity":{},"packing_type":"AMD"}}"#, usize::MAX))]
    fn test_info_json_format(
        #[case] capacity: usize,
        #[case] packing_type: PackingType,
        #[case] expected_json: &str,
    ) {
        let info = PackingWorkerInfo {
            available_capacity: capacity,
            packing_type,
        };
        let json = serde_json::to_string(&info).unwrap();
        assert_eq!(json, expected_json);
    }

    #[rstest]
    #[case(PackingType::CPU, "CPU")]
    #[case(PackingType::CUDA, "CUDA")]
    #[case(PackingType::AMD, "AMD")]
    fn test_info_deserialize_from_known_json(
        #[case] expected_type: PackingType,
        #[case] type_str: &str,
    ) {
        let json = format!(
            r#"{{"available_capacity":10,"packing_type":"{}"}}"#,
            type_str
        );
        let info: PackingWorkerInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info.available_capacity, 10);
        assert_eq!(info.packing_type, expected_type);
    }
}
