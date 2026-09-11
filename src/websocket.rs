use std::{sync::Arc, time::Duration};

use axum::{
    body::Body,
    extract::{ConnectInfo, Request, State},
    http::{header, HeaderMap, HeaderValue, StatusCode, Version},
    response::{IntoResponse, Response},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use hyper_util::rt::TokioIo;
use sha1::{Digest, Sha1};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::watch,
    time::{timeout, Instant},
};

use crate::{error::GatewayError, headers, http_proxy, server::Gateway};

pub(crate) async fn proxy(State(gateway): State<Arc<Gateway>>, mut request: Request) -> Response {
    match handshake(gateway, &mut request).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    }
}

async fn handshake(gateway: Arc<Gateway>, request: &mut Request) -> Result<Response, GatewayError> {
    let key = validate_request(request)?;
    let endpoint = gateway
        .config
        .upstreams
        .indexer_ws
        .as_ref()
        .ok_or_else(GatewayError::unavailable)?;
    let permit = gateway.ws_slots.clone().try_acquire_owned().map_err(|_| {
        GatewayError(
            StatusCode::SERVICE_UNAVAILABLE,
            "websocket_capacity_reached",
        )
    })?;
    let mut endpoint = http_proxy::ws_http_url(endpoint);
    endpoint.set_query(request.uri().query());
    let mut forwarded = headers::end_to_end(request.headers(), true);
    forwarded.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    forwarded.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    if let Some(peer) = request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
    {
        if let Ok(ip) = peer.0.ip().to_string().parse() {
            forwarded.insert("x-forwarded-for", ip);
        }
    }
    let handshake_timeout = Duration::from_millis(gateway.config.websocket.handshake_timeout_ms);
    let started = Instant::now();
    let upstream = timeout(
        handshake_timeout,
        gateway.client.get(endpoint).headers(forwarded).send(),
    )
    .await
    .map_err(|_| GatewayError::timeout())?
    .map_err(GatewayError::upstream)?;
    let remaining = handshake_timeout.saturating_sub(started.elapsed());
    if upstream.status() != StatusCode::SWITCHING_PROTOCOLS {
        // Do not acknowledge a WebSocket when the downstream returned ordinary success.
        if upstream.status().is_success() || upstream.status().is_informational() {
            return Err(GatewayError(
                StatusCode::BAD_GATEWAY,
                "invalid_upstream_handshake",
            ));
        }
        return timeout(
            remaining,
            http_proxy::buffered_response(
                upstream,
                gateway.config.http.max_response_body_bytes,
                Some(permit),
            ),
        )
        .await
        .map_err(|_| GatewayError::timeout())?;
    }
    validate_response(upstream.headers(), request.headers(), &key)?;
    let mut response_headers = headers::end_to_end(upstream.headers(), false);
    response_headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    response_headers.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    let mut upstream = timeout(remaining, upstream.upgrade())
        .await
        .map_err(|_| GatewayError::timeout())?
        .map_err(GatewayError::upstream)?;
    let client = hyper::upgrade::on(request);
    let stopping = gateway.stopping.clone();
    let buffer = gateway.config.websocket.tunnel_buffer_bytes;
    let idle = gateway.config.websocket.idle_timeout_ms;
    gateway.tasks.spawn(async move {
        let _permit = permit;
        let upgraded = tokio::select! {
            () = stopping.cancelled() => return,
            result = timeout(handshake_timeout, client) => match result {
                Ok(Ok(client)) => client,
                _ => return,
            },
        };
        let mut client = TokioIo::new(upgraded);
        let (activity, observed) = watch::channel(Instant::now());
        let transfer = async {
            let (client_read, client_write) = tokio::io::split(&mut client);
            let (upstream_read, upstream_write) = tokio::io::split(&mut upstream);
            // Either endpoint ending its transport closes the other endpoint too.
            // Frames (including close/ping/pong, masking and compression) are not decoded.
            tokio::select! {
                result = pump(client_read, upstream_write, buffer, activity.clone()) => result,
                result = pump(upstream_read, client_write, buffer, activity) => result,
            }
        };
        tokio::select! {
            result = transfer => {
                if result.is_err() { tracing::debug!("WebSocket transport closed with an I/O error"); }
            },
            () = stopping.cancelled() => {},
            () = idle_deadline(observed, idle) => { tracing::debug!("WebSocket idle deadline reached"); },
        }
    });
    Ok(http_proxy::response(
        StatusCode::SWITCHING_PROTOCOLS,
        response_headers,
        Body::empty(),
    ))
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

fn validate_response(
    upstream: &HeaderMap,
    client: &HeaderMap,
    key: &str,
) -> Result<(), GatewayError> {
    let invalid = || GatewayError(StatusCode::BAD_GATEWAY, "invalid_upstream_handshake");
    let expected = STANDARD.encode(Sha1::digest(format!(
        "{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
    )));
    if !headers::has_token(upstream, "connection", "upgrade")
        || !headers::has_token(upstream, "upgrade", "websocket")
        || upstream.get_all("sec-websocket-accept").iter().count() != 1
        || upstream
            .get("sec-websocket-accept")
            .and_then(|value| value.to_str().ok())
            != Some(expected.as_str())
    {
        return Err(invalid());
    }
    if let Some(protocol) = upstream.get("sec-websocket-protocol") {
        let protocol = protocol.to_str().map_err(|_| invalid())?;
        if upstream.get_all("sec-websocket-protocol").iter().count() != 1
            || !client
                .get_all("sec-websocket-protocol")
                .iter()
                .filter_map(|value| value.to_str().ok())
                .flat_map(|value| value.split(','))
                .any(|offered| offered.trim() == protocol)
        {
            return Err(invalid());
        }
    }
    if upstream.contains_key("sec-websocket-extensions")
        && !client.contains_key("sec-websocket-extensions")
    {
        return Err(invalid());
    }
    Ok(())
}

async fn pump(
    mut reader: impl AsyncRead + Unpin,
    mut writer: impl AsyncWrite + Unpin,
    buffer_size: usize,
    activity: watch::Sender<Instant>,
) -> std::io::Result<()> {
    let mut buffer = vec![0; buffer_size];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(());
        }
        activity.send_replace(Instant::now());
        writer.write_all(&buffer[..count]).await?;
        writer.flush().await?;
        activity.send_replace(Instant::now());
    }
}

async fn idle_deadline(mut activity: watch::Receiver<Instant>, millis: u64) {
    if millis == 0 {
        return std::future::pending().await;
    }
    loop {
        let deadline = *activity.borrow_and_update() + Duration::from_millis(millis);
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => return,
            changed = activity.changed() => if changed.is_err() { return; },
        }
    }
}
