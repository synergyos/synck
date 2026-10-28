use super::MutationPolicy;
use synck_file_folder::{FileFolderKind, is_profile_event_type};

pub const ITEM_TYPE: &str = "FILE";

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    let rest = event_type.strip_prefix("FILE::")?;

    if is_profile_event_type(FileFolderKind::File, event_type) {
        return Some(MutationPolicy::Optimistic);
    }
    if !rest.contains("::") {
        return match rest {
            "SET_METADATA" | "REVISION_SUPPRESS" | "REVISION_UNSUPPRESS" | "FEDERATE" => {
                Some(MutationPolicy::Optimistic)
            }
            _ => None,
        };
    }

    match event_type {
        _ => None,
    }
}
