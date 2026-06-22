set shell := ["bash", "-uc"]

# Canonical command an autonomous loop runs to verify its work.
check: fmt-check lint test

fmt:
    cargo fmt

fmt-check:
    cargo fmt --check

lint:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test --all

# Apply pending SQL migrations to the database pointed at by $DATABASE_URL.
migrate:
    sqlx migrate run --source migrations

# Refresh the .sqlx/ offline query cache after changing SQL in code.
# Requires a reachable database (see `just dev`).
prepare:
    cargo sqlx prepare --workspace

# Bring up Postgres + MinIO + the app with hot reload via cargo-watch.
dev:
    docker compose up --build

# Release binary build.
build:
    cargo build --release

# Build the production Docker image.
image:
    docker build -t lied:latest .

# Back up Postgres (pg_dump --format=custom) and MinIO (mc mirror) against
# the docker-compose volumes, per the phase-1 backup posture in CLAUDE.md.
# Ordering matters: Postgres first, then MinIO (DB is the existence
# authority; a DB-references-missing-file is detectable, the reverse is
# silent dead bytes). Output lands in ./backup, a host-mounted dir.
backup:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p backup
    timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
    echo "Backing up Postgres..."
    docker compose exec -T postgres pg_dump --format=custom -U "${POSTGRES_USER:-lied}" "${POSTGRES_DB:-lied}" > "backup/postgres-${timestamp}.dump"
    echo "Backing up MinIO..."
    mkdir -p "backup/minio-${timestamp}"
    docker run --rm --network lied_default \
      -v "$(pwd)/backup/minio-${timestamp}:/backup" \
      minio/mc:latest \
      sh -c "mc alias set src http://minio:9000 \"\${MINIO_ROOT_USER:-minioadmin}\" \"\${MINIO_ROOT_PASSWORD:-minioadmin}\" && mc mirror src /backup"
    echo "Backup complete: backup/postgres-${timestamp}.dump, backup/minio-${timestamp}/"
