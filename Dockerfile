# Build:  docker build -t rbm:0.1.0 .
# Run:    see deploy/docker-compose.example.yml and docs/DEPLOYMENT.md
# Build it off the broadcaster host if that is small, then `docker save | ssh <host> docker load`.
FROM rust:1-trixie AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY assets assets
COPY config.example.toml ./
RUN cargo build --release --locked && strip target/release/rbm

FROM debian:trixie-slim
# systemd provides journalctl for `source.kind = "journald"`; libssl3 is needed by the Waku client.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates libssl3 systemd \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home /data --shell /usr/sbin/nologin rbm \
 && mkdir -p /data /config && chown rbm:rbm /data
COPY --from=build /src/target/release/rbm /usr/local/bin/rbm
COPY config.example.toml /config/rbm.example.toml
LABEL org.opencontainers.image.title="railgun-broadcaster-monitor" \
      org.opencontainers.image.description="Read-only dashboard for RAILGUN broadcaster operators" \
      org.opencontainers.image.source="https://github.com/Shogoki/railgun-broadcaster-monitor" \
      org.opencontainers.image.licenses="MIT"
USER rbm
VOLUME ["/data"]
ENV RBM_CONFIG=/config/rbm.toml
# Note: inside the container `web.bind = 127.0.0.1:8080` is only reachable with
# `network_mode: host` (recommended: keeps the UI on the host's loopback).
ENTRYPOINT ["/usr/local/bin/rbm"]
CMD ["run"]
