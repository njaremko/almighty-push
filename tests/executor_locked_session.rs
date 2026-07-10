use almighty_push::domain::{HeadRef, Limits, RemoteName, RepositoryId, Scope};
use almighty_push::executor::{Executor, NoExternalEffects, StageOutcome};
use almighty_push::plan::{Effect, Plan, ReobserveBarrier};
use almighty_push::state::StateStore;
use std::fs;
use std::path::PathBuf;

#[test]
fn caller_owned_locked_session_executes_without_releasing_invocation_authority() {
    let root = unique_directory();
    fs::create_dir(root.join(".jj")).unwrap();
    let repository = RepositoryId::parse("github.com/source/project").unwrap();
    let scope = Scope::new(
        repository.clone(),
        repository,
        RemoteName::parse("origin").unwrap(),
        HeadRef::parse("main").unwrap(),
        "@".to_owned(),
    )
    .unwrap();
    let store = StateStore::open(&root, scope.clone(), Limits::default()).unwrap();
    let mut session = store.lock().unwrap();
    let mut driver = NoExternalEffects::for_store(&store);
    let plan = Plan::new(
        scope,
        vec![Effect::Reobserve(ReobserveBarrier::All)].into_boxed_slice(),
        Limits::default(),
    )
    .unwrap();

    let completion = Executor::execute_stage_in_session(&mut session, plan, &mut driver).unwrap();

    assert_eq!(completion, StageOutcome::Reobserve(ReobserveBarrier::All));
    session.persist_loaded().unwrap();
    drop(session);
    fs::remove_dir_all(root).unwrap();
}

fn unique_directory() -> PathBuf {
    for sequence in 0..1_000 {
        let path = std::env::temp_dir().join(format!(
            "almighty-push-locked-executor-{}-{sequence}",
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
