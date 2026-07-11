use almighty_push::domain::{
    ChangeId, HeadRef, LimitValues, Limits, PrLifecycle, PrNumber, RemoteName, RepositoryId, Scope,
};
use almighty_push::executor::{Executor, ExecutorError, NoExternalEffects, StageOutcome};
use almighty_push::plan::{Effect, OwnershipEffect, Plan, PrOwnership, ReobserveBarrier};
use almighty_push::state::{CheckpointState, StateStore};
use std::fs;
use std::io;
use std::path::PathBuf;

fn limits() -> Limits {
    Limits::new(LimitValues::default()).unwrap()
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

fn change(value: char) -> ChangeId {
    ChangeId::parse(std::iter::repeat_n(value, 32).collect::<String>()).unwrap()
}

fn install_plan(change_id: ChangeId, number: u64) -> Plan {
    Plan::new(
        scope(),
        vec![Effect::Ownership(OwnershipEffect::InstallObserved {
            ownership: PrOwnership::generated(change_id).unwrap(),
            number: PrNumber::new(number).unwrap(),
            expected_absent: true,
            lifecycle: PrLifecycle::Open,
        })]
        .into_boxed_slice(),
        limits(),
    )
    .unwrap()
}

#[test]
fn complete_plan_is_durable_before_effect_zero_and_retains_its_completion_receipt() {
    let (root, store) = make_store("checkpoint-first");
    let plan = install_plan(change('k'), 7);
    let mut session = store.lock().unwrap();
    session.start_plan(plan.clone()).unwrap();
    drop(session);
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Executing(checkpoint)
            if checkpoint.plan() == &plan
                && checkpoint.next_effect_index() == 0
                && checkpoint.results() == [None]
    ));

    let mut driver = NoExternalEffects::for_store(&store);
    assert_eq!(
        Executor::resume_stage(&store, &mut driver).unwrap(),
        StageOutcome::Complete
    );
    let state = store.load().unwrap();
    assert!(matches!(
        state.checkpoint(),
        CheckpointState::Completed(checkpoint) if checkpoint.plan() == &plan
    ));
    assert_eq!(
        state.verified().get(&change('k')).unwrap().number(),
        PrNumber::new(7).unwrap()
    );
    cleanup(root);
}

#[test]
fn already_satisfied_ownership_postcondition_advances_without_reapplying() {
    let (root, store) = make_store("already-satisfied");
    let plan = install_plan(change('k'), 7);
    let mut driver = NoExternalEffects::for_store(&store);
    Executor::execute_stage(&store, plan.clone(), &mut driver).unwrap();

    assert_eq!(
        Executor::execute_stage(&store, plan.clone(), &mut driver).unwrap(),
        StageOutcome::Complete
    );
    let state = store.load().unwrap();
    assert_eq!(state.verified().len(), 1);
    assert!(matches!(
        state.checkpoint(),
        CheckpointState::Completed(checkpoint) if checkpoint.plan() == &plan
    ));
    cleanup(root);
}

#[test]
fn conflicting_ownership_stops_on_drift_without_erasing_the_existing_row() {
    let (root, store) = make_store("drift");
    let mut driver = NoExternalEffects::for_store(&store);
    let installed = install_plan(change('k'), 7);
    let installed_receipt =
        Executor::execute_stage(&store, installed.clone(), &mut driver).unwrap();

    let conflicting = install_plan(change('k'), 8);
    assert!(matches!(
        Executor::execute_next_stage(
            &store,
            installed_receipt.acknowledgement(),
            conflicting,
            &mut driver,
        ),
        Err(ExecutorError::Drift { effect_index: 0 })
    ));
    let state = store.load().unwrap();
    assert_eq!(
        state.verified().get(&change('k')).unwrap().number(),
        PrNumber::new(7).unwrap()
    );
    assert!(matches!(state.checkpoint(), CheckpointState::Executing(_)));
    cleanup(root);
}

#[test]
fn terminal_barrier_retains_the_checkpoint_receipt_and_returns_reobserve() {
    let (root, store) = make_store("barrier");
    let plan = Plan::new(
        scope(),
        vec![Effect::Reobserve(ReobserveBarrier::Github)].into_boxed_slice(),
        limits(),
    )
    .unwrap();
    let mut driver = NoExternalEffects::for_store(&store);
    assert_eq!(
        Executor::execute_stage(&store, plan, &mut driver).unwrap(),
        StageOutcome::Reobserve(ReobserveBarrier::Github)
    );
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Completed(checkpoint)
            if checkpoint.plan().effects()
                == [Effect::Reobserve(ReobserveBarrier::Github)]
    ));
    cleanup(root);
}

fn make_store(label: &str) -> (PathBuf, StateStore) {
    for sequence in 0..1_000 {
        let root = std::env::temp_dir().join(format!(
            "almighty-push-executor-{label}-{}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&root) {
            Ok(()) => {
                fs::create_dir(root.join(".jj")).unwrap();
                let root = root.canonicalize().unwrap();
                let store = StateStore::open(&root, scope(), limits()).unwrap();
                return (root, store);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("fixture creation failed: {error}"),
        }
    }
    panic!("fixture attempts exhausted")
}

fn cleanup(root: PathBuf) {
    fs::remove_dir_all(root).unwrap();
}
