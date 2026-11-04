//! Fresh media reads use primary-database event and channel state.
use crate::{Db, DbError, Result};
use buzz_core::{kind, media::reference_hashes, CommunityId};
use buzz_datastore_tracing::datastore_span;
use futures_util::TryStreamExt;
use sqlx::{Postgres, QueryBuilder, Row};

/// Result of a complete live-reference lookup, for bounded denial metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaAccess {
    /// A current readable reference.
    Allowed,
    /// Uploaded bytes with no past publication boundary or current reference.
    UploaderPreview,
    /// No published reference or upload proof exists.
    NoReference,
    /// References exist but none is currently readable.
    Restricted,
}

impl Db {
    /// Record possession after the upload pipeline verified the actual bytes.
    /// Independent of optional audit and moderation records; admission fences deletion.
    #[datastore_span(name = "record_media_upload", system = "postgresql")]
    pub async fn record_media_upload(
        &self,
        community: CommunityId,
        sha256: &str,
        pubkey: &[u8],
    ) -> Result<()> {
        let mut tx = crate::begin_community_write_transaction(
            &self.pool,
            community,
            crate::observability::WriterOperation::EventWrite,
        )
        .await?;
        sqlx::query("INSERT INTO media_uploaders (community_id, sha256, pubkey) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING")
            .bind(community.as_uuid()).bind(sha256).bind(pubkey).execute(tx.conn()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Record a sticky publication boundary before dispatch, even if the later
    /// command fails. This grants no read access and survives source purging.
    pub async fn record_media_publications(
        &self,
        community: CommunityId,
        hashes: Vec<String>,
    ) -> Result<()> {
        let mut tx = crate::begin_community_write_transaction(
            &self.pool,
            community,
            crate::observability::WriterOperation::EventWrite,
        )
        .await?;
        sqlx::query("INSERT INTO media_publications (community_id,sha256) SELECT $1, unnest($2::text[]) ON CONFLICT DO NOTHING")
            .bind(community.as_uuid()).bind(hashes).execute(tx.conn()).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Owner-uploaded bytes may become a registered agent's profile picture.
    /// Callers must restrict this delegation to that one field, not messages.
    pub async fn agent_owner_uploaded_media(
        &self,
        community: CommunityId,
        hash: &str,
        agent: &[u8],
    ) -> Result<bool> {
        let mut connection = crate::observability::acquire_writer(
            &self.pool,
            crate::observability::WriterOperation::Authorization,
        )
        .await?;
        Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users u JOIN media_uploaders m ON m.community_id=u.community_id AND m.pubkey=u.agent_owner_pubkey WHERE u.community_id=$1 AND u.pubkey=$2 AND u.deactivated_at IS NULL AND m.sha256=$3)")
            .bind(community.as_uuid()).bind(agent).bind(hash).fetch_one(&mut *connection).await?)
    }

    /// Resolve an attachment through live references, never a membership cache
    /// or read replica. A repeatable-read snapshot covers both indexed references
    /// and profile fallback, including the absence check for uploader previews.
    /// `visible` is the ordinary relay REQ visibility predicate.
    #[allow(clippy::too_many_arguments)] // Complete tenant/media/reader authority stays explicit.
    #[datastore_span(name = "media_access", system = "postgresql")]
    pub async fn media_access(
        &self,
        community: CommunityId,
        sha256: &str,
        ext: &str,
        base: &str,
        pubkey: &[u8],
        for_publication: bool,
        visible: fn(&nostr::Event, &[u8]) -> bool,
    ) -> Result<MediaAccess> {
        let connection = crate::observability::acquire_writer(
            &self.pool,
            crate::observability::WriterOperation::Authorization,
        )
        .await?;
        let mut tx = sqlx::Transaction::begin(connection, None).await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await?;
        sqlx::query("SET LOCAL statement_timeout = '2s'")
            .execute(&mut *tx)
            .await?;
        let uploaded: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM media_uploaders WHERE community_id=$1 AND sha256=$2 AND pubkey=$3)")
            .bind(community.as_uuid()).bind(sha256).bind(pubkey).fetch_one(&mut *tx).await?;
        if uploaded && for_publication {
            return Ok(MediaAccess::Allowed);
        }

        let mut needles = vec![serde_json::json!([["imeta", format!("x {sha256}")]])];
        let mut bases = vec!["/media".to_owned(), base.trim_end_matches('/').to_owned()];
        // Both schemes have historically appeared in tenant descriptor URLs.
        if let Some(rest) = base.strip_prefix("https://") {
            bases.push(format!("http://{rest}"));
        }
        if let Some(rest) = base.strip_prefix("http://") {
            bases.push(format!("https://{rest}"));
        }
        for prefix in bases {
            for suffix in [
                String::new(),
                format!(".{ext}"),
                ".thumb.jpg".into(),
                ".thumb.webp".into(),
            ] {
                let url = format!("{prefix}/{sha256}{suffix}");
                for field in ["url", "image"] {
                    needles.push(serde_json::json!([["imeta", format!("{field} {url}")]]));
                }
                needles.push(serde_json::json!([["picture", url]]));
                needles.push(serde_json::json!([["emoji", url]]));
            }
        }
        let mut referenced = false;
        let mut scanned_bytes = 0usize;
        for probe in 0..3 {
            let mut query = QueryBuilder::<Postgres>::new(
                "SELECT e.id,e.pubkey,e.created_at,e.kind,e.tags,e.content,e.sig,e.received_at,e.channel_id, \
                 (e.deleted_at IS NULL AND (e.channel_id IS NULL OR EXISTS ( \
                     SELECT 1 FROM channels c WHERE c.community_id=e.community_id AND c.id=e.channel_id \
                     AND c.deleted_at IS NULL AND (c.visibility='open' OR EXISTS ( \
                         SELECT 1 FROM channel_members m WHERE m.community_id=c.community_id AND m.channel_id=c.id \
                         AND m.removed_at IS NULL AND m.pubkey="
            );
            query
                .push_bind(pubkey)
                .push(
                    ")))) AND (e.kind <> 40003 OR EXISTS (SELECT 1 FROM events original \
                WHERE original.community_id=e.community_id AND original.channel_id=e.channel_id \
                AND original.deleted_at IS NULL AND original.kind IN (9,40002) \
                AND lower((SELECT tag->>1 FROM jsonb_array_elements(e.tags) WITH ORDINALITY AS t(tag,ord) \
                    WHERE tag->>0='e' AND tag->>1 ~ '^[0-9A-Fa-f]{64}$' ORDER BY ord LIMIT 1))=encode(original.id,'hex') \
                AND NOT EXISTS (SELECT 1 FROM events newer \
                    WHERE newer.community_id=e.community_id AND newer.channel_id=e.channel_id \
                    AND newer.kind=40003 AND newer.deleted_at IS NULL \
                    AND (newer.created_at,newer.id) > (e.created_at,e.id) \
                    AND lower((SELECT tag->>1 FROM jsonb_array_elements(newer.tags) WITH ORDINALITY AS t(tag,ord) \
                    WHERE tag->>0='e' AND tag->>1 ~ '^[0-9A-Fa-f]{64}$' ORDER BY ord LIMIT 1))=encode(original.id,'hex')))) \
                AND (e.kind NOT IN (9,40002) OR NOT EXISTS (SELECT 1 FROM events edit \
                    WHERE edit.community_id=e.community_id AND edit.channel_id=e.channel_id \
                    AND edit.kind=40003 AND edit.deleted_at IS NULL \
                    AND lower((SELECT tag->>1 FROM jsonb_array_elements(edit.tags) WITH ORDINALITY AS t(tag,ord) \
                    WHERE tag->>0='e' AND tag->>1 ~ '^[0-9A-Fa-f]{64}$' ORDER BY ord LIMIT 1))=encode(e.id,'hex')))) AS readable \
                FROM events e WHERE e.community_id=",
                )
                .push_bind(community.as_uuid());
            if probe == 2 {
                // Never scan message bodies. Only profiles have content references;
                // current profiles are indexed by community/kind/deleted_at.
                query
                    .push(" AND e.kind=0 AND e.deleted_at IS NULL AND (strpos(e.content, ")
                    .push_bind(sha256)
                    .push(") > 0 OR strpos(e.content, E'\\\\u') > 0)");
            } else {
                query.push(" AND (");
                for (i, needle) in (if probe == 0 {
                    &needles[..1]
                } else {
                    &needles[1..]
                })
                .iter()
                .enumerate()
                {
                    if i > 0 {
                        query.push(" OR ");
                    }
                    query.push("e.tags @> ").push_bind(needle);
                }
                query.push(")");
            }
            query.push(" LIMIT 1025");
            // Stream candidates: a frequently shared hash must not allocate
            // 1,025 full message bodies before finding the first readable one.
            let statement = query.build();
            let mut rows = statement.fetch(&mut *tx);
            let mut count = 0;
            while let Some(row) = rows.try_next().await? {
                count += 1;
                let content: &str = row.try_get("content")?;
                let tags: serde_json::Value = row.try_get("tags")?;
                scanned_bytes = scanned_bytes
                    .saturating_add(content.len())
                    .saturating_add(tags.to_string().len());
                if count > 1024 || scanned_bytes > 16 * 1024 * 1024 {
                    return Err(DbError::InvalidData(
                        "media reference work budget exceeded".into(),
                    ));
                }
                let readable: bool = row.try_get("readable")?;
                let stored = crate::event::row_to_stored_event(row)?.ok_or_else(|| {
                    DbError::InvalidData("malformed media reference event".into())
                })?;
                let event = &stored.event;
                if !reference_hashes(event, base).contains(sha256) {
                    continue;
                }
                referenced = true;
                let kind = event.kind.as_u16() as u32;
                let tagged = event.tags.iter().any(|tag| matches!(tag.as_slice(), [name, value, ..] if name == "p" && value == &hex::encode(pubkey)));
                let private_ok = (!kind::P_GATED_KINDS.contains(&kind) || tagged)
                    && (kind != kind::KIND_AGENT_ENGRAM
                        || event.pubkey.as_bytes() == pubkey
                        || tagged);
                if readable && private_ok && visible(event, pubkey) {
                    return Ok(MediaAccess::Allowed);
                }
            }
        }
        // Community icons are current community state; commands validate their
        // structured URLs before updating this row. They grant community access.
        let icon: Option<String> = sqlx::query_scalar("SELECT icon FROM communities WHERE id=$1")
            .bind(community.as_uuid())
            .fetch_one(&mut *tx)
            .await?;
        if icon
            .as_deref()
            .and_then(|value| buzz_core::media::local_hash(value, base))
            .as_deref()
            == Some(sha256)
        {
            return Ok(MediaAccess::Allowed);
        }
        let published: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM media_publications WHERE community_id=$1 AND sha256=$2)",
        )
        .bind(community.as_uuid())
        .bind(sha256)
        .fetch_one(&mut *tx)
        .await?;
        if referenced || published {
            Ok(MediaAccess::Restricted)
        } else if uploaded {
            Ok(MediaAccess::UploaderPreview)
        } else {
            Ok(MediaAccess::NoReference)
        }
    }
}
