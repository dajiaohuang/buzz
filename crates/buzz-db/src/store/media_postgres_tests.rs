use super::media::MediaAccess;
use crate::{
    channel::{ChannelType, ChannelVisibility},
    Db,
};
use buzz_core::CommunityId;
use nostr::{Event, EventBuilder, Keys, Kind, Tag, Timestamp};

async fn allowed(
    db: &Db,
    community: CommunityId,
    hash: &str,
    base: &str,
    keys: &Keys,
) -> MediaAccess {
    db.media_access(
        community,
        hash,
        "bin",
        base,
        keys.public_key().as_bytes(),
        false,
        |_, _| true,
    )
    .await
    .unwrap()
}
fn attachment(
    keys: &Keys,
    hash: &str,
    channel: uuid::Uuid,
    kind: u16,
    target: Option<&Event>,
    at: u64,
) -> Event {
    let mut tags = vec![Tag::parse(["h", &channel.to_string()]).unwrap()];
    if !hash.is_empty() {
        tags.push(
            Tag::parse([
                "imeta",
                &format!("url /media/{hash}.bin"),
                &format!("x {hash}"),
            ])
            .unwrap(),
        );
    }
    if let Some(target) = target {
        tags.push(Tag::parse(["e", &target.id.to_hex().to_uppercase()]).unwrap());
    }
    EventBuilder::new(Kind::Custom(kind), "media")
        .tags(tags)
        .custom_created_at(Timestamp::from(at))
        .sign_with_keys(keys)
        .unwrap()
}
#[tokio::test]
#[ignore = "requires Postgres"]
async fn edits_deletion_legacy_profiles_and_publication_boundaries() {
    let pool = sqlx::PgPool::connect(&crate::test_support::database_url())
        .await
        .unwrap();
    let db = Db::from_pool(pool);
    let host = format!("media-db-{}.example", uuid::Uuid::new_v4());
    let community = db.ensure_configured_community(&host).await.unwrap().id;
    let base = format!("https://{host}/media");
    let owner = Keys::generate();
    let outsider = Keys::generate();
    db.ensure_user(community, owner.public_key().as_bytes())
        .await
        .unwrap();
    let channel = db
        .create_channel(
            community,
            "media",
            ChannelType::Stream,
            ChannelVisibility::Private,
            None,
            owner.public_key().as_bytes(),
            None,
        )
        .await
        .unwrap()
        .id;
    let a = "a".repeat(64);
    let b = "b".repeat(64);
    db.record_media_upload(community, &a, owner.public_key().as_bytes())
        .await
        .unwrap();
    assert_eq!(
        allowed(&db, community, &a, &base, &owner).await,
        MediaAccess::UploaderPreview
    );
    assert_eq!(
        allowed(&db, community, &a, &base, &outsider).await,
        MediaAccess::NoReference
    );
    let now = Timestamp::now().as_secs();
    let original = attachment(&owner, &a, channel, 9, None, now);
    db.record_media_publications(community, vec![a.clone(), b.clone()])
        .await
        .unwrap();
    db.insert_event(community, &original, Some(channel))
        .await
        .unwrap();
    assert_eq!(
        allowed(&db, community, &a, &base, &owner).await,
        MediaAccess::Allowed
    );
    assert_eq!(
        allowed(&db, community, &a, &base, &outsider).await,
        MediaAccess::Restricted
    );
    let decoy = attachment(&owner, "", channel, 9, None, now + 1);
    db.insert_event(community, &decoy, Some(channel))
        .await
        .unwrap();
    let edit = attachment(&owner, &b, channel, 40003, Some(&original), now + 1);
    let edit = EventBuilder::new(Kind::Custom(40003), &edit.content)
        .tags(edit.tags.iter().cloned())
        .tags([Tag::parse(["e", &decoy.id.to_hex()]).unwrap()])
        .custom_created_at(Timestamp::from(now + 1))
        .sign_with_keys(&owner)
        .unwrap();
    db.insert_event(community, &edit, Some(channel))
        .await
        .unwrap();
    assert_eq!(
        allowed(&db, community, &a, &base, &owner).await,
        MediaAccess::Restricted,
        "edit drops original attachment"
    );
    assert_eq!(
        allowed(&db, community, &b, &base, &owner).await,
        MediaAccess::Allowed
    );
    sqlx::query("UPDATE events SET deleted_at=now() WHERE community_id=$1 AND id=$2")
        .bind(community.as_uuid())
        .bind(original.id.as_bytes().as_slice())
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(
        allowed(&db, community, &b, &base, &owner).await,
        MediaAccess::Restricted,
        "a second e tag cannot substitute for the deleted edit target"
    );
    sqlx::query("UPDATE events SET deleted_at=NULL WHERE community_id=$1 AND id=$2")
        .bind(community.as_uuid())
        .bind(original.id.as_bytes().as_slice())
        .execute(db.pool())
        .await
        .unwrap();
    let edit2 = attachment(&owner, "", channel, 40003, Some(&original), now + 2);
    db.insert_event(community, &edit2, Some(channel))
        .await
        .unwrap();
    assert_eq!(
        allowed(&db, community, &b, &base, &owner).await,
        MediaAccess::Restricted,
        "newest edit alone supplies references"
    );
    sqlx::query("DELETE FROM events WHERE community_id=$1")
        .bind(community.as_uuid())
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(
        allowed(&db, community, &a, &base, &owner).await,
        MediaAccess::Restricted,
        "purge must not resurrect uploader preview"
    );
    // Profiles predate recording and therefore need no grant backfill.
    sqlx::query(r#"INSERT INTO events(community_id,id,pubkey,created_at,kind,tags,content,sig)
        SELECT $1,decode(md5(g::text)||md5(g::text),'hex'),decode(md5(g::text)||md5(g::text),'hex'),now(),0,'[]',
        '{"name":"\u00e9"}',decode(repeat('a',128),'hex') FROM generate_series(1,1100) g"#)
        .bind(community.as_uuid()).execute(db.pool()).await.unwrap();
    let profile = EventBuilder::new(
        Kind::Metadata,
        serde_json::json!({"picture":format!("{base}/{a}.bin")}).to_string(),
    )
    .sign_with_keys(&owner)
    .unwrap();
    db.insert_event(community, &profile, None).await.unwrap();
    assert_eq!(
        allowed(&db, community, &a, &base, &outsider).await,
        MediaAccess::Allowed
    );
    sqlx::query("DELETE FROM events WHERE community_id=$1 AND kind=0 AND pubkey<>$2")
        .bind(community.as_uuid())
        .bind(owner.public_key().as_bytes().as_slice())
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE events SET deleted_at=now() WHERE community_id=$1 AND id=$2")
        .bind(community.as_uuid())
        .bind(profile.id.as_bytes().as_slice())
        .execute(db.pool())
        .await
        .unwrap();
    assert_eq!(
        allowed(&db, community, &a, &base, &outsider).await,
        MediaAccess::Restricted,
        "superseded avatar grants nothing"
    );
    // Accepted image-only and animated profiles remain readable after publishing.
    // Fully escaped animation hashes exercise the descriptor candidate fallback.
    let encoded = format!("{base}/{a}.bin")
        .replace(':', "%3A")
        .replace('/', "%2F")
        .replace('a', "%61");
    for content in [
        serde_json::json!({"image":format!("{base}/{a}.bin")}),
        serde_json::json!({"picture":format!("https://foreign/poster.png#buzz-anim={encoded}")}),
    ] {
        let current = EventBuilder::new(Kind::Metadata, content.to_string())
            .sign_with_keys(&owner)
            .unwrap();
        db.insert_event(community, &current, None).await.unwrap();
        assert_eq!(
            allowed(&db, community, &a, &base, &outsider).await,
            MediaAccess::Allowed
        );
        sqlx::query("UPDATE events SET deleted_at=now() WHERE community_id=$1 AND id=$2")
            .bind(community.as_uuid())
            .bind(current.id.as_bytes().as_slice())
            .execute(db.pool())
            .await
            .unwrap();
        assert_eq!(
            allowed(&db, community, &a, &base, &owner).await,
            MediaAccess::Restricted
        );
    }
    // A tenant never borrows another tenant's proof or publication.
    let other = db
        .ensure_configured_community(&format!("other-{}.example", uuid::Uuid::new_v4()))
        .await
        .unwrap()
        .id;
    assert_eq!(
        allowed(&db, other, &a, &base, &owner).await,
        MediaAccess::NoReference
    );
}

#[tokio::test]
#[ignore = "requires Postgres"]
async fn popular_media_prefers_live_readable_references_and_bounds_work() {
    let pool = sqlx::PgPool::connect(&crate::test_support::database_url())
        .await
        .unwrap();
    let db = Db::from_pool(pool);
    let host = format!("popular-media-{}.example", uuid::Uuid::new_v4());
    let community = db.ensure_configured_community(&host).await.unwrap().id;
    let base = format!("https://{host}/media");
    let owner = Keys::generate();
    let reader = Keys::generate();
    db.ensure_user(community, owner.public_key().as_bytes())
        .await
        .unwrap();
    let channel = db
        .create_channel(
            community,
            "popular",
            ChannelType::Stream,
            ChannelVisibility::Private,
            None,
            owner.public_key().as_bytes(),
            None,
        )
        .await
        .unwrap()
        .id;
    let hash = "c".repeat(64);
    let emoji_url = format!("{base}/{hash}.bin");
    // Non-counting reactions and superseded sets must never consume the budget.
    sqlx::query("INSERT INTO events (community_id,id,pubkey,created_at,kind,tags,content,sig,deleted_at) SELECT $1,decode(lpad(to_hex(n),64,'0'),'hex'),$2,now(),CASE WHEN n%2=0 THEN 7 ELSE 30030 END,$3,'emoji',decode(repeat('00',64),'hex'),CASE WHEN n%2=1 THEN now() ELSE NULL END FROM generate_series(1,2052) n")
        .bind(community.as_uuid()).bind(owner.public_key().as_bytes().as_slice())
        .bind(serde_json::json!([["emoji","popular",emoji_url]]))
        .execute(db.pool()).await.unwrap();
    let live = EventBuilder::new(Kind::Custom(30030), "")
        .tags([
            Tag::parse(["d", "current"]).unwrap(),
            Tag::parse(["emoji", "popular", &emoji_url]).unwrap(),
        ])
        .sign_with_keys(&owner)
        .unwrap();
    db.insert_event(community, &live, None).await.unwrap();
    assert_eq!(
        allowed(&db, community, &hash, &base, &reader).await,
        MediaAccess::Allowed
    );
    let shared_hash = "d".repeat(64);
    sqlx::query("INSERT INTO events (community_id,id,pubkey,created_at,kind,tags,content,sig,channel_id) SELECT $1,decode(lpad(to_hex(n),64,'0'),'hex'),$2,now(),9,$3,'private',decode(repeat('00',64),'hex'),$4 FROM generate_series(3000,4050) n")
        .bind(community.as_uuid()).bind(owner.public_key().as_bytes().as_slice())
        .bind(serde_json::json!([["imeta",format!("x {shared_hash}"),format!("url /media/{shared_hash}.bin")]]))
        .bind(channel).execute(db.pool()).await.unwrap();
    let public = attachment(
        &owner,
        &shared_hash,
        channel,
        1,
        None,
        Timestamp::now().as_secs(),
    );
    db.insert_event(community, &public, None).await.unwrap();
    assert_eq!(
        allowed(&db, community, &shared_hash, &base, &reader).await,
        MediaAccess::Allowed
    );
    // An oversized readable row must fail closed without buffering all candidates.
    let huge_hash = "e".repeat(64);
    let huge = EventBuilder::new(Kind::Custom(1), "z".repeat(16 * 1024 * 1024 + 1))
        .tags([Tag::parse([
            "imeta",
            &format!("x {huge_hash}"),
            &format!("url /media/{huge_hash}.bin"),
        ])
        .unwrap()])
        .sign_with_keys(&owner)
        .unwrap();
    db.insert_event(community, &huge, None).await.unwrap();
    let error = db
        .media_access(
            community,
            &huge_hash,
            "bin",
            &base,
            reader.public_key().as_bytes(),
            false,
            |_, _| true,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("work budget"), "{error}");
    // Capacity saturation has a bounded wait before attempting to acquire a writer.
    let permits = db.media_authorization_slots.available_permits();
    let _held = db
        .media_authorization_slots
        .acquire_many(permits as u32)
        .await
        .unwrap();
    let error = db
        .media_access(
            community,
            &hash,
            "bin",
            &base,
            reader.public_key().as_bytes(),
            false,
            |_, _| true,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, crate::DbError::MediaAuthorizationCapacity),
        "{error}"
    );
    // Poll until the request is queued, then release capacity without a delay.
    let reader_pubkey = reader.public_key();
    let waiting = db.media_access(
        community,
        &hash,
        "bin",
        &base,
        reader_pubkey.as_bytes(),
        false,
        |_, _| true,
    );
    tokio::pin!(waiting);
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    drop(_held);
    assert_eq!(waiting.await.unwrap(), MediaAccess::Allowed);

    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(db.pool())
            .await
            .unwrap(),
        1
    );
}

/// Explain the actual complete statement, including channel and edit joins.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn media_reference_full_query_plans() {
    use sqlx::{Execute, Row};
    let pool = sqlx::PgPool::connect(&crate::test_support::database_url())
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let fixture = include_str!("media_plan_fixture.sql");
    sqlx::raw_sql(fixture).execute(&mut *tx).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE INDEX idx_events_media_edit_target ON media_access_plan.events (community_id,channel_id, ({}),created_at DESC,id DESC) WHERE kind=40003 AND deleted_at IS NULL", super::media::MEDIA_EDIT_TARGET_SQL)))
        .execute(&mut *tx).await.unwrap();
    sqlx::query("SET LOCAL jit = on")
        .execute(&mut *tx)
        .await
        .unwrap();
    let jit_available: bool = sqlx::query_scalar("SELECT pg_jit_available()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    sqlx::raw_sql(super::media::MEDIA_LOOKUP_SETTINGS_SQL)
        .execute(&mut *tx)
        .await
        .unwrap();
    let jit: String = sqlx::query_scalar("SHOW jit")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        jit, "off",
        "the production transaction must not compile partitioned authorization probes"
    );
    println!(
        "PostgreSQL: {}; JIT available: {}; authorization jit={jit}",
        sqlx::query_scalar::<_, String>("SELECT version()")
            .fetch_one(&mut *tx)
            .await
            .unwrap(),
        jit_available
    );
    let community = CommunityId::from_uuid(uuid::Uuid::from_u128(1));
    let hash = "a".repeat(64);
    let reader = [0x11; 32];
    let needles = super::media::media_reference_needles(&hash, "bin", "https://plan.example/media");
    for readable_only in [true, false] {
        for probe in 0..3 {
            let mut builder = super::media::media_reference_query(
                community,
                &hash,
                &reader,
                &needles,
                probe,
                readable_only,
            );
            let mut statement = builder.build();
            let args = statement.take_arguments().unwrap().unwrap();
            let sql = format!(
                "EXPLAIN (ANALYZE, BUFFERS, COSTS ON) {}",
                statement.sql().as_str()
            );
            // Only the production QueryBuilder SQL is prefixed; all values stay bound.
            let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                .fetch_all(&mut *tx)
                .await
                .unwrap();
            println!("FULL_QUERY_PROBE_{probe}_READABLE_{readable_only}: 24 partitions, 1200000 rows including 300000 live edits across 256 channels");
            let plan = rows
                .iter()
                .map(|row| row.get::<String, _>(0))
                .collect::<Vec<_>>()
                .join("\n");
            if readable_only && probe < 2 {
                let names: Vec<String> = sqlx::query_scalar("SELECT c.relname::text FROM pg_inherits h JOIN pg_class c ON c.oid=h.inhrelid WHERE h.inhparent='media_access_plan.idx_events_media_edit_target'::regclass")
                .fetch_all(&mut *tx).await.unwrap();
                assert!(
                    names.iter().any(|name| plan.contains(name)),
                    "edit-target index absent: {plan}"
                );
            }
            let execution_ms: f64 = plan
                .lines()
                .find_map(|line| line.strip_prefix("Execution Time: "))
                .and_then(|value| value.strip_suffix(" ms"))
                .unwrap()
                .parse()
                .unwrap();
            assert!(
                execution_ms < 1000.0,
                "authorization probe must retain headroom under its 2s timeout: {plan}"
            );
            for row in rows {
                println!("{}", row.get::<String, _>(0));
            }
        }
    }
    tx.rollback().await.unwrap();
}

/// Compare the production Rust target rule to SQL, the catalog and new partitions.
#[tokio::test]
#[ignore = "requires Postgres"]
async fn media_edit_target_sql_parity_and_partition_inheritance() {
    use serde_json::json;
    let pool = sqlx::PgPool::connect(&crate::test_support::database_url())
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let expression = super::media::MEDIA_EDIT_TARGET_SQL;
    let a = "aB".repeat(32);
    let b = "C".repeat(64);
    let cases = [
        json!([["e", a.to_uppercase()]]),
        json!([["e", a]]),
        json!([["e", "bad"], ["e", a], ["e", b]]),
        json!([["e", a], ["e", b]]),
        json!([["e"], ["e", 23], ["e", a]]),
        json!([["e"]]),
        json!([["e", 23]]),
        json!([null, ["e", a]]),
        json!(null),
        json!([]),
    ];
    let absent: Option<Vec<u8>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {expression} FROM (SELECT NULL::jsonb AS tags) input"
    )))
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert!(absent.is_none());
    let keys = Keys::generate();
    for tags in cases {
        // Non-string historical tags cannot be signed by the Rust SDK; omit
        // malformed tags when constructing its reference input.
        let valid_tags: Vec<Tag> = tags
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tag| {
                let values: Option<Vec<&str>> =
                    tag.as_array()?.iter().map(|v| v.as_str()).collect();
                Tag::parse(values?).ok()
            })
            .collect();
        let event = EventBuilder::new(Kind::Custom(40003), "")
            .tags(valid_tags)
            .sign_with_keys(&keys)
            .unwrap();
        let expected = buzz_core::media::first_edit_target(&event);
        let actual: Option<Vec<u8>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT {expression} FROM (SELECT $1::jsonb AS tags) input"
        )))
        .bind(&tags)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(actual, expected, "tags={tags}");
    }
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE TEMP TABLE media_expected_target(tags jsonb); CREATE INDEX media_expected_target_idx ON media_expected_target (({expression}));")))
        .execute(&mut *tx).await.unwrap();
    let expressions: Vec<String> = sqlx::query_scalar("SELECT pg_get_expr(indexprs,indrelid) FROM pg_index WHERE indexrelid IN ('media_expected_target_idx'::regclass,'public.idx_events_media_edit_target'::regclass) ORDER BY indexrelid")
        .fetch_all(&mut *tx).await.unwrap();
    assert_eq!(expressions.len(), 2);
    assert_eq!(expressions[0], expressions[1]);
    sqlx::raw_sql("CREATE TABLE media_partition_parent (LIKE public.events INCLUDING ALL) PARTITION BY RANGE (created_at); CREATE TABLE media_future_partition PARTITION OF media_partition_parent FOR VALUES FROM ('2099-01-01') TO ('2099-02-01');")
        .execute(&mut *tx).await.unwrap();
    let inherited: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_index i JOIN pg_inherits h ON h.inhrelid=i.indexrelid JOIN pg_index parent ON parent.indexrelid=h.inhparent WHERE i.indrelid='media_future_partition'::regclass AND parent.indexprs IS NOT NULL AND i.indisvalid AND pg_get_expr(i.indexprs,i.indrelid)=(SELECT pg_get_expr(indexprs,indrelid) FROM pg_index WHERE indexrelid='public.idx_events_media_edit_target'::regclass))")
        .fetch_one(&mut *tx).await.unwrap();
    assert!(inherited);
    tx.rollback().await.unwrap();
}
