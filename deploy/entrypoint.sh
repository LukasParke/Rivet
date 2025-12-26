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

trap 'kill -TERM "$web_pid" "$api_pid" 2>/dev/null || true; wait' INT TERM

echo "Starting nginx on :8080"
exec nginx -g "daemon off;"


