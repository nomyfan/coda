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

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::Arc;

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

const DIR_OPEN_FLAGS: libc::c_int =
    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// An opened directory fd, confined: descendants are reached only fd-relative.
#[derive(Clone)]
pub struct ArchiveDir {
    fd: Arc<OwnedFd>,
}

impl ArchiveDir {
    /// Open (creating the path if needed) the session archive root. The path is
    /// server-controlled, not model-controlled; intermediate components are
    /// created with `create_dir_all`, then the leaf is opened `O_NOFOLLOW` and
    /// `fstat`-verified as a directory we own, and forced to `0700`.
    pub fn open_or_create_root(path: &Path) -> Result<Self, ArchiveError> {
        std::fs::create_dir_all(path)?;
        let cpath = cstring(path.as_os_str().as_encoded_bytes())?;
        // SAFETY: `cpath` is a valid NUL-terminated C string; flags forbid
        // following a symlinked leaf.
        let raw = unsafe { libc::open(cpath.as_ptr(), DIR_OPEN_FLAGS) };
        let fd = owned_or_err(raw)?;
        verify_dir(fd.as_raw_fd())?;
        // Force restrictive perms on the leaf (create_dir_all honours umask).
        // SAFETY: fd is an open directory we just verified we own.
        if unsafe { libc::fchmod(fd.as_raw_fd(), 0o700) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(ArchiveDir { fd: Arc::new(fd) })
    }

    #[cfg(test)]
    pub(crate) fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Lazily enumerate direct children, one classified entry at a time, without
    /// materialising the whole directory. Never follows symlinks.
    pub fn entries(&self) -> Result<ArchiveEntries, ArchiveError> {
        ArchiveEntries::open(self.fd.as_raw_fd())
    }

    /// Open a direct child directory named by a validated [`TaskId`]. Uses
    /// `O_NOFOLLOW`, then `fstat`-verifies directory + owner + `0700`.
    pub fn open_dir(&self, name: &TaskId) -> Result<ArchiveDir, ArchiveError> {
        let cname = cstring(name.as_str().as_bytes())?;
        // SAFETY: openat relative to our verified dir fd; O_NOFOLLOW blocks a
        // symlinked child.
        let raw = unsafe { libc::openat(self.fd.as_raw_fd(), cname.as_ptr(), DIR_OPEN_FLAGS) };
        let fd = owned_or_err(raw)?;
        verify_dir(fd.as_raw_fd())?;
        verify_mode(fd.as_raw_fd(), 0o700)?;
        Ok(ArchiveDir { fd: Arc::new(fd) })
    }

    /// `mkdirat(0700)` a fresh task directory then open and verify it. Fails if
    /// the name already exists (ids are unique).
    pub fn create_dir(&self, name: &TaskId) -> Result<ArchiveDir, ArchiveError> {
        let cname = cstring(name.as_str().as_bytes())?;
        // SAFETY: mkdirat relative to our dir fd with an explicit mode.
        if unsafe { libc::mkdirat(self.fd.as_raw_fd(), cname.as_ptr(), 0o700) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        self.open_dir(name)
    }

    /// Create a `0600` regular file with `O_CREAT|O_EXCL|O_NOFOLLOW`, then
    /// `fstat`-verify regular + owner + mode. Opened `O_RDWR` so ring files can
    /// be `pread`/`pwrite`n.
    pub fn create_file(&self, name: ArchiveFileName) -> Result<std::fs::File, ArchiveError> {
        let cname = cstring(name.as_str().as_bytes())?;
        let flags =
            libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: openat relative to our dir fd; mode applies because O_CREAT.
        let raw = unsafe {
            libc::openat(
                self.fd.as_raw_fd(),
                cname.as_ptr(),
                flags,
                0o600 as libc::c_uint,
            )
        };
        let fd = owned_or_err(raw)?;
        verify_regular(fd.as_raw_fd())?;
        verify_mode(fd.as_raw_fd(), 0o600)?;
        // SAFETY: OwnedFd guarantees a valid, exclusively owned descriptor.
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
        let cname = cstring(name.as_str().as_bytes())?;
        let access = if write { libc::O_RDWR } else { libc::O_RDONLY };
        let flags = access | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: openat relative to our dir fd; O_NOFOLLOW blocks a swapped
        // symlink at this name.
        let raw = unsafe { libc::openat(self.fd.as_raw_fd(), cname.as_ptr(), flags) };
        let fd = owned_or_err(raw)?;
        verify_regular(fd.as_raw_fd())?;
        verify_mode(fd.as_raw_fd(), 0o600)?;
        Ok(std::fs::File::from(fd))
    }

    /// `renameat` within this directory — the manifest temp→final commit step.
    pub fn rename(&self, from: ArchiveFileName, to: ArchiveFileName) -> Result<(), ArchiveError> {
        let cfrom = cstring(from.as_str().as_bytes())?;
        let cto = cstring(to.as_str().as_bytes())?;
        // SAFETY: both names resolved relative to the same dir fd.
        let rc = unsafe {
            libc::renameat(
                self.fd.as_raw_fd(),
                cfrom.as_ptr(),
                self.fd.as_raw_fd(),
                cto.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    /// `unlinkat` a known file. `ENOENT` is reported as `Ok(false)` so callers
    /// can treat an already-absent target as done.
    pub fn unlink(&self, name: ArchiveFileName) -> Result<bool, ArchiveError> {
        let cname = cstring(name.as_str().as_bytes())?;
        // SAFETY: unlinkat relative to our dir fd, no AT_REMOVEDIR (files only).
        let rc = unsafe { libc::unlinkat(self.fd.as_raw_fd(), cname.as_ptr(), 0) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(false);
            }
            return Err(err.into());
        }
        Ok(true)
    }

    /// Remove an (already emptied) task directory by validated id. Used by the
    /// temporary-registry teardown; the session-backed path keeps the dir.
    pub fn remove_dir(&self, name: &TaskId) -> Result<bool, ArchiveError> {
        let cname = cstring(name.as_str().as_bytes())?;
        // SAFETY: unlinkat with AT_REMOVEDIR removes the child directory.
        let rc = unsafe { libc::unlinkat(self.fd.as_raw_fd(), cname.as_ptr(), libc::AT_REMOVEDIR) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOENT) {
                return Ok(false);
            }
            return Err(err.into());
        }
        Ok(true)
    }
}

/// Streaming directory iterator backed by an owned `DIR*`.
pub struct ArchiveEntries {
    dir: *mut libc::DIR,
}

// The `DIR*` is exclusively owned by this value; it is only touched behind `&mut
// self`, so moving it across threads is sound.
unsafe impl Send for ArchiveEntries {}

impl ArchiveEntries {
    fn open(dirfd: RawFd) -> Result<Self, ArchiveError> {
        // fdopendir consumes the fd it's given, so hand it a private duplicate
        // (openat ".") rather than the capability's own fd.
        let dot = cstring(b".").expect("`.` is a valid C string");
        // SAFETY: reopen the directory itself relative to its own fd.
        let raw = unsafe { libc::openat(dirfd, dot.as_ptr(), DIR_OPEN_FLAGS) };
        if raw < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: `raw` is a fresh directory fd; fdopendir takes ownership.
        let dir = unsafe { libc::fdopendir(raw) };
        if dir.is_null() {
            let err = io::Error::last_os_error();
            // SAFETY: fdopendir failed, so we still own `raw`.
            unsafe { libc::close(raw) };
            return Err(err.into());
        }
        Ok(ArchiveEntries { dir })
    }
}

impl Iterator for ArchiveEntries {
    type Item = Result<ArchiveEntry, ArchiveError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // errno must be cleared to distinguish end-of-stream from error.
            // SAFETY: writing 0 to the thread's errno slot.
            unsafe { *errno_location() = 0 };
            // SAFETY: `self.dir` is a live DIR* we exclusively own.
            let ent = unsafe { libc::readdir(self.dir) };
            if ent.is_null() {
                let err = io::Error::last_os_error();
                return match err.raw_os_error() {
                    Some(0) | None => None, // clean end of stream
                    _ => Some(Err(err.into())),
                };
            }
            // SAFETY: readdir returned a valid dirent for this iteration.
            let (name, d_type) = unsafe {
                let name_ptr = (*ent).d_name.as_ptr();
                let cstr = std::ffi::CStr::from_ptr(name_ptr);
                (cstr.to_bytes().to_vec(), (*ent).d_type)
            };
            if name == b"." || name == b".." {
                continue;
            }
            let name = String::from_utf8_lossy(&name).into_owned();
            let kind = match d_type {
                libc::DT_DIR => EntryKind::Dir,
                libc::DT_REG => EntryKind::File,
                libc::DT_LNK => EntryKind::Symlink,
                libc::DT_UNKNOWN => EntryKind::Unknown,
                _ => EntryKind::Other,
            };
            return Some(Ok(ArchiveEntry { name, kind }));
        }
    }
}

impl Drop for ArchiveEntries {
    fn drop(&mut self) {
        // SAFETY: closedir closes the DIR* (and its fd) exactly once.
        unsafe { libc::closedir(self.dir) };
    }
}

/// Thread-local errno slot, spelled differently per libc.
#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

fn cstring(bytes: &[u8]) -> Result<CString, ArchiveError> {
    CString::new(bytes).map_err(|_| ArchiveError::corrupt("name contains an interior NUL byte"))
}

fn owned_or_err(raw: RawFd) -> Result<OwnedFd, ArchiveError> {
    if raw < 0 {
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: `raw` is a fresh, exclusively owned descriptor from open/openat.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn fstat(fd: RawFd) -> Result<libc::stat, ArchiveError> {
    // SAFETY: zeroed stat is a valid out-parameter; fd is open.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(st)
}

fn verify_dir(fd: RawFd) -> Result<(), ArchiveError> {
    let st = fstat(fd)?;
    if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(ArchiveError::corrupt("expected a directory"));
    }
    verify_owner(&st)
}

fn verify_regular(fd: RawFd) -> Result<(), ArchiveError> {
    let st = fstat(fd)?;
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(ArchiveError::corrupt("expected a regular file"));
    }
    verify_owner(&st)
}

fn verify_owner(st: &libc::stat) -> Result<(), ArchiveError> {
    // SAFETY: geteuid is always safe.
    let euid = unsafe { libc::geteuid() };
    if st.st_uid != euid {
        return Err(ArchiveError::corrupt(
            "archive entry not owned by this user",
        ));
    }
    Ok(())
}

fn verify_mode(fd: RawFd, expected: libc::mode_t) -> Result<(), ArchiveError> {
    let st = fstat(fd)?;
    if st.st_mode & 0o777 != expected {
        return Err(ArchiveError::corrupt(format!(
            "unexpected permissions {:o}, wanted {:o}",
            st.st_mode & 0o777,
            expected
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
        verify_mode(reopened.raw_fd(), 0o700).unwrap();
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
