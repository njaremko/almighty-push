use almighty_push::domain::{HeadRef, LimitValues, Limits, RemoteName, RepositoryId, Scope};
use almighty_push::lock::{LockError, RepositoryLock};
use almighty_push::state::{StateError, StateStore};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HELPER_ENV: &str = "ALMIGHTY_PUSH_LOCK_HELPER";

fn limits(wait_ms: u64) -> Limits {
    Limits::new(LimitValues {
        lock_wait_ms: wait_ms,
        ..LimitValues::default()
    })
    .unwrap()
}

fn scope() -> Scope {
    let repository = RepositoryId::parse("github.com/owner/project").unwrap();
    Scope::new(
        repository.clone(),
        repository,
        RemoteName::parse("origin").unwrap(),
        HeadRef::parse("main").unwrap(),
        "@".to_owned(),
    )
    .unwrap()
}

#[test]
fn contention_stops_at_the_configured_deadline() {
    let (root, store) = store("contention");
    let first = RepositoryLock::acquire(&store, limits(100)).unwrap();
    let started = Instant::now();
    let error = RepositoryLock::acquire(&store, limits(100)).unwrap_err();
    let elapsed = started.elapsed();
    assert!(matches!(error, LockError::Contended { .. }));
    assert!(elapsed >= Duration::from_millis(90));
    assert!(elapsed < Duration::from_secs(1));
    drop(first);
    RepositoryLock::acquire(&store, limits(100)).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn lock_is_released_when_a_process_exits() {
    let (root, store) = store("process-exit");
    let ready = store.state_directory_path().join("ready");
    let executable = std::env::current_exe().unwrap();
    let mut child = Command::new(executable)
        .arg("--exact")
        .arg("lock_helper_process")
        .arg("--nocapture")
        .env(HELPER_ENV, &root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if ready.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(ready.exists());
    assert!(matches!(
        RepositoryLock::acquire(&store, limits(100)),
        Err(LockError::Contended { .. })
    ));
    child.kill().unwrap();
    child.wait().unwrap();
    RepositoryLock::acquire(&store, limits(1_000)).unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn lock_helper_process() {
    let Some(root) = std::env::var_os(HELPER_ENV) else {
        return;
    };
    let store = StateStore::open(Path::new(&root), scope(), limits(1_000)).unwrap();
    let _lock = RepositoryLock::acquire(&store, limits(1_000)).unwrap();
    fs::write(store.state_directory_path().join("ready"), "ready").unwrap();
    std::thread::sleep(Duration::from_secs(30));
}

#[test]
fn retained_state_directory_prevents_parent_path_replacement() {
    let (root, store) = store("parent-replacement");
    let original = store.state_directory_path().to_owned();
    let moved = root.join("moved-state");
    let _lock = RepositoryLock::acquire(&store, limits(100)).unwrap();
    fs::rename(&original, &moved).unwrap();
    fs::create_dir(&original).unwrap();

    assert!(matches!(
        RepositoryLock::acquire(&store, limits(100)),
        Err(LockError::State(StateError::NamespaceDrift { .. }))
    ));
    assert!(moved.join("lock").is_file());
    assert!(!original.join("lock").exists());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn symlinked_lock_path_is_rejected() {
    use std::os::unix::fs::symlink;

    let (root, store) = store("symlink");
    let target = store.state_directory_path().join("target");
    fs::write(&target, "").unwrap();
    symlink(&target, store.state_directory_path().join("lock")).unwrap();
    assert!(matches!(
        RepositoryLock::acquire(&store, limits(100)),
        Err(LockError::UnsafePath { .. })
    ));
    fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn shared_or_permissive_lock_inode_is_rejected() {
    use std::os::unix::fs::PermissionsExt;

    let (root, store) = store("hardlink");
    let lock_path = store.state_directory_path().join("lock");
    fs::write(&lock_path, "").unwrap();
    fs::hard_link(&lock_path, store.state_directory_path().join("alias")).unwrap();
    assert!(matches!(
        RepositoryLock::acquire(&store, limits(100)),
        Err(LockError::UnsafePath { .. })
    ));
    fs::remove_file(store.state_directory_path().join("alias")).unwrap();
    fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o666)).unwrap();
    assert!(matches!(
        RepositoryLock::acquire(&store, limits(100)),
        Err(LockError::UnsafePath { .. })
    ));
    fs::remove_dir_all(root).unwrap();
}

fn store(label: &str) -> (PathBuf, StateStore) {
    let root = unique_directory(label);
    fs::create_dir(root.join(".jj")).unwrap();
    let store = StateStore::open(&root, scope(), limits(1_000)).unwrap();
    (root, store)
}

fn unique_directory(label: &str) -> PathBuf {
    for sequence in 0..1000 {
        let path = std::env::temp_dir().join(format!(
            "almighty-push-lock-{label}-{}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return path.canonicalize().unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("fixture creation failed: {error}"),
        }
    }
    panic!("fixture attempts exhausted")
}
