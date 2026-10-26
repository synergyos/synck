//! CALL is non-tree and accepts only its canonical command vocabulary.
use super::MutationPolicy;
pub const ITEM_TYPE: &str = crate::calls::history::ITEM_TYPE;
pub fn event_policy(event: &str) -> Option<MutationPolicy> {
    match event {
        "CALL::CREATE" | "CALL::PARTICIPANT::ANSWER" | "CALL::PARTICIPANT::JOIN" | "CALL::END" => {
            Some(MutationPolicy::Optimistic)
        }
        _ => None,
    }
}
