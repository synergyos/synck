//! Item metadata interpreted by the shared event projection.

pub const ITEM_SUBTYPE_STANDARD: &str = "STANDARD";

/// How a history's item metadata payloads relate to the item's metadata
/// (`docs/spec/format.md`, Metadata payloads).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MetadataMaps {
    /// Patches, as an unencrypted authority's writers may emit: only CREATE
    /// and SET_METADATA change the item's metadata.
    #[default]
    Patch,
    /// Complete maps, as every encrypted repository's writers emit: every
    /// two-segment event with metadata carries the item's.
    Complete,
}
/// Item metadata flag excluding a VCS-backed item from generic tree/navigation surfaces.
///
/// The item remains addressable through its typed API. Human direct chats use this because their
/// folder parent is an internal storage/authz detail, not user-visible hierarchy.
pub const ITEM_TREE_HIDDEN_METADATA_KEY: &str = "tree_hidden";
pub const ITEM_COLOR_METADATA_VALUES: &[&str] =
    &["yellow", "orange", "red", "purple", "blue", "green"];

pub fn normalize_item_color_metadata(raw: Option<&str>) -> Option<String> {
    let value = raw?.trim().to_ascii_lowercase();
    ITEM_COLOR_METADATA_VALUES
        .contains(&value.as_str())
        .then_some(value)
}

pub fn item_color_metadata_from_json(meta: &serde_json::Value) -> Option<String> {
    normalize_item_color_metadata(meta.get("color_metadata").and_then(|v| v.as_str()))
}
