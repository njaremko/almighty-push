use super::*;
use crate::body::{BodyMerge, ManagedBody, ManagedSection};
use crate::domain::{LimitValues, RemoteName, RepositoryId, Revision};
use crate::github::{GithubSnapshot, ObservedPr, ObservedPrFixture};
use crate::state::StateV3;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ModelLocation {
    Missing,
    Active,
    Historic,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ModelProvenance {
    Generated,
    SealedLegacy,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ModelMode {
    Full,
    DryFull,
    NoPr,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ModelEffect {
    Push(usize),
    Create(usize),
    InstallCreated(usize),
    InstallObserved(usize, ModelProvenance, PrLifecycle),
    Reactivate(usize),
    PersistLifecycle(usize, PrLifecycle, PrLifecycle),
    GithubLifecycle(usize, PrLifecycle, PrLifecycle),
    UpdateBase(usize),
    UpdateBody(usize),
    Historicize(usize),
    Delete(usize, ModelProvenance),
    Barrier(ReobserveBarrier),
    Rebase(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ModelRow {
    location: ModelLocation,
    lifecycle: PrLifecycle,
    provenance: ModelProvenance,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum ModelError {
    GithubRequired,
    GithubForbidden,
    OwnershipMissing,
    OwnershipConflict,
    ObservationDrift,
    HistoryBound,
    LifecycleDrift,
    MergedTip,
}

#[derive(Default)]
struct Coverage {
    lengths: BTreeSet<usize>,
    modes: BTreeSet<ModelMode>,
    locations: BTreeSet<ModelLocation>,
    lifecycles: BTreeSet<u8>,
    provenances: BTreeSet<ModelProvenance>,
    delete_policies: BTreeSet<bool>,
    effects: Vec<ModelEffect>,
    errors: BTreeSet<ModelError>,
    reorder_case_count: usize,
    membership_change_count: usize,
    scenario_count: usize,
}

fn lifecycle_code(lifecycle: PrLifecycle) -> u8 {
    match lifecycle {
        PrLifecycle::Open => 0,
        PrLifecycle::Closed => 1,
        PrLifecycle::Merged => 2,
    }
}

fn limits() -> Limits {
    Limits::new(LimitValues {
        change_count_max: 4,
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
    assert!(index < 16);
    let mut bytes = [b'k'; 32];
    bytes[31] += index as u8;
    ChangeId::parse(std::str::from_utf8(&bytes).unwrap()).unwrap()
}

fn change_index(change_id: &ChangeId) -> usize {
    (0..16)
        .find(|index| change(*index) == *change_id)
        .expect("oracle fixtures use indexed changes")
}

fn commit(index: usize) -> CommitId {
    CommitId::parse(format!("{:040x}", index + 1)).unwrap()
}

fn head(index: usize, provenance: ModelProvenance) -> HeadRef {
    match provenance {
        ModelProvenance::Generated => HeadRef::owned(&change(index)).unwrap(),
        ModelProvenance::SealedLegacy => HeadRef::parse(format!("legacy/topic-{index}")).unwrap(),
    }
}

fn chain(order: &[usize]) -> SelectedChain {
    let revisions = order
        .iter()
        .enumerate()
        .map(|(position, index)| {
            Revision::new(
                change(*index),
                commit(*index),
                format!("change {index}"),
                if position == 0 {
                    Box::new([])
                } else {
                    vec![change(order[position - 1])].into_boxed_slice()
                },
                false,
            )
            .unwrap()
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    SelectedChain::new(scope(), revisions, limits()).unwrap()
}

fn body(index: usize, stack: &[usize]) -> String {
    let section = ManagedSection::new(
        scope().source_repository().clone(),
        change(index),
        stack
            .iter()
            .map(|row| change(*row))
            .collect::<Vec<_>>()
            .into_boxed_slice(),
        limits(),
    )
    .unwrap();
    let BodyMerge::Changed(body) =
        ManagedBody::merge("user bytes", &section, limits().body_bytes_max()).unwrap()
    else {
        panic!("new fixture section must change the body")
    };
    body
}

fn observed_pr(
    index: usize,
    number: u64,
    provenance: ModelProvenance,
    lifecycle: PrLifecycle,
    base: HeadRef,
    stack: &[usize],
) -> ObservedPr {
    ObservedPr::fixture(ObservedPrFixture {
        number: PrNumber::new(number).unwrap(),
        lifecycle,
        head_repository: scope().source_repository().clone(),
        head_ref: head(index, provenance),
        base_ref: base,
        title: format!("change {index}"),
        body: body(index, stack),
        head_commit: commit(index),
    })
}

fn snapshot(prs: Vec<ObservedPr>, owned: BTreeMap<HeadRef, CommitId>) -> GithubSnapshot {
    GithubSnapshot::fixture(
        scope().source_repository().clone(),
        scope().target_repository().clone(),
        HeadRef::parse("main").unwrap(),
        scope().base().clone(),
        owned,
        prs.into_boxed_slice(),
    )
}

fn insert_state_row(state: &mut StateV3, index: usize, row: &ModelRow, number: PrNumber) {
    match (row.location, row.provenance) {
        (ModelLocation::Missing, _) => {}
        (ModelLocation::Active, ModelProvenance::Generated) => {
            state.insert_generated_fixture(change(index), number, row.lifecycle, false)
        }
        (ModelLocation::Historic, ModelProvenance::Generated) => {
            state.insert_generated_fixture(change(index), number, row.lifecycle, true)
        }
        (ModelLocation::Active, ModelProvenance::SealedLegacy) => state
            .insert_validated_legacy_fixture(
                change(index),
                number,
                head(index, row.provenance),
                row.lifecycle,
                false,
            ),
        (ModelLocation::Historic, ModelProvenance::SealedLegacy) => state
            .insert_validated_legacy_fixture(
                change(index),
                number,
                head(index, row.provenance),
                row.lifecycle,
                true,
            ),
    }
}

fn normalize(effect: &Effect) -> ModelEffect {
    match effect {
        Effect::Jj(JjEffect::PushHead { ownership, .. }) => {
            ModelEffect::Push(change_index(ownership.change_id()))
        }
        Effect::Jj(JjEffect::DeleteHead { ownership, .. }) => ModelEffect::Delete(
            change_index(ownership.change_id()),
            if ownership.is_validated_legacy() {
                ModelProvenance::SealedLegacy
            } else {
                ModelProvenance::Generated
            },
        ),
        Effect::Jj(JjEffect::Rebase { change_id, .. }) => {
            ModelEffect::Rebase(change_index(change_id))
        }
        Effect::Github(GithubEffect::CreatePullRequest { change_id, .. }) => {
            ModelEffect::Create(change_index(change_id))
        }
        Effect::Github(GithubEffect::UpdateBase { ownership, .. }) => {
            ModelEffect::UpdateBase(change_index(ownership.change_id()))
        }
        Effect::Github(GithubEffect::UpdateBody { ownership, .. }) => {
            ModelEffect::UpdateBody(change_index(ownership.change_id()))
        }
        Effect::Github(GithubEffect::SetLifecycle {
            ownership,
            expected,
            desired,
            ..
        }) => {
            ModelEffect::GithubLifecycle(change_index(ownership.change_id()), *expected, *desired)
        }
        Effect::Ownership(OwnershipEffect::InstallCreated { change_id, .. }) => {
            ModelEffect::InstallCreated(change_index(change_id))
        }
        Effect::Ownership(OwnershipEffect::InstallObserved {
            ownership,
            lifecycle,
            ..
        }) => ModelEffect::InstallObserved(
            change_index(ownership.change_id()),
            if ownership.is_validated_legacy() {
                ModelProvenance::SealedLegacy
            } else {
                ModelProvenance::Generated
            },
            *lifecycle,
        ),
        Effect::Ownership(OwnershipEffect::UpdateLifecycle {
            change_id,
            expected,
            desired,
            ..
        }) => ModelEffect::PersistLifecycle(change_index(change_id), *expected, *desired),
        Effect::Ownership(OwnershipEffect::Historicize { change_id, .. }) => {
            ModelEffect::Historicize(change_index(change_id))
        }
        Effect::Ownership(OwnershipEffect::Reactivate { change_id, .. }) => {
            ModelEffect::Reactivate(change_index(change_id))
        }
        Effect::Reobserve(barrier) => ModelEffect::Barrier(*barrier),
    }
}

fn normalized_outcome(derived: &DerivedPlan) -> Vec<ModelEffect> {
    match derived.outcome() {
        PlanningOutcome::Executable(plan) => plan.effects().iter().map(normalize).collect(),
        PlanningOutcome::DryRun(preview) => preview
            .actions()
            .iter()
            .map(|action| serde_json::from_str::<Effect>(action).unwrap())
            .map(|effect| normalize(&effect))
            .collect(),
    }
}

fn normalize_error(error: &PlanError) -> Option<ModelError> {
    match error {
        PlanError::GithubObservationRequired => Some(ModelError::GithubRequired),
        PlanError::GithubObservationForbidden => Some(ModelError::GithubForbidden),
        PlanError::OwnershipMissing { .. } => Some(ModelError::OwnershipMissing),
        PlanError::OwnershipConflict { .. } => Some(ModelError::OwnershipConflict),
        PlanError::ObservationDrift { .. } => Some(ModelError::ObservationDrift),
        PlanError::HistoryBound => Some(ModelError::HistoryBound),
        PlanError::LifecycleDrift { .. } => Some(ModelError::LifecycleDrift),
        PlanError::MergedTip { .. } => Some(ModelError::MergedTip),
        _ => None,
    }
}

fn apply_effects(initial: Option<ModelRow>, effects: &[ModelEffect]) -> Option<ModelRow> {
    let mut row = initial;
    for effect in effects {
        match effect {
            ModelEffect::InstallCreated(_) => {
                assert!(row.is_none());
                row = Some(ModelRow {
                    location: ModelLocation::Active,
                    lifecycle: PrLifecycle::Open,
                    provenance: ModelProvenance::Generated,
                });
            }
            ModelEffect::InstallObserved(_, provenance, lifecycle) => {
                assert!(row.is_none());
                row = Some(ModelRow {
                    location: ModelLocation::Active,
                    lifecycle: *lifecycle,
                    provenance: *provenance,
                });
            }
            ModelEffect::Reactivate(_) => {
                let row = row.as_mut().expect("reactivation requires a row");
                assert_eq!(row.location, ModelLocation::Historic);
                row.location = ModelLocation::Active;
            }
            ModelEffect::PersistLifecycle(_, expected, desired) => {
                let row = row.as_mut().expect("lifecycle update requires a row");
                assert_eq!(row.location, ModelLocation::Active);
                assert_eq!(row.lifecycle, *expected);
                row.lifecycle = *desired;
            }
            ModelEffect::Historicize(_) => {
                let row = row.as_mut().expect("historicize requires a row");
                assert_eq!(row.location, ModelLocation::Active);
                row.location = ModelLocation::Historic;
            }
            _ => {}
        }
    }
    row
}

fn ordered_subsets() -> Vec<Vec<usize>> {
    let mut orders = vec![Vec::new()];
    for first in 0..4 {
        orders.push(vec![first]);
        for second in 0..4 {
            if second == first {
                continue;
            }
            orders.push(vec![first, second]);
            for third in 0..4 {
                if third == first || third == second {
                    continue;
                }
                orders.push(vec![first, second, third]);
                for fourth in 0..4 {
                    if fourth == first || fourth == second || fourth == third {
                        continue;
                    }
                    orders.push(vec![first, second, third, fourth]);
                }
            }
        }
    }
    orders
}

fn adjacency_fixture(
    order: &[usize],
    mode: ModelMode,
) -> (
    SelectedChain,
    BTreeMap<HeadRef, RemoteRefState>,
    StateV3,
    Option<GithubSnapshot>,
) {
    let chain = chain(order);
    let mut state = StateV3::empty(scope());
    let mut old_order = order.to_vec();
    old_order.sort_unstable();
    let mut prs = Vec::new();
    let mut owned = BTreeMap::new();
    let mut remote = BTreeMap::new();
    for (old_position, index) in old_order.iter().enumerate() {
        let number = PrNumber::new((*index + 1) as u64).unwrap();
        state.insert_generated_fixture(change(*index), number, PrLifecycle::Open, false);
        let base = if old_position == 0 {
            scope().base().clone()
        } else {
            head(old_order[old_position - 1], ModelProvenance::Generated)
        };
        prs.push(observed_pr(
            *index,
            number.get(),
            ModelProvenance::Generated,
            PrLifecycle::Open,
            base,
            &old_order,
        ));
        let current_head = head(*index, ModelProvenance::Generated);
        owned.insert(current_head.clone(), commit(*index));
        remote.insert(current_head, RemoteRefState::At(commit(*index)));
    }
    prs.reverse();
    let snapshot = (!matches!(mode, ModelMode::NoPr)).then(|| snapshot(prs, owned));
    (chain, remote, state, snapshot)
}

fn expected_adjacency_effects(order: &[usize]) -> Vec<ModelEffect> {
    let mut old_order = order.to_vec();
    old_order.sort_unstable();
    let reordered = old_order != order;
    let old_bases = old_order
        .iter()
        .enumerate()
        .map(|(position, index)| {
            let base = if position == 0 {
                scope().base().clone()
            } else {
                head(old_order[position - 1], ModelProvenance::Generated)
            };
            (*index, base)
        })
        .collect::<BTreeMap<_, _>>();
    let mut expected = Vec::new();
    for (position, index) in order.iter().enumerate() {
        let desired = if position == 0 {
            scope().base().clone()
        } else {
            head(order[position - 1], ModelProvenance::Generated)
        };
        if old_bases[index] != desired {
            expected.push(ModelEffect::UpdateBase(*index));
        }
        if reordered {
            expected.push(ModelEffect::UpdateBody(*index));
        }
    }
    expected
}

fn run_adjacency_models(coverage: &mut Coverage) {
    let orders = ordered_subsets();
    assert_eq!(orders.len(), 65);
    for order in orders {
        coverage.lengths.insert(order.len());
        let mut sorted = order.clone();
        sorted.sort_unstable();
        if sorted != order {
            coverage.reorder_case_count += 1;
        }
        for mode in [ModelMode::Full, ModelMode::DryFull, ModelMode::NoPr] {
            coverage.modes.insert(mode);
            coverage.scenario_count += 1;
            let (chain, remote, state, snapshot) = adjacency_fixture(&order, mode);
            let (github, execution_mode) = match (mode, snapshot.as_ref()) {
                (ModelMode::Full, Some(snapshot)) => (
                    PlanningGithub::Observed(snapshot),
                    ExecutionMode::Full {
                        delete_closed_heads: false,
                    },
                ),
                (ModelMode::DryFull, Some(snapshot)) => (
                    PlanningGithub::Observed(snapshot),
                    ExecutionMode::DryRun(DryRunMode::Full {
                        delete_closed_heads: false,
                    }),
                ),
                (ModelMode::NoPr, None) => (PlanningGithub::NotObserved, ExecutionMode::NoPr),
                _ => unreachable!(),
            };
            let derived =
                derive_plan(&chain, &remote, github, &state, execution_mode, limits()).unwrap();
            let actual = normalized_outcome(&derived);
            let expected = if mode == ModelMode::NoPr {
                Vec::new()
            } else {
                expected_adjacency_effects(&order)
            };
            assert_eq!(actual, expected, "order={order:?}, mode={mode:?}");
            coverage.effects.extend(actual);

            let desired = derived
                .desired_prs()
                .iter()
                .map(|row| {
                    (
                        change_index(row.change_id()),
                        row.head().clone(),
                        row.base().clone(),
                    )
                })
                .collect::<Vec<_>>();
            let expected_desired = order
                .iter()
                .enumerate()
                .map(|(position, index)| {
                    (
                        *index,
                        head(*index, ModelProvenance::Generated),
                        if position == 0 {
                            scope().base().clone()
                        } else {
                            head(order[position - 1], ModelProvenance::Generated)
                        },
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(desired, expected_desired);
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct SingleScenario {
    selected: bool,
    location: ModelLocation,
    persisted: PrLifecycle,
    observed: Option<PrLifecycle>,
    provenance: ModelProvenance,
    delete: bool,
    mode: ModelMode,
}

fn cleanup_reference(
    effects: &mut Vec<ModelEffect>,
    mut lifecycle: PrLifecycle,
    observed: PrLifecycle,
    provenance: ModelProvenance,
    delete: bool,
) {
    if lifecycle != observed {
        effects.push(ModelEffect::PersistLifecycle(0, lifecycle, observed));
        lifecycle = observed;
    }
    if lifecycle == PrLifecycle::Open {
        effects.push(ModelEffect::GithubLifecycle(
            0,
            PrLifecycle::Open,
            PrLifecycle::Closed,
        ));
        effects.push(ModelEffect::PersistLifecycle(
            0,
            PrLifecycle::Open,
            PrLifecycle::Closed,
        ));
    }
    effects.push(ModelEffect::Historicize(0));
    if delete {
        effects.push(ModelEffect::Delete(0, provenance));
    }
}

fn reference_single(
    scenario: SingleScenario,
) -> Result<(Vec<ModelEffect>, Option<ModelRow>), ModelError> {
    let initial = (scenario.location != ModelLocation::Missing).then_some(ModelRow {
        location: scenario.location,
        lifecycle: scenario.persisted,
        provenance: scenario.provenance,
    });
    if scenario.mode == ModelMode::NoPr {
        return Ok((Vec::new(), initial));
    }
    let mut effects = Vec::new();
    if scenario.selected {
        match scenario.observed {
            Some(observed) => {
                if observed == PrLifecycle::Merged {
                    return Err(ModelError::MergedTip);
                }
                let mut persisted = scenario.persisted;
                match scenario.location {
                    ModelLocation::Active => {}
                    ModelLocation::Historic => effects.push(ModelEffect::Reactivate(0)),
                    ModelLocation::Missing => {
                        effects.push(ModelEffect::InstallObserved(
                            0,
                            ModelProvenance::Generated,
                            observed,
                        ));
                        persisted = observed;
                    }
                }
                if persisted == PrLifecycle::Merged && persisted != observed {
                    return Err(ModelError::LifecycleDrift);
                }
                if persisted != observed {
                    effects.push(ModelEffect::PersistLifecycle(0, persisted, observed));
                }
                if observed == PrLifecycle::Closed {
                    effects.push(ModelEffect::GithubLifecycle(
                        0,
                        PrLifecycle::Closed,
                        PrLifecycle::Open,
                    ));
                    effects.push(ModelEffect::PersistLifecycle(
                        0,
                        PrLifecycle::Closed,
                        PrLifecycle::Open,
                    ));
                }
            }
            None if scenario.location == ModelLocation::Missing => {
                effects.push(ModelEffect::Create(0));
                effects.push(ModelEffect::InstallCreated(0));
            }
            None => return Err(ModelError::OwnershipMissing),
        }
    } else {
        match (scenario.location, scenario.observed) {
            (ModelLocation::Active, Some(observed)) => cleanup_reference(
                &mut effects,
                scenario.persisted,
                observed,
                scenario.provenance,
                scenario.delete,
            ),
            (ModelLocation::Active, None) => return Err(ModelError::OwnershipMissing),
            (ModelLocation::Historic, Some(observed)) if observed == scenario.persisted => {
                if scenario.delete {
                    effects.push(ModelEffect::Delete(0, scenario.provenance));
                }
            }
            (ModelLocation::Historic, Some(_)) if scenario.persisted == PrLifecycle::Merged => {
                return Err(ModelError::LifecycleDrift);
            }
            (ModelLocation::Historic, Some(observed)) => {
                effects.push(ModelEffect::Reactivate(0));
                cleanup_reference(
                    &mut effects,
                    scenario.persisted,
                    observed,
                    scenario.provenance,
                    scenario.delete,
                );
            }
            (ModelLocation::Historic, None) => return Err(ModelError::OwnershipMissing),
            (ModelLocation::Missing, Some(observed)) => {
                effects.push(ModelEffect::InstallObserved(
                    0,
                    ModelProvenance::Generated,
                    observed,
                ));
                cleanup_reference(
                    &mut effects,
                    observed,
                    observed,
                    ModelProvenance::Generated,
                    scenario.delete,
                );
            }
            // A generated-looking remote name without persisted or observed PR
            // authority is not owned and must be preserved.
            (ModelLocation::Missing, None) => {}
        }
    }
    if effects
        .iter()
        .any(|effect| matches!(effect, ModelEffect::Delete(_, _)))
    {
        effects.push(ModelEffect::Barrier(ReobserveBarrier::All));
    }
    let final_state = apply_effects(initial, &effects);
    Ok((effects, final_state))
}

fn actual_single(
    scenario: SingleScenario,
) -> Result<(Vec<ModelEffect>, Option<ModelRow>), ModelError> {
    let initial = (scenario.location != ModelLocation::Missing).then_some(ModelRow {
        location: scenario.location,
        lifecycle: scenario.persisted,
        provenance: scenario.provenance,
    });
    let mut state = StateV3::empty(scope());
    if let Some(row) = &initial {
        insert_state_row(&mut state, 0, row, PrNumber::new(1).unwrap());
    }
    let chain = if scenario.selected {
        chain(&[0])
    } else {
        chain(&[])
    };
    let row_head = if scenario.location == ModelLocation::Missing {
        head(0, ModelProvenance::Generated)
    } else {
        head(0, scenario.provenance)
    };
    let owned = BTreeMap::from([(row_head.clone(), commit(0))]);
    let prs = scenario.observed.map_or_else(Vec::new, |lifecycle| {
        vec![observed_pr(
            0,
            1,
            if scenario.location == ModelLocation::Missing {
                ModelProvenance::Generated
            } else {
                scenario.provenance
            },
            lifecycle,
            scope().base().clone(),
            &[0],
        )]
    });
    let snapshot = snapshot(prs, owned);
    let remote = if scenario.selected {
        BTreeMap::from([(row_head, RemoteRefState::At(commit(0)))])
    } else {
        BTreeMap::new()
    };
    let (github, mode) = match scenario.mode {
        ModelMode::Full => (
            PlanningGithub::Observed(&snapshot),
            ExecutionMode::Full {
                delete_closed_heads: scenario.delete,
            },
        ),
        ModelMode::DryFull => (
            PlanningGithub::Observed(&snapshot),
            ExecutionMode::DryRun(DryRunMode::Full {
                delete_closed_heads: scenario.delete,
            }),
        ),
        ModelMode::NoPr => (PlanningGithub::NotObserved, ExecutionMode::NoPr),
    };
    let derived = derive_plan(&chain, &remote, github, &state, mode, limits())
        .map_err(|error| normalize_error(&error).expect("scenario expected a modeled error"))?;
    let effects = normalized_outcome(&derived);
    let final_state = apply_effects(initial, &effects);
    Ok((effects, final_state))
}

fn single_scenarios() -> Vec<SingleScenario> {
    let mut scenarios = Vec::new();
    for provenance in [ModelProvenance::Generated, ModelProvenance::SealedLegacy] {
        for delete in [false, true] {
            scenarios.extend([
                SingleScenario {
                    selected: true,
                    location: ModelLocation::Active,
                    persisted: PrLifecycle::Open,
                    observed: Some(PrLifecycle::Open),
                    provenance,
                    delete,
                    mode: ModelMode::Full,
                },
                SingleScenario {
                    selected: true,
                    location: ModelLocation::Historic,
                    persisted: PrLifecycle::Closed,
                    observed: Some(PrLifecycle::Closed),
                    provenance,
                    delete,
                    mode: ModelMode::DryFull,
                },
                SingleScenario {
                    selected: true,
                    location: ModelLocation::Historic,
                    persisted: PrLifecycle::Closed,
                    observed: Some(PrLifecycle::Open),
                    provenance,
                    delete,
                    mode: ModelMode::Full,
                },
                SingleScenario {
                    selected: false,
                    location: ModelLocation::Active,
                    persisted: PrLifecycle::Open,
                    observed: Some(PrLifecycle::Open),
                    provenance,
                    delete,
                    mode: ModelMode::Full,
                },
                SingleScenario {
                    selected: false,
                    location: ModelLocation::Active,
                    persisted: PrLifecycle::Closed,
                    observed: Some(PrLifecycle::Closed),
                    provenance,
                    delete,
                    mode: ModelMode::DryFull,
                },
                SingleScenario {
                    selected: false,
                    location: ModelLocation::Active,
                    persisted: PrLifecycle::Open,
                    observed: Some(PrLifecycle::Merged),
                    provenance,
                    delete,
                    mode: ModelMode::Full,
                },
                SingleScenario {
                    selected: false,
                    location: ModelLocation::Historic,
                    persisted: PrLifecycle::Closed,
                    observed: Some(PrLifecycle::Open),
                    provenance,
                    delete,
                    mode: ModelMode::Full,
                },
            ]);
        }
    }
    scenarios.extend([
        SingleScenario {
            selected: true,
            location: ModelLocation::Missing,
            persisted: PrLifecycle::Open,
            observed: Some(PrLifecycle::Open),
            provenance: ModelProvenance::Generated,
            delete: false,
            mode: ModelMode::Full,
        },
        SingleScenario {
            selected: true,
            location: ModelLocation::Missing,
            persisted: PrLifecycle::Open,
            observed: None,
            provenance: ModelProvenance::Generated,
            delete: false,
            mode: ModelMode::DryFull,
        },
        SingleScenario {
            selected: false,
            location: ModelLocation::Missing,
            persisted: PrLifecycle::Open,
            observed: Some(PrLifecycle::Closed),
            provenance: ModelProvenance::Generated,
            delete: true,
            mode: ModelMode::Full,
        },
        SingleScenario {
            selected: false,
            location: ModelLocation::Missing,
            persisted: PrLifecycle::Open,
            observed: None,
            provenance: ModelProvenance::Generated,
            delete: true,
            mode: ModelMode::Full,
        },
        SingleScenario {
            selected: true,
            location: ModelLocation::Active,
            persisted: PrLifecycle::Open,
            observed: Some(PrLifecycle::Open),
            provenance: ModelProvenance::Generated,
            delete: false,
            mode: ModelMode::NoPr,
        },
        SingleScenario {
            selected: false,
            location: ModelLocation::Active,
            persisted: PrLifecycle::Open,
            observed: Some(PrLifecycle::Open),
            provenance: ModelProvenance::Generated,
            delete: false,
            mode: ModelMode::NoPr,
        },
        SingleScenario {
            selected: true,
            location: ModelLocation::Active,
            persisted: PrLifecycle::Open,
            observed: None,
            provenance: ModelProvenance::Generated,
            delete: false,
            mode: ModelMode::Full,
        },
        SingleScenario {
            selected: false,
            location: ModelLocation::Active,
            persisted: PrLifecycle::Open,
            observed: None,
            provenance: ModelProvenance::Generated,
            delete: false,
            mode: ModelMode::Full,
        },
        SingleScenario {
            selected: false,
            location: ModelLocation::Historic,
            persisted: PrLifecycle::Merged,
            observed: Some(PrLifecycle::Closed),
            provenance: ModelProvenance::Generated,
            delete: false,
            mode: ModelMode::Full,
        },
        SingleScenario {
            selected: true,
            location: ModelLocation::Active,
            persisted: PrLifecycle::Open,
            observed: Some(PrLifecycle::Merged),
            provenance: ModelProvenance::Generated,
            delete: false,
            mode: ModelMode::Full,
        },
    ]);
    scenarios
}

fn run_single_models(coverage: &mut Coverage) {
    for scenario in single_scenarios() {
        coverage.scenario_count += 1;
        coverage.modes.insert(scenario.mode);
        coverage.locations.insert(scenario.location);
        coverage
            .lifecycles
            .insert(lifecycle_code(scenario.persisted));
        if let Some(observed) = scenario.observed {
            coverage.lifecycles.insert(lifecycle_code(observed));
        }
        coverage.provenances.insert(scenario.provenance);
        coverage.delete_policies.insert(scenario.delete);
        let membership_changed = matches!(
            (scenario.selected, scenario.location),
            (true, ModelLocation::Missing | ModelLocation::Historic)
                | (false, ModelLocation::Active)
        );
        if membership_changed {
            coverage.membership_change_count += 1;
        }
        let expected = reference_single(scenario);
        let actual = actual_single(scenario);
        match (expected, actual) {
            (Ok((expected_effects, expected_state)), Ok((actual_effects, actual_state))) => {
                assert_eq!(actual_effects, expected_effects, "scenario={scenario:?}");
                assert_eq!(actual_state, expected_state, "scenario={scenario:?}");
                coverage.effects.extend(actual_effects);
            }
            (Err(expected), Err(actual)) => {
                assert_eq!(actual, expected, "scenario={scenario:?}");
                coverage.errors.insert(actual);
            }
            (expected, actual) => {
                panic!("oracle disagreement for {scenario:?}: expected={expected:?}, actual={actual:?}")
            }
        }
    }
}

#[test]
fn independent_reference_model_covers_adjacency_lifecycle_membership_and_capabilities() {
    let mut coverage = Coverage::default();
    run_adjacency_models(&mut coverage);
    run_single_models(&mut coverage);

    assert_eq!(coverage.lengths, BTreeSet::from([0, 1, 2, 3, 4]));
    assert_eq!(
        coverage.modes,
        BTreeSet::from([ModelMode::Full, ModelMode::DryFull, ModelMode::NoPr])
    );
    assert_eq!(
        coverage.locations,
        BTreeSet::from([
            ModelLocation::Missing,
            ModelLocation::Active,
            ModelLocation::Historic,
        ])
    );
    assert_eq!(coverage.lifecycles, BTreeSet::from([0, 1, 2]));
    assert_eq!(
        coverage.provenances,
        BTreeSet::from([ModelProvenance::Generated, ModelProvenance::SealedLegacy,])
    );
    assert_eq!(coverage.delete_policies, BTreeSet::from([false, true]));
    assert!(coverage.reorder_case_count >= 40);
    assert!(coverage.membership_change_count >= 20);
    for required in [
        ModelEffect::Create(0),
        ModelEffect::InstallCreated(0),
        ModelEffect::InstallObserved(0, ModelProvenance::Generated, PrLifecycle::Open),
        ModelEffect::Reactivate(0),
        ModelEffect::GithubLifecycle(0, PrLifecycle::Open, PrLifecycle::Closed),
        ModelEffect::GithubLifecycle(0, PrLifecycle::Closed, PrLifecycle::Open),
        ModelEffect::Historicize(0),
        ModelEffect::Delete(0, ModelProvenance::Generated),
        ModelEffect::Delete(0, ModelProvenance::SealedLegacy),
        ModelEffect::UpdateBase(0),
        ModelEffect::UpdateBody(0),
        ModelEffect::Barrier(ReobserveBarrier::All),
    ] {
        assert!(coverage.effects.contains(&required), "missing {required:?}");
    }
    assert!(coverage.errors.contains(&ModelError::OwnershipMissing));
    assert!(coverage.errors.contains(&ModelError::LifecycleDrift));
    assert!(coverage.errors.contains(&ModelError::MergedTip));
    assert!(coverage.scenario_count >= 230);
}

#[test]
fn conflict_drift_and_cardinality_errors_are_part_of_the_reference_error_algebra() {
    let mut conflict_state = StateV3::empty(scope());
    conflict_state.insert_generated_fixture(
        change(0),
        PrNumber::new(1).unwrap(),
        PrLifecycle::Open,
        false,
    );
    let conflicting = observed_pr(
        1,
        1,
        ModelProvenance::Generated,
        PrLifecycle::Open,
        scope().base().clone(),
        &[1],
    );
    let conflict_snapshot = snapshot(
        vec![conflicting],
        BTreeMap::from([(head(1, ModelProvenance::Generated), commit(1))]),
    );
    let conflict = derive_plan(
        &chain(&[]),
        &BTreeMap::new(),
        PlanningGithub::Observed(&conflict_snapshot),
        &conflict_state,
        ExecutionMode::Full {
            delete_closed_heads: false,
        },
        limits(),
    )
    .unwrap_err();

    let drift_snapshot = snapshot(
        Vec::new(),
        BTreeMap::from([(
            head(0, ModelProvenance::Generated),
            CommitId::parse("9999999999999999999999999999999999999999").unwrap(),
        )]),
    );
    let drift = derive_plan(
        &chain(&[0]),
        &BTreeMap::from([(
            head(0, ModelProvenance::Generated),
            RemoteRefState::At(commit(0)),
        )]),
        PlanningGithub::Observed(&drift_snapshot),
        &StateV3::empty(scope()),
        ExecutionMode::Full {
            delete_closed_heads: false,
        },
        limits(),
    )
    .unwrap_err();

    let mut bounded_state = StateV3::empty(scope());
    let mut bounded_prs = Vec::new();
    let mut bounded_heads = BTreeMap::new();
    for index in 0..4 {
        bounded_state.insert_generated_fixture(
            change(index),
            PrNumber::new((index + 1) as u64).unwrap(),
            PrLifecycle::Closed,
            true,
        );
        bounded_prs.push(observed_pr(
            index,
            (index + 1) as u64,
            ModelProvenance::Generated,
            PrLifecycle::Closed,
            scope().base().clone(),
            &[index],
        ));
    }
    bounded_heads.insert(head(4, ModelProvenance::Generated), commit(4));
    let bounded_snapshot = snapshot(bounded_prs, bounded_heads);
    let history = derive_plan(
        &chain(&[4]),
        &BTreeMap::from([(
            head(4, ModelProvenance::Generated),
            RemoteRefState::At(commit(4)),
        )]),
        PlanningGithub::Observed(&bounded_snapshot),
        &bounded_state,
        ExecutionMode::Full {
            delete_closed_heads: false,
        },
        limits(),
    )
    .unwrap_err();

    let covered = [conflict, drift, history]
        .iter()
        .map(|error| normalize_error(error).unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        covered,
        BTreeSet::from([
            ModelError::OwnershipConflict,
            ModelError::ObservationDrift,
            ModelError::HistoryBound,
        ])
    );
}

#[test]
fn capability_mismatch_errors_are_part_of_the_reference_error_algebra() {
    let chain = chain(&[]);
    let state = StateV3::empty(scope());
    let snapshot = snapshot(Vec::new(), BTreeMap::new());
    let cases = [
        (
            derive_plan(
                &chain,
                &BTreeMap::new(),
                PlanningGithub::NotObserved,
                &state,
                ExecutionMode::Full {
                    delete_closed_heads: false,
                },
                limits(),
            ),
            ModelError::GithubRequired,
        ),
        (
            derive_plan(
                &chain,
                &BTreeMap::new(),
                PlanningGithub::Observed(&snapshot),
                &state,
                ExecutionMode::NoPr,
                limits(),
            ),
            ModelError::GithubForbidden,
        ),
        (
            derive_plan(
                &chain,
                &BTreeMap::new(),
                PlanningGithub::NotObserved,
                &state,
                ExecutionMode::DryRun(DryRunMode::Full {
                    delete_closed_heads: false,
                }),
                limits(),
            ),
            ModelError::GithubRequired,
        ),
        (
            derive_plan(
                &chain,
                &BTreeMap::new(),
                PlanningGithub::Observed(&snapshot),
                &state,
                ExecutionMode::DryRun(DryRunMode::NoPr),
                limits(),
            ),
            ModelError::GithubForbidden,
        ),
    ];
    let mut covered = BTreeSet::new();
    for (result, expected) in cases {
        let error = result.unwrap_err();
        assert_eq!(normalize_error(&error), Some(expected));
        covered.insert(expected);
    }
    assert_eq!(
        covered,
        BTreeSet::from([ModelError::GithubRequired, ModelError::GithubForbidden])
    );
}
