//! Disposable reader state, never accepted history or an authorization proof.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedReader {
    version: u32,
    digest: String,
    state: ReaderState,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReaderState {
    item_id: ItemId,
    file_revisions: Vec<(i64, (String, Option<i64>))>,
    ref_seq: i64,
    data_links: Vec<subitems::ItemDataLink>,
    ancillary_events: Vec<AncillaryEvent>,
    projection: Option<replica_projection::ReplicaItemProjection>,
    chat: Option<ChatDetails>,
    subitems: Vec<ReadableSubitem>,
    erased_subitem_creations: Vec<(String, i64)>,
    erased_version_creation: bool,
}
fn digest(state: &ReaderState) -> Result<String, ReadError> {
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(state).map_err(invalid)?)
    ))
}
impl ItemReader {
    /// Export a disposable, versioned checkpoint of the incremental reader.
    ///
    /// The cache contains hydrated plaintext and costs O(item state size). Keep
    /// it in trusted storage, separately bound to repository and verified history
    /// checkpoint. Its checksum detects damage; it does not authenticate history.
    /// Use periodic snapshots with a verified replay tail, not full export on
    /// every message. A poisoned reader cannot export a usable checkpoint.
    pub fn export_cache(&self) -> Result<Vec<u8>, ReadError> {
        self.export_cache_without(&[])
    }
    /// Export the cache without the sequence-subitem `families`: their rows and
    /// their erased creations. A reader restored from it knows nothing of those
    /// rows, so the caller must apply any later event that names one with
    /// `MetadataRef::Absent`, or fold the full history instead.
    pub fn export_cache_without(&self, families: &[&str]) -> Result<Vec<u8>, ReadError> {
        self.export_cache_map_subitems(families, |_| {})
    }
    /// Export a trusted checkpoint after removing families and transforming the
    /// remaining subitem copies. Applications use this to omit source details
    /// below a member's history floor without changing their own reader.
    /// The caller owns the meaning of the redaction and its authorization.
    pub fn export_cache_map_subitems(
        &self,
        families: &[&str],
        mut transform: impl FnMut(&mut ReadableSubitem),
    ) -> Result<Vec<u8>, ReadError> {
        let kept = |family: &str| !families.contains(&family);
        if self.poisoned {
            return Err(invalid("reader must be discarded after an error"));
        }
        let state = ReaderState {
            item_id: self.item_id.clone(),
            file_revisions: self
                .file_revisions
                .iter()
                .map(|(k, v)| (*k, v.clone()))
                .collect(),
            ref_seq: self.ref_seq,
            data_links: self.data_links.values().cloned().collect(),
            ancillary_events: self.ancillary_events.clone(),
            projection: self.projection.clone(),
            chat: self.chat.clone(),
            subitems: self
                .subitems
                .values()
                .filter(|row| kept(&row.family))
                .cloned()
                .map(|mut row| {
                    transform(&mut row);
                    row
                })
                .collect(),
            erased_subitem_creations: self
                .erased_subitem_creations
                .iter()
                .filter(|(family, _)| kept(family))
                .cloned()
                .collect(),
            erased_version_creation: self.erased_version_creation,
        };
        let digest = digest(&state)?;
        serde_json::to_vec(&CachedReader {
            version: 6,
            digest,
            state,
        })
        .map_err(invalid)
    }
    /// Restore a trusted disposable cache, then continue at the next item/ref
    /// positions. Reject unknown versions, damage, identity mismatch or duplicate
    /// map keys. On failure the caller must rebuild from verified history.
    ///
    /// This is not a canonical or portable repository format. The caller owns
    /// input-size limits, confidentiality, checkpoint verification and authority.
    pub fn from_cache(expected_item_id: ItemId, bytes: &[u8]) -> Result<Self, ReadError> {
        let cache: CachedReader = serde_json::from_slice(bytes).map_err(invalid)?;
        // Version 3 predates canonical record filing-link projection. Its
        // state checksum cannot detect a semantically stale projection.
        if cache.version != 6
            || cache.state.item_id != expected_item_id
            || cache.digest != digest(&cache.state)?
        {
            return Err(invalid(
                "reader cache version, identity or checksum mismatch",
            ));
        }
        let state = cache.state;
        if state.ref_seq < 0
            || state
                .projection
                .as_ref()
                .is_some_and(|p| p.item_id != expected_item_id || p.latest_item_seq <= 0)
        {
            return Err(invalid("reader cache cursor or projection mismatch"));
        }
        if state.projection.is_none()
            && (state.ref_seq != 0
                || !state.file_revisions.is_empty()
                || !state.data_links.is_empty()
                || !state.ancillary_events.is_empty()
                || state.chat.is_some()
                || !state.subitems.is_empty())
        {
            return Err(invalid("reader cache state precedes item creation"));
        }
        let mut reader = Self::new(expected_item_id.clone());
        let latest = state.projection.as_ref().map_or(0, |p| p.latest_item_seq);
        for (seq, revision) in state.file_revisions {
            if seq <= 0 || seq > latest || reader.file_revisions.insert(seq, revision).is_some() {
                return Err(invalid("invalid reader cache file revision"));
            }
        }
        for link in state.data_links {
            if reader
                .data_links
                .insert(link.link_key.clone(), link)
                .is_some()
            {
                return Err(invalid("duplicate reader cache data link"));
            }
        }
        for row in state.subitems {
            for position in [&row.created_position, &row.latest_position]
                .into_iter()
                .chain(row.content_position.as_ref())
            {
                let limit = match position.ledger {
                    HistoryLedger::Item => latest,
                    HistoryLedger::Ref => state.ref_seq,
                };
                if position.item_id != expected_item_id || position.seq <= 0 || position.seq > limit
                {
                    return Err(invalid("invalid reader cache subitem position"));
                }
            }
            let key = (row.family.clone(), row.identity.clone());
            if reader.subitems.insert(key, row).is_some() {
                return Err(invalid("duplicate reader cache subitem"));
            }
        }
        reader.ref_seq = state.ref_seq;
        reader.ancillary_events = state.ancillary_events;
        reader.projection = state.projection;
        reader.chat = state.chat;
        reader.erased_subitem_creations = state.erased_subitem_creations.into_iter().collect();
        reader.erased_version_creation = state.erased_version_creation;
        Ok(reader)
    }
}
