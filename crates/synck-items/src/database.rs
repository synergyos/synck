use super::MutationPolicy;

pub const ITEM_TYPE: &str = "DATABASE";

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    let rest = event_type.strip_prefix("DATABASE::")?;
    if rest.contains("::") {
        return None;
    }
    match rest {
        "CREATE" | "SET_NAME" | "SET_METADATA" | "TRASH" | "RESTORE" | "TOMBSTONE" => {
            Some(MutationPolicy::Optimistic)
        }
        _ => None,
    }
}
