//! The text of an Office document, for `synck diff`.
//!
//! A docx, xlsx or pptx file is a ZIP of XML parts, so one changed word
//! changes compressed bytes that no reader can compare. This reduces a
//! document to lines: Word paragraphs; PowerPoint paragraphs under a line per
//! slide; Excel cells as `A1: value` (with the formula, if any) under a line
//! per sheet. Formatting, images, comments and notes are left out, so a change
//! only there has no text difference.
//!
//! Every part read is checked against the CRC and size its ZIP directory
//! records. A document that cannot be read this way has no text, and `diff`
//! reports it as a binary file.

use rawzip::{CompressionMethod, ZipArchive};

use crate::ooxml::OoxmlKind;

/// Most inflated XML read from one document.
const MAX_XML: usize = 64 * 1024 * 1024;

pub(crate) fn document_text(kind: OoxmlKind, bytes: &[u8]) -> Option<String> {
    let parts = parts(bytes, kind)?;
    let mut out = String::new();
    match kind {
        OoxmlKind::Docx => {
            let (_, xml) = parts.first()?;
            paragraphs(xml, "", &mut out);
        }
        OoxmlKind::Pptx => {
            for (number, xml) in &parts {
                out.push_str(&format!("Slide {number}\n"));
                paragraphs(xml, "  ", &mut out);
            }
        }
        OoxmlKind::Xlsx => {
            let strings = match parts.first() {
                Some((0, xml)) => shared_strings(xml),
                _ => Vec::new(),
            };
            for (number, xml) in parts.iter().filter(|(number, _)| *number > 0) {
                out.push_str(&format!("Sheet {number}\n"));
                cells(xml, &strings, &mut out)?;
            }
        }
    }
    Some(out)
}

/// The parts holding the document's text, sorted by number: slides and
/// sheets by their number, the Word body and the Excel shared strings as 0.
fn parts(bytes: &[u8], kind: OoxmlKind) -> Option<Vec<(u32, String)>> {
    let archive = ZipArchive::from_slice(bytes).ok()?;
    let mut parts = Vec::new();
    let mut budget = MAX_XML;
    for entry in archive.entries() {
        let entry = entry.ok()?;
        let Ok(name) = std::str::from_utf8(entry.file_path().as_bytes()) else {
            continue;
        };
        let number = match kind {
            OoxmlKind::Docx => (name == "word/document.xml").then_some(0),
            OoxmlKind::Pptx => numbered(name, "ppt/slides/slide"),
            OoxmlKind::Xlsx if name == "xl/sharedStrings.xml" => Some(0),
            OoxmlKind::Xlsx => numbered(name, "xl/worksheets/sheet"),
        };
        let Some(number) = number else {
            continue;
        };
        let data = archive.get_entry(entry.wayfinder()).ok()?;
        let expected = data.claim_verifier();
        let size = usize::try_from(expected.uncompressed_size)
            .ok()
            .filter(|size| *size <= budget)?;
        let method = entry.compression_method();
        let xml = if method == CompressionMethod::STORE {
            data.data().to_vec()
        } else if method == CompressionMethod::DEFLATE {
            miniz_oxide::inflate::decompress_to_vec_with_limit(data.data(), size).ok()?
        } else {
            return None;
        };
        if xml.len() != size || rawzip::crc32(&xml) != expected.crc {
            return None;
        }
        budget -= size;
        parts.push((number, String::from_utf8(xml).ok()?));
    }
    parts.sort_by_key(|(number, _)| *number);
    if parts.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return None;
    }
    Some(parts)
}

/// `prefix<N>.xml` with N > 0.
fn numbered(name: &str, prefix: &str) -> Option<u32> {
    let digits = name.strip_prefix(prefix)?.strip_suffix(".xml")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|number| *number > 0)
}

/// Word and DrawingML paragraphs (`p`), one line each; text comes from `t`
/// elements. Deleted text (`delText`) is not `t`, so a tracked deletion is
/// left out. The fallback copy of alternate content is skipped.
fn paragraphs(xml: &str, indent: &str, out: &mut String) {
    let mut open: Vec<String> = Vec::new();
    let mut runs = 0usize;
    let mut text = false;
    let mut fallback = 0usize;
    walk(xml, |node| match node {
        Node::Open("Fallback", _) => fallback += 1,
        Node::Close("Fallback") => fallback = fallback.saturating_sub(1),
        _ if fallback > 0 => {}
        Node::Open("p", _) => open.push(String::new()),
        Node::Close("p") => {
            if let Some(line) = open.pop() {
                push_line(out, indent, &line);
            }
        }
        Node::Open("r", _) => runs += 1,
        Node::Close("r") => runs = runs.saturating_sub(1),
        Node::Open("t", _) => text = true,
        Node::Close("t") => text = false,
        Node::Empty("tab") if runs > 0 => {
            if let Some(line) = open.last_mut() {
                line.push('\t');
            }
        }
        Node::Empty("br" | "cr") => {
            if let Some(line) = open.last_mut() {
                line.push('\n');
            }
        }
        Node::Text(raw) if text => {
            if let Some(line) = open.last_mut() {
                unescape(raw, line);
            }
        }
        _ => {}
    });
}

fn push_line(out: &mut String, indent: &str, line: &str) {
    if line.trim().is_empty() {
        return;
    }
    for part in line.split('\n') {
        out.push_str(indent);
        out.push_str(part);
        out.push('\n');
    }
}

/// The shared string table: one string per `si`, without phonetic runs.
fn shared_strings(xml: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut current: Option<String> = None;
    let mut text = false;
    let mut phonetic = 0usize;
    walk(xml, |node| match node {
        Node::Open("si", _) => current = Some(String::new()),
        Node::Close("si") => strings.extend(current.take()),
        Node::Empty("si") => strings.push(String::new()),
        Node::Open("rPh", _) => phonetic += 1,
        Node::Close("rPh") => phonetic = phonetic.saturating_sub(1),
        Node::Open("t", _) => text = true,
        Node::Close("t") => text = false,
        Node::Text(raw) if text && phonetic == 0 => {
            if let Some(current) = current.as_mut() {
                unescape(raw, current);
            }
        }
        _ => {}
    });
    strings
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Capture {
    Nothing,
    Value,
    Formula,
    Inline,
}

/// The cells of one worksheet with a value or a formula, one line each.
fn cells(xml: &str, strings: &[String], out: &mut String) -> Option<()> {
    let mut reference = String::new();
    let mut kind = String::new();
    let (mut value, mut formula, mut inline) = (String::new(), String::new(), String::new());
    let mut capture = Capture::Nothing;
    let mut phonetic = 0usize;
    let mut broken = false;
    walk(xml, |node| match node {
        Node::Open("c", attributes) => {
            reference = attribute(attributes, "r").unwrap_or("?").to_string();
            kind = attribute(attributes, "t").unwrap_or("n").to_string();
            value.clear();
            formula.clear();
            inline.clear();
        }
        Node::Close("c") => {
            let shown = match kind.as_str() {
                "s" if !value.is_empty() => {
                    match value
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|index| strings.get(index))
                    {
                        Some(string) => string.clone(),
                        None => {
                            broken = true;
                            String::new()
                        }
                    }
                }
                "inlineStr" => inline.clone(),
                "b" => match value.trim() {
                    "1" => "TRUE".to_string(),
                    "0" => "FALSE".to_string(),
                    other => other.to_string(),
                },
                _ => value.clone(),
            };
            if shown.is_empty() && formula.is_empty() {
                return;
            }
            out.push_str(&format!("  {reference}: {shown}"));
            if !formula.is_empty() {
                out.push_str(&format!(" (={formula})"));
            }
            out.push('\n');
        }
        Node::Open("v", _) => capture = Capture::Value,
        Node::Open("f", _) => capture = Capture::Formula,
        Node::Open("t", _) => capture = Capture::Inline,
        Node::Close("v" | "f" | "t") => capture = Capture::Nothing,
        Node::Open("rPh", _) => phonetic += 1,
        Node::Close("rPh") => phonetic = phonetic.saturating_sub(1),
        Node::Text(raw) => match capture {
            Capture::Nothing => {}
            Capture::Value => unescape(raw, &mut value),
            Capture::Formula => unescape(raw, &mut formula),
            Capture::Inline if phonetic == 0 => unescape(raw, &mut inline),
            Capture::Inline => {}
        },
        _ => {}
    });
    (!broken).then_some(())
}

/// A piece of XML: an element by its local name (no namespace prefix), with
/// the raw attributes of an opening tag, or raw character data.
enum Node<'a> {
    Open(&'a str, &'a str),
    Close(&'a str),
    Empty(&'a str),
    Text(&'a str),
}

/// Visit the elements and character data of `xml` in order. Declarations,
/// processing instructions, comments and CDATA are skipped; Office does not
/// put document text in them. Malformed input ends the walk early.
fn walk<'a>(xml: &'a str, mut visit: impl FnMut(Node<'a>)) {
    let mut rest = xml;
    while let Some(start) = rest.find('<') {
        if start > 0 {
            visit(Node::Text(&rest[..start]));
        }
        rest = &rest[start..];
        let terminator = if rest.starts_with("<!--") {
            "-->"
        } else if rest.starts_with("<![CDATA[") {
            "]]>"
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            ">"
        } else {
            let Some(end) = tag_end(rest) else {
                return;
            };
            let tag = &rest[1..end];
            rest = &rest[end + 1..];
            if let Some(name) = tag.strip_prefix('/') {
                visit(Node::Close(local(name.trim_end())));
            } else if let Some(tag) = tag.strip_suffix('/') {
                visit(Node::Empty(split_tag(tag).0));
            } else {
                let (name, attributes) = split_tag(tag);
                visit(Node::Open(name, attributes));
            }
            continue;
        };
        let Some(end) = rest.find(terminator) else {
            return;
        };
        rest = &rest[end + terminator.len()..];
    }
    if !rest.is_empty() {
        visit(Node::Text(rest));
    }
}

/// The index of the `>` closing the tag at the start of `rest`, outside any
/// quoted attribute value.
fn tag_end(rest: &str) -> Option<usize> {
    let mut quote = None;
    for (index, byte) in rest.bytes().enumerate() {
        match (quote, byte) {
            (None, b'"' | b'\'') => quote = Some(byte),
            (Some(open), _) if open == byte => quote = None,
            (None, b'>') => return Some(index),
            _ => {}
        }
    }
    None
}

fn split_tag(tag: &str) -> (&str, &str) {
    let end = tag
        .find(|c: char| c.is_ascii_whitespace())
        .unwrap_or(tag.len());
    (local(&tag[..end]), &tag[end..])
}

fn local(name: &str) -> &str {
    name.rsplit_once(':').map_or(name, |(_, local)| local)
}

/// The raw value of attribute `name` (unprefixed) in `attributes`.
fn attribute<'a>(attributes: &'a str, name: &str) -> Option<&'a str> {
    let mut rest = attributes;
    loop {
        rest = rest.trim_start();
        let equals = rest.find('=')?;
        let key = rest[..equals].trim_end();
        let after = rest[equals + 1..].trim_start();
        let quote = after.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let close = after[1..].find(quote)?;
        if key == name {
            return Some(&after[1..1 + close]);
        }
        rest = &after[close + 2..];
    }
}

/// Append `raw` to `out` with the XML predefined and numeric character
/// references decoded. An unknown reference is kept as written.
fn unescape(raw: &str, out: &mut String) {
    let mut rest = raw;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let decoded = rest.find(';').and_then(|end| {
            let character = match &rest[1..end] {
                "lt" => Some('<'),
                "gt" => Some('>'),
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                entity => entity
                    .strip_prefix("#x")
                    .map(|hex| u32::from_str_radix(hex, 16))
                    .or_else(|| entity.strip_prefix('#').map(str::parse))
                    .and_then(Result::ok)
                    .and_then(char::from_u32),
            }?;
            Some((character, end))
        });
        match decoded {
            Some((character, end)) => {
                out.push(character);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_paragraphs_keep_text_and_skip_deletions_and_fallbacks() {
        let mut out = String::new();
        paragraphs(
            r#"<?xml version="1.0"?><w:document><w:body>
            <w:p><w:pPr><w:tabs><w:tab w:val="left"/></w:tabs></w:pPr>
              <w:r><w:t xml:space="preserve">Fees &amp; costs</w:t></w:r><w:r><w:tab/><w:t>due</w:t></w:r>
              <w:del><w:r><w:delText>gone</w:delText></w:r></w:del></w:p>
            <w:p/><w:p><w:r><w:t></w:t></w:r></w:p>
            <w:p><mc:AlternateContent><mc:Choice><w:t>box</w:t></mc:Choice><mc:Fallback><w:t>box</w:t></mc:Fallback></mc:AlternateContent>
              <w:r><w:t>a&#233;&#x41;&bogus;</w:t><w:br/><w:t>b</w:t></w:r></w:p>
            </w:body></w:document>"#,
            "",
            &mut out,
        );
        assert_eq!(out, "Fees & costs\tdue\nboxaéA&bogus;\nb\n");
    }

    #[test]
    fn worksheet_cells_resolve_shared_strings_and_show_formulas() {
        let strings = shared_strings(
            r#"<sst><si><t>Rent</t></si><si><r><t>Ki</t></r><r><t>t</t></r><rPh><t>x</t></rPh></si><si/></sst>"#,
        );
        assert_eq!(strings, ["Rent", "Kit", ""]);
        let mut out = String::new();
        cells(
            r#"<worksheet><sheetData><row r="1">
              <c r="A1" t="s"><v>0</v></c><c r="B1"><v>1200.5</v></c><c r="C1" s="2"/>
              <c r="D1"><f>SUM(B1:B3)</f><v>3600</v></c><c r="E1" t="b"><v>1</v></c>
              <c r="F1" t="inlineStr"><is><t>a &lt; b</t></is></c><c r="G1" t="s"><v>1</v></c>
            </row></sheetData></worksheet>"#,
            &strings,
            &mut out,
        )
        .unwrap();
        assert_eq!(
            out,
            "  A1: Rent\n  B1: 1200.5\n  D1: 3600 (=SUM(B1:B3))\n  E1: TRUE\n  F1: a < b\n  G1: Kit\n"
        );
        assert!(
            cells(
                r#"<c r="A1" t="s"><v>7</v></c>"#,
                &strings,
                &mut String::new()
            )
            .is_none()
        );
    }

    #[test]
    fn the_blank_templates_are_readable() {
        let templates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/templates");
        for (name, kind, text) in [
            ("empty.docx", OoxmlKind::Docx, ""),
            ("empty.pptx", OoxmlKind::Pptx, "Slide 1\n"),
            ("empty.xlsx", OoxmlKind::Xlsx, "Sheet 1\n"),
        ] {
            let bytes = std::fs::read(templates.join(name)).unwrap();
            assert_eq!(document_text(kind, &bytes).as_deref(), Some(text), "{name}");
        }
    }

    #[test]
    fn attributes_are_read_whole_and_quoted() {
        assert_eq!(attribute(r#" s="1" r='B2' t="s""#, "r"), Some("B2"));
        assert_eq!(attribute(r#" rr="X" r="A1""#, "r"), Some("A1"));
        assert_eq!(attribute(r#" s="1""#, "t"), None);
        assert_eq!(
            numbered("ppt/slides/slide12.xml", "ppt/slides/slide"),
            Some(12)
        );
        assert_eq!(numbered("ppt/slides/slide.xml", "ppt/slides/slide"), None);
        assert_eq!(
            numbered("ppt/slides/slide1.xml.rels", "ppt/slides/slide"),
            None
        );
    }
}
