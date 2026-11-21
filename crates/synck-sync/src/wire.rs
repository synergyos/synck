//! Protocol 2 message encoding (protocol.md, "Encoding"). Messages use the
//! format's binary primitives and its strict decoding: one encoding per value,
//! no trailing bytes, no unknown tags or flag bits. Rows a message carries are
//! rebuilt from the message's own context; nothing is sent twice.

use crate::{
    Capabilities, CommitLogPage, CommitLogRequest, CommitMetadata, CommitSummary,
    CommitSummaryEvent, Cursor, HistoryFragment, HistoryPage, HistoryPosition, HistoryRequest,
    ItemCommit, ItemHead, ItemHistoriesPage, ItemHistoriesRequest, ItemHistoryPage,
    ItemHistoryPosition, Lease, LeasePaths, LeaseRenewal, LeaseRequest, LeaseToken, LeasedPath,
    Leases, MAX_PACK_OBJECTS, MAX_PAGE_RECORDS, ObjectRequest, PROTOCOL_VERSION, PageAck,
    PendingCommit, PolicyLimit, PolicyLimitName, ProposalPage, ProposalRecord, ProtocolError,
    PublishedHeader, Receipt, Released, RepositoryIdentity, ServerPolicy, StateDeparture,
    StateEntry, StatePage, StatePosition, StateRequest, SubmitRequest, SubmitResult, invalid,
};
use synck_vcs::codec::{
    Decoder, KIND_ITEM, KIND_REF, canonical_commit_bytes_for_hash, canonical_event_proposal_bytes,
    canonical_ref_event_proposal_bytes, decode_commit_header, decode_event_proposal,
    decode_ref_event_proposal, put_bool, put_content, put_digest, put_seq, put_str, put_target,
    put_time, put_u8, put_u16, put_u32, put_u64, put_uuid,
};
use synck_vcs::types::{
    CommitEventRow, CommitId, CommitRow, EventRow, ItemId, LifecycleState, ObjectId, RefEventRow,
    RepoId, SequencingMode, Sha256Digest, SubtreeClosureGuard, Timestamp,
    commit_id_from_digest_bytes, commit_id_to_digest_bytes, sha256_digest_from_digest_bytes,
    sha256_digest_to_digest_bytes,
};

type Result<T> = std::result::Result<T, ProtocolError>;

/// A message with one encoding.
pub trait Wire: Sized {
    fn put(&self, out: &mut Vec<u8>) -> Result<()>;
    fn get(decoder: &mut Decoder) -> Result<Self>;
}

pub fn encode<T: Wire>(value: &T) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    value.put(&mut out)?;
    Ok(out)
}

/// Decode one whole message. Transports cap the body before calling this.
pub fn decode<T: Wire>(bytes: &[u8]) -> Result<T> {
    let mut decoder = Decoder::new(bytes);
    let value = T::get(&mut decoder)?;
    decoder.finish()?;
    Ok(value)
}

// --- Protocol types ---

fn put_opt<T>(
    out: &mut Vec<u8>,
    value: Option<T>,
    put: impl FnOnce(&mut Vec<u8>, T) -> Result<()>,
) -> Result<()> {
    match value {
        None => put_u8(out, 0),
        Some(value) => {
            put_u8(out, 1);
            put(out, value)?;
        }
    }
    Ok(())
}

fn get_opt<'a, T>(
    decoder: &mut Decoder<'a>,
    get: impl FnOnce(&mut Decoder<'a>) -> Result<T>,
) -> Result<Option<T>> {
    match decoder.u8()? {
        0 => Ok(None),
        1 => get(decoder).map(Some),
        _ => Err(invalid("invalid option tag")),
    }
}

fn put_list<T>(
    out: &mut Vec<u8>,
    items: &[T],
    mut put: impl FnMut(&mut Vec<u8>, &T) -> Result<()>,
) -> Result<()> {
    put_u32(
        out,
        u32::try_from(items.len()).map_err(|_| invalid("list too long"))?,
    );
    items.iter().try_for_each(|item| put(out, item))
}

/// Never preallocates from the count: every element takes at least one byte,
/// so a forged count fails as truncation.
fn get_list<'a, T>(
    decoder: &mut Decoder<'a>,
    mut get: impl FnMut(&mut Decoder<'a>) -> Result<T>,
) -> Result<Vec<T>> {
    let count = decoder.u32()?;
    let mut items = Vec::new();
    for _ in 0..count {
        items.push(get(decoder)?);
    }
    Ok(items)
}

fn put_blob(out: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    put_u32(
        out,
        u32::try_from(bytes.len()).map_err(|_| invalid("blob too long"))?,
    );
    out.extend_from_slice(bytes);
    Ok(())
}

fn get_blob<'a>(decoder: &mut Decoder<'a>) -> Result<&'a [u8]> {
    let length = decoder.u32()? as usize;
    Ok(decoder.take(length)?)
}

fn put_limit(out: &mut Vec<u8>, limit: usize) -> Result<()> {
    if limit == 0 || limit > MAX_PAGE_RECORDS {
        return Err(ProtocolError::Limit("page limit"));
    }
    put_u16(out, limit as u16);
    Ok(())
}

fn get_limit(decoder: &mut Decoder) -> Result<usize> {
    let limit = decoder.u16()? as usize;
    if limit == 0 || limit > MAX_PAGE_RECORDS {
        return Err(ProtocolError::Limit("page limit"));
    }
    Ok(limit)
}

/// A `u64` sequence or count, at most 2^63 - 1.
fn put_count(out: &mut Vec<u8>, value: u64) -> Result<()> {
    if value > i64::MAX as u64 {
        return Err(invalid("value exceeds 2^63 - 1"));
    }
    put_u64(out, value);
    Ok(())
}

fn get_count(decoder: &mut Decoder) -> Result<u64> {
    Ok(decoder.count()? as u64)
}

/// A `u64` count held as `i64` in memory, such as a head that may be zero.
fn put_head(out: &mut Vec<u8>, value: i64) -> Result<()> {
    put_count(
        out,
        u64::try_from(value).map_err(|_| invalid("negative count"))?,
    )
}

fn put_index(out: &mut Vec<u8>, index: i32) -> Result<()> {
    put_u32(
        out,
        u32::try_from(index).map_err(|_| invalid("negative event index"))?,
    );
    Ok(())
}

fn get_index(decoder: &mut Decoder) -> Result<i32> {
    i32::try_from(decoder.u32()?).map_err(|_| invalid("event index exceeds 2^31 - 1"))
}

fn put_sha(out: &mut Vec<u8>, digest: &Sha256Digest) {
    put_digest(out, &sha256_digest_to_digest_bytes(digest));
}

fn get_sha(decoder: &mut Decoder) -> Result<Sha256Digest> {
    sha256_digest_from_digest_bytes(&decoder.digest()?).map_err(|e| invalid(e.to_string()))
}

fn put_commit_id(out: &mut Vec<u8>, id: &CommitId) {
    put_digest(out, &commit_id_to_digest_bytes(id));
}

fn get_commit_id(decoder: &mut Decoder) -> Result<CommitId> {
    commit_id_from_digest_bytes(&decoder.digest()?).map_err(|e| invalid(e.to_string()))
}

fn put_item(out: &mut Vec<u8>, id: &ItemId) {
    put_uuid(out, id.as_str());
}

fn get_item(decoder: &mut Decoder) -> Result<ItemId> {
    Ok(decoder.item_id()?)
}

fn get_string(decoder: &mut Decoder) -> Result<String> {
    Ok(decoder.str()?.to_string())
}

fn put_object(out: &mut Vec<u8>, id: &ObjectId) {
    put_digest(out, &id.as_bytes());
}

fn get_object(decoder: &mut Decoder) -> Result<ObjectId> {
    Ok(decoder.object_id()?)
}

fn put_strs(out: &mut Vec<u8>, values: &[String]) -> Result<()> {
    put_list(out, values, |out, value| Ok(put_str(out, value)?))
}

fn get_strs(decoder: &mut Decoder) -> Result<Vec<String>> {
    get_list(decoder, get_string)
}

// --- Identity and cursors ---

impl Wire for RepositoryIdentity {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_uuid(out, self.repo_id.as_str());
        out.extend_from_slice(self.generation.as_bytes());
        Ok(())
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        let repo_id =
            RepoId::parse(decoder.uuid()?.to_string()).map_err(|e| invalid(e.to_string()))?;
        Ok(Self {
            repo_id,
            generation: decoder.uuid()?,
        })
    }
}

impl Wire for Cursor {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.repository.put(out)?;
        put_count(out, self.repo_seq)?;
        put_sha(out, &self.checkpoint);
        Ok(())
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            repository: RepositoryIdentity::get(decoder)?,
            repo_seq: get_count(decoder)?,
            checkpoint: get_sha(decoder)?,
        })
    }
}

impl Wire for ServerPolicy {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        for limit in [
            self.max_events_per_commit,
            self.max_object_bytes,
            self.quota_bytes,
        ] {
            put_opt(out, limit, put_count)?;
        }
        Ok(())
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        let mut limit = || get_opt(decoder, get_count);
        Ok(Self {
            max_events_per_commit: limit()?,
            max_object_bytes: limit()?,
            quota_bytes: limit()?,
        })
    }
}

impl Wire for Capabilities {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_u16(out, self.protocol_version);
        put_bool(out, self.encrypted);
        self.latest.put(out)?;
        self.policy.put(out)
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        let protocol_version = decoder.u16()?;
        if protocol_version != PROTOCOL_VERSION {
            return Err(invalid("unsupported sync protocol version"));
        }
        Ok(Self {
            protocol_version,
            encrypted: decoder.bool()?,
            latest: Cursor::get(decoder)?,
            policy: ServerPolicy::get(decoder)?,
        })
    }
}

// --- Proposals ---

const RECORD_ITEM: u8 = 1;
const RECORD_REF: u8 = 2;
const RECORD_HEAD_GUARD: u8 = 3;
const RECORD_SUBTREE_GUARD: u8 = 4;

impl Wire for ProposalRecord {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Item(event) => {
                put_u8(out, RECORD_ITEM);
                put_blob(out, &canonical_event_proposal_bytes(event)?)
            }
            Self::Ref(event) => {
                put_u8(out, RECORD_REF);
                put_blob(out, &canonical_ref_event_proposal_bytes(event)?)
            }
            Self::Guard(guard) => {
                put_u8(out, RECORD_HEAD_GUARD);
                put_item(out, &guard.item_id);
                Ok(put_seq(out, guard.head_seq)?)
            }
            Self::SubtreeGuard(guard) => {
                put_u8(out, RECORD_SUBTREE_GUARD);
                put_item(out, &guard.root_item_id);
                put_bool(out, guard.require_empty_live_descendants);
                put_list(
                    out,
                    &guard.expected_non_tombstoned_descendants,
                    |out, (id, seq)| {
                        put_item(out, id);
                        Ok(put_seq(out, *seq)?)
                    },
                )
            }
        }
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        match decoder.u8()? {
            RECORD_ITEM => Ok(Self::Item(decode_event_proposal(get_blob(decoder)?)?)),
            RECORD_REF => Ok(Self::Ref(decode_ref_event_proposal(get_blob(decoder)?)?)),
            RECORD_HEAD_GUARD => Ok(Self::Guard(ItemHead {
                item_id: get_item(decoder)?,
                head_seq: decoder.seq()?,
            })),
            RECORD_SUBTREE_GUARD => Ok(Self::SubtreeGuard(SubtreeClosureGuard {
                root_item_id: get_item(decoder)?,
                require_empty_live_descendants: decoder.bool()?,
                expected_non_tombstoned_descendants: get_list(decoder, |d| {
                    Ok((get_item(d)?, d.seq()?))
                })?,
            })),
            _ => Err(invalid("unknown proposal record tag")),
        }
    }
}

impl Wire for ProposalPage {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.repository.put(out)?;
        put_str(out, &self.operation_id)?;
        put_count(out, self.offset)?;
        put_list(out, &self.records, |out, record| record.put(out))
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            repository: RepositoryIdentity::get(decoder)?,
            operation_id: get_string(decoder)?,
            offset: get_count(decoder)?,
            records: get_list(decoder, ProposalRecord::get)?,
        })
    }
}

impl Wire for PageAck {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_count(out, self.next_offset)?;
        Ok(put_time(out, self.expires_at)?)
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            next_offset: get_count(decoder)?,
            expires_at: decoder.time()?,
        })
    }
}

/// The commit header a request's metadata and event summary make.
pub fn submit_header(request: &SubmitRequest) -> Result<Vec<u8>> {
    let metadata = &request.metadata;
    Ok(canonical_commit_bytes_for_hash(
        &metadata.author_id,
        &metadata.committer_id,
        metadata.occurred_at,
        metadata.message.as_deref(),
        usize::try_from(request.event_count).map_err(|_| invalid("event count overflow"))?,
        &request.events_digest,
    )?)
}

impl Wire for SubmitRequest {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.repository.put(out)?;
        put_str(out, &self.operation_id)?;
        put_opt(out, self.repository_head, put_count)?;
        put_blob(out, &submit_header(self)?)?;
        put_count(out, self.guard_count)?;
        put_sha(out, &self.guards_digest);
        Ok(())
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        let repository = RepositoryIdentity::get(decoder)?;
        let operation_id = get_string(decoder)?;
        let repository_head = get_opt(decoder, get_count)?;
        let header = decode_commit_header(get_blob(decoder)?)?;
        Ok(Self {
            repository_head,
            repository,
            operation_id,
            metadata: CommitMetadata {
                author_id: header.author_id,
                committer_id: header.committer_id,
                message: header.message,
                occurred_at: header.occurred_at,
            },
            event_count: header.event_count.into(),
            events_digest: header.events_digest,
            guard_count: get_count(decoder)?,
            guards_digest: get_sha(decoder)?,
        })
    }
}

impl Wire for Receipt {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_str(out, &self.operation_id)?;
        put_sha(out, &self.request_digest);
        put_commit_id(out, &self.commit_id);
        put_time(out, self.accepted_at)?;
        put_opt(out, self.published.as_ref(), |out, cursor| cursor.put(out))
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            operation_id: get_string(decoder)?,
            request_digest: get_sha(decoder)?,
            commit_id: get_commit_id(decoder)?,
            accepted_at: decoder.time()?,
            published: get_opt(decoder, Cursor::get)?,
        })
    }
}

const POLICY_EVENTS_PER_COMMIT: u8 = 1;
const POLICY_OBJECT_BYTES: u8 = 2;
const POLICY_QUOTA_BYTES: u8 = 3;

impl Wire for SubmitResult {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Self::Accepted { receipt } => {
                put_u8(out, 0);
                receipt.put(out)
            }
            Self::Conflict { items } => {
                put_u8(out, 1);
                put_list(out, items, |out, head| {
                    put_item(out, &head.item_id);
                    put_head(out, head.head_seq)
                })
            }
            Self::PolicyLimit { violation } => {
                put_u8(out, 2);
                put_u8(
                    out,
                    match violation.limit {
                        PolicyLimitName::EventsPerCommit => POLICY_EVENTS_PER_COMMIT,
                        PolicyLimitName::ObjectBytes => POLICY_OBJECT_BYTES,
                        PolicyLimitName::QuotaBytes => POLICY_QUOTA_BYTES,
                    },
                );
                put_count(out, violation.maximum)?;
                put_count(out, violation.actual)
            }
        }
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        match decoder.u8()? {
            0 => Ok(Self::Accepted {
                receipt: Receipt::get(decoder)?,
            }),
            1 => Ok(Self::Conflict {
                items: get_list(decoder, |d| {
                    Ok(ItemHead {
                        item_id: get_item(d)?,
                        head_seq: d.count()?,
                    })
                })?,
            }),
            2 => Ok(Self::PolicyLimit {
                violation: PolicyLimit {
                    limit: match decoder.u8()? {
                        POLICY_EVENTS_PER_COMMIT => PolicyLimitName::EventsPerCommit,
                        POLICY_OBJECT_BYTES => PolicyLimitName::ObjectBytes,
                        POLICY_QUOTA_BYTES => PolicyLimitName::QuotaBytes,
                        _ => return Err(invalid("unknown policy limit")),
                    },
                    maximum: get_count(decoder)?,
                    actual: get_count(decoder)?,
                },
            }),
            _ => Err(invalid("unknown submit status")),
        }
    }
}

// --- History ---

impl Wire for HistoryPosition {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.completed.put(out)?;
        put_opt(out, self.pending.as_ref(), |out, pending| {
            put_commit_id(out, &pending.commit_id);
            put_u32(
                out,
                u32::try_from(pending.next_event_index)
                    .map_err(|_| invalid("pending event index overflow"))?,
            );
            Ok(())
        })
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            completed: Cursor::get(decoder)?,
            pending: get_opt(decoder, |d| {
                let commit_id = get_commit_id(d)?;
                let next_event_index = d.u32()?;
                if next_event_index == 0 {
                    return Err(invalid("a pending event index is positive"));
                }
                Ok(PendingCommit {
                    commit_id,
                    next_event_index: next_event_index.into(),
                })
            })?,
        })
    }
}

impl Wire for HistoryRequest {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.after.put(out)?;
        put_limit(out, self.limit)
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            after: HistoryPosition::get(decoder)?,
            limit: get_limit(decoder)?,
        })
    }
}

const HEADER_OCCURRED_AT: u8 = 1 << 0;
const HEADER_MESSAGE: u8 = 1 << 1;

fn put_published_header(out: &mut Vec<u8>, header: &PublishedHeader) -> Result<()> {
    let commit = &header.commit;
    put_commit_id(out, &commit.commit_id);
    put_seq(
        out,
        commit
            .repo_seq
            .ok_or_else(|| invalid("unpublished commit"))?,
    )?;
    put_time(out, commit.created_at)?;
    put_sha(out, &header.checkpoint);
    put_str(out, &commit.author_id)?;
    put_str(out, &commit.committer_id)?;
    let mut flags = 0;
    if commit.occurred_at.is_some() {
        flags |= HEADER_OCCURRED_AT;
    }
    if commit.message.is_some() {
        flags |= HEADER_MESSAGE;
    }
    put_u8(out, flags);
    if let Some(time) = commit.occurred_at {
        put_time(out, time)?;
    }
    if let Some(message) = &commit.message {
        put_str(out, message)?;
    }
    put_index(out, commit.event_count)
}

fn get_published_header(decoder: &mut Decoder, repo_id: &RepoId) -> Result<PublishedHeader> {
    let commit_id = get_commit_id(decoder)?;
    let repo_seq = decoder.seq()?;
    let created_at = decoder.time()?;
    let checkpoint = get_sha(decoder)?;
    let author_id = get_string(decoder)?;
    let committer_id = get_string(decoder)?;
    let flags = decoder.u8()?;
    if flags & !(HEADER_OCCURRED_AT | HEADER_MESSAGE) != 0 {
        return Err(invalid("unknown published header flag"));
    }
    let occurred_at = (flags & HEADER_OCCURRED_AT != 0)
        .then(|| decoder.time())
        .transpose()?;
    let message = (flags & HEADER_MESSAGE != 0)
        .then(|| get_string(decoder))
        .transpose()?;
    Ok(PublishedHeader {
        commit: CommitRow {
            repo_id: repo_id.clone(),
            commit_id,
            repo_seq: Some(repo_seq),
            author_id,
            committer_id,
            message,
            occurred_at,
            created_at,
            event_count: get_index(decoder)?,
        },
        checkpoint,
    })
}

fn row_seq(row: &CommitEventRow) -> i64 {
    match row {
        CommitEventRow::Item(row) => row.item_seq,
        CommitEventRow::Ref(row) => row.ref_seq,
    }
}

fn row_payload(row: &CommitEventRow) -> &[u8] {
    match row {
        CommitEventRow::Item(row) => &row.payload,
        CommitEventRow::Ref(row) => &row.payload,
    }
}

/// `Event`: the envelope and its sequence.
fn put_event(out: &mut Vec<u8>, row: &CommitEventRow) -> Result<()> {
    put_blob(out, row_payload(row))?;
    put_seq(out, row_seq(row))?;
    Ok(())
}

/// The encoded size of one history `Event`, for producers filling a page.
pub fn event_len(row: &CommitEventRow) -> usize {
    4 + row_payload(row).len() + 8
}

/// The encoded size of one `PublishedHeader`, for producers filling a page.
pub fn header_len(header: &PublishedHeader) -> Result<usize> {
    let mut out = Vec::new();
    put_published_header(&mut out, header)?;
    Ok(out.len())
}

/// An `Event` as read, before its commit is known.
struct WireEvent<'a> {
    kind: u8,
    payload: &'a [u8],
    event_type: String,
    id: ItemId,
    seq: i64,
}

/// The kind, event type and item ID open both envelope kinds. History
/// verification decodes the whole envelope.
fn get_event<'a>(decoder: &mut Decoder<'a>) -> Result<WireEvent<'a>> {
    let payload = get_blob(decoder)?;
    let mut head = Decoder::new(payload);
    let kind = head.u8()?;
    if kind != KIND_ITEM && kind != KIND_REF {
        return Err(invalid("not an envelope"));
    }
    let event_type = get_string(&mut head)?;
    let id = get_item(&mut head)?;
    let seq = decoder.seq()?;
    Ok(WireEvent {
        kind,
        payload,
        event_type,
        id,
        seq,
    })
}

impl WireEvent<'_> {
    fn row(
        self,
        repo_id: &RepoId,
        commit_id: &CommitId,
        repo_seq: i64,
        created_at: Timestamp,
        commit_event_idx: i32,
    ) -> CommitEventRow {
        if self.kind == KIND_ITEM {
            CommitEventRow::Item(EventRow {
                repo_id: repo_id.clone(),
                item_id: self.id,
                item_seq: self.seq,
                event_type: self.event_type,
                commit_id: commit_id.clone(),
                commit_event_idx,
                repo_seq: Some(repo_seq),
                payload: self.payload.to_vec(),
                metadata: None,
                created_at,
                // The wire carries no parent: a reader replays the envelope.
                parent_folder_id: None,
            })
        } else {
            CommitEventRow::Ref(RefEventRow {
                repo_id: repo_id.clone(),
                owner_item_id: self.id,
                ref_seq: self.seq,
                event_type: self.event_type,
                commit_id: commit_id.clone(),
                commit_event_idx,
                repo_seq: Some(repo_seq),
                payload: self.payload.to_vec(),
                metadata: None,
                created_at,
            })
        }
    }
}

impl Wire for HistoryPage {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.after.put(out)?;
        put_list(out, &self.fragments, |out, fragment| {
            put_published_header(out, &fragment.header)?;
            put_list(out, &fragment.events, put_event)
        })?;
        put_bool(out, self.has_more);
        Ok(())
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        let after = HistoryPosition::get(decoder)?;
        let repo_id = after.completed.repository.repo_id.clone();
        // Only the first fragment may continue the pending commit.
        let mut first_index = after
            .pending
            .as_ref()
            .map_or(0, |pending| pending.next_event_index);
        let fragments = get_list(decoder, |d| {
            let header = get_published_header(d, &repo_id)?;
            let commit = &header.commit;
            let mut index = std::mem::take(&mut first_index);
            let events = get_list(d, |d| {
                let event = get_event(d)?;
                let commit_event_idx =
                    i32::try_from(index).map_err(|_| invalid("event index overflow"))?;
                index += 1;
                Ok(event.row(
                    &repo_id,
                    &commit.commit_id,
                    commit.repo_seq.expect("decoded header"),
                    commit.created_at,
                    commit_event_idx,
                ))
            })?;
            Ok(HistoryFragment { header, events })
        })?;
        Ok(Self {
            after,
            fragments,
            has_more: decoder.bool()?,
        })
    }
}

// --- State ---

impl Wire for StatePosition {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.repository.put(out)?;
        put_count(out, self.changed_seq)?;
        put_opt(out, self.item_id.as_ref(), |out, id| {
            put_item(out, id);
            Ok(())
        })
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            repository: RepositoryIdentity::get(decoder)?,
            changed_seq: get_count(decoder)?,
            item_id: get_opt(decoder, get_item)?,
        })
    }
}

impl Wire for StateRequest {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.after.put(out)?;
        put_limit(out, self.limit)
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            after: StatePosition::get(decoder)?,
            limit: get_limit(decoder)?,
        })
    }
}

fn put_opt_str(out: &mut Vec<u8>, value: Option<&String>) -> Result<()> {
    put_opt(out, value, |out, value| Ok(put_str(out, value)?))
}

fn get_opt_str(decoder: &mut Decoder) -> Result<Option<String>> {
    get_opt(decoder, get_string)
}

fn put_opt_object(out: &mut Vec<u8>, value: Option<&ObjectId>) -> Result<()> {
    put_opt(out, value, |out, id| {
        put_object(out, id);
        Ok(())
    })
}

impl Wire for StateEntry {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_item(out, &self.item_id);
        put_str(out, &self.item_type)?;
        put_count(out, self.changed_seq)?;
        put_seq(out, self.head_seq)?;
        put_head(out, self.ref_head_seq)?;
        put_target(out, self.parent_folder_id.as_ref());
        put_opt_str(out, self.name.as_ref())?;
        put_opt_str(out, self.record_key.as_ref())?;
        put_target(out, self.link_folder_id.as_ref());
        put_u8(
            out,
            match self.lifecycle_state {
                LifecycleState::Live => 1,
                LifecycleState::Trash => 2,
                LifecycleState::Tombstoned => 3,
            },
        );
        put_u8(
            out,
            match self.sequencing_mode {
                SequencingMode::Local => 1,
                SequencingMode::Federated => 2,
            },
        );
        put_opt_str(out, self.federation_stream_id.as_ref())?;
        put_content(out, &self.content)?;
        put_opt_object(out, self.metadata_id.as_ref())?;
        put_opt_object(out, self.name_metadata_id.as_ref())?;
        put_time(out, self.created_at)?;
        put_str(out, &self.created_by)?;
        put_time(out, self.changed_at)?;
        Ok(put_str(out, &self.changed_by)?)
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            item_id: get_item(decoder)?,
            item_type: get_string(decoder)?,
            changed_seq: get_count(decoder)?,
            head_seq: decoder.seq()?,
            ref_head_seq: decoder.count()?,
            parent_folder_id: decoder.target()?,
            name: get_opt_str(decoder)?,
            record_key: get_opt_str(decoder)?,
            link_folder_id: decoder.target()?,
            lifecycle_state: match decoder.u8()? {
                1 => LifecycleState::Live,
                2 => LifecycleState::Trash,
                3 => LifecycleState::Tombstoned,
                _ => return Err(invalid("unknown lifecycle state")),
            },
            sequencing_mode: match decoder.u8()? {
                1 => SequencingMode::Local,
                2 => SequencingMode::Federated,
                _ => return Err(invalid("unknown sequencing mode")),
            },
            federation_stream_id: get_opt_str(decoder)?,
            content: decoder.content()?.roles().clone(),
            metadata_id: get_opt(decoder, get_object)?,
            name_metadata_id: get_opt(decoder, get_object)?,
            created_at: decoder.time()?,
            created_by: get_string(decoder)?,
            changed_at: decoder.time()?,
            changed_by: get_string(decoder)?,
        })
    }
}

impl Wire for StatePage {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.after.put(out)?;
        put_list(out, &self.entries, |out, entry| entry.put(out))?;
        put_list(out, &self.departed, |out, departure| {
            put_item(out, &departure.item_id);
            put_count(out, departure.changed_seq)
        })?;
        self.through.put(out)?;
        put_bool(out, self.has_more);
        Ok(())
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            after: StatePosition::get(decoder)?,
            entries: get_list(decoder, StateEntry::get)?,
            departed: get_list(decoder, |d| {
                Ok(StateDeparture {
                    item_id: get_item(d)?,
                    changed_seq: get_count(d)?,
                })
            })?,
            through: Cursor::get(decoder)?,
            has_more: decoder.bool()?,
        })
    }
}

// --- Item histories ---

impl Wire for ItemHistoriesRequest {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.repository.put(out)?;
        put_list(out, &self.items, |out, item| {
            put_item(out, &item.item_id);
            put_count(out, item.after_seq)?;
            put_count(out, item.after_ref_seq)
        })?;
        put_limit(out, self.limit)
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        let repository = RepositoryIdentity::get(decoder)?;
        let items = get_list(decoder, |d| {
            Ok(ItemHistoryPosition {
                item_id: get_item(d)?,
                after_seq: get_count(d)?,
                after_ref_seq: get_count(d)?,
            })
        })?;
        if items.is_empty() || items.len() > MAX_PAGE_RECORDS {
            return Err(ProtocolError::Limit("item histories items"));
        }
        Ok(Self {
            repository,
            items,
            limit: get_limit(decoder)?,
        })
    }
}

fn put_item_event(out: &mut Vec<u8>, row: &CommitEventRow) -> Result<()> {
    put_seq(
        out,
        row.repo_seq().ok_or_else(|| invalid("unpublished event"))?,
    )?;
    put_index(out, row.commit_event_idx())?;
    put_event(out, row)
}

fn put_item_history_page(out: &mut Vec<u8>, page: &ItemHistoryPage) -> Result<()> {
    put_item(out, &page.item_id);
    put_list(out, &page.events, put_item_event)?;
    put_list(out, &page.refs, put_item_event)?;
    put_list(out, &page.commits, |out, commit| {
        put_count(out, commit.repo_seq)?;
        put_commit_id(out, &commit.commit_id);
        put_str(out, &commit.author_id)?;
        put_time(out, commit.occurred_at)?;
        Ok(put_time(out, commit.created_at)?)
    })?;
    put_bool(out, page.has_more);
    Ok(())
}

fn get_item_rows<'a>(
    decoder: &mut Decoder<'a>,
    kind: u8,
) -> Result<Vec<(i64, i32, WireEvent<'a>)>> {
    get_list(decoder, |d| {
        let repo_seq = d.seq()?;
        let index = get_index(d)?;
        let event = get_event(d)?;
        if event.kind != kind {
            return Err(invalid("item history event of the wrong kind"));
        }
        Ok((repo_seq, index, event))
    })
}

/// Rows name their commit by `repo_seq`; `commits` lists each once, ascending,
/// and holds the rest of what a row needs.
fn get_item_history_page(decoder: &mut Decoder, repo_id: &RepoId) -> Result<ItemHistoryPage> {
    let item_id = get_item(decoder)?;
    let events = get_item_rows(decoder, KIND_ITEM)?;
    let refs = get_item_rows(decoder, KIND_REF)?;
    let commits = get_list(decoder, |d| {
        Ok(ItemCommit {
            repo_seq: get_count(d)?,
            commit_id: get_commit_id(d)?,
            author_id: get_string(d)?,
            occurred_at: d.time()?,
            created_at: d.time()?,
        })
    })?;
    if commits
        .windows(2)
        .any(|pair| pair[0].repo_seq >= pair[1].repo_seq)
    {
        return Err(invalid("item history commits are not ascending"));
    }
    let build = |rows: Vec<(i64, i32, WireEvent)>| -> Result<Vec<CommitEventRow>> {
        rows.into_iter()
            .map(|(repo_seq, index, event)| {
                let at = commits
                    .binary_search_by_key(&(repo_seq as u64), |commit| commit.repo_seq)
                    .map_err(|_| invalid("item history row without its commit"))?;
                let commit = &commits[at];
                Ok(event.row(
                    repo_id,
                    &commit.commit_id,
                    repo_seq,
                    commit.created_at,
                    index,
                ))
            })
            .collect()
    };
    let events = build(events)?;
    let refs = build(refs)?;
    Ok(ItemHistoryPage {
        item_id,
        events,
        refs,
        commits,
        has_more: decoder.bool()?,
    })
}

impl Wire for ItemHistoriesPage {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.repository.put(out)?;
        put_list(out, &self.pages, put_item_history_page)
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        let repository = RepositoryIdentity::get(decoder)?;
        let pages = get_list(decoder, |d| get_item_history_page(d, &repository.repo_id))?;
        Ok(Self { repository, pages })
    }
}

// --- Commit log ---

impl Wire for CommitLogRequest {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        self.repository.put(out)?;
        put_opt(out, self.before_seq, put_count)?;
        put_limit(out, self.limit)
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            repository: RepositoryIdentity::get(decoder)?,
            before_seq: get_opt(decoder, get_count)?,
            limit: get_limit(decoder)?,
        })
    }
}

impl Wire for CommitSummary {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_count(out, self.repo_seq)?;
        put_commit_id(out, &self.commit_id);
        put_str(out, &self.author_id)?;
        put_time(out, self.occurred_at)?;
        put_u32(out, self.event_count);
        put_list(out, &self.events, |out, event| {
            put_item(out, &event.item_id);
            put_seq(out, event.item_seq)?;
            put_index(out, event.commit_event_idx)?;
            Ok(put_str(out, &event.event_type)?)
        })
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            repo_seq: get_count(decoder)?,
            commit_id: get_commit_id(decoder)?,
            author_id: get_string(decoder)?,
            occurred_at: decoder.time()?,
            event_count: decoder.u32()?,
            events: get_list(decoder, |d| {
                Ok(CommitSummaryEvent {
                    item_id: get_item(d)?,
                    item_seq: d.seq()?,
                    commit_event_idx: get_index(d)?,
                    event_type: get_string(d)?,
                })
            })?,
        })
    }
}

impl Wire for CommitLogPage {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_list(out, &self.commits, |out, commit| commit.put(out))?;
        put_bool(out, self.has_more);
        Ok(())
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            commits: get_list(decoder, CommitSummary::get)?,
            has_more: decoder.bool()?,
        })
    }
}

// --- Objects and leases ---

impl Wire for ObjectRequest {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_list(out, &self.objects, |out, id| {
            put_object(out, id);
            Ok(())
        })
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        let objects = get_list(decoder, get_object)?;
        if objects.is_empty() || objects.len() > MAX_PACK_OBJECTS {
            return Err(ProtocolError::Limit("object request objects"));
        }
        Ok(Self { objects })
    }
}

impl Wire for LeaseRequest {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_strs(out, &self.paths)?;
        put_str(out, &self.owner)?;
        put_u32(out, self.ttl);
        Ok(())
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            paths: get_strs(decoder)?,
            owner: get_string(decoder)?,
            ttl: decoder.u32()?,
        })
    }
}

impl Wire for LeasedPath {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_str(out, &self.path)?;
        put_str(out, &self.owner)?;
        out.extend_from_slice(&self.expires_at.to_be_bytes());
        Ok(())
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            path: get_string(decoder)?,
            owner: get_string(decoder)?,
            expires_at: i64::from_be_bytes(decoder.array()?),
        })
    }
}

impl Wire for Lease {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_str(out, &self.token)?;
        put_list(out, &self.locks, |out, lock| lock.put(out))
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            token: get_string(decoder)?,
            locks: get_list(decoder, LeasedPath::get)?,
        })
    }
}

impl Wire for Leases {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_list(out, &self.locks, |out, lock| lock.put(out))
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            locks: get_list(decoder, LeasedPath::get)?,
        })
    }
}

impl Wire for LeaseRenewal {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_str(out, &self.token)?;
        put_u32(out, self.ttl);
        Ok(())
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            token: get_string(decoder)?,
            ttl: decoder.u32()?,
        })
    }
}

impl Wire for LeaseToken {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        Ok(put_str(out, &self.token)?)
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            token: get_string(decoder)?,
        })
    }
}

impl Wire for LeasePaths {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_strs(out, &self.paths)
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            paths: get_strs(decoder)?,
        })
    }
}

impl Wire for Released {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_count(out, self.released)
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(Self {
            released: get_count(decoder)?,
        })
    }
}

// --- Refusals ---

/// A refused request's body (protocol.md, "Refusals").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    Invalid,
    Unauthenticated,
    Forbidden,
    NotFound,
    ObjectMissing,
    Checkpoint,
    Locked { path: String, owner: String },
    LockLost,
    Generation,
    OperationReused,
    ObjectsMissing(Vec<ObjectId>),
    UploadExpired,
    ServerOverloaded,
}

impl Refusal {
    pub fn code(&self) -> u8 {
        match self {
            Self::Invalid => 1,
            Self::Unauthenticated => 2,
            Self::Forbidden => 3,
            Self::NotFound => 4,
            Self::ObjectMissing => 5,
            Self::Checkpoint => 6,
            Self::Locked { .. } => 7,
            Self::LockLost => 8,
            Self::Generation => 9,
            Self::OperationReused => 10,
            Self::ObjectsMissing(_) => 11,
            Self::UploadExpired => 12,
            Self::ServerOverloaded => 13,
        }
    }
    pub fn http_status(&self) -> u16 {
        match self {
            Self::Invalid => 400,
            Self::Unauthenticated => 401,
            Self::Forbidden => 403,
            Self::NotFound | Self::ObjectMissing => 404,
            Self::Checkpoint
            | Self::Locked { .. }
            | Self::LockLost
            | Self::Generation
            | Self::OperationReused
            | Self::ObjectsMissing(_) => 409,
            Self::UploadExpired => 410,
            Self::ServerOverloaded => 503,
        }
    }
    pub fn name(&self) -> &'static str {
        match self {
            Self::Invalid => "INVALID",
            Self::Unauthenticated => "UNAUTHENTICATED",
            Self::Forbidden => "FORBIDDEN",
            Self::NotFound => "NOT_FOUND",
            Self::ObjectMissing => "OBJECT_MISSING",
            Self::Checkpoint => "CHECKPOINT",
            Self::Locked { .. } => "LOCKED",
            Self::LockLost => "LOCK_LOST",
            Self::Generation => "GENERATION",
            Self::OperationReused => "OPERATION_REUSED",
            Self::ObjectsMissing(_) => "OBJECTS_MISSING",
            Self::UploadExpired => "UPLOAD_EXPIRED",
            Self::ServerOverloaded => "SERVER_OVERLOADED",
        }
    }
}

impl Wire for Refusal {
    fn put(&self, out: &mut Vec<u8>) -> Result<()> {
        put_u8(out, self.code());
        match self {
            Self::Locked { path, owner } => {
                put_str(out, path)?;
                put_str(out, owner)?;
            }
            Self::ObjectsMissing(objects) => put_list(out, objects, |out, id| {
                put_object(out, id);
                Ok(())
            })?,
            _ => {}
        }
        Ok(())
    }
    fn get(decoder: &mut Decoder) -> Result<Self> {
        Ok(match decoder.u8()? {
            1 => Self::Invalid,
            2 => Self::Unauthenticated,
            3 => Self::Forbidden,
            4 => Self::NotFound,
            5 => Self::ObjectMissing,
            6 => Self::Checkpoint,
            7 => Self::Locked {
                path: get_string(decoder)?,
                owner: get_string(decoder)?,
            },
            8 => Self::LockLost,
            9 => Self::Generation,
            10 => Self::OperationReused,
            11 => Self::ObjectsMissing(get_list(decoder, get_object)?),
            12 => Self::UploadExpired,
            13 => Self::ServerOverloaded,
            _ => return Err(invalid("unknown refusal code")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode `bytes` as the vector type `kind` and encode the value again.
    /// Shapes that live only inside a message are read with a stand-in context.
    fn round_trip(kind: &str, bytes: &[u8]) -> Result<Vec<u8>> {
        fn whole<T: Wire>(bytes: &[u8]) -> Result<Vec<u8>> {
            encode(&decode::<T>(bytes)?)
        }
        let repo_id = RepoId::parse("019a2900-0000-7000-8000-000000000001").unwrap();
        let commit_id = commit_id_from_digest_bytes(&[0; 32]).unwrap();
        let mut decoder = Decoder::new(bytes);
        let mut out = Vec::new();
        let d = &mut decoder;
        match kind {
            "RepositoryIdentity" => return whole::<RepositoryIdentity>(bytes),
            "Cursor" => return whole::<Cursor>(bytes),
            "Capabilities" => return whole::<Capabilities>(bytes),
            "ServerPolicy" => return whole::<ServerPolicy>(bytes),
            "ProposalPage" => return whole::<ProposalPage>(bytes),
            "Record" => return whole::<ProposalRecord>(bytes),
            "PageAck" => return whole::<PageAck>(bytes),
            "SubmitRequest" => return whole::<SubmitRequest>(bytes),
            "SubmitResult" => return whole::<SubmitResult>(bytes),
            "Receipt" => return whole::<Receipt>(bytes),
            "HistoryRequest" => return whole::<HistoryRequest>(bytes),
            "HistoryPosition" => return whole::<HistoryPosition>(bytes),
            "HistoryPage" => return whole::<HistoryPage>(bytes),
            "StateRequest" => return whole::<StateRequest>(bytes),
            "StatePosition" => return whole::<StatePosition>(bytes),
            "StatePage" => return whole::<StatePage>(bytes),
            "StateEntry" => return whole::<StateEntry>(bytes),
            "ItemHistoriesRequest" => return whole::<ItemHistoriesRequest>(bytes),
            "ItemHistoriesPage" => return whole::<ItemHistoriesPage>(bytes),
            "CommitLogRequest" => return whole::<CommitLogRequest>(bytes),
            "CommitLogPage" => return whole::<CommitLogPage>(bytes),
            "CommitSummary" => return whole::<CommitSummary>(bytes),
            "ObjectRequest" => return whole::<ObjectRequest>(bytes),
            "LeaseRequest" => return whole::<LeaseRequest>(bytes),
            "Lease" => return whole::<Lease>(bytes),
            "LeasedPath" => return whole::<LeasedPath>(bytes),
            "Leases" => return whole::<Leases>(bytes),
            "LeaseRenewal" => return whole::<LeaseRenewal>(bytes),
            "LeaseToken" => return whole::<LeaseToken>(bytes),
            "LeasePaths" => return whole::<LeasePaths>(bytes),
            "Released" => return whole::<Released>(bytes),
            "Refusal" => return whole::<Refusal>(bytes),
            "PublishedHeader" => {
                put_published_header(&mut out, &get_published_header(d, &repo_id)?)?;
            }
            "Event" => {
                let row = get_event(d)?.row(&repo_id, &commit_id, 1, Timestamp::UNIX_EPOCH, 0);
                put_event(&mut out, &row)?;
            }
            "Fragment" => {
                let header = get_published_header(d, &repo_id)?;
                put_published_header(&mut out, &header)?;
                let events = get_list(d, |d| {
                    Ok(get_event(d)?.row(&repo_id, &commit_id, 1, Timestamp::UNIX_EPOCH, 0))
                })?;
                put_list(&mut out, &events, put_event)?;
            }
            "ItemEvent" => {
                let repo_seq = d.seq()?;
                let index = get_index(d)?;
                let row =
                    get_event(d)?.row(&repo_id, &commit_id, repo_seq, Timestamp::UNIX_EPOCH, index);
                put_item_event(&mut out, &row)?;
            }
            "ItemCommit" => {
                put_count(&mut out, get_count(d)?)?;
                put_commit_id(&mut out, &get_commit_id(d)?);
                put_str(&mut out, &get_string(d)?)?;
                put_time(&mut out, d.time()?)?;
                put_time(&mut out, d.time()?)?;
            }
            "ItemHistoryPage" => {
                put_item_history_page(&mut out, &get_item_history_page(d, &repo_id)?)?;
            }
            other => panic!("unknown vector type {other}"),
        }
        decoder.finish()?;
        Ok(out)
    }

    /// `sync-wire.json`: every valid message decodes and encodes to its exact
    /// bytes; every invalid one is refused. Object packs are a stream the
    /// transports read; their vectors are not checked here.
    #[test]
    fn wire_vectors() {
        let vector: serde_json::Value =
            serde_json::from_str(include_str!("../../../docs/spec/vectors/sync-wire.json"))
                .unwrap();
        for case in vector["valid"].as_array().unwrap() {
            let kind = case["type"].as_str().unwrap();
            if kind == "ObjectPack" {
                continue;
            }
            let bytes = hex::decode(case["bytes_hex"].as_str().unwrap()).unwrap();
            let again = round_trip(kind, &bytes).unwrap_or_else(|e| panic!("{kind}: {e}"));
            assert_eq!(hex::encode(again), case["bytes_hex"], "{kind}");
        }
        for case in vector["invalid"].as_array().unwrap() {
            let kind = case["type"].as_str().unwrap();
            if kind == "ObjectPack" {
                continue;
            }
            let bytes = hex::decode(case["bytes_hex"].as_str().unwrap()).unwrap();
            assert!(
                round_trip(kind, &bytes).is_err(),
                "{kind}: {}",
                case["reason"]
            );
        }
    }
}
