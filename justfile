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

# Regenerate docs/openapi.json, the committed snapshot of the /v1 contract
# (issue #75). Run it whenever a change alters the API; CI fails otherwise.
openapi:
    LIED_UPDATE_OPENAPI_SNAPSHOT=1 cargo test -p lied --test openapi openapi_snapshot_is_current

# Apply pending SQL migrations to the database pointed at by $DATABASE_URL.
migrate:
    sqlx migrate run --source migrations

# Refresh the .sqlx/ offline query cache after changing SQL in code.
# Requires a reachable database (see `just dev`).
prepare:
    cargo sqlx prepare --workspace

# Bring up Postgres + RustFS + the app with hot reload via cargo-watch.
dev:
    docker compose up --build

# Release binary build.
build:
    cargo build --release

# Build the production Docker image.
image:
    docker build -t lied:latest .

# Back up Postgres (pg_dump --format=custom) and object storage (rclone) against
# the docker-compose volumes, per the phase-1 backup posture in docs/design.md.
# Ordering matters: Postgres first, then object storage (DB is the existence
# authority; a DB-references-missing-file is detectable, the reverse is
# silent dead bytes). Output lands in ./backup, a host-mounted dir. Only the
# current version of each object is copied, not the versions kept by bucket
# versioning.
backup:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p backup
    timestamp="$(date -u +%Y%m%dT%H%M%SZ)"
    echo "Backing up Postgres..."
    docker compose exec -T postgres pg_dump --format=custom -U "${POSTGRES_USER:-lied}" "${POSTGRES_DB:-lied}" > "backup/postgres-${timestamp}.dump"
    echo "Backing up object storage..."
    mkdir -p "backup/storage-${timestamp}"
    # The remote `lied` is defined entirely by RCLONE_CONFIG_LIED_* variables,
    # so no config file is needed; running as the host user keeps the backup
    # files owned by whoever ran `just backup`.
    docker run --rm --network lied_default \
      --user "$(id -u):$(id -g)" \
      -e RCLONE_CONFIG_LIED_TYPE=s3 \
      -e RCLONE_CONFIG_LIED_PROVIDER=Other \
      -e RCLONE_CONFIG_LIED_ENDPOINT=http://storage:9000 \
      -e "RCLONE_CONFIG_LIED_ACCESS_KEY_ID=${STORAGE_ACCESS_KEY:-rustfsadmin}" \
      -e "RCLONE_CONFIG_LIED_SECRET_ACCESS_KEY=${STORAGE_SECRET_KEY:-rustfsadmin}" \
      -e RCLONE_CONFIG_LIED_FORCE_PATH_STYLE=true \
      -e RCLONE_CONFIG=/tmp/rclone.conf \
      -v "$(pwd)/backup/storage-${timestamp}:/backup" \
      rclone/rclone:1.76.0 \
      copy "lied:${LIED_S3_BUCKET:-lied}" /backup
    echo "Backup complete: backup/postgres-${timestamp}.dump, backup/storage-${timestamp}/"
