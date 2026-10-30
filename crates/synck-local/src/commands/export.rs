//! `synck export` and `synck export-tree`: copy committed bytes out.

use super::*;

pub(super) fn resolve_export_destination(
    caller: &Path,
    input: &Path,
    metadata_path: &Path,
) -> Result<(PathBuf, OsString, PathBuf)> {
    let candidate = if input.is_absolute() {
        input.to_path_buf()
    } else {
        caller.join(input)
    };
    let name = match candidate.components().next_back() {
        Some(Component::Normal(name)) => name.to_os_string(),
        _ => {
            return Err(LocalError::Invalid(
                "export destination must name a file".to_string(),
            ));
        }
    };
    let unresolved_parent = candidate.parent().ok_or_else(|| {
        LocalError::Invalid("export destination has no parent directory".to_string())
    })?;
    let metadata = std::fs::symlink_metadata(unresolved_parent)
        .at("inspect export parent", unresolved_parent)?;
    if !metadata.file_type().is_dir() || is_link_or_reparse(&metadata) {
        return Err(LocalError::Invalid(format!(
            "export parent is not a real directory: {}",
            unresolved_parent.display()
        )));
    }
    let parent = unresolved_parent
        .canonicalize()
        .at("resolve export parent", unresolved_parent)?;
    if parent.starts_with(metadata_path) {
        return Err(LocalError::Invalid(
            "export destination must be outside .synck".to_string(),
        ));
    }
    let destination = parent.join(&name);
    Ok((parent, name, destination))
}

#[cfg(windows)]
pub(super) fn replace_file_windows(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source =
        windows_extended_length_path_units(&source.as_os_str().encode_wide().collect::<Vec<_>>());
    let destination = windows_extended_length_path_units(
        &destination.as_os_str().encode_wide().collect::<Vec<_>>(),
    );
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

impl Repository {
    pub async fn export(
        &self,
        caller: &Path,
        input: &Path,
        destination: &Path,
        requested_seq: Option<i64>,
    ) -> Result<()> {
        let (_, blob_id, size) = self
            .resolve_file_revision(caller, input, requested_seq)
            .await?;
        let (parent, name, destination) =
            resolve_export_destination(caller, destination, self.metadata_path())?;
        crate::agent_locks::guard_output(self.root_path(), self.repository_id().as_str(), &self.agent_lock_tokens, &destination)?;
        let parent_dir = Dir::open_ambient_dir(&parent, ambient_authority())
            .at("open export parent", &parent)?;
        match parent_dir.symlink_metadata(&name) {
            Ok(_) => {
                return Err(LocalError::Invalid(format!(
                    "export destination already exists: {}",
                    destination.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect export destination",
                    path: destination,
                    source,
                });
            }
        }
        let temporary =
            self.copy_object_to_temporary(&parent_dir, &parent, &destination, &blob_id, size)?;
        self.publish_temporary_file(&parent_dir, &parent, &temporary, &name, &destination, false)
    }

    pub async fn export_tree(
        &self,
        caller: &Path,
        revision: &str,
        folder: Option<&Path>,
        destination: &Path,
    ) -> Result<()> {
        self.recover().await?;
        let tree = if let Some(folder) = folder {
            let selected = normalize_input_path(self.root_path(), caller, folder)?;
            self.historical_subtree_at(revision, &selected).await?.1
        } else {
            self.historical_tree_at(revision).await?.1
        };

        let (parent, name, destination) =
            resolve_export_destination(caller, destination, self.metadata_path())?;
        crate::agent_locks::guard_output(self.root_path(), self.repository_id().as_str(), &self.agent_lock_tokens, &destination)?;
        let temporary_name = OsString::from(format!(
            ".{}.synck-tmp-{}",
            name.to_string_lossy(),
            Uuid::new_v4()
        ));
        let temporary = parent.join(&temporary_name);
        std::fs::create_dir(&temporary).at("create historical export staging", &temporary)?;
        let result = (|| -> Result<()> {
            materialize_tree(self, &temporary, &tree)?;
            let staged = Dir::open_ambient_dir(&temporary, ambient_authority())
                .at("open historical export staging", &temporary)?;
            let working = crate::worktree::scan_materialized_tree(&staged, &tree)?;
            validate_materialized_tree(&tree, &working)?;
            drop(staged);
            rename_noreplace(&temporary, &destination)
                .at("publish historical export", &destination)?;
            sync_directory(&parent)
        })();
        if result.is_err() && path_exists_nofollow(&temporary)? {
            remove_tree_nofollow(&temporary)?;
            sync_directory(&parent)?;
        }
        result
    }

    pub(super) async fn resolve_file_revision(
        &self,
        caller: &Path,
        input: &Path,
        requested_seq: Option<i64>,
    ) -> Result<(String, ObjectId, i64)> {
        self.recover().await?;
        let display = normalize_input_path(self.root_path(), caller, input)?;
        let committed = self.committed_tree().await?;
        let node = committed.get(&display).ok_or_else(|| {
            LocalError::Invalid(format!("path is not a tracked live file: {display}"))
        })?;
        if node.kind != NodeKind::File {
            return Err(LocalError::Invalid(format!(
                "path is not a tracked live file: {display}"
            )));
        }
        let (blob_id, size) = match requested_seq {
            None => (
                node.blob_id
                    .clone()
                    .expect("a committed FILE has a blob projection"),
                node.size.expect("a committed FILE has projected size"),
            ),
            Some(item_seq) if item_seq > 0 => {
                let events = self
                    .core()
                    .list_events(
                        self.config().repo_id().clone(),
                        node.item_id.clone(),
                        item_seq,
                        1,
                    )
                    .await?;
                let event = events
                    .first()
                    .filter(|event| event.item_seq == item_seq)
                    .ok_or_else(|| {
                        LocalError::Invalid(format!(
                            "item sequence {item_seq} does not exist for {display}"
                        ))
                    })?;
                if !synck_file_folder::is_file_content_event(&event.event_type) {
                    return Err(LocalError::Invalid(format!(
                        "item sequence {item_seq} is not a file content version"
                    )));
                }
                let proposal = parse_event(event)?;
                if proposal.set_content.is_none() {
                    return Err(LocalError::Invalid(format!(
                        "item sequence {item_seq} is not a file content version"
                    )));
                }
                content_from_event(&proposal)?
            }
            Some(_) => {
                return Err(LocalError::Invalid(
                    "item sequence must be greater than zero".to_string(),
                ));
            }
        };
        Ok((display, blob_id, size))
    }

    pub(crate) fn write_working_file(
        &self,
        display: &str,
        blob_id: &ObjectId,
        expected_size: i64,
        overwrite: bool,
    ) -> Result<()> {
        let relative = display_to_path(display);
        let parent = match relative.parent() {
            Some(path) if !path.as_os_str().is_empty() => path,
            _ => Path::new("."),
        };
        let name = relative
            .file_name()
            .ok_or_else(|| LocalError::Invalid(format!("invalid restore path: {display}")))?;
        let parent_dir = self
            .root()
            .open_dir_nofollow(parent)
            .at("open restore parent", parent)?;
        match parent_dir.symlink_metadata(name) {
            Ok(metadata) => {
                if !overwrite {
                    return Err(LocalError::Invalid(format!(
                        "recovery target already exists: {display}"
                    )));
                }
                if !metadata.file_type().is_file() || is_cap_link_or_reparse(&metadata) {
                    return Err(LocalError::Invalid(format!(
                        "restore target is not a regular file: {display}"
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(LocalError::Io {
                    action: "inspect restore target",
                    path: relative,
                    source,
                });
            }
        }
        let temporary = self.copy_object_to_temporary(
            &parent_dir,
            &self.root_path().join(parent),
            Path::new(display),
            blob_id,
            expected_size,
        )?;
        self.publish_temporary_file(
            &parent_dir,
            &self.root_path().join(parent),
            &temporary,
            name,
            Path::new(display),
            overwrite,
        )
    }

    pub(super) fn copy_object_to_temporary(
        &self,
        parent_dir: &Dir,
        parent: &Path,
        display: &Path,
        blob_id: &ObjectId,
        expected_size: i64,
    ) -> Result<String> {
        let temporary_name = format!(".synck-tmp-{}", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .create_new(true)
            .write(true)
            .follow(FollowSymlinks::No);
        let mut temporary = parent_dir
            .open_with(&temporary_name, &options)
            .at("create temporary file", display)?;
        if let Err(error) = (|| -> Result<()> {
            let expected_size = u64::try_from(expected_size).map_err(|_| {
                LocalError::Integrity(format!("negative object size for {blob_id}"))
            })?;
            self.copy_blob(blob_id, expected_size, &mut temporary)?;
            temporary.sync_all().at("sync", display)
        })() {
            parent_dir
                .remove_file(&temporary_name)
                .at("remove temporary file", display)?;
            sync_directory(parent)?;
            return Err(error);
        }
        Ok(temporary_name)
    }

    pub(super) fn publish_temporary_file(
        &self,
        parent_dir: &Dir,
        parent: &Path,
        temporary_name: &str,
        name: &OsStr,
        display: &Path,
        overwrite: bool,
    ) -> Result<()> {
        let temporary = parent.join(temporary_name);
        let destination = parent.join(name);
        let result = (|| -> Result<()> {
            if overwrite {
                #[cfg(not(windows))]
                parent_dir
                    .rename(temporary_name, parent_dir, name)
                    .at("replace", display)?;
                #[cfg(windows)]
                replace_file_windows(&temporary, &destination).at("replace", display)?;
            } else {
                rename_noreplace(&temporary, &destination).at("publish file", display)?;
            }
            sync_directory(parent)
        })();
        if result.is_err() && path_exists_nofollow(&temporary)? {
            parent_dir
                .remove_file(temporary_name)
                .at("remove temporary file", &temporary)?;
            sync_directory(parent)?;
        }
        result
    }
}
