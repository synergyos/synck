use super::MutationPolicy;

pub const ITEM_TYPE: &str = "TABLE";

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    let rest = event_type.strip_prefix("TABLE::")?;
    match rest {
        "CREATE" | "SET_NAME" | "SET_METADATA" | "TRASH" | "RESTORE" | "TOMBSTONE" | "FEDERATE" => {
            Some(MutationPolicy::Optimistic)
        }
        "RECORD::CREATE"
        | "RECORD::SET_METADATA"
        | "RECORD::SET_CONTENT"
        | "RECORD::TRASH"
        | "RECORD::RESTORE"
        | "RECORD::TOMBSTONE"
        | "RECORD.COMMENT::CREATE"
        | "RECORD.COMMENT::SET_METADATA"
        | "RECORD.COMMENT::SET_CONTENT"
        | "RECORD.COMMENT::TRASH"
        | "RECORD.COMMENT::RESTORE"
        | "RECORD.COMMENT::TOMBSTONE" => Some(MutationPolicy::Optimistic),
        _ => None,
    }
}
