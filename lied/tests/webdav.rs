//! Integration tests for the WebDAV visibility layer (issue #8, step 2).
//!
//! Exercises the per-role read filtering / PROPFIND root-scope policy in
//! `lied::webdav::access` against a real Postgres: staff see every arrangement,
//! a musician sees only the ones they hold a part assignment on, and
//! soft-deleted arrangements are hidden from everyone.

use lied::webdav::access::{resolve_org_visibility, visible_arrangement_slugs, Visibility};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, ImageExt};
use testcontainers_modules::postgres::Postgres;
use uuid::Uuid;

struct TestDb {
    _container: ContainerAsync<Postgres>,
    pool: PgPool,
}

impl TestDb {
    async fn create_and_migrate() -> Self {
        let container = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("start postgres");
        let host = container.get_host().await.expect("host");
        let port = container.get_host_port_ipv4(5432).await.expect("port");
        let url = format!("postgres://postgres:postgres@{host}:{port}/postgres");
        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await
            .expect("connect");
        sqlx::migrate!("../migrations")
            .run(&pool)
            .await
            .expect("migrations apply");
        Self {
            _container: container,
            pool,
        }
    }
}

/// Seed an org, one owner + one musician + one unrelated user, three
/// arrangements (a1/a2 live, a3 soft-deleted), a voice per live arrangement,
/// and a part assignment giving the musician a1's voice only. Returns
/// `(org_slug, owner_id, musician_id)`.
async fn seed(pool: &PgPool) -> (String, Uuid, Uuid) {
    let org_id = Uuid::now_v7();
    let org_slug = "acme".to_string();
    sqlx::query!(
        r#"INSERT INTO organization (id, name, slug) VALUES ($1, 'Acme', $2)"#,
        org_id,
        org_slug,
    )
    .execute(pool)
    .await
    .expect("org");

    let instrument_id: Uuid = sqlx::query_scalar!(r#"SELECT id FROM instrument LIMIT 1"#)
        .fetch_one(pool)
        .await
        .expect("a seeded instrument exists");

    let mk_user = |slug: &'static str| {
        let pool = pool.clone();
        async move {
            let id = Uuid::now_v7();
            sqlx::query!(
                r#"INSERT INTO "user" (id, slug, username, display_name) VALUES ($1, $2, $3, $4)"#,
                id,
                slug,
                slug,
                slug,
            )
            .execute(&pool)
            .await
            .expect("user");
            id
        }
    };
    let owner_id = mk_user("owner").await;
    let musician_id = mk_user("musician").await;

    let mk_member = |user_id: Uuid, role: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query!(
                r#"INSERT INTO membership
                     (id, user_id, organization_id, role, instrument_ids,
                      is_principal, principal_instrument_ids)
                   VALUES ($1, $2, $3, $4, '{}', false, '{}')"#,
                Uuid::now_v7(),
                user_id,
                org_id,
                role,
            )
            .execute(&pool)
            .await
            .expect("membership");
        }
    };
    mk_member(owner_id, "owner").await;
    mk_member(musician_id, "musician").await;

    // Arrangements: a1, a2 live; a3 soft-deleted.
    let mk_arr = |slug: &'static str, deleted: bool| {
        let pool = pool.clone();
        async move {
            let id = Uuid::now_v7();
            sqlx::query!(
                r#"INSERT INTO arrangement (id, organization_id, title, slug, status, deleted_at)
                   VALUES ($1, $2, $3, $3, 'active', CASE WHEN $4 THEN now() ELSE NULL END)"#,
                id,
                org_id,
                slug,
                deleted,
            )
            .execute(&pool)
            .await
            .expect("arrangement");
            id
        }
    };
    let a1 = mk_arr("a1", false).await;
    let _a2 = mk_arr("a2", false).await;
    let _a3 = mk_arr("a3", true).await;

    // One voice on a1, and a part assignment giving the musician that voice.
    let v1 = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO voice (id, arrangement_id, name, slug, instrument_id)
           VALUES ($1, $2, 'Flute 1', 'flute-1', $3)"#,
        v1,
        a1,
        instrument_id,
    )
    .execute(pool)
    .await
    .expect("voice");

    let coll = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO collection (id, organization_id, name, slug, type)
           VALUES ($1, $2, 'Spring', 'spring', 'program')"#,
        coll,
        org_id,
    )
    .execute(pool)
    .await
    .expect("collection");
    let item = Uuid::now_v7();
    sqlx::query!(
        r#"INSERT INTO collection_item (id, collection_id, arrangement_id, index)
           VALUES ($1, $2, $3, 1)"#,
        item,
        coll,
        a1,
    )
    .execute(pool)
    .await
    .expect("collection_item");
    sqlx::query!(
        r#"INSERT INTO part_assignment (id, collection_item_id, user_id, voice_id)
           VALUES ($1, $2, $3, $4)"#,
        Uuid::now_v7(),
        item,
        musician_id,
        v1,
    )
    .execute(pool)
    .await
    .expect("part_assignment");

    (org_slug, owner_id, musician_id)
}

#[tokio::test]
async fn staff_see_all_live_arrangements_musician_sees_only_assigned() {
    let db = TestDb::create_and_migrate().await;
    let (org_slug, owner_id, musician_id) = seed(&db.pool).await;

    // Owner is staff.
    let (org_id, owner_vis) = resolve_org_visibility(&db.pool, &org_slug, owner_id)
        .await
        .expect("resolve owner")
        .expect("org exists");
    assert_eq!(owner_vis, Visibility::Staff);

    // Musician is restricted.
    let (_, musician_vis) = resolve_org_visibility(&db.pool, &org_slug, musician_id)
        .await
        .expect("resolve musician")
        .expect("org exists");
    assert_eq!(musician_vis, Visibility::Restricted);

    // Staff sees a1 + a2 (a3 soft-deleted, hidden), never a3.
    let staff_view = visible_arrangement_slugs(&db.pool, org_id, owner_id, owner_vis)
        .await
        .expect("staff listing");
    assert_eq!(staff_view, vec!["a1".to_string(), "a2".to_string()]);

    // Musician sees only a1 (the arrangement they hold an assignment on).
    let musician_view = visible_arrangement_slugs(&db.pool, org_id, musician_id, musician_vis)
        .await
        .expect("musician listing");
    assert_eq!(musician_view, vec!["a1".to_string()]);
}

#[tokio::test]
async fn unknown_org_slug_resolves_to_none() {
    let db = TestDb::create_and_migrate().await;
    let (_org_slug, owner_id, _musician_id) = seed(&db.pool).await;

    let resolved = resolve_org_visibility(&db.pool, "does-not-exist", owner_id)
        .await
        .expect("query runs");
    assert!(resolved.is_none());
}
