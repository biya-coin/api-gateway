use std::{future::Future, time::Duration};

use api_gateway::{config::Config, server};
use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use hyper_util::rt::TokioIo;
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

struct Service {
    url: String,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

impl Service {
    async fn gateway(config: Config) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server::serve(listener, config, async {
            let _ = stopped.await;
        }));
        Self {
            url,
            stop: Some(stop),
            task,
        }
    }

    async fn upstream(router: Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
        });
        Self {
            url,
            stop: Some(stop),
            task,
        }
    }

    async fn shutdown(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        timeout(Duration::from_secs(2), &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

fn config(upstream: &Service) -> Config {
    let mut config = Config::parse(include_str!("../config/default.toml")).unwrap();
    config.upstreams.indexer_ws = Some(upstream.url.replace("http:", "ws:").parse().unwrap());
    config
        .upstreams
        .indexer_ws
        .as_mut()
        .unwrap()
        .set_path("/ws");
    config
}

fn request(service: &Service) -> reqwest::RequestBuilder {
    reqwest::Client::builder()
        .no_proxy()
        .http1_only()
        .build()
        .unwrap()
        .get(format!("{}/ws", service.url))
        .header("connection", "Upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", KEY)
}

fn accept(key: &str) -> String {
    STANDARD.encode(Sha1::digest(format!(
        "{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
    )))
}

fn upgrade_response() -> Response {
    Response::builder()
        .status(101)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-accept", accept(KEY))
        .body(Body::empty())
        .unwrap()
}

async fn bounded(test: impl Future<Output = ()>) {
    timeout(Duration::from_secs(8), test)
        .await
        .expect("WebSocket test timed out");
}

// Minimal frame fixture encoder, not a production WebSocket implementation.
fn frame(opcode: u8, payload: &[u8], masked: bool) -> Vec<u8> {
    assert!(payload.len() < 126);
    let mut result = vec![
        0x80 | opcode,
        payload.len() as u8 | if masked { 0x80 } else { 0 },
    ];
    if masked {
        let mask = [1, 2, 3, 4];
        result.extend_from_slice(&mask);
        result.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
    } else {
        result.extend_from_slice(payload);
    }
    result
}

#[tokio::test]
async fn tunnel_preserves_handshake_frames_and_close_in_both_directions() {
    bounded(async {
        let (seen, mut received) = mpsc::unbounded_channel::<(HeaderMap, String)>();
        let (finished, mut finishes) = mpsc::unbounded_channel();
        let client_frames = [
            frame(
                1,
                br#"{"method":"subscribe","subscription":{"type":"assetCtxs","dex":""}}"#,
                true,
            ),
            frame(
                1,
                br#"{"method":"unsubscribe","subscription":{"type":"assetCtxs","dex":""}}"#,
                true,
            ),
            frame(1, br#"{"method":"ping"}"#, true),
            frame(2, &[0, 255, 42], true),
            frame(9, b"ping", true),
            frame(10, b"pong", true),
        ]
        .concat();
        let server_frames = [
            frame(1, br#"{"channel":"pong"}"#, false),
            frame(2, &[255, 0, 128], false),
            frame(9, b"server-ping", false),
            frame(10, b"pong", false),
            // RSV1 / compressed opaque payload must not be decoded or remasked by the tunnel.
            vec![0xc1, 2, 0x02, 0x00],
            frame(8, &[0x03, 0xe8], false),
        ]
        .concat();
        let expected = client_frames.clone();
        let output = server_frames.clone();
        let upstream = Service::upstream(Router::new().route(
            "/ws",
            get(move |mut req: Request| {
                let seen = seen.clone();
                let expected = expected.clone();
                let output = output.clone();
                let finished = finished.clone();
                async move {
                    seen.send((req.headers().clone(), req.uri().to_string()))
                        .unwrap();
                    let on_upgrade = hyper::upgrade::on(&mut req);
                    tokio::spawn(async move {
                        let mut socket = TokioIo::new(on_upgrade.await.unwrap());
                        let mut buffer = vec![0; expected.len()];
                        socket.read_exact(&mut buffer).await.unwrap();
                        assert_eq!(buffer, expected);
                        socket.write_all(&output).await.unwrap();
                        let close = frame(8, &[0x03, 0xe8], true);
                        let mut got_close = vec![0; close.len()];
                        socket.read_exact(&mut got_close).await.unwrap();
                        assert_eq!(got_close, close);
                        socket.shutdown().await.unwrap();
                        finished.send(()).unwrap();
                    });
                    let mut response = upgrade_response();
                    response
                        .headers_mut()
                        .insert("sec-websocket-protocol", HeaderValue::from_static("hl"));
                    response.headers_mut().insert(
                        "sec-websocket-extensions",
                        HeaderValue::from_static("permessage-deflate"),
                    );
                    response
                }
            }),
        ))
        .await;
        let gateway = Service::gateway(config(&upstream)).await;
        let response = request(&gateway)
            .query(&[("session", "test")])
            .header("authorization", "Bearer local-test")
            .header("sec-websocket-protocol", "hl, other")
            .header("sec-websocket-extensions", "permessage-deflate")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
        assert_eq!(response.headers()["sec-websocket-protocol"], "hl");
        assert_eq!(
            response.headers()["sec-websocket-extensions"],
            "permessage-deflate"
        );
        let (headers, uri) = received.recv().await.unwrap();
        assert_eq!(headers["sec-websocket-key"], KEY);
        assert_eq!(headers["authorization"], "Bearer local-test");
        assert_eq!(uri, "/ws?session=test");
        let mut connection = response.upgrade().await.unwrap();
        connection.write_all(&client_frames).await.unwrap();
        let mut response_frames = vec![0; server_frames.len()];
        connection.read_exact(&mut response_frames).await.unwrap();
        assert_eq!(response_frames, server_frames);
        connection
            .write_all(&frame(8, &[0x03, 0xe8], true))
            .await
            .unwrap();
        finishes.recv().await.unwrap();
        assert_eq!(connection.read(&mut [0; 1]).await.unwrap(), 0);
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn invalid_handshake_missing_backend_and_origin_are_rejected() {
    bounded(async {
        let mut cfg = Config::parse(include_str!("../config/default.toml")).unwrap();
        cfg.access.allowed_origins = vec!["https://dex.example".into()];
        let gateway = Service::gateway(cfg).await;
        assert_eq!(request(&gateway).send().await.unwrap().status(), 503);
        for (header, value, status) in [
            ("sec-websocket-key", "bad", 400),
            ("sec-websocket-version", "12", 400),
            ("origin", "https://evil.example", 403),
            ("origin", "https://dex.example", 503),
        ] {
            assert_eq!(
                request(&gateway)
                    .header(header, value)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                status
            );
        }
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn upstream_handshake_rejection_is_preserved_without_upgrade() {
    bounded(async {
        let upstream = Service::upstream(Router::new().route(
            "/ws",
            get(|| async {
                Response::builder()
                    .status(429)
                    .header("retry-after", "7")
                    .header("content-type", "text/plain")
                    .body(Body::from("busy-indexer"))
                    .unwrap()
            }),
        ))
        .await;
        let gateway = Service::gateway(config(&upstream)).await;
        let response = request(&gateway).send().await.unwrap();
        assert_eq!(response.status(), 429);
        assert_eq!(response.headers()["retry-after"], "7");
        assert_eq!(response.text().await.unwrap(), "busy-indexer");
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn invalid_upstream_accept_is_a_bad_gateway() {
    bounded(async {
        let upstream = Service::upstream(Router::new().route(
            "/ws",
            get(|| async {
                let mut response = upgrade_response();
                response
                    .headers_mut()
                    .insert("sec-websocket-accept", HeaderValue::from_static("wrong"));
                response
            }),
        ))
        .await;
        let gateway = Service::gateway(config(&upstream)).await;
        assert_eq!(request(&gateway).send().await.unwrap().status(), 502);
        gateway.shutdown().await;
    })
    .await;
}
#[tokio::test]
async fn handshake_timeout_is_504() {
    bounded(async {
        let upstream = Service::upstream(Router::new().route(
            "/ws",
            get(|| async { std::future::pending::<Response>().await }),
        ))
        .await;
        let mut cfg = config(&upstream);
        cfg.websocket.handshake_timeout_ms = 30;
        let gateway = Service::gateway(cfg).await;
        assert_eq!(request(&gateway).send().await.unwrap().status(), 504);
        gateway.shutdown().await;
    })
    .await;
}

async fn quiet_upstream() -> (Service, mpsc::UnboundedReceiver<()>) {
    let (closed, receiver) = mpsc::unbounded_channel();
    let upstream = Service::upstream(Router::new().route(
        "/ws",
        get(move |mut req: Request| {
            let closed = closed.clone();
            async move {
                let upgraded = hyper::upgrade::on(&mut req);
                tokio::spawn(async move {
                    let mut socket = TokioIo::new(upgraded.await.unwrap());
                    let mut byte = [0; 1];
                    while socket.read(&mut byte).await.unwrap_or(0) != 0 {}
                    let _ = closed.send(());
                });
                upgrade_response()
            }
        }),
    ))
    .await;
    (upstream, receiver)
}

#[tokio::test]
async fn connection_limit_and_shutdown_close_live_tunnels() {
    bounded(async {
        let (upstream, mut closed) = quiet_upstream().await;
        let mut cfg = config(&upstream);
        cfg.websocket.max_connections = 1;
        let gateway = Service::gateway(cfg).await;
        let response = request(&gateway).send().await.unwrap();
        assert_eq!(response.status(), 101);
        let mut socket = response.upgrade().await.unwrap();
        assert_eq!(request(&gateway).send().await.unwrap().status(), 503);
        gateway.shutdown().await;
        assert_eq!(socket.read(&mut [0; 1]).await.unwrap(), 0);
        closed.recv().await.unwrap();
    })
    .await;
}

#[tokio::test]
async fn idle_timeout_is_optional_and_closes_both_transports_when_enabled() {
    bounded(async {
        let (upstream, mut closed) = quiet_upstream().await;
        let mut cfg = config(&upstream);
        assert_eq!(cfg.websocket.idle_timeout_ms, 0);
        cfg.websocket.idle_timeout_ms = 30;
        let gateway = Service::gateway(cfg).await;
        let mut socket = request(&gateway)
            .send()
            .await
            .unwrap()
            .upgrade()
            .await
            .unwrap();
        assert_eq!(socket.read(&mut [0; 1]).await.unwrap(), 0);
        closed.recv().await.unwrap();
        // Expired tunnels return their concurrency permit.
        assert_eq!(request(&gateway).send().await.unwrap().status(), 101);
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn head_cannot_initiate_an_upstream_websocket() {
    bounded(async {
        let gateway =
            Service::gateway(Config::parse(include_str!("../config/default.toml")).unwrap()).await;
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .head(format!("{}/ws", gateway.url))
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", KEY)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        gateway.shutdown().await;
    })
    .await;
}
