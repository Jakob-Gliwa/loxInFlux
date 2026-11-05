# syntax=docker/dockerfile:1
# -------------------------------------
# 1) Build stage
# -------------------------------------
ARG RUST_VERSION=1.99
FROM rust:${RUST_VERSION}-slim-bookworm AS builder

# musl, so the result is a static binary with no loader and no libc to ship. That
# is what makes the final stage `scratch` possible.
#
# musl-tools provides musl-gcc, which `ring` needs for its handful of C files.
RUN apt-get update && apt-get install -y --no-install-recommends \
        musl-tools ca-certificates \
    && rm -rf /var/lib/apt/lists/*

ARG TARGETARCH
WORKDIR /build

# benches/ is listed because Cargo.toml declares the bench targets explicitly.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches

# `-C target-feature=+crt-static` is the default for musl targets but is stated
# here so a future target change cannot silently produce a dynamic binary that
# `scratch` then cannot run. `target-cpu=generic` keeps the image runnable on
# every CPU of the architecture (no AVX2 requirement, any Raspberry Pi).
#
# The `ldd` check is not paranoia: a dynamically linked binary builds and runs
# fine here and then fails in `scratch` with nothing but "no such file or
# directory", which points at the path rather than at the linkage.
#
# `loxInFlux` is the command the 0.3 entrypoint ran; it stays as a symlink.
#
# The optional build_ca secret is for builds behind a TLS-intercepting proxy; it
# is never written into a layer.
#
# NOTE: no `#` comments inside the RUN below. Docker joins the continued lines
# into one before handing them to the shell, and a `#` there comments out
# everything that follows it - including the rest of the script.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/target,id=loxinflux-target-${TARGETARCH} \
    --mount=type=secret,id=build_ca \
    set -eux; \
    if [ -f /run/secrets/build_ca ]; then \
      export CARGO_HTTP_CAINFO=/run/secrets/build_ca SSL_CERT_FILE=/run/secrets/build_ca; \
    fi; \
    case "${TARGETARCH}" in \
      amd64) TARGET=x86_64-unknown-linux-musl ;; \
      arm64) TARGET=aarch64-unknown-linux-musl ;; \
      *) echo "unsupported architecture: ${TARGETARCH}" >&2; exit 1 ;; \
    esac; \
    rustup target add "${TARGET}"; \
    RUSTFLAGS="-C target-feature=+crt-static -C target-cpu=generic" \
      cargo build --release --locked --target "${TARGET}" --bin loxinflux; \
    mkdir -p /out/bin /out/app/config /out/app/data; \
    cp "target/${TARGET}/release/loxinflux" /out/bin/loxinflux; \
    ln -s loxinflux /out/bin/loxInFlux; \
    if ldd /out/bin/loxinflux 2>&1 | grep -q "=>"; then \
      echo "loxinflux is dynamically linked and will not run in scratch" >&2; \
      exit 1; \
    fi; \
    /out/bin/loxinflux --version

# -------------------------------------
# 2) Final stage
# -------------------------------------
FROM scratch

# The binary resolves its directories from these, so the documented
# `-v ./config:/app/config` and `-v ./data:/app/data` mounts are what it reads.
ENV CONFIG_DIR=/app/config \
    DATA_DIR=/app/data \
    LOG_LEVEL=INFO

WORKDIR /app

# reqwest and the WebSocket client verify against compiled-in webpki roots. The
# system bundle covers anything that reads the platform trust store instead.
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/

COPY --from=builder /out/bin/ /usr/local/bin/
COPY --from=builder /out/app/ /app/

VOLUME ["/app/config", "/app/data"]
EXPOSE 9108/tcp

# Docker's default, stated so the contract is visible: SIGTERM stops the
# producers and drains the queue within telegraf.shutdown_timeout.
STOPSIGNAL SIGTERM

# No shell and no curl in scratch: the binary probes its own /health endpoint.
HEALTHCHECK --interval=30s --timeout=5s --start-period=60s --retries=3 \
    CMD ["/usr/local/bin/loxinflux", "--healthcheck"]

ENTRYPOINT ["/usr/local/bin/loxinflux"]
