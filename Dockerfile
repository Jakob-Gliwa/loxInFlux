FROM ghcr.io/astral-sh/uv:python3.13-alpine

# Set working directory
WORKDIR /app

# Copy all files into the image
COPY . .

# Install build dependencies, Python dependencies, and clean up in a single layer
RUN apk add --no-cache --virtual .build-deps \
    gcc \
    musl-dev \
    python3-dev \
 && uv pip install --system . \
 && cd src/loxwebsocket/cython_modules \
 && python setup.py build_ext --inplace \
 && cd /app \
 && apk del .build-deps

# Set PYTHONPATH to include the src directory
ENV PYTHONPATH=/app/src

# Environment variables
ENV LOG_LEVEL=INFO
# Configuration storage location
ENV CONFIG_DIR=/app/config
# Location for persistent cached data (Loxone Config / Websocket structure file)
ENV DATA_DIR=/app/data

# Volumes for persistent data
VOLUME ["$CONFIG_DIR", "$CONFIG_DIR"]
VOLUME ["$DATA_DIR", "$DATA_DIR"]

# Expose ports
EXPOSE 11885/udp
EXPOSE 8502/tcp

# Start command
CMD loxInFlux \
  $([ ! -z "$LOG_LEVEL" ] && echo "--log-level $LOG_LEVEL") \
  --config-dir $CONFIG_DIR \
  --data-dir $DATA_DIR