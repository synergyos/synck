//! Held work: a clone made with `--hold` keeps its commits until `push`.
//!
//! A held clone has its own copy of the origin's history, made when it was
//! cloned; content objects stay in the origin. Its commits append to the copy,
//! so `status`, `log`, `diff` and `restore` see them like any other. `pull`
//! replays the origin's newer commits into the copy and `push` replays the held
//! ones into the origin. A commit ID hashes neither the commit's sequence nor
//! its parents, so a replayed commit keeps its ID.
//!
//! Held commits are proposals: the origin decides. When the origin changed an
//! item a held commit changes too, `pull` keeps the held commits before the
//! first such one and withdraws that one and every later one: it records them
//! as withdrawn saves (`withdrawn.rs`), takes them out of the copy and leaves
//! their changes in the working files as changes not yet committed, merged
//! with the origin's like any other. Committing the result makes new proposals.

use super::*;
use synck_vcs::config::CoreConfig;
use synck_vcs::errors::CoreError;
use synck_vcs::repo::Repo;
use synck_vcs::storage::{Storage, StorageBackend as _};
use synck_vcs_sqlite::SqliteStorage;

pub(crate) const HELD_FILE: &str = "held.json";
/// Held commits taken out of the copy, until the working tree is based on
/// what remains.
const UNWIND_INTENT_NAME: &str = "unwind.json";
/// In the origin's `.synck`: one file per held clone naming the objects its
/// unpushed commits reference.
pub(crate) const PINS_DIRECTORY: &str = "pins";
const PAGE: usize = 256;
/// SQLite limits are signed.
const UNBOUNDED: usize = i64::MAX as usize;

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Held {
    /// The origin's commits through this sequence are in the copy.
    pub(crate) origin_seq: i64,
    /// The copy's commits through this sequence are in the origin.
    pub(crate) pushed_seq: i64,
    /// An HTTPS clone's verified position in the server's history.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) remote: Option<synck_sync::Cursor>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pin {
    pub(crate) clone: PathBuf,
    pub(crate) objects: BTreeSet<String>,
    /// Objects of withdrawn saves, kept until each expires.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) withdrawn: Vec<PinnedUntil>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PinnedUntil {
    pub(crate) until: DateTime<Utc>,
    pub(crate) objects: BTreeSet<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Unwind {
    commits: Vec<String>,
    /// The working tree's baseline in the copy without them.
    baseline: i64,
    held: Held,
}

/// What a push sent.
#[derive(Debug, Default)]
pub struct PushOutcome {
    pub pushed: Vec<String>,
}

pub(crate) fn read_held(metadata: &Path) -> Result<Held> {
    let path = metadata.join(HELD_FILE);
    let bytes = std::fs::read(&path).at("read", &path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn write_held(metadata: &Path, held: &Held) -> Result<()> {
    let path = metadata
        .join("tmp")
        .join(format!("held-{}.tmp", Uuid::new_v4()));
    std::fs::write(&path, serde_json::to_vec(held)?).at("write", &path)?;
    crate::repository::sync_file(&path)?;
    crate::repository::replace_file(&path, &metadata.join(HELD_FILE))
        .at("publish held state", &path)?;
    sync_directory(metadata)
}

/// `pins/<SHA-256 of the clone's root>.json` in the origin's store.
pub(crate) fn pin_path(store: &Path, clone: &Path) -> PathBuf {
    let name = hex::encode(Sha256::digest(clone.as_os_str().as_encoded_bytes()));
    store.join(PINS_DIRECTORY).join(format!("{name}.json"))
}

fn core(storage: SqliteStorage) -> Repo {
    Repo::new(
        Storage::new(std::sync::Arc::new(storage)),
        CoreConfig::default(),
    )
}

/// A history at `path` with no commits yet, for commits to be replayed into.
pub(crate) async fn empty_history(path: PathBuf, repo_id: &RepoId) -> Result<SqliteStorage> {
    let storage = SqliteStorage::new(path);
    storage
        .init_schema()
        .await
        .map_err(|error| LocalError::Integrity(error.to_string()))?;
    core(storage.clone())
        .sequence_repo_activity(repo_id.clone(), 1)
        .await?;
    storage
        .project_imported_history(repo_id)
        .await
        .map_err(|error| LocalError::Integrity(error.to_string()))?;
    Ok(storage)
}

pub(crate) fn is_conflict(error: &CoreError) -> bool {
    matches!(
        error,
        CoreError::BaseSeqMismatch { .. }
            | CoreError::RefBaseSeqMismatch { .. }
            | CoreError::SubtreeChanged { .. }
            | CoreError::StorageError(synck_vcs::errors::StorageError::UniqueViolation { .. })
    )
}

/// The proposal a stored commit was accepted from.
pub(crate) async fn stored_proposal(
    source: &Repo,
    repo_id: &RepoId,
    commit: &CommitRow,
) -> Result<(CommitProposal, Vec<RefEventProposal>)> {
    let count = usize::try_from(commit.event_count)
        .map_err(|_| LocalError::Integrity("negative commit event count".into()))?;
    let rows = source
        .list_commit_events(repo_id.clone(), commit.commit_id.clone(), count)
        .await?;
    proposal_from_rows(commit, rows)
}

/// The proposal `commit` was accepted from, given its stored events in order.
pub(crate) fn proposal_from_rows(
    commit: &CommitRow,
    rows: Vec<CommitEventRow>,
) -> Result<(CommitProposal, Vec<RefEventProposal>)> {
    let count = usize::try_from(commit.event_count)
        .map_err(|_| LocalError::Integrity("negative commit event count".into()))?;
    let mut events = Vec::new();
    let mut ref_events = Vec::new();
    for row in rows {
        match row {
            CommitEventRow::Item(row) => {
                let mut event: EventProposal = serde_json::from_slice(&row.payload)?;
                if let (Some(reference), Some(metadata)) =
                    (event.set_metadata.as_mut(), row.metadata)
                {
                    reference
                        .resolve(metadata)
                        .map_err(|error| LocalError::Integrity(error.to_string()))?;
                }
                events.push(event);
            }
            CommitEventRow::Ref(row) => {
                let mut event: RefEventProposal = serde_json::from_slice(&row.payload)?;
                if let (Some(reference), Some(metadata)) =
                    (event.set_metadata.as_mut(), row.metadata)
                {
                    reference
                        .resolve(metadata)
                        .map_err(|error| LocalError::Integrity(error.to_string()))?;
                }
                ref_events.push(event);
            }
        }
    }
    if events.len() + ref_events.len() != count {
        return Err(LocalError::Integrity(format!(
            "commit {} has missing events",
            commit.commit_id
        )));
    }
    Ok((
        CommitProposal {
            author_id: commit.author_id.clone(),
            committer_id: commit.committer_id.clone(),
            message: commit.message.clone(),
            occurred_at: commit.occurred_at,
            // Retries are found by commit ID before appending.
            operation_id: Some(commit.commit_id.to_string()),
            events,
        },
        ref_events,
    ))
}

/// Items the proposal changes that `target` has moved past: an item at
/// another head, or a folder it deletes that holds a live item the proposal
/// does not delete too.
pub(crate) async fn stale_items(
    target: &Repo,
    repo_id: &RepoId,
    events: &[EventProposal],
) -> Result<Vec<ItemId>> {
    let bases: BTreeMap<&ItemId, i64> = events
        .iter()
        .filter_map(|event| Some((event.item_id.as_ref()?, event.base_seq?)))
        .collect();
    let rows: HashMap<ItemId, synck_vcs::types::ItemRow> = target
        .get_items(
            repo_id.clone(),
            bases.keys().map(|id| (*id).clone()).collect(),
        )
        .await?
        .into_iter()
        .map(|row| (row.item_id.clone(), row))
        .collect();
    let mut stale: Vec<ItemId> = bases
        .iter()
        .filter(|(item_id, base)| rows.get(**item_id).is_none_or(|row| row.head_seq != **base))
        .map(|(item_id, _)| (*item_id).clone())
        .collect();
    let deleted: HashSet<&ItemId> = events
        .iter()
        .filter(|event| event.set_in_trash == Some(true))
        .filter_map(|event| event.item_id.as_ref())
        .collect();
    for root in &deleted {
        if rows
            .get(*root)
            .is_none_or(|row| row.item_type.as_str() != "FOLDER")
        {
            continue;
        }
        let descendants: Vec<ItemId> = target
            .list_non_tombstoned_descendants_bounded(repo_id.clone(), (*root).clone(), UNBOUNDED)
            .await?
            .into_iter()
            .map(|(item_id, _)| item_id)
            .filter(|item_id| !deleted.contains(item_id))
            .collect();
        for row in target.get_items(repo_id.clone(), descendants).await? {
            if row.lifecycle_state == LifecycleState::Live {
                stale.push(row.item_id);
            }
        }
    }
    Ok(stale)
}

/// Append and sequence `commit` in `target` with its ID, or return the items
/// `target` has moved past.
pub(crate) async fn apply(
    target: &Repo,
    repo_id: &RepoId,
    commit: &CommitRow,
    proposal: CommitProposal,
    ref_events: Vec<RefEventProposal>,
) -> Result<std::result::Result<(), Vec<ItemId>>> {
    let stale = stale_items(target, repo_id, &proposal.events).await?;
    if !stale.is_empty() {
        return Ok(Err(stale));
    }
    let items: Vec<ItemId> = proposal
        .events
        .iter()
        .filter_map(|event| event.item_id.clone())
        .collect();
    let accepted = match target
        .append_file_folder_checked_with_refs(repo_id.clone(), proposal, ref_events, &[], &[])
        .await
    {
        Ok(accepted) => accepted,
        Err(error) if is_conflict(&error) => return Ok(Err(items)),
        Err(error) => return Err(error.into()),
    };
    if accepted.commit_id != commit.commit_id {
        return Err(LocalError::Integrity(format!(
            "commit {} was replayed as {}",
            commit.commit_id, accepted.commit_id
        )));
    }
    while target.sequence_repo_activity(repo_id.clone(), PAGE).await? == PAGE {}
    Ok(Ok(()))
}

/// Append `source`'s commits after sequence `after` that `target` lacks, in
/// order and with their IDs. Returns the last source sequence replayed, or
/// the items of the first commit `target` cannot take.
async fn replay(
    source: &Repo,
    target: &Repo,
    repo_id: &RepoId,
    after: i64,
) -> Result<std::result::Result<i64, Vec<ItemId>>> {
    let mut last = after;
    loop {
        let commits = source
            .list_commits_by_repo_seq(repo_id.clone(), last + 1, None, PAGE)
            .await?;
        let done = commits.len() < PAGE;
        for commit in commits {
            let seq = commit
                .repo_seq
                .ok_or_else(|| LocalError::Integrity("unsequenced commit in history".into()))?;
            if target
                .get_commit(repo_id.clone(), commit.commit_id.clone())
                .await?
                .is_none()
            {
                let (proposal, ref_events) = stored_proposal(source, repo_id, &commit).await?;
                if let Err(items) = apply(target, repo_id, &commit, proposal, ref_events).await? {
                    return Ok(Err(items));
                }
            }
            last = seq;
        }
        if done {
            return Ok(Ok(last));
        }
    }
}

impl Repository {
    fn origin_core(&self) -> Repo {
        core(self.origin_storage())
    }

    /// Paths of `items` at this clone's HEAD, for a conflict report.
    pub(crate) async fn item_paths(&self, items: &[ItemId]) -> Result<Vec<String>> {
        let items: HashSet<&ItemId> = items.iter().collect();
        let mut paths: Vec<String> = self
            .build_committed_tree(true)
            .await?
            .into_iter()
            .filter(|(_, node)| items.contains(&node.item_id))
            .map(|(path, _)| path)
            .collect();
        if paths.is_empty() {
            paths = items.iter().map(|item_id| item_id.to_string()).collect();
        }
        Ok(paths)
    }

    /// Replay the origin's newer commits into this held clone's history, or
    /// return the items of the first one it cannot take. The working files
    /// take them in afterwards, as in any pull.
    pub(super) async fn take_in_origin(&self) -> Result<std::result::Result<(), Vec<ItemId>>> {
        let mut held = read_held(self.metadata_path())?;
        let head = self.head_repo_seq()?;
        let last = match replay(
            &self.origin_core(),
            self.core(),
            self.repository_id(),
            held.origin_seq,
        )
        .await?
        {
            Ok(last) => last,
            Err(items) => return Ok(Err(items)),
        };
        // Sequenced but not yet in the working files: the pull writes them.
        if self.head_repo_seq()? != head {
            self.leave_head();
            self.recover_current().await?;
        }
        if last != held.origin_seq {
            held.origin_seq = last;
            write_held(self.metadata_path(), &held)?;
        }
        Ok(Ok(()))
    }

    /// The origin moved `items` past the held commits. Withdraw the first held
    /// commit that changes one of them and every later one, keeping those
    /// before it, until the origin's newer commits replay. Returns the labels
    /// the withdrawn commits are recorded under.
    pub(super) async fn withdraw_held(&self, mut items: Vec<ItemId>) -> Result<Vec<String>> {
        let mut labels = Vec::new();
        loop {
            let commits = self.held_commits().await?.unwrap_or_default();
            if commits.is_empty() {
                return Err(LocalError::Integrity(
                    "the origin's history does not replay without held commits".into(),
                ));
            }
            let conflicting: HashSet<&ItemId> = items.iter().collect();
            let mut keep = commits.len() - 1;
            for (index, commit_id) in commits.iter().enumerate() {
                let commit_id = CommitId::parse(commit_id)
                    .map_err(|error| LocalError::Integrity(error.to_string()))?;
                let events = self
                    .core()
                    .list_events_for_commit(self.repository_id().clone(), commit_id, UNBOUNDED)
                    .await?;
                if events
                    .iter()
                    .any(|event| conflicting.contains(&event.item_id))
                {
                    keep = index;
                    break;
                }
            }
            labels.extend(self.unwind_held(&commits[keep..], keep > 0).await?);
            match self.take_in_items().await? {
                Ok(()) => return Ok(labels),
                Err(next) => items = next,
            }
        }
    }

    /// Record `withdrawn`, the last of the held commits, and take them out of
    /// this clone's history copy, leaving their changes in the working files,
    /// which are then based on the copy without them. `keeps` says whether
    /// held commits remain before them.
    async fn unwind_held(&self, withdrawn: &[String], keeps: bool) -> Result<Vec<String>> {
        let skip: HashSet<&str> = withdrawn.iter().map(String::as_str).collect();
        let held_ids: HashSet<String> = self
            .held_commits()
            .await?
            .unwrap_or_default()
            .into_iter()
            .collect();
        let saves = self.build_withdrawn(withdrawn).await?;
        let repo_id = self.repository_id();
        let directory = self
            .metadata_path()
            .join("tmp")
            .join(format!("unwind-{}", Uuid::new_v4()));
        std::fs::create_dir(&directory).at("create", &directory)?;
        let result = async {
            let storage = empty_history(directory.join("repository.sqlite3"), repo_id).await?;
            let target = core(storage.clone());
            let old_baseline = self.baseline();
            let mut baseline = None;
            let mut from = 1;
            loop {
                let page = self
                    .core()
                    .list_commits_by_repo_seq(repo_id.clone(), from, None, PAGE)
                    .await?;
                let done = page.len() < PAGE;
                for commit in page {
                    let seq = commit.repo_seq.ok_or_else(|| {
                        LocalError::Integrity("unsequenced commit in history".into())
                    })?;
                    from = seq + 1;
                    if seq > old_baseline && baseline.is_none() {
                        baseline = Some(crate::repository::head_repo_seq(&storage, repo_id)?);
                    }
                    if skip.contains(commit.commit_id.as_str()) {
                        continue;
                    }
                    let (mut proposal, ref_events) =
                        stored_proposal(self.core(), repo_id, &commit).await?;
                    if held_ids.contains(commit.commit_id.as_str()) {
                        // Kept as made: an HTTPS clone tells a sent commit by
                        // an operation ID equal to its commit ID.
                        proposal.operation_id = Some(self.commit_operation_id(&commit.commit_id)?);
                    }
                    if apply(&target, repo_id, &commit, proposal, ref_events)
                        .await?
                        .is_err()
                    {
                        return Err(LocalError::Integrity(format!(
                            "commit {} does not replay without the withdrawn commits",
                            commit.commit_id
                        )));
                    }
                }
                if done {
                    break;
                }
            }
            let head = crate::repository::head_repo_seq(&storage, repo_id)?;
            let held = read_held(self.metadata_path())?;
            let unwind = Unwind {
                commits: withdrawn.to_vec(),
                baseline: baseline.unwrap_or(head),
                // Commits through `pushed_seq` precede every held one, so they
                // keep their sequences.
                held: Held {
                    pushed_seq: if keeps { held.pushed_seq } else { head },
                    ..held
                },
            };
            rusqlite::Connection::open(storage.path())?.query_row(
                "PRAGMA journal_mode=WAL",
                [],
                |_| Ok(()),
            )?;
            self.write_json_intent(UNWIND_INTENT_NAME, "unwind", &unwind)?;
            // Recorded before the copy loses them; an unwind cut short forgets them.
            let labels = self.add_withdrawn(saves)?;
            // One transaction replaces the copy's content; the connections
            // this process holds read the new content from their next query.
            let source = rusqlite::Connection::open(storage.path())?;
            let mut destination = rusqlite::Connection::open(self.storage().path())?;
            let step = rusqlite::backup::Backup::new(&source, &mut destination)?.step(-1)?;
            if step != rusqlite::backup::StepResult::Done {
                return Err(LocalError::Integrity(format!(
                    "history copy was not replaced: {step:?}"
                )));
            }
            Ok((unwind, labels))
        }
        .await;
        let _ = std::fs::remove_dir_all(&directory);
        let (unwind, labels) = result?;
        self.finish_unwind(&unwind)?;
        self.recover_current().await?;
        if self.remote().is_none() {
            self.pin_held_objects(&[]).await?;
        }
        Ok(labels)
    }

    /// Base the working tree on the copy without the held commits. The file
    /// projection is rebuilt from the new copy at the next recovery.
    fn finish_unwind(&self, unwind: &Unwind) -> Result<()> {
        write_held(self.metadata_path(), &unwind.held)?;
        self.local_db()
            .open_writable()?
            .execute("DELETE FROM local_projection_state", [])?;
        self.advance_baseline(unwind.baseline)?;
        self.clear_intent(UNWIND_INTENT_NAME)
    }

    /// Finish an unwind cut short, before anything reads the projection.
    /// While a held commit is still in the copy, the copy was not replaced.
    pub(crate) async fn recover_pending_unwind(&self) -> Result<()> {
        let Some(unwind) = self.read_json_intent::<Unwind>(UNWIND_INTENT_NAME, "unwind")? else {
            return Ok(());
        };
        for commit_id in &unwind.commits {
            let commit_id = CommitId::parse(commit_id)
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
            if self
                .core()
                .get_commit(self.repository_id().clone(), commit_id)
                .await?
                .is_some()
            {
                self.forget_withdrawn(&unwind.commits)?;
                return self.clear_intent(UNWIND_INTENT_NAME);
            }
        }
        self.finish_unwind(&unwind)
    }

    /// A held clone's commits that the origin does not have, oldest first;
    /// `None` in any other working tree.
    pub async fn held_commits(&self) -> Result<Option<Vec<String>>> {
        if !self.held() {
            return Ok(None);
        }
        let held = read_held(self.metadata_path())?;
        let origin = self.origin_core();
        let repo_id = self.repository_id();
        let mut commits = Vec::new();
        let mut from = held.pushed_seq + 1;
        loop {
            let page = self
                .core()
                .list_commits_by_repo_seq(repo_id.clone(), from, None, PAGE)
                .await?;
            let done = page.len() < PAGE;
            for commit in page {
                from = commit.repo_seq.unwrap_or(from) + 1;
                let sent = if self.remote().is_some() {
                    // Commits taken in from the server were appended with
                    // their commit ID as operation ID; local ones never are.
                    self.core()
                        .get_commit_by_operation_id(repo_id.clone(), commit.commit_id.as_str())
                        .await?
                        .is_some_and(|replayed| replayed.commit_id == commit.commit_id)
                } else {
                    origin
                        .get_commit(repo_id.clone(), commit.commit_id.clone())
                        .await?
                        .is_some()
                };
                if !sent {
                    commits.push(commit.commit_id.to_string());
                }
            }
            if done {
                return Ok(Some(commits));
            }
        }
    }

    /// Send the held commits to the origin, oldest first, after taking in the
    /// origin's newer ones. Each keeps its commit ID.
    pub async fn push(&self) -> Result<PushOutcome> {
        if !self.held() {
            return Ok(PushOutcome::default());
        }
        if self.remote().is_some() {
            return self.push_remote().await;
        }
        let pulled = self.pull().await?;
        if !pulled.unwound.is_empty() {
            return Err(LocalError::HeldMerged {
                paths: pulled.unwound,
                withdrawn: pulled.withdrawn,
            });
        }
        let pushed = self.held_commits().await?.unwrap_or_default();
        // A reservation from any working tree refuses these commits as it
        // would have refused them unheld.
        let mut items = Vec::new();
        for commit_id in &pushed {
            let commit_id = CommitId::parse(commit_id)
                .map_err(|error| LocalError::Integrity(error.to_string()))?;
            for event in self
                .core()
                .list_events_for_commit(self.repository_id().clone(), commit_id, UNBOUNDED)
                .await?
            {
                items.push(event.item_id);
            }
        }
        if !items.is_empty() {
            let paths = self.item_paths(&items).await?;
            self.guard_agent_paths(&paths.iter().map(String::as_str).collect::<Vec<_>>())?;
        }
        let mut held = read_held(self.metadata_path())?;
        let origin = self.origin_core();
        // The origin's lock is held, so nothing has reached it since the pull.
        if let Err(items) =
            replay(self.core(), &origin, self.repository_id(), held.pushed_seq).await?
        {
            return Err(LocalError::HeldConflict {
                paths: self.item_paths(&items).await?,
            });
        }
        held.pushed_seq = self.head_repo_seq()?;
        held.origin_seq =
            crate::repository::head_repo_seq(&self.origin_storage(), self.repository_id())?;
        write_held(self.metadata_path(), &held)?;
        self.pin_held_objects(&[]).await?;
        Ok(PushOutcome { pushed })
    }

    /// Objects of held commits are in the origin's store, which `gc` empties
    /// of everything its own history does not reference. Pin them, and
    /// `extra` about to be committed, before the commit that uses them.
    pub(super) async fn pin_held_objects(&self, extra: &[&ObjectId]) -> Result<()> {
        let held = read_held(self.metadata_path())?;
        let mut objects: BTreeSet<String> = extra
            .iter()
            .map(|id| id.as_str()[7..].to_string())
            .collect();
        let mut cursor = Some(RepoEventCursor {
            repo_seq: held.pushed_seq,
            commit_event_idx: i32::MAX,
        });
        loop {
            let page = self
                .core()
                .list_repo_commit_events(self.repository_id().clone(), cursor.clone(), PAGE)
                .await?;
            let done = page.len() < PAGE;
            for row in &page {
                let references = synck_items::references::commit_event_references(row)
                    .map_err(|error| LocalError::Integrity(error.to_string()))?;
                objects.extend(
                    references
                        .objects
                        .iter()
                        .map(|id| id.as_str()[7..].to_string()),
                );
                cursor = Some(RepoEventCursor {
                    repo_seq: row.repo_seq().unwrap_or(i64::MAX),
                    commit_event_idx: row.commit_event_idx(),
                });
            }
            if done {
                break;
            }
        }
        if self
            .held_commits()
            .await?
            .is_none_or(|commits| commits.is_empty())
            && extra.is_empty()
        {
            objects.clear();
        }
        let withdrawn = self.withdrawn_pins()?;
        let path = pin_path(self.store_path(), self.root_path());
        if objects.is_empty() && withdrawn.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(LocalError::Io {
                    action: "remove held-object pin",
                    path,
                    source: error,
                }),
                _ => Ok(()),
            };
        }
        let pins = self.store_path().join(PINS_DIRECTORY);
        match std::fs::create_dir(&pins) {
            Err(error) if error.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(LocalError::Io {
                    action: "create pins directory",
                    path: pins,
                    source: error,
                });
            }
            _ => {}
        }
        let pin = Pin {
            clone: self.root_path().to_path_buf(),
            objects,
            withdrawn,
        };
        let temporary = self
            .store_path()
            .join("tmp")
            .join(format!("pin-{}.tmp", Uuid::new_v4()));
        std::fs::write(&temporary, serde_json::to_vec(&pin)?).at("write", &temporary)?;
        crate::repository::sync_file(&temporary)?;
        crate::repository::replace_file(&temporary, &path).at("publish held-object pin", &path)?;
        sync_directory(&pins)
    }
}
