FROM ubuntu:24.04 AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    curl build-essential pkg-config libssl-dev ca-certificates \
 && rm -rf /var/lib/apt/lists/*

RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.93.0
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release

FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3t64 \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd -g 1000 appgroup \
 && useradd -u 1000 -g appgroup appuser

COPY --from=builder /app/target/release/bookstack-mcp /usr/local/bin/bookstack-mcp

RUN mkdir -p /data/models && chown -R appuser:appgroup /data
VOLUME /data

USER appuser

EXPOSE 8080

CMD ["bookstack-mcp"]
