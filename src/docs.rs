use axum::{
    http::header,
    response::{Html, IntoResponse},
};

/// Merged OpenAPI spec produced by `scripts/merge-openapi.py` from the three
/// backend fragments. Regenerate on every release; never hand-edit output.
pub(crate) const OPENAPI_JSON: &str = include_str!("../docs/.generated/openapi.json");

/// Scalar API reference page. UI assets are embedded in the binary; no CDN.
pub(crate) async fn docs_page() -> Html<String> {
    let spec: serde_json::Value =
        serde_json::from_str(OPENAPI_JSON).expect("embedded openapi.json must parse");
    Html(utoipa_scalar::Scalar::new(spec).to_html())
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
    }

    #[tokio::test]
    async fn docs_page_serves_scalar_ui() {
        let response = router(config())
            .unwrap()
            .oneshot(Request::get("/docs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("scalar"), "Scalar UI marker missing");
        assert!(html.contains("BIYA DEX API"), "merged spec not embedded");
    }
}
