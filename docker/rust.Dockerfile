# syntax=docker/dockerfile:1
# One build for the backend, worker and migrator. Build from the repo root:
#   docker build -f docker/rust.Dockerfile --target backend .   (or worker, migrate)

FROM lukemathwalker/cargo-chef:latest-rust-1-bookworm AS chef
WORKDIR /app
# SAML signature verification (samael's "xmlsec" feature) needs these headers and clang for bindgen.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libxml2-dev libxmlsec1-dev clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
COPY rust-toolchain.toml rust-toolchain.toml
ARG PACKAGES="-p arcodash-backend -p arcodash-worker -p arcodash-migration"
RUN cargo chef cook --release --recipe-path recipe.json $PACKAGES
COPY . .
RUN cargo build --release $PACKAGES

FROM gcr.io/distroless/cc-debian12:nonroot AS migrate
COPY --from=builder /app/target/release/arcodash-migration /app/arcodash-migration
ENTRYPOINT ["/app/arcodash-migration"]
CMD ["up"]

FROM gcr.io/distroless/cc-debian12:nonroot AS worker
COPY --from=builder /app/target/release/arcodash-worker /app/arcodash-worker
ENTRYPOINT ["/app/arcodash-worker"]

# The backend links libxml2/libxmlsec1 at runtime, which distroless lacks.
FROM debian:12-slim AS backend
RUN apt-get update && apt-get install -y --no-install-recommends \
        libxml2 libxmlsec1-openssl libxslt1.1 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 65532 --no-create-home --shell /usr/sbin/nologin nonroot
WORKDIR /app
COPY --from=builder /app/target/release/arcodash-backend /app/arcodash-backend
# Offline ENCRYPTION_KEY rotation, run with --entrypoint (see ENV_VARS.md).
COPY --from=builder /app/target/release/rotate_encryption_key /app/rotate_encryption_key
USER nonroot
EXPOSE 8080
ENTRYPOINT ["/app/arcodash-backend"]
