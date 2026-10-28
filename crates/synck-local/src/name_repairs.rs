//! Deterministic portable-name diagnostics for a complete observed tree.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use serde::{Deserialize, Serialize};
use synck_vcs::naming::{compute_name_key, portable_name, suffixed_name};

use crate::error::{IoContext, LocalError, Result};
use crate::repository::is_cap_link_or_reparse;
use crate::worktree::{WorkingNode, directory_identity, file_identity};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NameRepair {
    pub from: String,
    pub to: String,
    pub reason: String,
}

pub(crate) fn validate(root: &Dir, working: &BTreeMap<String, WorkingNode>) -> Result<()> {
    let repairs = plan(root, working)?;
    if repairs.is_empty() {
        return Ok(());
    }
    let mut message = format!("{} non-portable name(s):", repairs.len());
    for repair in repairs {
        message.push_str(&format!(
            "\n  {:?} -> {:?}: {}",
            repair.from, repair.to, repair.reason
        ));
    }
    Err(LocalError::Invalid(message))
}

pub(crate) fn plan(root: &Dir, working: &BTreeMap<String, WorkingNode>) -> Result<Vec<NameRepair>> {
    let mut folders = BTreeMap::<&str, Vec<&str>>::new();
    for path in working.keys() {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        folders.entry(parent).or_default().push(name);
    }
    let mut repairs = Vec::new();
    let mut renamed = BTreeMap::<String, String>::new();
    for (parent, names) in folders {
        // Reserve valid names first, so an invalid spelling never displaces one.
        let mut keys = BTreeMap::<String, &str>::new();
        let mut invalid = Vec::new();
        for &name in &names {
            match compute_name_key(name) {
                Ok(key) => match keys.entry(key) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(name);
                    }
                    std::collections::btree_map::Entry::Occupied(entry) => {
                        invalid.push((
                            name,
                            format!("portable name collision with {:?}", entry.get()),
                        ));
                    }
                },
                Err(error) => invalid.push((name, error.to_string())),
            }
        }
        if !invalid.is_empty() {
            let directory = root
                .open_dir_nofollow(if parent.is_empty() { "." } else { parent })
                .at("open names directory", parent)?;
            let occupied = directory
                .entries()
                .at("read names directory", parent)?
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect::<std::io::Result<BTreeSet<_>>>()
                .at("read names directory", parent)?;
            for (name, reason) in invalid {
                let from = join(parent, name);
                let original = &working[&from];
                let base = portable_name(name);
                let mut candidate = base.clone();
                let mut ordinal = 2;
                loop {
                    let key = compute_name_key(&candidate)
                        .map_err(|e| LocalError::Invalid(e.to_string()))?;
                    let available = !keys.contains_key(&key)
                        && (candidate == name
                            || (!occupied.contains(std::ffi::OsStr::new(&candidate))
                                && destination_available(&directory, &candidate, original)?));
                    if available {
                        keys.insert(key, name);
                        break;
                    }
                    candidate = suffixed_name(&base, ordinal);
                    ordinal = ordinal
                        .checked_add(1)
                        .ok_or_else(|| LocalError::Invalid("name suffix exhausted".into()))?;
                }
                let new_parent = renamed.get(parent).map(String::as_str).unwrap_or(parent);
                let to = join(new_parent, &candidate);
                renamed.insert(from.clone(), to.clone());
                repairs.push(NameRepair { from, to, reason });
            }
        }
        // Carry renamed ancestors through unchanged intermediate directories.
        if let Some(new_parent) = renamed.get(parent).cloned() {
            for name in names {
                renamed
                    .entry(join(parent, name))
                    .or_insert_with(|| join(&new_parent, name));
            }
        }
    }
    Ok(repairs)
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

// On case-insensitive or normalization-insensitive filesystems, the candidate
// can resolve to the source itself even though readdir has no such spelling.
fn destination_available(directory: &Dir, name: &str, original: &WorkingNode) -> Result<bool> {
    let metadata = match directory.symlink_metadata(name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(source) => {
            return Err(LocalError::Io {
                action: "inspect name destination",
                path: Path::new(name).into(),
                source,
            });
        }
    };
    if is_cap_link_or_reparse(&metadata) {
        return Ok(false);
    }
    let identity = if metadata.file_type().is_dir() {
        let dir = directory
            .open_dir_nofollow(name)
            .at("open name destination", name)?;
        directory_identity(
            &dir,
            &dir.dir_metadata().at("inspect name destination", name)?,
        )?
    } else if metadata.file_type().is_file() {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let file = directory
            .open_with(name, &options)
            .at("open name destination", name)?;
        file_identity(
            &file,
            &file.metadata().at("inspect name destination", name)?,
        )?
    } else {
        return Ok(false);
    };
    Ok(identity == original.identity)
}
