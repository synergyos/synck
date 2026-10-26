use super::MutationPolicy;

pub const ITEM_TYPE: &str = "CALENDAR";

pub fn event_policy(event_type: &str) -> Option<MutationPolicy> {
    match event_type {
        "CALENDAR::CREATE" | "CALENDAR::SET_METADATA" => Some(MutationPolicy::Optimistic),
        _ => None,
    }
}
