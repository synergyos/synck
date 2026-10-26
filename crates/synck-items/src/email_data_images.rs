use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use base64::Engine as _;
use lol_html::{RewriteStrSettings, element, rewrite_str, text};
use sha2::{Digest, Sha256};

const DATA_IMAGE_ATTRIBUTES: &[&str] = &["src", "srcset", "background", "style", "href"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedDataImage {
    pub content_id: String,
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedEmailHtml {
    pub html: String,
    pub images: Vec<ExtractedDataImage>,
}

#[derive(Debug, thiserror::Error)]
pub enum EmailDataImageError {
    #[error("embedded image HTML rewrite failed: {0}")]
    Rewrite(String),
}

impl EmailDataImageError {
    fn is_iframe_ambiguity(&self) -> bool {
        matches!(
            self,
            Self::Rewrite(message) if message.contains("text content tag (`<iframe>`)")
        )
    }
}

#[derive(Debug, Default)]
struct ExtractionState {
    reserved_content_ids: BTreeSet<String>,
    by_digest: BTreeMap<String, ExtractedDataImage>,
    style_text: String,
    changed: bool,
}

pub fn extract_data_images(
    html: &str,
    reserved_content_ids: impl IntoIterator<Item = String>,
) -> Result<NormalizedEmailHtml, EmailDataImageError> {
    let reserved_content_ids = reserved_content_ids
        .into_iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    match extract_data_images_once(html, &reserved_content_ids) {
        Ok(normalized) => Ok(normalized),
        Err(error) => {
            if !error.is_iframe_ambiguity() {
                return Err(error);
            }
            let Some(neutralized) = neutralize_iframe_tags(html) else {
                return Err(error);
            };
            extract_data_images_once(neutralized.as_str(), &reserved_content_ids)
        }
    }
}

fn extract_data_images_once(
    html: &str,
    reserved_content_ids: &BTreeSet<String>,
) -> Result<NormalizedEmailHtml, EmailDataImageError> {
    let state = Arc::new(Mutex::new(ExtractionState {
        reserved_content_ids: reserved_content_ids.clone(),
        ..ExtractionState::default()
    }));
    let handler_state = Arc::clone(&state);
    let style_state = Arc::clone(&state);
    let rewritten = rewrite_str(
        html,
        RewriteStrSettings::new()
            .append_element_content_handler(element!(
                "*[src], *[srcset], *[background], *[style], *[href]",
                move |element| {
                    let mut state = handler_state.lock().expect("data image state poisoned");
                    for name in DATA_IMAGE_ATTRIBUTES {
                        let Some(value) = element.get_attribute(name) else {
                            continue;
                        };
                        if let Some(rewritten) =
                            rewrite_attribute_data_images(value.as_str(), &mut state)
                        {
                            state.changed = true;
                            element.set_attribute(name, rewritten.as_str())?;
                        }
                    }
                    Ok(())
                }
            ))
            .append_element_content_handler(text!("style", move |text| {
                let mut state = style_state.lock().expect("data image state poisoned");
                state.style_text.push_str(text.as_str());
                text.set_str(String::new());
                if !text.last_in_text_node() {
                    return Ok(());
                }
                let style = std::mem::take(&mut state.style_text);
                if let Some(rewritten) = rewrite_attribute_data_images(style.as_str(), &mut state) {
                    state.changed = true;
                    text.set_str(rewritten);
                } else {
                    text.set_str(style);
                }
                Ok(())
            })),
    )
    .map_err(|error| EmailDataImageError::Rewrite(error.to_string()))?;
    let state = Arc::try_unwrap(state)
        .expect("data image rewrite retained state")
        .into_inner()
        .expect("data image state poisoned");
    let changed = state.changed;
    let images: Vec<_> = state.by_digest.into_values().collect();
    Ok(NormalizedEmailHtml {
        html: if changed { rewritten } else { html.to_string() },
        images,
    })
}

fn neutralize_iframe_tags(html: &str) -> Option<String> {
    let bytes = html.as_bytes();
    let mut output = String::with_capacity(html.len());
    let mut cursor = 0usize;
    let mut changed = false;
    while let Some(relative) = find_ascii_case_insensitive(&html[cursor..], "iframe") {
        let start = cursor + relative;
        let before = &bytes[..start];
        let tag_name = before.ends_with(b"<") || before.ends_with(b"</");
        let after = bytes.get(start + "iframe".len()).copied();
        let tag_boundary =
            after.is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'));
        if !tag_name || !tag_boundary {
            output.push_str(&html[cursor..start + "iframe".len()]);
            cursor = start + "iframe".len();
            continue;
        }
        output.push_str(&html[cursor..start]);
        output.push_str("div");
        cursor = start + "iframe".len();
        changed = true;
    }
    if !changed {
        return None;
    }
    output.push_str(&html[cursor..]);
    Some(output)
}

fn rewrite_attribute_data_images(value: &str, state: &mut ExtractionState) -> Option<String> {
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0usize;
    let mut changed = false;
    while let Some(relative_start) = find_ascii_case_insensitive(&value[cursor..], "data:image/") {
        let start = cursor + relative_start;
        output.push_str(&value[cursor..start]);
        let input = &value[start..];
        let reference_len = data_image_reference_len(input);
        let reference = &input[..reference_len];
        let Some((end, mime_type, extension, bytes)) = parse_data_image(reference) else {
            cursor = start + reference_len;
            changed = true;
            continue;
        };
        let digest = hex_lower(&Sha256::digest(bytes.as_slice()));
        let image_key = format!("{mime_type}:{digest}");
        let content_id = match state.by_digest.get(image_key.as_str()) {
            Some(existing) => existing.content_id.clone(),
            None => allocate_content_id(digest.as_str(), &mut state.reserved_content_ids),
        };
        state
            .by_digest
            .entry(image_key)
            .or_insert_with(|| ExtractedDataImage {
                content_id: content_id.clone(),
                filename: format!("inline-{digest}.{extension}"),
                mime_type,
                bytes,
            });
        output.push_str("cid:");
        output.push_str(content_id.as_str());
        cursor = start + end;
        changed = true;
    }
    if !changed {
        return None;
    }
    output.push_str(&value[cursor..]);
    Some(output)
}

fn data_image_reference_len(input: &str) -> usize {
    let candidate_len = input
        .bytes()
        .position(|byte| {
            byte.is_ascii_whitespace() || matches!(byte, b')' | b'\'' | b'"' | b'<' | b'>')
        })
        .unwrap_or(input.len());
    let candidate = &input[..candidate_len];
    let Some(comma) = candidate.find(',') else {
        return candidate_len;
    };
    let payload_len = candidate[comma + 1..]
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        .count();
    comma + 1 + payload_len
}

fn parse_data_image(input: &str) -> Option<(usize, String, &'static str, Vec<u8>)> {
    let comma = input.find(',')?;
    let header = &input[..comma];
    let (mime_type, extension) = parse_data_image_header(header)?;
    let payload = &input[comma + 1..];
    let encoded_len = payload
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        .count();
    if encoded_len == 0 {
        return None;
    }
    let encoded = &payload[..encoded_len];
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded))
        .ok()?;
    if bytes.is_empty() || !image_signature_matches(mime_type.as_str(), bytes.as_slice()) {
        return None;
    }
    Some((comma + 1 + encoded_len, mime_type, extension, bytes))
}

fn parse_data_image_header(header: &str) -> Option<(String, &'static str)> {
    let lower = header.to_ascii_lowercase();
    let value = lower.strip_prefix("data:")?;
    let mut parts = value.split(';');
    let mime_type = parts.next().unwrap_or_default();
    let parameters: Vec<_> = parts.collect();
    if parameters.last().copied() != Some("base64")
        || parameters[..parameters.len().saturating_sub(1)]
            .iter()
            .any(|parameter| parameter.is_empty() || !parameter.contains('='))
    {
        return None;
    }
    let (canonical, extension) = match mime_type {
        "image/png" => ("image/png", "png"),
        "image/jpeg" | "image/jpg" => ("image/jpeg", "jpg"),
        "image/gif" => ("image/gif", "gif"),
        "image/webp" => ("image/webp", "webp"),
        "image/avif" => ("image/avif", "avif"),
        "image/bmp" => ("image/bmp", "bmp"),
        _ => return None,
    };
    Some((canonical.to_string(), extension))
}

fn image_signature_matches(mime_type: &str, bytes: &[u8]) -> bool {
    match mime_type {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP",
        "image/avif" => {
            bytes.len() >= 12
                && &bytes[4..8] == b"ftyp"
                && matches!(&bytes[8..12], b"avif" | b"avis")
        }
        "image/bmp" => bytes.starts_with(b"BM"),
        _ => false,
    }
}

fn allocate_content_id(digest: &str, reserved: &mut BTreeSet<String>) -> String {
    for suffix in 0u64.. {
        let candidate = if suffix == 0 {
            format!("synckhub-data-{digest}@inline")
        } else {
            format!("synckhub-data-{digest}-{suffix}@inline")
        };
        if reserved.insert(candidate.to_ascii_lowercase()) {
            return candidate;
        }
    }
    unreachable!("u64 content-id suffix space exhausted")
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nsmall";

    #[test]
    fn extracts_deduplicates_and_rewrites_raster_data_images() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(PNG);
        let html = format!(
            "<p style=\"background:url(data:image/png;charset=utf-8;base64,{encoded})\"><img src=\"data:image/png;base64,{encoded}\"></p>"
        );
        let normalized = extract_data_images(html.as_str(), Vec::new()).unwrap();
        assert_eq!(normalized.images.len(), 1);
        assert_eq!(normalized.images[0].bytes, PNG);
        assert_eq!(normalized.images[0].mime_type, "image/png");
        assert!(!normalized.html.contains("data:image"));
        assert_eq!(normalized.html.matches("cid:").count(), 2);
    }

    #[test]
    fn rewrites_data_images_in_style_elements() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(PNG);
        let html = format!(
            "<style>.logo {{ background-image: url(data:image/png;base64,{encoded}) }}</style>"
        );
        let normalized = extract_data_images(html.as_str(), Vec::new()).unwrap();
        assert_eq!(normalized.images.len(), 1);
        assert!(!normalized.html.contains("data:image"));
        assert!(normalized.html.contains("url(cid:synckhub-data-"));
    }

    #[test]
    fn keeps_valid_data_image_when_a_sibling_reference_is_malformed() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(PNG);
        let html = format!(
            "<p style=\"background-image:url(data:image/png),url(data:image/png;base64,{encoded}),url(data:image/png;base64,abcde)\">body</p>"
        );
        let normalized = extract_data_images(html.as_str(), Vec::new()).unwrap();
        assert_eq!(normalized.images.len(), 1);
        assert_eq!(normalized.images[0].bytes, PNG);
        assert!(!normalized.html.contains("data:image"));
        assert_eq!(normalized.html.matches("cid:").count(), 1);
    }

    #[test]
    fn leaves_remote_and_cid_images_unchanged() {
        let html = "<img src=\"cid:one\"><img src=\"https://example.test/image.png\">";
        let normalized = extract_data_images(html, Vec::new()).unwrap();
        assert!(normalized.images.is_empty());
        assert_eq!(normalized.html, html);
    }

    #[test]
    fn drops_malformed_mislabeled_and_active_data_images() {
        let fake_png = base64::engine::general_purpose::STANDARD.encode(b"not png");
        for html in [
            format!("<p>before</p><img src=\"data:image/png;base64,{fake_png}\"><p>after</p>"),
            "<p>before</p><img src=\"data:image/svg+xml;base64,PHN2Zz4=\"><p>after</p>".to_string(),
            "<p style=\"background:url(data:image/png;base64,abcde)\">body</p>".to_string(),
            "<img src=\"data:image/png;base64,\">".to_string(),
        ] {
            let normalized = extract_data_images(html.as_str(), Vec::new()).unwrap();
            assert!(normalized.images.is_empty(), "{html}");
            assert!(!normalized.html.contains("data:image"), "{normalized:?}");
        }
    }

    #[test]
    fn retries_malformed_iframe_html_without_retaining_data_images() {
        let encoded = base64::engine::general_purpose::STANDARD.encode(PNG);
        let html = format!(
            "<select><iframe><img src=\"data:image/png;base64,{encoded}\"></iframe></select>"
        );
        assert!(extract_data_images_once(html.as_str(), &BTreeSet::new()).is_err());

        let normalized = extract_data_images(html.as_str(), Vec::new()).unwrap();
        assert_eq!(normalized.images.len(), 1);
        assert!(!normalized.html.contains("data:image"));
        assert!(!normalized.html.to_ascii_lowercase().contains("iframe"));
        assert!(normalized.html.contains("<div>"));
    }
}
