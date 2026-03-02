FROM rust:1.93-alpine AS builder

RUN apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release

FROM alpine:3.21

RUN apk add --no-cache ca-certificates \
 && addgroup -S appgroup \
 && adduser -S appuser -G appgroup

COPY --from=builder /app/target/release/bookstack-mcp /usr/local/bin/bookstack-mcp

USER appuser

EXPOSE 8080

CMD ["bookstack-mcp"]
