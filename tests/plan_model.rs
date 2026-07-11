use almighty_push::body::ManagedSection;
use almighty_push::domain::{
    ChangeId, HeadRef, LimitValues, Limits, PrLifecycle, PrNumber, RemoteName, RepositoryId, Scope,
};
use almighty_push::plan::{
    BodyHash, Effect, GithubEffect, OwnershipEffect, Plan, PlanError, PrOwnership, ReobserveBarrier,
};

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

fn change(value: char) -> ChangeId {
    ChangeId::parse(std::iter::repeat_n(value, 32).collect::<String>()).unwrap()
}

fn ownership(change_id: ChangeId) -> PrOwnership {
    PrOwnership::generated(change_id).unwrap()
}

fn managed_section(source: &Scope, current: &ChangeId) -> String {
    ManagedSection::new(
        source.source_repository().clone(),
        current.clone(),
        vec![current.clone()].into_boxed_slice(),
        limits(),
    )
    .unwrap()
    .render()
}

#[test]
fn canonical_plan_identity_is_stable_and_binds_scope_and_effects() {
    let effects = vec![Effect::Reobserve(ReobserveBarrier::All)].into_boxed_slice();
    let first = Plan::new(scope("owner"), effects.clone(), limits()).unwrap();
    let repeated = Plan::new(scope("owner"), effects, limits()).unwrap();
    let other_scope = Plan::new(
        scope("other"),
        vec![Effect::Reobserve(ReobserveBarrier::All)].into_boxed_slice(),
        limits(),
    )
    .unwrap();
    let other_effect = Plan::new(
        scope("owner"),
        vec![Effect::Reobserve(ReobserveBarrier::Github)].into_boxed_slice(),
        limits(),
    )
    .unwrap();

    assert_eq!(first.id(), repeated.id());
    assert_eq!(
        first.id().to_hex(),
        "6e51534275dcda3315fed1330e553dfad30ffea07deb4984188aba09826a6242"
    );
    assert_ne!(first.id(), other_scope.id());
    assert_ne!(first.id(), other_effect.id());
}

#[test]
fn create_result_references_and_barriers_are_structural_laws() {
    let plan_scope = scope("owner");
    let created_change = change('k');
    let create = Effect::Github(GithubEffect::CreatePullRequest {
        change_id: created_change.clone(),
        head: HeadRef::owned(&created_change).unwrap(),
        base: HeadRef::parse("main").unwrap(),
        title: "title".to_owned(),
        managed_section: managed_section(&plan_scope, &created_change),
    });
    let install = Effect::Ownership(OwnershipEffect::InstallCreated {
        change_id: created_change,
        expected_absent: true,
        create_effect_index: 0,
        lifecycle: PrLifecycle::Open,
    });
    Plan::new(
        plan_scope.clone(),
        vec![create.clone(), install].into_boxed_slice(),
        limits(),
    )
    .unwrap();

    let bad_reference = Effect::Ownership(OwnershipEffect::InstallCreated {
        change_id: change('l'),
        expected_absent: true,
        create_effect_index: 0,
        lifecycle: PrLifecycle::Open,
    });
    assert!(matches!(
        Plan::new(
            plan_scope.clone(),
            vec![create, bad_reference].into_boxed_slice(),
            limits()
        ),
        Err(PlanError::InvalidCreateReference)
    ));
    assert!(matches!(
        Plan::new(
            scope("owner"),
            vec![
                Effect::Reobserve(ReobserveBarrier::Github),
                Effect::Github(GithubEffect::SetLifecycle {
                    ownership: ownership(change('k')),
                    number: PrNumber::new(1).unwrap(),
                    expected: PrLifecycle::Open,
                    desired: PrLifecycle::Closed,
                }),
            ]
            .into_boxed_slice(),
            limits()
        ),
        Err(PlanError::BarrierNotTerminal)
    ));
}

#[test]
fn mutation_authority_and_transition_laws_reject_invalid_generated_shapes_and_no_ops() {
    let plan_scope = scope("owner");
    let current = change('k');
    let section = managed_section(&plan_scope, &current);
    let canonical_head = HeadRef::owned(&current).unwrap();
    let create_wrong_head = Effect::Github(GithubEffect::CreatePullRequest {
        change_id: current.clone(),
        head: HeadRef::parse("attacker/head").unwrap(),
        base: HeadRef::parse("main").unwrap(),
        title: "title".to_owned(),
        managed_section: section.clone(),
    });
    assert!(matches!(
        Plan::new(
            plan_scope.clone(),
            vec![create_wrong_head].into_boxed_slice(),
            limits()
        ),
        Err(PlanError::InvalidEffect)
    ));

    let wrong_source_section = ManagedSection::new(
        scope("other").source_repository().clone(),
        current.clone(),
        vec![current.clone()].into_boxed_slice(),
        limits(),
    )
    .unwrap()
    .render();
    let create_wrong_source = Effect::Github(GithubEffect::CreatePullRequest {
        change_id: current.clone(),
        head: canonical_head,
        base: HeadRef::parse("main").unwrap(),
        title: "title".to_owned(),
        managed_section: wrong_source_section,
    });
    assert!(matches!(
        Plan::new(
            plan_scope.clone(),
            vec![create_wrong_source].into_boxed_slice(),
            limits()
        ),
        Err(PlanError::InvalidEffect)
    ));

    let owner = ownership(current.clone());
    for invalid in [
        Effect::Github(GithubEffect::UpdateBase {
            ownership: owner.clone(),
            number: PrNumber::new(1).unwrap(),
            expected: HeadRef::parse("main").unwrap(),
            desired: HeadRef::parse("main").unwrap(),
        }),
        Effect::Github(GithubEffect::UpdateBody {
            ownership: owner.clone(),
            number: PrNumber::new(1).unwrap(),
            expected_hash: BodyHash::of(b"same"),
            desired_hash: BodyHash::of(b"same"),
            managed_section: section,
        }),
        Effect::Github(GithubEffect::SetLifecycle {
            ownership: owner,
            number: PrNumber::new(1).unwrap(),
            expected: PrLifecycle::Merged,
            desired: PrLifecycle::Open,
        }),
    ] {
        assert!(matches!(
            Plan::new(
                plan_scope.clone(),
                vec![invalid].into_boxed_slice(),
                limits()
            ),
            Err(PlanError::InvalidEffect)
        ));
    }
}

#[test]
fn serialized_noncanonical_generated_proof_is_rejected_by_plan_validation() {
    let forged: PrOwnership = serde_json::from_value(serde_json::json!({
        "change_id": "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk",
        "head": "attacker/head",
        "proof": "Generated",
    }))
    .unwrap();
    let effect = Effect::Github(GithubEffect::UpdateBase {
        ownership: forged,
        number: PrNumber::new(1).unwrap(),
        expected: HeadRef::parse("main").unwrap(),
        desired: HeadRef::parse("next").unwrap(),
    });

    assert!(matches!(
        Plan::new(scope("owner"), vec![effect].into_boxed_slice(), limits()),
        Err(PlanError::InvalidEffect)
    ));
}

#[test]
fn effect_count_accepts_the_configured_maximum_and_rejects_one_more() {
    let make_effects = |count: usize| {
        (0..count)
            .map(|index| {
                let change_id = change('k');
                Effect::Github(GithubEffect::UpdateBase {
                    ownership: ownership(change_id),
                    number: PrNumber::new((index + 1) as u64).unwrap(),
                    expected: HeadRef::parse("main").unwrap(),
                    desired: HeadRef::parse("next").unwrap(),
                })
            })
            .collect::<Vec<_>>()
            .into_boxed_slice()
    };
    let maximum = Plan::new(
        scope("owner"),
        make_effects(limits().effect_count_max()),
        limits(),
    )
    .unwrap();
    assert_eq!(maximum.publication_count_max(), 514);
    assert!(matches!(
        Plan::new(
            scope("owner"),
            make_effects(limits().effect_count_max() + 1),
            limits()
        ),
        Err(PlanError::EffectCount)
    ));
}
