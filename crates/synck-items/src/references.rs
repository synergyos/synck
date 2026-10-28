//! Structural closure over every retained event, independent of item families.
use synck_vcs::{
    metadata::MetadataRef,
    types::{CommitEventRow, EventContent, EventProposal, ObjectId, RefEventProposal},
};

#[derive(Debug, thiserror::Error)]
#[error("invalid event envelope: {0}")]
pub struct ReferenceError(#[from] serde_json::Error);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HistoryReferences {
    pub objects: Vec<ObjectId>,
    pub metadata: Option<ObjectId>,
}

fn references(content: Option<&EventContent>, metadata: Option<&MetadataRef>) -> HistoryReferences {
    let mut objects = content
        .map(|content| content.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    objects.sort();
    objects.dedup();
    HistoryReferences {
        objects,
        metadata: metadata.map(MetadataRef::id),
    }
}

pub fn event_references(event: &EventProposal) -> Result<HistoryReferences, ReferenceError> {
    Ok(references(
        event.set_content.as_ref(),
        event.set_metadata.as_ref(),
    ))
}
pub fn ref_event_references(event: &RefEventProposal) -> Result<HistoryReferences, ReferenceError> {
    Ok(references(
        event.set_content.as_ref(),
        event.set_metadata.as_ref(),
    ))
}
pub fn commit_event_references(row: &CommitEventRow) -> Result<HistoryReferences, ReferenceError> {
    // Presence or interpretation of metadata cannot alter closure.
    match row {
        CommitEventRow::Item(row) => event_references(&serde_json::from_slice(&row.payload)?),
        CommitEventRow::Ref(row) => ref_event_references(&serde_json::from_slice(&row.payload)?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closure_is_structural_even_for_unknown_families_and_absent_metadata() {
        let a = ObjectId::from_digest([1; 32]);
        let b = ObjectId::from_digest([2; 32]);
        let event = EventProposal::from_fields(serde_json::json!({"event_type":"FUTURE::CREATE", "set_content":{"source":a,"provider":a}, "set_metadata":{"embedded_id":b}})).unwrap();
        let expected = HistoryReferences {
            objects: vec![a],
            metadata: Some(event.set_metadata.as_ref().unwrap().id()),
        };
        assert_eq!(event_references(&event).unwrap(), expected);
        let absent: EventProposal =
            serde_json::from_value(serde_json::to_value(event).unwrap()).unwrap();
        assert_eq!(event_references(&absent).unwrap(), expected);
    }
}
