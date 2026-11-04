//! `server.lock`: one serving process per state root.

use std::fs::{self, File, OpenOptions};
use std::path::Path;

use super::{Error, Result};

pub struct LifetimeLock {
    _file: File,
}

impl LifetimeLock {
    pub fn initialize(state_root: &Path) -> Result<Self> {
        validate_absolute(state_root)?;
        match create_private_directory(state_root) {
            Ok(()) => sync_parent(state_root)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(Error::filesystem("create state root", state_root, error)),
        }
        validate_root(state_root)?;
        Self::acquire_inner(state_root, true)
    }

    pub fn acquire(state_root: &Path) -> Result<Self> {
        validate_absolute(state_root)?;
        validate_root(state_root)?;
        Self::acquire_inner(state_root, false)
    }

    fn acquire_inner(state_root: &Path, may_create: bool) -> Result<Self> {
        let path = state_root.join("server.lock");
        let file = open_lock_file(&path, may_create)?;
        let metadata = file
            .metadata()
            .map_err(|error| Error::filesystem("inspect lock file", &path, error))?;
        if !metadata.is_file() {
            return Err(Error::State(
                "server.lock is not a regular file".to_string(),
            ));
        }
        validate_owner(&metadata)?;
        validate_single_link(&metadata)?;
        try_lock(&file).map_err(|error| match error.kind() {
            std::io::ErrorKind::WouldBlock => Error::Locked(path.clone()),
            _ => Error::filesystem("lock server state", &path, error),
        })?;
        validate_lock_identity(&path, &file, &metadata)?;
        Ok(Self { _file: file })
    }
}

fn validate_absolute(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::State("state root must be absolute".to_string()));
    }
    Ok(())
}

fn validate_root(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| Error::filesystem("inspect state root", path, error))?;
    if !metadata.file_type().is_dir() {
        return Err(Error::State(
            "state root is not a real directory".to_string(),
        ));
    }
    validate_owner(&metadata)
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).create(path)
}

#[cfg(windows)]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    fs::create_dir(path)
}

#[cfg(unix)]
fn validate_owner(metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if metadata.uid() != rustix::process::getuid().as_raw() {
        return Err(Error::State(
            "state root and lock file must be owned by this user".to_string(),
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(Error::State(
            "state root and lock file must not be group/world accessible".to_string(),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_owner(_metadata: &fs::Metadata) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_single_link(metadata: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    if metadata.nlink() != 1 {
        return Err(Error::State(
            "server.lock must have exactly one filesystem link".to_string(),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_single_link(_metadata: &fs::Metadata) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn validate_lock_identity(path: &Path, _file: &File, opened: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt as _;

    let current = fs::symlink_metadata(path)
        .map_err(|error| Error::filesystem("reinspect server lock", path, error))?;
    if !current.file_type().is_file()
        || current.dev() != opened.dev()
        || current.ino() != opened.ino()
    {
        return Err(Error::State(
            "server.lock changed while it was being acquired".to_string(),
        ));
    }
    Ok(())
}

/// Stable Rust has no file index in `Metadata` on Windows, so both handles
/// are asked directly.
#[cfg(windows)]
fn validate_lock_identity(path: &Path, file: &File, _opened: &fs::Metadata) -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        GetFileInformationByHandle,
    };

    let identity = |file: &File| -> Result<(u32, u32, u32, u32)> {
        // SAFETY: the handle is open for the call and `info` is a plain struct.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
            let error = std::io::Error::last_os_error();
            return Err(Error::filesystem("reinspect server lock", path, error));
        }
        Ok((
            info.dwFileAttributes,
            info.dwVolumeSerialNumber,
            info.nFileIndexHigh,
            info.nFileIndexLow,
        ))
    };
    let current = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|error| Error::filesystem("reinspect server lock", path, error))?;
    if identity(&current)? != identity(file)? {
        return Err(Error::State(
            "server.lock changed while it was being acquired".to_string(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn open_lock_file(path: &Path, may_create: bool) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW);
    if may_create {
        options.create(true).mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| Error::filesystem("open server lock", path, error))
}

#[cfg(windows)]
fn open_lock_file(path: &Path, may_create: bool) -> Result<File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .create(may_create);
    let file = options
        .open(path)
        .map_err(|error| Error::filesystem("open server lock", path, error))?;
    let metadata = file
        .metadata()
        .map_err(|error| Error::filesystem("inspect server lock", path, error))?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(Error::State(
            "server.lock must not be a reparse point".to_string(),
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn try_lock(file: &File) -> std::io::Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
        .map_err(std::io::Error::from)
}

#[cfg(windows)]
fn try_lock(file: &File) -> std::io::Result<()> {
    use std::mem::zeroed;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, GetLastError};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    // SAFETY: an all-zero OVERLAPPED selects a synchronous byte-range lock.
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    // SAFETY: `file` owns a valid handle and `overlapped` outlives this call.
    let locked = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if locked != 0 {
        return Ok(());
    }
    // SAFETY: this reads the calling thread's immediately preceding error.
    let code = unsafe { GetLastError() };
    if code == ERROR_LOCK_VIOLATION {
        Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
    } else {
        Err(std::io::Error::from_raw_os_error(code as i32))
    }
}

fn sync_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::State("state root has no parent".to_string()))?;
    File::open(parent)
        .and_then(|file| file.sync_all())
        .map_err(|error| Error::filesystem("sync state-root parent", parent, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_excludes_second_owner_and_survives_handle_lifetime() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("state");
        let first = LifetimeLock::initialize(&root).unwrap();
        assert!(matches!(
            LifetimeLock::acquire(&root),
            Err(Error::Locked(_))
        ));
        drop(first);
        LifetimeLock::acquire(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lock_refuses_symlink() {
        use std::os::unix::fs::symlink;
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("state");
        fs::create_dir(&root).unwrap();
        let target = parent.path().join("target");
        File::create(&target).unwrap();
        symlink(&target, root.join("server.lock")).unwrap();
        assert!(LifetimeLock::acquire(&root).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn lock_refuses_hard_link() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("state");
        let first = LifetimeLock::initialize(&root).unwrap();
        drop(first);
        fs::hard_link(root.join("server.lock"), parent.path().join("other-link")).unwrap();
        assert!(LifetimeLock::acquire(&root).is_err());
    }
}
