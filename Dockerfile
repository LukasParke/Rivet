# Rivet — media automation control plane (Rust API/WS server + SvelteKit web).

FROM rust:1.90-slim-bookworm AS rust-builder
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked -p rivet --bin rivet-server

FROM node:20-bookworm-slim AS web-builder
WORKDIR /src/apps/web
COPY apps/web/package.json apps/web/package-lock.json ./
RUN npm ci
COPY apps/web/ ./
RUN npm run build

FROM node:20-bookworm-slim
WORKDIR /app
RUN apt-get update && \
  apt-get install -y --no-install-recommends nginx tini ca-certificates && \
  rm -rf /var/lib/apt/lists/*

# Non-root runtime user. nginx keeps its root master (binds the port, manages
# workers) but drops workers to appuser via the `user` directive in
# deploy/nginx.conf; rivet-server and node are started through setpriv
# (util-linux, present in debian slim) from deploy/entrypoint.sh.
RUN groupadd --system --gid 10001 appuser && \
  useradd --system --uid 10001 --gid appuser --home-dir /home/appuser \
    --shell /usr/sbin/nologin appuser && \
  mkdir -p /data && chown appuser:appuser /data

COPY --from=rust-builder /src/target/release/rivet-server /app/rivet-server

WORKDIR /app/web
COPY --from=web-builder /src/apps/web/build ./build
COPY --from=web-builder /src/apps/web/node_modules ./node_modules
COPY --from=web-builder /src/apps/web/package.json ./package.json

WORKDIR /app
COPY deploy/nginx.conf /etc/nginx/nginx.conf
COPY deploy/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

# APP_ENV != dev marks session cookies Secure; TLS termination in front of
# nginx is therefore REQUIRED for any non-dev deployment of this image.
ENV APP_ENV=prod
ENV DATABASE_URL="file:/data/app.db?_pragma=busy_timeout(5000)&_pragma=journal_mode(WAL)"
ENV HTTP_ADDR="127.0.0.1:8081"
ENV HOST="127.0.0.1"
ENV PORT="3000"

EXPOSE 8080

VOLUME ["/data"]

ENTRYPOINT ["tini", "--"]
CMD ["/entrypoint.sh"]
