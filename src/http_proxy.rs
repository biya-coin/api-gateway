use std::{sync::Arc, time::Duration};

use axum::{
    body::{to_bytes, Body},
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use tokio::{sync::OwnedSemaphorePermit, time::timeout};
use url::Url;

use crate::{
    error::GatewayError,
    headers,
    routing::{info_backend, InfoBackend},
    server::Gateway,
};

pub(crate) async fn info(State(gateway): State<Arc<Gateway>>, request: Request) -> Response {
    handle(gateway, request, true)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

pub(crate) async fn exchange(State(gateway): State<Arc<Gateway>>, request: Request) -> Response {
    handle(gateway, request, false)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

async fn handle(
    gateway: Arc<Gateway>,
    request: Request,
    is_info: bool,
) -> Result<Response, GatewayError> {
    let permit = gateway
        .http_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| GatewayError(StatusCode::SERVICE_UNAVAILABLE, "gateway_busy"))?;
    let (parts, body) = request.into_parts();
    let body = timeout(
        Duration::from_millis(gateway.config.http.body_timeout_ms),
        to_bytes(body, gateway.config.http.max_request_body_bytes),
    )
    .await
    .map_err(|_| GatewayError(StatusCode::REQUEST_TIMEOUT, "request_body_timeout"))?
    .map_err(|error| {
        use std::error::Error;
        if error
            .source()
            .is_some_and(|source| source.is::<http_body_util::LengthLimitError>())
        {
            GatewayError(StatusCode::PAYLOAD_TOO_LARGE, "request_body_too_large")
        } else {
            GatewayError(StatusCode::BAD_REQUEST, "invalid_request_body")
        }
    })?;
    let (backend, endpoint) = if is_info {
        match info_backend(&body).ok_or(GatewayError(
            StatusCode::BAD_REQUEST,
            "invalid_or_unknown_info_type",
        ))? {
            InfoBackend::State => ("state", &gateway.config.upstreams.state_info),
            InfoBackend::Indexer => ("indexer", &gateway.config.upstreams.indexer_info),
        }
    } else {
        ("exchange", &gateway.config.upstreams.exchange)
    };
    let endpoint = endpoint.as_ref().ok_or_else(GatewayError::unavailable)?;
    let mut endpoint = endpoint.clone();
    endpoint.set_query(parts.uri.query());
    let mut request_headers = headers::end_to_end(&parts.headers, true);
    if let Some(peer) = parts.extensions.get::<ConnectInfo<std::net::SocketAddr>>() {
        if let Ok(ip) = peer.0.ip().to_string().parse() {
            request_headers.insert("x-forwarded-for", ip);
        }
    }
    let started = std::time::Instant::now();
    let result = timeout(
        Duration::from_millis(gateway.config.http.request_timeout_ms),
        async {
            let response = gateway
                .client
                .post(endpoint)
                .headers(request_headers)
                .body(body)
                .send()
                .await
                .map_err(GatewayError::upstream)?;
            if response.status().is_informational() {
                return Err(GatewayError(
                    StatusCode::BAD_GATEWAY,
                    "unexpected_upstream_upgrade",
                ));
            }
            buffered_response(
                response,
                gateway.config.http.max_response_body_bytes,
                Some(permit),
            )
            .await
        },
    )
    .await
    .map_err(|_| GatewayError::timeout())?;
    let status = result
        .as_ref()
        .map_or_else(|err| err.0.as_u16(), |response| response.status().as_u16());
    tracing::info!(
        backend,
        status,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "HTTP proxy request completed"
    );
    result
}

struct ResponseBuffer {
    bytes: Vec<u8>,
    _permit: Option<OwnedSemaphorePermit>,
}

impl AsRef<[u8]> for ResponseBuffer {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

pub(crate) async fn buffered_response(
    mut upstream: reqwest::Response,
    limit: usize,
    permit: Option<OwnedSemaphorePermit>,
) -> Result<Response, GatewayError> {
    let status = upstream.status();
    let headers = headers::end_to_end(upstream.headers(), false);
    if upstream
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(GatewayError(
            StatusCode::BAD_GATEWAY,
            "upstream_body_too_large",
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = upstream.chunk().await.map_err(GatewayError::upstream)? {
        if chunk.len() > limit.saturating_sub(bytes.len()) {
            return Err(GatewayError(
                StatusCode::BAD_GATEWAY,
                "upstream_body_too_large",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    // Hyper can retain byte slices after polling Body; bind the permit to byte ownership,
    // not the handler or Body wrapper, so slow readers cannot accumulate unbounded buffers.
    let bytes = axum::body::Bytes::from_owner(ResponseBuffer {
        bytes,
        _permit: permit,
    });
    Ok(response(status, headers, Body::from(bytes)))
}

pub(crate) fn response(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

pub(crate) fn ws_http_url(url: &Url) -> Url {
    let mut result = url.clone();
    // Only validated ws/wss URLs reach this conversion.
    result
        .set_scheme(if url.scheme() == "wss" {
            "https"
        } else {
            "http"
        })
        .expect("validated URL scheme");
    result
}
