/// Recover the original HTML encapsulated in an Outlook RTF body.
///
/// MS-OXRTFEX writes HTML fragments in numbered `htmltagN` destinations and
/// surrounds RTF-only presentation text with `htmlrtf`/`htmlrtf0`. The
/// `msg_parser` 0.3.6 fallback only recognizes the non-standard
/// `htmltag N` spelling, so real Outlook bodies leak one destination marker
/// (`*`) per fragment into the resulting HTML.
pub(super) fn extract_encapsulated_html(rtf: &[u8]) -> Option<String> {
    let rtf = std::str::from_utf8(rtf).ok()?;
    if !rtf.contains("\\fromhtml1") {
        return None;
    }

    let bytes = rtf.as_bytes();
    let mut html = String::with_capacity(bytes.len());
    let mut index = 0;
    let mut in_rtf_only_region = false;

    while index < bytes.len() {
        if starts_with(bytes, index, b"\\htmlrtf") {
            index += b"\\htmlrtf".len();
            if bytes.get(index) == Some(&b'0') {
                in_rtf_only_region = false;
                index += 1;
            } else {
                in_rtf_only_region = true;
            }
            skip_control_delimiter(bytes, &mut index);
            continue;
        }

        if starts_with(bytes, index, b"{\\*\\htmltag") {
            index += b"{\\*\\htmltag".len();
            skip_signed_number(bytes, &mut index);
            skip_control_delimiter(bytes, &mut index);
            collect_htmltag_group(bytes, &mut index, &mut html);
            continue;
        }

        if is_non_html_destination_group(bytes, index) {
            skip_group(bytes, &mut index);
            continue;
        }

        if in_rtf_only_region {
            index += 1;
            continue;
        }

        match bytes[index] {
            b'{' | b'}' | b'\r' | b'\n' => index += 1,
            b'\\' => collect_escape_or_skip_control(bytes, &mut index, &mut html),
            byte => {
                html.push(byte as char);
                index += 1;
            }
        }
    }

    (!html.is_empty()).then_some(html)
}

fn is_non_html_destination_group(bytes: &[u8], index: usize) -> bool {
    let Some((ignorable, destination)) = group_destination(bytes, index) else {
        return false;
    };
    if ignorable {
        return true;
    }
    const NON_HTML_DESTINATIONS: &[&[u8]] = &[
        b"colortbl",
        b"datastore",
        b"filetbl",
        b"fonttbl",
        b"footer",
        b"footerf",
        b"footerl",
        b"footerr",
        b"header",
        b"headerf",
        b"headerl",
        b"headerr",
        b"info",
        b"listoverridetable",
        b"listtable",
        b"object",
        b"pict",
        b"revtbl",
        b"rsidtbl",
        b"stylesheet",
        b"userprops",
    ];
    NON_HTML_DESTINATIONS.contains(&destination)
}

fn group_destination(bytes: &[u8], index: usize) -> Option<(bool, &[u8])> {
    if bytes.get(index) != Some(&b'{') || bytes.get(index + 1) != Some(&b'\\') {
        return None;
    }
    let mut cursor = index + 2;
    let ignorable = bytes.get(cursor) == Some(&b'*');
    if ignorable {
        cursor += 1;
        if bytes.get(cursor) != Some(&b'\\') {
            return None;
        }
        cursor += 1;
    }
    let start = cursor;
    while bytes.get(cursor).is_some_and(u8::is_ascii_alphabetic) {
        cursor += 1;
    }
    (cursor > start).then_some((ignorable, &bytes[start..cursor]))
}

fn skip_group(bytes: &[u8], index: &mut usize) {
    let mut depth = 0usize;
    while *index < bytes.len() {
        match bytes[*index] {
            b'{' => {
                depth += 1;
                *index += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                *index += 1;
                if depth == 0 {
                    return;
                }
            }
            b'\\' if matches!(bytes.get(*index + 1), Some(b'\\' | b'{' | b'}')) => {
                *index += 2;
            }
            b'\\' if bytes.get(*index + 1) == Some(&b'\'') => {
                *index = (*index + 4).min(bytes.len());
            }
            _ => *index += 1,
        }
    }
}

fn collect_htmltag_group(bytes: &[u8], index: &mut usize, html: &mut String) {
    let mut depth = 1usize;
    while *index < bytes.len() && depth > 0 {
        match bytes[*index] {
            b'{' => {
                depth += 1;
                *index += 1;
            }
            b'}' => {
                depth -= 1;
                *index += 1;
            }
            b'\r' | b'\n' => *index += 1,
            b'\\' => collect_escape_or_skip_control(bytes, index, html),
            byte => {
                html.push(byte as char);
                *index += 1;
            }
        }
    }
}

fn collect_escape_or_skip_control(bytes: &[u8], index: &mut usize, html: &mut String) {
    *index += 1;
    let Some(&next) = bytes.get(*index) else {
        return;
    };
    match next {
        b'\\' => {
            html.push('\\');
            *index += 1;
        }
        b'{' => {
            html.push('{');
            *index += 1;
        }
        b'}' => {
            html.push('}');
            *index += 1;
        }
        b'\'' if *index + 2 < bytes.len() => {
            if let Ok(hex) = std::str::from_utf8(&bytes[*index + 1..*index + 3])
                && let Ok(byte) = u8::from_str_radix(hex, 16) {
                    html.push(byte as char);
                }
            *index += 3;
        }
        b'\r' | b'\n' | b'*' => *index += 1,
        _ => {
            while bytes.get(*index).is_some_and(u8::is_ascii_alphabetic) {
                *index += 1;
            }
            skip_signed_number(bytes, index);
            skip_control_delimiter(bytes, index);
        }
    }
}

fn skip_signed_number(bytes: &[u8], index: &mut usize) {
    if bytes.get(*index) == Some(&b'-') {
        *index += 1;
    }
    while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
        *index += 1;
    }
}

fn skip_control_delimiter(bytes: &[u8], index: &mut usize) {
    if bytes.get(*index) == Some(&b' ') {
        *index += 1;
    }
}

fn starts_with(bytes: &[u8], index: usize, needle: &[u8]) -> bool {
    bytes
        .get(index..index.saturating_add(needle.len()))
        .is_some_and(|candidate| candidate == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_numbered_htmltag_destinations_without_marker_stars() {
        let rtf = br#"{\rtf1\ansi\fromhtml1
{\*\htmltag64 <html>}
{\*\htmltag64 <head>}
{\*\htmltag64 <style>}
{\*\htmltag64 /* keep literal stars */ p \{ color: red; \}}
{\*\htmltag64 </style>}
{\*\htmltag64 </head>}
{\*\htmltag64 <body>*Digest*</body>}
{\*\htmltag64 </html>}
}"#;

        assert_eq!(
            extract_encapsulated_html(rtf).as_deref(),
            Some(
                "<html><head><style>/* keep literal stars */ p { color: red; }</style></head><body>*Digest*</body></html>"
            )
        );
    }

    #[test]
    fn excludes_rtf_only_presentation_text() {
        let rtf = br#"{\rtf1\ansi\fromhtml1
{\*\htmltag64 <p>}before \htmlrtf RTF only\htmlrtf0 after{\*\htmltag64 </p>}
}"#;

        assert_eq!(
            extract_encapsulated_html(rtf).as_deref(),
            Some("<p>before after</p>")
        );
    }

    #[test]
    fn excludes_non_html_destination_groups() {
        let rtf = br#"{\rtf1\ansi\fromhtml1
{\fonttbl{\f0 Arial;}{\f1 Courier New;}{\f2 Symbol;}}
{\colortbl;\red0\green0\blue0;}
{\stylesheet{\s0 Normal;}}
{\*\generator Microsoft Outlook;}
{\*\htmltag64 <p>}Visible body{\*\htmltag64 </p>}
}"#;

        assert_eq!(
            extract_encapsulated_html(rtf).as_deref(),
            Some("<p>Visible body</p>")
        );
    }

    #[test]
    fn rejects_plain_rtf() {
        assert!(extract_encapsulated_html(br#"{\rtf1\ansi plain}"#).is_none());
    }
}
