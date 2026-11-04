//! Prevent a copied inaccessible URL from granting access through a new event.
use super::ingest::IngestError;
use crate::{api::media_access::MediaReadMode, state::AppState};
use buzz_core::TenantContext;
use nostr::Event;

pub(crate) async fn validate_media_references(
    tenant: &TenantContext,
    state: &AppState,
    event: &Event,
) -> Result<(), IngestError> {
    let mode = state.config.media_read_mode;
    if mode == MediaReadMode::Off {
        return Ok(());
    }
    let base = crate::api::media::media_base_url_for_tenant(&state.config.relay_url, tenant.host());
    let hashes = buzz_core::media::publication_hashes(event, &base);
    if hashes.len() > 64 {
        return Err(IngestError::Rejected(
            "invalid: too many media references".into(),
        ));
    }
    let owner_picture = if event.kind == nostr::Kind::Metadata {
        serde_json::from_str::<serde_json::Value>(&event.content)
            .ok()
            .and_then(|value| {
                value
                    .get("picture")
                    .and_then(|v| v.as_str())
                    .and_then(|url| buzz_core::media::local_hash(url, &base))
            })
    } else {
        None
    };
    let author = super::ingest::effective_message_author(event, &state.relay_keypair.public_key());
    for hash in &hashes {
        let meta = state
            .media_storage
            .get_sidecar(tenant, hash)
            .await
            .map_err(|_| IngestError::Internal("error: media reference lookup failed".into()))?;

        let result = state
            .db
            .media_access(
                tenant.community(),
                hash,
                &meta.ext,
                &base,
                &author,
                true,
                super::req::event_visible_to_reader,
            )
            .await;
        let result = match result {
            Ok(access)
                if access != buzz_db::media::MediaAccess::Allowed
                    && owner_picture.as_ref() == Some(hash) =>
            {
                state
                    .db
                    .agent_owner_uploaded_media(tenant.community(), hash, &author)
                    .await
                    .map(|uploaded| {
                        if uploaded {
                            buzz_db::media::MediaAccess::Allowed
                        } else {
                            access
                        }
                    })
            }
            result => result,
        };
        let reason = match &result {
            Ok(
                buzz_db::media::MediaAccess::Allowed | buzz_db::media::MediaAccess::UploaderPreview,
            ) => "allow",
            Ok(_) => "denied",
            Err(_) => "lookup_error",
        };
        metrics::counter!("buzz_media_publication_access_total", "mode" => mode.label(), "reason" => reason).increment(1);
        if mode == MediaReadMode::Enforce {
            match result {
                Ok(
                    buzz_db::media::MediaAccess::Allowed
                    | buzz_db::media::MediaAccess::UploaderPreview,
                ) => {}
                Ok(_) => {
                    return Err(IngestError::Rejected(
                        "restricted: attachment is not accessible".into(),
                    ))
                }
                Err(_) => {
                    return Err(IngestError::Internal(
                        "error: media authorization unavailable".into(),
                    ))
                }
            }
        }
    }
    // Consume preview authority before dispatch. A rejected publication may
    // conservatively consume a preview, but cannot create a download grant.
    if !hashes.is_empty() {
        state
            .db
            .record_media_publications(tenant.community(), hashes.into_iter().collect())
            .await
            .map_err(|_| {
                IngestError::Internal("error: media publication recording failed".into())
            })?;
    }
    Ok(())
}
