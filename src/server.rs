use std::{future::Future, io, sync::Arc, time::Duration};

use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use tokio::{net::TcpListener, sync::Semaphore};
use tokio_util::{sync::CancellationToken, task::TaskTracker};
use tower_http::cors::{AllowHeaders, AllowOrigin, CorsLayer};

use crate::{config::Config, error::GatewayError, http_proxy, websocket};

pub(crate) struct Gateway {
    pub(crate) config: Config,
    pub(crate) client: reqwest::Client,
    pub(crate) http_slots: Arc<Semaphore>,
    pub(crate) ws_slots: Arc<Semaphore>,
    pub(crate) stopping: CancellationToken,
    pub(crate) tasks: TaskTracker,
}

impl Gateway {
    fn new(config: Config) -> anyhow::Result<Arc<Self>> {
        config.validate()?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(config.http.connect_timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .http1_only()
            .build()?;
        Ok(Arc::new(Self {
            http_slots: Arc::new(Semaphore::new(config.http.max_in_flight)),
            ws_slots: Arc::new(Semaphore::new(config.websocket.max_connections)),
            config,
            client,
            stopping: CancellationToken::new(),
            tasks: TaskTracker::new(),
        }))
    }
}

pub fn router(config: Config) -> anyhow::Result<Router> {
    Ok(build_router(Gateway::new(config)?))
}

fn build_router(gateway: Arc<Gateway>) -> Router {
    let origins = &gateway.config.access.allowed_origins;
    let allow_origin = if origins.iter().any(|origin| origin == "*") {
        AllowOrigin::any()
    } else {
        AllowOrigin::list(
            origins
                .iter()
                .map(|origin| origin.parse::<HeaderValue>().expect("validated origin")),
        )
    };
    let cors = CorsLayer::new()
        .allow_origin(allow_origin)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers(AllowHeaders::mirror_request())
        .expose_headers([header::RETRY_AFTER]);
    Router::new()
        // Liveness is not a promise that any downstream is ready.
        .route("/healthz", get(|| async { "ok" }))
        .route("/info", post(http_proxy::info))
        .route("/exchange", post(http_proxy::exchange))
        .route("/ws", get(websocket::proxy))
        .layer(cors)
        .layer(middleware::from_fn_with_state(gateway.clone(), access))
        .with_state(gateway)
}

async fn access(State(gateway): State<Arc<Gateway>>, request: Request, next: Next) -> Response {
    if gateway.stopping.is_cancelled() {
        return GatewayError(StatusCode::SERVICE_UNAVAILABLE, "gateway_stopping").into_response();
    }
    let origins = request.headers().get_all(header::ORIGIN);
    if origins.iter().count() > 1 {
        return GatewayError(StatusCode::FORBIDDEN, "origin_not_allowed").into_response();
    }
    if let Some(origin) = origins.iter().next() {
        let allowed = origin.to_str().ok().is_some_and(|value| {
            gateway
                .config
                .access
                .allowed_origins
                .iter()
                .any(|origin| origin == "*" || origin == value)
        });
        if !allowed {
            return GatewayError(StatusCode::FORBIDDEN, "origin_not_allowed").into_response();
        }
    }
    next.run(request).await
}

pub async fn serve(
    listener: TcpListener,
    config: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let gateway = Gateway::new(config).map_err(io::Error::other)?;
    let transport_slots = Arc::new(Semaphore::new(gateway.config.http.max_connections));
    let grace = Duration::from_millis(gateway.config.server.shutdown_timeout_ms);
    let app = build_router(gateway.clone());
    let mut connections = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break Ok(()),
            // Reap before accepting: a permanently readable listener must not retain
            // completed JoinSet entries indefinitely under short-connection load.
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if result.is_err() { tracing::warn!("HTTP connection task failed"); }
            },
            accepted = listener.accept() => {
                let (stream, address) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => break Err(error),
                };
                let Ok(permit) = transport_slots.clone().try_acquire_owned() else {
                    // Transport cap is enforced before parsing a request; close the socket.
                    drop(stream);
                    continue;
                };
                let app = app.clone();
                let stopping = gateway.stopping.clone();
                let write_timeout = Duration::from_millis(gateway.config.http.write_timeout_ms);
                let header_timeout = Duration::from_millis(gateway.config.http.body_timeout_ms);
                connections.spawn(async move {
                    use tower::ServiceExt;
                    let service = hyper::service::service_fn(move |mut request: hyper::Request<hyper::body::Incoming>| {
                        request.extensions_mut().insert(axum::extract::ConnectInfo(address));
                        app.clone().oneshot(request.map(axum::body::Body::new))
                    });
                    // The transport permit follows the socket into upgraded WS connections.
                    let io = crate::transport::WriteDeadline::new(stream, write_timeout, permit);
                    let mut builder = hyper::server::conn::http1::Builder::new();
                    builder.timer(hyper_util::rt::TokioTimer::new())
                        .header_read_timeout(header_timeout).max_buf_size(32 * 1024)
                        // Keep response Bytes ownership until the socket consumes it.
                        .writev(true);
                    let connection = builder.serve_connection(hyper_util::rt::TokioIo::new(io), service).with_upgrades();
                    tokio::pin!(connection);
                    tokio::select! {
                        result = &mut connection => { if result.is_err() { tracing::debug!("HTTP connection ended with an I/O error"); } },
                        () = stopping.cancelled() => {
                            connection.as_mut().graceful_shutdown();
                            let _ = connection.await;
                        },
                    }
                });
            },
        }
    };
    drop(listener);
    gateway.stopping.cancel();
    let drain = async { while connections.join_next().await.is_some() {} };
    if tokio::time::timeout(grace, drain).await.is_err() {
        tracing::warn!("HTTP drain deadline reached; closing remaining connections");
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    // No HTTP handler can register another upgrade task after all connections exit.
    gateway.tasks.close();
    gateway.tasks.wait().await;
    result
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
        sync::oneshot,
        time::{timeout, Duration},
    };
    use tower::ServiceExt;

    use super::*;

    fn config() -> Config {
        Config::parse(include_str!("../config/default.toml")).unwrap()
    }

    #[tokio::test]
    async fn health_check_reports_liveness() {
        let response = router(config())
            .unwrap()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(to_bytes(response.into_body(), 16).await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn unconfigured_exchange_is_unavailable_and_unknown_paths_stay_missing() {
        for (method, uri, status) in [
            ("POST", "/exchange", StatusCode::SERVICE_UNAVAILABLE),
            ("GET", "/missing", StatusCode::NOT_FOUND),
        ] {
            let response = router(config())
                .unwrap()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{method} {uri}");
        }
    }

    #[tokio::test]
    async fn serves_http_and_shuts_down() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let server = serve(listener, config(), async {
            let _ = stopped.await;
        });
        let client = async {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream
                .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
            assert!(response.ends_with("\r\n\r\nok"), "{response}");
            stop.send(()).unwrap();
        };
        timeout(Duration::from_secs(5), async {
            let (result, ()) = tokio::join!(server, client);
            result.unwrap();
        })
        .await
        .expect("HTTP lifecycle test timed out");
    }
}
