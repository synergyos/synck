//! `synck clone`: another working tree of a repository on this machine.
//!
//! A clone reads its origin's history and content objects in place and keeps
//! only its own working-tree state. Commits from any working tree go into the
//! origin's history, which orders them.

use std::path::Path;

use crate::error::{IoContext, LocalError, Result};
use crate::repository::{Config, Repository};

/// Make `destination` a working tree of the repository at `origin`, limited
/// to the `include` subtrees when any are given.
pub async fn clone_repository(
    origin: &Path,
    destination: &Path,
    author: Option<&str>,
    include: &[String],
    hold: bool,
) -> Result<Repository> {
    let origin = origin
        .canonicalize()
        .at("resolve clone origin", origin)?;
    let store = origin.join(".synck");
    reject_network_filesystem(&store)?;
    let config_path = store.join("config.json");
    let bytes = std::fs::read(&config_path).at("read", &config_path)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if value.get("origin").is_some() {
        return Err(LocalError::Invalid(
            "a clone cannot be cloned; clone its origin instead".into(),
        ));
    }
    // A working origin finishes its own interrupted operations and records
    // which commit its files are based on before a second writer exists.
    if store.join("checkout.json").exists() {
        drop(Repository::discover(&origin).await?);
    }
    let include = crate::checkout::normalize_paths(include)?;
    let portable = crate::portable::PortableRepository::open(&origin)?;
    for path in &include {
        portable.item_at_path(path)?;
    }
    let (authority, origin_author) = portable.clone_source()?;
    let author = author.map(str::to_string).or(origin_author).ok_or_else(|| {
        LocalError::Invalid("the origin has no author; give one with --author".into())
    })?;
    let (repo_id, storage_mode, mut head) = (
        portable.repo_id().clone(),
        portable.storage_mode(),
        portable.head_repo_seq()?,
    );
    drop(portable);

    create_empty_directory(destination)?;
    let destination = destination
        .canonicalize()
        .at("resolve clone destination", destination)?;
    let metadata = destination.join(".synck");
    std::fs::create_dir(&metadata).at("create clone metadata", &metadata)?;
    std::fs::create_dir(metadata.join("tmp")).at("create clone metadata", &metadata)?;
    for name in ["lock", crate::repository::MONITOR_LOCK_FILE] {
        let path = metadata.join(name);
        std::fs::write(&path, b"").at("create", &path)?;
    }
    if hold {
        // A held clone's commits stay in its own copy of the history until
        // pushed. The copy is one consistent read of the origin's.
        let history = metadata.join("repository.sqlite3");
        let path = history.to_str().ok_or_else(|| {
            LocalError::Invalid("clone destination path is not valid UTF-8".into())
        })?;
        let origin_history = synck_vcs_sqlite::SqliteStorage::new(store.join("repository.sqlite3"));
        origin_history
            .open_readonly()
            .map_err(|error| LocalError::Integrity(format!("cannot read repository history: {error}")))?
            .execute("VACUUM INTO ?1", [path])?;
        rusqlite::Connection::open(&history)?.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        head = crate::repository::head_repo_seq(
            &synck_vcs_sqlite::SqliteStorage::new(history),
            &repo_id,
        )?;
        crate::commands::write_held(
            &metadata,
            &crate::commands::Held {
                origin_seq: head,
                pushed_seq: head,
            },
        )?;
    }
    let checkout = crate::checkout::Checkout {
        paths: include,
        hidden_files: Default::default(),
        pending: Some(crate::checkout::Transition {
            previous_paths: None,
        }),
        baseline: Some(head),
    };
    let source = metadata.join(crate::checkout::CHECKOUT_SOURCE_FILE);
    std::fs::write(&source, b"[]").at("write", &source)?;
    crate::checkout::write_checkout(&metadata, &checkout)?;
    // The config names the origin, so it goes last: before it, the directory
    // opens as nothing.
    let config = Config::clone_of(origin, repo_id, author, authority, checkout, storage_mode, hold);
    let path = metadata.join("tmp").join("config.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&config)?).at("write", &path)?;
    crate::repository::sync_file(&path)?;
    crate::repository::replace_file(&path, &metadata.join("config.json"))
        .at("publish clone config", &metadata)?;
    crate::repository::sync_directory(&metadata)?;
    let repository = Repository::discover(&destination).await?;
    // Materialization leaves a large write-ahead log; a clone should cost
    // only its working files.
    repository
        .local_db()
        .open_writable()?
        .execute_batch("PRAGMA main.wal_checkpoint(TRUNCATE)")?;
    Ok(repository)
}

fn create_empty_directory(path: &Path) -> Result<()> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if std::fs::read_dir(path).at("read", path)?.next().is_some() {
                return Err(LocalError::Invalid(format!(
                    "clone destination is not empty: {}",
                    path.display()
                )));
            }
            Ok(())
        }
        Err(source) => Err(LocalError::Io {
            action: "create clone destination",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn network_filesystem(path: &Path) -> LocalError {
    LocalError::Invalid(format!(
        "{} is on a network filesystem; SQLite cannot lock a repository safely there. \
         Keep the origin on a local disk",
        path.display()
    ))
}

/// SQLite's locks are unreliable on SMB, NFS, AFP and WebDAV, so a repository
/// shared there would corrupt.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn reject_network_filesystem(path: &Path) -> Result<()> {
    let statfs = rustix::fs::statfs(path).map_err(std::io::Error::from).at("inspect filesystem", path)?;
    if statfs.f_flags & libc::MNT_LOCAL as u32 == 0 {
        return Err(network_filesystem(path));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn reject_network_filesystem(path: &Path) -> Result<()> {
    const NETWORK: [i64; 6] = [
        0x6969,             // NFS
        0x517B,             // SMB
        0xFF53_4D42,        // CIFS
        0xFE53_4D42,        // SMB2
        0x5346_414F,        // AFS
        0x7375_7245,        // Coda
    ];
    let statfs = rustix::fs::statfs(path).map_err(std::io::Error::from).at("inspect filesystem", path)?;
    if NETWORK.contains(&(statfs.f_type as i64)) {
        return Err(network_filesystem(path));
    }
    Ok(())
}

#[cfg(windows)]
fn reject_network_filesystem(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    const DRIVE_REMOTE: u32 = 4;
    let text = path.to_string_lossy();
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if text.starts_with("UNC\\") || text.starts_with(r"\\") {
        return Err(network_filesystem(path));
    }
    let root: std::path::PathBuf = text.chars().take(3).collect::<String>().into();
    let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
    if unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) } == DRIVE_REMOTE {
        return Err(network_filesystem(path));
    }
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd", target_os = "linux", windows)))]
fn reject_network_filesystem(_: &Path) -> Result<()> {
    Ok(())
}
