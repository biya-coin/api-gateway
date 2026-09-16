use std::{future::Future, time::Duration};

use api_gateway::{config::Config, server};
use axum::{
    body::Body,
    extract::Request,
    http::{HeaderMap, HeaderValue},
    response::Response,
    routing::get,
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
    time::timeout,
};
use tokio_tungstenite::{
    tungstenite::{
        protocol::{frame::coding::CloseCode, CloseFrame, Role},
        Message,
    },
    WebSocketStream,
};
use tokio_util::sync::CancellationToken;

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
type Client = WebSocketStream<reqwest::Upgraded>;

struct Service {
    url: String,
    stop: CancellationToken,
    task: JoinHandle<std::io::Result<()>>,
}

impl Drop for Service {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
    }
}

impl Service {
    async fn gateway(config: Config) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = CancellationToken::new();
        let task = tokio::spawn(server::serve(
            listener,
            config,
            stop.clone().cancelled_owned(),
        ));
        Self { url, stop, task }
    }

    async fn upstream(router: Router, stop: CancellationToken) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let shutdown = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });
        Self { url, stop, task }
    }

    fn endpoint(&self) -> url::Url {
        format!("{}/ws", self.url.replace("http:", "ws:"))
            .parse()
            .unwrap()
    }

    async fn shutdown(mut self) {
        self.stop.cancel();
        timeout(Duration::from_secs(2), &mut self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

struct Connection {
    headers: HeaderMap,
    uri: String,
    push: mpsc::Sender<Message>,
}

struct Mock {
    service: Service,
    connections: mpsc::UnboundedReceiver<Connection>,
    messages: mpsc::UnboundedReceiver<Message>,
    closed: mpsc::UnboundedReceiver<()>,
}

impl Mock {
    async fn start() -> Self {
        let (opened, connections) = mpsc::unbounded_channel();
        let (messages_tx, messages) = mpsc::unbounded_channel();
        let (closed_tx, closed) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let socket_stop = stop.clone();
        let router = Router::new().route(
            "/ws",
            get(move |mut req: Request| {
                let response = upgrade_response(&req);
                let (push, mut pushes) = mpsc::channel::<Message>(8);
                opened
                    .send(Connection {
                        headers: req.headers().clone(),
                        uri: req.uri().to_string(),
                        push,
                    })
                    .unwrap();
                let upgrade = hyper::upgrade::on(&mut req);
                let messages = messages_tx.clone();
                let closed = closed_tx.clone();
                let stop = socket_stop.clone();
                async move {
                    tokio::spawn(async move {
                        let upgraded = tokio::select! {
                            () = stop.cancelled() => return,
                            result = upgrade => result.unwrap(),
                        };
                        let mut ws = WebSocketStream::from_raw_socket(
                            TokioIo::new(upgraded),
                            Role::Server,
                            None,
                        )
                        .await;
                        loop {
                            tokio::select! {
                                () = stop.cancelled() => break,
                                push = pushes.recv() => if let Some(push) = push {
                                    if ws.send(push).await.is_err() { break; }
                                } else { break; },
                                incoming = ws.next() => match incoming {
                                    Some(Ok(message)) => {
                                        let is_close = message.is_close();
                                        let _ = messages.send(message.clone());
                                        if message.is_text() || message.is_binary() {
                                            if ws.send(message).await.is_err() { break; }
                                        } else {
                                            let _ = ws.flush().await;
                                        }
                                        if is_close { break; }
                                    }
                                    _ => break,
                                },
                            }
                        }
                        let _ = closed.send(());
                    });
                    response
                }
            }),
        );
        let service = Service::upstream(router, stop).await;
        Self {
            service,
            connections,
            messages,
            closed,
        }
    }

    fn assert_no_messages(&mut self) {
        assert!(self.messages.try_recv().is_err());
    }
}

fn config() -> Config {
    let mut config = Config::parse(include_str!("../config/default.toml")).unwrap();
    config.upstreams = Default::default();
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

async fn connect(service: &Service) -> Client {
    let response = request(service).send().await.unwrap();
    assert_eq!(response.status(), 101);
    WebSocketStream::from_raw_socket(response.upgrade().await.unwrap(), Role::Client, None).await
}

fn upgrade_response(request: &Request) -> Response {
    let key = request.headers()["sec-websocket-key"].to_str().unwrap();
    let accept = STANDARD.encode(Sha1::digest(format!(
        "{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
    )));
    Response::builder()
        .status(101)
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-accept", accept)
        .body(Body::empty())
        .unwrap()
}

async fn bounded(test: impl Future<Output = ()>) {
    timeout(Duration::from_secs(10), test)
        .await
        .expect("WebSocket test timed out");
}

fn subscription(method: &str, kind: &str) -> Message {
    Message::text(format!(
        r#"{{"method":"{method}","subscription":{{"type":"{kind}","user":"0x1111111111111111111111111111111111111111","coin":"BTC","dex":""}},"unknown":{{"preserve":"原样"}}}}"#
    ))
}

async fn receive(client: &mut Client) -> Message {
    client.next().await.unwrap().unwrap()
}

async fn assert_error(client: &mut Client, code: &str, backend: &str) {
    let msg = receive(client).await;
    let value: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
    assert_eq!(value["channel"], "error");
    assert_eq!(value["data"]["error"], code);
    assert_eq!(value["data"]["backend"], backend);
}

#[tokio::test]
async fn subscriptions_and_unsubscriptions_have_one_owner_on_the_same_connection() {
    bounded(async {
        let mut indexer = Mock::start().await;
        let mut state = Mock::start().await;
        let mut cfg = config();
        cfg.upstreams.indexer_ws = Some(indexer.service.endpoint());
        cfg.upstreams.state_ws = Some(state.service.endpoint());
        let gateway = Service::gateway(cfg).await;
        let response = request(&gateway)
            .query(&[("session", "test")])
            .header("authorization", "Bearer local-test")
            .header("x-forwarded-for", "203.0.113.99")
            .header("x-real-ip", "203.0.113.99")
            .header("connection", "upgrade, x-strip")
            .header("x-strip", "must disappear")
            .header("sec-websocket-protocol", "hl, other")
            .header("sec-websocket-extensions", "permessage-deflate")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 101);
        assert!(!response.headers().contains_key("sec-websocket-protocol"));
        assert!(!response.headers().contains_key("sec-websocket-extensions"));
        let mut client =
            WebSocketStream::from_raw_socket(response.upgrade().await.unwrap(), Role::Client, None)
                .await;
        assert!(indexer.connections.try_recv().is_err());
        assert!(state.connections.try_recv().is_err());
        for method in ["subscribe", "unsubscribe"] {
            for kind in [
                "assetCtxs",
                "clearinghouseState",
                "l2Book",
                "activeAssetCtx",
                "openOrders",
                "allMids",
                "futureType",
            ] {
                let message = subscription(method, kind);
                client.send(message.clone()).await.unwrap();
                assert_eq!(receive(&mut client).await, message);
                let owner = if matches!(kind, "assetCtxs" | "clearinghouseState") {
                    &mut state
                } else {
                    &mut indexer
                };
                assert_eq!(owner.messages.recv().await.unwrap(), message);
                indexer.assert_no_messages();
                state.assert_no_messages();
            }
        }
        let mut live_connections = Vec::new();
        for backend in [&mut indexer, &mut state] {
            let connected = backend.connections.recv().await.unwrap();
            assert_eq!(connected.uri, "/ws?session=test");
            assert_eq!(connected.headers["authorization"], "Bearer local-test");
            assert_eq!(connected.headers["x-forwarded-for"], "127.0.0.1");
            assert!(!connected.headers.contains_key("x-real-ip"));
            assert!(!connected.headers.contains_key("x-strip"));
            assert!(!connected.headers.contains_key("sec-websocket-extensions"));
            assert!(!connected.headers.contains_key("sec-websocket-protocol"));
            assert!(backend.connections.try_recv().is_err());
            for message in [
                Message::text(r#"{"channel":"subscriptionResponse","data":{"verbatim":true}}"#),
                Message::text(
                    r#"{"channel":"clearinghouseState","data":{"accountValue":"12.345"}}"#,
                ),
                Message::Binary(vec![255, 0, 128].into()),
            ] {
                connected.push.send(message.clone()).await.unwrap();
                assert_eq!(receive(&mut client).await, message);
            }
            live_connections.push(connected);
        }
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn json_and_rfc_heartbeats_are_not_duplicated_or_misrouted() {
    bounded(async {
        let mut indexer = Mock::start().await;
        let mut state = Mock::start().await;
        let mut cfg = config();
        cfg.upstreams.indexer_ws = Some(indexer.service.endpoint());
        cfg.upstreams.state_ws = Some(state.service.endpoint());
        let gateway = Service::gateway(cfg).await;
        let mut client = connect(&gateway).await;
        client
            .send(Message::text(r#"{"method":"ping"}"#))
            .await
            .unwrap();
        assert_eq!(
            receive(&mut client).await,
            Message::text(r#"{"channel":"pong"}"#)
        );
        assert!(indexer.connections.try_recv().is_err());
        assert!(state.connections.try_recv().is_err());
        for kind in ["assetCtxs", "l2Book"] {
            client.send(subscription("subscribe", kind)).await.unwrap();
            receive(&mut client).await;
        }
        let indexer_connection = indexer.connections.recv().await.unwrap();
        let state_connection = state.connections.recv().await.unwrap();
        indexer.messages.recv().await.unwrap();
        state.messages.recv().await.unwrap();
        client
            .send(Message::text(r#"{"method":"ping"}"#))
            .await
            .unwrap();
        assert_eq!(
            receive(&mut client).await,
            Message::text(r#"{"channel":"pong"}"#)
        );
        assert!(indexer.messages.recv().await.unwrap().is_ping());
        assert!(state.messages.recv().await.unwrap().is_ping());
        client
            .send(Message::Ping(b"client-ping".to_vec().into()))
            .await
            .unwrap();
        assert_eq!(
            receive(&mut client).await,
            Message::Pong(b"client-ping".to_vec().into())
        );
        for (backend, connected) in [
            (&mut indexer, &indexer_connection),
            (&mut state, &state_connection),
        ] {
            connected
                .push
                .send(Message::Ping(b"backend-ping".to_vec().into()))
                .await
                .unwrap();
            assert_eq!(
                backend.messages.recv().await.unwrap(),
                Message::Pong(b"backend-ping".to_vec().into())
            );
        }
        assert!(timeout(Duration::from_millis(50), client.next())
            .await
            .is_err());
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn state_only_subscription_needs_no_indexer_and_missing_state_never_falls_back() {
    bounded(async {
        let mut state = Mock::start().await;
        let mut cfg = config();
        cfg.upstreams.state_ws = Some(state.service.endpoint());
        let gateway = Service::gateway(cfg).await;
        let mut client = connect(&gateway).await;
        let message = subscription("subscribe", "clearinghouseState");
        client.send(message.clone()).await.unwrap();
        assert_eq!(receive(&mut client).await, message);
        assert_eq!(state.messages.recv().await.unwrap(), message);
        gateway.shutdown().await;

        let mut indexer = Mock::start().await;
        let mut cfg = config();
        cfg.upstreams.indexer_ws = Some(indexer.service.endpoint());
        let gateway = Service::gateway(cfg).await;
        let mut client = connect(&gateway).await;
        for method in ["subscribe", "unsubscribe"] {
            client
                .send(subscription(method, "assetCtxs"))
                .await
                .unwrap();
            assert_error(&mut client, "upstream_not_configured", "state").await;
        }
        assert!(indexer.connections.try_recv().is_err());
        let message = subscription("subscribe", "l2Book");
        client.send(message.clone()).await.unwrap();
        assert_eq!(receive(&mut client).await, message);
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn default_origins_allow_https_frontend_and_local_development_for_websocket() {
    bounded(async {
        let mut upstream = Mock::start().await;
        let mut cfg = config();
        cfg.upstreams.indexer_ws = Some(upstream.service.endpoint());
        let gateway = Service::gateway(cfg).await;
        for origin in [
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "https://dev.dex.biya.io",
        ] {
            let response = request(&gateway)
                .header("origin", origin)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 101, "{origin}");
            assert_eq!(response.headers()["access-control-allow-origin"], origin);
        }
        for origin in [
            "http://101.36.123.139:35002",
            "http://dev.dex.biya.io",
            "https://dev.dex.biya.io:35002",
            "https://dev.dex.biya.io.evil.example",
        ] {
            assert_eq!(
                request(&gateway)
                    .header("origin", origin)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                403,
                "{origin}"
            );
        }
        assert!(upstream.connections.try_recv().is_err());
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn invalid_handshake_missing_backends_and_origin_are_rejected() {
    bounded(async {
        let mut cfg = config();
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
async fn lazy_handshake_errors_are_reported_without_fallback_and_other_backend_still_works() {
    bounded(async {
        for mode in [
            "reject",
            "redirect",
            "invalid_accept",
            "compression",
            "subprotocol",
            "timeout",
        ] {
            let (calls, mut called) = mpsc::unbounded_channel();
            let failing = Service::upstream(
                Router::new().route(
                    "/ws",
                    get(move |req: Request| {
                        calls.send(()).unwrap();
                        async move {
                            if mode == "timeout" {
                                return std::future::pending::<Response>().await;
                            }
                            if mode == "reject" || mode == "redirect" {
                                return Response::builder()
                                    .status(if mode == "reject" { 429 } else { 302 })
                                    .header("location", "http://127.0.0.1:1/should-not-follow")
                                    .body(Body::from("backend unavailable"))
                                    .unwrap();
                            }
                            let mut response = upgrade_response(&req);
                            let (header, value) = match mode {
                                "invalid_accept" => ("sec-websocket-accept", "wrong"),
                                "compression" => ("sec-websocket-extensions", "permessage-deflate"),
                                _ => ("sec-websocket-protocol", "hl"),
                            };
                            response
                                .headers_mut()
                                .insert(header, HeaderValue::from_static(value));
                            response
                        }
                    }),
                ),
                CancellationToken::new(),
            )
            .await;
            let mut indexer = Mock::start().await;
            let mut cfg = config();
            cfg.upstreams.state_ws = Some(failing.endpoint());
            cfg.upstreams.indexer_ws = Some(indexer.service.endpoint());
            cfg.websocket.handshake_timeout_ms = 50;
            let gateway = Service::gateway(cfg).await;
            let mut client = connect(&gateway).await;
            client
                .send(subscription("subscribe", "assetCtxs"))
                .await
                .unwrap();
            let expected = match mode {
                "timeout" => "upstream_timeout",
                "reject" | "redirect" => "upstream_handshake_rejected",
                _ => "invalid_upstream_handshake",
            };
            assert_error(&mut client, expected, "state").await;
            called.recv().await.unwrap();
            assert!(called.try_recv().is_err());
            assert!(indexer.connections.try_recv().is_err());
            let message = subscription("subscribe", "l2Book");
            client.send(message.clone()).await.unwrap();
            assert_eq!(receive(&mut client).await, message);
            gateway.shutdown().await;
        }
    })
    .await;
}

#[tokio::test]
async fn invalid_commands_and_duplicate_routing_fields_never_reach_upstreams() {
    bounded(async {
        let mut indexer = Mock::start().await;
        let mut state = Mock::start().await;
        let mut cfg = config();
        cfg.upstreams.indexer_ws = Some(indexer.service.endpoint());
        cfg.upstreams.state_ws = Some(state.service.endpoint());
        let gateway = Service::gateway(cfg).await;
        let mut client = connect(&gateway).await;
        for text in [
            "not json", "[]", "{}", r#"["subscribe",{"type":"assetCtxs"}]"#,
            r#"{"method":"post","request":{}}"#,
            r#"{"method":"subscribe"}"#,
            r#"{"method":"subscribe","subscription":["assetCtxs"]}"#,
            r#"{"method":"subscribe","subscription":{"type":1}}"#,
            r#"{"method":"subscribe","method":"unsubscribe","subscription":{"type":"assetCtxs"}}"#,
            r#"{"method":"subscribe","subscription":{"type":"l2Book","type":"assetCtxs"}}"#,
            r#"{"method":"subscribe","subscription":{"type":"assetCtxs"},"subscription":{"type":"l2Book"}}"#,
        ] {
            client.send(Message::text(text)).await.unwrap();
            let response = receive(&mut client).await;
            let value: serde_json::Value = serde_json::from_str(response.to_text().unwrap()).unwrap();
            assert_eq!(value["channel"], "error", "{text}");
        }
        assert!(indexer.connections.try_recv().is_err());
        assert!(state.connections.try_recv().is_err());
        gateway.shutdown().await;
    }).await;
}

// Only test fixture frames are encoded manually; production uses tungstenite.
fn frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut result = vec![(if fin { 0x80 } else { 0 }) | opcode];
    if payload.len() < 126 {
        result.push(payload.len() as u8 | 0x80);
    } else {
        result.push(126 | 0x80);
        result.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    let mask = [1, 2, 3, 4];
    result.extend_from_slice(&mask);
    result.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    result
}

#[tokio::test]
async fn fragmented_subscription_preserves_payload_and_processes_interleaved_ping() {
    bounded(async {
        let mut state = Mock::start().await;
        let mut cfg = config();
        cfg.upstreams.state_ws = Some(state.service.endpoint());
        let gateway = Service::gateway(cfg).await;
        let mut raw = request(&gateway).send().await.unwrap().upgrade().await.unwrap();
        let text = " {\n \"method\":\"subscribe\",\"subscription\":{\"type\":\"assetCtxs\",\"dex\":\"\"},\"unknown\":90071992547409931234567890 }";
        raw.write_all(&frame(false, 1, &text.as_bytes()[..42])).await.unwrap();
        raw.write_all(&frame(true, 9, b"check")).await.unwrap();
        let mut pong = [0; 7];
        raw.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"\x8a\x05check");
        raw.write_all(&frame(true, 0, &text.as_bytes()[42..])).await.unwrap();
        let mut client = WebSocketStream::from_raw_socket(raw, Role::Client, None).await;
        assert_eq!(receive(&mut client).await, Message::text(text));
        assert_eq!(state.messages.recv().await.unwrap(), Message::text(text));
        gateway.shutdown().await;
    }).await;
}

#[tokio::test]
async fn binary_and_oversized_client_messages_close_without_contacting_backend() {
    bounded(async {
        for binary in [true, false] {
            let mut state = Mock::start().await;
            let mut cfg = config();
            cfg.upstreams.state_ws = Some(state.service.endpoint());
            cfg.websocket.max_message_bytes = 128;
            let gateway = Service::gateway(cfg).await;
            let mut client = connect(&gateway).await;
            let message = if binary {
                Message::Binary(vec![0, 1].into())
            } else {
                Message::text("x".repeat(129))
            };
            client.send(message).await.unwrap();
            let Message::Close(Some(close)) = receive(&mut client).await else {
                panic!("expected close");
            };
            assert_eq!(
                close.code,
                if binary {
                    CloseCode::Unsupported
                } else {
                    CloseCode::Size
                }
            );
            assert!(state.connections.try_recv().is_err());
            gateway.shutdown().await;
        }
    })
    .await;
}

#[tokio::test]
async fn client_close_reaches_both_backends_and_releases_capacity() {
    bounded(async {
        let mut indexer = Mock::start().await;
        let mut state = Mock::start().await;
        let mut cfg = config();
        cfg.upstreams.indexer_ws = Some(indexer.service.endpoint());
        cfg.upstreams.state_ws = Some(state.service.endpoint());
        cfg.websocket.max_connections = 1;
        let gateway = Service::gateway(cfg).await;
        let mut client = connect(&gateway).await;
        for kind in ["l2Book", "assetCtxs"] {
            client.send(subscription("subscribe", kind)).await.unwrap();
            receive(&mut client).await;
        }
        indexer.messages.recv().await.unwrap();
        state.messages.recv().await.unwrap();
        assert_eq!(request(&gateway).send().await.unwrap().status(), 503);
        let close = Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "done".into(),
        });
        client.send(Message::Close(close.clone())).await.unwrap();
        assert_eq!(receive(&mut client).await, Message::Close(close.clone()));
        assert_eq!(
            indexer.messages.recv().await.unwrap(),
            Message::Close(close.clone())
        );
        assert_eq!(state.messages.recv().await.unwrap(), Message::Close(close));
        indexer.closed.recv().await.unwrap();
        state.closed.recv().await.unwrap();
        // Observe task completion rather than assuming the close ACK released the permit already.
        loop {
            let response = request(&gateway).send().await.unwrap();
            if response.status() == 101 {
                break;
            }
            assert_eq!(response.status(), 503);
            tokio::task::yield_now().await;
        }
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn upstream_close_or_disconnect_closes_frontend_and_other_backend_without_reconnect() {
    bounded(async {
        for abrupt in [false, true] {
            let mut indexer = Mock::start().await;
            let mut state = Mock::start().await;
            let mut cfg = config();
            cfg.upstreams.indexer_ws = Some(indexer.service.endpoint());
            cfg.upstreams.state_ws = Some(state.service.endpoint());
            let gateway = Service::gateway(cfg).await;
            let mut client = connect(&gateway).await;
            for kind in ["l2Book", "clearinghouseState"] {
                client.send(subscription("subscribe", kind)).await.unwrap();
                receive(&mut client).await;
            }
            let state_connection = state.connections.recv().await.unwrap();
            let close = Some(CloseFrame {
                code: CloseCode::Away,
                reason: "maintenance".into(),
            });
            if abrupt {
                state.service.stop.cancel();
            } else {
                state_connection
                    .push
                    .send(Message::Close(close.clone()))
                    .await
                    .unwrap();
            }
            let Message::Close(received) = receive(&mut client).await else {
                panic!("expected close");
            };
            if abrupt {
                assert_eq!(received.unwrap().code, CloseCode::Error);
            } else {
                assert_eq!(received, close);
            }
            indexer.closed.recv().await.unwrap();
            assert!(state.connections.try_recv().is_err());
            gateway.shutdown().await;
        }
    })
    .await;
}

#[tokio::test]
async fn shutdown_and_idle_timeout_close_all_upstream_connections() {
    bounded(async {
        for idle in [false, true] {
            let mut indexer = Mock::start().await;
            let mut state = Mock::start().await;
            let mut cfg = config();
            assert_eq!(cfg.websocket.idle_timeout_ms, 0);
            if idle {
                cfg.websocket.idle_timeout_ms = 150;
            }
            cfg.upstreams.indexer_ws = Some(indexer.service.endpoint());
            cfg.upstreams.state_ws = Some(state.service.endpoint());
            let gateway = Service::gateway(cfg).await;
            let mut client = connect(&gateway).await;
            for kind in ["l2Book", "assetCtxs"] {
                client.send(subscription("subscribe", kind)).await.unwrap();
                receive(&mut client).await;
            }
            if !idle {
                gateway.stop.cancel();
            }
            let end = client.next().await;
            assert!(end.is_none() || end.unwrap().is_err());
            indexer.closed.recv().await.unwrap();
            state.closed.recv().await.unwrap();
            gateway.shutdown().await;
        }
    })
    .await;
}

#[tokio::test]
async fn oversized_upstream_message_closes_connection() {
    bounded(async {
        let mut state = Mock::start().await;
        let mut cfg = config();
        cfg.websocket.max_message_bytes = 256;
        cfg.upstreams.state_ws = Some(state.service.endpoint());
        let gateway = Service::gateway(cfg).await;
        let mut client = connect(&gateway).await;
        client
            .send(subscription("subscribe", "assetCtxs"))
            .await
            .unwrap();
        receive(&mut client).await;
        let connected = state.connections.recv().await.unwrap();
        connected
            .push
            .send(Message::text("x".repeat(257)))
            .await
            .unwrap();
        let Message::Close(Some(close)) = receive(&mut client).await else {
            panic!("expected close");
        };
        assert_eq!(close.code, CloseCode::Size);
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn frontend_connections_have_separate_upstreams_and_independent_lifetimes() {
    bounded(async {
        let mut state = Mock::start().await;
        let mut cfg = config();
        cfg.upstreams.state_ws = Some(state.service.endpoint());
        let gateway = Service::gateway(cfg).await;
        let mut first = connect(&gateway).await;
        let mut second = connect(&gateway).await;
        for client in [&mut first, &mut second] {
            client
                .send(subscription("subscribe", "assetCtxs"))
                .await
                .unwrap();
            receive(client).await;
        }
        let first_upstream = state.connections.recv().await.unwrap();
        let second_upstream = state.connections.recv().await.unwrap();
        first_upstream
            .push
            .send(Message::text("first only"))
            .await
            .unwrap();
        assert_eq!(receive(&mut first).await, Message::text("first only"));
        second_upstream
            .push
            .send(Message::text("second only"))
            .await
            .unwrap();
        assert_eq!(receive(&mut second).await, Message::text("second only"));
        drop(first);
        state.closed.recv().await.unwrap();
        second_upstream
            .push
            .send(Message::text("still connected"))
            .await
            .unwrap();
        assert_eq!(receive(&mut second).await, Message::text("still connected"));
        assert!(state.connections.try_recv().is_err());
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn invalid_frames_and_oversized_fragmented_messages_never_reach_backend() {
    bounded(async {
        let mut reserved = frame(true, 1, b"{}");
        reserved[0] |= 0x40; // Compression was not negotiated.
        let fragmented = [frame(false, 1, &[b'x'; 80]), frame(true, 0, &[b'x'; 80])].concat();
        for (bytes, expected) in [
            (vec![0x81, 0x02, b'{', b'}'], CloseCode::Protocol), // Unmasked client frame.
            (reserved, CloseCode::Protocol),
            (fragmented, CloseCode::Size),
        ] {
            let mut state = Mock::start().await;
            let mut cfg = config();
            cfg.upstreams.state_ws = Some(state.service.endpoint());
            cfg.websocket.max_message_bytes = 128;
            let gateway = Service::gateway(cfg).await;
            let mut raw = request(&gateway)
                .send()
                .await
                .unwrap()
                .upgrade()
                .await
                .unwrap();
            raw.write_all(&bytes).await.unwrap();
            let mut client = WebSocketStream::from_raw_socket(raw, Role::Client, None).await;
            let Message::Close(Some(close)) = receive(&mut client).await else {
                panic!("expected close");
            };
            assert_eq!(close.code, expected);
            assert!(state.connections.try_recv().is_err());
            gateway.shutdown().await;
        }
    })
    .await;
}

#[tokio::test]
async fn refused_state_connection_reports_failure_without_contacting_indexer() {
    bounded(async {
        let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}/ws", reserved.local_addr().unwrap());
        drop(reserved);
        let mut indexer = Mock::start().await;
        let mut cfg = config();
        cfg.upstreams.indexer_ws = Some(indexer.service.endpoint());
        cfg.upstreams.state_ws = Some(endpoint.parse().unwrap());
        let gateway = Service::gateway(cfg).await;
        let mut client = connect(&gateway).await;
        client
            .send(subscription("subscribe", "clearinghouseState"))
            .await
            .unwrap();
        assert_error(&mut client, "upstream_failure", "state").await;
        assert!(indexer.connections.try_recv().is_err());
        gateway.shutdown().await;
    })
    .await;
}

#[tokio::test]
async fn head_cannot_initiate_a_websocket() {
    bounded(async {
        let gateway = Service::gateway(config()).await;
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
