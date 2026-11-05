# Container alternative to deploy/install.sh (the Ubuntu + systemd path in docs/hosting.md is the
# primary one). Used by docker-compose.yml for both the dashboard and the daily-job sidecar.
# Secrets are never baked in: .dockerignore excludes .env and data/, compose passes .env at runtime.
#
# Stage 1 compiles the single `fm` binary (templates, static files and fitted parameters are
# compiled into it); stage 2 is a slim Debian with that binary and the deploy scripts.

# ---------------------------------------------------------------------------- build
FROM rust:1-slim-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
# BuildKit caches keep the crates.io index and compiled dependencies between builds.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p fm-cli \
 && install -m 755 target/release/fm /usr/local/bin/fm

# ---------------------------------------------------------------------------- runtime
FROM debian:bookworm-slim

ENV FM_DATA_DIR=/app/data

# libssl3: HTTPS client; sqlite3: deploy/backup.sh snapshots; curl: the health check
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3 tzdata sqlite3 curl \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --user-group --home-dir /app --shell /usr/sbin/nologin fantasy

COPY --from=build /usr/local/bin/fm /usr/local/bin/fm
WORKDIR /app
COPY deploy ./deploy
RUN chmod +x deploy/*.sh && install -d -m 700 -o fantasy -g fantasy /app/data /app/backups \
 && fm --help > /dev/null

USER fantasy
EXPOSE 8765
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
  CMD curl -fsS --max-time 4 http://127.0.0.1:8765/healthz > /dev/null || exit 1

# Only the caddy container can reach this port (it is not published). fm takes the client IP and
# https scheme from X-Forwarded-* headers only from FM_WEB_TRUSTED_PROXIES (compose sets it to the
# caddy container's fixed address); the Secure cookie flag follows. Binding 0.0.0.0 needs
# FM_WEB_PASSWORD in .env: without it fm refuses to start (see docs/hosting.md).
CMD ["fm", "web", "--host", "0.0.0.0", "--port", "8765"]
