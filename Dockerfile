# syntax=docker/dockerfile:1
# -------------------------------------
# 1) Build stage
# -------------------------------------
FROM ghcr.io/astral-sh/uv:python3.14-bookworm-slim AS builder

# - UV_PYTHON_DOWNLOADS=0: use the image's system Python at
#   /usr/local/bin/python3.14. That path is identical in the final
#   python:3.14-slim image, so the copied venv stays valid.
# - UV_COMPILE_BYTECODE=1: precompile .pyc for faster cold starts.
# - UV_LINK_MODE=copy: materialize real files in the venv (do NOT hardlink into
#   the cache mount, which is not part of the image layer).
ENV UV_PYTHON_DOWNLOADS=0 \
    UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy \
    VIRTUAL_ENV=/app/.venv

WORKDIR /app

# --- Layer A: third-party dependencies only (cache-friendly) ---
# Re-runs only when pyproject.toml / uv.lock change, so the (potentially
# expensive) dependency install is reused across pure source edits.
# All runtime deps ship manylinux/musllinux wheels for cp313, so no compiler
# toolchain is needed here.
RUN --mount=type=cache,target=/root/.cache/uv \
    --mount=type=bind,source=uv.lock,target=uv.lock \
    --mount=type=bind,source=pyproject.toml,target=pyproject.toml \
    uv sync --locked --no-install-project --no-dev --no-editable

# --- Layer B: build & install our own project (non-editable) ---
# --no-editable installs a real wheel into site-packages, so the final image
# needs only the venv (no source tree, pyproject or lockfile).
COPY pyproject.toml uv.lock README.md ./
COPY src ./src
RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --locked --no-dev --no-editable

# -------------------------------------
# 2) Final stage (no uv, no build tools)
# -------------------------------------
FROM python:3.14-slim-bookworm
WORKDIR /app

ENV PATH="/app/.venv/bin:${PATH}" \
    LOG_LEVEL=INFO \
    CONFIG_DIR=/app/config \
    DATA_DIR=/app/data

# Project + deps are installed non-editably into the venv, so we only copy that.
COPY --from=builder /app/.venv /app/.venv

COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
RUN chmod +x /usr/local/bin/docker-entrypoint.sh

# Persistent config (config.toml) and cached Miniserver data (structure files).
VOLUME ["/app/config"]
VOLUME ["/app/data"]

EXPOSE 11885/udp
EXPOSE 8502/tcp

ENTRYPOINT ["docker-entrypoint.sh"]
