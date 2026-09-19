//! An opened directory capability shared by task archives and tool output storage.
//!
//! The security contract: every descendant operation is relative to a held
//! directory fd via `openat`/`mkdirat`/`renameat`/`unlinkat` with `O_NOFOLLOW`,
//! and the opened fd is `fstat`-verified for type, owner and mode. A path is
//! never re-resolved from an ambient string between check and use, so a
//! concurrently swapped symlink (TOCTOU) cannot redirect an operation outside
//! the archive. Model-controlled names arrive only as validated single-component names;
//! the fixed archive files are named by the closed [`ArchiveFileName`] set.
//!
//! Methods are synchronous syscalls; async callers offload them to the blocking
//! pool. The capability is cheap to clone (`Arc<OwnedFd>`).

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::sync::Arc;

use coda_core::output::Channel;
use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags, RawMode, Stat};
use rustix::io::Errno;
use rustix::process::geteuid;

/// Fault in the archive layer. `Corrupt` marks a structurally invalid on-disk
/// state (wrong type, bad mode) that must not be silently repaired.
#[derive(Debug)]
pub enum ArchiveError {
    Io(io::Error),
    Corrupt(String),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArchiveError::Io(e) => write!(f, "archive I/O error: {e}"),
            ArchiveError::Corrupt(m) => write!(f, "archive corrupt: {m}"),
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<io::Error> for ArchiveError {
    fn from(e: io::Error) -> Self {
        ArchiveError::Io(e)
    }
}

impl From<Errno> for ArchiveError {
    fn from(e: Errno) -> Self {
        ArchiveError::Io(e.into())
    }
}

impl ArchiveError {
    pub fn corrupt(msg: impl Into<String>) -> Self {
        ArchiveError::Corrupt(msg.into())
    }
}

/// The closed set of files that may live in a task directory or an output
/// object. Keeping it an enum means create/rename/unlink never take an
/// arbitrary caller string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFileName {
    OutputOwner,
    /// One channel of an output object, named by [`Channel::file_name`].
    Channel(Channel),
    Meta,
    MetaTmp,
}

impl ArchiveFileName {
    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveFileName::OutputOwner => "owner.json",
            ArchiveFileName::Channel(channel) => channel.file_name(),
            ArchiveFileName::Meta => "meta.json",
            ArchiveFileName::MetaTmp => "meta.json.tmp",
        }
    }
}

/// A classified directory entry yielded by [`ArchiveDir::entries`].
#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    pub name: String,
    pub kind: EntryKind,
}

/// `d_type` hint. `Unknown` means the filesystem did not fill it in; the caller
/// must still open with `O_NOFOLLOW` and `fstat` to classify safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Symlink,
    Other,
    Unknown,
}

/// An opened directory fd, confined: descendants are reached only fd-relative.
#[derive(Clone)]
pub struct ArchiveDir {
    fd: Arc<OwnedFd>,
}

/// Process-wide ownership of an archive root. The verified directory
/// capability and locked file descriptor stay alive together; dropping this
/// value releases the kernel lock but deliberately leaves `.lock` in place so
/// every process incarnation locks the same inode.
pub struct ArchiveRootLock {
    _root: ArchiveDir,
    _lock: OwnedFd,
}

fn dir_oflags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

impl ArchiveDir {
    /// Open an existing archive without creating directories or changing permissions.
    pub fn open_existing_root(path: &Path) -> Result<Option<Self>, ArchiveError> {
        let fd = match fs::open(path, dir_oflags(), Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        verify_dir(&fd)?;
        verify_mode(&fd, 0o700)?;
        Ok(Some(Self { fd: Arc::new(fd) }))
    }

    /// Open (creating the path if needed) an archive root. The path is
    /// server-controlled, not model-controlled; intermediate components are
    /// created with `create_dir_all`, then the leaf is opened `O_NOFOLLOW` and
    /// `fstat`-verified as a directory we own, and forced to `0700`.
    pub fn open_or_create_root(path: &Path) -> Result<Self, ArchiveError> {
        std::fs::create_dir_all(path)?;
        let fd = fs::open(path, dir_oflags(), Mode::empty())?;
        verify_dir(&fd)?;
        // Force restrictive perms on the leaf (create_dir_all honours umask).
        fs::fchmod(&fd, Mode::from_raw_mode(0o700))?;
        Ok(ArchiveDir { fd: Arc::new(fd) })
    }

    /// Lazily enumerate direct children, one classified entry at a time, without
    /// materialising the whole directory. Never follows symlinks.
    pub fn entries(&self) -> Result<ArchiveEntries, ArchiveError> {
        Ok(ArchiveEntries {
            dir: fs::Dir::read_from(self.fd.as_fd())?,
        })
    }

    /// Open a direct child directory named by a single-component name. Uses
    /// `O_NOFOLLOW`, then `fstat`-verifies directory + owner + `0700`.
    pub fn open_dir(&self, name: impl AsRef<str>) -> Result<ArchiveDir, ArchiveError> {
        let fd = fs::openat(
            self.fd.as_fd(),
            component(name.as_ref())?,
            dir_oflags(),
            Mode::empty(),
        )?;
        verify_dir(&fd)?;
        verify_mode(&fd, 0o700)?;
        Ok(ArchiveDir { fd: Arc::new(fd) })
    }

    /// `mkdirat(0700)` a fresh child directory then open and verify it. Fails if
    /// the name already exists (ids are unique).
    pub fn create_dir(&self, name: impl AsRef<str>) -> Result<ArchiveDir, ArchiveError> {
        fs::mkdirat(
            self.fd.as_fd(),
            component(name.as_ref())?,
            Mode::from_raw_mode(0o700),
        )?;
        self.open_dir(name.as_ref())
    }

    /// Create a `0600` regular file with `O_CREAT|O_EXCL|O_NOFOLLOW`, then
    /// `fstat`-verify regular + owner + mode. Opened `O_RDWR` so ring files can
    /// be `pread`/`pwrite`n.
    pub fn create_file(&self, name: ArchiveFileName) -> Result<std::fs::File, ArchiveError> {
        let flags =
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = fs::openat(
            self.fd.as_fd(),
            name.as_str(),
            flags,
            Mode::from_raw_mode(0o600),
        )?;
        verify_regular(&fd)?;
        verify_mode(&fd, 0o600)?;
        Ok(std::fs::File::from(fd))
    }

    /// Open an existing regular file `O_NOFOLLOW`, `fstat`-verifying regular,
    /// owner, and exact `0600` permissions. `write` selects `O_RDWR` (rings)
    /// vs `O_RDONLY` (manifest reads).
    pub fn open_file(
        &self,
        name: ArchiveFileName,
        write: bool,
    ) -> Result<std::fs::File, ArchiveError> {
        let access = if write { OFlags::RDWR } else { OFlags::RDONLY };
        let flags = access | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = fs::openat(self.fd.as_fd(), name.as_str(), flags, Mode::empty())?;
        verify_regular(&fd)?;
        verify_mode(&fd, 0o600)?;
        Ok(std::fs::File::from(fd))
    }

    pub fn sync(&self) -> Result<(), ArchiveError> {
        fs::fsync(self.fd.as_fd())?;
        Ok(())
    }

    /// `renameat` within this directory — the manifest temp→final commit step.
    pub fn rename(&self, from: ArchiveFileName, to: ArchiveFileName) -> Result<(), ArchiveError> {
        fs::renameat(self.fd.as_fd(), from.as_str(), self.fd.as_fd(), to.as_str())?;
        Ok(())
    }

    /// `unlinkat` a known file. `ENOENT` is reported as `Ok(false)` so callers
    /// can treat an already-absent target as done.
    pub fn unlink(&self, name: ArchiveFileName) -> Result<bool, ArchiveError> {
        match fs::unlinkat(self.fd.as_fd(), name.as_str(), AtFlags::empty()) {
            Ok(()) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Remove an (already emptied) task directory by validated id. Used by the
    /// temporary-registry teardown; the session-backed path keeps the dir.
    pub fn remove_dir(&self, name: impl AsRef<str>) -> Result<bool, ArchiveError> {
        match fs::unlinkat(
            self.fd.as_fd(),
            component(name.as_ref())?,
            AtFlags::REMOVEDIR,
        ) {
            Ok(()) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

impl ArchiveRootLock {
    pub fn directory(&self) -> ArchiveDir {
        self._root.clone()
    }

    /// Safely create or strictly open an archive root, then acquire a
    /// non-blocking exclusive lock on its fd-relative `.lock` file.
    ///
    /// A newly-created leaf is requested as `0700` (umask can only make it
    /// narrower), then tightened and verified. An existing leaf must already
    /// be an owned `0700` directory; it is never silently repaired. The lock
    /// file follows the same rule at `0600`, and neither path follows symlinks.
    pub fn acquire(path: &Path) -> Result<Self, ArchiveError> {
        let parent = path.parent().ok_or_else(|| {
            ArchiveError::Io(io::Error::new(
                io::ErrorKind::InvalidInput,
                "archive root has no parent directory",
            ))
        })?;
        std::fs::create_dir_all(parent)?;

        let created = match fs::mkdir(path, Mode::from_raw_mode(0o700)) {
            Ok(()) => true,
            Err(Errno::EXIST) => false,
            Err(error) => return Err(error.into()),
        };
        let fd = fs::open(path, dir_oflags(), Mode::empty())?;
        verify_dir(&fd)?;
        if created {
            fs::fchmod(&fd, Mode::from_raw_mode(0o700))?;
        }
        verify_mode(&fd, 0o700)?;
        let root = ArchiveDir { fd: Arc::new(fd) };

        let create_flags =
            OFlags::CREATE | OFlags::EXCL | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let (lock, lock_created) = match fs::openat(
            root.fd.as_fd(),
            ".lock",
            create_flags,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(fd) => (fd, true),
            Err(Errno::EXIST) => {
                let flags = OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC;
                (
                    fs::openat(root.fd.as_fd(), ".lock", flags, Mode::empty())?,
                    false,
                )
            }
            Err(error) => return Err(error.into()),
        };
        verify_regular(&lock)?;
        if lock_created {
            fs::fchmod(&lock, Mode::from_raw_mode(0o600))?;
        }
        verify_mode(&lock, 0o600)?;
        fs::flock(&lock, FlockOperation::NonBlockingLockExclusive)?;

        Ok(Self {
            _root: root,
            _lock: lock,
        })
    }
}

/// Streaming directory iterator. `rustix::fs::Dir` owns an independent fd
/// (dup'd from ours internally) and closes it on drop.
pub struct ArchiveEntries {
    dir: fs::Dir,
}

impl Iterator for ArchiveEntries {
    type Item = Result<ArchiveEntry, ArchiveError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = match self.dir.next()? {
                Ok(entry) => entry,
                Err(e) => return Some(Err(e.into())),
            };
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let kind = match entry.file_type() {
                FileType::Directory => EntryKind::Dir,
                FileType::RegularFile => EntryKind::File,
                FileType::Symlink => EntryKind::Symlink,
                FileType::Unknown => EntryKind::Unknown,
                _ => EntryKind::Other,
            };
            let name = String::from_utf8_lossy(name).into_owned();
            return Some(Ok(ArchiveEntry { name, kind }));
        }
    }
}

fn verify_dir<Fd: AsFd>(fd: Fd) -> Result<(), ArchiveError> {
    let st = fs::fstat(fd)?;
    if FileType::from_raw_mode(st.st_mode as RawMode) != FileType::Directory {
        return Err(ArchiveError::corrupt("expected a directory"));
    }
    verify_owner(&st)
}

fn verify_regular<Fd: AsFd>(fd: Fd) -> Result<(), ArchiveError> {
    let st = fs::fstat(fd)?;
    if FileType::from_raw_mode(st.st_mode as RawMode) != FileType::RegularFile {
        return Err(ArchiveError::corrupt("expected a regular file"));
    }
    verify_owner(&st)
}

fn verify_owner(st: &Stat) -> Result<(), ArchiveError> {
    if st.st_uid != geteuid().as_raw() {
        return Err(ArchiveError::corrupt(
            "archive entry not owned by this user",
        ));
    }
    Ok(())
}

fn verify_mode<Fd: AsFd>(fd: Fd, expected: RawMode) -> Result<(), ArchiveError> {
    let st = fs::fstat(fd)?;
    let mode = st.st_mode as RawMode & 0o777;
    if mode != expected {
        return Err(ArchiveError::corrupt(format!(
            "unexpected permissions {mode:o}, wanted {expected:o}"
        )));
    }
    Ok(())
}

fn component(name: &str) -> Result<&str, ArchiveError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(ArchiveError::corrupt("expected a single path component"));
    }
    Ok(name)
}

#[cfg(test)]
#[path = "archive_dir_tests.rs"]
mod tests;
