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
        tags.push(Tag::parse(["e", &target.id.to_hex()]).unwrap());
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
