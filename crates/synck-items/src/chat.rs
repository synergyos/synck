use super::MutationPolicy;

pub const ITEM_TYPE: &str = "CHAT";

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    let rest = event_type.strip_prefix("CHAT::")?;

    if !rest.contains("::") {
        return match rest {
            "CREATE"
            | "SET_TITLE"
            | "SET_LINK"
            | "SET_METADATA"
            | "SET_PROFILE"
            | "ADD_ASSISTANT"
            | "REMOVE_ASSISTANT"
            | "UPSERT_USER_PARTICIPANT"
            | "REMOVE_USER_PARTICIPANT"
            | "FEDERATE"
            | "TRASH"
            | "RESTORE"
            | "TOMBSTONE" => Some(MutationPolicy::Optimistic),
            _ => None,
        };
    }

    match event_type {
        "CHAT::MESSAGE::CREATE" => Some(MutationPolicy::Append),
        "CHAT::MESSAGE::SET_CONTENT" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE::TRASH" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE::RESTORE" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE::TOMBSTONE" => Some(MutationPolicy::Append),
        "CHAT::MESSAGE.REACTION::CREATE" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.REACTION::SET_CONTENT" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.REACTION::TRASH" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.REACTION::RESTORE" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.REACTION::TOMBSTONE" => Some(MutationPolicy::Append),
        "CHAT::MESSAGE.ATTACHMENT::CREATE" => Some(MutationPolicy::Append),
        "CHAT::MESSAGE.ATTACHMENT::SET_METADATA" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.ATTACHMENT::SET_CONTENT" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.ATTACHMENT::TRASH" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.ATTACHMENT::RESTORE" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.ATTACHMENT::TOMBSTONE" => Some(MutationPolicy::Append),
        "CHAT::MESSAGE.FILE_REF::CREATE" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.FILE_REF::SET_METADATA" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.FILE_REF::TRASH" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.FILE_REF::RESTORE" => Some(MutationPolicy::Optimistic),
        "CHAT::MESSAGE.FILE_REF::TOMBSTONE" => Some(MutationPolicy::Append),
        "CHAT::FILE::CREATE" => Some(MutationPolicy::Optimistic),
        "CHAT::FILE::SET_METADATA" => Some(MutationPolicy::Optimistic),
        "CHAT::FILE::SET_CONTENT" => Some(MutationPolicy::Optimistic),
        "CHAT::FILE::TRASH" => Some(MutationPolicy::Optimistic),
        "CHAT::FILE::RESTORE" => Some(MutationPolicy::Optimistic),
        "CHAT::FILE::TOMBSTONE" => Some(MutationPolicy::Append),
        "CHAT::FILE_FOLDER::CREATE" => Some(MutationPolicy::Optimistic),
        "CHAT::FILE_FOLDER::SET_METADATA" => Some(MutationPolicy::Optimistic),
        "CHAT::FILE_FOLDER::TRASH" => Some(MutationPolicy::Optimistic),
        "CHAT::FILE_FOLDER::RESTORE" => Some(MutationPolicy::Optimistic),
        "CHAT::FILE_FOLDER::TOMBSTONE" => Some(MutationPolicy::Append),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_message_and_attachment_create_use_append_policy() {
        for event in ["CHAT::MESSAGE::CREATE", "CHAT::MESSAGE.ATTACHMENT::CREATE"] {
            assert_eq!(event_policy(event), Some(MutationPolicy::Append));
        }
    }

    #[test]
    fn chat_message_edit_stays_optimistic() {
        assert_eq!(
            event_policy("CHAT::MESSAGE::SET_CONTENT"),
            Some(MutationPolicy::Optimistic)
        );
    }

    #[test]
    fn chat_file_folder_events_are_chat_events() {
        assert_eq!(
            event_policy("CHAT::FILE_FOLDER::CREATE"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(
            event_policy("CHAT::FILE_FOLDER::SET_METADATA"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(
            event_policy("CHAT::FILE_FOLDER::TRASH"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(
            event_policy("CHAT::FILE_FOLDER::TOMBSTONE"),
            Some(MutationPolicy::Append)
        );
    }

    #[test]
    fn chat_file_events_are_chat_events() {
        assert_eq!(
            event_policy("CHAT::FILE::CREATE"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(
            event_policy("CHAT::FILE::SET_METADATA"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(
            event_policy("CHAT::FILE::SET_CONTENT"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(
            event_policy("CHAT::FILE::TRASH"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(
            event_policy("CHAT::FILE::TOMBSTONE"),
            Some(MutationPolicy::Append)
        );
    }

    #[test]
    fn chat_message_file_ref_events_are_chat_events() {
        assert_eq!(
            event_policy("CHAT::MESSAGE.FILE_REF::CREATE"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(
            event_policy("CHAT::MESSAGE.FILE_REF::SET_METADATA"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(
            event_policy("CHAT::MESSAGE.FILE_REF::TRASH"),
            Some(MutationPolicy::Optimistic)
        );
        assert_eq!(
            event_policy("CHAT::MESSAGE.FILE_REF::TOMBSTONE"),
            Some(MutationPolicy::Append)
        );
    }
}
