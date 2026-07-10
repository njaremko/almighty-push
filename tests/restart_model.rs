use almighty_push::domain::{HeadRef, LimitValues, Limits, RemoteName, RepositoryId, Scope};
use almighty_push::executor::{Executor, ExecutorError, NoExternalEffects, StageOutcome};
use almighty_push::plan::{Effect, Plan, ReobserveBarrier};
use almighty_push::state::{CheckpointState, SessionError, StateError, StateStore};
use std::fs;
use std::io;
use std::path::PathBuf;

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

fn barrier_plan(reason: ReobserveBarrier) -> Plan {
    Plan::new(
        scope("owner"),
        vec![Effect::Reobserve(reason)].into_boxed_slice(),
        limits(),
    )
    .unwrap()
}

#[test]
fn resume_uses_the_exact_durable_plan_without_a_newly_derived_plan() {
    let (root, store) = make_store("resume-durable");
    let plan = barrier_plan(ReobserveBarrier::All);
    let mut session = store.lock().unwrap();
    session.start_plan(plan.clone()).unwrap();
    drop(session);

    let mut driver = NoExternalEffects::for_store(&store);
    assert_eq!(
        Executor::resume_stage(&store, &mut driver).unwrap(),
        StageOutcome::Reobserve(ReobserveBarrier::All)
    );
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Completed(checkpoint) if checkpoint.plan() == &plan
    ));
    cleanup(root);
}

#[test]
fn a_new_plan_cannot_replace_or_alias_an_in_progress_checkpoint() {
    let (root, store) = make_store("plan-mismatch");
    let durable = barrier_plan(ReobserveBarrier::Github);
    let replacement = barrier_plan(ReobserveBarrier::All);
    let mut session = store.lock().unwrap();
    session.start_plan(durable.clone()).unwrap();
    drop(session);

    let mut driver = NoExternalEffects::for_store(&store);
    assert!(matches!(
        Executor::execute_stage(&store, replacement, &mut driver),
        Err(ExecutorError::PlanMismatch { expected, observed })
            if expected == durable.id() && observed != expected
    ));
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Executing(checkpoint)
            if checkpoint.plan() == &durable && checkpoint.next_effect_index() == 0
    ));
    cleanup(root);
}

#[test]
fn completion_requires_the_opaque_exact_current_receipt() {
    let (root, store) = make_store("completion-acknowledgement");
    let completed = barrier_plan(ReobserveBarrier::Github);
    let next = barrier_plan(ReobserveBarrier::All);
    let mut driver = NoExternalEffects::for_store(&store);
    let completed_receipt =
        Executor::execute_stage(&store, completed.clone(), &mut driver).unwrap();
    assert_eq!(
        completed_receipt,
        StageOutcome::Reobserve(ReobserveBarrier::Github)
    );

    let pending_receipt = match Executor::execute_stage(&store, next.clone(), &mut driver) {
        Err(ExecutorError::CompletionPending(completion))
            if completion.plan_id() == completed.id()
                && completion.outcome() == StageOutcome::Reobserve(ReobserveBarrier::Github) =>
        {
            completion.acknowledgement()
        }
        other => panic!("expected exact pending completion, got {other:?}"),
    };
    assert_eq!(pending_receipt, completed_receipt.acknowledgement());
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Completed(checkpoint) if checkpoint.plan() == &completed
    ));

    assert_eq!(
        Executor::execute_next_stage(
            &store,
            completed_receipt.acknowledgement(),
            next.clone(),
            &mut driver,
        )
        .unwrap(),
        StageOutcome::Reobserve(ReobserveBarrier::All)
    );
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Completed(checkpoint) if checkpoint.plan() == &next
    ));
    cleanup(root);
}

#[test]
fn same_plan_without_ack_recovers_receipt_and_exact_ack_starts_a_fresh_occurrence() {
    let (root, store) = make_store("same-plan-occurrence");
    let plan = barrier_plan(ReobserveBarrier::Github);
    let mut driver = NoExternalEffects::for_store(&store);
    let first = Executor::execute_stage(&store, plan.clone(), &mut driver).unwrap();
    let first_occurrence = match store.load().unwrap().checkpoint() {
        CheckpointState::Completed(checkpoint) => checkpoint.occurrence(),
        _ => panic!("first occurrence must be complete"),
    };

    let recovered = Executor::execute_stage(&store, plan.clone(), &mut driver).unwrap();
    assert_eq!(recovered.acknowledgement(), first.acknowledgement());
    assert_eq!(
        match store.load().unwrap().checkpoint() {
            CheckpointState::Completed(checkpoint) => checkpoint.occurrence(),
            _ => panic!("receipt recovery must remain complete"),
        },
        first_occurrence
    );

    let second =
        Executor::execute_next_stage(&store, first.acknowledgement(), plan.clone(), &mut driver)
            .unwrap();
    assert_eq!(second, StageOutcome::Reobserve(ReobserveBarrier::Github));
    assert_ne!(second.acknowledgement(), first.acknowledgement());
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Completed(checkpoint)
            if checkpoint.plan() == &plan && checkpoint.occurrence() > first_occurrence
    ));
    cleanup(root);
}

#[test]
fn stale_p_q_p_receipt_cannot_acknowledge_the_later_equal_plan_occurrence() {
    let (root, store) = make_store("stale-p-q-p");
    let plan_p = barrier_plan(ReobserveBarrier::Github);
    let plan_q = barrier_plan(ReobserveBarrier::All);
    let mut driver = NoExternalEffects::for_store(&store);

    let first_p = Executor::execute_stage(&store, plan_p.clone(), &mut driver).unwrap();
    let q = Executor::execute_next_stage(
        &store,
        first_p.acknowledgement(),
        plan_q.clone(),
        &mut driver,
    )
    .unwrap();
    let second_p =
        Executor::execute_next_stage(&store, q.acknowledgement(), plan_p.clone(), &mut driver)
            .unwrap();
    assert_ne!(first_p.acknowledgement(), second_p.acknowledgement());

    assert!(matches!(
        Executor::execute_next_stage(&store, first_p.acknowledgement(), plan_q, &mut driver,),
        Err(ExecutorError::CompletionAcknowledgementMismatch)
    ));
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Completed(checkpoint) if checkpoint.plan() == &plan_p
    ));
    cleanup(root);
}

#[test]
fn receipt_from_another_store_session_cannot_acknowledge_equal_durable_bytes() {
    let (first_root, first_store) = make_store("receipt-first-store");
    let (second_root, second_store) = make_store("receipt-second-store");
    let plan = barrier_plan(ReobserveBarrier::Github);
    let next = barrier_plan(ReobserveBarrier::All);
    let mut first_driver = NoExternalEffects::for_store(&first_store);
    let mut second_driver = NoExternalEffects::for_store(&second_store);
    let first = Executor::execute_stage(&first_store, plan.clone(), &mut first_driver).unwrap();
    let second = Executor::execute_stage(&second_store, plan.clone(), &mut second_driver).unwrap();
    assert_ne!(first.acknowledgement(), second.acknowledgement());

    assert!(matches!(
        Executor::execute_next_stage(
            &second_store,
            first.acknowledgement(),
            next,
            &mut second_driver,
        ),
        Err(ExecutorError::CompletionAcknowledgementMismatch)
    ));
    assert!(matches!(
        second_store.load().unwrap().checkpoint(),
        CheckpointState::Completed(checkpoint) if checkpoint.plan() == &plan
    ));
    cleanup(first_root);
    cleanup(second_root);
}

#[test]
fn resume_without_a_checkpoint_fails_without_publishing_state() {
    let (root, store) = make_store("resume-idle");
    let mut driver = NoExternalEffects::for_store(&store);
    assert!(matches!(
        Executor::resume_stage(&store, &mut driver),
        Err(ExecutorError::NoCheckpoint)
    ));
    assert!(!store.state_path().exists());
    cleanup(root);
}

#[test]
fn every_terminal_barrier_returns_its_exact_reobserve_reason() {
    for (index, reason) in [
        ReobserveBarrier::LocalHistory,
        ReobserveBarrier::RemoteRefs,
        ReobserveBarrier::Github,
        ReobserveBarrier::All,
    ]
    .into_iter()
    .enumerate()
    {
        let (root, store) = make_store(&format!("barrier-{index}"));
        let plan = barrier_plan(reason);
        let mut driver = NoExternalEffects::for_store(&store);
        assert_eq!(
            Executor::execute_stage(&store, plan.clone(), &mut driver).unwrap(),
            StageOutcome::Reobserve(reason)
        );
        assert_eq!(
            Executor::resume_stage(&store, &mut driver).unwrap(),
            StageOutcome::Reobserve(reason)
        );
        assert!(matches!(
            store.load().unwrap().checkpoint(),
            CheckpointState::Completed(checkpoint) if checkpoint.plan() == &plan
        ));
        cleanup(root);
    }
}

#[test]
fn nonterminal_legacy_migration_forbids_execution_checkpoint_publication() {
    let (root, store) = make_store("migration-blocks-execution");
    fs::write(
        root.join(".almighty"),
        r#"{"version":2,"prs":{"legacy":{"pr_number":7,"pr_url":"https://github.com/owner/project/pull/7","branch_name":"legacy/topic","commit_id":"1111111111111111111111111111111111111111","change_id":"kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk"}},"merged_prs":[],"closed_prs":[],"last_operation_id":null}"#,
    )
    .unwrap();
    let mut driver = NoExternalEffects::for_store(&store);
    assert!(matches!(
        Executor::execute_stage(&store, barrier_plan(ReobserveBarrier::All), &mut driver,),
        Err(ExecutorError::State(
            almighty_push::state::StateError::InvalidCheckpoint
        ))
    ));
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Idle
    ));
    cleanup(root);
}

#[test]
fn cross_scope_state_cannot_be_resumed_or_replaced() {
    let (root, owner_store) = make_store("cross-scope");
    let mut session = owner_store.lock().unwrap();
    session
        .start_plan(barrier_plan(ReobserveBarrier::All))
        .unwrap();
    drop(session);

    let other_store = StateStore::open(&root, scope("other"), limits()).unwrap();
    let mut driver = NoExternalEffects::for_store(&owner_store);
    assert!(matches!(
        Executor::resume_stage(&other_store, &mut driver),
        Err(ExecutorError::Session(SessionError::State(
            StateError::ScopeMismatch { expected, observed }
        ))) if expected.as_ref() == &scope("other") && observed.as_ref() == &scope("owner")
    ));
    assert!(matches!(
        owner_store.load().unwrap().checkpoint(),
        CheckpointState::Executing(checkpoint)
            if checkpoint.plan() == &barrier_plan(ReobserveBarrier::All)
                && checkpoint.next_effect_index() == 0
    ));
    cleanup(root);
}

fn make_store(label: &str) -> (PathBuf, StateStore) {
    for sequence in 0..1_000 {
        let root = std::env::temp_dir().join(format!(
            "almighty-push-restart-{label}-{}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&root) {
            Ok(()) => {
                fs::create_dir(root.join(".jj")).unwrap();
                let root = root.canonicalize().unwrap();
                let store = StateStore::open(&root, scope("owner"), limits()).unwrap();
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
