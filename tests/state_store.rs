use almighty_push::domain::{HeadRef, LimitValues, Limits, RemoteName, RepositoryId, Scope};
use almighty_push::plan::{Effect, Plan, ReobserveBarrier};
use almighty_push::state::{CheckpointState, StateError, StateStore, StateV3};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const CHANGE: &str = "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk";
const PERSIST_HELPER_ENV: &str = "ALMIGHTY_PUSH_PERSIST_HELPER";

fn limits() -> Limits {
    Limits::new(LimitValues::default()).unwrap()
}

fn scope(owner: &str) -> Scope {
    let repository = RepositoryId::parse(format!("github.com/{owner}/project")).unwrap();
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
fn missing_state_loads_exact_empty_v3() {
    let root = workspace("missing");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    assert_eq!(store.load().unwrap(), StateV3::empty(scope("owner")));
    cleanup(root);
}

#[test]
fn locked_session_publishes_the_exact_loaded_state() {
    let root = workspace("round-trip");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let mut session = store.lock().unwrap();
    session.persist_loaded().unwrap();
    assert_eq!(store.load().unwrap(), *session.state());
    cleanup(root);
}

#[test]
fn executing_checkpoint_round_trips_and_enforces_effect_and_index_bounds() {
    let root = workspace("checkpoint");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let plan = Plan::new(
        scope("owner"),
        vec![Effect::Reobserve(ReobserveBarrier::Github)].into_boxed_slice(),
        limits(),
    )
    .unwrap();
    let mut session = store.lock().unwrap();
    session.start_plan(plan).unwrap();
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Executing(checkpoint)
            if checkpoint.occurrence() == 1
                && checkpoint.next_effect_index() == 0
                && checkpoint.results() == [None]
    ));

    let too_many =
        vec![Effect::Reobserve(ReobserveBarrier::Github); limits().effect_count_max() + 1]
            .into_boxed_slice();
    assert!(Plan::new(scope("owner"), too_many, limits()).is_err());
    cleanup(root);
}

#[test]
fn malformed_future_wrong_scope_and_oversized_state_fail_closed() {
    let root = workspace("invalid");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let path = store.state_path().to_owned();

    write_private(&path, "{broken");
    assert!(matches!(store.load(), Err(StateError::Malformed { .. })));

    write_private(&path, r#"{"schema_version":4}"#);
    assert!(matches!(
        store.load(),
        Err(StateError::FutureSchema { version: 4 })
    ));

    let other = StateV3::empty(scope("other"));
    write_private(&path, serde_json::to_vec(&other).unwrap());
    assert!(matches!(
        store.load(),
        Err(StateError::ScopeMismatch { .. })
    ));

    write_private(&path, vec![b'x'; limits().state_bytes_max() + 1]);
    assert!(matches!(store.load(), Err(StateError::SizeLimit { .. })));
    cleanup(root);
}

#[test]
fn one_stale_regular_temporary_is_cleaned_under_the_lock() {
    let root = workspace("stale-temp");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    write_private(store.state_directory_path().join("state-v3.tmp"), "partial");
    let mut session = store.lock().unwrap();
    session.persist_loaded().unwrap();
    assert!(!store.state_directory_path().join("state-v3.tmp").exists());
    assert_eq!(store.load().unwrap(), *session.state());
    cleanup(root);
}

#[test]
fn wrong_scope_store_cannot_replace_current_state() {
    let root = workspace("atomic");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let mut session = store.lock().unwrap();
    session.persist_loaded().unwrap();
    let before = fs::read(store.state_path()).unwrap();
    drop(session);

    let wrong_scope = StateStore::open(&root, scope("other"), limits()).unwrap();
    assert!(matches!(
        wrong_scope.lock(),
        Err(almighty_push::state::SessionError::State(
            StateError::ScopeMismatch { .. }
        ))
    ));
    assert_eq!(fs::read(store.state_path()).unwrap(), before);
    cleanup(root);
}

#[test]
fn locked_session_rejects_a_changed_expected_generation() {
    let root = workspace("stale-generation");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let mut session = store.lock().unwrap();
    session.persist_loaded().unwrap();
    let old_bytes = fs::read(store.state_path()).unwrap();
    let plan = Plan::new(
        scope("owner"),
        vec![Effect::Reobserve(ReobserveBarrier::Github)].into_boxed_slice(),
        limits(),
    )
    .unwrap();
    session.start_plan(plan).unwrap();
    fs::write(store.state_path(), old_bytes).unwrap();

    assert!(matches!(
        session.persist_loaded(),
        Err(StateError::StaleState)
    ));
    cleanup(root);
}

#[test]
fn replaced_lock_entry_cannot_authorize_mutation() {
    let root = workspace("lock-replacement");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let mut session = store.lock().unwrap();
    let lock_path = store.state_directory_path().join("lock");
    fs::remove_file(&lock_path).unwrap();
    fs::write(&lock_path, "replacement").unwrap();
    assert!(matches!(
        session.persist_loaded(),
        Err(StateError::LockMismatch)
    ));
    cleanup(root);
}

#[cfg(unix)]
#[test]
fn post_acquisition_directory_and_lock_metadata_drift_fail_closed() {
    use std::os::unix::fs::PermissionsExt;

    let root = workspace("metadata-drift");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let mut session = store.lock().unwrap();
    let lock_path = store.state_directory_path().join("lock");
    fs::hard_link(&lock_path, store.state_directory_path().join("lock-alias")).unwrap();
    assert!(matches!(
        session.persist_loaded(),
        Err(StateError::LockMismatch)
    ));
    fs::remove_file(store.state_directory_path().join("lock-alias")).unwrap();
    fs::set_permissions(
        store.state_directory_path(),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(matches!(
        session.persist_loaded(),
        Err(StateError::NamespaceDrift { .. })
    ));
    fs::set_permissions(
        store.state_directory_path(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    cleanup(root);
}

#[test]
fn retained_state_directory_prevents_parent_path_replacement() {
    let root = workspace("parent-replacement");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let original = store.state_directory_path().to_owned();
    let moved = root.join("moved-state");
    let mut session = store.lock().unwrap();
    fs::rename(&original, &moved).unwrap();
    fs::create_dir(&original).unwrap();

    let error = session.persist_loaded();
    assert!(matches!(error, Err(StateError::NamespaceDrift { .. })));
    assert!(!moved.join("state-v3.json").exists());
    assert!(!original.join("state-v3.json").exists());
    cleanup(root);
}

#[cfg(unix)]
#[test]
fn symlinked_or_non_file_state_is_rejected() {
    use std::os::unix::fs::symlink;

    let root = workspace("symlink");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let target = root.join("target");
    fs::write(&target, "{}").unwrap();
    symlink(&target, store.state_path()).unwrap();
    assert!(matches!(store.load(), Err(StateError::UnsafePath { .. })));
    fs::remove_file(store.state_path()).unwrap();
    fs::create_dir(store.state_path()).unwrap();
    assert!(matches!(store.load(), Err(StateError::UnsafePath { .. })));
    cleanup(root);
}

#[test]
fn truncated_or_unknown_v2_schema_is_rejected() {
    let root = workspace("legacy-schema");
    fs::write(root.join(".almighty"), r#"{"version":2,"prs":{}}"#).unwrap();
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    assert!(matches!(store.load(), Err(StateError::Malformed { .. })));

    fs::write(
        root.join(".almighty"),
        r#"{"version":2,"prs":{},"merged_prs":[],"closed_prs":[],"last_operation_id":null,"unknown":true}"#,
    )
    .unwrap();
    assert!(matches!(store.load(), Err(StateError::Malformed { .. })));
    cleanup(root);
}

#[test]
fn v2_entries_remain_non_authoritative_legacy_candidates() {
    let root = workspace("migration");
    fs::write(
        root.join(".almighty"),
        format!(
            r#"{{"version":2,"prs":{{"short":{{"pr_number":7,"pr_url":"https://github.com/o/r/pull/7","branch_name":"push-short","commit_id":"abc","change_id":"short"}},"{CHANGE}":{{"pr_number":8,"pr_url":"","branch_name":"almighty-push/{CHANGE}","commit_id":"1111111111111111111111111111111111111111","change_id":"{CHANGE}"}}}},"merged_prs":[],"closed_prs":[],"last_operation_id":null}}"#
        ),
    )
    .unwrap();
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let state = store.load().unwrap();
    assert!(state.verified().is_empty());
    assert_eq!(state.legacy_candidates().len(), 2);
    assert_eq!(state.checkpoint(), &CheckpointState::Idle);
    assert!(root.join(".almighty").exists());

    let mut session = store.lock().unwrap();
    session.persist_loaded().unwrap();
    assert!(root.join(".almighty").exists());
    assert!(matches!(
        session.finish_legacy_deletion(),
        Err(StateError::UnverifiedLegacy { count: 2 })
    ));
    assert!(root.join(".almighty").exists());
    cleanup(root);
}

#[cfg(unix)]
#[test]
fn symlinked_metadata_or_state_directory_is_rejected_at_open() {
    use std::os::unix::fs::symlink;

    let root = unique_directory("metadata-link");
    let target = root.join("metadata");
    fs::create_dir(&target).unwrap();
    symlink(&target, root.join(".jj")).unwrap();
    assert!(matches!(
        StateStore::open(&root, scope("owner"), limits()),
        Err(StateError::UnsafePath { .. })
    ));
    cleanup(root);

    let root = workspace("state-link");
    let target = root.join("state-target");
    fs::create_dir(&target).unwrap();
    symlink(&target, root.join(".jj/almighty-push")).unwrap();
    assert!(matches!(
        StateStore::open(&root, scope("owner"), limits()),
        Err(StateError::UnsafePath { .. })
    ));
    cleanup(root);
}

#[test]
fn empty_legacy_migration_is_vacuously_terminal_and_deletable() {
    let root = workspace("empty-migration");
    fs::write(
        root.join(".almighty"),
        r#"{"version":2,"prs":{},"merged_prs":[],"closed_prs":[],"last_operation_id":null}"#,
    )
    .unwrap();
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let mut session = store.lock().unwrap();
    session.persist_loaded().unwrap();
    session.finish_legacy_deletion().unwrap();
    assert!(!root.join(".almighty").exists());
    assert!(session.state().legacy_candidates().is_empty());
    cleanup(root);
}

#[test]
fn replacement_legacy_evidence_never_matches_persisted_migration() {
    let root = workspace("migration-replacement");
    fs::write(
        root.join(".almighty"),
        r#"{"version":2,"prs":{},"merged_prs":[],"closed_prs":[],"last_operation_id":null}"#,
    )
    .unwrap();
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let mut session = store.lock().unwrap();
    session.persist_loaded().unwrap();
    fs::write(
        root.join(".almighty"),
        r#"{"version":2,"prs":{},"merged_prs":["changed"],"closed_prs":[],"last_operation_id":null}"#,
    )
    .unwrap();
    assert!(matches!(
        store.load(),
        Err(StateError::LegacyDigestMismatch)
    ));
    cleanup(root);
}

#[cfg(unix)]
#[test]
fn fifo_state_never_blocks_and_is_never_replaced() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::FileTypeExt;

    let root = workspace("fifo");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let path = CString::new(store.state_path().as_os_str().as_bytes()).unwrap();
    // SAFETY: path is a valid NUL-terminated temporary fixture path.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    assert!(matches!(store.load(), Err(StateError::UnsafePath { .. })));
    assert!(matches!(
        store.lock(),
        Err(almighty_push::state::SessionError::State(
            StateError::UnsafePath { .. }
        ))
    ));
    assert!(fs::symlink_metadata(store.state_path())
        .unwrap()
        .file_type()
        .is_fifo());
    cleanup(root);
}

#[test]
fn process_interruption_leaves_a_complete_old_or_new_state() {
    let root = workspace("interruption");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    let mut session = store.lock().unwrap();
    session.persist_loaded().unwrap();
    drop(session);

    let ready = store.state_directory_path().join("persist-ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("state_persist_helper")
        .arg("--nocapture")
        .env(PERSIST_HELPER_ENV, &root)
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
    std::thread::sleep(Duration::from_millis(5));
    let _ = child.kill();
    child.wait().unwrap();

    let observed = store.load().unwrap();
    assert!(observed.verified().len() <= 1);
    cleanup(root);
}

#[test]
fn state_persist_helper() {
    let Some(root) = std::env::var_os(PERSIST_HELPER_ENV) else {
        return;
    };
    let store = StateStore::open(Path::new(&root), scope("owner"), limits()).unwrap();
    let mut session = store.lock().unwrap();
    fs::write(store.state_directory_path().join("persist-ready"), "ready").unwrap();
    for _ in 0..1000 {
        session.persist_loaded().unwrap();
    }
}

#[test]
fn state_location_is_derived_only_from_the_canonical_workspace() {
    let root = workspace("location");
    let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
    assert_eq!(
        store.state_path(),
        root.join(".jj/almighty-push/state-v3.json")
    );
    cleanup(root);
}

fn write_private(path: impl AsRef<std::path::Path>, bytes: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path.as_ref(), bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn workspace(label: &str) -> PathBuf {
    let root = unique_directory(label);
    fs::create_dir(root.join(".jj")).unwrap();
    root
}

fn cleanup(root: PathBuf) {
    fs::remove_dir_all(root).unwrap();
}

fn unique_directory(label: &str) -> PathBuf {
    for sequence in 0..1000 {
        let path = std::env::temp_dir().join(format!(
            "almighty-push-state-{label}-{}-{sequence}",
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
