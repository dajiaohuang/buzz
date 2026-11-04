//! A shared vocabulary for local media references on reads and publications.
use nostr::Event;
use std::collections::BTreeSet;

/// Parse a local media URL, returning its parent hash (also for thumbnails).
/// Only the tenant's canonical base and root-relative paths count.
pub fn local_hash(value: &str, base: &str) -> Option<String> {
    let alternate = if let Some(rest) = base.strip_prefix("https://") {
        format!("http://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("https://{rest}")
    } else {
        base.to_owned()
    };
    let prefix = format!("{}/", base.trim_end_matches('/'));
    let alternate_prefix = format!("{}/", alternate.trim_end_matches('/'));
    let rest = value
        .strip_prefix("/media/")
        .or_else(|| value.strip_prefix(&prefix))
        .or_else(|| value.strip_prefix(&alternate_prefix))?;
    let mut parts = rest.split('.');
    let hash = parts.next()?;
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let safe_ext = |s: &str| {
        !s.is_empty()
            && s.len() <= 8
            && s.bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    };
    match (parts.next(), parts.next(), parts.next()) {
        (None, None, None) => {}
        (Some(ext), None, None) if safe_ext(ext) => {}
        (Some("thumb"), Some("jpg" | "webp"), None) => {}
        _ => return None,
    }
    Some(hash.to_owned())
}

/// Extract counted references. imeta URL/image fields cover attachments,
/// previews and posters. Profile pictures/banners, group pictures and emoji
/// are the explicitly supported non-message references. Free text grants nothing.
pub fn reference_hashes(event: &Event, base: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for tag in event.tags.iter() {
        let parts = tag.as_slice();
        match parts.first().map(String::as_str) {
            Some("imeta") => {
                for part in &parts[1..] {
                    if let Some(("url" | "image", value)) = part.split_once(' ') {
                        out.extend(local_hash(value, base));
                    }
                }
            }
            Some("picture") if event.kind.as_u16() == 39000 => {
                if let Some(value) = parts.get(1) {
                    out.extend(local_hash(value, base));
                }
            }
            Some("emoji") if matches!(event.kind.as_u16(), 10030 | 30030) => {
                if let Some(value) = parts.get(2) {
                    out.extend(local_hash(value, base));
                }
            }
            _ => {}
        }
    }
    if event.kind.as_u16() == 0 {
        if let Ok(profile) = serde_json::from_str::<serde_json::Value>(&event.content) {
            for field in ["picture", "banner"] {
                if let Some(value) = profile.get(field).and_then(|v| v.as_str()) {
                    out.extend(local_hash(value, base));
                }
            }
        }
    }
    out
}

/// All local URLs in structured command payloads as well as counted event
/// references must be authorized before dispatch. This also covers metadata
/// commands whose accepted payload later becomes relay-signed state.
pub fn publication_hashes(event: &Event, base: &str) -> BTreeSet<String> {
    fn visit(value: &serde_json::Value, base: &str, out: &mut BTreeSet<String>) {
        match value {
            serde_json::Value::String(value) => {
                out.extend(local_hash(value, base));
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    visit(value, base, out);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values() {
                    visit(value, base, out);
                }
            }
            _ => {}
        }
    }
    let mut out = reference_hashes(event, base);
    if let Ok(value) = serde_json::from_str(&event.content) {
        visit(&value, base, &mut out);
    }
    for tag in event.tags.iter() {
        for part in tag.as_slice().iter().skip(1) {
            out.extend(local_hash(part, base));
        }
    }
    out
}

/// First valid edit target in signed tag order, accepting all ASCII hex casing.
pub fn first_edit_target(event: &nostr::Event) -> Option<Vec<u8>> {
    event.tags.iter().find_map(|tag| {
        let parts = tag.as_slice();
        let value = parts.get(1)?;
        if parts.first().map(String::as_str) == Some("e")
            && value.len() == 64
            && value.bytes().all(|c| c.is_ascii_hexdigit())
        {
            hex::decode(value).ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind, Tag};
    #[test]
    fn aliases_and_foreign_hosts() {
        let hash = "a".repeat(64);
        for suffix in ["", ".png", ".thumb.jpg", ".thumb.webp"] {
            assert_eq!(
                local_hash(&format!("/media/{hash}{suffix}"), "https://local/media"),
                Some(hash.clone())
            );
        }
        for value in [
            format!("https://foreign/media/{hash}.jpg"),
            format!("/media/{hash}0"),
            format!("/media/{hash}.png?x=1"),
        ] {
            assert_eq!(local_hash(&value, "https://local/media"), None);
        }
    }
    #[test]
    fn profile_and_imeta_share_the_publication_extractor() {
        let hash = "b".repeat(64);
        let event = EventBuilder::new(
            Kind::Metadata,
            format!(r#"{{"picture":"https:\/\/local\/media\/{hash}.jpg"}}"#),
        )
        .tags([Tag::parse(["imeta", &format!("url /media/{hash}.jpg")]).unwrap()])
        .sign_with_keys(&Keys::generate())
        .unwrap();
        assert_eq!(
            reference_hashes(&event, "https://local/media"),
            BTreeSet::from([hash])
        );
        assert_eq!(
            publication_hashes(&event, "https://local/media"),
            reference_hashes(&event, "https://local/media")
        );
    }
}
