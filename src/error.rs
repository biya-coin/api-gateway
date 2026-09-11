use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};

pub(crate) struct GatewayError(pub StatusCode, pub &'static str);

impl GatewayError {
    pub(crate) fn unavailable() -> Self {
        Self(StatusCode::SERVICE_UNAVAILABLE, "upstream_not_configured")
    }

    pub(crate) fn upstream(error: reqwest::Error) -> Self {
        // Never log URLs, credentials or upstream bodies from reqwest errors.
        if error.is_timeout() {
            Self::timeout()
        } else {
            Self(StatusCode::BAD_GATEWAY, "upstream_failure")
        }
    }

    pub(crate) fn timeout() -> Self {
        Self(StatusCode::GATEWAY_TIMEOUT, "upstream_timeout")
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}
