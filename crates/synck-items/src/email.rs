use super::MutationPolicy;

pub const ITEM_TYPE: &str = "EMAIL";

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    let rest = event_type.strip_prefix("EMAIL::")?;

    if !rest.contains("::") {
        return match rest {
            "CREATE" | "SET_NAME" | "SET_PARENT" | "SET_METADATA" | "FEDERATE" | "TRASH"
            | "RESTORE" | "TOMBSTONE" => Some(MutationPolicy::Optimistic),
            _ => None,
        };
    }

    match event_type {
        "EMAIL::ATTACHMENT::CREATE" => Some(MutationPolicy::Optimistic),
        "EMAIL::ATTACHMENT::SET_METADATA" => Some(MutationPolicy::Optimistic),
        "EMAIL::ATTACHMENT::TRASH" => Some(MutationPolicy::Optimistic),
        "EMAIL::ATTACHMENT::RESTORE" => Some(MutationPolicy::Optimistic),
        "EMAIL::ATTACHMENT::TOMBSTONE" => Some(MutationPolicy::Append),
        _ => None,
    }
}
