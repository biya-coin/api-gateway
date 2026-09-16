use std::{future::pending, sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Version},
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::{timeout, Instant},
};
use tokio_tungstenite::{
    tungstenite::{
        self,
        protocol::{frame::coding::CloseCode, CloseFrame, Role, WebSocketConfig},
        Message,
    },
    WebSocketStream,
};

use crate::{
    error::GatewayError,
    headers, http_proxy,
    routing::{websocket_backend, InfoBackend},
    server::Gateway,
};

type ClientSocket = WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>;
type UpstreamSocket = WebSocketStream<reqwest::Upgraded>;

pub(crate) async fn proxy(State(gateway): State<Arc<Gateway>>, mut request: Request) -> Response {
    match handshake(gateway, &mut request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn handshake(gateway: Arc<Gateway>, request: &mut Request) -> Result<Response, GatewayError> {
    let key = validate_request(request)?;
    if gateway.config.upstreams.indexer_ws.is_none() && gateway.config.upstreams.state_ws.is_none()
    {
        return Err(GatewayError::unavailable());
    }
    let permit = gateway.ws_slots.clone().try_acquire_owned().map_err(|_| {
        GatewayError(
            StatusCode::SERVICE_UNAVAILABLE,
            "websocket_capacity_reached",
        )
    })?;
    let mut forwarded = headers::end_to_end(request.headers(), true);
    // These are separate WebSocket connections, not an end-to-end byte tunnel.
    // Compression and subprotocols are deliberately not negotiated on either leg.
    for name in [
        "sec-websocket-key",
        "sec-websocket-accept",
        "sec-websocket-protocol",
        "sec-websocket-extensions",
    ] {
        forwarded.remove(name);
    }
    if let Some(peer) = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
    {
        if let Ok(ip) = peer.0.ip().to_string().parse() {
            forwarded.insert("x-forwarded-for", ip);
        }
    }
    let query = request.uri().query().map(str::to_owned);
    let client = hyper::upgrade::on(request);
    let stopping = gateway.stopping.clone();
    let task_gateway = gateway.clone();
    gateway.tasks.spawn(async move {
        let _permit = permit;
        let handshake_timeout =
            Duration::from_millis(task_gateway.config.websocket.handshake_timeout_ms);
        let upgraded = tokio::select! {
            () = stopping.cancelled() => return,
            result = timeout(handshake_timeout, client) => match result {
                Ok(Ok(client)) => client,
                _ => return,
            },
        };
        let socket = WebSocketStream::from_raw_socket(
            TokioIo::new(upgraded),
            Role::Server,
            Some(socket_config(&task_gateway)),
        )
        .await;
        // Cancellation covers all pending reads, lazy handshakes and bounded writes.
        tokio::select! {
            () = stopping.cancelled() => {},
            () = relay(task_gateway, socket, forwarded, query) => {},
        }
    });
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header("sec-websocket-accept", accept(&key))
        .body(Body::empty())
        .expect("static WebSocket response"))
}

fn socket_config(gateway: &Gateway) -> WebSocketConfig {
    let cfg = &gateway.config.websocket;
    WebSocketConfig::default()
        .read_buffer_size(cfg.tunnel_buffer_bytes)
        .write_buffer_size(0)
        .max_write_buffer_size(cfg.max_message_bytes + 1024)
        .max_message_size(Some(cfg.max_message_bytes))
        .max_frame_size(Some(cfg.max_message_bytes))
}

#[derive(Deserialize)]
struct Subscription<'a> {
    #[serde(rename = "type", borrow)]
    kind: std::borrow::Cow<'a, str>,
}

#[derive(Deserialize)]
struct Command<'a> {
    #[serde(borrow)]
    method: std::borrow::Cow<'a, str>,
    #[serde(borrow)]
    subscription: Option<&'a serde_json::value::RawValue>,
}

// Inspect only routing fields; never reserialize the original message sent downstream.
fn destination(text: &str) -> Result<Option<InfoBackend>, &'static str> {
    if !text.trim_start().starts_with('{') {
        return Err("invalid_websocket_message");
    }
    let command: Command<'_> =
        serde_json::from_str(text).map_err(|_| "invalid_websocket_message")?;
    match command.method.as_ref() {
        "ping" => Ok(None),
        "subscribe" | "unsubscribe" => {
            let raw = command
                .subscription
                .ok_or("invalid_websocket_subscription")?
                .get();
            if !raw.starts_with('{') {
                return Err("invalid_websocket_subscription");
            }
            let subscription: Subscription<'_> =
                serde_json::from_str(raw).map_err(|_| "invalid_websocket_subscription")?;
            Ok(Some(websocket_backend(&subscription.kind)))
        }
        _ => Err("unsupported_websocket_method"),
    }
}

async fn relay(
    gateway: Arc<Gateway>,
    mut client: ClientSocket,
    headers: HeaderMap,
    query: Option<String>,
) {
    let mut indexer: Option<UpstreamSocket> = None;
    let mut state: Option<UpstreamSocket> = None;
    let write_timeout = Duration::from_millis(gateway.config.http.write_timeout_ms);
    let idle = gateway.config.websocket.idle_timeout_ms;
    let mut last_activity = Instant::now();
    loop {
        let (source, incoming) = tokio::select! {
            message = client.next() => (None, message),
            message = next_upstream(&mut indexer) => (Some(InfoBackend::Indexer), message),
            message = next_upstream(&mut state) => (Some(InfoBackend::State), message),
            () = idle_deadline(last_activity, idle) => return,
        };
        last_activity = Instant::now();
        let message = match incoming {
            Some(Ok(message)) => message,
            other => {
                let code = if matches!(other, Some(Err(tungstenite::Error::Capacity(_)))) {
                    CloseCode::Size
                } else if source.is_none()
                    && matches!(
                        other,
                        Some(Err(
                            tungstenite::Error::Protocol(_) | tungstenite::Error::Utf8(_)
                        ))
                    )
                {
                    CloseCode::Protocol
                } else {
                    CloseCode::Error
                };
                let reason = if source.is_some() {
                    "upstream_disconnected"
                } else {
                    "invalid_websocket_frame"
                };
                let _ = send(
                    &mut client,
                    Message::Close(Some(CloseFrame {
                        code,
                        reason: reason.into(),
                    })),
                    write_timeout,
                )
                .await;
                return;
            }
        };
        if let Some(backend) = source {
            let upstream = match backend {
                InfoBackend::Indexer => indexer.as_mut().unwrap(),
                InfoBackend::State => state.as_mut().unwrap(),
            };
            match message {
                Message::Ping(_) => {
                    if !flush(upstream, write_timeout).await {
                        return;
                    }
                }
                Message::Pong(_) => {}
                Message::Close(frame) => {
                    let _ = flush(upstream, write_timeout).await;
                    let _ = send(&mut client, Message::Close(frame), write_timeout).await;
                    return;
                }
                message => {
                    if !send(&mut client, message, write_timeout).await {
                        return;
                    }
                }
            }
            continue;
        }
        match message {
            Message::Text(text) => {
                let backend = match destination(&text) {
                    Ok(Some(backend)) => backend,
                    Ok(None) => {
                        // JSON heartbeat belongs to the frontend connection: exactly one reply.
                        if !send(
                            &mut client,
                            Message::text(r#"{"channel":"pong"}"#),
                            write_timeout,
                        )
                        .await
                        {
                            return;
                        }
                        // Keep both existing upstream legs alive without duplicating JSON pong responses.
                        for socket in [&mut indexer, &mut state].into_iter().flatten() {
                            if !send(socket, Message::Ping(Default::default()), write_timeout).await
                            {
                                return;
                            }
                        }
                        continue;
                    }
                    Err(code) => {
                        if !send_error(&mut client, code, None, write_timeout).await {
                            return;
                        }
                        continue;
                    }
                };
                let slot = match backend {
                    InfoBackend::Indexer => &mut indexer,
                    InfoBackend::State => &mut state,
                };
                if slot.is_none() {
                    match connect(&gateway, backend, &headers, query.as_deref()).await {
                        Ok(socket) => *slot = Some(socket),
                        Err(error) => {
                            if !send_error(&mut client, error.1, Some(backend), write_timeout).await
                            {
                                return;
                            }
                            continue;
                        }
                    }
                }
                if !send(slot.as_mut().unwrap(), Message::Text(text), write_timeout).await {
                    let _ = send(
                        &mut client,
                        Message::Close(Some(CloseFrame {
                            code: CloseCode::Error,
                            reason: "upstream_write_failed".into(),
                        })),
                        write_timeout,
                    )
                    .await;
                    return;
                }
            }
            Message::Ping(_) => {
                if !flush(&mut client, write_timeout).await {
                    return;
                }
            }
            Message::Pong(_) => {}
            Message::Close(frame) => {
                let _ = flush(&mut client, write_timeout).await;
                // Bound total close fanout; no background tasks or orphan upstream connections.
                for socket in [&mut indexer, &mut state].into_iter().flatten() {
                    let _ = send(socket, Message::Close(frame.clone()), write_timeout).await;
                }
                return;
            }
            Message::Binary(_) | Message::Frame(_) => {
                let _ = send(
                    &mut client,
                    Message::Close(Some(CloseFrame {
                        code: CloseCode::Unsupported,
                        reason: "text_subscriptions_required".into(),
                    })),
                    write_timeout,
                )
                .await;
                return;
            }
        }
    }
}

async fn next_upstream(
    socket: &mut Option<UpstreamSocket>,
) -> Option<Result<Message, tungstenite::Error>> {
    match socket {
        Some(socket) => socket.next().await,
        None => pending().await,
    }
}

async fn connect(
    gateway: &Gateway,
    backend: InfoBackend,
    forwarded: &HeaderMap,
    query: Option<&str>,
) -> Result<UpstreamSocket, GatewayError> {
    let endpoint = match backend {
        InfoBackend::State => &gateway.config.upstreams.state_ws,
        InfoBackend::Indexer => &gateway.config.upstreams.indexer_ws,
    }
    .as_ref()
    .ok_or_else(GatewayError::unavailable)?;
    let mut endpoint = http_proxy::ws_http_url(endpoint);
    endpoint.set_query(query);
    let key = tungstenite::handshake::client::generate_key();
    let mut headers = forwarded.clone();
    headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    headers.insert("sec-websocket-version", HeaderValue::from_static("13"));
    headers.insert(
        "sec-websocket-key",
        key.parse().expect("generated ASCII key"),
    );
    // Reuse the HTTP transport's TLS roots, proxy/redirect/retry policy and connect timeout.
    timeout(
        Duration::from_millis(gateway.config.websocket.handshake_timeout_ms),
        async {
            let upstream = gateway
                .client
                .get(endpoint)
                .headers(headers)
                .send()
                .await
                .map_err(GatewayError::upstream)?;
            if upstream.status() != StatusCode::SWITCHING_PROTOCOLS {
                return Err(GatewayError(
                    StatusCode::BAD_GATEWAY,
                    "upstream_handshake_rejected",
                ));
            }
            validate_response(upstream.headers(), &key)?;
            let socket = upstream.upgrade().await.map_err(GatewayError::upstream)?;
            Ok(
                WebSocketStream::from_raw_socket(
                    socket,
                    Role::Client,
                    Some(socket_config(gateway)),
                )
                .await,
            )
        },
    )
    .await
    .map_err(|_| GatewayError::timeout())?
}

async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    message: Message,
    duration: Duration,
) -> bool {
    matches!(timeout(duration, socket.send(message)).await, Ok(Ok(())))
}

async fn flush<S: AsyncRead + AsyncWrite + Unpin>(
    socket: &mut WebSocketStream<S>,
    duration: Duration,
) -> bool {
    matches!(timeout(duration, socket.flush()).await, Ok(Ok(())))
}

async fn send_error(
    client: &mut ClientSocket,
    code: &str,
    backend: Option<InfoBackend>,
    duration: Duration,
) -> bool {
    let backend = backend.map(|backend| match backend {
        InfoBackend::State => "state",
        InfoBackend::Indexer => "indexer",
    });
    let message = serde_json::json!({"channel":"error","data":{"error":code,"backend":backend}});
    send(client, Message::text(message.to_string()), duration).await
}

fn accept(key: &str) -> String {
    STANDARD.encode(Sha1::digest(format!(
        "{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
    )))
}

fn validate_request(request: &Request) -> Result<String, GatewayError> {
    let headers = request.headers();
    let invalid = || GatewayError(StatusCode::BAD_REQUEST, "invalid_websocket_handshake");
    if request.method() != axum::http::Method::GET
        || request.version() != Version::HTTP_11
        || !headers::has_token(headers, "connection", "upgrade")
        || !headers::has_token(headers, "upgrade", "websocket")
        || headers.get_all("sec-websocket-version").iter().count() != 1
        || headers
            .get("sec-websocket-version")
            .is_none_or(|value| value != "13")
        || headers.contains_key(header::TRANSFER_ENCODING)
        || headers
            .get(header::CONTENT_LENGTH)
            .is_some_and(|value| value != "0")
        || headers.get_all("sec-websocket-key").iter().count() != 1
    {
        return Err(invalid());
    }
    let key = headers
        .get("sec-websocket-key")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(invalid)?;
    if STANDARD.decode(key).map_or(true, |value| value.len() != 16) {
        return Err(invalid());
    }
    Ok(key.to_owned())
}

fn validate_response(upstream: &HeaderMap, key: &str) -> Result<(), GatewayError> {
    if !headers::has_token(upstream, "connection", "upgrade")
        || !headers::has_token(upstream, "upgrade", "websocket")
        || upstream.get_all("sec-websocket-accept").iter().count() != 1
        || upstream
            .get("sec-websocket-accept")
            .and_then(|value| value.to_str().ok())
            != Some(accept(key).as_str())
        || upstream.contains_key("sec-websocket-protocol")
        || upstream.contains_key("sec-websocket-extensions")
    {
        return Err(GatewayError(
            StatusCode::BAD_GATEWAY,
            "invalid_upstream_handshake",
        ));
    }
    Ok(())
}

async fn idle_deadline(last_activity: Instant, millis: u64) {
    if millis == 0 {
        pending().await
    } else {
        tokio::time::sleep_until(last_activity + Duration::from_millis(millis)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_decodes_escaped_types_without_rewriting_payload() {
        assert_eq!(
            destination(r#"{"method":"subscribe","subscription":{"type":"asset\u0043txs"}}"#),
            Ok(Some(InfoBackend::State))
        );
        assert_eq!(
            destination(r#"{"method":"unsubscribe","subscription":{"type":"clearinghouseState"}}"#),
            Ok(Some(InfoBackend::State))
        );
        assert_eq!(
            destination(r#"{"method":"subscribe","subscription":{"type":"openOrders"}}"#),
            Ok(Some(InfoBackend::Indexer))
        );
    }

    #[tokio::test]
    async fn slow_peer_cannot_stall_message_write_indefinitely() {
        let (transport, _unread) = tokio::io::duplex(16);
        let mut socket = WebSocketStream::from_raw_socket(transport, Role::Server, None).await;
        let written = timeout(
            Duration::from_secs(2),
            send(
                &mut socket,
                Message::text("x".repeat(1024)),
                Duration::from_millis(20),
            ),
        )
        .await
        .unwrap();
        assert!(!written);
    }
}
