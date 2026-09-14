use std::{net::SocketAddr, path::Path};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing_subscriber::EnvFilter;
use url::Url;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub logging: LoggingConfig,
    #[serde(default)]
    pub upstreams: Upstreams,
    #[serde(default)]
    pub http: HttpConfig,
    #[serde(default)]
    pub websocket: WebSocketConfig,
    #[serde(default)]
    pub access: AccessConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub listen_addr: SocketAddr,
    #[serde(default = "default_shutdown_timeout")]
    pub shutdown_timeout_ms: u64,
}

fn default_shutdown_timeout() -> u64 {
    5_000
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    pub filter: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Upstreams {
    pub state_info: Option<Url>,
    pub indexer_info: Option<Url>,
    pub exchange: Option<Url>,
    pub indexer_ws: Option<Url>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpConfig {
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
    pub body_timeout_ms: u64,
    pub write_timeout_ms: u64,
    pub max_connections: usize,
    pub max_request_body_bytes: usize,
    pub max_response_body_bytes: usize,
    pub max_in_flight: usize,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 3_000,
            request_timeout_ms: 10_000,
            body_timeout_ms: 5_000,
            write_timeout_ms: 10_000,
            max_connections: 1_024,
            max_request_body_bytes: 4 * 1024 * 1024,
            max_response_body_bytes: 16 * 1024 * 1024,
            max_in_flight: 128,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebSocketConfig {
    pub handshake_timeout_ms: u64,
    pub max_connections: usize,
    pub tunnel_buffer_bytes: usize,
    /// Zero disables the idle timeout: quiet subscriptions are not failures.
    pub idle_timeout_ms: u64,
}

impl Default for WebSocketConfig {
    fn default() -> Self {
        Self {
            handshake_timeout_ms: 5_000,
            max_connections: 1_024,
            tunnel_buffer_bytes: 8_192,
            idle_timeout_ms: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessConfig {
    /// Empty disallows browser Origin requests; non-browser clients are unaffected.
    pub allowed_origins: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config: {}", path.display()))?;
        Self::parse(&contents).with_context(|| format!("invalid config: {}", path.display()))
    }

    pub fn parse(contents: &str) -> Result<Self> {
        let config: Self = toml::from_str(contents).context("invalid TOML configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.logging.filter.trim().is_empty(),
            "logging.filter must not be empty"
        );
        EnvFilter::try_new(&self.logging.filter).context("invalid logging.filter")?;
        for (name, endpoint, schemes) in [
            (
                "state_info",
                &self.upstreams.state_info,
                &["http", "https"][..],
            ),
            (
                "indexer_info",
                &self.upstreams.indexer_info,
                &["http", "https"][..],
            ),
            ("exchange", &self.upstreams.exchange, &["http", "https"][..]),
            ("indexer_ws", &self.upstreams.indexer_ws, &["ws", "wss"][..]),
        ] {
            if let Some(url) = endpoint {
                anyhow::ensure!(
                    schemes.contains(&url.scheme())
                        && url.host_str().is_some()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.fragment().is_none()
                        && url.query().is_none(),
                    "upstreams.{name} must be a full endpoint URL without credentials, query or fragment"
                );
            }
        }
        anyhow::ensure!(
            self.server.shutdown_timeout_ms > 0
                && self.http.connect_timeout_ms > 0
                && self.http.request_timeout_ms > 0
                && self.http.body_timeout_ms > 0
                && self.http.write_timeout_ms > 0
                && self.websocket.handshake_timeout_ms > 0,
            "timeouts must be positive (except websocket.idle_timeout_ms)"
        );
        anyhow::ensure!(
            [
                self.server.shutdown_timeout_ms,
                self.http.connect_timeout_ms,
                self.http.request_timeout_ms,
                self.http.body_timeout_ms,
                self.http.write_timeout_ms,
                self.websocket.handshake_timeout_ms,
                self.websocket.idle_timeout_ms
            ]
            .into_iter()
            .all(|millis| millis <= 86_400_000),
            "timeouts must not exceed 24 hours"
        );
        anyhow::ensure!(
            (1..=65_536).contains(&self.http.max_in_flight)
                && (1..=65_536).contains(&self.http.max_connections)
                && (1..=65_536).contains(&self.websocket.max_connections),
            "connection and concurrency limits must be between 1 and 65536"
        );
        anyhow::ensure!(
            (1..=256 * 1024 * 1024).contains(&self.http.max_request_body_bytes)
                && (1..=256 * 1024 * 1024).contains(&self.http.max_response_body_bytes)
                && (1..=1024 * 1024).contains(&self.websocket.tunnel_buffer_bytes),
            "body limits must be 1..256MiB; tunnel buffer must be 1..1MiB"
        );
        for origin in &self.access.allowed_origins {
            if origin == "*" {
                anyhow::ensure!(
                    self.access.allowed_origins.len() == 1,
                    "origin wildcard must be used alone"
                );
                continue;
            }
            let url = Url::parse(origin).context("invalid allowed origin")?;
            anyhow::ensure!(
                matches!(url.scheme(), "http" | "https")
                    && url.origin().ascii_serialization() == *origin,
                "allowed origins must be canonical HTTP(S) origins without path or credentials"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../config/default.toml");

    #[test]
    fn default_config_is_valid() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert_eq!(config.server.listen_addr, "127.0.0.1:8080".parse().unwrap());
        assert_eq!(config.logging.filter, "info");
        assert_eq!(
            config.upstreams.indexer_info.as_ref().map(Url::as_str),
            Some("http://localhost:9090/info")
        );
        assert_eq!(
            config.upstreams.indexer_ws.as_ref().map(Url::as_str),
            Some("ws://localhost:9090/ws")
        );
        assert_eq!(
            config.upstreams.state_info.as_ref().map(Url::as_str),
            Some("http://localhost:3300/info")
        );
        assert_eq!(
            config.upstreams.exchange.as_ref().map(Url::as_str),
            Some("http://localhost:18080/exchange")
        );
        assert_eq!(
            config.access.allowed_origins,
            ["http://localhost:8080", "http://127.0.0.1:8080"]
        );
    }

    #[test]
    fn rejects_invalid_listen_address() {
        assert!(Config::parse(&EXAMPLE.replace("127.0.0.1:8080", "not-an-address")).is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(Config::parse(&EXAMPLE.replace("listen_addr", "listen_adrr")).is_err());
    }

    #[test]
    fn rejects_invalid_or_empty_log_filter() {
        for filter in ["", "   ", "api_gateway=not-a-level"] {
            assert!(Config::parse(&EXAMPLE.replace("\"info\"", &format!("{filter:?}"))).is_err());
        }
    }

    #[test]
    fn rejects_missing_sections() {
        assert!(Config::parse("").is_err());
    }

    #[test]
    fn upstreams_must_use_expected_schemes_without_embedded_secrets() {
        for endpoint in [
            "ftp://localhost/info",
            "http://user:secret@localhost/info",
            "http://localhost/info?token=secret",
            "http://localhost/info#fragment",
        ] {
            let mut config = Config::parse(EXAMPLE).unwrap();
            config.upstreams.state_info = Some(endpoint.parse().unwrap());
            assert!(config.validate().is_err());
        }
        let mut config = Config::parse(EXAMPLE).unwrap();
        config.upstreams.indexer_ws = Some("https://localhost/ws".parse().unwrap());
        assert!(config.validate().is_err());
        config.upstreams.indexer_ws = Some("wss://localhost/ws".parse().unwrap());
        config.upstreams.state_info = Some("https://localhost/prefix/info".parse().unwrap());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn exchange_requires_http_and_can_be_omitted() {
        let mut config = Config::parse(EXAMPLE).unwrap();
        config.upstreams.exchange = Some("ws://localhost:18080/exchange".parse().unwrap());
        assert!(config.validate().is_err());
        config.upstreams.exchange = None;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_zero_limits_and_excessive_timeouts() {
        let mut config = Config::parse(EXAMPLE).unwrap();
        config.http.max_in_flight = 0;
        assert!(config.validate().is_err());
        config.http.max_in_flight = 1;
        config.websocket.tunnel_buffer_bytes = 0;
        assert!(config.validate().is_err());
        config.websocket.tunnel_buffer_bytes = 1024;
        config.websocket.idle_timeout_ms = u64::MAX;
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_non_origin_urls_and_mixed_wildcard() {
        for origins in [
            vec!["*", "https://dex.example"],
            vec!["https://dex.example/"],
            vec!["null"],
            vec!["https://user:password@dex.example"],
        ] {
            let mut config = Config::parse(EXAMPLE).unwrap();
            config.access.allowed_origins = origins.into_iter().map(String::from).collect();
            assert!(config.validate().is_err());
        }
        let mut config = Config::parse(EXAMPLE).unwrap();
        config.access.allowed_origins = vec!["*".into()];
        assert!(config.validate().is_ok());
    }
}
