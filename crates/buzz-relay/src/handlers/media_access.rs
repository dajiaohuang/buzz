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
    let author = super::ingest::effective_message_author(event, &state.relay_keypair.public_key());
    for hash in &hashes {
        let checked = async {
            let meta = state
                .media_storage
                .get_sidecar(tenant, hash)
                .await
                .map_err(|error| {
                    if matches!(error, buzz_media::MediaError::NotFound) {
                        (
                            "missing_sidecar",
                            IngestError::Rejected(
                                "restricted: attachment is not accessible".into(),
                            ),
                        )
                    } else {
                        (
                            "lookup_error",
                            IngestError::Internal("error: media reference lookup failed".into()),
                        )
                    }
                })?;
            let access = state
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
                .await
                .map_err(|_| {
                    (
                        "lookup_error",
                        IngestError::Internal("error: media authorization unavailable".into()),
                    )
                })?;
            if matches!(
                access,
                buzz_db::media::MediaAccess::Allowed | buzz_db::media::MediaAccess::UploaderPreview
            ) {
                Ok(())
            } else {
                Err((
                    "denied",
                    IngestError::Rejected("restricted: attachment is not accessible".into()),
                ))
            }
        }
        .await;
        let reason = checked
            .as_ref()
            .err()
            .map_or("allow", |(reason, _)| *reason);
        metrics::counter!("buzz_media_publication_access_total", "mode" => mode.label(), "reason" => reason).increment(1);
        if let Err((_, error)) = checked {
            if mode == MediaReadMode::Enforce {
                return Err(error);
            }
        }
    }
    // A marker consumes previews but never grants download access. Shadow
    // recording is best effort and cannot change event acceptance on failure.
    if !hashes.is_empty()
        && state
            .db
            .record_media_publications(tenant.community(), hashes.into_iter().collect())
            .await
            .is_err()
    {
        metrics::counter!("buzz_media_publication_access_total", "mode" => mode.label(), "reason" => "record_error").increment(1);
        if mode == MediaReadMode::Enforce {
            return Err(IngestError::Internal(
                "error: media publication recording failed".into(),
            ));
        }
    }
    Ok(())
}
