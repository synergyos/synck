//! `synck pull`: bring commits made from other working trees into this one.
//!
//! A working tree is based on one repository sequence, its baseline. Pull
//! compares three trees: the baseline's, HEAD's and the working files. An item
//! changed only in history is written; an item changed only here is left
//! alone. A file whose content changed on both sides is merged: text
//! three-way, with conflict markers where both sides changed the same lines;
//! anything else keeps this tree's version and writes the repository's beside
//! it as `<name>.~theirs.<ext>`. Then the baseline is HEAD, and merged files
//! are changes here like any other. Any other change on both sides, such as a
//! move against an edit, is reported and nothing is written.
//!
//! `conflicts.json` records each merge before its file is written, so a pull
//! repeated after an interruption keeps the merged file instead of merging it
//! again. Clean merges are dropped when the pull completes; conflicted ones
//! stay until `synck resolve`, and `commit` refuses to run while any do.

use super::*;
use crate::comparison::{FileBytes, TEXT_DIFF_MAX_BYTES};

pub(super) const PULL_INTENT_VERSION: u32 = 1;
pub(super) const PULL_INTENT_NAME: &str = "pull.json";
pub(super) const PULL_STAGE_NAME: &str = "pull-stage";
pub(super) const CONFLICTS_FILE: &str = "conflicts.json";

/// A file a pull merged.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Merged {
    pub(super) path: String,
    pub(super) item_id: ItemId,
    /// The repository's head of the item that this merge took in.
    pub(super) head_seq: i64,
    /// SHA-256 of the working file as the merge left it.
    pub(super) digest: String,
    /// Where the repository's version was written, when the file is not text.
    pub(super) theirs: Option<String>,
    /// Both sides changed the same lines, or the file is not text.
    pub(super) conflicted: bool,
}

/// An unresolved conflict, for `status`.
#[derive(Debug, Serialize)]
pub struct Conflict {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
}

enum Resolution {
    Text { bytes: Vec<u8>, conflicted: bool },
    Both { theirs: String },
}

struct PullPlan {
    items: Vec<RevertIntentItem>,
    merges: Vec<String>,
    conflicts: Vec<String>,
}

fn read_conflicts(metadata: &Path) -> Result<Vec<Merged>> {
    let path = metadata.join(CONFLICTS_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(LocalError::Io { action: "read", path, source }),
    }
}

fn write_conflicts(metadata: &Path, records: &[Merged]) -> Result<()> {
    let target = metadata.join(CONFLICTS_FILE);
    if records.is_empty() {
        return match std::fs::remove_file(&target) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(LocalError::Io { action: "remove", path: target, source: error })
            }
            _ => sync_directory(metadata),
        };
    }
    let path = metadata.join("tmp").join(format!("conflicts-{}.tmp", Uuid::new_v4()));
    std::fs::write(&path, serde_json::to_vec(records)?).at("write", &path)?;
    crate::repository::sync_file(&path)?;
    crate::repository::replace_file(&path, &target).at("publish conflicts", &target)?;
    sync_directory(metadata)
}

fn text(bytes: FileBytes) -> Option<String> {
    match bytes {
        FileBytes::Bytes(bytes) => String::from_utf8(bytes).ok().filter(|text| !text.contains('\0')),
        FileBytes::TooLarge => None,
    }
}

const CONFLICT_MARKERS: [&str; 2] = ["<<<<<<< ours", ">>>>>>> theirs"];

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PullIntent {
    pub(super) version: u32,
    pub(super) baseline: i64,
    pub(super) target_repo_seq: i64,
    pub(super) items: Vec<RevertIntentItem>,
}

/// What a pull wrote.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct PullOutcome {
    pub from_repo_seq: i64,
    pub to_repo_seq: i64,
    /// Working paths written, moved, removed or merged.
    pub updated: Vec<String>,
    /// Files left with conflicts to resolve.
    pub conflicts: Vec<String>,
    /// Paths that held commits changed and the origin changed too: the held
    /// commits were taken back into the working files and merged.
    pub unwound: Vec<String>,
}

fn item_states(
    tree: &BTreeMap<String, CommittedNode>,
) -> Result<HashMap<ItemId, (&String, RevertItemState)>> {
    tree.iter()
        .map(|(path, node)| {
            Ok((
                node.item_id.clone(),
                (path, committed_restore_state(path, node, tree)?),
            ))
        })
        .collect()
}

fn same_state(left: Option<&RevertItemState>, right: Option<&RevertItemState>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => same_revert_value(left, right),
        _ => false,
    }
}

fn under(path: &str, ancestor: &str) -> bool {
    path.strip_prefix(ancestor).is_some_and(|rest| rest.starts_with('/'))
}

/// The writes that take `working` from `base` to `head` without touching a
/// local change, the files to merge when `merge` is set, and every other path
/// changed on both sides. Items in `settled` were merged already.
fn plan_pull(
    base: &BTreeMap<String, CommittedNode>,
    head: &BTreeMap<String, CommittedNode>,
    working: &BTreeMap<String, WorkingNode>,
    working_ids: &HashMap<String, ItemId>,
    merge: bool,
    settled: &HashSet<ItemId>,
) -> Result<PullPlan> {
    let base_states = item_states(base)?;
    let head_states = item_states(head)?;
    let local = build_worktree_restore_items(base, working, working_ids)?;
    let local_ids: HashSet<&ItemId> = local
        .iter()
        .filter(|item| item.before.is_some())
        .map(|item| &item.item_id)
        .collect();
    // Working paths this pull must not move or remove: local changes and additions.
    let local_paths: Vec<&str> = local
        .iter()
        .filter(|item| item.after.live)
        .map(|item| item.after.path.as_str())
        .collect();
    let incoming: HashSet<&ItemId> = base_states
        .keys()
        .chain(head_states.keys())
        .filter(|item_id| {
            !same_state(
                base_states.get(*item_id).map(|(_, state)| state),
                head_states.get(*item_id).map(|(_, state)| state),
            )
        })
        .collect();

    let head_ids = target_working_ids(head, working_ids.clone());
    let mut conflicts = BTreeSet::new();
    let mut merges = Vec::new();
    let mut items = Vec::new();
    for item in build_worktree_restore_items(head, working, &head_ids)? {
        if item.before.is_some() {
            if !incoming.contains(&item.item_id) || settled.contains(&item.item_id) {
                continue;
            }
            let before = item.before.as_ref().expect("checked");
            let path = &before.path;
            let file = item.item_type == "FILE";
            if local_ids.contains(&item.item_id) {
                // Content changed on both sides, in place: merge it.
                if merge
                    && file
                    && before.live
                    && item.after.live
                    && item.after.path == *path
                    && base.get(path).is_some_and(|node| node.item_id == item.item_id)
                {
                    merges.push(path.clone());
                } else {
                    conflicts.insert(item.after.path.clone());
                }
                continue;
            }
            // Added here too: nothing to write if the same, else merge it.
            if !item.after.live
                && !working_ids.contains_key(path)
                && let Some(node) = working.get(path)
            {
                if !working_restore_state(path, node, before.head_seq, &head_ids)
                    .is_ok_and(|local| same_revert_value(&local, before))
                {
                    if merge && file && node.kind == NodeKind::File {
                        merges.push(path.clone());
                    } else {
                        conflicts.insert(path.clone());
                    }
                }
                continue;
            }
            // Its new place must not hold or sit inside a local change.
            if let Some(local) = local_paths
                .iter()
                .find(|local| **local != item.after.path && (*local == path || under(path, local)))
            {
                conflicts.insert((*local).to_string());
                continue;
            }
            items.push(item);
            continue;
        }
        // A working path HEAD lacks: a local addition, or an item HEAD deleted.
        let Some(item_id) = working_ids.get(&item.after.path) else {
            continue;
        };
        if !incoming.contains(item_id) {
            continue;
        }
        if local_ids.contains(item_id) {
            conflicts.insert(item.after.path.clone());
            continue;
        }
        if let Some(local) = local_paths
            .iter()
            .find(|local| under(local, &item.after.path))
        {
            conflicts.insert((*local).to_string());
            continue;
        }
        items.push(item);
    }
    // Each written item needs its folder: in place already, or written too.
    // A folder moved or deleted here leaves history's addition no place.
    let targets: HashSet<&str> = items
        .iter()
        .filter_map(|item| item.before.as_ref().filter(|state| state.live))
        .map(|state| state.path.as_str())
        .collect();
    for state in items.iter().filter_map(|item| item.before.as_ref().filter(|state| state.live)) {
        if let Some((parent, _)) = state.path.rsplit_once('/')
            && !targets.contains(parent)
            && working_ids.get(parent) != head.get(parent).map(|node| &node.item_id)
        {
            conflicts.insert(state.path.clone());
        }
    }
    items.sort_by(|left, right| left.item_id.cmp(&right.item_id));
    Ok(PullPlan {
        items,
        merges,
        conflicts: conflicts.into_iter().collect(),
    })
}

impl Repository {
    /// Bring this working tree to HEAD, keeping every local change and
    /// merging files changed on both sides.
    pub async fn pull(&self) -> Result<PullOutcome> {
        self.pull_with(true).await
    }

    /// Writers build on HEAD. A working tree that is behind takes in the other
    /// working trees' commits first, or stops at an item changed on both sides:
    /// merging is for `pull`, where the result is looked at before a commit.
    pub(crate) async fn pull_if_behind(&self) -> Result<()> {
        // An HTTPS clone that sends at once builds on the server's HEAD when
        // it can reach it, and on its own when it cannot.
        if self.sends_at_once() {
            match self.take_in_remote().await {
                Err(LocalError::Unreachable(_)) => {}
                result => result?,
            }
        }
        if !self.at_head() {
            self.pull_with(false).await?;
        }
        Ok(())
    }

    async fn pull_with(&self, merge: bool) -> Result<PullOutcome> {
        self.guard_agent_all()?;
        self.recover_pending_pull().await?;
        let mut unwound = Vec::new();
        // A writer's own fetch from a server is in `pull_if_behind`.
        if self.held() && (merge || self.remote().is_none()) {
            match self.take_in().await {
                Err(LocalError::HeldConflict { paths }) if merge => {
                    self.unwind_held().await?;
                    self.take_in().await?;
                    unwound = paths;
                }
                result => result?,
            }
        }
        let from = self.baseline();
        if self.at_head() {
            return Ok(PullOutcome {
                from_repo_seq: from,
                to_repo_seq: from,
                updated: Vec::new(),
                conflicts: self.conflicts()?.into_iter().map(|conflict| conflict.path).collect(),
                unwound,
            });
        }
        let (base, working, _, working_ids) = self.changes().await?;
        let head = self.build_committed_tree(true).await?;
        let target = self.head_repo_seq()?;
        let mut records = read_conflicts(self.metadata_path())?;
        let settled: HashSet<ItemId> = records
            .iter()
            .filter(|record| {
                head.get(&record.path)
                    .is_some_and(|node| node.item_id == record.item_id && node.head_seq == record.head_seq)
                    && working.get(&record.path).and_then(|node| node.digest_hex.as_deref())
                        == Some(record.digest.as_str())
            })
            .map(|record| record.item_id.clone())
            .collect();
        let plan = plan_pull(&base, &head, &working, &working_ids, merge, &settled)?;
        if !plan.conflicts.is_empty() {
            return Err(if merge {
                LocalError::CannotMerge { paths: plan.conflicts }
            } else {
                LocalError::ChangedOnBothSides { paths: plan.conflicts }
            });
        }
        let items = plan.items;
        reject_ignored_restore_descendants(self.root(), &head, &items, &working)?;
        self.validate_inverse_objects(&items)?;
        self.validate_inverse_destinations(&items)?;

        // Merge before anything else moves: a merged file is then a change
        // here, which the rest of the pull keeps.
        let mut resolutions = Vec::with_capacity(plan.merges.len());
        for path in &plan.merges {
            let theirs = &head[path];
            let resolution = self.resolve_both(path, base.get(path), theirs, &working[path])?;
            let (digest, theirs_path, conflicted) = match &resolution {
                Resolution::Text { bytes, conflicted } => (hex::encode(Sha256::digest(bytes)), None, *conflicted),
                Resolution::Both { theirs } => (
                    working[path].digest_hex.clone().expect("scanned FILE has digest"),
                    Some(theirs.clone()),
                    true,
                ),
            };
            records.retain(|record| record.path != *path);
            records.push(Merged {
                path: path.clone(),
                item_id: theirs.item_id.clone(),
                head_seq: theirs.head_seq,
                digest,
                theirs: theirs_path,
                conflicted,
            });
            resolutions.push((path, theirs, resolution));
        }
        if !resolutions.is_empty() {
            write_conflicts(self.metadata_path(), &records)?;
        }
        for (path, theirs, resolution) in resolutions {
            match resolution {
                Resolution::Text { bytes, .. } => self.replace_working_bytes(path, &bytes)?,
                Resolution::Both { theirs: copy } => self.write_working_file(
                    &copy,
                    theirs.blob_id.as_ref().expect("FILE has an object"),
                    theirs.size.expect("FILE has a size"),
                    false,
                )?,
            }
        }

        let updated = items
            .iter()
            .map(|item| {
                item.before
                    .as_ref()
                    .filter(|state| state.live)
                    .unwrap_or(&item.after)
                    .path
                    .clone()
            })
            .chain(plan.merges.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let intent = PullIntent {
            version: PULL_INTENT_VERSION,
            baseline: from,
            target_repo_seq: target,
            items,
        };
        if !intent.items.is_empty() {
            self.write_json_intent(PULL_INTENT_NAME, "pull", &intent)?;
        }
        self.complete_pull(&intent).await?;
        Ok(PullOutcome {
            from_repo_seq: from,
            to_repo_seq: target,
            updated,
            conflicts: self.conflicts()?.into_iter().map(|conflict| conflict.path).collect(),
            unwound,
        })
    }

    /// Take in the origin's or the server's newer commits.
    async fn take_in(&self) -> Result<()> {
        if self.remote().is_some() { self.take_in_remote().await } else { self.take_in_origin().await }
    }

    /// Merge one file changed on both sides. `base` is absent when both sides
    /// added it.
    fn resolve_both(
        &self,
        path: &str,
        base: Option<&CommittedNode>,
        theirs: &CommittedNode,
        mine: &WorkingNode,
    ) -> Result<Resolution> {
        let small = |size: Option<i64>| size.is_some_and(|size| size as u64 <= TEXT_DIFF_MAX_BYTES);
        if small(theirs.size)
            && base.is_none_or(|node| small(node.size))
            && mine.size.is_some_and(|size| size <= TEXT_DIFF_MAX_BYTES)
        {
            let objects = self.blob_reader()?;
            let base_text = match base {
                Some(node) => text(self.read_object_bytes(&objects, node)?),
                None => Some(String::new()),
            };
            if let (Some(base), Some(mine), Some(theirs)) = (
                base_text,
                text(self.read_working_bytes(path, mine)?),
                text(self.read_object_bytes(&objects, theirs)?),
            ) {
                let mut options = diffy::MergeOptions::new();
                options.set_conflict_style(diffy::ConflictStyle::Merge);
                return Ok(match options.merge(&base, &mine, &theirs) {
                    Ok(merged) => Resolution::Text { bytes: merged.into_bytes(), conflicted: false },
                    Err(marked) => Resolution::Text { bytes: marked.into_bytes(), conflicted: true },
                });
            }
        }
        Ok(Resolution::Both { theirs: self.theirs_path(path)? })
    }

    /// `<stem>.~theirs.<ext>` beside `path`, numbered when taken.
    fn theirs_path(&self, path: &str) -> Result<String> {
        let (folder, name) = match path.rsplit_once('/') {
            Some((folder, name)) => (Some(folder), name),
            None => (None, path),
        };
        let (stem, extension) = match name.rsplit_once('.') {
            Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
            _ => (name, None),
        };
        for number in 1..=100 {
            let tag = if number == 1 { "~theirs".to_string() } else { format!("~theirs-{number}") };
            let name = match extension {
                Some(extension) => format!("{stem}.{tag}.{extension}"),
                None => format!("{stem}.{tag}"),
            };
            let candidate = match folder {
                Some(folder) => format!("{folder}/{name}"),
                None => name,
            };
            if std::fs::symlink_metadata(self.root_path().join(display_to_path(&candidate))).is_err() {
                return Ok(candidate);
            }
        }
        Err(LocalError::Invalid(format!("no free name beside {path} for the repository's version")))
    }

    /// Replace a working file with `bytes`, atomically.
    fn replace_working_bytes(&self, display: &str, bytes: &[u8]) -> Result<()> {
        let relative = display_to_path(display);
        let parent = match relative.parent() {
            Some(path) if !path.as_os_str().is_empty() => path,
            _ => Path::new("."),
        };
        let name = relative
            .file_name()
            .ok_or_else(|| LocalError::Invalid(format!("invalid merge path: {display}")))?;
        let parent_dir = self.root().open_dir_nofollow(parent).at("open merge parent", parent)?;
        let temporary = format!(".synck-tmp-{}", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options.create_new(true).write(true).follow(FollowSymlinks::No);
        let mut file = parent_dir.open_with(&temporary, &options).at("create temporary file", display)?;
        if let Err(error) = file
            .write_all(bytes)
            .and_then(|()| sync_ordered(&file))
            .at("write merged file", display)
        {
            parent_dir.remove_file(&temporary).at("remove temporary file", display)?;
            return Err(error);
        }
        self.publish_temporary_file(&parent_dir, &self.root_path().join(parent), &temporary, name, Path::new(display), true)
    }

    /// Files a pull merged with conflicts, or kept both versions of.
    pub fn conflicts(&self) -> Result<Vec<Conflict>> {
        Ok(read_conflicts(self.metadata_path())?
            .into_iter()
            .filter(|record| record.conflicted)
            .map(|record| Conflict { path: record.path, theirs: record.theirs })
            .collect())
    }

    /// A commit waits until every conflict is resolved.
    pub(crate) fn reject_unresolved(&self) -> Result<()> {
        let paths: Vec<String> = self.conflicts()?.into_iter().map(|conflict| conflict.path).collect();
        if paths.is_empty() { Ok(()) } else { Err(LocalError::Unresolved { paths }) }
    }

    /// Mark conflicts resolved. A text file must no longer hold markers.
    pub fn resolve(&self, caller: &Path, inputs: &[PathBuf]) -> Result<Vec<String>> {
        let mut records = read_conflicts(self.metadata_path())?;
        let mut resolved = Vec::new();
        for input in inputs {
            let path = normalize_input_path(self.root_path(), caller, input)?;
            let Some(index) = records.iter().position(|record| record.conflicted && record.path == path) else {
                return Err(LocalError::Invalid(format!("no unresolved conflict at {path}")));
            };
            if records[index].theirs.is_none() {
                let file = self.root_path().join(display_to_path(&path));
                let content = std::fs::read(&file).at("read", &file)?;
                if String::from_utf8_lossy(&content)
                    .lines()
                    .any(|line| CONFLICT_MARKERS.contains(&line))
                {
                    return Err(LocalError::Invalid(format!("{path} still has conflict markers")));
                }
            }
            records.remove(index);
            resolved.push(path);
        }
        write_conflicts(self.metadata_path(), &records)?;
        Ok(resolved)
    }

    /// Forget merges of files the working tree no longer has as merged:
    /// after `restore` or `restore-tree`, there is nothing left to resolve.
    pub(crate) fn forget_conflicts(&self, path: Option<&str>) -> Result<()> {
        let mut records = read_conflicts(self.metadata_path())?;
        let before = records.len();
        records.retain(|record| path.is_some_and(|path| record.path != path));
        if records.len() != before {
            write_conflicts(self.metadata_path(), &records)?;
        }
        Ok(())
    }

    pub(crate) async fn recover_pending_pull(&self) -> Result<()> {
        let Some(intent) = self.read_json_intent::<PullIntent>(PULL_INTENT_NAME, "pull")? else {
            return Ok(());
        };
        if intent.version != PULL_INTENT_VERSION
            || intent.items.is_empty()
            || intent.target_repo_seq <= intent.baseline
            || intent.target_repo_seq > self.head_repo_seq()?
        {
            return Err(LocalError::Integrity("pull intent has invalid fixed fields".into()));
        }
        self.complete_pull(&intent).await
    }

    /// Staging checks each entry against the state the plan saw, so a file
    /// changed since then stops the pull instead of being overwritten.
    async fn complete_pull(&self, intent: &PullIntent) -> Result<()> {
        if !intent.items.is_empty() {
            self.materialize_inverse_items(&intent.items, PULL_STAGE_NAME)?;
        }
        self.advance_baseline(intent.target_repo_seq)?;
        self.refresh_worktree_identities().await?;
        if !intent.items.is_empty() {
            self.remove_inverse_stage(PULL_STAGE_NAME)?;
            self.clear_intent(PULL_INTENT_NAME)?;
        }
        // A clean merge is an ordinary change here once the tree is at HEAD.
        let mut records = read_conflicts(self.metadata_path())?;
        let before = records.len();
        records.retain(|record| record.conflicted);
        if records.len() != before {
            write_conflicts(self.metadata_path(), &records)?;
        }
        Ok(())
    }
}
