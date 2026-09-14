# syntax=docker/dockerfile:1

# May be overridden with an equivalent image from the company's registry.
ARG RUST_IMAGE=rust:1.97.1-bookworm
ARG RUNTIME_IMAGE=debian:bookworm-slim

FROM ${RUST_IMAGE} AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin api-gateway -j 2

FROM ${RUNTIME_IMAGE} AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder --chmod=0555 /build/target/release/api-gateway /usr/local/bin/api-gateway
COPY --chmod=0444 config/default.toml /app/config/default.toml
# COPY may create destination directories with restrictive permissions.
RUN chmod 0755 /app /app/config

USER 10001:10001
EXPOSE 8888/tcp
STOPSIGNAL SIGTERM

# Validate the executable and packaged configuration without starting a listener.
RUN ["/usr/local/bin/api-gateway", "--config", "/app/config/default.toml", "--check-config"]

# Liveness only. HTTP and WebSocket share 8888; no second port is needed.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s --retries=3 \
    CMD ["curl", "--fail", "--silent", "--show-error", "--max-time", "3", "--noproxy", "*", "http://127.0.0.1:8888/healthz"]

ENTRYPOINT ["/usr/local/bin/api-gateway"]
CMD ["--config", "/app/config/default.toml"]
