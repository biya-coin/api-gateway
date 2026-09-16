use std::{
    collections::VecDeque,
    convert::Infallible,
    future::{pending, Future},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use api_gateway::{
    config::Config,
    routing::{INDEXER_TYPES, STATE_TYPES},
    server::{router, serve},
};
use axum::{
    body::{to_bytes, Body, Bytes},
    http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode, Uri},
    response::Response,
    Router,
};
use hyper::{
    body::{Body as HttpBody, Frame, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use tokio::{
    io::AsyncReadExt,
    net::TcpListener,
    sync::{mpsc, oneshot, Notify},
    task::{JoinHandle, JoinSet},
    time::timeout,
};
use tower::ServiceExt;
use url::Url;

const TEST_TIMEOUT: Duration = Duration::from_secs(15);
const CAPTURE_LIMIT: usize = 1024 * 1024;

fn config() -> Config {
    let mut config = Config::parse(include_str!("../config/default.toml")).unwrap();
    // Deployment defaults must never become live test backends.
    config.upstreams = Default::default();
    config.http.connect_timeout_ms = 1_000;
    config.http.request_timeout_ms = 5_000;
    config.http.body_timeout_ms = 1_000;
    config
}

async fn bounded(test: impl Future<Output = ()>) {
    timeout(TEST_TIMEOUT, test)
        .await
        .expect("HTTP proxy integration test timed out");
}

struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Debug)]
struct CapturedRequest {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

struct MockUpstream {
    address: SocketAddr,
    requests: mpsc::UnboundedReceiver<CapturedRequest>,
    _task: AbortOnDrop<()>,
}

impl MockUpstream {
    async fn start<F, Fut>(respond: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Response> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, requests) = mpsc::unbounded_channel();
        let respond = Arc::new(respond);
        let app = Router::new().fallback(move |request: Request<Body>| {
            let sender = sender.clone();
            let respond = respond.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = to_bytes(body, CAPTURE_LIMIT).await.unwrap();
                sender
                    .send(CapturedRequest {
                        method: parts.method,
                        uri: parts.uri,
                        headers: parts.headers,
                        body,
                    })
                    .expect("mock request receiver dropped");
                respond().await
            }
        });
        let task = tokio::spawn(async move {
            // Track connection tasks as well as the accept loop: stalled handlers and
            // response bodies must be cancelled when a test drops its mock.
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let app = app.clone();
                        connections.spawn(async move {
                            let service = service_fn(move |request: Request<Incoming>| {
                                app.clone().oneshot(request.map(Body::new))
                            });
                            // Disconnects are expected in timeout and size-limit tests.
                            let _ = http1::Builder::new()
                                .serve_connection(TokioIo::new(stream), service)
                                .await;
                        });
                    }
                    Some(result) = connections.join_next(), if !connections.is_empty() => {
                        result.expect("mock connection task panicked");
                    }
                }
            }
        });
        Self {
            address,
            requests,
            _task: AbortOnDrop(task),
        }
    }

    async fn fixed(body: &'static str) -> Self {
        Self::start(move || async move { Response::new(Body::from(body)) }).await
    }

    fn endpoint(&self, path: &str) -> Url {
        Url::parse(&format!("http://{}{path}", self.address)).unwrap()
    }

    async fn next_request(&mut self) -> CapturedRequest {
        self.requests.recv().await.expect("mock server stopped")
    }

    fn assert_no_requests(&mut self) {
        // Call only after a gateway response (or while its sole admitted request
        // is blocked). No time-based wait is needed to observe completed retries.
        assert!(
            matches!(
                self.requests.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ),
            "unexpected upstream call or stopped mock"
        );
    }
}

fn post(uri: &str, body: impl Into<Body>) -> Request<Body> {
    Request::post(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.into())
        .unwrap()
}

async fn send(app: &Router, request: Request<Body>) -> Response {
    app.clone().oneshot(request).await.unwrap()
}

async fn bytes(response: Response) -> Bytes {
    to_bytes(response.into_body(), CAPTURE_LIMIT).await.unwrap()
}

async fn assert_error(response: Response, status: StatusCode, code: &str) {
    assert_eq!(response.status(), status);
    let body = bytes(response).await;
    let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["error"], code);
}

// Unknown size forces the upstream's HTTP/1 response to use chunked framing.
// With `stall` set, completion is deliberately withheld after the final chunk.
struct TestBody {
    chunks: VecDeque<Bytes>,
    stall: Option<Arc<Notify>>,
}

impl HttpBody for TestBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        if let Some(chunk) = self.chunks.pop_front() {
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        if let Some(stall) = &self.stall {
            stall.notify_one();
            Poll::Pending
        } else {
            Poll::Ready(None)
        }
    }
}

fn chunked(chunks: impl IntoIterator<Item = Bytes>) -> Body {
    Body::new(TestBody {
        chunks: chunks.into_iter().collect(),
        stall: None,
    })
}

#[tokio::test]
async fn all_23_approved_info_types_reach_their_only_upstream() {
    bounded(async {
        assert_eq!(
            STATE_TYPES,
            &[
                "meta",
                "metaAndAssetCtxs",
                "extraAgents",
                "recentTrades",
                "clearinghouseState",
                "activeAssetData",
                "openOrders",
                "frontendOpenOrders",
                "userFees",
                "unifiedBalances",
                "accountNonces",
                "marketSnapshot",
            ]
        );
        assert_eq!(
            INDEXER_TYPES,
            &[
                "allMids",
                "l2Book",
                "webData2",
                "candleSnapshot",
                "historicalOrders",
                "orderStatus",
                "userFills",
                "userFillsByTime",
                "userFunding",
                "fundingHistory",
                "userNonFundingLedgerUpdates",
            ]
        );
        let mut state = MockUpstream::fixed("state").await;
        let mut indexer = MockUpstream::fixed("indexer").await;
        let mut exchange = MockUpstream::fixed("exchange").await;
        let mut config = config();
        config.upstreams.state_info = Some(state.endpoint("/state/info"));
        config.upstreams.indexer_info = Some(indexer.endpoint("/indexer/info"));
        config.upstreams.exchange = Some(exchange.endpoint("/exchange"));
        let app = router(config).unwrap();

        for (types, marker, path, upstream) in [
            (STATE_TYPES, "state", "/state/info", &mut state),
            (INDEXER_TYPES, "indexer", "/indexer/info", &mut indexer),
        ] {
            for kind in types {
                let body = format!(r#"{{"type":"{kind}", "unknown": {{"keep":"原样"}}}}"#);
                let response =
                    send(&app, post("/info?asset=BTC%2FUSD&limit=2", body.clone())).await;
                assert_eq!(response.status(), StatusCode::OK, "{kind}");
                assert_eq!(bytes(response).await.as_ref(), marker.as_bytes(), "{kind}");
                let received = upstream.next_request().await;
                assert_eq!(received.method, Method::POST, "{kind}");
                assert_eq!(received.uri.path(), path, "{kind}");
                assert_eq!(received.uri.query(), Some("asset=BTC%2FUSD&limit=2"));
                assert_eq!(received.body.as_ref(), body.as_bytes(), "{kind}");
            }
        }
        state.assert_no_requests();
        indexer.assert_no_requests();
        exchange.assert_no_requests();
    })
    .await;
}

#[tokio::test]
async fn info_health_is_rejected_and_gateway_liveness_never_calls_upstreams() {
    bounded(async {
        let mut state = MockUpstream::fixed("must not query state").await;
        let mut indexer = MockUpstream::fixed("must not query indexer").await;
        let mut exchange = MockUpstream::fixed("must not query exchange").await;
        let mut config = config();
        config.upstreams.state_info = Some(state.endpoint("/info"));
        config.upstreams.indexer_info = Some(indexer.endpoint("/info"));
        config.upstreams.exchange = Some(exchange.endpoint("/exchange"));
        let app = router(config).unwrap();
        let response = send(&app, Request::get("/healthz").body(Body::empty()).unwrap()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(bytes(response).await.as_ref(), b"ok");

        assert_error(
            send(&app, post("/info", r#"{ "type": "health" }"#)).await,
            StatusCode::BAD_REQUEST,
            "invalid_or_unknown_info_type",
        )
        .await;
        exchange.assert_no_requests();
        state.assert_no_requests();
        indexer.assert_no_requests();
    })
    .await;
}

#[tokio::test]
async fn s1_exchange_actions_and_local_admission_results_are_forwarded_verbatim() {
    bounded(async {
        // Signature fixtures exercise transport only; the gateway must not verify them.
        let actions = [
            r#"{"type":"approveAgent","hyperliquidChain":"Testnet","signatureChainId":"0x66eee","agentAddress":"0x4242424242424242424242424242424242424242","agentName":null,"nonce":1}"#,
            r#"{"type":"order","orders":[{"a":0,"b":true,"p":"65000","s":"0.001","r":false,"t":{"limit":{"tif":"Gtc"}},"c":"0x11111111111111111111111111111111"}],"grouping":"na"}"#,
            r#"{"type":"cancel","cancels":[{"a":0,"o":123}]}"#,
            r#"{"type":"cancelByCloid","cancels":[{"asset":0,"cloid":"0x11111111111111111111111111111111"}]}"#,
            r#"{"type":"updateLeverage","asset":0,"isCross":true,"leverage":10}"#,
        ];
        for response_body in [
            r#"{"code":0,"message":"accepted","tx_hash":"0x1234"}"#,
            r#"{"code":1001,"message":"invalid parameter"}"#,
            r#"{"code":1002,"message":"duplicate transaction"}"#,
            r#"{"code":1003,"message":"invalid signature"}"#,
        ] {
            let mut exchange = MockUpstream::fixed(response_body).await;
            let mut config = config();
            config.upstreams.exchange = Some(exchange.endpoint("/exchange"));
            // No query backends are needed to forward signed writes.
            let app = router(config).unwrap();
            for action in actions {
                let payload = format!(
                    "{{\n \"action\": {action}, \"nonce\": 1, \"signature\": {{\"r\":\"0xA\",\"s\":\"0xabc\",\"v\":27}}, \"vaultAddress\": null, \"expiresAfter\": 2\n}}"
                );
                let response = send(&app, post("/exchange", payload.clone())).await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(bytes(response).await.as_ref(), response_body.as_bytes());
                let received = exchange.next_request().await;
                assert_eq!(received.uri.path(), "/exchange");
                assert_eq!(received.body.as_ref(), payload.as_bytes());
                exchange.assert_no_requests();
            }
        }
    })
    .await;
}

#[tokio::test]
async fn order_status_never_retries_or_falls_back_to_state() {
    bounded(async {
        let mut state = MockUpstream::fixed("must not query state").await;
        for (status, body) in [
            (StatusCode::OK, r#"{"status":"unknownOid"}"#),
            (StatusCode::NOT_FOUND, "not found"),
            (StatusCode::SERVICE_UNAVAILABLE, "indexer unavailable"),
        ] {
            let mut indexer = MockUpstream::start(move || async move {
                Response::builder()
                    .status(status)
                    .body(Body::from(body))
                    .unwrap()
            })
            .await;
            let mut config = config();
            config.upstreams.state_info = Some(state.endpoint("/info"));
            config.upstreams.indexer_info = Some(indexer.endpoint("/info"));
            let app = router(config).unwrap();
            let request = r#"{"type":"orderStatus","oid":18446744073709551615}"#;
            let response = send(&app, post("/info", request)).await;
            assert_eq!(response.status(), status);
            assert_eq!(bytes(response).await.as_ref(), body.as_bytes());
            assert_eq!(
                indexer.next_request().await.body.as_ref(),
                request.as_bytes()
            );
            indexer.assert_no_requests();
            state.assert_no_requests();
        }
    })
    .await;
}

#[tokio::test]
async fn exchange_preserves_signed_bytes_status_binary_body_and_end_to_end_headers() {
    bounded(async {
        const SIGNED: &str = concat!(
            " \n",
            r#"{
  "action": { "type": "order", "orders": [] },
  "nonce": 184467440737095516151234567890,
  "signature": { "r": "0x00aBcD", "s": "0x000001", "v": 27 },
  "unknown": { "备注": "签名必须原样 🧪", "amount": 90071992547409931234567890 }
}"#,
            "\n "
        );
        const BINARY: &[u8] = &[0, 255, 254, 13, 10, 128, b'X'];
        let mut upstream = MockUpstream::start(|| async {
            let mut response = Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .header(header::RETRY_AFTER, "17")
                .header("x-upstream", "preserve")
                .header(header::CONNECTION, "keep-alive, X-Response-Hop-One")
                .header(header::CONNECTION, "X-Response-Hop-Two")
                .header("keep-alive", "timeout=5")
                .header("x-response-hop-one", "remove")
                .header("x-response-hop-two", "remove")
                .body(Body::from(BINARY))
                .unwrap();
            for cookie in ["session=a; Path=/; HttpOnly", "preference=b; Path=/"] {
                response
                    .headers_mut()
                    .append(header::SET_COOKIE, HeaderValue::from_static(cookie));
            }
            response
        })
        .await;
        let mut config = config();
        config.upstreams.exchange = Some(upstream.endpoint("/signed/exchange"));
        let app = router(config).unwrap();
        let mut request = post("/exchange?trace=a%2Fb", SIGNED);
        for (name, value) in [
            ("authorization", "Bearer test-only-credential"),
            ("host", "forged.example"),
            ("forwarded", "for=203.0.113.8;proto=https"),
            ("x-forwarded-for", "203.0.113.8, 203.0.113.9"),
            ("x-forwarded-host", "forged.example"),
            ("x-forwarded-proto", "https"),
            ("x-real-ip", "203.0.113.8"),
            ("connection", "keep-alive, X-Request-Hop-One"),
            ("connection", "X-Request-Hop-Two, upgrade"),
            ("x-request-hop-one", "remove"),
            ("x-request-hop-two", "remove"),
            ("keep-alive", "timeout=5"),
            ("proxy-authorization", "Basic test-only"),
            ("proxy-authenticate", "Basic"),
            ("te", "trailers"),
            ("trailer", "x-trailer"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "websocket"),
            ("content-length", "1"),
            ("x-request-id", "preserve"),
        ] {
            request
                .headers_mut()
                .append(name, HeaderValue::from_static(value));
        }
        let response = send(&app, request).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let headers = response.headers();
        assert_eq!(headers[header::CONTENT_TYPE], "application/octet-stream");
        assert_eq!(headers[header::RETRY_AFTER], "17");
        assert_eq!(headers["x-upstream"], "preserve");
        assert_eq!(
            headers
                .get_all(header::SET_COOKIE)
                .iter()
                .map(|value| value.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["session=a; Path=/; HttpOnly", "preference=b; Path=/"]
        );
        for name in [
            "connection",
            "keep-alive",
            "x-response-hop-one",
            "x-response-hop-two",
            "transfer-encoding",
        ] {
            assert!(!headers.contains_key(name), "response leaked {name}");
        }
        assert_eq!(bytes(response).await.as_ref(), BINARY);

        let received = upstream.next_request().await;
        assert_eq!(received.method, Method::POST);
        assert_eq!(received.uri, "/signed/exchange?trace=a%2Fb");
        assert_eq!(received.body.as_ref(), SIGNED.as_bytes());
        assert_eq!(received.headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(
            received.headers[header::AUTHORIZATION],
            "Bearer test-only-credential"
        );
        assert_eq!(received.headers["x-request-id"], "preserve");
        assert_eq!(
            received.headers[header::HOST].to_str().unwrap(),
            upstream.address.to_string()
        );
        assert_eq!(
            received.headers[header::CONTENT_LENGTH]
                .to_str()
                .unwrap()
                .parse::<usize>()
                .unwrap(),
            SIGNED.len()
        );
        for name in [
            "connection",
            "keep-alive",
            "x-request-hop-one",
            "x-request-hop-two",
            "proxy-authorization",
            "proxy-authenticate",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
            "x-real-ip",
        ] {
            assert!(
                !received.headers.contains_key(name),
                "request leaked {name}"
            );
        }
        upstream.assert_no_requests();
    })
    .await;
}

#[tokio::test]
async fn invalid_unknown_and_duplicate_info_types_are_rejected_before_forwarding() {
    bounded(async {
        let mut upstream = MockUpstream::fixed("must not be called").await;
        let mut config = config();
        config.upstreams.state_info = Some(upstream.endpoint("/state"));
        config.upstreams.indexer_info = Some(upstream.endpoint("/indexer"));
        config.upstreams.exchange = Some(upstream.endpoint("/exchange"));
        let app = router(config).unwrap();
        for body in [
            "{}",
            "[]",
            "null",
            "not json",
            r#"{"type":1}"#,
            r#"{"type":"notApproved"}"#,
            r#"{"type":"health"}"#,
            r#"{"type":"exchangeStatus"}"#,
            r#"{"type":"userRateLimit"}"#,
            r#"{"type":"stateInfo"}"#,
            r#"{"type":"block","height":1}"#,
            r#"{"type":"bridgeSnapshot"}"#,
            r#"{"type":"bridgeDepositStatus"}"#,
            r#"{"type":"bridgeWithdrawalStatus"}"#,
            r#"{"type":"accountOverview"}"#,
            r#"{"type":"health","type":"meta"}"#,
            r#"{"type":"meta","type":"orderStatus"}"#,
            r#"{"type":"meta","type":"meta"}"#,
        ] {
            assert_error(
                send(&app, post("/info", body)).await,
                StatusCode::BAD_REQUEST,
                "invalid_or_unknown_info_type",
            )
            .await;
        }
        upstream.assert_no_requests();
    })
    .await;
}

#[tokio::test]
async fn missing_upstreams_return_503_without_using_another_configured_backend() {
    bounded(async {
        let mut upstream = MockUpstream::fixed("must not be used as fallback").await;
        for (missing, path, body) in [
            ("state", "/info", r#"{"type":"meta"}"#),
            ("indexer", "/info", r#"{"type":"orderStatus"}"#),
            ("exchange", "/exchange", r#"{"nonce":1}"#),
        ] {
            let mut config = config();
            config.upstreams.state_info = Some(upstream.endpoint("/state"));
            config.upstreams.indexer_info = Some(upstream.endpoint("/indexer"));
            config.upstreams.exchange = Some(upstream.endpoint("/exchange"));
            match missing {
                "state" => config.upstreams.state_info = None,
                "indexer" => config.upstreams.indexer_info = None,
                "exchange" => config.upstreams.exchange = None,
                _ => unreachable!(),
            }
            let app = router(config).unwrap();
            assert_error(
                send(&app, post(path, body)).await,
                StatusCode::SERVICE_UNAVAILABLE,
                "upstream_not_configured",
            )
            .await;
            upstream.assert_no_requests();
        }
    })
    .await;
}

#[tokio::test]
async fn refused_upstream_connection_returns_502() {
    bounded(async {
        // A bound but non-listening socket may silently drop SYNs on macOS.
        // Close an ephemeral listener to exercise connection refusal instead.
        let reserved = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let mut config = config();
        config.upstreams.exchange =
            Some(Url::parse(&format!("http://{address}/exchange")).unwrap());
        let app = router(config).unwrap();
        assert_error(
            send(&app, post("/exchange", "{}")).await,
            StatusCode::BAD_GATEWAY,
            "upstream_failure",
        )
        .await;
    })
    .await;
}

#[tokio::test]
async fn upstream_header_and_body_stalls_return_504() {
    bounded(async {
        for stall_headers in [true, false] {
            let body_polled = Arc::new(Notify::new());
            let signal = body_polled.clone();
            let mut upstream = MockUpstream::start(move || {
                let signal = signal.clone();
                async move {
                    if stall_headers {
                        pending::<Response>().await
                    } else {
                        Response::new(Body::new(TestBody {
                            chunks: [Bytes::from_static(b"partial body")].into(),
                            stall: Some(signal),
                        }))
                    }
                }
            })
            .await;
            let mut config = config();
            config.http.request_timeout_ms = 250;
            config.upstreams.exchange = Some(upstream.endpoint("/exchange"));
            let app = router(config).unwrap();
            let observed = async {
                upstream.next_request().await;
                if !stall_headers {
                    body_polled.notified().await;
                }
            };
            let (response, ()) = tokio::join!(send(&app, post("/exchange", "{}")), observed);
            assert_error(response, StatusCode::GATEWAY_TIMEOUT, "upstream_timeout").await;
            upstream.assert_no_requests();
        }
    })
    .await;
}

#[tokio::test]
async fn request_body_limit_covers_buffered_and_unknown_length_bodies() {
    bounded(async {
        let mut upstream = MockUpstream::fixed("accepted").await;
        let mut config = config();
        config.http.max_request_body_bytes = 8;
        config.upstreams.exchange = Some(upstream.endpoint("/exchange"));
        let app = router(config).unwrap();
        let response = send(&app, post("/exchange", "12345678")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(upstream.next_request().await.body.as_ref(), b"12345678");

        for body in [
            Body::from("123456789"),
            chunked([
                Bytes::from_static(b"1234"),
                Bytes::from_static(b"5678"),
                Bytes::from_static(b"9"),
            ]),
        ] {
            assert_error(
                send(&app, post("/exchange", body)).await,
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_body_too_large",
            )
            .await;
            upstream.assert_no_requests();
        }
    })
    .await;
}

#[tokio::test]
async fn response_body_limit_covers_content_length_and_chunked_bodies() {
    bounded(async {
        for streaming in [false, true] {
            for oversized in [false, true] {
                let payload: &'static [u8] = if oversized { b"123456789" } else { b"12345678" };
                let mut upstream = MockUpstream::start(move || async move {
                    let body = if streaming {
                        chunked([
                            Bytes::from_static(&payload[..4]),
                            Bytes::from_static(&payload[4..]),
                        ])
                    } else {
                        Body::from(payload)
                    };
                    Response::new(body)
                })
                .await;
                let mut config = config();
                config.http.max_response_body_bytes = 8;
                config.upstreams.exchange = Some(upstream.endpoint("/exchange"));
                let app = router(config).unwrap();
                let response = send(&app, post("/exchange", "{}")).await;
                if oversized {
                    assert_error(response, StatusCode::BAD_GATEWAY, "upstream_body_too_large")
                        .await;
                } else {
                    assert_eq!(response.status(), StatusCode::OK);
                    assert_eq!(bytes(response).await.as_ref(), payload);
                }
                upstream.next_request().await;
                upstream.assert_no_requests();
            }
        }
    })
    .await;
}

#[tokio::test]
async fn redirects_are_returned_without_contacting_the_target() {
    bounded(async {
        let mut target = MockUpstream::fixed("must not follow redirect").await;
        for status in [StatusCode::FOUND, StatusCode::TEMPORARY_REDIRECT] {
            let location = target.endpoint("/redirect-target").to_string();
            let upstream_location = location.clone();
            let mut upstream = MockUpstream::start(move || {
                let location = upstream_location.clone();
                async move {
                    Response::builder()
                        .status(status)
                        .header(header::LOCATION, location)
                        .body(Body::from("redirect body"))
                        .unwrap()
                }
            })
            .await;
            let mut config = config();
            config.upstreams.exchange = Some(upstream.endpoint("/exchange"));
            let app = router(config).unwrap();
            let response = send(&app, post("/exchange", "{}")).await;
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::LOCATION], location);
            assert_eq!(bytes(response).await.as_ref(), b"redirect body");
            upstream.next_request().await;
            upstream.assert_no_requests();
            target.assert_no_requests();
        }
    })
    .await;
}

#[tokio::test]
async fn disconnected_write_is_sent_only_once() {
    bounded(async {
        const PAYLOAD: &[u8] = br#"{"nonce":7,"action":{"type":"cancel","cancels":[]}}"#;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, mut attempts) = mpsc::unbounded_channel();
        let _task = AbortOnDrop(tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 1024];
                let body_start = loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    assert_ne!(read, 0, "gateway disconnected before sending the write");
                    request.extend_from_slice(&buffer[..read]);
                    assert!(request.len() <= CAPTURE_LIMIT);
                    if let Some(header_end) =
                        request.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        let body_start = header_end + 4;
                        if request.len() >= body_start + PAYLOAD.len() {
                            break body_start;
                        }
                    }
                };
                sender.send(request[body_start..].to_vec()).unwrap();
                // Close only after reading the complete write, before any response.
                drop(stream);
            }
        }));
        let mut config = config();
        config.upstreams.exchange =
            Some(Url::parse(&format!("http://{address}/exchange")).unwrap());
        let app = router(config).unwrap();
        assert_error(
            send(&app, post("/exchange", Body::from(PAYLOAD))).await,
            StatusCode::BAD_GATEWAY,
            "upstream_failure",
        )
        .await;
        assert_eq!(attempts.recv().await.unwrap(), PAYLOAD);
        assert!(matches!(
            attempts.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    })
    .await;
}

#[tokio::test]
async fn default_origins_allow_preflight_and_requests_but_reject_other_origins() {
    bounded(async {
        let mut upstream = MockUpstream::fixed("[]").await;
        let mut config = config();
        config.upstreams.state_info = Some(upstream.endpoint("/info"));
        config.upstreams.exchange = Some(upstream.endpoint("/exchange"));
        let app = router(config).unwrap();
        for path in ["/info", "/exchange"] {
            for origin in [
                "http://localhost:8080",
                "http://127.0.0.1:8080",
                "http://101.36.123.139:35002",
            ] {
                let preflight = Request::builder()
                    .method(Method::OPTIONS)
                    .uri(path)
                    .header(header::ORIGIN, origin)
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "content-type")
                    .body(Body::empty())
                    .unwrap();
                let response = send(&app, preflight).await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
                    origin
                );
                assert_eq!(
                    response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS],
                    "content-type"
                );
                upstream.assert_no_requests();

                let body =
                    r#"{"type":"extraAgents","user":"0x0000000000000000000000000000000000000001"}"#;
                let mut request = post(path, body);
                request
                    .headers_mut()
                    .insert(header::ORIGIN, HeaderValue::from_static(origin));
                let response = send(&app, request).await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
                    origin
                );
                assert!(!response
                    .headers()
                    .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS));
                assert_eq!(bytes(response).await.as_ref(), b"[]");
                let received = upstream.next_request().await;
                assert_eq!(received.uri.path(), path);
                assert_eq!(received.headers[header::ORIGIN], origin);
                assert_eq!(received.body.as_ref(), body.as_bytes());
            }
            for origin in [
                "https://101.36.123.139:35002",
                "http://101.36.123.139:35003",
            ] {
                for method in [Method::POST, Method::OPTIONS] {
                    let request = Request::builder()
                        .method(method)
                        .uri(path)
                        .header(header::ORIGIN, origin)
                        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                        .body(Body::from(r#"{"type":"meta"}"#))
                        .unwrap();
                    assert_error(
                        send(&app, request).await,
                        StatusCode::FORBIDDEN,
                        "origin_not_allowed",
                    )
                    .await;
                }
            }
        }
        upstream.assert_no_requests();
    })
    .await;
}

#[tokio::test]
async fn cors_preflight_and_origin_rejection_never_call_upstream() {
    bounded(async {
        const ALLOWED: &str = "https://app.example";
        let mut upstream = MockUpstream::fixed("must not be called").await;
        let mut config = config();
        config.access.allowed_origins = vec![ALLOWED.to_owned()];
        config.upstreams.state_info = Some(upstream.endpoint("/info"));
        config.upstreams.exchange = Some(upstream.endpoint("/exchange"));
        let app = router(config).unwrap();
        for path in ["/info", "/exchange"] {
            let preflight = Request::builder()
                .method(Method::OPTIONS)
                .uri(path)
                .header(header::ORIGIN, ALLOWED)
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "authorization,content-type",
                )
                .body(Body::empty())
                .unwrap();
            let response = send(&app, preflight).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
                ALLOWED
            );
            assert!(response.headers()[header::ACCESS_CONTROL_ALLOW_METHODS]
                .to_str()
                .unwrap()
                .split(',')
                .any(|method| method.trim() == "POST"));
            assert_eq!(
                response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS],
                "authorization,content-type"
            );

            for method in [Method::POST, Method::OPTIONS] {
                let denied = Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::ORIGIN, "https://denied.example")
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                    .body(Body::from(r#"{"type":"meta"}"#))
                    .unwrap();
                let response = send(&app, denied).await;
                assert!(!response
                    .headers()
                    .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
                assert_error(response, StatusCode::FORBIDDEN, "origin_not_allowed").await;
            }
        }
        let mut duplicate = post("/exchange", "{}");
        duplicate
            .headers_mut()
            .append(header::ORIGIN, HeaderValue::from_static(ALLOWED));
        duplicate
            .headers_mut()
            .append(header::ORIGIN, HeaderValue::from_static(ALLOWED));
        assert_error(
            send(&app, duplicate).await,
            StatusCode::FORBIDDEN,
            "origin_not_allowed",
        )
        .await;
        upstream.assert_no_requests();
    })
    .await;
}

#[tokio::test]
async fn upstream_cors_cannot_override_gateway_policy() {
    bounded(async {
        const ALLOWED: &str = "https://app.example";
        let mut upstream = MockUpstream::start(|| async {
            Response::builder()
                .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
                .header(header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true")
                .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "x-upstream-secret")
                .header(header::ACCESS_CONTROL_ALLOW_METHODS, "DELETE")
                .header(header::ACCESS_CONTROL_EXPOSE_HEADERS, "x-upstream-secret")
                .header(header::ACCESS_CONTROL_MAX_AGE, "99999")
                .header(header::RETRY_AFTER, "3")
                .body(Body::from("allowed"))
                .unwrap()
        })
        .await;
        let mut config = config();
        config.access.allowed_origins = vec![ALLOWED.to_owned()];
        config.upstreams.exchange = Some(upstream.endpoint("/exchange"));
        let app = router(config).unwrap();
        let mut request = post("/exchange", "{}");
        request
            .headers_mut()
            .insert(header::ORIGIN, HeaderValue::from_static(ALLOWED));
        let response = send(&app, request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let headers = response.headers();
        assert_eq!(headers[header::ACCESS_CONTROL_ALLOW_ORIGIN], ALLOWED);
        assert_eq!(
            headers[header::ACCESS_CONTROL_EXPOSE_HEADERS],
            "retry-after"
        );
        assert_eq!(headers[header::RETRY_AFTER], "3");
        for name in [
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            header::ACCESS_CONTROL_ALLOW_METHODS,
            header::ACCESS_CONTROL_MAX_AGE,
        ] {
            assert!(!headers.contains_key(&name), "upstream CORS leaked {name}");
        }
        assert_eq!(bytes(response).await.as_ref(), b"allowed");
        assert_eq!(
            upstream.next_request().await.headers[header::ORIGIN],
            ALLOWED
        );

        let response = send(&app, post("/exchange", "{}")).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
        assert!(!response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS));
        assert_eq!(bytes(response).await.as_ref(), b"allowed");
        assert!(!upstream
            .next_request()
            .await
            .headers
            .contains_key(header::ORIGIN));
        upstream.assert_no_requests();
    })
    .await;
}

#[tokio::test]
async fn saturated_http_limit_returns_503_and_releases_capacity_after_completion() {
    bounded(async {
        let release = Arc::new(Notify::new());
        let upstream_release = release.clone();
        let mut upstream = MockUpstream::start(move || {
            let release = upstream_release.clone();
            async move {
                release.notified().await;
                Response::new(Body::from("done"))
            }
        })
        .await;
        let mut config = config();
        config.http.max_in_flight = 1;
        config.upstreams.exchange = Some(upstream.endpoint("/exchange"));
        config.upstreams.state_info = Some(upstream.endpoint("/info"));
        let app = router(config).unwrap();
        let while_occupied = async {
            assert_eq!(upstream.next_request().await.body.as_ref(), b"first write");
            assert_error(
                send(&app, post("/info", r#"{"type":"meta"}"#)).await,
                StatusCode::SERVICE_UNAVAILABLE,
                "gateway_busy",
            )
            .await;
            upstream.assert_no_requests();
            release.notify_one();
        };
        let (first, ()) =
            tokio::join!(send(&app, post("/exchange", "first write")), while_occupied);
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(bytes(first).await.as_ref(), b"done");

        release.notify_one();
        let response = send(&app, post("/info", r#"{"type":"meta"}"#)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(bytes(response).await.as_ref(), b"done");
        assert_eq!(upstream.next_request().await.uri.path(), "/info");
        upstream.assert_no_requests();
    })
    .await;
}

#[tokio::test]
async fn serve_uses_socket_peer_instead_of_forged_forwarding_headers_and_shuts_down() {
    bounded(async {
        let mut upstream = MockUpstream::fixed("proxied").await;
        let mut config = config();
        config.upstreams.exchange = Some(upstream.endpoint("/exchange"));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let server = serve(listener, config, async {
            let _ = stopped.await;
        });
        let client = async {
            let client = reqwest::Client::builder()
                .no_proxy()
                .http1_only()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(TEST_TIMEOUT)
                .build()
                .unwrap();
            let response = client
                .post(format!("http://{address}/exchange"))
                .header(header::AUTHORIZATION, "Bearer test-only")
                .header("forwarded", "for=203.0.113.8")
                .header("x-forwarded-for", "203.0.113.8")
                .header("x-real-ip", "203.0.113.8")
                .body(Vec::from(&b"wire bytes"[..]))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.bytes().await.unwrap().as_ref(), b"proxied");
            let received = upstream.next_request().await;
            assert_eq!(received.body.as_ref(), b"wire bytes");
            assert_eq!(received.headers["x-forwarded-for"], "127.0.0.1");
            assert_eq!(
                received.headers.get_all("x-forwarded-for").iter().count(),
                1
            );
            assert_eq!(received.headers[header::AUTHORIZATION], "Bearer test-only");
            assert!(!received.headers.contains_key("forwarded"));
            assert!(!received.headers.contains_key("x-real-ip"));
            upstream.assert_no_requests();
            drop(client);
            stop.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(server, client);
        result.expect("gateway failed to shut down cleanly");
    })
    .await;
}
