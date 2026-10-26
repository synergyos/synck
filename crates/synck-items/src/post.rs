use super::MutationPolicy;

pub const ITEM_TYPE: &str = "POST";

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    let rest = event_type.strip_prefix("POST::")?;

    if !rest.contains("::") {
        return match rest {
            "CREATE" | "SET_CONTENT" | "SET_METADATA" => Some(MutationPolicy::Optimistic),
            _ => None,
        };
    }

    match event_type {
        "POST::COMMENT::CREATE" => Some(MutationPolicy::Append),
        "POST::COMMENT::SET_CONTENT" => Some(MutationPolicy::Optimistic),
        "POST::COMMENT::TRASH" => Some(MutationPolicy::Optimistic),
        "POST::COMMENT::RESTORE" => Some(MutationPolicy::Optimistic),
        "POST::COMMENT::TOMBSTONE" => Some(MutationPolicy::Append),
        "POST::ASSET::CREATE" => Some(MutationPolicy::Optimistic),
        "POST::ASSET::TOMBSTONE" => Some(MutationPolicy::Append),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn post_comment_create_uses_append_policy() {
        assert_eq!(
            event_policy("POST::COMMENT::CREATE"),
            Some(MutationPolicy::Append)
        );
    }

    #[test]
    fn post_lifecycle_uses_metadata_event() {
        assert_eq!(
            event_policy("POST::SET_METADATA"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(
            event_policy("POST::SET_CONTENT"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(event_policy("POST::PUBLISH"), None);
        assert_eq!(event_policy("POST::ASSET::SET_METADATA"), None);
    }
}
