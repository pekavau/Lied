//! Integration tests for the full schema migration set (`/migrations`).
//!
//! Each test spins up a fresh ephemeral Postgres via `testcontainers`
//! (CLAUDE.md Testing posture: real Postgres, no mocks, a fresh per-test
//! container so there is no fixture pollution between tests), applies every
//! migration with `sqlx::migrate!`, and asserts:
//! - the instrument seed lands ~150+ rows (CLAUDE.md: "Instrument seed
//!   (~150 standard instruments)"),
//! - partial unique indexes behave as designed: a duplicate *live* row is
//!   rejected, but a duplicate that includes a soft-deleted row is allowed
//!   (CLAUDE.md: "Unique constraints... as partial unique indexes `WHERE
//!   deleted_at IS NULL`").

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

/// A fresh ephemeral Postgres container with every migration applied. The
/// container is owned here and stopped when the struct is dropped at the end
/// of each test, so tests share no state.
struct TestDb {
    // Held only to keep the container alive for the duration of the test;
    // dropping it stops and removes the container.
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
}

impl TestDb {
    /// Starts a Postgres container, connects, and runs every migration in
    /// `/migrations` against it.
    async fn create_and_migrate() -> Self {
        let container = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("failed to start postgres container");

        let host = container
            .get_host()
            .await
            .expect("failed to get container host");
        let port = container
            .get_host_port_ipv4(5432)
            .await
            .expect("failed to get container port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(&url)
            .await
            .expect("failed to connect to container postgres");

        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrations should apply cleanly to a fresh database");

        Self {
            _container: container,
            pool,
        }
    }

    fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Creates a minimal Organization row, returning its id. Several
/// unique-constraint tests below need a parent Organization to satisfy FKs.
async fn seed_organization(pool: &PgPool) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO organization (id, name, slug) VALUES ($1, 'Test Orchestra', $2)"#,
        id,
        format!("test-orchestra-{id}")
    )
    .execute(pool)
    .await
    .expect("insert organization");
    id
}

#[tokio::test]
async fn migrations_apply_cleanly_and_are_idempotent() {
    let db = TestDb::create_and_migrate().await;

    // Re-running the same migration set against an already-migrated
    // database must be a no-op, not an error (CLAUDE.md acceptance
    // criterion: "Re-running migrations is idempotent").
    sqlx::migrate!("../migrations")
        .run(db.pool())
        .await
        .expect("re-running migrations should be idempotent");
}

#[tokio::test]
async fn instrument_seed_has_at_least_150_rows() {
    let db = TestDb::create_and_migrate().await;
    let pool = db.pool();

    let count: i64 = sqlx::query_scalar!(r#"SELECT count(*) FROM instrument"#)
        .fetch_one(pool)
        .await
        .expect("count instruments")
        .unwrap_or(0);

    assert!(
        count >= 150,
        "expected at least 150 seeded instruments, found {count}"
    );

    // Spot-check that every standard family is represented.
    let families: Vec<String> =
        sqlx::query_scalar!(r#"SELECT DISTINCT family FROM instrument ORDER BY family"#)
            .fetch_all(pool)
            .await
            .expect("distinct families");
    for expected in [
        "brass",
        "keyboard",
        "other",
        "percussion",
        "strings",
        "voice",
        "woodwind",
    ] {
        assert!(
            families.iter().any(|f| f == expected),
            "expected family {expected} to be represented, got {families:?}"
        );
    }
}

#[tokio::test]
async fn instrument_key_unique_index_rejects_duplicates() {
    let db = TestDb::create_and_migrate().await;
    let pool = db.pool();

    // `trumpet_bb` already exists from the seed; inserting it again must
    // violate the unique index on `instrument.key`.
    let result = sqlx::query!(
        r#"INSERT INTO instrument (id, key, display_name, family) VALUES ($1, 'trumpet_bb', 'Duplicate Trumpet', 'brass')"#,
        Uuid::now_v7()
    )
    .execute(pool)
    .await;

    assert!(
        result.is_err(),
        "duplicate instrument.key should be rejected"
    );
}

#[tokio::test]
async fn organization_slug_partial_unique_index_behaves_correctly() {
    let db = TestDb::create_and_migrate().await;
    let pool = db.pool();

    let id1 = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO organization (id, name, slug) VALUES ($1, 'Orchestra One', 'shared-slug')"#,
        id1
    )
    .execute(pool)
    .await
    .expect("first insert should succeed");

    // A second *live* row with the same slug must be rejected.
    let dup = sqlx::query!(
        r#"INSERT INTO organization (id, name, slug) VALUES ($1, 'Orchestra Two', 'shared-slug')"#,
        Uuid::now_v7()
    )
    .execute(pool)
    .await;
    assert!(
        dup.is_err(),
        "duplicate live organization.slug should be rejected"
    );
}

#[tokio::test]
async fn arrangement_slug_partial_unique_index_allows_duplicate_after_soft_delete() {
    let db = TestDb::create_and_migrate().await;
    let pool = db.pool();
    let org_id = seed_organization(pool).await;

    let arr1 = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO arrangement (id, organization_id, title, slug) VALUES ($1, $2, 'Symphony No. 5', 'symphony-5')"#,
        arr1,
        org_id
    )
    .execute(pool)
    .await
    .expect("first arrangement insert should succeed");

    // A second *live* row with the same (organization_id, slug) is rejected.
    let dup = sqlx::query!(
        r#"INSERT INTO arrangement (id, organization_id, title, slug) VALUES ($1, $2, 'Symphony No. 5 (dup)', 'symphony-5')"#,
        Uuid::now_v7(),
        org_id
    )
    .execute(pool)
    .await;
    assert!(
        dup.is_err(),
        "duplicate live (organization_id, slug) should be rejected"
    );

    // Soft-delete the first row...
    sqlx::query!(
        r#"UPDATE arrangement SET deleted_at = now() WHERE id = $1"#,
        arr1
    )
    .execute(pool)
    .await
    .expect("soft delete should succeed");

    // ...now a new live row with the same slug is allowed, since the
    // partial unique index only applies `WHERE deleted_at IS NULL`.
    let after_delete = sqlx::query!(
        r#"INSERT INTO arrangement (id, organization_id, title, slug) VALUES ($1, $2, 'Symphony No. 5 (re-added)', 'symphony-5')"#,
        Uuid::now_v7(),
        org_id
    )
    .execute(pool)
    .await;
    assert!(
        after_delete.is_ok(),
        "a duplicate slug should be allowed once the original is soft-deleted: {after_delete:?}"
    );
}

#[tokio::test]
async fn file_unique_indexes_distinguish_voice_files_from_score_files() {
    let db = TestDb::create_and_migrate().await;
    let pool = db.pool();
    let org_id = seed_organization(pool).await;

    let arr_id = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO arrangement (id, organization_id, title, slug) VALUES ($1, $2, 'Test Piece', 'test-piece')"#,
        arr_id,
        org_id
    )
    .execute(pool)
    .await
    .expect("insert arrangement");

    let instrument_id: Uuid =
        sqlx::query_scalar!(r#"SELECT id FROM instrument WHERE key = 'flute'"#)
            .fetch_one(pool)
            .await
            .expect("seeded flute instrument should exist");

    let voice_id = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO voice (id, arrangement_id, name, slug, instrument_id) VALUES ($1, $2, 'Flute 1', 'flute-1', $3)"#,
        voice_id,
        arr_id,
        instrument_id
    )
    .execute(pool)
    .await
    .expect("insert voice");

    // Two full-score files (voice_id IS NULL) with the same (name, format)
    // must collide on `file_score_file_key`.
    sqlx::query!(
        r#"INSERT INTO file (id, arrangement_id, voice_id, name, format, mime_type) VALUES ($1, $2, NULL, 'full-score', 'pdf', 'application/pdf')"#,
        Uuid::now_v7(),
        arr_id
    )
    .execute(pool)
    .await
    .expect("first score file insert should succeed");

    let dup_score = sqlx::query!(
        r#"INSERT INTO file (id, arrangement_id, voice_id, name, format, mime_type) VALUES ($1, $2, NULL, 'full-score', 'pdf', 'application/pdf')"#,
        Uuid::now_v7(),
        arr_id
    )
    .execute(pool)
    .await;
    assert!(
        dup_score.is_err(),
        "duplicate full-score (arrangement_id, name, format) should be rejected"
    );

    // A voice file (voice_id IS NOT NULL) with the *same* name/format as the
    // score file above must be allowed -- the two partial indexes are
    // independent because Postgres treats voice_id IS NULL vs IS NOT NULL
    // as disjoint partitions.
    let voice_file = sqlx::query!(
        r#"INSERT INTO file (id, arrangement_id, voice_id, name, format, mime_type) VALUES ($1, $2, $3, 'full-score', 'pdf', 'application/pdf')"#,
        Uuid::now_v7(),
        arr_id,
        voice_id
    )
    .execute(pool)
    .await;
    assert!(
        voice_file.is_ok(),
        "a voice file may share (name, format) with a score file: {voice_file:?}"
    );

    // But a second voice file on the *same* voice with the same name/format
    // collides on `file_voice_file_key`.
    let dup_voice_file = sqlx::query!(
        r#"INSERT INTO file (id, arrangement_id, voice_id, name, format, mime_type) VALUES ($1, $2, $3, 'full-score', 'pdf', 'application/pdf')"#,
        Uuid::now_v7(),
        arr_id,
        voice_id
    )
    .execute(pool)
    .await;
    assert!(
        dup_voice_file.is_err(),
        "duplicate (arrangement_id, voice_id, name, format) should be rejected"
    );
}

#[tokio::test]
async fn tower_sessions_table_exists_and_is_writable() {
    let db = TestDb::create_and_migrate().await;
    let pool = db.pool();

    // Smoke-check the adopted tower-sessions-sqlx-store schema: write then
    // read a row directly (mirrors what `PostgresStore` does internally),
    // without joining it to any domain table (CLAUDE.md infra-pluggability
    // rule: treat it as an opaque K/V store).
    sqlx::query!(
        r#"INSERT INTO tower_sessions.session (id, data, expiry_date) VALUES ('test-session-id', '\x00', now() + interval '1 hour')"#
    )
    .execute(pool)
    .await
    .expect("insert into tower_sessions.session should succeed");

    let count: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) FROM tower_sessions.session WHERE id = 'test-session-id'"#
    )
    .fetch_one(pool)
    .await
    .expect("count session rows")
    .unwrap_or(0);
    assert_eq!(count, 1);
}
