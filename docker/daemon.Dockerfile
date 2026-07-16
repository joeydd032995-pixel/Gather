# Gather daemon — multi-stage build.
# Stage 1 compiles a release binary; stage 2 is a minimal Debian runtime with
# a non-root user. Build context is the repository root:
#   docker build -f docker/daemon.Dockerfile .

# ---------------------------------------------------------------------------
# Builder
# ---------------------------------------------------------------------------
FROM rust:1.94-slim-bookworm AS builder

WORKDIR /build/daemon

# Cache dependency compilation: build an empty crate with the real manifests
# (and a stub build script, so build-dependencies like tonic-build/protox
# cache too) first, then copy sources and rebuild only the crate itself.
COPY daemon/Cargo.toml daemon/Cargo.lock ./
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && echo 'fn main() {}' > build.rs \
 && cargo build --release --locked \
 && rm -rf src build.rs target/release/gather-daemon \
        target/release/deps/gather_daemon-* target/release/deps/libgather_daemon-* \
        target/release/.fingerprint/gather-daemon-* target/release/build/gather-daemon-*

# build.rs compiles ../proto/gather/v1/gather.proto with protox (pure Rust,
# no system protoc needed in this image).
COPY proto /build/proto
COPY daemon/build.rs ./build.rs
COPY daemon/src ./src
COPY daemon/migrations ./migrations
RUN cargo build --release --locked

# ---------------------------------------------------------------------------
# Runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl tini \
        tesseract-ocr tesseract-ocr-eng \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /var/lib/gather --create-home --shell /usr/sbin/nologin gather

COPY --from=builder /build/daemon/target/release/gather-daemon /usr/local/bin/gather-daemon

USER gather
WORKDIR /var/lib/gather

# In-container default; compose maps it to 127.0.0.1 on the host so the API
# is never exposed off-machine. GATHER_ALLOW_NON_LOOPBACK is required because
# 0.0.0.0 here is only reachable through the container's published port.
ENV GATHER_BIND_ADDR=0.0.0.0:7601 \
    GATHER_GRPC_BIND_ADDR=0.0.0.0:7602 \
    GATHER_ALLOW_NON_LOOPBACK=true \
    GATHER_LOG_JSON=true

# 7601 = REST, 7602 = gRPC; compose maps both to 127.0.0.1 on the host.
EXPOSE 7601 7602

HEALTHCHECK --interval=10s --timeout=3s --start-period=15s --retries=5 \
    CMD curl -fsS http://127.0.0.1:7601/healthz || exit 1

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["gather-daemon"]
