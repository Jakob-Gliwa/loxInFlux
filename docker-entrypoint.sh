#!/bin/sh
set -e

# Always pass the container's config/data locations, then let any extra args
# passed to `docker run` (after the image name) override or extend them.
set -- --config-dir "${CONFIG_DIR:-/app/config}" --data-dir "${DATA_DIR:-/app/data}" "$@"

# Prepend the configured log level (if any).
if [ -n "${LOG_LEVEL}" ]; then
    set -- --log-level "${LOG_LEVEL}" "$@"
fi

# exec so the Python process becomes PID 1 and receives SIGTERM/SIGINT directly
# for a clean shutdown.
exec loxInFlux "$@"
