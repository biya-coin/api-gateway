use axum::{
    http::header,
    response::{Html, IntoResponse},
};

/// Self-built API portal (single offline HTML file, no CDN, no UI framework).
/// Produced by `scripts/render-portal.py` from the merged spec; regenerate on
/// every release, never hand-edit output.
pub(crate) const PORTAL_HTML: &str = include_str!("../docs/.generated/portal.html");

/// Merged OpenAPI spec, kept for tooling and SDK generation.
pub(crate) const OPENAPI_JSON: &str = include_str!("../docs/.generated/openapi.json");

/// Human portal: left nav per service, per-item detail with prefilled tests.
pub(crate) async fn docs_page() -> Html<&'static str> {
    Html(PORTAL_HTML)
}

/// Raw merged spec for tooling and SDK generation.
pub(crate) async fn openapi_json() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/json")], OPENAPI_JSON)
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use crate::config::Config;
    use crate::server::router;

    fn config() -> Config {
        let mut config = Config::parse(include_str!("../config/default.toml")).unwrap();
        config.upstreams = Default::default();
        config
    }

    #[tokio::test]
    async fn openapi_spec_serves_three_backend_tags() {
        let response = router(config())
            .unwrap()
            .oneshot(Request::get("/openapi.json").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
        let spec: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let tags: Vec<&str> = spec["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tag| tag["name"].as_str().unwrap())
            .collect();
        assert_eq!(tags, ["Exchange", "State", "Indexer"]);
        assert!(spec["x-backend-revs"].as_object().unwrap().len() == 3);
        let servers: Vec<&str> = spec["servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|server| server["url"].as_str().unwrap())
            .collect();
        assert!(servers.contains(&"https://dev.dex-api.biya.io"));
    }

    #[tokio::test]
    async fn docs_page_serves_self_built_portal() {
        let response = router(config())
            .unwrap()
            .oneshot(Request::get("/docs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        for marker in [
            "BIYA DEX API",
            "nav-search",
            "post-State-orderStatus",
            "ws-Indexer-userHistoricalOrders",
            "TypeScript",
        ] {
            assert!(html.contains(marker), "portal marker missing: {marker}");
        }
        for banned in ["cdn.jsdelivr.net", "Scalar", "$spec"] {
            assert!(
                !html.contains(banned),
                "portal must be offline/self-built: {banned}"
            );
        }
        // Fragments are trimmed to the gateway-routed scope: no item may
        // render a routed-elsewhere hint (CSS class definitions excluded).
        assert!(
            !html.contains("nav-hint warn") && !html.contains("nav-hint off"),
            "routed-elsewhere item leaked into portal"
        );
    }
}
