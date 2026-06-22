# syntax=docker/dockerfile:1

# ---- Builder ----
FROM rust:1.96.0-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependency compilation: copy manifests first.
COPY Cargo.toml Cargo.lock ./
COPY lied/Cargo.toml lied/Cargo.toml
COPY lied-server/Cargo.toml lied-server/Cargo.toml

# Offline mode: the committed .sqlx/ cache means cargo never needs a live DB
# to typecheck sqlx::query!/query_as! macros during this build.
ENV SQLX_OFFLINE=true

COPY .sqlx .sqlx
COPY lied lied
COPY lied-server lied-server
COPY migrations migrations

RUN cargo build --release --package lied-server

# ---- Runtime ----
FROM gcr.io/distroless/cc-debian12 AS runtime

COPY --from=builder /build/target/release/lied-server /usr/local/bin/lied-server

EXPOSE 8080

ENTRYPOINT ["/usr/local/bin/lied-server"]
