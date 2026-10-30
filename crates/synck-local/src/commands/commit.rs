//! Status and commits of the working tree.

use super::*;

#[derive(Debug)]
pub enum CommitOutcome {
    Clean,
    Committed(String),
}

pub(super) struct PreparedWorkingCommit {
    pub(super) created_ids: HashMap<String, ItemId>,
    pub(super) events: Vec<EventProposal>,
    pub(super) ref_events: Vec<RefEventProposal>,
    pub(super) subtree_guards: Vec<SubtreeClosureGuard>,
}

pub(crate) fn validate_commit_message(message: Option<&str>) -> Result<()> {
    if message == Some("") {
        return Err(LocalError::Invalid(
            "commit message must not be empty".to_string(),
        ));
    }
    if message.is_some_and(|value| value.len() > synck_file_folder::MAX_MESSAGE_BYTES) {
        return Err(LocalError::FileFolder(SemanticError {
            code: SemanticErrorCode::MessageTooLarge,
            detail: "message exceeds the UTF-8 byte limit".to_string(),
        }));
    }
    Ok(())
}

pub(crate) fn select_changes(
    root: &Path,
    caller: &Path,
    inputs: &[PathBuf],
    committed: &BTreeMap<String, CommittedNode>,
    working: &BTreeMap<String, WorkingNode>,
    working_ids: &HashMap<String, ItemId>,
    changes: &[Change],
) -> Result<Vec<Change>> {
    if inputs.is_empty() {
        return Ok(changes.to_vec());
    }

    let committed_paths = committed
        .iter()
        .map(|(path, node)| (&node.item_id, path))
        .collect::<HashMap<_, _>>();
    let working_paths = working_ids
        .iter()
        .map(|(path, item_id)| (item_id, path))
        .collect::<HashMap<_, _>>();
    let mut scopes = Vec::with_capacity(inputs.len() * 2);
    for input in inputs {
        let scope = normalize_input_path(root, caller, input)?;
        if !committed.contains_key(&scope) && !working.contains_key(&scope) {
            return Err(LocalError::Invalid(format!(
                "selected path does not exist in the working tree or accepted state: {scope}"
            )));
        }

        let committed_item = committed.get(&scope).map(|node| &node.item_id);
        let working_item = working_ids.get(&scope);
        let recursive = committed
            .get(&scope)
            .is_some_and(|node| node.kind == NodeKind::Folder)
            || working
                .get(&scope)
                .is_some_and(|node| node.kind == NodeKind::Folder);
        let selected_item = working_item.or(committed_item);
        if let Some(item_id) = selected_item {
            let working_path = working_paths.get(item_id).copied();
            for path in [committed_paths.get(item_id), working_paths.get(item_id)]
                .into_iter()
                .flatten()
            {
                if committed
                    .get(*path)
                    .is_some_and(|node| node.item_id != *item_id)
                    || working_path.is_some_and(|working_path| {
                        working_path != *path && working.contains_key(*path)
                    })
                {
                    return Err(LocalError::Invalid(format!(
                        "selected item path is reused by another item in accepted or working state: {path}"
                    )));
                }
            }
        }
        add_selection_scope(
            &mut scopes,
            SelectionScope {
                path: scope,
                recursive,
            },
        );
        if let Some(item_id) = selected_item {
            if let Some(path) = committed_paths.get(item_id) {
                add_selection_scope(
                    &mut scopes,
                    SelectionScope {
                        path: (*path).clone(),
                        recursive,
                    },
                );
            }
            if let Some(path) = working_paths.get(item_id) {
                add_selection_scope(
                    &mut scopes,
                    SelectionScope {
                        path: (*path).clone(),
                        recursive,
                    },
                );
            }
        }
    }

    let mut selected = changes
        .iter()
        .enumerate()
        .filter(|(_, change)| {
            scopes.iter().any(|scope| {
                selection_scope_contains(scope, &change.path)
                    || change
                        .source_path
                        .as_deref()
                        .is_some_and(|path| selection_scope_contains(scope, path))
            })
        })
        .map(|(index, _)| index)
        .collect::<HashSet<_>>();

    let dependencies = changes
        .iter()
        .enumerate()
        .filter(|(_, change)| change.kind == ChangeKind::Add)
        .map(|(index, change)| (change.path.as_str(), index))
        .collect::<HashMap<_, _>>();
    let same_path_deletions = changes
        .iter()
        .enumerate()
        .filter(|(_, change)| change.kind == ChangeKind::Delete)
        .map(|(index, change)| (change.path.as_str(), index))
        .collect::<HashMap<_, _>>();
    for index in selected.iter().copied().collect::<Vec<_>>() {
        let mut parent = changes[index]
            .path
            .rsplit_once('/')
            .map(|(parent, _)| parent);
        while let Some(path) = parent {
            if let Some(dependency) = dependencies.get(path) {
                selected.insert(*dependency);
                if let Some(deletion) = same_path_deletions.get(path) {
                    selected.insert(*deletion);
                }
            }
            parent = path.rsplit_once('/').map(|(parent, _)| parent);
        }
    }

    Ok(changes
        .iter()
        .enumerate()
        .filter(|(index, _)| selected.contains(index))
        .map(|(_, change)| change.clone())
        .collect())
}

pub(super) struct SelectionScope {
    pub(super) path: String,
    pub(super) recursive: bool,
}

pub(super) fn add_selection_scope(scopes: &mut Vec<SelectionScope>, scope: SelectionScope) {
    if scopes.iter().any(|existing| {
        (existing.recursive && path_in_scope(&scope.path, &existing.path))
            || (!scope.recursive && existing.path == scope.path)
    }) {
        return;
    }
    if scope.recursive {
        scopes.retain(|existing| !path_in_scope(&existing.path, &scope.path));
    }
    scopes.push(scope);
}

pub(super) fn selection_scope_contains(scope: &SelectionScope, path: &str) -> bool {
    path == scope.path || (scope.recursive && path_in_scope(path, &scope.path))
}

pub(super) fn path_in_scope(path: &str, scope: &str) -> bool {
    path == scope
        || path
            .strip_prefix(scope)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

pub(crate) struct BuiltEvents {
    pub events: Vec<EventProposal>,
    pub ref_events: Vec<RefEventProposal>,
    pub subtree_guards: Vec<SubtreeClosureGuard>,
}

pub(crate) struct BuiltIntents {
    pub intents: Vec<Intent>,
    pub created_ids: HashMap<String, ItemId>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_intents(
    committed: &BTreeMap<String, CommittedNode>,
    working: &BTreeMap<String, WorkingNode>,
    changes: &[Change],
    working_ids: &HashMap<String, ItemId>,
    objects: &HashMap<String, ObjectId>,
    root_parent_id: Option<ItemId>,
) -> Result<BuiltIntents> {
    let trash_paths = changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Delete)
        .map(|change| change.path.as_str())
        .collect::<HashSet<_>>();
    let mut trash_roots = trash_paths
        .iter()
        .copied()
        .filter(|path| {
            !trash_paths
                .iter()
                .any(|ancestor| *ancestor != *path && path_in_scope(path, ancestor))
        })
        .collect::<Vec<_>>();
    trash_roots.sort();
    let mut intents = Vec::new();
    for path in trash_roots {
        let root = committed.get(path).ok_or_else(|| {
            LocalError::Integrity(format!("deleted path is not committed: {path}"))
        })?;
        intents.push(Intent::Trash {
            root: ItemBase {
                item_id: root.item_id.clone(),
                base_seq: root.head_seq,
            },
            descendants: Vec::new(),
        });
    }

    let new_paths: HashSet<&str> = changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Add)
        .map(|change| change.path.as_str())
        .collect();
    let mut new_ids = HashMap::new();
    for (path, _) in working
        .iter()
        .filter(|(path, _)| new_paths.contains(path.as_str()))
    {
        new_ids.insert(path.clone(), ItemId::new());
    }
    let mut folders: Vec<&String> = new_ids
        .keys()
        .filter(|path| working[*path].kind == NodeKind::Folder)
        .collect();
    folders.sort_by(|left, right| {
        path_depth(left)
            .cmp(&path_depth(right))
            .then_with(|| left.cmp(right))
    });
    for path in folders {
        let item_id = new_ids[path].clone();
        intents.push(Intent::CreateFolder {
            item_id,
            location: Location {
                parent_item_id: parent_id(
                    path,
                    committed,
                    working_ids,
                    &new_ids,
                    root_parent_id.clone(),
                )?,
                name: path_name(path).to_string(),
            },
        });
    }

    let mut moves = changes
        .iter()
        .filter(|change| change.kind.is_relocation())
        .collect::<Vec<_>>();
    moves.sort_by(|left, right| {
        path_depth(&left.path)
            .cmp(&path_depth(&right.path))
            .then_with(|| left.path.cmp(&right.path))
    });
    for change in moves {
        let source = change
            .source_path
            .as_deref()
            .expect("relocation has a source path");
        let old = &committed[source];
        let node = &working[&change.path];
        let location = Location {
            parent_item_id: parent_id(
                &change.path,
                committed,
                working_ids,
                &new_ids,
                root_parent_id.clone(),
            )?,
            name: path_name(&change.path).to_string(),
        };
        match old.kind {
            NodeKind::Folder => intents.push(Intent::UpdateFolder {
                item_id: old.item_id.clone(),
                base_seq: old.head_seq,
                location,
            }),
            NodeKind::File => intents.push(Intent::UpdateFile {
                item_id: old.item_id.clone(),
                base_seq: old.head_seq,
                location: Some(location),
                content: objects
                    .get(&change.path)
                    .map(|blob_id| blob_ref(blob_id, node.size.expect("moved FILE has size")))
                    .transpose()?,
            }),
        }
    }

    let mut files: Vec<&String> = new_ids
        .keys()
        .filter(|path| working[*path].kind == NodeKind::File)
        .collect();
    files.sort();
    for path in files {
        let item_id = new_ids[path].clone();
        let node = &working[path];
        intents.push(Intent::CreateFile {
            item_id,
            location: Location {
                parent_item_id: parent_id(
                    path,
                    committed,
                    working_ids,
                    &new_ids,
                    root_parent_id.clone(),
                )?,
                name: path_name(path).to_string(),
            },
            content: blob_ref(&objects[path], node.size.expect("new FILE has size"))?,
        });
    }

    for change in changes
        .iter()
        .filter(|change| change.kind == ChangeKind::Modify)
    {
        let node = &working[&change.path];
        let source = change.source_path.as_deref().unwrap_or(&change.path);
        let old = &committed[source];
        intents.push(Intent::UpdateFile {
            item_id: old.item_id.clone(),
            base_seq: old.head_seq,
            location: None,
            content: Some(blob_ref(
                &objects[&change.path],
                node.size.expect("modified FILE has size"),
            )?),
        });
    }
    Ok(BuiltIntents {
        intents,
        created_ids: new_ids,
    })
}

pub(crate) fn materialize_events(
    repo_id: &RepoId,
    message: Option<&str>,
    intents: Vec<Intent>,
    current: &BTreeMap<ItemId, CurrentItem>,
) -> Result<BuiltEvents> {
    let materialized = validate_and_materialize(
        repo_id,
        &IntentProposal {
            message: message.map(str::to_string),
            intents,
        },
        current,
    )?;
    let mut events = Vec::new();
    let mut ref_events = Vec::new();
    for proposal in materialized.events {
        let initial_file = proposal.event_type == "FILE::CREATE";
        let file_id = proposal.item_id.clone();
        events.push(proposal);
        if initial_file {
            let file_id = file_id.expect("materialized content has an item ID");
            let source_commit_event_idx = i32::try_from(events.len() - 1)
                .map_err(|_| LocalError::Invalid("commit event index overflow".to_string()))?;
            let version_id = Uuid::now_v7().to_string();
            ref_events.push(
                synck_file_folder::file_versions::automatic_initial_ref_event(
                    repo_id,
                    &file_id,
                    &version_id,
                    0,
                    1,
                    source_commit_event_idx,
                )
                .map_err(LocalError::Invalid)?,
            );
        }
    }
    let (target_ids, _) = proposal_item_kind_requirements(&events);
    let kinds = target_ids
        .iter()
        .map(|item_id| {
            let item = &current[item_id];
            synck_file_folder::ImmutableItemKind {
                item_id: item_id.clone(),
                item_type: item.item_type.clone(),
            }
        })
        .collect::<Vec<_>>();
    validate_projected_proposal(message, &events, &kinds)?;
    Ok(BuiltEvents {
        events,
        ref_events,
        subtree_guards: materialized.subtree_guards,
    })
}

impl Repository {
    pub async fn status(&self) -> Result<Vec<Change>> {
        let (_, _, changes, _) = self.changes().await?;
        Ok(changes)
    }

    /// Status plus Windows interoperability warnings, from the same observation.
    pub async fn status_with_path_warnings(&self) -> Result<(Vec<Change>, Vec<PathBuf>)> {
        let (_, working, changes, _) = self.changes().await?;
        let warnings = crate::path_warnings::long_paths(
            self.root_path(), working.keys().map(String::as_str),
        );
        Ok((changes, warnings))
    }

    pub async fn commit(&self, message: &str) -> Result<CommitOutcome> {
        self.commit_selected(self.root_path(), &[], Some(message))
            .await
    }

    pub async fn commit_selected(
        &self,
        caller: &Path,
        inputs: &[PathBuf],
        message: Option<&str>,
    ) -> Result<CommitOutcome> {
        let options = crate::portable::WriteOptions {
            message: message.map(str::to_string),
            ..Default::default()
        };
        self.commit_selected_with(caller, inputs, &options, None).await
    }

    /// Selected commit with direct-writer attribution; the configured author
    /// remains the committer.
    pub(crate) async fn commit_selected_with(
        &self,
        caller: &Path,
        inputs: &[PathBuf],
        options: &crate::portable::WriteOptions,
        captured_file: Option<(&str, &BlobRef)>,
    ) -> Result<CommitOutcome> {
        let message = options.message.as_deref();
        validate_commit_message(message)?;
        let (committed, working, all_changes, working_ids) = self.changes().await?;
        let changes = select_changes(
            self.root_path(),
            caller,
            inputs,
            &committed,
            &working,
            &working_ids,
            &all_changes,
        )?;
        if changes.is_empty() {
            return Ok(CommitOutcome::Clean);
        }
        // Implicit new parents do not alter a sibling's reserved file. The
        // selected descendants still check reservations on the parent itself.
        let mut changed_parents = BTreeSet::new();
        for change in &changes {
            let mut path = change.path.as_str();
            while let Some((parent, _)) = path.rsplit_once('/') {
                changed_parents.insert(parent);
                path = parent;
            }
        }
        let reserved_paths: Vec<&str> = changes.iter()
            .filter(|c| !(c.kind == ChangeKind::Add
                && working.get(&c.path).is_some_and(|n| n.kind == NodeKind::Folder)
                && changed_parents.contains(c.path.as_str())))
            .flat_map(|c| std::iter::once(c.path.as_str()).chain(c.source_path.as_deref()))
            .collect();
        self.guard_agent_paths(&reserved_paths)?;
        let captured = self.capture_changed_files(&working, &changes, captured_file)?;
        let mut prepared = self.prepare_working_commit(&committed, &working, &changes, &working_ids, message, &captured)?;
        options.mark_file_write(&mut prepared.events);
        self.guard_agent_paths(&reserved_paths)?;
        let accepted = self
            .core()
            .append_file_folder_checked_with_refs(
                self.config().repo_id().clone(),
                CommitProposal {
                    author_id: options
                        .author
                        .clone()
                        .unwrap_or_else(|| self.config().author().to_string()),
                    committer_id: self.config().author().to_string(),
                    message: message.map(str::to_string),
                    occurred_at: options.occurred_at,
                    operation_id: Some(
                        options
                            .operation_id
                            .as_ref()
                            .map(crate::portable::stored_operation_id)
                            .unwrap_or_else(|| Uuid::new_v4().to_string()),
                    ),
                    events: prepared.events,
                },
                prepared.ref_events,
                &[],
                &prepared.subtree_guards,
            )
            .await?;
        let commit_id = accepted.commit_id.to_string();
        if let Err(error) = self.recover_current().await {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: error.to_string(),
            });
        }
        if let Err(error) = self.publish_committed_identities(
            &committed,
            &working,
            &changes,
            &working_ids,
            &prepared.created_ids,
        ) {
            return Err(LocalError::AcceptedNeedsRecovery {
                commit_id,
                detail: error.to_string(),
            });
        }
        Ok(CommitOutcome::Committed(commit_id))
    }

    pub(super) fn prepare_working_commit(
        &self,
        committed: &BTreeMap<String, CommittedNode>,
        working: &BTreeMap<String, WorkingNode>,
        changes: &[Change],
        working_ids: &HashMap<String, ItemId>,
        message: Option<&str>,
        objects: &HashMap<String, ObjectId>,
    ) -> Result<PreparedWorkingCommit> {
        let built_intents =
            build_intents(committed, working, changes, working_ids, &objects, None)?;
        let mut intents = built_intents.intents;
        let current = self.current_file_folder_context(&mut intents)?;
        let built = materialize_events(self.config().repo_id(), message, intents, &current)?;
        Ok(PreparedWorkingCommit {
            created_ids: built_intents.created_ids,
            events: built.events,
            ref_events: built.ref_events,
            subtree_guards: built.subtree_guards,
        })
    }

    pub(super) fn capture_changed_files(
        &self,
        working: &BTreeMap<String, WorkingNode>,
        changes: &[Change],
        captured_file: Option<(&str, &BlobRef)>,
    ) -> Result<HashMap<String, ObjectId>> {
        let mut objects = HashMap::new();
        for change in changes {
            if !matches!(change.kind, ChangeKind::Add | ChangeKind::Modify)
                && !change.kind.is_relocation()
            {
                continue;
            }
            let Some(node) = working.get(&change.path) else {
                continue;
            };
            if node.kind != NodeKind::File {
                continue;
            }
            if change.kind.is_relocation() && !change.content_changed {
                continue;
            }
            let digest = node.digest_hex.as_deref().expect("scanned FILE has digest");
            let size = node.size.expect("scanned FILE has size");
            if let Some((path, content)) = captured_file.filter(|(path, _)| *path == change.path) {
                if content.size != size || self.plain_digest(&content.blob_id)? != digest {
                    return Err(LocalError::Invalid("working bytes differ from the supplied file".into()));
                }
                verify_working_file(self, path, &content.blob_id, i64::try_from(size).map_err(|_| LocalError::Invalid("file too large".into()))?)?;
                objects.insert(change.path.clone(), content.blob_id.clone());
                continue;
            }
            let relative = display_to_path(&change.path);
            let blob_id =
                self.install_object_before_commit(&relative, &change.path, digest, size)?;
            objects.insert(change.path.clone(), blob_id);
        }
        if !objects.is_empty() {
            self.checkpoint_commit_objects()?;
        }
        Ok(objects)
    }
}
