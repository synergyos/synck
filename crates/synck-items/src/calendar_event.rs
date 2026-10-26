use super::MutationPolicy;

pub const ITEM_TYPE: &str = "CALENDAR_EVENT";

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    match event_type {
        "CALENDAR_EVENT::CREATE"
        | "CALENDAR_EVENT::DELEGATE_CREATE"
        | "CALENDAR_EVENT::SET_METADATA"
        | "CALENDAR_EVENT::CONFIGURE_EXTERNAL"
        | "CALENDAR_EVENT::EXTERNAL_RESPONSE"
        | "CALENDAR_EVENT::REVIEW_EXTERNAL"
        | "CALENDAR_EVENT::IMPORT_INVITATION"
        | "CALENDAR_EVENT::SET_DESCRIPTION"
        | "CALENDAR_EVENT::SET_OCCURRENCE_DESCRIPTION"
        | "CALENDAR_EVENT::EDIT_OCCURRENCE"
        | "CALENDAR_EVENT::RESCHEDULE_OCCURRENCE"
        | "CALENDAR_EVENT::SPLIT_FOLLOWING_SOURCE"
        | "CALENDAR_EVENT::SPLIT_FOLLOWING_TARGET"
        | "CALENDAR_EVENT::CANCEL_FOLLOWING"
        | "CALENDAR_EVENT::CANCEL_OCCURRENCE" => Some(MutationPolicy::Optimistic),
        "CALENDAR_EVENT::ATTENDEE_RESPONSE" | "CALENDAR_EVENT::DELEGATE_RESPONSE" => {
            Some(MutationPolicy::Append)
        }
        _ => None,
    }
}
