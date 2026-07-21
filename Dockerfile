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
# - CI=1: pygixml's Optimize.cmake then omits -march=native, so its source build
#   below is portable across every CPU of the target arch (non-AVX2 x86, Pi).
ENV UV_PYTHON_DOWNLOADS=0 \
    UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy \
    VIRTUAL_ENV=/app/.venv \
    CI=1

WORKDIR /app

# Build toolchain for the pygixml C++ source build. pygixml ships no linux/arm64
# wheel and its x86_64 wheel is AVX2-only (no runtime CPU dispatch), so we force
# a portable source build from the sdist (uv --no-binary-package pygixml + CI=1).
# All other runtime deps ship manylinux/musllinux wheels, so this is the only
# package that needs a compiler.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential python3-dev \
    && rm -rf /var/lib/apt/lists/*

# --- Layer A: third-party dependencies only (cache-friendly) ---
# Re-runs only when pyproject.toml / uv.lock change, so the (potentially
# expensive) dependency install - incl. the pygixml source build - is reused
# across pure source edits.
RUN --mount=type=cache,target=/root/.cache/uv \
    --mount=type=bind,source=uv.lock,target=uv.lock \
    --mount=type=bind,source=pyproject.toml,target=pyproject.toml \
    uv sync --locked --no-install-project --no-dev --no-editable \
        --no-binary-package pygixml

# --- Layer B: build & install our own project (non-editable) ---
# --no-editable installs a real wheel into site-packages, so the final image
# needs only the venv (no source tree, pyproject or lockfile).
COPY pyproject.toml uv.lock README.md ./
COPY src ./src
RUN --mount=type=cache,target=/root/.cache/uv \
    uv sync --locked --no-dev --no-editable --no-binary-package pygixml

# Strip debug symbols from native extensions (pygixml et al.) to shrink the venv.
RUN find /app/.venv -name '*.so' -exec strip --strip-unneeded {} + || true

# Fail the build early if pygixml did not build/install as a loadable native
# extension (e.g. an accidentally pure-Python install or a broken strip). Runs
# the same import+XPath-compile the runtime probe does, using the venv python so
# it inspects what actually got installed.
RUN /app/.venv/bin/python -c "import pygixml; pygixml.XPathQuery('//C'); print('pygixml native extension OK')"

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
