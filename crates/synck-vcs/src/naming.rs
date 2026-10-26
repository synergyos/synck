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
    /// Contains a disallowed invisible / format character.
    ContainsInvisible(char),
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
            NameErrorKind::ContainsInvisible(ch) => {
                write!(f, "name contains invisible character U+{:04X}", *ch as u32)
            }
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
/// - no selected zero-width / format characters;
/// - no '/', '\\', '<', '>', ':', '\"', '|', '?', '*';
/// - no trailing ASCII space or dot;
/// - no Windows reserved device names (case-insensitive) as whole segment.
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
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(NameError {
            kind: NameErrorKind::Empty,
        });
    }

    if trimmed == "." || trimmed == ".." {
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
            '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}' | '\u{FEFF}' => {
                return Err(NameError {
                    kind: NameErrorKind::ContainsInvisible(ch),
                });
            }
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

    // Use Unicode-aware, locale-independent lowercasing on the NFC form
    // as an approximation of default case-folding.
    Ok(nfc.to_lowercase())
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

    Ok(nfc.to_lowercase())
}

/// Legacy helper for backfilling existing rows.
///
/// This deliberately skips validation and length checks and just applies
/// NFC + default case-folding in a deterministic way.
pub fn compute_name_key_legacy(name: &str) -> String {
    let nfc: String = name.nfc().collect();
    nfc.to_lowercase()
}

fn is_reserved_device_name(upper: &str) -> bool {
    // Device names are ASCII; a four-byte Unicode name need not have a
    // character boundary after its third byte.
    matches!(
        upper.as_bytes(),
        b"CON"
            | b"PRN"
            | b"AUX"
            | b"NUL"
            | [b'C', b'O', b'M', b'1'..=b'9']
            | [b'L', b'P', b'T', b'1'..=b'9']
    )
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
        let decomposed = compute_name_key("e\u{0301}").unwrap();
        assert_eq!(composed, decomposed);
    }

    #[test]
    fn rejects_control_and_invisible_chars() {
        assert!(matches!(
            validate_display_name("bad\u{0001}"),
            Err(NameError {
                kind: NameErrorKind::ContainsControl(_)
            })
        ));

        assert!(matches!(
            validate_display_name("bad\u{200B}"),
            Err(NameError {
                kind: NameErrorKind::ContainsInvisible(_)
            })
        ));
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
