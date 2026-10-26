pub const ITEM_TYPE: &str = "FEDERATED_MOUNT";

use super::MutationPolicy;

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    let rest = event_type.strip_prefix("FEDERATED_MOUNT::")?;
    if rest.contains("::") {
        return None;
    }
    match rest {
        "CREATE" | "SET_NAME" | "SET_PARENT" | "SET_METADATA" | "TRASH" | "RESTORE"
        | "TOMBSTONE" => Some(MutationPolicy::Optimistic),
        _ => None,
    }
}
