use super::*;
use coda_core::task::TaskId;

#[test]
fn directory_names_are_confined_without_requiring_a_task_id() {
    let (_tmp, root) = temp_root();
    root.create_dir("objects").unwrap();
    root.open_dir("objects").unwrap();
    for name in ["", ".", "..", "../outside", "/tmp/outside", "a/b", "a\0b"] {
        assert!(root.create_dir(name).is_err(), "accepted {name:?}");
        assert!(root.open_dir(name).is_err(), "opened {name:?}");
        assert!(root.remove_dir(name).is_err(), "removed {name:?}");
    }
}
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::process::{Command, Stdio};
use std::time::Duration;

fn temp_root() -> (tempfile::TempDir, ArchiveDir) {
    let dir = tempfile::tempdir().unwrap();
    let root = ArchiveDir::open_or_create_root(dir.path()).unwrap();
    (dir, root)
}

#[test]
fn background_root_lock_creates_strict_root_and_lock_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("background");
    let lock = ArchiveRootLock::acquire(&path).unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(path.join(".lock"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    drop(lock);
    assert!(path.join(".lock").is_file(), "lock inode stays stable");
}

#[test]
fn background_root_lock_rejects_unsafe_root_and_lock_entries() {
    let tmp = tempfile::tempdir().unwrap();

    let wrong_mode = tmp.path().join("wrong-mode");
    std::fs::create_dir(&wrong_mode).unwrap();
    std::fs::set_permissions(&wrong_mode, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        ArchiveRootLock::acquire(&wrong_mode),
        Err(ArchiveError::Corrupt(_))
    ));

    let target = tmp.path().join("target");
    std::fs::create_dir(&target).unwrap();
    let linked_root = tmp.path().join("linked-root");
    symlink(&target, &linked_root).unwrap();
    assert!(ArchiveRootLock::acquire(&linked_root).is_err());

    let linked_lock_root = tmp.path().join("linked-lock-root");
    std::fs::create_dir(&linked_lock_root).unwrap();
    std::fs::set_permissions(&linked_lock_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let outside = tmp.path().join("outside-lock");
    std::fs::File::create(&outside).unwrap();
    symlink(&outside, linked_lock_root.join(".lock")).unwrap();
    assert!(ArchiveRootLock::acquire(&linked_lock_root).is_err());

    let directory_lock_root = tmp.path().join("directory-lock-root");
    std::fs::create_dir(&directory_lock_root).unwrap();
    std::fs::set_permissions(&directory_lock_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::create_dir(directory_lock_root.join(".lock")).unwrap();
    assert!(ArchiveRootLock::acquire(&directory_lock_root).is_err());

    let wide_lock_root = tmp.path().join("wide-lock-root");
    std::fs::create_dir(&wide_lock_root).unwrap();
    std::fs::set_permissions(&wide_lock_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let wide_lock = wide_lock_root.join(".lock");
    std::fs::File::create(&wide_lock).unwrap();
    std::fs::set_permissions(&wide_lock, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        ArchiveRootLock::acquire(&wide_lock_root),
        Err(ArchiveError::Corrupt(_))
    ));
}

#[test]
fn background_root_lock_child_helper() {
    let Some(root) = std::env::var_os("CODA_BACKGROUND_LOCK_CHILD_ROOT") else {
        return;
    };
    let _lock = ArchiveRootLock::acquire(Path::new(&root)).unwrap();
    println!("CODA_BACKGROUND_LOCK_READY");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn background_root_lock_relative_root_child_helper() {
    if std::env::var_os("CODA_BACKGROUND_LOCK_RELATIVE_CHILD").is_none() {
        return;
    }
    let _lock = ArchiveRootLock::acquire(Path::new("background")).unwrap();
    assert!(Path::new("background/.lock").is_file());
}

#[test]
fn background_root_lock_accepts_a_root_relative_to_the_working_directory() {
    // `background.root = "background"` beside a config named by a bare
    // filename resolves to this shape: one component, empty parent. A child
    // process because such a path means nothing without a cwd, and every
    // test in a binary shares one.
    let tmp = tempfile::tempdir().unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "archive_dir::tests::background_root_lock_relative_root_child_helper",
            "--nocapture",
        ])
        .env("CODA_BACKGROUND_LOCK_RELATIVE_CHILD", "1")
        .current_dir(tmp.path())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "a single-component relative root was refused"
    );
    assert!(tmp.path().join("background").join(".lock").is_file());
}

#[test]
fn background_root_lock_is_exclusive_across_processes_and_released_on_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("background");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "archive_dir::tests::background_root_lock_child_helper",
            "--nocapture",
        ])
        .env("CODA_BACKGROUND_LOCK_CHILD_ROOT", &root)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("CODA_BACKGROUND_LOCK_READY") {
                let _ = ready_tx.send(());
                return;
            }
        }
    });
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("child did not acquire the background root lock");

    assert!(
        ArchiveRootLock::acquire(&root).is_err(),
        "a second process acquired the same root"
    );
    child.kill().unwrap();
    child.wait().unwrap();

    ArchiveRootLock::acquire(&root).expect("kernel did not release the lock when the child exited");
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
    verify_mode(reopened.fd.as_fd(), 0o700).unwrap();
}

#[test]
fn reopen_rejects_widened_file_permissions() {
    let (tmp, root) = temp_root();
    let id = TaskId::new();
    let task = root.create_dir(&id).unwrap();
    task.create_file(ArchiveFileName::Meta).unwrap();
    let path = tmp.path().join(id.as_str()).join("meta.json");
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
    let link = tmp.path().join(id.as_str());
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
    let path = tmp.path().join(id.as_str());
    std::fs::File::create(&path).unwrap();
    assert!(root.open_dir(&id).is_err());
}
