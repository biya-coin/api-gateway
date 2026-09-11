use axum::http::{header, HeaderMap, HeaderName};

pub(crate) fn has_token(headers: &HeaderMap, name: &str, token: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim().eq_ignore_ascii_case(token))
}

/// Strip hop-by-hop fields, including fields nominated by any Connection header.
pub(crate) fn end_to_end(source: &HeaderMap, request: bool) -> HeaderMap {
    let mut headers = source.clone();
    for connection in source.get_all(header::CONNECTION) {
        if let Ok(value) = connection.to_str() {
            for token in value.split(',') {
                if let Ok(name) = HeaderName::from_bytes(token.trim().as_bytes()) {
                    headers.remove(name);
                }
            }
        }
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "content-length",
    ] {
        headers.remove(name);
    }
    let remove: Vec<_> = headers
        .keys()
        .filter(|name| {
            let name = name.as_str();
            if request {
                name == "host"
                    || name == "forwarded"
                    || name == "x-real-ip"
                    || name.starts_with("x-forwarded-")
            } else {
                // The gateway, not a permissive upstream, owns browser access policy.
                name.starts_with("access-control-")
            }
        })
        .cloned()
        .collect();
    for name in remove {
        headers.remove(name);
    }
    headers
}
