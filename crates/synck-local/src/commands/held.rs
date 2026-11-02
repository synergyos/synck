//! Held work: a clone made with `--hold` keeps its commits until `push`.
//!
//! A held clone has its own copy of the origin's history, made when it was
//! cloned; content objects stay in the origin. Its commits append to the copy,
//! so `status`, `log`, `diff` and `restore` see them like any other. `pull`
//! replays the origin's newer commits into the copy and `push` replays the held
//! ones into the origin. A commit ID hashes neither the commit's sequence nor
//! its parents, so a replayed commit keeps its ID. A commit that changes an
//! item the other side changed too is refused, and nothing moves.

use super::*;
use synck_vcs::config::CoreConfig;
use synck_vcs::errors::CoreError;
use synck_vcs::repo::Repo;
use synck_vcs::storage::Storage;
use synck_vcs_sqlite::SqliteStorage;

pub(crate) const HELD_FILE: &str = "held.json";
/// In the origin's `.synck`: one file per held clone naming the objects its
/// unpushed commits reference.
pub(crate) const PINS_DIRECTORY: &str = "pins";
const PAGE: usize = 256;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Held {
    /// The origin's commits through this sequence are in the copy.
    pub(crate) origin_seq: i64,
    /// The copy's commits through this sequence are in the origin.
    pub(crate) pushed_seq: i64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pin {
    pub(crate) clone: PathBuf,
    pub(crate) objects: BTreeSet<String>,
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
    let path = metadata.join("tmp").join(format!("held-{}.tmp", Uuid::new_v4()));
    std::fs::write(&path, serde_json::to_vec(held)?).at("write", &path)?;
    crate::repository::sync_file(&path)?;
    crate::repository::replace_file(&path, &metadata.join(HELD_FILE)).at("publish held state", &path)?;
    sync_directory(metadata)
}

/// `pins/<SHA-256 of the clone's root>.json` in the origin's store.
pub(crate) fn pin_path(store: &Path, clone: &Path) -> PathBuf {
    let name = hex::encode(Sha256::digest(clone.as_os_str().as_encoded_bytes()));
    store.join(PINS_DIRECTORY).join(format!("{name}.json"))
}

fn core(storage: SqliteStorage) -> Repo {
    Repo::new(Storage::new(std::sync::Arc::new(storage)), CoreConfig::default())
}

fn is_conflict(error: &CoreError) -> bool {
    matches!(
        error,
        CoreError::BaseSeqMismatch { .. }
            | CoreError::RefBaseSeqMismatch { .. }
            | CoreError::SubtreeChanged { .. }
            | CoreError::StorageError(synck_vcs::errors::StorageError::UniqueViolation { .. })
    )
}

/// The proposal a stored commit was accepted from.
async fn stored_proposal(
    source: &Repo,
    repo_id: &RepoId,
    commit: &CommitRow,
) -> Result<(CommitProposal, Vec<RefEventProposal>)> {
    let count = usize::try_from(commit.event_count)
        .map_err(|_| LocalError::Integrity("negative commit event count".into()))?;
    let mut events = Vec::new();
    let mut ref_events = Vec::new();
    for row in source
        .list_commit_events(repo_id.clone(), commit.commit_id.clone(), count)
        .await?
    {
        match row {
            CommitEventRow::Item(row) => {
                let mut event: EventProposal = serde_json::from_slice(&row.payload)?;
                if let (Some(reference), Some(metadata)) = (event.set_metadata.as_mut(), row.metadata) {
                    reference.resolve(metadata).map_err(|error| LocalError::Integrity(error.to_string()))?;
                }
                events.push(event);
            }
            CommitEventRow::Ref(row) => {
                let mut event: RefEventProposal = serde_json::from_slice(&row.payload)?;
                if let (Some(reference), Some(metadata)) = (event.set_metadata.as_mut(), row.metadata) {
                    reference.resolve(metadata).map_err(|error| LocalError::Integrity(error.to_string()))?;
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
async fn stale_items(target: &Repo, repo_id: &RepoId, events: &[EventProposal]) -> Result<Vec<ItemId>> {
    let bases: BTreeMap<&ItemId, i64> = events
        .iter()
        .filter_map(|event| Some((event.item_id.as_ref()?, event.base_seq?)))
        .collect();
    let rows: HashMap<ItemId, synck_vcs::types::ItemRow> = target
        .get_items(repo_id.clone(), bases.keys().map(|id| (*id).clone()).collect())
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
        if rows.get(*root).is_none_or(|row| row.item_type.as_str() != "FOLDER") {
            continue;
        }
        let descendants: Vec<ItemId> = target
            .list_non_tombstoned_descendants_bounded(repo_id.clone(), (*root).clone(), usize::MAX)
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
            if target.get_commit(repo_id.clone(), commit.commit_id.clone()).await?.is_none() {
                let (proposal, ref_events) = stored_proposal(source, repo_id, &commit).await?;
                let stale = stale_items(target, repo_id, &proposal.events).await?;
                if !stale.is_empty() {
                    return Ok(Err(stale));
                }
                let items: Vec<ItemId> = proposal.events.iter().filter_map(|event| event.item_id.clone()).collect();
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
    async fn item_paths(&self, items: &[ItemId]) -> Result<Vec<String>> {
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

    /// Replay the origin's newer commits into this held clone's history. The
    /// working files take them in afterwards, as in any pull.
    pub(super) async fn take_in_origin(&self) -> Result<()> {
        let mut held = read_held(self.metadata_path())?;
        let head = self.head_repo_seq()?;
        let last = match replay(&self.origin_core(), self.core(), self.repository_id(), held.origin_seq).await? {
            Ok(last) => last,
            Err(items) => {
                return Err(LocalError::ChangedOnBothSides {
                    paths: self.item_paths(&items).await?,
                });
            }
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
        Ok(())
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
            let page = self.core().list_commits_by_repo_seq(repo_id.clone(), from, None, PAGE).await?;
            let done = page.len() < PAGE;
            for commit in page {
                from = commit.repo_seq.unwrap_or(from) + 1;
                if origin.get_commit(repo_id.clone(), commit.commit_id.clone()).await?.is_none() {
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
        self.pull().await?;
        let pushed = self.held_commits().await?.unwrap_or_default();
        let mut held = read_held(self.metadata_path())?;
        let origin = self.origin_core();
        // The origin's lock is held, so nothing has reached it since the pull.
        if let Err(items) = replay(self.core(), &origin, self.repository_id(), held.pushed_seq).await? {
            return Err(LocalError::ChangedOnBothSides {
                paths: self.item_paths(&items).await?,
            });
        }
        held.pushed_seq = self.head_repo_seq()?;
        held.origin_seq = crate::repository::head_repo_seq(&self.origin_storage(), self.repository_id())?;
        write_held(self.metadata_path(), &held)?;
        self.pin_held_objects(&[]).await?;
        Ok(PushOutcome { pushed })
    }

    /// Objects of held commits are in the origin's store, which `gc` empties
    /// of everything its own history does not reference. Pin them, and
    /// `extra` about to be committed, before the commit that uses them.
    pub(super) async fn pin_held_objects(&self, extra: &[&ObjectId]) -> Result<()> {
        let held = read_held(self.metadata_path())?;
        let mut objects: BTreeSet<String> = extra.iter().map(|id| id.as_str()[7..].to_string()).collect();
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
                objects.extend(references.objects.iter().map(|id| id.as_str()[7..].to_string()));
                cursor = Some(RepoEventCursor {
                    repo_seq: row.repo_seq().unwrap_or(i64::MAX),
                    commit_event_idx: row.commit_event_idx(),
                });
            }
            if done {
                break;
            }
        }
        if self.held_commits().await?.is_none_or(|commits| commits.is_empty()) && extra.is_empty() {
            objects.clear();
        }
        let path = pin_path(self.store_path(), self.root_path());
        if objects.is_empty() {
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
                return Err(LocalError::Io { action: "create pins directory", path: pins, source: error });
            }
            _ => {}
        }
        let pin = Pin {
            clone: self.root_path().to_path_buf(),
            objects,
        };
        let temporary = self.store_path().join("tmp").join(format!("pin-{}.tmp", Uuid::new_v4()));
        std::fs::write(&temporary, serde_json::to_vec(&pin)?).at("write", &temporary)?;
        crate::repository::sync_file(&temporary)?;
        crate::repository::replace_file(&temporary, &path).at("publish held-object pin", &path)?;
        sync_directory(&pins)
    }
}
