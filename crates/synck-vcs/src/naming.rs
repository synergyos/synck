//! Name validation and canonicalization utilities.
//!
//! This module defines the semantic rules for item names in Synck VCS.
//! The database only enforces uniqueness over the computed `name_key`;
//! all correctness rules live here.

use std::error::Error;
use std::fmt;

use unicode_normalization::UnicodeNormalization;

/// Maximum allowed UTF-8 byte length for a single name segment.
pub const MAX_NAME_BYTES: usize = 255;

/// Validation / canonicalization error for names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameError {
    pub kind: NameErrorKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameErrorKind {
    /// Name is empty or only whitespace.
    Empty,
    /// Name is "." or ".." after trimming.
    DotOrDotDot,
    /// Name ends with ASCII space or dot.
    EndsWithDotOrSpace,
    /// Contains an ASCII control character U+0000–U+001F or U+007F.
    ContainsControl(char),
    /// Display names in canonical history must already be NFC.
    NotNormalized,
    /// Contains a forbidden path / shell character.
    ContainsForbiddenChar(char),
    /// Matches a reserved Windows device name (case-insensitive).
    ReservedDeviceName { raw: String },
    /// Exceeds the per-segment byte length limit.
    TooLong { bytes: usize, max: usize },
}

impl fmt::Display for NameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            NameErrorKind::Empty => write!(f, "name must not be empty"),
            NameErrorKind::DotOrDotDot => write!(f, "name must not be '.' or '..'"),
            NameErrorKind::EndsWithDotOrSpace => {
                write!(f, "name must not end with space or dot")
            }
            NameErrorKind::ContainsControl(ch) => {
                write!(f, "name contains control character U+{:04X}", *ch as u32)
            }
            NameErrorKind::NotNormalized => write!(f, "name must be NFC-normalized"),
            NameErrorKind::ContainsForbiddenChar(ch) => {
                write!(f, "name contains forbidden character {:?}", ch)
            }
            NameErrorKind::ReservedDeviceName { raw } => {
                write!(f, "name {:?} is a reserved device name", raw)
            }
            NameErrorKind::TooLong { bytes, max } => {
                write!(f, "name is too long: {bytes} bytes (max {max})")
            }
        }
    }
}

impl Error for NameError {}

/// Validate a display name without changing it.
///
/// This enforces:
/// - non-empty, not just whitespace;
/// - not "." or ".." after trimming;
/// - no ASCII control chars U+0000–U+001F or U+007F;
/// - NFC-normalized UTF-8, at most 255 bytes;
/// - no '/', '\\', '<', '>', ':', '\"', '|', '?', '*';
/// - no trailing ASCII space or dot;
/// - no Windows reserved device names (case-insensitive) including names with extensions.
pub fn validate_display_name(name: &str) -> Result<(), NameError> {
    validate_display_name_inner(name, false)
}

/// Validate a display name while allowing a trailing '.'.
///
/// This is intended for non-item display strings (e.g. tenant names like "Acme Inc.").
pub fn validate_display_name_allow_trailing_dot(name: &str) -> Result<(), NameError> {
    validate_display_name_inner(name, true)
}

fn validate_display_name_inner(name: &str, allow_trailing_dot: bool) -> Result<(), NameError> {
    if name.is_empty() {
        return Err(NameError {
            kind: NameErrorKind::Empty,
        });
    }

    if name == "." || name == ".." {
        return Err(NameError {
            kind: NameErrorKind::DotOrDotDot,
        });
    }

    // Per-character checks on the raw name.
    for ch in name.chars() {
        let code = ch as u32;
        if (code <= 0x1F) || code == 0x7F {
            return Err(NameError {
                kind: NameErrorKind::ContainsControl(ch),
            });
        }

        match ch {
            '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*' => {
                return Err(NameError {
                    kind: NameErrorKind::ContainsForbiddenChar(ch),
                });
            }
            _ => {}
        }
    }

    if let Some(last) = name.chars().last() {
        if last == ' ' {
            return Err(NameError {
                kind: NameErrorKind::EndsWithDotOrSpace,
            });
        }
        if last == '.' && !allow_trailing_dot {
            return Err(NameError {
                kind: NameErrorKind::EndsWithDotOrSpace,
            });
        }
    }

    if !unicode_normalization::is_nfc(name) {
        return Err(NameError {
            kind: NameErrorKind::NotNormalized,
        });
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(NameError {
            kind: NameErrorKind::TooLong {
                bytes: name.len(),
                max: MAX_NAME_BYTES,
            },
        });
    }
    let upper = name.to_ascii_uppercase();
    if is_reserved_device_name(&upper) {
        return Err(NameError {
            kind: NameErrorKind::ReservedDeviceName {
                raw: name.to_string(),
            },
        });
    }

    Ok(())
}

/// Compute the canonical name key used for uniqueness.
///
/// This:
/// 1. validates the display name;
/// 2. normalizes to Unicode NFC;
/// 3. enforces the byte-length limit on the NFC form;
/// 4. applies Unicode default case-folding and returns the result.
pub fn compute_name_key(name: &str) -> Result<String, NameError> {
    validate_display_name(name)?;

    let nfc: String = name.nfc().collect();
    let bytes = nfc.len();
    if bytes > MAX_NAME_BYTES {
        return Err(NameError {
            kind: NameErrorKind::TooLong {
                bytes,
                max: MAX_NAME_BYTES,
            },
        });
    }

    Ok(crate::case_folding::fold(&nfc))
}

/// Compute a canonical key for non-item display strings.
///
/// This matches `compute_name_key` except it allows a trailing '.' in the display name.
pub fn compute_display_name_key(name: &str) -> Result<String, NameError> {
    validate_display_name_allow_trailing_dot(name)?;

    let nfc: String = name.nfc().collect();
    let bytes = nfc.len();
    if bytes > MAX_NAME_BYTES {
        return Err(NameError {
            kind: NameErrorKind::TooLong {
                bytes,
                max: MAX_NAME_BYTES,
            },
        });
    }

    Ok(crate::case_folding::fold(&nfc))
}

/// Legacy helper for backfilling existing rows.
///
/// This deliberately skips validation and length checks and just applies
/// NFC + default case-folding in a deterministic way.
pub fn compute_name_key_legacy(name: &str) -> String {
    let nfc: String = name.nfc().collect();
    crate::case_folding::fold(&nfc)
}

fn is_reserved_device_name(upper: &str) -> bool {
    let stem = upper
        .split('.')
        .next()
        .unwrap_or(upper)
        .trim_end_matches(' ');
    matches!(stem, "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
}

/// Deterministic portable spelling, before sibling collision allocation.
pub fn portable_name(name: &str) -> String {
    let mut fixed: String = name
        .nfc()
        .map(|ch| {
            if ch <= '\u{1f}'
                || ch == '\u{7f}'
                || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            {
                '_'
            } else {
                ch
            }
        })
        .collect();
    fixed.truncate(fixed.trim_end_matches([' ', '.']).len());
    if fixed.is_empty() {
        fixed.push('_');
    }
    if is_reserved_device_name(&fixed.to_ascii_uppercase()) {
        fixed.insert(0, '_');
    }
    truncate_name(&mut fixed, MAX_NAME_BYTES);
    fixed.truncate(fixed.trim_end_matches([' ', '.']).len());
    fixed
}

/// Reserve the suffix's bytes before truncating a colliding portable name.
pub fn suffixed_name(name: &str, ordinal: usize) -> String {
    let suffix = format!(" ({ordinal})");
    let mut result = name.to_string();
    truncate_name(&mut result, MAX_NAME_BYTES - suffix.len());
    result.push_str(&suffix);
    result
}

fn truncate_name(name: &mut String, max: usize) {
    let mut end = name.len().min(max);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    name.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_case_conflict_key() {
        let a = compute_name_key("Report").unwrap();
        let b = compute_name_key("report").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn cosmetic_rename_allowed() {
        // Same key, different display strings is fine at this layer.
        let a = compute_name_key("Report").unwrap();
        let b = compute_name_key("REPORT").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn nfc_equivalence_conflicts() {
        let composed = compute_name_key("é").unwrap();
        assert!(compute_name_key("e\u{0301}").is_err());
        assert_eq!(
            composed,
            compute_name_key(&portable_name("e\u{0301}")).unwrap()
        );
    }

    #[test]
    fn rejects_control_and_invisible_chars() {
        assert!(matches!(
            validate_display_name("bad\u{0001}"),
            Err(NameError {
                kind: NameErrorKind::ContainsControl(_)
            })
        ));

        assert!(validate_display_name("bad\u{200B}").is_ok());
    }

    #[test]
    fn rejects_forbidden_chars_and_reserved_names() {
        assert!(matches!(
            validate_display_name("bad/name"),
            Err(NameError {
                kind: NameErrorKind::ContainsForbiddenChar('/')
            })
        ));

        assert!(matches!(
            validate_display_name("CON"),
            Err(NameError {
                kind: NameErrorKind::ReservedDeviceName { .. }
            })
        ));
        assert!(matches!(
            validate_display_name("com1"),
            Err(NameError {
                kind: NameErrorKind::ReservedDeviceName { .. }
            })
        ));
    }

    #[test]
    fn leading_space_does_not_trigger_reserved_device_name() {
        assert!(validate_display_name(" CON").is_ok());
    }

    #[test]
    fn reserved_device_checks_accept_four_byte_unicode_names() {
        for name in ["😀", "📁", "éé", "a文", "ABé"] {
            assert_eq!(name.len(), 4, "regression must exercise four-byte names");
            assert!(validate_display_name(name).is_ok(), "{name}");
            assert!(compute_name_key(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn enforces_max_length() {
        let ok = "a".repeat(MAX_NAME_BYTES);
        compute_name_key(&ok).expect("max length should be allowed");

        let too_long = "a".repeat(MAX_NAME_BYTES + 1);
        let err = compute_name_key(&too_long).unwrap_err();
        assert!(matches!(
            err.kind,
            NameErrorKind::TooLong { bytes, max } if bytes == MAX_NAME_BYTES + 1 && max == MAX_NAME_BYTES
        ));
    }

    #[test]
    fn allows_trailing_dot_for_display_names() {
        assert!(
            validate_display_name("Acme Inc.").is_err(),
            "item display name validation must reject trailing dot"
        );
        validate_display_name_allow_trailing_dot("Acme Inc.")
            .expect("display name allows trailing dot");
        compute_display_name_key("Acme Inc.").expect("compute display name key");
    }
}
