use std::time::Duration;

use api_gateway::{config::Config, server};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Router,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpSocket, TcpStream},
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tower::ServiceExt;

struct Task<T>(JoinHandle<T>);
impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn config() -> Config {
    Config::parse(include_str!("../config/default.toml")).unwrap()
}

async fn upstream(size: usize) -> (String, Task<()>, mpsc::UnboundedReceiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/exchange", listener.local_addr().unwrap());
    let (calls, received) = mpsc::unbounded_channel();
    let router = Router::new().fallback(move || {
        let calls = calls.clone();
        async move {
            calls.send(()).unwrap();
            Body::from(vec![b'x'; size])
        }
    });
    let task = Task(tokio::spawn(async {
        axum::serve(listener, router).await.unwrap();
    }));
    (url, task, received)
}

fn request() -> Request<Body> {
    Request::post("/exchange").body(Body::from("{}")).unwrap()
}

#[tokio::test]
async fn response_bytes_hold_permit_until_the_last_slice_is_released() {
    timeout(Duration::from_secs(5), async {
        let (url, _upstream, _calls) = upstream(1024).await;
        let mut config = config();
        config.http.max_in_flight = 1;
        config.upstreams.exchange = Some(url.parse().unwrap());
        let router = server::router(config).unwrap();
        let first = router.clone().oneshot(request()).await.unwrap();
        assert_eq!(first.status(), 200);
        let held = to_bytes(first.into_body(), 2048).await.unwrap();
        let slice = held.slice(0..1);
        drop(held);
        assert_eq!(
            router.clone().oneshot(request()).await.unwrap().status(),
            503
        );
        drop(slice);
        assert_eq!(router.oneshot(request()).await.unwrap().status(), 200);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn slow_socket_cannot_accumulate_large_responses_past_in_flight_limit() {
    timeout(Duration::from_secs(8), async {
        let (url, _upstream, mut calls) = upstream(16 * 1024 * 1024).await;
        let mut config = config();
        config.http.max_in_flight = 1;
        config.upstreams.exchange = Some(url.parse().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let mut task = Task(tokio::spawn(server::serve(listener, config, async {
            let _ = stopped.await;
        })));
        let socket = TcpSocket::new_v4().unwrap();
        socket.set_recv_buffer_size(1024).unwrap();
        let mut unread = socket.connect(address).await.unwrap();
        unread
            .write_all(b"POST /exchange HTTP/1.1\r\nHost: local\r\nContent-Length: 2\r\n\r\n{}")
            .await
            .unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            headers.push(unread.read_u8().await.unwrap());
            assert!(headers.len() < 8192);
        }
        assert!(headers.starts_with(b"HTTP/1.1 200"));
        calls.recv().await.unwrap();
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(format!("http://{address}/exchange"))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(calls.try_recv().is_err());
        drop(unread);
        stop.send(()).unwrap();
        (&mut task.0).await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn shutdown_cancels_incomplete_write_before_it_can_reach_upstream() {
    timeout(Duration::from_secs(5), async {
        let (url, _upstream, mut calls) = upstream(1).await;
        let mut config = config();
        config.upstreams.exchange = Some(url.parse().unwrap());
        config.server.shutdown_timeout_ms = 20;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let mut task = Task(tokio::spawn(server::serve(listener, config, async {
            let _ = stopped.await;
        })));
        let mut client = TcpStream::connect(address).await.unwrap();
        client.write_all(b"POST /exchange HTTP/1.1\r\nHost: local\r\nExpect: 100-continue\r\nContent-Length: 100\r\n\r\n").await.unwrap();
        let mut interim = Vec::new();
        while !interim.ends_with(b"\r\n\r\n") {
            interim.push(client.read_u8().await.unwrap());
            assert!(interim.len() < 8192);
        }
        assert!(interim.starts_with(b"HTTP/1.1 100"));
        stop.send(()).unwrap();
        (&mut task.0).await.unwrap().unwrap();
        let _ = client.write_all(&[b'x'; 100]).await;
        assert_eq!(client.read(&mut [0; 1]).await.unwrap_or(0), 0);
        assert!(calls.try_recv().is_err());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn shutdown_cancels_pending_websocket_handshake_before_registering_tunnel() {
    timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/ws", listener.local_addr().unwrap());
        let (entered, mut seen) = mpsc::unbounded_channel();
        let upstream = Router::new().fallback(move || {
            let entered = entered.clone();
            async move {
                entered.send(()).unwrap();
                std::future::pending::<StatusCode>().await
            }
        });
        let _upstream = Task(tokio::spawn(async {
            axum::serve(listener, upstream).await.unwrap();
        }));
        let mut config = config();
        config.upstreams.indexer_ws = Some(url.parse().unwrap());
        config.server.shutdown_timeout_ms = 20;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let mut task = Task(tokio::spawn(server::serve(listener, config, async {
            let _ = stopped.await;
        })));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut request = Task(tokio::spawn(async move {
            client
                .get(format!("http://{address}/ws"))
                .header("connection", "upgrade")
                .header("upgrade", "websocket")
                .header("sec-websocket-version", "13")
                .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
                .send()
                .await
        }));
        seen.recv().await.unwrap();
        stop.send(()).unwrap();
        (&mut task.0).await.unwrap().unwrap();
        assert!((&mut request.0).await.unwrap().is_err());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn repeated_short_connections_release_capacity_and_allow_shutdown() {
    timeout(Duration::from_secs(5), async {
        let mut config = config();
        config.http.max_connections = 4;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let mut task = Task(tokio::spawn(server::serve(listener, config, async {
            let _ = stopped.await;
        })));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        for _ in 0..64 {
            let response = client
                .get(format!("http://{address}/healthz"))
                .header("connection", "close")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert_eq!(response.text().await.unwrap(), "ok");
        }
        stop.send(()).unwrap();
        (&mut task.0).await.unwrap().unwrap();
    })
    .await
    .unwrap();
}
