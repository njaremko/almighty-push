use almighty_push::domain::{
    ChangeId, CommitId, HeadRef, Limits, RemoteName, RepositoryId, Revision, Scope, SelectedChain,
};
use almighty_push::plan::{
    derive_plan, DryRunMode, Effect, ExecutionMode, JjEffect, PlanningGithub, PlanningOutcome,
    RemoteRefState, ReobserveBarrier,
};
use almighty_push::state::StateV3;
use std::collections::BTreeMap;

fn scope() -> Scope {
    Scope::new(
        RepositoryId::parse("github.com/source/project").unwrap(),
        RepositoryId::parse("github.com/target/project").unwrap(),
        RemoteName::parse("origin").unwrap(),
        HeadRef::parse("main").unwrap(),
        "@".to_owned(),
    )
    .unwrap()
}

fn change(index: usize) -> ChangeId {
    assert!(index < 256);
    let mut bytes = [b'k'; 32];
    bytes[30] += (index / 16) as u8;
    bytes[31] += (index % 16) as u8;
    ChangeId::parse(std::str::from_utf8(&bytes).unwrap()).unwrap()
}

fn commit(index: usize) -> CommitId {
    CommitId::parse(format!("{index:040x}")).unwrap()
}

fn chain(length: usize) -> SelectedChain {
    let revisions = (0..length)
        .map(|index| {
            Revision::new(
                change(index),
                commit(index + 1),
                if index == 0 {
                    "root".to_owned()
                } else {
                    format!("change {index}")
                },
                if index == 0 {
                    Box::new([])
                } else {
                    vec![change(index - 1)].into_boxed_slice()
                },
                false,
            )
            .unwrap()
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    SelectedChain::new(scope(), revisions, Limits::default()).unwrap()
}

#[test]
fn no_pr_requires_no_github_snapshot_and_emits_only_head_publication() {
    let chain = chain(2);
    let state = StateV3::empty(scope());
    let remote = chain
        .revisions()
        .iter()
        .map(|revision| {
            (
                HeadRef::owned(revision.change_id()).unwrap(),
                RemoteRefState::Absent,
            )
        })
        .collect::<BTreeMap<_, _>>();

    let derived = derive_plan(
        &chain,
        &remote,
        PlanningGithub::NotObserved,
        &state,
        ExecutionMode::NoPr,
        Limits::default(),
    )
    .unwrap();
    let PlanningOutcome::Executable(plan) = derived.outcome() else {
        panic!("no-pr remains executable for its jj-only effects")
    };
    assert_eq!(plan.effects().len(), 3);
    assert!(plan.effects()[..2]
        .iter()
        .all(|effect| matches!(effect, Effect::Jj(JjEffect::PushHead { .. }))));
    assert_eq!(
        plan.effects().last(),
        Some(&Effect::Reobserve(ReobserveBarrier::RemoteRefs))
    );
    assert!(!plan
        .effects()
        .iter()
        .any(|effect| matches!(effect, Effect::Github(_) | Effect::Ownership(_))));
}

#[test]
fn dry_run_has_no_executor_consumable_effects_but_keeps_canonical_identity() {
    let chain = chain(1);
    let state = StateV3::empty(scope());
    let head = HeadRef::owned(chain.revisions()[0].change_id()).unwrap();
    let remote = BTreeMap::from([(head, RemoteRefState::Absent)]);

    let executable = derive_plan(
        &chain,
        &remote,
        PlanningGithub::NotObserved,
        &state,
        ExecutionMode::NoPr,
        Limits::default(),
    )
    .unwrap();
    let preview = derive_plan(
        &chain,
        &remote,
        PlanningGithub::NotObserved,
        &state,
        ExecutionMode::DryRun(DryRunMode::NoPr),
        Limits::default(),
    )
    .unwrap();

    let PlanningOutcome::Executable(executable) = executable.outcome() else {
        unreachable!()
    };
    let PlanningOutcome::DryRun(preview) = preview.outcome() else {
        panic!("dry-run must not return an executable plan")
    };
    assert_eq!(preview.id(), executable.id());
    assert_eq!(preview.action_count(), executable.effects().len());
    assert_eq!(preview.actions().len(), executable.effects().len());
}
