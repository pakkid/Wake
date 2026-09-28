# syntax=docker/dockerfile:1

# ---- build: a static musl binary ----
FROM rust:1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src

# Build dependencies first so they cache across source changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
COPY templates ./templates
COPY static ./static
RUN touch src/main.rs src/lib.rs \
 && cargo build --release --locked

# ---- runtime: alpine for busybox (healthcheck) and setcap ----
FROM alpine:3
RUN apk add --no-cache libcap \
 && addgroup -S -g 10001 wake \
 && adduser -S -D -H -u 10001 -G wake wake \
 && mkdir -p /data && chown wake:wake /data

COPY --from=build /src/target/release/wake /usr/local/bin/wake
# ICMP probes need a raw socket. File capabilities let the non-root user open
# one; NET_RAW is in Docker's default capability set.
RUN setcap cap_net_raw+ep /usr/local/bin/wake && apk del libcap

USER wake
ENV DATA_DIR=/data \
    WEB_PORT=8080 \
    GRUB_PROTOCOL_PORT=8081 \
    RUST_LOG=info
VOLUME ["/data"]
EXPOSE 8080/tcp 8081/tcp

HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
  CMD wget -q -O /dev/null "http://127.0.0.1:${WEB_PORT}/healthz" || exit 1

ENTRYPOINT ["/usr/local/bin/wake"]
