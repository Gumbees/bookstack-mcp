FROM rust:1.93-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd -r appgroup \
 && useradd -r -g appgroup appuser

COPY --from=builder /app/target/release/bookstack-mcp /usr/local/bin/bookstack-mcp

RUN mkdir -p /data/models && chown -R appuser:appgroup /data
VOLUME /data

USER appuser

EXPOSE 8080

CMD ["bookstack-mcp"]
