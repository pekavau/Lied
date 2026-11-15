//! Integration tests for the full schema migration set (`/migrations`).
//!
//! Each test spins up a fresh ephemeral Postgres via `testcontainers`
//! (CLAUDE.md Testing posture: real Postgres, no mocks, a fresh per-test
//! container so there is no fixture pollution between tests), applies every
//! migration with `sqlx::migrate!`, and asserts:
//! - the instrument seed lands ~150+ rows (docs/design.md: "Instrument seed
//!   (~150 standard instruments)"),
//! - partial unique indexes behave as designed: a duplicate *live* row is
//!   rejected, but a duplicate that includes a soft-deleted row is allowed
//!   (docs/design.md: "Unique constraints... as partial unique indexes `WHERE
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
    // database must be a no-op, not an error (docs/design.md acceptance
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
    // without joining it to any domain table (docs/design.md infra-pluggability
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

/// Production startup (`AppState::connect`, shared by the server and the
/// `create-admin` CLI) must bring an empty database up to the full schema —
/// issue #87, where nothing outside the tests ever ran the migrations — and
/// a second start against the now-current schema must be a no-op.
#[tokio::test]
async fn startup_migrates_an_empty_database_and_restarting_is_a_no_op() {
    use lied::config::{AppConfig, Secret};
    use lied::state::{AppState, MIGRATOR};

    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("failed to start postgres container");
    let host = container.get_host().await.expect("container host");
    let port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("container port");
    let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");

    let config = || AppConfig {
        bind_addr: "127.0.0.1:0".parse().unwrap(),
        database_url: Secret::from(url.clone()),
        s3_endpoint: "http://localhost:9000".to_string(),
        s3_bucket: "lied-test".to_string(),
        s3_access_key_id: Secret::from("test".to_string()),
        s3_secret_access_key: Secret::from("test".to_string()),
        s3_region: "us-east-1".to_string(),
        jwt_signing_key: Secret::from("test-signing-key-material-for-startup".to_string()),
        jwt_lifetime_days: 30,
        max_upload_bytes: 1,
        max_request_bytes: 256 * 1024,
        max_files_per_voice: 1,
        max_arrangements_per_org: None,
        max_page_size: 200,
        default_page_size: 50,
        ratelimit_auth_per_min: 120,
        ratelimit_anon_per_min: 20,
        ratelimit_login_per_min: 10,
        metrics_enabled: false,
        docs_enabled: true,
        tracing_pretty: false,
        secure_cookies: false,
    };

    let applied = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>(r#"SELECT count(*) FROM _sqlx_migrations WHERE success"#)
            .fetch_one(&pool)
            .await
            .expect("migration ledger exists")
    };

    let state = AppState::connect(config())
        .await
        .expect("startup migrates an empty database");
    let expected = MIGRATOR.iter().count() as i64;
    assert_eq!(applied(state.db.clone()).await, expected);
    // The schema is really there, seed included.
    let instruments: i64 = sqlx::query_scalar("SELECT count(*) FROM instrument")
        .fetch_one(&state.db)
        .await
        .expect("instrument table exists");
    assert!(instruments > 100, "instrument seed applied");

    let restarted = AppState::connect(config())
        .await
        .expect("starting against a current schema succeeds");
    assert_eq!(applied(restarted.db.clone()).await, expected);
}

/// #102 replaces `collection_item.index` with `position` + `label`. Data
/// written before it must come through: live positions dense, labels as
/// the old numbers (a program's equal to its positions), a number the label
/// format cannot hold replaced by the next free ones, removed rows keeping a
/// label to try on restore, and a wide book widened to four digits.
#[tokio::test]
async fn collection_numbering_migration_carries_old_numbers_over() {
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .expect("start postgres");
    let port = container.get_host_port_ipv4(5432).await.expect("port");
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&format!(
            "postgres://postgres:postgres@127.0.0.1:{port}/postgres"
        ))
        .await
        .expect("connect");

    // Everything up to the migration before #102.
    let mut before = sqlx::migrate!("../migrations");
    before.migrations = before
        .migrations
        .iter()
        .filter(|m| m.version < 26)
        .cloned()
        .collect::<Vec<_>>()
        .into();
    before.run(&pool).await.expect("migrate to 25");

    let org = Uuid::now_v7();
    sqlx::query("INSERT INTO organization (id, name, slug) VALUES ($1, 'O', 'o')")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    let mut arrangement = 0;
    let mut next_arrangement = || {
        arrangement += 1;
        arrangement
    };
    // (collection type, [(index, removed)])
    let collections: [(&str, &[(i32, bool)]); 3] = [
        ("program", &[(1, false), (3, false), (2, true)]),
        (
            "standing",
            &[(2, false), (5, false), (20_000, false), (7, true)],
        ),
        ("standing", &[(1, false), (1200, false)]),
    ];
    let mut ids = Vec::new();
    for (n, (kind, items)) in collections.iter().enumerate() {
        let coll = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO collection (id, organization_id, name, slug, type) VALUES ($1, $2, $3, $3, $4)",
        )
        .bind(coll)
        .bind(org)
        .bind(format!("c{n}"))
        .bind(kind)
        .execute(&pool)
        .await
        .unwrap();
        for (index, removed) in items.iter() {
            let arr = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO arrangement (id, organization_id, title, slug, status) \
                 VALUES ($1, $2, $3, $3, 'active')",
            )
            .bind(arr)
            .bind(org)
            .bind(format!("a{}", next_arrangement()))
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO collection_item (id, collection_id, arrangement_id, index, deleted_at) \
                 VALUES ($1, $2, $3, $4, CASE WHEN $5 THEN now() END)",
            )
            .bind(Uuid::now_v7())
            .bind(coll)
            .bind(arr)
            .bind(index)
            .bind(removed)
            .execute(&pool)
            .await
            .unwrap();
        }
        ids.push(coll);
    }

    sqlx::migrate!("../migrations")
        .run(&pool)
        .await
        .expect("the numbering migration applies to existing data");

    let rows = |coll: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_as::<_, (i32, String, bool)>(
                "SELECT position, label, deleted_at IS NOT NULL FROM collection_item \
                 WHERE collection_id = $1 ORDER BY deleted_at IS NOT NULL, position",
            )
            .bind(coll)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    let width = |coll: Uuid| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i16>("SELECT number_width FROM collection WHERE id = $1")
                .bind(coll)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let owned = |rows: &[(i32, &str, bool)]| {
        rows.iter()
            .map(|(p, l, d)| (*p, l.to_string(), *d))
            .collect::<Vec<_>>()
    };

    // The program: dense positions, labels = positions; its removed piece
    // keeps its old number as the place to come back to.
    assert_eq!(
        rows(ids[0]).await,
        owned(&[(1, "1", false), (2, "2", false), (2, "2", true)])
    );
    // The standing book keeps its numbers and gaps; 20000 cannot be a label
    // and becomes the next number after the highest legal one.
    assert_eq!(
        rows(ids[1]).await,
        owned(&[
            (1, "2", false),
            (2, "5", false),
            (3, "6", false),
            (7, "7", true)
        ])
    );
    assert_eq!(width(ids[1]).await, 3);
    // A book numbered past 999 gets four-digit folder names.
    assert_eq!(
        rows(ids[2]).await,
        owned(&[(1, "1", false), (2, "1200", false)])
    );
    assert_eq!(width(ids[2]).await, 4);
}

/// Applied migrations never change, comments included (#111). sqlx stores
/// each migration's SHA-384 when it applies it and, since the server
/// migrates on start (#87), refuses to start against a database whose
/// applied migration no longer matches. Every test here migrates a fresh
/// database, so nothing else would notice: this pins the checksums in
/// `tests/migration_checksums.txt`.
#[test]
fn applied_migrations_are_never_edited() {
    let manifest = include_str!("migration_checksums.txt");
    let pinned: std::collections::BTreeMap<i64, &str> = manifest
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (version, checksum) = line.split_once(' ').expect("`<version> <sha384>`");
            (version.parse().expect("numeric version"), checksum.trim())
        })
        .collect();
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let current: std::collections::BTreeMap<i64, String> = lied::state::MIGRATOR
        .iter()
        .map(|m| (m.version, hex(&m.checksum)))
        .collect();

    for (version, checksum) in &pinned {
        match current.get(version) {
            None => panic!("migration {version:04} was removed; applied migrations must stay"),
            Some(now) => assert_eq!(
                now, checksum,
                "migration {version:04} was edited after it was committed; databases that \
                 applied it will refuse to start. Restore it and put the change in a new migration."
            ),
        }
    }
    let missing: Vec<String> = current
        .iter()
        .filter(|(version, _)| !pinned.contains_key(version))
        .map(|(version, checksum)| format!("{version:04} {checksum}"))
        .collect();
    assert!(
        missing.is_empty(),
        "new migration(s) not pinned; append to lied/tests/migration_checksums.txt:\n{}",
        missing.join("\n")
    );
}
