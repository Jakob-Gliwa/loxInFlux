# syntax=docker/dockerfile:1
ARG RUST_VERSION=1.99
FROM rust:${RUST_VERSION}-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=secret,id=build_ca \
    if [ -f /run/secrets/build_ca ]; then export CARGO_HTTP_CAINFO=/run/secrets/build_ca SSL_CERT_FILE=/run/secrets/build_ca; fi; \
    cargo build --release --locked --bin loxinflux

FROM debian:bookworm-slim
WORKDIR /app
ENV CONFIG_DIR=/app/config DATA_DIR=/app/data
COPY --from=builder /build/target/release/loxinflux /usr/local/bin/loxinflux
RUN ln -s /usr/local/bin/loxinflux /usr/local/bin/loxInFlux \
    && mkdir -p /app/config /app/data
VOLUME ["/app/config", "/app/data"]
EXPOSE 9108/tcp
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD ["loxinflux", "--healthcheck"]
ENTRYPOINT ["loxinflux"]
