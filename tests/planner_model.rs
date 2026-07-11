use almighty_push::domain::{
    ChangeId, CommitId, HeadRef, LimitValues, Limits, RemoteName, RepositoryId, Revision, Scope,
    SelectedChain,
};
use almighty_push::plan::{derive_plan, ExecutionMode, PlanningGithub, RemoteRefState};
use almighty_push::state::StateV3;
use std::collections::BTreeMap;

fn limits(change_count_max: u64) -> Limits {
    Limits::new(LimitValues {
        change_count_max,
        ..LimitValues::default()
    })
    .unwrap()
}

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

fn fixture(length: usize, limits: Limits) -> (SelectedChain, BTreeMap<HeadRef, RemoteRefState>) {
    let revisions = (0..length)
        .map(|index| {
            Revision::new(
                change(index),
                commit(index + 1),
                format!("change {index}"),
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
    let chain = SelectedChain::new(scope(), revisions, limits).unwrap();
    let remote = chain
        .revisions()
        .iter()
        .map(|revision| {
            (
                HeadRef::owned(revision.change_id()).unwrap(),
                RemoteRefState::At(revision.commit_id().clone()),
            )
        })
        .collect();
    (chain, remote)
}

#[test]
fn desired_bases_are_the_exact_base_to_tip_chain_adjacency_for_every_small_model() {
    for length in 0..=4 {
        let configured_limits = limits(4);
        let (chain, remote) = fixture(length, configured_limits);
        let state = StateV3::empty(scope());
        let derived = derive_plan(
            &chain,
            &remote,
            PlanningGithub::NotObserved,
            &state,
            ExecutionMode::NoPr,
            configured_limits,
        )
        .unwrap();
        assert_eq!(derived.desired_prs().len(), length);
        for (index, desired) in derived.desired_prs().iter().enumerate() {
            assert_eq!(desired.change_id(), chain.revisions()[index].change_id());
            let expected_base = if index == 0 {
                scope().base().clone()
            } else {
                HeadRef::owned(chain.revisions()[index - 1].change_id()).unwrap()
            };
            assert_eq!(desired.base(), &expected_base);
        }
    }
}

#[test]
fn exact_configured_chain_maximum_is_plannable_without_effect_growth() {
    let configured_limits = limits(64);
    let (chain, remote) = fixture(64, configured_limits);
    let state = StateV3::empty(scope());
    let derived = derive_plan(
        &chain,
        &remote,
        PlanningGithub::NotObserved,
        &state,
        ExecutionMode::NoPr,
        configured_limits,
    )
    .unwrap();
    assert_eq!(derived.desired_prs().len(), 64);
    assert_eq!(derived.action_count(), 0);
}
