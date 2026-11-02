//! `synck pull`: bring commits made from other working trees into this one.
//!
//! A working tree is based on one repository sequence, its baseline. Pull
//! compares three trees: the baseline's, HEAD's and the working files. An item
//! changed only in history is written; an item changed only here is left
//! alone; an item changed on both sides to different states is reported and
//! nothing is written. Then the baseline is HEAD.

use super::*;

pub(super) const PULL_INTENT_VERSION: u32 = 1;
pub(super) const PULL_INTENT_NAME: &str = "pull.json";
pub(super) const PULL_STAGE_NAME: &str = "pull-stage";

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
    /// Working paths written, moved or removed.
    pub updated: Vec<String>,
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
/// local change, or every path changed on both sides.
fn plan_pull(
    base: &BTreeMap<String, CommittedNode>,
    head: &BTreeMap<String, CommittedNode>,
    working: &BTreeMap<String, WorkingNode>,
    working_ids: &HashMap<String, ItemId>,
) -> Result<std::result::Result<Vec<RevertIntentItem>, Vec<String>>> {
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
    let mut items = Vec::new();
    for item in build_worktree_restore_items(head, working, &head_ids)? {
        if item.before.is_some() {
            if !incoming.contains(&item.item_id) {
                continue;
            }
            if local_ids.contains(&item.item_id) {
                conflicts.insert(item.after.path.clone());
                continue;
            }
            let before = item.before.as_ref().expect("checked");
            let path = &before.path;
            // Added here too, with the same content: nothing to write.
            if !item.after.live
                && !working_ids.contains_key(path)
                && let Some(node) = working.get(path)
            {
                if !working_restore_state(path, node, before.head_seq, &head_ids)
                    .is_ok_and(|local| same_revert_value(&local, before))
                {
                    conflicts.insert(path.clone());
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
    if conflicts.is_empty() {
        items.sort_by(|left, right| left.item_id.cmp(&right.item_id));
        Ok(Ok(items))
    } else {
        Ok(Err(conflicts.into_iter().collect()))
    }
}

impl Repository {
    /// Bring this working tree to HEAD, keeping every local change.
    pub async fn pull(&self) -> Result<PullOutcome> {
        self.guard_agent_all()?;
        self.recover_pending_pull().await?;
        if self.held() {
            self.take_in_origin().await?;
        }
        let from = self.baseline();
        if self.at_head() {
            return Ok(PullOutcome {
                from_repo_seq: from,
                to_repo_seq: from,
                updated: Vec::new(),
            });
        }
        let (base, working, _, working_ids) = self.changes().await?;
        let head = self.build_committed_tree(true).await?;
        let target = self.head_repo_seq()?;
        let items = match plan_pull(&base, &head, &working, &working_ids)? {
            Ok(items) => items,
            Err(paths) => return Err(LocalError::ChangedOnBothSides { paths }),
        };
        reject_ignored_restore_descendants(self.root(), &head, &items, &working)?;
        self.validate_inverse_objects(&items)?;
        self.validate_inverse_destinations(&items)?;
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
        })
    }

    /// Writers build on HEAD. A working tree that is behind takes in the other
    /// working trees' commits first, or stops at an item changed on both sides.
    pub(crate) async fn pull_if_behind(&self) -> Result<()> {
        if !self.at_head() {
            self.pull().await?;
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
        Ok(())
    }
}
