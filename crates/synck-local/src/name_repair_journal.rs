//! Crash recovery for explicitly authorized portable-name repairs.
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Component, Path};

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{IoContext, LocalError, Result};
use crate::name_repairs::{self, NameRepair};
use crate::repository::{
    Repository, is_cap_link_or_reparse, rename_between_noreplace, sync_open_directory,
};
use crate::worktree::{FileIdentity, directory_identity, file_identity};

pub(crate) const JOURNAL: &str = "name-repair.jsonl";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    repair: NameRepair,
    temporary: String,
    identity: FileIdentity,
    parent_identity: FileIdentity,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    repo_id: synck_vcs::types::RepoId,
    entries: Vec<Entry>,
}

impl Repository {
    /// Rename non-portable paths throughout the observed checkout. No commit is
    /// appended. The repository lock covers planning, renaming and recovery.
    pub async fn fix_names(&self) -> Result<Vec<NameRepair>> {
        self.guard_agent_all()?;
        if self
            .validate_pending_operations()?
            .is_some_and(|name| name != JOURNAL)
        {
            return Err(invalid(
                "finish the pending working-tree operation before repairing names",
            ));
        }
        self.recover_name_repairs()?;
        let working = self.scan_for_name_repairs().await?;
        let mut repairs = name_repairs::plan(self.root(), &working)?;
        if repairs.is_empty() {
            return Ok(repairs);
        }
        // Children move before ancestors, so every unfinished entry retains its
        // original parent path. Progress records make completed entries skippable.
        repairs.sort_by(|a, b| {
            b.from
                .matches('/')
                .count()
                .cmp(&a.from.matches('/').count())
                .then(a.from.cmp(&b.from))
        });
        let mut entries = Vec::with_capacity(repairs.len());
        for repair in &repairs {
            let parent_path = parent(&repair.from);
            let directory = open_parent(self.root(), parent_path)?;
            let parent_identity = directory_identity(
                &directory,
                &directory
                    .dir_metadata()
                    .at("inspect rename parent", parent_path)?,
            )?;
            if !parent_path.is_empty() && working[parent_path].identity != parent_identity {
                return Err(invalid("name repair parent changed during planning"));
            }
            entries.push(Entry {
                repair: repair.clone(),
                temporary: format!(".synck-name-{}", Uuid::new_v4()),
                identity: working[&repair.from].identity,
                parent_identity,
            });
        }
        let journal = Journal {
            version: 1,
            repo_id: self.repository_id().clone(),
            entries,
        };
        let metadata = self
            .root()
            .open_dir_nofollow(".synck")
            .at("open metadata", self.metadata_path())?;
        let temporary = metadata
            .open_dir_nofollow("tmp")
            .at("open temporary directory", self.metadata_path())?;
        let filename = format!("name-repair-{}.jsonl", Uuid::new_v4());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        let mut file = temporary
            .open_with(&filename, &options)
            .at("create rename journal", &filename)?;
        serde_json::to_writer(&mut file, &journal)?;
        file.write_all(b"\n")
            .at("write rename journal", &filename)?;
        file.sync_all().at("sync rename journal", &filename)?;
        drop(file);
        rename_between_noreplace(
            &temporary,
            OsStr::new(&filename),
            &metadata,
            OsStr::new(JOURNAL),
            &self.metadata_path().join("tmp").join(&filename),
            &self.metadata_path().join(JOURNAL),
        )
        .at("publish rename journal", self.metadata_path())?;
        sync_open_directory(&metadata, self.metadata_path())?;
        sync_open_directory(&temporary, &self.metadata_path().join("tmp"))?;
        self.recover_name_repairs()?;
        Ok(repairs)
    }

    /// Return repaired paths relative to the repository root, preserving explicit
    /// commit selections when an ancestor or the selected entry is renamed.
    pub async fn fix_names_and_remap_paths(
        &self,
        caller: &Path,
        inputs: &[std::path::PathBuf],
    ) -> Result<(Vec<NameRepair>, Vec<std::path::PathBuf>)> {
        let paths = inputs
            .iter()
            .map(|path| crate::commands::normalize_input_path(self.root_path(), caller, path))
            .collect::<Result<Vec<_>>>()?;
        let repairs = self.fix_names().await?;
        let paths = paths
            .into_iter()
            .map(|path| {
                let repaired = repairs
                    .iter()
                    .filter(|repair| {
                        path == repair.from
                            || path
                                .strip_prefix(&repair.from)
                                .is_some_and(|suffix| suffix.starts_with('/'))
                    })
                    .max_by_key(|repair| repair.from.len());
                match repaired {
                    Some(repair) => format!("{}{}", repair.to, &path[repair.from.len()..]).into(),
                    None => path.into(),
                }
            })
            .collect();
        Ok((repairs, paths))
    }

    pub(crate) fn recover_name_repairs(&self) -> Result<()> {
        let metadata = self
            .root()
            .open_dir_nofollow(".synck")
            .at("open metadata", self.metadata_path())?;
        let info = match metadata.symlink_metadata(JOURNAL) {
            Ok(info) => info,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(invalid(error)),
        };
        if !info.file_type().is_file() || is_cap_link_or_reparse(&info) {
            return Err(invalid("name repair journal is not a regular file"));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).follow(FollowSymlinks::No);
        let file = metadata
            .open_with(JOURNAL, &options)
            .at("open rename journal", JOURNAL)?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut valid_bytes = reader
            .read_line(&mut line)
            .at("read rename journal", JOURNAL)? as u64;
        if !line.ends_with('\n') {
            return Err(invalid("incomplete name repair journal header"));
        }
        let journal: Journal = serde_json::from_str(&line)?;
        validate_journal(&journal, self)?;
        let mut next = 0;
        loop {
            line.clear();
            let bytes = reader
                .read_line(&mut line)
                .at("read rename progress", JOURNAL)?;
            if bytes == 0 || !line.ends_with('\n') {
                break;
            }
            let recorded: usize = line.trim_end_matches('\n').parse().map_err(invalid)?;
            if recorded != next + 1 || recorded > journal.entries.len() {
                return Err(invalid("invalid name repair progress"));
            }
            next = recorded;
            valid_bytes += bytes as u64;
        }
        let mut file = reader.into_inner();
        file.set_len(valid_bytes)
            .at("discard incomplete rename progress", JOURNAL)?;
        file.seek(SeekFrom::Start(valid_bytes))
            .at("seek rename progress", JOURNAL)?;
        let mut current_parent =
            None::<(String, Dir, std::collections::BTreeSet<std::ffi::OsString>)>;
        for (index, entry) in journal.entries.iter().enumerate().skip(next) {
            let parent_path = parent(&entry.repair.from);
            if current_parent
                .as_ref()
                .is_none_or(|(path, _, _)| path != parent_path)
            {
                let directory = open_parent(self.root(), parent_path)?;
                let names = directory
                    .entries()
                    .at("read rename parent", parent_path)?
                    .map(|entry| entry.map(|entry| entry.file_name()))
                    .collect::<std::io::Result<_>>()
                    .at("read rename parent", parent_path)?;
                current_parent = Some((parent_path.to_string(), directory, names));
            }
            let (_, directory, names) = current_parent.as_mut().unwrap();
            self.finish_name_repair(entry, directory, names)?;
            writeln!(file, "{}", index + 1).at("write rename progress", JOURNAL)?;
            file.sync_all().at("sync rename progress", JOURNAL)?;
        }
        drop(file);
        metadata
            .remove_file(JOURNAL)
            .at("remove completed rename journal", JOURNAL)?;
        sync_open_directory(&metadata, self.metadata_path())
    }

    fn finish_name_repair(
        &self,
        entry: &Entry,
        directory: &Dir,
        names: &mut std::collections::BTreeSet<std::ffi::OsString>,
    ) -> Result<()> {
        let parent_path = parent(&entry.repair.from);
        if directory_identity(
            &directory,
            &directory
                .dir_metadata()
                .at("inspect rename parent", parent_path)?,
        )? != entry.parent_identity
        {
            return Err(invalid("name repair parent changed; journal retained"));
        }
        let old = leaf(&entry.repair.from);
        let new = leaf(&entry.repair.to);
        if names
            .iter()
            .any(|name| name == OsStr::new(&entry.temporary))
        {
            require_identity(&directory, &entry.temporary, entry.identity)?;
        } else if names.iter().any(|name| name == OsStr::new(old)) {
            require_identity(&directory, old, entry.identity)?;
            rename_between_noreplace(
                &directory,
                OsStr::new(old),
                &directory,
                OsStr::new(&entry.temporary),
                Path::new(&entry.repair.from),
                Path::new(&entry.temporary),
            )
            .at("stage portable name", &entry.repair.from)?;
            sync_open_directory(&directory, &self.root_path().join(parent_path))?;
            names.remove(OsStr::new(old));
            names.insert(entry.temporary.clone().into());
            fail_after_rename(1)?;
        } else if names.iter().any(|name| name == OsStr::new(new)) {
            require_identity(&directory, new, entry.identity)?;
            return Ok(());
        } else {
            return Err(invalid("name repair source disappeared; journal retained"));
        }
        rename_between_noreplace(
            &directory,
            OsStr::new(&entry.temporary),
            &directory,
            OsStr::new(new),
            Path::new(&entry.temporary),
            Path::new(&entry.repair.to),
        )
        .at("publish portable name", &entry.repair.to)?;
        sync_open_directory(&directory, &self.root_path().join(parent_path))?;
        names.remove(OsStr::new(&entry.temporary));
        names.insert(new.into());
        fail_after_rename(2)
    }
}

fn validate_journal(journal: &Journal, repository: &Repository) -> Result<()> {
    if journal.version != 1
        || &journal.repo_id != repository.repository_id()
        || journal.entries.is_empty()
    {
        return Err(invalid("name repair journal identity mismatch"));
    }
    let mut previous_depth = usize::MAX;
    let mut sources = std::collections::BTreeSet::new();
    for entry in &journal.entries {
        for path in [&entry.repair.from, &entry.repair.to] {
            if path.split('/').any(|part| {
                part.is_empty() || matches!(part, "." | ".." | ".synck") || part.contains('\0')
            }) || !Path::new(path)
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
            {
                return Err(invalid("invalid name repair path"));
            }
        }
        let depth = entry.repair.from.matches('/').count();
        if depth > previous_depth || !sources.insert(&entry.repair.from) {
            return Err(invalid("invalid name repair order"));
        }
        previous_depth = depth;
        synck_vcs::naming::compute_name_key(leaf(&entry.repair.to)).map_err(invalid)?;
        let id = entry
            .temporary
            .strip_prefix(".synck-name-")
            .ok_or_else(|| invalid("invalid rename temporary name"))?;
        if Uuid::parse_str(id).map_err(invalid)?.to_string() != id {
            return Err(invalid("invalid rename temporary identity"));
        }
    }
    Ok(())
}

fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}
fn leaf(path: &str) -> &str {
    path.rsplit_once('/').map_or(path, |(_, name)| name)
}
fn invalid(error: impl std::fmt::Display) -> LocalError {
    LocalError::Invalid(error.to_string())
}

fn open_parent(root: &Dir, path: &str) -> Result<Dir> {
    let mut directory = root.try_clone().at("open rename root", ".")?;
    for part in path.split('/').filter(|part| !part.is_empty()) {
        directory = directory
            .open_dir_nofollow(part)
            .at("open rename parent", path)?;
    }
    Ok(directory)
}

fn require_identity(directory: &Dir, name: &str, expected: FileIdentity) -> Result<()> {
    let metadata = directory
        .symlink_metadata(name)
        .at("inspect rename entry", name)?;
    if is_cap_link_or_reparse(&metadata) {
        return Err(invalid("rename entry is a link"));
    }
    let actual = if metadata.file_type().is_dir() {
        let child = directory
            .open_dir_nofollow(name)
            .at("open rename entry", name)?;
        directory_identity(
            &child,
            &child.dir_metadata().at("inspect rename entry", name)?,
        )?
    } else if metadata.file_type().is_file() {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = directory
            .open_with(name, &options)
            .at("open rename entry", name)?;
        file_identity(&file, &file.metadata().at("inspect rename entry", name)?)?
    } else {
        return Err(invalid("rename entry is not a regular file or directory"));
    };
    if actual != expected {
        return Err(invalid("rename entry identity changed; journal retained"));
    }
    Ok(())
}

#[cfg(test)]
thread_local! { static FAIL_PHASE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) }; }
fn fail_after_rename(_phase: u8) -> Result<()> {
    #[cfg(test)]
    if FAIL_PHASE.with(|slot| slot.get() == _phase) {
        return Err(invalid("injected rename interruption"));
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn interrupted_repairs_resume_before_and_after_destination_publication() {
        for phase in [1, 2] {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path();
            crate::init(root, "author").await.unwrap();
            std::fs::create_dir(root.join("folder?")).unwrap();
            std::fs::write(root.join("folder?/NUL.txt"), b"preserve exactly").unwrap();
            let repository = Repository::discover(root).await.unwrap();
            FAIL_PHASE.with(|slot| slot.set(phase));
            assert!(
                repository
                    .fix_names()
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("injected")
            );
            FAIL_PHASE.with(|slot| slot.set(0));
            drop(repository);
            if phase == 1 {
                // An interrupted progress record is not an acknowledgment.
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(root.join(".synck").join(JOURNAL))
                    .unwrap();
                file.write_all(b"1").unwrap();
            }
            let repository = Repository::discover(root).await.unwrap();
            assert_eq!(
                std::fs::read(root.join("folder_/_NUL.txt")).unwrap(),
                b"preserve exactly"
            );
            assert!(!root.join(".synck").join(JOURNAL).exists());
            assert!(repository.fix_names().await.unwrap().is_empty());
            repository.commit("repaired names").await.unwrap();
            assert!(repository.status().await.unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn interrupted_repair_never_overwrites_a_new_destination() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        crate::init(root, "author").await.unwrap();
        std::fs::write(root.join("bad?name"), b"original").unwrap();
        let repository = Repository::discover(root).await.unwrap();
        FAIL_PHASE.with(|slot| slot.set(1));
        assert!(repository.fix_names().await.is_err());
        FAIL_PHASE.with(|slot| slot.set(0));
        std::fs::write(root.join("bad_name"), b"new destination").unwrap();
        drop(repository);
        assert!(Repository::discover(root).await.is_err());
        assert_eq!(
            std::fs::read(root.join("bad_name")).unwrap(),
            b"new destination"
        );
        let staged = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".synck-name-")
            })
            .unwrap();
        assert_eq!(std::fs::read(staged).unwrap(), b"original");
        std::fs::remove_file(root.join("bad_name")).unwrap();
        Repository::discover(root).await.unwrap();
        assert_eq!(std::fs::read(root.join("bad_name")).unwrap(), b"original");
    }
}
