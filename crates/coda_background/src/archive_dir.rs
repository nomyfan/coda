//! `ArchiveDir` — an opened, fd-confined capability over one session's
//! `background/tasks` directory (design: "Archive path safety").
//!
//! The security contract: every descendant operation is relative to a held
//! directory fd via `openat`/`mkdirat`/`renameat`/`unlinkat` with `O_NOFOLLOW`,
//! and the opened fd is `fstat`-verified for type, owner and mode. A path is
//! never re-resolved from an ambient string between check and use, so a
//! concurrently swapped symlink (TOCTOU) cannot redirect an operation outside
//! the archive. Model-controlled names arrive only as validated [`TaskId`]s;
//! the fixed archive files are named by the closed [`ArchiveFileName`] set.
//!
//! Methods are synchronous syscalls; async callers offload them to the blocking
//! pool. The capability is cheap to clone (`Arc<OwnedFd>`).

use std::io;
use std::os::fd::{AsFd, OwnedFd};
#[cfg(test)]
use std::os::fd::{AsRawFd, RawFd};
use std::path::Path;
use std::sync::Arc;

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, RawMode, Stat};
use rustix::io::Errno;
use rustix::process::geteuid;

use super::task_id::TaskId;

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

/// The closed set of files that may live in a task directory. Keeping it an
/// enum means create/rename/unlink never take an arbitrary caller string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFileName {
    Meta,
    MetaTmp,
    StdoutRing,
    StderrRing,
}

impl ArchiveFileName {
    pub fn as_str(self) -> &'static str {
        match self {
            ArchiveFileName::Meta => "meta.json",
            ArchiveFileName::MetaTmp => "meta.json.tmp",
            ArchiveFileName::StdoutRing => "stdout.ring",
            ArchiveFileName::StderrRing => "stderr.ring",
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

fn dir_oflags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

impl ArchiveDir {
    /// Open (creating the path if needed) the session archive root. The path is
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

    #[cfg(test)]
    pub(crate) fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Lazily enumerate direct children, one classified entry at a time, without
    /// materialising the whole directory. Never follows symlinks.
    pub fn entries(&self) -> Result<ArchiveEntries, ArchiveError> {
        Ok(ArchiveEntries {
            dir: fs::Dir::read_from(self.fd.as_fd())?,
        })
    }

    /// Open a direct child directory named by a validated [`TaskId`]. Uses
    /// `O_NOFOLLOW`, then `fstat`-verifies directory + owner + `0700`.
    pub fn open_dir(&self, name: &TaskId) -> Result<ArchiveDir, ArchiveError> {
        let fd = fs::openat(self.fd.as_fd(), name.as_str(), dir_oflags(), Mode::empty())?;
        verify_dir(&fd)?;
        verify_mode(&fd, 0o700)?;
        Ok(ArchiveDir { fd: Arc::new(fd) })
    }

    /// `mkdirat(0700)` a fresh task directory then open and verify it. Fails if
    /// the name already exists (ids are unique).
    pub fn create_dir(&self, name: &TaskId) -> Result<ArchiveDir, ArchiveError> {
        fs::mkdirat(self.fd.as_fd(), name.as_str(), Mode::from_raw_mode(0o700))?;
        self.open_dir(name)
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
    pub fn remove_dir(&self, name: &TaskId) -> Result<bool, ArchiveError> {
        match fs::unlinkat(self.fd.as_fd(), name.as_str(), AtFlags::REMOVEDIR) {
            Ok(()) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(e) => Err(e.into()),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;

    fn temp_root() -> (tempfile::TempDir, ArchiveDir) {
        let dir = tempfile::tempdir().unwrap();
        let root = ArchiveDir::open_or_create_root(&dir.path().join("background/tasks")).unwrap();
        (dir, root)
    }

    #[test]
    fn create_open_and_verify_perms() {
        let (_tmp, root) = temp_root();
        let id = TaskId::new();
        let task = root.create_dir(&id).unwrap();
        let mut f = task.create_file(ArchiveFileName::StdoutRing).unwrap();
        f.write_all(b"hi").unwrap();
        drop(f);

        let mut r = task.open_file(ArchiveFileName::StdoutRing, false).unwrap();
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        assert_eq!(s, "hi");

        // Reopen the task dir by id and confirm the mode is 0700.
        let reopened = root.open_dir(&id).unwrap();
        // SAFETY: `reopened` keeps the fd alive for the duration of this call.
        let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(reopened.raw_fd()) };
        verify_mode(borrowed, 0o700).unwrap();
    }

    #[test]
    fn reopen_rejects_widened_file_permissions() {
        let (tmp, root) = temp_root();
        let id = TaskId::new();
        let task = root.create_dir(&id).unwrap();
        task.create_file(ArchiveFileName::Meta).unwrap();
        let path = tmp
            .path()
            .join("background/tasks")
            .join(id.as_str())
            .join("meta.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            task.open_file(ArchiveFileName::Meta, false),
            Err(ArchiveError::Corrupt(_))
        ));
    }

    #[test]
    fn create_dir_rejects_duplicate() {
        let (_tmp, root) = temp_root();
        let id = TaskId::new();
        root.create_dir(&id).unwrap();
        assert!(matches!(
            root.create_dir(&id),
            Err(ArchiveError::Io(_)) // EEXIST
        ));
    }

    #[test]
    fn rename_and_unlink() {
        let (_tmp, root) = temp_root();
        let id = TaskId::new();
        let task = root.create_dir(&id).unwrap();
        {
            let mut f = task.create_file(ArchiveFileName::MetaTmp).unwrap();
            f.write_all(b"{}").unwrap();
        }
        task.rename(ArchiveFileName::MetaTmp, ArchiveFileName::Meta)
            .unwrap();
        assert!(task.open_file(ArchiveFileName::Meta, false).is_ok());
        // tmp is gone after the rename.
        assert!(!task.unlink(ArchiveFileName::MetaTmp).unwrap());
        assert!(task.unlink(ArchiveFileName::Meta).unwrap());
        assert!(!task.unlink(ArchiveFileName::Meta).unwrap());
    }

    #[test]
    fn entries_lists_task_dirs() {
        let (_tmp, root) = temp_root();
        let a = TaskId::new();
        let b = TaskId::new();
        root.create_dir(&a).unwrap();
        root.create_dir(&b).unwrap();
        let mut names: Vec<String> = root.entries().unwrap().map(|e| e.unwrap().name).collect();
        names.sort();
        let mut want = vec![a.as_str().to_owned(), b.as_str().to_owned()];
        want.sort();
        assert_eq!(names, want);
    }

    /// A symlinked child named like a task dir cannot be opened as one: the
    /// `O_NOFOLLOW` open refuses to traverse it, so the target is unreachable.
    #[test]
    fn open_dir_refuses_to_follow_symlink() {
        let (tmp, root) = temp_root();
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let id = TaskId::new();
        let link = tmp.path().join("background/tasks").join(id.as_str());
        symlink(&outside, &link).unwrap();
        // The name appears in entries (as a symlink) but cannot be opened.
        assert!(root.open_dir(&id).is_err());
    }

    /// A regular file where a task directory is expected is rejected, not
    /// silently treated as an empty task.
    #[test]
    fn open_dir_rejects_non_directory() {
        let (tmp, root) = temp_root();
        let id = TaskId::new();
        let path = tmp.path().join("background/tasks").join(id.as_str());
        std::fs::File::create(&path).unwrap();
        assert!(root.open_dir(&id).is_err());
    }
}
