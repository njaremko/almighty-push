use almighty_push::domain::{
    ChangeId, CommitId, DescriptionErrorReason, DomainError, HeadRef, LimitField, LimitValues,
    Limits, PrNumber, RemoteName, RepositoryId, Revision, Scope, SelectedChain, TextKind,
};
use serde::{de::DeserializeOwned, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

const SOURCE: &str = "github.com/owner/project";
const ROOT_ID: &str = "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk";
const CHILD_ID: &str = "llllllllllllllllllllllllllllllll";
const OTHER_ID: &str = "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm";
const ROOT_COMMIT: &str = "1111111111111111111111111111111111111111";
const CHILD_COMMIT: &str = "2222222222222222222222222222222222222222";
const OTHER_COMMIT: &str = "3333333333333333333333333333333333333333";

fn scope() -> Scope {
    let repository = RepositoryId::parse(SOURCE).unwrap();
    Scope::new(
        repository.clone(),
        repository,
        RemoteName::parse("origin").unwrap(),
        HeadRef::parse("main").unwrap(),
        "@".to_owned(),
    )
    .unwrap()
}

fn revision(id: &str, commit: &str, parents: &[&str]) -> Revision {
    Revision::new(
        ChangeId::parse(id).unwrap(),
        CommitId::parse(commit).unwrap(),
        format!("change {id}"),
        parents
            .iter()
            .map(|parent| ChangeId::parse(*parent).unwrap())
            .collect::<Box<[_]>>(),
        false,
    )
    .unwrap()
}

fn change_id(index: usize) -> ChangeId {
    let alphabet = b"klmnopqrstuvwxyz";
    let mut value = [b'k'; 32];
    let mut remaining = index;
    for slot in value.iter_mut().rev() {
        *slot = alphabet[remaining % alphabet.len()];
        remaining /= alphabet.len();
    }
    ChangeId::parse(String::from_utf8(value.to_vec()).unwrap()).unwrap()
}

fn commit_id(index: usize) -> CommitId {
    CommitId::parse(format!("{index:040x}")).unwrap()
}

#[test]
fn exact_identifiers_reject_lossy_or_invalid_forms() {
    assert!(ChangeId::parse(ROOT_ID).is_ok());
    assert!(ChangeId::parse("kkkkkkkkkkkk").is_err());
    assert!(ChangeId::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_err());
    assert!(CommitId::parse(ROOT_COMMIT).is_ok());
    assert!(CommitId::parse("ABCDEF1111111111111111111111111111111111").is_err());
    assert!(HeadRef::parse("almighty-push/kkkk").is_ok());
    assert!(HeadRef::parse("feature..broken").is_err());
    assert!(HeadRef::parse("feature.lock").is_err());
}

#[test]
fn invalid_text_errors_keep_only_a_bounded_preview() {
    let invalid = "x".repeat(10_000);
    let DomainError::InvalidText {
        kind,
        actual_bytes,
        preview,
    } = RepositoryId::parse(invalid).unwrap_err()
    else {
        panic!("expected invalid text")
    };
    assert_eq!(kind, TextKind::RepositoryId);
    assert_eq!(actual_bytes, 10_000);
    assert!(preview.len() <= 64);
}

#[test]
fn repository_identity_validates_each_namespace() {
    let repository = RepositoryId::parse("GitHub.COM/Owner/Project").unwrap();
    assert_eq!(repository.canonical(), "github.com/owner/project");
    assert_eq!(repository.host(), "github.com");
    assert_eq!(repository.owner(), "owner");
    assert_eq!(repository.name(), "project");

    assert!(RepositoryId::parse("github.com/owner/.github").is_ok());
    assert!(RepositoryId::parse("github..com/owner/project").is_err());
    assert!(RepositoryId::parse("-github.com/owner/project").is_err());
    assert!(RepositoryId::parse("github.com/-owner/project").is_err());
    assert!(RepositoryId::parse("github.com/owner").is_err());
    assert!(RepositoryId::parse("github.com/owner/project/extra").is_err());
}

#[test]
fn remote_names_use_a_valid_remote_tracking_namespace() {
    assert!(RemoteName::parse("origin").is_ok());
    assert!(RemoteName::parse("team/upstream").is_ok());
    assert!(RemoteName::parse("tëam").is_ok());
    assert!(RemoteName::parse("..").is_err());
    assert!(RemoteName::parse("team//upstream").is_err());
    assert!(RemoteName::parse("team.lock").is_err());
}

#[test]
fn owned_heads_compose_for_every_full_change_id() {
    let head = HeadRef::owned(&ChangeId::parse(ROOT_ID).unwrap()).unwrap();
    assert_eq!(head.as_str(), format!("almighty-push/{ROOT_ID}"));
    assert!(head.as_str().len() < 255);
}

#[test]
fn limits_are_checked_at_construction_and_deserialization() {
    let defaults = Limits::default();
    let encoded = serde_json::to_string(&defaults).unwrap();
    assert_eq!(serde_json::from_str::<Limits>(&encoded).unwrap(), defaults);

    let invalid = LimitValues {
        change_count_max: 0,
        ..LimitValues::default()
    };
    assert_eq!(
        Limits::new(invalid).unwrap_err(),
        DomainError::InvalidLimit {
            field: LimitField::ChangeCount,
            value: 0,
            min: 1,
            max: 64,
        }
    );

    let wire = serde_json::json!({
        "change_count_max": u64::MAX,
        "github_page_count_max": 10,
        "github_page_size": 100,
        "effect_count_max": 512,
        "command_output_bytes_max": 4_194_304,
        "state_bytes_max": 4_194_304,
        "body_bytes_max": 65_536,
        "command_timeout_ms": 30_000,
        "lock_wait_ms": 5_000
    });
    assert!(serde_json::from_value::<Limits>(wire).is_err());
}

#[test]
fn every_limit_field_enforces_its_exact_compiled_range() {
    let boundaries = [
        (LimitField::ChangeCount, 1, 64),
        (LimitField::GithubPageCount, 1, 10),
        (LimitField::GithubPageSize, 1, 100),
        (LimitField::EffectCount, 1, 512),
        (LimitField::CommandOutputBytes, 65_536, 4_194_304),
        (LimitField::StateBytes, 65_536, 4_194_304),
        (LimitField::BodyBytes, 4_096, 65_536),
        (LimitField::CommandTimeoutMs, 100, 30_000),
        (LimitField::LockWaitMs, 100, 5_000),
    ];

    for (field, min, max) in boundaries {
        assert!(
            Limits::new(limit_values(field, min)).is_ok(),
            "{field:?} min"
        );
        assert!(
            Limits::new(limit_values(field, max)).is_ok(),
            "{field:?} max"
        );
        assert!(
            Limits::new(limit_values(field, max + 1)).is_err(),
            "{field:?} max+1"
        );
        assert!(
            Limits::new(limit_values(field, min - 1)).is_err(),
            "{field:?} min-1"
        );
    }
}

fn limit_values(field: LimitField, value: u64) -> LimitValues {
    let mut values = LimitValues::default();
    match field {
        LimitField::ChangeCount => values.change_count_max = value,
        LimitField::GithubPageCount => values.github_page_count_max = value,
        LimitField::GithubPageSize => values.github_page_size = value,
        LimitField::EffectCount => values.effect_count_max = value,
        LimitField::CommandOutputBytes => {
            values.command_output_bytes_max = value;
            values.github_page_size = 8;
        }
        LimitField::StateBytes => values.state_bytes_max = value,
        LimitField::BodyBytes => values.body_bytes_max = value,
        LimitField::CommandTimeoutMs => values.command_timeout_ms = value,
        LimitField::LockWaitMs => values.lock_wait_ms = value,
    }
    values
}

#[test]
fn revision_preserves_bounded_exact_multiline_descriptions() {
    let description = "subject | exact\n\nmultiline unicode λ\n".to_owned();
    let row = Revision::new(
        ChangeId::parse(ROOT_ID).unwrap(),
        CommitId::parse(ROOT_COMMIT).unwrap(),
        description.clone(),
        Box::new([]),
        false,
    )
    .unwrap();
    assert_eq!(row.description(), description);

    assert_eq!(
        Revision::new(
            ChangeId::parse(ROOT_ID).unwrap(),
            CommitId::parse(ROOT_COMMIT).unwrap(),
            "contains \0 nul".to_owned(),
            Box::new([]),
            false,
        )
        .unwrap_err(),
        DomainError::InvalidDescription {
            change_id: ChangeId::parse(ROOT_ID).unwrap(),
            reason: DescriptionErrorReason::Nul,
            actual_bytes: 14,
        }
    );
}

#[test]
fn selected_chain_accepts_empty_singleton_and_orders_a_linear_selection() {
    let empty = SelectedChain::new(scope(), Box::new([]), Limits::default()).unwrap();
    assert!(empty.revisions().is_empty());

    let singleton = SelectedChain::new(
        scope(),
        Box::new([revision(ROOT_ID, ROOT_COMMIT, &[OTHER_ID])]),
        Limits::default(),
    )
    .unwrap();
    assert_eq!(singleton.revisions().len(), 1);

    let child = revision(CHILD_ID, CHILD_COMMIT, &[ROOT_ID]);
    let root = revision(ROOT_ID, ROOT_COMMIT, &[OTHER_ID]);
    let chain = SelectedChain::new(
        scope(),
        vec![child, root].into_boxed_slice(),
        Limits::default(),
    )
    .unwrap();

    let ordered = chain
        .revisions()
        .iter()
        .map(|row| row.change_id().as_str())
        .collect::<Vec<_>>();
    assert_eq!(ordered, vec![ROOT_ID, CHILD_ID]);
}

#[test]
fn selected_chain_results_are_independent_of_input_order() {
    let rows = [
        revision(ROOT_ID, ROOT_COMMIT, &[]),
        revision(CHILD_ID, CHILD_COMMIT, &[ROOT_ID]),
        revision(OTHER_ID, OTHER_COMMIT, &[CHILD_ID]),
    ];
    let permutations = [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ];

    for permutation in permutations {
        let input = permutation
            .map(|index| rows[index].clone())
            .into_iter()
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let chain = SelectedChain::new(scope(), input, Limits::default()).unwrap();
        let ordered = chain
            .revisions()
            .iter()
            .map(|row| row.change_id().as_str())
            .collect::<Vec<_>>();
        assert_eq!(ordered, vec![ROOT_ID, CHILD_ID, OTHER_ID]);
    }

    let merge_root = revision(ROOT_ID, ROOT_COMMIT, &[CHILD_ID, OTHER_ID]);
    let merge_child = revision(CHILD_ID, CHILD_COMMIT, &[ROOT_ID, OTHER_ID]);
    let forward = SelectedChain::new(
        scope(),
        vec![merge_child.clone(), merge_root.clone()].into_boxed_slice(),
        Limits::default(),
    )
    .unwrap_err();
    let reverse = SelectedChain::new(
        scope(),
        vec![merge_root, merge_child].into_boxed_slice(),
        Limits::default(),
    )
    .unwrap_err();
    assert_eq!(forward, reverse);
}

#[test]
fn selected_chain_rejects_a_merge_node() {
    let revisions = vec![
        revision(ROOT_ID, ROOT_COMMIT, &[OTHER_ID]),
        revision(CHILD_ID, CHILD_COMMIT, &[ROOT_ID, OTHER_ID]),
    ]
    .into_boxed_slice();

    assert_eq!(
        SelectedChain::new(scope(), revisions, Limits::default()).unwrap_err(),
        DomainError::SelectedMerge {
            change_id: ChangeId::parse(CHILD_ID).unwrap(),
        }
    );
}

#[test]
fn selected_chain_rejects_a_fork() {
    let revisions = vec![
        revision(ROOT_ID, ROOT_COMMIT, &[OTHER_ID]),
        revision(CHILD_ID, CHILD_COMMIT, &[ROOT_ID]),
        revision(OTHER_ID, OTHER_COMMIT, &[ROOT_ID]),
    ]
    .into_boxed_slice();

    assert_eq!(
        SelectedChain::new(scope(), revisions, Limits::default()).unwrap_err(),
        DomainError::SelectedFork {
            parent_change_id: ChangeId::parse(ROOT_ID).unwrap(),
        }
    );
}

#[test]
fn selected_chain_rejects_disconnected_roots_and_cycles_precisely() {
    let disconnected = vec![
        revision(ROOT_ID, ROOT_COMMIT, &[]),
        revision(CHILD_ID, CHILD_COMMIT, &[]),
    ]
    .into_boxed_slice();
    assert_eq!(
        SelectedChain::new(scope(), disconnected, Limits::default()).unwrap_err(),
        DomainError::DisconnectedSelection { root_count: 2 }
    );

    let cycle = vec![
        revision(ROOT_ID, ROOT_COMMIT, &[CHILD_ID]),
        revision(CHILD_ID, CHILD_COMMIT, &[ROOT_ID]),
    ]
    .into_boxed_slice();
    assert_eq!(
        SelectedChain::new(scope(), cycle, Limits::default()).unwrap_err(),
        DomainError::SelectedCycle {
            change_id: ChangeId::parse(ROOT_ID).unwrap(),
        }
    );
}

#[test]
fn selected_chain_rejects_duplicate_change_ids_deterministically() {
    let root_duplicate = revision(ROOT_ID, CHILD_COMMIT, &[]);
    let child_duplicate = revision(CHILD_ID, OTHER_COMMIT, &[]);
    let rows = vec![
        revision(CHILD_ID, CHILD_COMMIT, &[]),
        revision(ROOT_ID, ROOT_COMMIT, &[]),
        root_duplicate,
        child_duplicate,
    ];
    let forward = SelectedChain::new(scope(), rows.clone().into_boxed_slice(), Limits::default())
        .unwrap_err();
    let reverse = SelectedChain::new(
        scope(),
        rows.into_iter()
            .rev()
            .collect::<Vec<_>>()
            .into_boxed_slice(),
        Limits::default(),
    )
    .unwrap_err();
    assert_eq!(forward, reverse);
    assert_eq!(
        forward,
        DomainError::DuplicateChangeId {
            change_id: ChangeId::parse(ROOT_ID).unwrap(),
        }
    );
}

#[test]
fn selected_chain_accepts_exactly_the_configured_maximum() {
    let revisions = (0..64)
        .map(|index| {
            let parents = if index == 0 {
                Box::new([])
            } else {
                Box::new([change_id(index - 1)]) as Box<[ChangeId]>
            };
            Revision::new(
                change_id(index),
                commit_id(index),
                format!("change {index}"),
                parents,
                false,
            )
            .unwrap()
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let chain = SelectedChain::new(scope(), revisions, Limits::default()).unwrap();
    assert_eq!(chain.revisions().len(), 64);
}

#[test]
fn selected_chain_rejects_more_than_the_configured_maximum_first() {
    let revisions = (0..65)
        .map(|index| {
            Revision::new(
                change_id(index),
                commit_id(index),
                format!("change {index}"),
                Box::new([]),
                false,
            )
            .unwrap()
        })
        .collect::<Vec<_>>()
        .into_boxed_slice();

    assert_eq!(
        SelectedChain::new(scope(), revisions, Limits::default()).unwrap_err(),
        DomainError::ChangeCountExceeded { count: 65, max: 64 }
    );
}

#[test]
fn exhaustive_small_graphs_match_an_independent_path_oracle() {
    for node_count in 1_usize..=4 {
        let choice_count = node_count + 1;
        let assignment_count = choice_count.pow(node_count as u32);
        for encoded in 0..assignment_count {
            let mut value = encoded;
            let mut parents = Vec::with_capacity(node_count);
            for _ in 0..node_count {
                parents.push(value % choice_count);
                value /= choice_count;
            }

            let revisions = (0..node_count)
                .map(|index| {
                    let parent_choice = parents[index];
                    let parent_ids = if parent_choice == node_count {
                        Box::new([])
                    } else {
                        Box::new([change_id(parent_choice)]) as Box<[ChangeId]>
                    };
                    Revision::new(
                        change_id(index),
                        commit_id(index),
                        format!("change {index}"),
                        parent_ids,
                        false,
                    )
                    .unwrap()
                })
                .collect::<Vec<_>>()
                .into_boxed_slice();

            let accepted = SelectedChain::new(scope(), revisions, Limits::default()).is_ok();
            assert_eq!(accepted, is_one_path(&parents, node_count), "{parents:?}");
        }
    }
}

fn is_one_path(parents: &[usize], outside: usize) -> bool {
    let mut children = BTreeMap::<usize, usize>::new();
    let roots = parents
        .iter()
        .enumerate()
        .filter_map(|(node, parent)| {
            if *parent == outside {
                return Some(node);
            }
            if children.insert(*parent, node).is_some() {
                return None;
            }
            Some(outside)
        })
        .filter(|node| *node != outside)
        .collect::<Vec<_>>();
    if roots.len() != 1 || children.len() != parents.len() - 1 {
        return false;
    }

    let mut visited = BTreeSet::new();
    let mut current = Some(roots[0]);
    while let Some(node) = current {
        if !visited.insert(node) {
            return false;
        }
        current = children.get(&node).copied();
    }
    visited.len() == parents.len()
}

#[test]
fn every_validated_serializable_owner_is_serde_closed() {
    assert_round_trip(ChangeId::parse(ROOT_ID).unwrap());
    assert_round_trip(CommitId::parse(ROOT_COMMIT).unwrap());
    assert_round_trip(HeadRef::parse("main").unwrap());
    assert_round_trip(RemoteName::parse("origin").unwrap());
    assert_round_trip(RepositoryId::parse(SOURCE).unwrap());
    assert_round_trip(PrNumber::new(1).unwrap());
    assert_round_trip(scope());
    assert_round_trip(Limits::default());

    assert!(serde_json::from_str::<ChangeId>("\"short\"").is_err());
    assert!(serde_json::from_str::<CommitId>("\"short\"").is_err());
    assert!(serde_json::from_str::<HeadRef>("\"bad..ref\"").is_err());
    assert!(serde_json::from_str::<RemoteName>("\"..\"").is_err());
    assert!(serde_json::from_str::<RepositoryId>("\"missing/scope\"").is_err());
    assert!(serde_json::from_str::<PrNumber>("0").is_err());
}

fn assert_round_trip<T>(value: T)
where
    T: Debug + DeserializeOwned + Eq + Serialize,
{
    let encoded = serde_json::to_vec(&value).unwrap();
    assert_eq!(serde_json::from_slice::<T>(&encoded).unwrap(), value);
}
