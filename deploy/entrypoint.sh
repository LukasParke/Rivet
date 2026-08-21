#!/bin/sh
set -eu

mkdir -p /data

API_ADDR="${HTTP_ADDR:-127.0.0.1:8081}"
WEB_HOST="${HOST:-127.0.0.1}"
WEB_PORT="${PORT:-3000}"

export HTTP_ADDR="$API_ADDR"
export HOST="$WEB_HOST"
export PORT="$WEB_PORT"
export NODE_ENV="${NODE_ENV:-production}"

echo "Starting rivet-server on ${HTTP_ADDR}"
/app/rivet-server &
api_pid="$!"

echo "Starting rivet-web on ${HOST}:${PORT}"
node /app/web/build/index.js &
web_pid="$!"

shutdown() {
	kill -TERM "$api_pid" "$web_pid" 2>/dev/null || true
	wait "$api_pid" "$web_pid" 2>/dev/null || true
	exit 0
}
trap shutdown INT TERM

echo "Starting nginx on :8080"
nginx -g "daemon off;" &
nginx_pid="$!"

# Wait on children; if any dies, tear down the rest so tini restarts the container.
wait -n "$api_pid" "$web_pid" "$nginx_pid" 2>/dev/null || wait
shutdown


