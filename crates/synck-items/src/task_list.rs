use super::MutationPolicy;

pub const ITEM_TYPE: &str = "TASK_LIST";

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    let rest = event_type.strip_prefix("TASK_LIST::")?;

    if !rest.contains("::") {
        return match rest {
            "CREATE" | "SET_NAME" | "SET_PARENT" | "SET_METADATA" | "FEDERATE" | "TRASH"
            | "RESTORE" | "TOMBSTONE" => Some(MutationPolicy::Optimistic),
            _ => None,
        };
    }

    match event_type {
        "TASK_LIST::TASK::CREATE" => Some(MutationPolicy::Optimistic),
        "TASK_LIST::TASK::SET_METADATA" => Some(MutationPolicy::Optimistic),
        "TASK_LIST::TASK::TRASH" => Some(MutationPolicy::Optimistic),
        "TASK_LIST::TASK::RESTORE" => Some(MutationPolicy::Optimistic),
        "TASK_LIST::TASK::TOMBSTONE" => Some(MutationPolicy::Append),
        "TASK_LIST::TASK.COMMENT::CREATE" => Some(MutationPolicy::Append),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_row_tombstone_uses_append_policy() {
        assert_eq!(
            event_policy("TASK_LIST::TASK::TOMBSTONE"),
            Some(MutationPolicy::Append)
        );
    }

    #[test]
    fn task_row_create_uses_optimistic_policy() {
        assert_eq!(
            event_policy("TASK_LIST::TASK::CREATE"),
            Some(MutationPolicy::Optimistic)
        );
    }

    #[test]
    fn task_comment_create_uses_append_policy() {
        assert_eq!(
            event_policy("TASK_LIST::TASK.COMMENT::CREATE"),
            Some(MutationPolicy::Append)
        );
    }
}
