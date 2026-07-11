use almighty_push::body::{BodyMerge, ManagedBody, ManagedSection};
use almighty_push::command::{
    CommandError, CommandExecutor, CommandOutput, CommandRunner, CommandSpec,
};
use almighty_push::config::{ConfigInput, ConfigResolver, ResolvedConfig};
use almighty_push::domain::{
    ChangeId, CommitId, HeadRef, LimitValues, Limits, PrLifecycle, PrNumber, RemoteName,
    RepositoryId, Scope,
};
use almighty_push::executor::{
    Executor, ExecutorError, FullEffectDriver, StageCompletion, StageOutcome,
};
use almighty_push::github::{GithubClient, GithubError, GithubQuery};
use almighty_push::jj::JjClient;
use almighty_push::plan::{BodyHash, Effect, GithubEffect, JjEffect, Plan, PlanError, PrOwnership};
use almighty_push::state::{
    CheckpointState, LegacyDisposition, LegacyRejectionReason, StateError, StateStore, StateV3,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const CHANGE: &str = "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk";
const NEXT: &str = "llllllllllllllllllllllllllllllll";
const COMMIT: &str = "1111111111111111111111111111111111111111";

#[derive(Clone, Debug)]
struct Call {
    args: Vec<OsString>,
    stdin: Vec<u8>,
}

fn query_parameter(call: &Call, name: &str) -> Option<usize> {
    call.args.iter().find_map(|argument| {
        let argument = argument.to_str()?;
        let (_, query) = argument.split_once('?')?;
        query.split('&').find_map(|component| {
            let (component_name, value) = component.split_once('=')?;
            (component_name == name)
                .then(|| value.parse().ok())
                .flatten()
        })
    })
}

struct FakeGh {
    replies: Mutex<VecDeque<Result<CommandOutput, CommandError>>>,
    calls: Mutex<Vec<Call>>,
}

impl FakeGh {
    fn new(replies: impl IntoIterator<Item = Value>) -> Self {
        Self {
            replies: Mutex::new(
                replies
                    .into_iter()
                    .map(|value| {
                        Ok(CommandOutput {
                            stdout: value.to_string(),
                            stderr: String::new(),
                        })
                    })
                    .collect(),
            ),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn with_raw(replies: impl IntoIterator<Item = Result<String, CommandError>>) -> Self {
        Self {
            replies: Mutex::new(
                replies
                    .into_iter()
                    .map(|result| {
                        result.map(|stdout| CommandOutput {
                            stdout,
                            stderr: String::new(),
                        })
                    })
                    .collect(),
            ),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

impl CommandExecutor for FakeGh {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        self.calls.lock().unwrap().push(Call {
            args: spec.args().to_vec(),
            stdin: spec.stdin().to_vec(),
        });
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected fake-gh invocation")
    }
}

fn limits() -> Limits {
    Limits::new(LimitValues::default()).unwrap()
}

fn pagination_limits(page_count: u64, page_size: u64) -> Limits {
    Limits::new(LimitValues {
        github_page_count_max: page_count,
        github_page_size: page_size,
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

fn workspace(label: &str) -> PathBuf {
    for sequence in 0..1_000 {
        let root = std::env::temp_dir().join(format!(
            "almighty-push-github-{label}-{}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&root) {
            Ok(()) => {
                fs::create_dir(root.join(".jj")).unwrap();
                return root.canonicalize().unwrap();
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("fixture creation failed: {error}"),
        }
    }
    panic!("fixture attempts exhausted")
}

fn resolved(root: &Path) -> ResolvedConfig {
    resolved_with_limits(root, limits())
}

fn resolved_with_limits(root: &Path, configured_limits: Limits) -> ResolvedConfig {
    resolved_for(root, "github.com/target/project", configured_limits)
}

fn resolved_for(root: &Path, target: &str, configured_limits: Limits) -> ResolvedConfig {
    struct ResolverExecutor {
        replies: Mutex<VecDeque<CommandOutput>>,
    }

    impl CommandExecutor for ResolverExecutor {
        fn run(&self, _spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
            Ok(self.replies.lock().unwrap().pop_front().unwrap())
        }
    }

    let executor = ResolverExecutor {
        replies: Mutex::new(VecDeque::from([
            CommandOutput {
                stdout: root.display().to_string(),
                stderr: String::new(),
            },
            CommandOutput {
                stdout: "origin https://github.com/source/project.git\n".to_owned(),
                stderr: String::new(),
            },
        ])),
    };
    ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    )
    .resolve(
        &ConfigInput {
            remote: Some(RemoteName::parse("origin").unwrap()),
            repository: Some(RepositoryId::parse(target).unwrap()),
            base: Some(HeadRef::parse("main").unwrap()),
            tip_revset: "@".to_owned(),
            limits: configured_limits,
            github_enabled: false,
        },
        root,
    )
    .unwrap()
}

fn client<'a>(fake: &'a FakeGh, root: &Path) -> GithubClient<'a, FakeGh> {
    client_with_limits(fake, root, limits())
}

fn client_with_limits<'a>(
    fake: &'a FakeGh,
    root: &Path,
    configured_limits: Limits,
) -> GithubClient<'a, FakeGh> {
    let config = resolved_with_limits(root, configured_limits);
    GithubClient::new(
        fake,
        PathBuf::from("/fake/gh"),
        &config,
        &StateV3::empty(config.scope().clone()),
        Box::new([]),
    )
}

fn client_with_state<'a, E: CommandExecutor>(
    executor: &'a E,
    root: &Path,
    state: &StateV3,
) -> GithubClient<'a, E> {
    GithubClient::new(
        executor,
        PathBuf::from("/fake/gh"),
        &resolved(root),
        state,
        Box::new([]),
    )
}

fn repository(full_name: &str) -> Value {
    json!({ "full_name": full_name, "default_branch": "main" })
}

fn change_at(index: usize) -> String {
    assert!(index < 256);
    let high = char::from_u32(u32::from(b'k') + (index / 16) as u32).unwrap();
    let low = char::from_u32(u32::from(b'k') + (index % 16) as u32).unwrap();
    format!("{}{}{}", "k".repeat(30), high, low)
}

fn owned_ref(change: &str) -> Value {
    json!({
        "ref": format!("refs/heads/almighty-push/{change}"),
        "object": { "type": "commit", "sha": COMMIT },
    })
}

fn invalid_utf8_error() -> std::str::Utf8Error {
    let bytes = std::hint::black_box(vec![0xff]);
    String::from_utf8(bytes).unwrap_err().utf8_error()
}

fn unrelated(number: u64) -> Value {
    json!({
        "number": number,
        "state": "open",
        "merged_at": null,
        "title": "unrelated",
        "head_ref": format!("user-{number}"),
        "head_sha": COMMIT,
        "head_repository": "other/project",
        "base_ref": "main",
        "base_repository": "target/project",
    })
}

fn ownership(change: &str) -> PrOwnership {
    PrOwnership::generated(ChangeId::parse(change).unwrap()).unwrap()
}

fn managed_section(change: &str) -> ManagedSection {
    let change = ChangeId::parse(change).unwrap();
    ManagedSection::new(
        scope().source_repository().clone(),
        change.clone(),
        vec![change].into_boxed_slice(),
        limits(),
    )
    .unwrap()
}

fn write_legacy(
    root: &Path,
    legacy_key: &str,
    url: &str,
    branch: &str,
    commit: &str,
    change: Option<&str>,
) {
    fs::write(
        root.join(".almighty"),
        json!({
            "version": 2,
            "prs": {
                (legacy_key): {
                    "pr_number": 8,
                    "pr_url": url,
                    "branch_name": branch,
                    "commit_id": commit,
                    "change_id": change,
                }
            },
            "merged_prs": [],
            "closed_prs": [],
            "last_operation_id": null,
        })
        .to_string(),
    )
    .unwrap();
}

fn managed_body(change: &str) -> String {
    let almighty_push::body::BodyMerge::Changed(body) = ManagedBody::merge(
        "user bytes",
        &managed_section(change),
        limits().body_bytes_max(),
    )
    .unwrap() else {
        panic!("section append must change")
    };
    body
}

fn compact(number: u64, change: &str) -> Value {
    json!({
        "number": number,
        "state": "open",
        "merged_at": null,
        "title": format!("change {change}"),
        "head_ref": format!("almighty-push/{change}"),
        "head_sha": COMMIT,
        "head_repository": "source/project",
        "base_ref": "main",
        "base_repository": "target/project",
    })
}

fn detail(number: u64, change: &str, base: &str, state: &str) -> Value {
    json!({
        "number": number,
        "state": state,
        "merged_at": null,
        "title": format!("change {change}"),
        "head_ref": format!("almighty-push/{change}"),
        "head_sha": COMMIT,
        "head_repository": "source/project",
        "base_ref": base,
        "base_repository": "target/project",
        "body": managed_body(change),
    })
}

#[test]
fn snapshot_observes_source_refs_and_target_prs_with_body_free_list_projection() {
    let fake = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([{
            "ref": format!("refs/heads/almighty-push/{CHANGE}"),
            "object": { "type": "commit", "sha": COMMIT },
        }]),
        json!([compact(8, CHANGE)]),
        detail(8, CHANGE, "main", "open"),
    ]);
    let root = workspace("snapshot");
    let snapshot = client(&fake, &root).observe().unwrap();
    assert_eq!(snapshot.source_repository(), scope().source_repository());
    assert_eq!(snapshot.target_repository(), scope().target_repository());
    assert_eq!(snapshot.configured_base().as_str(), "main");
    assert_eq!(snapshot.owned_heads().len(), 1);
    assert_eq!(snapshot.prs().len(), 1);

    let calls = fake.calls();
    let ref_endpoint = calls[2]
        .args
        .iter()
        .map(|v| v.to_string_lossy())
        .collect::<Vec<_>>();
    assert!(ref_endpoint
        .iter()
        .any(|arg| arg.contains("repos/source/project/git/matching-refs")));
    let pr_endpoint = calls[3]
        .args
        .iter()
        .map(|v| v.to_string_lossy())
        .collect::<Vec<_>>();
    assert!(pr_endpoint
        .iter()
        .any(|arg| arg.contains("repos/target/project/pulls?")));
    let projection = pr_endpoint.last().unwrap();
    assert!(!projection.contains("body"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_full_tenth_page_is_incomplete_and_never_requests_page_eleven() {
    let mut replies = vec![
        repository("source/project"),
        repository("target/project"),
        json!([]),
    ];
    replies.extend((0..10).map(|page| {
        Value::Array(
            (1..=100)
                .map(|row| {
                    let number = page * 100 + row;
                    json!({
                        "number": number,
                        "state": "open",
                        "merged_at": null,
                        "title": "unrelated",
                        "head_ref": format!("user-{number}"),
                        "head_sha": COMMIT,
                        "head_repository": "other/project",
                        "base_ref": "main",
                        "base_repository": "target/project",
                    })
                })
                .collect(),
        )
    }));
    let fake = FakeGh::new(replies);
    let root = workspace("pagination-cap");
    assert!(matches!(
        client(&fake, &root).observe(),
        Err(GithubError::ObservationIncomplete {
            query: GithubQuery::PullRequests,
            page: 10,
            per_page: 100,
        })
    ));
    assert_eq!(fake.calls().len(), 13);
    let pull_calls = fake
        .calls()
        .into_iter()
        .filter(|call| {
            call.args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("/pulls?"))
        })
        .collect::<Vec<_>>();
    let pages = pull_calls
        .iter()
        .filter_map(|call| query_parameter(call, "page"))
        .collect::<Vec<_>>();
    assert_eq!(pages, (1..=10).collect::<Vec<_>>());
    assert!(pull_calls
        .iter()
        .all(|call| query_parameter(call, "per_page") == Some(100)));
    assert!(!pages.contains(&11));
    assert!(!fake.calls().iter().any(|call| {
        query_parameter(call, "page") == Some(11)
            && call
                .args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("/pulls?"))
    }));
    assert!(!fake.calls().iter().any(|call| call.args.iter().any(|arg| {
        let arg = arg.to_string_lossy();
        arg.contains("repos/target/project/pulls/") && !arg.contains('?')
    })));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn repository_metadata_schema_and_scope_fail_with_the_exact_query() {
    let source_cases = [
        (repository("other/project"), true),
        (json!({"full_name":"source/project"}), false),
        (
            json!({"full_name":"source/project","default_branch":null}),
            false,
        ),
        (
            json!({"full_name":"source/project","default_branch":"bad ref?"}),
            false,
        ),
        (
            json!({"full_name":"source/project","default_branch":"main","extra":1}),
            false,
        ),
    ];
    for (index, (response, scope_error)) in source_cases.into_iter().enumerate() {
        let fake = FakeGh::new([response]);
        let root = workspace(&format!("source-metadata-{index}"));
        let error = client(&fake, &root).observe().unwrap_err();
        if scope_error {
            assert!(matches!(
                error,
                GithubError::ScopeMismatch {
                    query: GithubQuery::SourceRepository
                }
            ));
        } else {
            assert!(matches!(
                error,
                GithubError::Malformed {
                    query: GithubQuery::SourceRepository
                }
            ));
        }
        fs::remove_dir_all(root).unwrap();
    }

    for (index, response) in [
        repository("other/project"),
        json!({"full_name":"target/project"}),
        json!({"full_name":"target/project","default_branch":null}),
    ]
    .into_iter()
    .enumerate()
    {
        let fake = FakeGh::new([repository("source/project"), response]);
        let root = workspace(&format!("target-metadata-{index}"));
        let error = client(&fake, &root).observe().unwrap_err();
        assert!(matches!(
            error,
            GithubError::ScopeMismatch {
                query: GithubQuery::TargetRepository
            } | GithubError::Malformed {
                query: GithubQuery::TargetRepository
            }
        ));
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn same_repository_scope_still_observes_and_validates_both_repository_roles() {
    let fake = FakeGh::new([
        repository("source/project"),
        repository("source/project"),
        json!([]),
        json!([]),
    ]);
    let root = workspace("same-repository");
    let config = resolved_for(&root, "github.com/source/project", limits());
    let snapshot = GithubClient::new(
        &fake,
        PathBuf::from("/fake/gh"),
        &config,
        &StateV3::empty(config.scope().clone()),
        Box::new([]),
    )
    .observe()
    .unwrap();
    assert_eq!(snapshot.source_repository(), snapshot.target_repository());
    let calls = fake.calls();
    assert!(calls[0]
        .args
        .iter()
        .any(|arg| arg == "repos/source/project"));
    assert!(calls[1]
        .args
        .iter()
        .any(|arg| arg == "repos/source/project"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ref_pagination_is_manual_complete_and_bounded_independently_from_pr_pages() {
    let configured = pagination_limits(10, 6);
    let mut complete_replies = vec![repository("source/project"), repository("target/project")];
    complete_replies.extend((0..9).map(|page| {
        Value::Array(
            (0..6)
                .map(|row| owned_ref(&change_at(page * 6 + row)))
                .collect(),
        )
    }));
    complete_replies.push(Value::Array(
        (54..59).map(|index| owned_ref(&change_at(index))).collect(),
    ));
    complete_replies.push(json!([]));
    let complete = FakeGh::new(complete_replies);
    let root = workspace("ref-page-ten-short");
    let snapshot = client_with_limits(&complete, &root, configured)
        .observe()
        .unwrap();
    assert_eq!(snapshot.owned_heads().len(), 59);
    let calls = complete.calls();
    assert_eq!(calls.len(), 13);
    assert!(calls.iter().all(|call| !call
        .args
        .iter()
        .any(|arg| arg.to_string_lossy() == "--paginate")));
    for page in 1..=10 {
        assert!(calls.iter().any(|call| call.args.iter().any(|arg| {
            arg.to_string_lossy()
                .contains(&format!("per_page=6&page={page}"))
        })));
    }
    fs::remove_dir_all(root).unwrap();

    let mut incomplete_replies = vec![repository("source/project"), repository("target/project")];
    incomplete_replies.extend((0..10).map(|page| {
        Value::Array(
            (0..6)
                .map(|row| owned_ref(&change_at(page * 6 + row)))
                .collect(),
        )
    }));
    let incomplete = FakeGh::new(incomplete_replies);
    let root = workspace("ref-page-ten-full");
    assert!(matches!(
        client_with_limits(&incomplete, &root, configured).observe(),
        Err(GithubError::ObservationIncomplete {
            query: GithubQuery::OwnedRefs,
            page: 10,
            per_page: 6,
        })
    ));
    assert!(!incomplete.calls().iter().any(|call| call
        .args
        .iter()
        .any(|arg| arg.to_string_lossy().contains("page=11"))));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pr_pagination_accepts_a_short_tenth_page_and_rejects_oversized_pages() {
    let mut replies = vec![
        repository("source/project"),
        repository("target/project"),
        json!([]),
    ];
    replies.extend(
        (0..9).map(|page| Value::Array((1..=100).map(|row| unrelated(page * 100 + row)).collect())),
    );
    replies.push(Value::Array((901..=999).map(unrelated).collect()));
    let fake = FakeGh::new(replies);
    let root = workspace("pr-page-ten-short");
    assert!(client(&fake, &root).observe().unwrap().prs().is_empty());
    assert_eq!(fake.calls().len(), 13);
    fs::remove_dir_all(root).unwrap();

    let oversized = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([]),
        Value::Array((1..=101).map(unrelated).collect()),
    ]);
    let root = workspace("pr-page-oversized");
    assert!(matches!(
        client(&oversized, &root).observe(),
        Err(GithubError::Bound {
            query: GithubQuery::PullRequests,
            count: 101,
            max: 100,
        })
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn pagination_rejects_cross_page_duplicate_refs_and_prs() {
    let configured = pagination_limits(3, 2);
    let duplicate_ref = owned_ref(CHANGE);
    let fake = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([duplicate_ref.clone(), owned_ref(NEXT)]),
        json!([duplicate_ref]),
    ]);
    let root = workspace("duplicate-ref-pages");
    assert!(matches!(
        client_with_limits(&fake, &root, configured).observe(),
        Err(GithubError::Duplicate {
            query: GithubQuery::OwnedRefs
        })
    ));
    fs::remove_dir_all(root).unwrap();

    let fake = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([]),
        json!([unrelated(1), unrelated(2)]),
        json!([unrelated(2)]),
    ]);
    let root = workspace("duplicate-pr-pages");
    assert!(matches!(
        client_with_limits(&fake, &root, configured).observe(),
        Err(GithubError::Duplicate {
            query: GithubQuery::PullRequests
        })
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sixty_four_candidates_fetch_exactly_sixty_four_body_details() {
    let changes = (0..64).map(change_at).collect::<Vec<_>>();
    let mut replies = vec![
        repository("source/project"),
        repository("target/project"),
        json!([]),
        Value::Array(
            changes
                .iter()
                .enumerate()
                .map(|(index, change)| compact((index + 1) as u64, change))
                .collect(),
        ),
    ];
    replies.extend(
        changes
            .iter()
            .enumerate()
            .map(|(index, change)| detail((index + 1) as u64, change, "main", "open")),
    );
    let fake = FakeGh::new(replies);
    let root = workspace("candidate-max");
    assert_eq!(client(&fake, &root).observe().unwrap().prs().len(), 64);
    let detail_count = fake
        .calls()
        .iter()
        .filter(|call| {
            call.args.iter().any(|arg| {
                let arg = arg.to_string_lossy();
                arg.contains("repos/target/project/pulls/") && !arg.contains('?')
            })
        })
        .count();
    assert_eq!(detail_count, 64);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn sixty_five_candidates_fail_before_any_body_fetch() {
    let rows = (0..65)
        .map(|index| {
            let first = char::from_u32(u32::from(b'k') + (index / 16) as u32).unwrap();
            let second = char::from_u32(u32::from(b'k') + (index % 16) as u32).unwrap();
            let id = format!("{}{}{}", "k".repeat(30), first, second);
            compact((index + 1) as u64, &id)
        })
        .collect::<Vec<_>>();
    let fake = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([]),
        Value::Array(rows),
    ]);
    let root = workspace("candidate-bound");
    assert!(matches!(
        client(&fake, &root).observe(),
        Err(GithubError::Bound {
            query: GithubQuery::PullRequestDetail,
            count: 65,
            max: 64,
        })
    ));
    assert_eq!(fake.calls().len(), 4);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn compact_schema_lifecycle_scope_and_bounds_report_the_compact_query() {
    let mut cases = Vec::new();
    let mut missing = compact(8, CHANGE);
    missing.as_object_mut().unwrap().remove("title");
    cases.push((missing, "malformed"));
    let mut unknown = compact(8, CHANGE);
    unknown["extra"] = json!(true);
    cases.push((unknown, "malformed"));
    let mut empty_title = compact(8, CHANGE);
    empty_title["title"] = json!("");
    cases.push((empty_title, "malformed"));
    let mut nul_title = compact(8, CHANGE);
    nul_title["title"] = json!("title\0suffix");
    cases.push((nul_title, "malformed"));
    let mut oversized_title = compact(8, CHANGE);
    oversized_title["title"] = json!("x".repeat(513));
    cases.push((oversized_title, "title-bound"));
    let mut malformed_head_ref = compact(8, CHANGE);
    malformed_head_ref["head_ref"] = json!("bad ref?");
    cases.push((malformed_head_ref, "malformed"));
    let mut bad_state = compact(8, CHANGE);
    bad_state["state"] = json!("unknown");
    cases.push((bad_state, "malformed"));
    let mut bad_sha = compact(8, CHANGE);
    bad_sha["head_sha"] = json!("short");
    cases.push((bad_sha, "malformed"));
    let mut wrong_base = compact(8, CHANGE);
    wrong_base["base_repository"] = json!("other/project");
    cases.push((wrong_base, "scope"));
    let mut merged_at_long = compact(8, CHANGE);
    merged_at_long["state"] = json!("closed");
    merged_at_long["merged_at"] = json!("x".repeat(65));
    cases.push((merged_at_long, "bound"));

    for (index, (row, expected)) in cases.into_iter().enumerate() {
        let fake = FakeGh::new([
            repository("source/project"),
            repository("target/project"),
            json!([]),
            json!([row]),
        ]);
        let root = workspace(&format!("compact-negative-{index}"));
        let error = client(&fake, &root).observe().unwrap_err();
        match expected {
            "scope" => assert!(matches!(
                error,
                GithubError::ScopeMismatch {
                    query: GithubQuery::PullRequests
                }
            )),
            "bound" => assert!(matches!(
                error,
                GithubError::Bound {
                    query: GithubQuery::PullRequests,
                    count: 65,
                    max: 64,
                }
            )),
            "title-bound" => assert!(matches!(
                error,
                GithubError::Bound {
                    query: GithubQuery::PullRequests,
                    count: 513,
                    max: 512,
                }
            )),
            _ => assert!(matches!(
                error,
                GithubError::Malformed {
                    query: GithubQuery::PullRequests
                }
            )),
        }
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn compact_detail_drift_is_rejected_for_every_authority_field() {
    let mut variants = Vec::new();
    for field in [
        "number",
        "state",
        "head_ref",
        "head_sha",
        "head_repository",
        "base_ref",
        "base_repository",
        "title",
    ] {
        let mut changed = detail(8, CHANGE, "main", "open");
        match field {
            "number" => changed["number"] = json!(9),
            "state" => changed["state"] = json!("closed"),
            "head_ref" => changed["head_ref"] = json!(format!("almighty-push/{NEXT}")),
            "head_sha" => changed["head_sha"] = json!("2222222222222222222222222222222222222222"),
            "head_repository" => changed["head_repository"] = json!("other/project"),
            "base_ref" => changed["base_ref"] = json!("next"),
            "base_repository" => changed["base_repository"] = json!("target/other"),
            "title" => changed["title"] = json!("changed title"),
            _ => unreachable!(),
        }
        variants.push((field, changed));
    }
    for (field, changed) in variants {
        let fake = FakeGh::new([
            repository("source/project"),
            repository("target/project"),
            json!([]),
            json!([compact(8, CHANGE)]),
            changed,
        ]);
        let root = workspace(&format!("detail-drift-{field}"));
        let error = client(&fake, &root).observe().unwrap_err();
        assert!(
            matches!(
                &error,
                GithubError::ObservationChanged { number } if *number == PrNumber::new(8).unwrap()
            ) || matches!(
                &error,
                GithubError::ScopeMismatch {
                    query: GithubQuery::PullRequestDetail
                }
            )
        );
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn malformed_scope_lifecycle_title_body_and_schema_details_fail_precisely() {
    let mut cases = Vec::new();
    let mut null_head = detail(8, CHANGE, "main", "open");
    null_head["head_repository"] = Value::Null;
    cases.push((null_head, "malformed"));
    let mut wrong_base = detail(8, CHANGE, "main", "open");
    wrong_base["base_repository"] = json!("other/project");
    cases.push((wrong_base, "scope"));
    let mut bad_sha = detail(8, CHANGE, "main", "open");
    bad_sha["head_sha"] = json!("short");
    cases.push((bad_sha, "malformed"));
    let mut bad_ref = detail(8, CHANGE, "main", "open");
    bad_ref["head_ref"] = json!("bad ref?");
    cases.push((bad_ref, "malformed"));
    let mut empty_title = detail(8, CHANGE, "main", "open");
    empty_title["title"] = json!("");
    cases.push((empty_title, "malformed"));
    let mut unknown_state = detail(8, CHANGE, "main", "open");
    unknown_state["state"] = json!("unknown");
    cases.push((unknown_state, "malformed"));
    let mut merged_open = detail(8, CHANGE, "main", "open");
    merged_open["merged_at"] = json!("2026-01-01T00:00:00Z");
    cases.push((merged_open, "malformed"));
    let mut nul_body = detail(8, CHANGE, "main", "open");
    nul_body["body"] = json!("nul\0body");
    cases.push((nul_body, "malformed"));
    let mut unknown_field = detail(8, CHANGE, "main", "open");
    unknown_field["extra"] = json!(true);
    cases.push((unknown_field, "malformed"));

    for (index, (response, expected)) in cases.into_iter().enumerate() {
        let fake = FakeGh::new([response]);
        let root = workspace(&format!("detail-negative-{index}"));
        let error = client(&fake, &root)
            .observe_pr(PrNumber::new(8).unwrap())
            .unwrap_err();
        match expected {
            "scope" => assert!(matches!(
                error,
                GithubError::ScopeMismatch {
                    query: GithubQuery::PullRequestDetail
                }
            )),
            _ => assert!(matches!(
                error,
                GithubError::Malformed {
                    query: GithubQuery::PullRequestDetail
                }
            )),
        }
        fs::remove_dir_all(root).unwrap();
    }

    let mut oversized_body = detail(8, CHANGE, "main", "open");
    oversized_body["body"] = json!("x".repeat(limits().body_bytes_max() + 1));
    let fake = FakeGh::new([oversized_body]);
    let root = workspace("detail-body-bound");
    assert!(matches!(
        client(&fake, &root).observe_pr(PrNumber::new(8).unwrap()),
        Err(GithubError::Bound {
            query: GithubQuery::PullRequestDetail,
            count,
            max,
        }) if count == max + 1 && max == limits().body_bytes_max()
    ));
    fs::remove_dir_all(root).unwrap();

    let mut oversized_title = detail(8, CHANGE, "main", "open");
    oversized_title["title"] = json!("x".repeat(513));
    let fake = FakeGh::new([oversized_title]);
    let root = workspace("detail-title-bound");
    assert!(matches!(
        client(&fake, &root).observe_pr(PrNumber::new(8).unwrap()),
        Err(GithubError::Bound {
            query: GithubQuery::PullRequestDetail,
            count: 513,
            max: 512,
        })
    ));
    fs::remove_dir_all(root).unwrap();

    let mut oversized_merged_at = detail(8, CHANGE, "main", "closed");
    oversized_merged_at["merged_at"] = json!("x".repeat(65));
    let fake = FakeGh::new([oversized_merged_at]);
    let root = workspace("detail-merged-at-bound");
    assert!(matches!(
        client(&fake, &root).observe_pr(PrNumber::new(8).unwrap()),
        Err(GithubError::Bound {
            query: GithubQuery::PullRequestDetail,
            count: 65,
            max: 64,
        })
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn compact_scope_and_managed_identity_mismatches_are_not_ignored() {
    let mut wrong_source = compact(8, CHANGE);
    wrong_source["head_repository"] = json!("other/project");
    let fake = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([]),
        json!([wrong_source]),
    ]);
    let root = workspace("compact-wrong-source");
    assert!(matches!(
        client(&fake, &root).observe(),
        Err(GithubError::ScopeMismatch {
            query: GithubQuery::PullRequests
        })
    ));
    fs::remove_dir_all(root).unwrap();

    let mut null_source = compact(8, CHANGE);
    null_source["head_repository"] = Value::Null;
    let fake = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([]),
        json!([null_source]),
    ]);
    let root = workspace("compact-null-source");
    assert!(matches!(
        client(&fake, &root).observe(),
        Err(GithubError::Malformed {
            query: GithubQuery::PullRequests
        })
    ));
    fs::remove_dir_all(root).unwrap();

    let wrong_section = ManagedSection::new(
        RepositoryId::parse("github.com/other/project").unwrap(),
        ChangeId::parse(CHANGE).unwrap(),
        vec![ChangeId::parse(CHANGE).unwrap()].into_boxed_slice(),
        limits(),
    )
    .unwrap();
    let almighty_push::body::BodyMerge::Changed(wrong_body) =
        ManagedBody::merge("user bytes", &wrong_section, limits().body_bytes_max()).unwrap()
    else {
        unreachable!()
    };
    let mut wrong_detail = detail(8, CHANGE, "main", "open");
    wrong_detail["body"] = json!(wrong_body);
    let fake = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([]),
        json!([compact(8, CHANGE)]),
        wrong_detail,
    ]);
    let root = workspace("managed-wrong-source");
    assert!(matches!(
        client(&fake, &root).observe(),
        Err(GithubError::OwnershipAmbiguous)
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ref_namespace_object_sha_duplicate_and_count_rules_fail_closed() {
    let exact_maximum = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        Value::Array(
            (0..limits().change_count_max())
                .map(|index| owned_ref(&change_at(index)))
                .collect(),
        ),
        json!([]),
    ]);
    let root = workspace("ref-count-exact-maximum");
    let snapshot = client(&exact_maximum, &root).observe().unwrap();
    assert_eq!(snapshot.owned_heads().len(), limits().change_count_max());
    assert_eq!(exact_maximum.calls().len(), 4);
    fs::remove_dir_all(root).unwrap();

    let malformed_rows = [
        json!({"ref":format!("refs/tags/almighty-push/{CHANGE}"),"object":{"type":"commit","sha":COMMIT}}),
        json!({"ref":"refs/heads/almighty-push/short","object":{"type":"commit","sha":COMMIT}}),
        json!({"ref":format!("refs/heads/almighty-push/{CHANGE}"),"object":{"type":"tree","sha":COMMIT}}),
        json!({"ref":format!("refs/heads/almighty-push/{CHANGE}"),"object":{"type":"commit","sha":"short"}}),
        json!({"ref":format!("refs/heads/almighty-push-extra/{CHANGE}"),"object":{"type":"commit","sha":COMMIT}}),
    ];
    for (index, row) in malformed_rows.into_iter().enumerate() {
        let fake = FakeGh::new([
            repository("source/project"),
            repository("target/project"),
            json!([row]),
        ]);
        let root = workspace(&format!("ref-malformed-{index}"));
        assert!(matches!(
            client(&fake, &root).observe(),
            Err(GithubError::Malformed {
                query: GithubQuery::OwnedRefs
            })
        ));
        fs::remove_dir_all(root).unwrap();
    }

    let fake = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([owned_ref(CHANGE), owned_ref(CHANGE)]),
    ]);
    let root = workspace("ref-duplicate");
    assert!(matches!(
        client(&fake, &root).observe(),
        Err(GithubError::Duplicate {
            query: GithubQuery::OwnedRefs
        })
    ));
    fs::remove_dir_all(root).unwrap();

    let oversized = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        Value::Array((0..101).map(|_| owned_ref(CHANGE)).collect()),
    ]);
    let root = workspace("ref-page-oversized");
    assert!(matches!(
        client(&oversized, &root).observe(),
        Err(GithubError::Bound {
            query: GithubQuery::OwnedRefs,
            count: 101,
            max: 100,
        })
    ));
    fs::remove_dir_all(root).unwrap();

    let fake = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        Value::Array((0..65).map(|index| owned_ref(&change_at(index))).collect()),
    ]);
    let root = workspace("ref-count-bound");
    assert!(matches!(
        client(&fake, &root).observe(),
        Err(GithubError::Bound {
            query: GithubQuery::OwnedRefs,
            count: 65,
            max: 64,
        })
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn snapshot_order_is_independent_of_compact_page_order() {
    let rows = [compact(2, NEXT), compact(1, CHANGE)];
    let first = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([]),
        Value::Array(rows.to_vec()),
        detail(1, CHANGE, "main", "open"),
        detail(2, NEXT, "main", "open"),
    ]);
    let second = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([]),
        Value::Array(rows.into_iter().rev().collect()),
        detail(1, CHANGE, "main", "open"),
        detail(2, NEXT, "main", "open"),
    ]);
    let first_root = workspace("order-first");
    let second_root = workspace("order-second");
    assert_eq!(
        client(&first, &first_root).observe().unwrap(),
        client(&second, &second_root).observe().unwrap()
    );
    fs::remove_dir_all(first_root).unwrap();
    fs::remove_dir_all(second_root).unwrap();
}

#[test]
fn malformed_nonzero_and_duplicate_observations_never_mean_absent() {
    let failed = FakeGh::with_raw([Err(CommandError::Exit {
        status_code: Some(23),
        stdout: String::new(),
        stderr: "failed".to_owned(),
    })]);
    let root = workspace("nonzero");
    assert!(matches!(
        client(&failed, &root).observe(),
        Err(GithubError::Command(_))
    ));
    fs::remove_dir_all(root).unwrap();

    let malformed = FakeGh::with_raw([Ok("not-json".to_owned())]);
    let root = workspace("malformed");
    assert!(matches!(
        client(&malformed, &root).observe(),
        Err(GithubError::Malformed { .. })
    ));
    fs::remove_dir_all(root).unwrap();

    let duplicate = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([]),
        json!([compact(8, CHANGE), compact(8, NEXT)]),
    ]);
    let root = workspace("duplicate");
    assert!(matches!(
        client(&duplicate, &root).observe(),
        Err(GithubError::Duplicate {
            query: GithubQuery::PullRequests
        })
    ));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn page_command_and_json_failures_never_become_empty_ref_or_pr_observations() {
    let ref_failures = [
        Err(CommandError::Timeout {
            timeout: std::time::Duration::from_millis(100),
        }),
        Err(CommandError::OutputLimit { limit_bytes: 64 }),
        Err(CommandError::Utf8 {
            stream: almighty_push::command::OutputStream::Stdout,
            source: invalid_utf8_error(),
        }),
        Ok("{".to_owned()),
        Ok("{}".to_owned()),
    ];
    for (index, failure) in ref_failures.into_iter().enumerate() {
        let fake = FakeGh::with_raw([
            Ok(repository("source/project").to_string()),
            Ok(repository("target/project").to_string()),
            failure,
        ]);
        let root = workspace(&format!("ref-failure-{index}"));
        assert!(client(&fake, &root).observe().is_err());
        assert_eq!(fake.calls().len(), 3);
        fs::remove_dir_all(root).unwrap();
    }

    let pr_failures = [
        Err(CommandError::Timeout {
            timeout: std::time::Duration::from_millis(100),
        }),
        Err(CommandError::OutputLimit { limit_bytes: 64 }),
        Err(CommandError::Utf8 {
            stream: almighty_push::command::OutputStream::Stdout,
            source: invalid_utf8_error(),
        }),
    ];
    for (index, failure) in pr_failures.into_iter().enumerate() {
        let fake = FakeGh::with_raw([
            Ok(repository("source/project").to_string()),
            Ok(repository("target/project").to_string()),
            Ok("[]".to_owned()),
            failure,
        ]);
        let root = workspace(&format!("pr-command-failure-{index}"));
        assert!(matches!(
            client(&fake, &root).observe(),
            Err(GithubError::Command(_))
        ));
        assert_eq!(fake.calls().len(), 4);
        assert!(fake.calls()[3]
            .args
            .iter()
            .any(|argument| argument.to_string_lossy().contains("/pulls?")));
        fs::remove_dir_all(root).unwrap();
    }

    for (index, response) in ["{".to_owned(), "{}".to_owned()].into_iter().enumerate() {
        let fake = FakeGh::with_raw([
            Ok(repository("source/project").to_string()),
            Ok(repository("target/project").to_string()),
            Ok("[]".to_owned()),
            Ok(response),
        ]);
        let root = workspace(&format!("pr-json-failure-{index}"));
        assert!(matches!(
            client(&fake, &root).observe(),
            Err(GithubError::Malformed {
                query: GithubQuery::PullRequests
            })
        ));
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn mutations_use_json_stdin_and_keep_source_and_target_scopes_separate() {
    let change_id = ChangeId::parse(CHANGE).unwrap();
    let head = HeadRef::owned(&change_id).unwrap();
    let section = managed_section(CHANGE).render();
    let create = Effect::Github(GithubEffect::CreatePullRequest {
        change_id: change_id.clone(),
        head,
        base: HeadRef::parse("main").unwrap(),
        title: "title with ' quotes $ and λ".to_owned(),
        managed_section: section,
    });
    let model = MutationModelGh::with_head(MutationOutcome::SuccessApply);
    execute_model("create-json", create, &model).unwrap();
    let calls = model.calls();
    let mutation = calls
        .iter()
        .find(|call| call.args.iter().any(|argument| argument == "POST"))
        .unwrap();
    let args = mutation
        .args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    assert!(args.iter().any(|arg| arg.as_ref() == "POST"));
    assert!(args
        .iter()
        .any(|arg| arg.contains("repos/target/project/pulls")));
    assert!(!args.iter().any(|arg| arg.contains("title with")));
    let payload: Value = serde_json::from_slice(&mutation.stdin).unwrap();
    assert_eq!(payload["head"], format!("source:almighty-push/{CHANGE}"));
    assert_eq!(payload["title"], "title with ' quotes $ and λ");

    let delete = Effect::Jj(JjEffect::DeleteHead {
        ownership: PrOwnership::generated(change_id).unwrap(),
        expected: CommitId::parse(COMMIT).unwrap(),
    });
    let model = MutationModelGh::with_head(MutationOutcome::SuccessApply);
    execute_model("delete-json", delete, &model).unwrap();
    let mutation = model
        .calls()
        .into_iter()
        .find(|call| call.args.iter().any(|argument| argument == "DELETE"))
        .unwrap();
    let args = mutation
        .args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    assert!(args.iter().any(|arg| arg.as_ref() == "DELETE"));
    assert!(args
        .iter()
        .any(|arg| arg.contains("repos/source/project/git/refs")));
    assert_eq!(
        serde_json::from_slice::<Value>(&mutation.stdin).unwrap(),
        json!({})
    );
}

#[test]
fn lost_create_response_is_recovered_by_exact_postcondition_without_replay() {
    let root = workspace("lost-create");
    let config = resolved(&root);
    let change_id = ChangeId::parse(CHANGE).unwrap();
    let head = HeadRef::owned(&change_id).unwrap();
    let effect = Effect::Github(GithubEffect::CreatePullRequest {
        change_id: change_id.clone(),
        head,
        base: HeadRef::parse("main").unwrap(),
        title: "lost response".to_owned(),
        managed_section: managed_section(CHANGE).render(),
    });
    let plan = Plan::new(scope(), vec![effect].into_boxed_slice(), limits()).unwrap();
    let store = StateStore::open(&root, scope(), limits()).unwrap();
    let model = CreateModelGh::default();
    let mut driver = GithubClient::new(
        &model,
        PathBuf::from("/fake/gh"),
        &config,
        &StateV3::empty(config.scope().clone()),
        Box::new([]),
    );
    assert_eq!(
        Executor::execute_stage(&store, plan.clone(), &mut driver).unwrap(),
        StageOutcome::Complete
    );
    assert_eq!(model.mutation_count(), 1);
    assert_eq!(
        Executor::execute_stage(&store, plan, &mut driver).unwrap(),
        StageOutcome::Complete
    );
    assert_eq!(model.mutation_count(), 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn full_driver_routes_the_canonical_create_and_ownership_sequence() {
    let root = workspace("full-driver-create");
    let config = resolved(&root);
    let change_id = ChangeId::parse(CHANGE).unwrap();
    let create = Effect::Github(GithubEffect::CreatePullRequest {
        change_id: change_id.clone(),
        head: HeadRef::owned(&change_id).unwrap(),
        base: HeadRef::parse("main").unwrap(),
        title: "full driver".to_owned(),
        managed_section: managed_section(CHANGE).render(),
    });
    let install = Effect::Ownership(almighty_push::plan::OwnershipEffect::InstallCreated {
        change_id: change_id.clone(),
        expected_absent: true,
        create_effect_index: 0,
        lifecycle: PrLifecycle::Open,
    });
    let plan = Plan::new(scope(), vec![create, install].into_boxed_slice(), limits()).unwrap();
    let store = StateStore::open(&root, scope(), limits()).unwrap();
    let model = CreateModelGh::default();
    let state = store.load().unwrap();
    let jj = JjClient::new(&model, PathBuf::from("/fake/jj"), &config, Box::new([]));
    let github = GithubClient::new(
        &model,
        PathBuf::from("/fake/gh"),
        &config,
        &state,
        Box::new([]),
    );
    let mut driver = FullEffectDriver::new(jj, github);

    assert_eq!(
        Executor::execute_stage(&store, plan, &mut driver).unwrap(),
        StageOutcome::Complete
    );
    let state = store.load().unwrap();
    assert_eq!(
        state.verified().get(&change_id).unwrap().number(),
        PrNumber::new(8).unwrap()
    );
    assert_eq!(model.mutation_count(), 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn create_adapter_covers_already_success_stale_wrong_failure_and_exact_json_scope() {
    let effect = || {
        Effect::Github(GithubEffect::CreatePullRequest {
            change_id: ChangeId::parse(CHANGE).unwrap(),
            head: HeadRef::parse(format!("almighty-push/{CHANGE}")).unwrap(),
            base: HeadRef::parse("main").unwrap(),
            title: "title ' $ λ".to_owned(),
            managed_section: managed_section(CHANGE).render(),
        })
    };
    let BodyMerge::Changed(body) =
        ManagedBody::merge("", &managed_section(CHANGE), limits().body_bytes_max()).unwrap()
    else {
        unreachable!()
    };
    let desired_pr = json!({
        "number":8,"state":"open","merged_at":null,"title":"title ' $ λ",
        "head_ref":format!("almighty-push/{CHANGE}"),"head_sha":COMMIT,
        "head_repository":"source/project","base_ref":"main",
        "base_repository":"target/project","body":body
    });

    let already =
        MutationModelGh::with_pr(desired_pr.clone(), MutationOutcome::FailureWithoutApply);
    execute_model("create-already", effect(), &already).unwrap();
    assert_eq!(already.mutation_count(), 0);

    let success = MutationModelGh::with_head(MutationOutcome::SuccessApply);
    execute_model("create-success", effect(), &success).unwrap();
    let mutation = success
        .calls()
        .into_iter()
        .find(|call| call.args.iter().any(|arg| arg == "POST"))
        .unwrap();
    assert!(mutation
        .args
        .iter()
        .any(|arg| arg == "repos/target/project/pulls"));
    let payload: Value = serde_json::from_slice(&mutation.stdin).unwrap();
    assert_eq!(payload["head"], format!("source:almighty-push/{CHANGE}"));
    assert_eq!(payload["body"], desired_pr["body"]);
    assert!(!mutation
        .args
        .iter()
        .any(|arg| arg.to_string_lossy().contains("title '")));

    let mut stale_pr = desired_pr.clone();
    stale_pr["base_ref"] = json!("other");
    let stale = MutationModelGh::with_pr(stale_pr, MutationOutcome::SuccessApply);
    assert!(matches!(
        execute_model("create-stale", effect(), &stale),
        Err(ExecutorError::Drift { effect_index: 0 })
    ));
    assert_eq!(stale.mutation_count(), 0);
    for (label, merged_at) in [
        ("closed", Value::Null),
        ("merged", json!("2026-01-01T00:00:00Z")),
    ] {
        let mut lifecycle_conflict = desired_pr.clone();
        lifecycle_conflict["state"] = json!("closed");
        lifecycle_conflict["merged_at"] = merged_at;
        let conflict = MutationModelGh::with_pr(lifecycle_conflict, MutationOutcome::SuccessApply);
        assert!(matches!(
            execute_model(&format!("create-{label}-conflict"), effect(), &conflict),
            Err(ExecutorError::Drift { effect_index: 0 })
        ));
        assert_eq!(conflict.mutation_count(), 0);
    }

    let duplicate = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([owned_ref(CHANGE)]),
        json!([compact(8, CHANGE), compact(9, CHANGE)]),
    ]);
    let root = workspace("create-duplicate-head");
    let driver = client(&duplicate, &root);
    assert!(matches!(
        driver.observe_effect_postcondition(&effect()),
        Err(GithubError::Duplicate {
            query: GithubQuery::PullRequests
        })
    ));
    assert!(!duplicate
        .calls()
        .iter()
        .any(|call| call.args.iter().any(|arg| arg == "POST")));
    fs::remove_dir_all(root).unwrap();

    let wrong = MutationModelGh::with_head(MutationOutcome::SuccessWithoutApply);
    assert!(matches!(
        execute_model("create-wrong-post", effect(), &wrong),
        Err(ExecutorError::Postcondition { effect_index: 0 })
    ));
    let wrong_source = MutationModelGh::with_head(MutationOutcome::SuccessApplyWrongSource);
    assert!(matches!(
        execute_model("create-wrong-source-post", effect(), &wrong_source),
        Err(ExecutorError::ObservationAfterExecution(
            GithubError::ScopeMismatch {
                query: GithubQuery::PullRequests
            }
        ))
    ));
    assert_eq!(wrong_source.mutation_count(), 1);
    let wrong_head = MutationModelGh::with_head(MutationOutcome::SuccessApplyWrongHead);
    assert!(matches!(
        execute_model("create-wrong-head-post", effect(), &wrong_head),
        Err(ExecutorError::Postcondition { effect_index: 0 })
    ));
    assert_eq!(wrong_head.mutation_count(), 1);
    let failed = MutationModelGh::with_head(MutationOutcome::FailureWithoutApply);
    assert!(matches!(
        execute_model("create-command-failure", effect(), &failed),
        Err(ExecutorError::Driver(GithubError::Command(_)))
    ));
    let timeout = MutationModelGh::with_head(MutationOutcome::TimeoutWithoutApply);
    assert!(matches!(
        execute_model("create-timeout", effect(), &timeout),
        Err(ExecutorError::Driver(GithubError::Command(_)))
    ));
    let malformed = MutationModelGh::with_head(MutationOutcome::MalformedWithoutApply);
    assert!(matches!(
        execute_model("create-malformed-response", effect(), &malformed),
        Err(ExecutorError::Driver(GithubError::Malformed {
            query: GithubQuery::MutationResponse
        }))
    ));
    let malformed_lost = MutationModelGh::with_head(MutationOutcome::MalformedAfterApply);
    execute_model("create-malformed-lost", effect(), &malformed_lost).unwrap();
    assert_eq!(malformed_lost.mutation_count(), 1);
}

#[test]
fn body_base_and_lifecycle_preconditions_require_current_exact_ownership() {
    let change_id = ChangeId::parse(CHANGE).unwrap();
    let current = managed_body(CHANGE);
    let new_section = ManagedSection::new(
        scope().source_repository().clone(),
        change_id.clone(),
        vec![change_id.clone(), ChangeId::parse(NEXT).unwrap()].into_boxed_slice(),
        limits(),
    )
    .unwrap();
    let desired =
        match ManagedBody::merge(&current, &new_section, limits().body_bytes_max()).unwrap() {
            almighty_push::body::BodyMerge::Changed(body) => body,
            almighty_push::body::BodyMerge::Unchanged => panic!("section changed"),
        };
    let update = Effect::Github(GithubEffect::UpdateBody {
        ownership: PrOwnership::generated(change_id).unwrap(),
        number: PrNumber::new(8).unwrap(),
        expected_hash: BodyHash::of(current.as_bytes()),
        desired_hash: BodyHash::of(desired.as_bytes()),
        managed_section: new_section.render(),
    });
    let fake = FakeGh::new([detail(8, CHANGE, "main", "open")]);
    let root = workspace("body-precondition");
    let driver = client(&fake, &root);
    assert!(driver.observe_effect_precondition(&update).unwrap());
    fs::remove_dir_all(root).unwrap();

    let attacker = json!({
        "number": 8,
        "state": "open",
        "merged_at": null,
        "title": format!("change {CHANGE}"),
        "head_ref": format!("almighty-push/{CHANGE}"),
        "head_sha": COMMIT,
        "head_repository": "attacker/project",
        "base_ref": "main",
        "base_repository": "target/project",
        "body": managed_body(CHANGE),
    });
    let fake = FakeGh::new([attacker]);
    let root = workspace("attacker-precondition");
    let driver = client(&fake, &root);
    assert!(matches!(
        driver.observe_effect_precondition(&update),
        Err(GithubError::OwnershipMissing)
    ));
    fs::remove_dir_all(root).unwrap();

    let base = Effect::Github(GithubEffect::UpdateBase {
        ownership: ownership(CHANGE),
        number: PrNumber::new(8).unwrap(),
        expected: HeadRef::parse("main").unwrap(),
        desired: HeadRef::parse("next").unwrap(),
    });
    let lifecycle = Effect::Github(GithubEffect::SetLifecycle {
        ownership: ownership(CHANGE),
        number: PrNumber::new(8).unwrap(),
        expected: PrLifecycle::Open,
        desired: PrLifecycle::Closed,
    });
    for (label, effect) in [("base", base), ("lifecycle", lifecycle)] {
        let exact = FakeGh::new([detail(8, CHANGE, "main", "open")]);
        let root = workspace(&format!("{label}-exact-ownership"));
        assert!(client(&exact, &root)
            .observe_effect_precondition(&effect)
            .unwrap());
        fs::remove_dir_all(root).unwrap();

        let attacker = FakeGh::new([json!({
            "number":8,"state":"open","merged_at":null,"title":format!("change {CHANGE}"),
            "head_ref":format!("almighty-push/{CHANGE}"),"head_sha":COMMIT,
            "head_repository":"attacker/project","base_ref":"main",
            "base_repository":"target/project","body":managed_body(CHANGE)
        })]);
        let root = workspace(&format!("{label}-attacker-ownership"));
        assert!(matches!(
            client(&attacker, &root).observe_effect_precondition(&effect),
            Err(GithubError::OwnershipMissing)
        ));
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn update_base_adapter_covers_noop_success_stale_wrong_failure_lost_retry_and_json_scope() {
    let effect = || {
        Effect::Github(GithubEffect::UpdateBase {
            ownership: ownership(CHANGE),
            number: PrNumber::new(8).unwrap(),
            expected: HeadRef::parse("main").unwrap(),
            desired: HeadRef::parse("next").unwrap(),
        })
    };

    let already = MutationModelGh::with_pr(
        detail(8, CHANGE, "next", "open"),
        MutationOutcome::FailureWithoutApply,
    );
    assert_eq!(
        execute_model("base-already", effect(), &already).unwrap(),
        StageOutcome::Complete
    );
    assert_eq!(already.mutation_count(), 0);

    let success = MutationModelGh::with_pr(
        detail(8, CHANGE, "main", "open"),
        MutationOutcome::SuccessApply,
    );
    execute_model("base-success", effect(), &success).unwrap();
    assert_eq!(success.mutation_count(), 1);
    let mutation = success
        .calls()
        .into_iter()
        .find(|call| call.args.iter().any(|arg| arg == "PATCH"))
        .unwrap();
    assert!(mutation
        .args
        .iter()
        .any(|arg| arg == "repos/target/project/pulls/8"));
    assert_eq!(
        serde_json::from_slice::<Value>(&mutation.stdin).unwrap(),
        json!({"base":"next"})
    );

    let stale = MutationModelGh::with_pr(
        detail(8, CHANGE, "other", "open"),
        MutationOutcome::SuccessApply,
    );
    assert!(matches!(
        execute_model("base-stale", effect(), &stale),
        Err(ExecutorError::Drift { effect_index: 0 })
    ));
    assert_eq!(stale.mutation_count(), 0);

    let wrong = MutationModelGh::with_pr(
        detail(8, CHANGE, "main", "open"),
        MutationOutcome::SuccessWithoutApply,
    );
    assert!(matches!(
        execute_model("base-wrong-post", effect(), &wrong),
        Err(ExecutorError::Postcondition { effect_index: 0 })
    ));

    let failed = MutationModelGh::with_pr(
        detail(8, CHANGE, "main", "open"),
        MutationOutcome::FailureWithoutApply,
    );
    assert!(matches!(
        execute_model("base-command-failure", effect(), &failed),
        Err(ExecutorError::Driver(GithubError::Command(_)))
    ));
    let timeout = MutationModelGh::with_pr(
        detail(8, CHANGE, "main", "open"),
        MutationOutcome::TimeoutWithoutApply,
    );
    assert!(matches!(
        execute_model("base-timeout", effect(), &timeout),
        Err(ExecutorError::Driver(GithubError::Command(_)))
    ));

    let malformed = MutationModelGh::with_pr(
        detail(8, CHANGE, "main", "open"),
        MutationOutcome::MalformedWithoutApply,
    );
    assert!(matches!(
        execute_model("base-malformed-response", effect(), &malformed),
        Err(ExecutorError::Driver(GithubError::Malformed {
            query: GithubQuery::MutationResponse
        }))
    ));
    let lost = MutationModelGh::with_pr(
        detail(8, CHANGE, "main", "open"),
        MutationOutcome::FailureAfterApply,
    );
    execute_model("base-lost", effect(), &lost).unwrap();
    assert_eq!(lost.mutation_count(), 1);
    let malformed_lost = MutationModelGh::with_pr(
        detail(8, CHANGE, "main", "open"),
        MutationOutcome::MalformedAfterApply,
    );
    execute_model("base-malformed-lost", effect(), &malformed_lost).unwrap();
    assert_eq!(malformed_lost.mutation_count(), 1);

    let dual = MutationModelGh::with_pr(
        detail(8, CHANGE, "main", "open"),
        MutationOutcome::FailureAfterApplyAndObservationFailure,
    );
    let root = workspace("base-dual-error");
    let store = StateStore::open(&root, scope(), limits()).unwrap();
    let plan = Plan::new(scope(), vec![effect()].into_boxed_slice(), limits()).unwrap();
    assert!(matches!(
        execute_persisted(&root, &store, &plan, &dual),
        Err(ExecutorError::DriverAfterExecution {
            execution: GithubError::Command(_),
            observation: GithubError::Command(_),
        })
    ));
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Executing(checkpoint)
            if checkpoint.next_effect_index() == 0 && checkpoint.results() == [None]
    ));
    fs::remove_dir_all(root).unwrap();
}

fn body_update_effect(expected_body: &str, desired_body: &str, section: &ManagedSection) -> Effect {
    Effect::Github(GithubEffect::UpdateBody {
        ownership: ownership(CHANGE),
        number: PrNumber::new(8).unwrap(),
        expected_hash: BodyHash::of(expected_body.as_bytes()),
        desired_hash: BodyHash::of(desired_body.as_bytes()),
        managed_section: section.render(),
    })
}

#[test]
fn update_body_adapter_preserves_bytes_and_covers_every_retry_boundary() {
    let BodyMerge::Changed(current) = ManagedBody::merge(
        "user '$ bytes\nλ🙂",
        &managed_section(CHANGE),
        limits().body_bytes_max(),
    )
    .unwrap() else {
        unreachable!()
    };
    let section = ManagedSection::new(
        scope().source_repository().clone(),
        ChangeId::parse(CHANGE).unwrap(),
        vec![
            ChangeId::parse(CHANGE).unwrap(),
            ChangeId::parse(NEXT).unwrap(),
        ]
        .into_boxed_slice(),
        limits(),
    )
    .unwrap();
    let BodyMerge::Changed(desired) =
        ManagedBody::merge(&current, &section, limits().body_bytes_max()).unwrap()
    else {
        unreachable!()
    };
    let effect = || body_update_effect(&current, &desired, &section);
    let pr_with_body = |body: &str| {
        let mut pr = detail(8, CHANGE, "main", "open");
        pr["body"] = json!(body);
        pr
    };

    let already =
        MutationModelGh::with_pr(pr_with_body(&desired), MutationOutcome::FailureWithoutApply);
    execute_model("body-already", effect(), &already).unwrap();
    assert_eq!(already.mutation_count(), 0);

    let success = MutationModelGh::with_pr(pr_with_body(&current), MutationOutcome::SuccessApply);
    execute_model("body-success", effect(), &success).unwrap();
    let mutation = success
        .calls()
        .into_iter()
        .find(|call| call.args.iter().any(|arg| arg == "PATCH"))
        .unwrap();
    let payload: Value = serde_json::from_slice(&mutation.stdin).unwrap();
    assert_eq!(payload, json!({"body":desired}));
    assert!(!mutation
        .args
        .iter()
        .any(|arg| arg.to_string_lossy().contains("user bytes")));

    let stale = MutationModelGh::with_pr(
        pr_with_body(&format!("concurrent λ edit\n{current}")),
        MutationOutcome::SuccessApply,
    );
    assert!(matches!(
        execute_model("body-stale", effect(), &stale),
        Err(ExecutorError::Drift { effect_index: 0 })
    ));
    assert_eq!(stale.mutation_count(), 0);

    let wrong =
        MutationModelGh::with_pr(pr_with_body(&current), MutationOutcome::SuccessWithoutApply);
    assert!(matches!(
        execute_model("body-wrong-post", effect(), &wrong),
        Err(ExecutorError::Postcondition { effect_index: 0 })
    ));
    let failed =
        MutationModelGh::with_pr(pr_with_body(&current), MutationOutcome::FailureWithoutApply);
    assert!(matches!(
        execute_model("body-command-failure", effect(), &failed),
        Err(ExecutorError::Driver(GithubError::Command(_)))
    ));
    let timeout =
        MutationModelGh::with_pr(pr_with_body(&current), MutationOutcome::TimeoutWithoutApply);
    assert!(matches!(
        execute_model("body-timeout", effect(), &timeout),
        Err(ExecutorError::Driver(GithubError::Command(_)))
    ));
    let malformed_response = MutationModelGh::with_pr(
        pr_with_body(&current),
        MutationOutcome::MalformedWithoutApply,
    );
    assert!(matches!(
        execute_model("body-malformed-response", effect(), &malformed_response),
        Err(ExecutorError::Driver(GithubError::Malformed {
            query: GithubQuery::MutationResponse
        }))
    ));
    let lost = MutationModelGh::with_pr(pr_with_body(&current), MutationOutcome::FailureAfterApply);
    execute_model("body-lost", effect(), &lost).unwrap();
    assert_eq!(lost.mutation_count(), 1);
    let malformed_lost =
        MutationModelGh::with_pr(pr_with_body(&current), MutationOutcome::MalformedAfterApply);
    execute_model("body-malformed-lost", effect(), &malformed_lost).unwrap();

    let mut malformed = pr_with_body(&current);
    malformed["body"] = json!(format!("{current}\n<!-- almighty-push:stack:v2:start -->"));
    let malformed = MutationModelGh::with_pr(malformed, MutationOutcome::SuccessApply);
    assert!(matches!(
        execute_model("body-malformed", effect(), &malformed),
        Err(ExecutorError::Driver(GithubError::Body(_)))
    ));
    assert_eq!(malformed.mutation_count(), 0);
}

#[test]
fn lifecycle_adapter_covers_close_reopen_merged_wrong_failure_lost_and_retry() {
    for (label, expected, desired, initial, satisfied) in [
        (
            "close",
            PrLifecycle::Open,
            PrLifecycle::Closed,
            "open",
            "closed",
        ),
        (
            "reopen",
            PrLifecycle::Closed,
            PrLifecycle::Open,
            "closed",
            "open",
        ),
    ] {
        let effect = || {
            Effect::Github(GithubEffect::SetLifecycle {
                ownership: ownership(CHANGE),
                number: PrNumber::new(8).unwrap(),
                expected,
                desired,
            })
        };
        let already = MutationModelGh::with_pr(
            detail(8, CHANGE, "main", satisfied),
            MutationOutcome::FailureWithoutApply,
        );
        execute_model(&format!("lifecycle-{label}-already"), effect(), &already).unwrap();
        assert_eq!(already.mutation_count(), 0);

        let success = MutationModelGh::with_pr(
            detail(8, CHANGE, "main", initial),
            MutationOutcome::SuccessApply,
        );
        execute_model(&format!("lifecycle-{label}-success"), effect(), &success).unwrap();
        let mutation = success
            .calls()
            .into_iter()
            .find(|call| call.args.iter().any(|arg| arg == "PATCH"))
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&mutation.stdin).unwrap(),
            json!({"state":satisfied})
        );

        let wrong = MutationModelGh::with_pr(
            detail(8, CHANGE, "main", initial),
            MutationOutcome::SuccessWithoutApply,
        );
        assert!(matches!(
            execute_model(&format!("lifecycle-{label}-wrong"), effect(), &wrong),
            Err(ExecutorError::Postcondition { effect_index: 0 })
        ));
        let failed = MutationModelGh::with_pr(
            detail(8, CHANGE, "main", initial),
            MutationOutcome::FailureWithoutApply,
        );
        assert!(matches!(
            execute_model(&format!("lifecycle-{label}-failed"), effect(), &failed),
            Err(ExecutorError::Driver(GithubError::Command(_)))
        ));
        let timeout = MutationModelGh::with_pr(
            detail(8, CHANGE, "main", initial),
            MutationOutcome::TimeoutWithoutApply,
        );
        assert!(matches!(
            execute_model(&format!("lifecycle-{label}-timeout"), effect(), &timeout),
            Err(ExecutorError::Driver(GithubError::Command(_)))
        ));
        let malformed_response = MutationModelGh::with_pr(
            detail(8, CHANGE, "main", initial),
            MutationOutcome::MalformedWithoutApply,
        );
        assert!(matches!(
            execute_model(
                &format!("lifecycle-{label}-malformed-response"),
                effect(),
                &malformed_response,
            ),
            Err(ExecutorError::Driver(GithubError::Malformed {
                query: GithubQuery::MutationResponse
            }))
        ));
        let lost = MutationModelGh::with_pr(
            detail(8, CHANGE, "main", initial),
            MutationOutcome::FailureAfterApply,
        );
        execute_model(&format!("lifecycle-{label}-lost"), effect(), &lost).unwrap();
        assert_eq!(lost.mutation_count(), 1);
        let malformed_lost = MutationModelGh::with_pr(
            detail(8, CHANGE, "main", initial),
            MutationOutcome::MalformedAfterApply,
        );
        execute_model(
            &format!("lifecycle-{label}-malformed-lost"),
            effect(),
            &malformed_lost,
        )
        .unwrap();
    }

    let mut merged = detail(8, CHANGE, "main", "closed");
    merged["merged_at"] = json!("2026-01-01T00:00:00Z");
    let merged = MutationModelGh::with_pr(merged, MutationOutcome::SuccessApply);
    let close = Effect::Github(GithubEffect::SetLifecycle {
        ownership: ownership(CHANGE),
        number: PrNumber::new(8).unwrap(),
        expected: PrLifecycle::Open,
        desired: PrLifecycle::Closed,
    });
    assert!(matches!(
        execute_model("lifecycle-merged", close, &merged),
        Err(ExecutorError::Drift { effect_index: 0 })
    ));
    assert_eq!(merged.mutation_count(), 0);
}

#[test]
fn delete_adapter_uses_only_source_and_covers_absent_stale_wrong_failure_lost_retry() {
    let effect = || {
        Effect::Jj(JjEffect::DeleteHead {
            ownership: ownership(CHANGE),
            expected: CommitId::parse(COMMIT).unwrap(),
        })
    };
    let already = MutationModelGh::with_head(MutationOutcome::FailureWithoutApply);
    *already.head_present.lock().unwrap() = false;
    execute_model("delete-already", effect(), &already).unwrap();
    assert_eq!(already.mutation_count(), 0);

    let success = MutationModelGh::with_head(MutationOutcome::SuccessApply);
    execute_model("delete-success", effect(), &success).unwrap();
    let mutation = success
        .calls()
        .into_iter()
        .find(|call| call.args.iter().any(|arg| arg == "DELETE"))
        .unwrap();
    assert!(mutation.args.iter().any(|arg| arg
        .to_string_lossy()
        .contains("repos/source/project/git/refs/heads/almighty-push%2F")));
    assert!(!mutation
        .args
        .iter()
        .any(|arg| arg.to_string_lossy().contains("target/project/git/refs")));
    assert_eq!(
        serde_json::from_slice::<Value>(&mutation.stdin).unwrap(),
        json!({})
    );

    for (index, response) in [
        Ok(json!([owned_ref(CHANGE), owned_ref(CHANGE)]).to_string()),
        Ok("[{\"ref\":\"broken\"}]".to_owned()),
        Err(CommandError::Exit {
            status_code: Some(1),
            stdout: String::new(),
            stderr: "failed exact ref read".to_owned(),
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let fake = FakeGh::with_raw([response]);
        let root = workspace(&format!("delete-ref-failure-{index}"));
        let driver = client(&fake, &root);
        assert!(driver.observe_effect_precondition(&effect()).is_err());
        assert_eq!(fake.calls().len(), 1);
        fs::remove_dir_all(root).unwrap();
    }

    let stale_effect = Effect::Jj(JjEffect::DeleteHead {
        ownership: ownership(CHANGE),
        expected: CommitId::parse("2222222222222222222222222222222222222222").unwrap(),
    });
    let stale = MutationModelGh::with_head(MutationOutcome::SuccessApply);
    assert!(matches!(
        execute_model("delete-stale", stale_effect, &stale),
        Err(ExecutorError::Drift { effect_index: 0 })
    ));
    assert_eq!(stale.mutation_count(), 0);

    let wrong = MutationModelGh::with_head(MutationOutcome::SuccessWithoutApply);
    assert!(matches!(
        execute_model("delete-wrong-post", effect(), &wrong),
        Err(ExecutorError::Postcondition { effect_index: 0 })
    ));
    let failed = MutationModelGh::with_head(MutationOutcome::FailureWithoutApply);
    assert!(matches!(
        execute_model("delete-command-failure", effect(), &failed),
        Err(ExecutorError::Driver(GithubError::Command(_)))
    ));
    let timeout = MutationModelGh::with_head(MutationOutcome::TimeoutWithoutApply);
    assert!(matches!(
        execute_model("delete-timeout", effect(), &timeout),
        Err(ExecutorError::Driver(GithubError::Command(_)))
    ));
    let malformed = MutationModelGh::with_head(MutationOutcome::MalformedWithoutApply);
    assert!(matches!(
        execute_model("delete-malformed-response", effect(), &malformed),
        Err(ExecutorError::Driver(GithubError::Malformed {
            query: GithubQuery::MutationResponse
        }))
    ));
    let lost = MutationModelGh::with_head(MutationOutcome::FailureAfterApply);
    execute_model("delete-lost", effect(), &lost).unwrap();
    assert_eq!(lost.mutation_count(), 1);
    let malformed_lost = MutationModelGh::with_head(MutationOutcome::MalformedAfterApply);
    execute_model("delete-malformed-lost", effect(), &malformed_lost).unwrap();
    assert_eq!(malformed_lost.mutation_count(), 1);
}

#[test]
fn base_body_lifecycle_and_delete_resume_the_same_durable_checkpoint() {
    let base = Effect::Github(GithubEffect::UpdateBase {
        ownership: ownership(CHANGE),
        number: PrNumber::new(8).unwrap(),
        expected: HeadRef::parse("main").unwrap(),
        desired: HeadRef::parse("next").unwrap(),
    });
    assert_persisted_retry(
        "base-persisted-retry",
        base,
        MutationModelGh::with_pr(
            detail(8, CHANGE, "main", "open"),
            MutationOutcome::FailureWithoutApply,
        ),
    );

    let current = managed_body(CHANGE);
    let section = ManagedSection::new(
        scope().source_repository().clone(),
        ChangeId::parse(CHANGE).unwrap(),
        vec![
            ChangeId::parse(CHANGE).unwrap(),
            ChangeId::parse(NEXT).unwrap(),
        ]
        .into_boxed_slice(),
        limits(),
    )
    .unwrap();
    let BodyMerge::Changed(desired) =
        ManagedBody::merge(&current, &section, limits().body_bytes_max()).unwrap()
    else {
        unreachable!()
    };
    let body = body_update_effect(&current, &desired, &section);
    let mut body_pr = detail(8, CHANGE, "main", "open");
    body_pr["body"] = json!(current);
    assert_persisted_retry(
        "body-persisted-retry",
        body,
        MutationModelGh::with_pr(body_pr, MutationOutcome::FailureWithoutApply),
    );

    let lifecycle = Effect::Github(GithubEffect::SetLifecycle {
        ownership: ownership(CHANGE),
        number: PrNumber::new(8).unwrap(),
        expected: PrLifecycle::Open,
        desired: PrLifecycle::Closed,
    });
    assert_persisted_retry(
        "lifecycle-persisted-retry",
        lifecycle,
        MutationModelGh::with_pr(
            detail(8, CHANGE, "main", "open"),
            MutationOutcome::FailureWithoutApply,
        ),
    );

    let delete = Effect::Jj(JjEffect::DeleteHead {
        ownership: ownership(CHANGE),
        expected: CommitId::parse(COMMIT).unwrap(),
    });
    assert_persisted_retry(
        "delete-persisted-retry",
        delete,
        MutationModelGh::with_head(MutationOutcome::FailureWithoutApply),
    );
}

#[test]
fn exact_legacy_observation_produces_only_a_sealed_consumable_capability() {
    let root = workspace("legacy-capability");
    fs::write(
        root.join(".almighty"),
        format!(
            r#"{{"version":2,"prs":{{"{CHANGE}":{{"pr_number":8,"pr_url":"https://github.com/target/project/pull/8","branch_name":"legacy-head","commit_id":"{COMMIT}","change_id":"{CHANGE}"}}}},"merged_prs":[],"closed_prs":[],"last_operation_id":null}}"#
        ),
    )
    .unwrap();
    let fake = FakeGh::new([json!({
        "number": 8,
        "state": "open",
        "merged_at": null,
        "title": "legacy",
        "head_ref": "legacy-head",
        "head_sha": COMMIT,
        "head_repository": "source/project",
        "base_ref": "main",
        "base_repository": "target/project",
        "body": format!("user prose\nChange ID: `{CHANGE}`\n"),
    })]);
    let store = StateStore::open(&root, scope(), limits()).unwrap();
    let mut session = store.lock().unwrap();
    let resolution = client(&fake, &root)
        .resolve_legacy_candidate(&session.state().legacy_candidates()[0])
        .unwrap();
    session.resolve_legacy(resolution).unwrap();
    let managed = session.state().verified().values().next().unwrap();
    assert!(managed.ownership().is_validated_legacy());
    let authority = managed.ownership().clone();
    assert!(matches!(
        session.state().legacy_resolutions()[0].disposition(),
        LegacyDisposition::Verified { .. }
    ));
    let verified_effect = Effect::Github(GithubEffect::UpdateBase {
        ownership: authority.clone(),
        number: PrNumber::new(8).unwrap(),
        expected: HeadRef::parse("main").unwrap(),
        desired: HeadRef::parse("next").unwrap(),
    });
    let verified = FakeGh::new([json!({
        "number": 8,
        "state": "open",
        "merged_at": null,
        "title": "legacy",
        "head_ref": "legacy-head",
        "head_sha": COMMIT,
        "head_repository": "source/project",
        "base_ref": "main",
        "base_repository": "target/project",
        "body": format!("Change ID: `{CHANGE}`"),
    })]);
    let driver = client_with_state(&verified, &root, session.state());
    assert!(driver
        .observe_effect_precondition(&verified_effect)
        .unwrap());

    let legacy_body = format!("user bytes\nChange ID: `{CHANGE}`");
    let desired_section = managed_section(CHANGE);
    let BodyMerge::Changed(desired_body) =
        ManagedBody::merge(&legacy_body, &desired_section, limits().body_bytes_max()).unwrap()
    else {
        unreachable!()
    };
    let body_effect = Effect::Github(GithubEffect::UpdateBody {
        ownership: authority.clone(),
        number: PrNumber::new(8).unwrap(),
        expected_hash: BodyHash::of(legacy_body.as_bytes()),
        desired_hash: BodyHash::of(desired_body.as_bytes()),
        managed_section: desired_section.render(),
    });
    let lifecycle_effect = Effect::Github(GithubEffect::SetLifecycle {
        ownership: authority.clone(),
        number: PrNumber::new(8).unwrap(),
        expected: PrLifecycle::Open,
        desired: PrLifecycle::Closed,
    });
    for (label, effect) in [("body", body_effect), ("lifecycle", lifecycle_effect)] {
        let observed = FakeGh::new([json!({
            "number":8,"state":"open","merged_at":null,"title":"legacy",
            "head_ref":"legacy-head","head_sha":COMMIT,"head_repository":"source/project",
            "base_ref":"main","base_repository":"target/project","body":legacy_body.clone()
        })]);
        assert!(
            client_with_state(&observed, &root, session.state())
                .observe_effect_precondition(&effect)
                .unwrap(),
            "{label}"
        );
    }
    let delete_effect = Effect::Jj(JjEffect::DeleteHead {
        ownership: authority.clone(),
        expected: CommitId::parse(COMMIT).unwrap(),
    });
    let observed = FakeGh::new([json!([{
        "ref":"refs/heads/legacy-head",
        "object":{"type":"commit","sha":COMMIT}
    }])]);
    assert!(client_with_state(&observed, &root, session.state())
        .observe_effect_precondition(&delete_effect)
        .unwrap());

    session.finish_legacy_deletion().unwrap();

    let mut forged_value = serde_json::to_value(&authority).unwrap();
    forged_value["head"] = json!("attacker-head");
    let forged_authority: PrOwnership = serde_json::from_value(forged_value).unwrap();
    let forged_effect = Effect::Github(GithubEffect::UpdateBase {
        ownership: forged_authority,
        number: PrNumber::new(8).unwrap(),
        expected: HeadRef::parse("main").unwrap(),
        desired: HeadRef::parse("next").unwrap(),
    });
    let forged_plan = Plan::new(scope(), vec![forged_effect].into_boxed_slice(), limits()).unwrap();
    assert!(matches!(
        session.start_plan(forged_plan),
        Err(StateError::OwnershipConflict)
    ));
    assert!(matches!(
        session.state().checkpoint(),
        CheckpointState::Idle
    ));

    session
        .start_plan(Plan::new(scope(), vec![verified_effect].into_boxed_slice(), limits()).unwrap())
        .unwrap();
    let reloaded = store.load().unwrap();
    let almighty_push::state::CheckpointState::Executing(checkpoint) = reloaded.checkpoint() else {
        panic!("legacy authority plan was not checkpointed")
    };
    assert!(matches!(
        &checkpoint.plan().effects()[0],
        Effect::Github(GithubEffect::UpdateBase { ownership, .. })
            if ownership.is_validated_legacy()
    ));

    let mut forged_checkpoint = serde_json::to_value(&reloaded).unwrap();
    forged_checkpoint["checkpoint"]["Executing"]["plan"]["effects"][0]["Github"]["UpdateBase"]
        ["ownership"]["head"] = json!("attacker-head");
    fs::write(
        store.state_path(),
        serde_json::to_vec(&forged_checkpoint).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        store.load(),
        Err(StateError::Plan(PlanError::IdentityMismatch))
    ));
    drop(session);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn validated_legacy_mutations_execute_checked_json_and_recover_lost_responses() {
    const LEGACY_HEAD: &str = "legacy/topic";
    let legacy_body = format!("user bytes\nChange ID: `{CHANGE}`");

    let (root, store, authority) =
        validated_legacy_fixture("legacy-base-execution", LEGACY_HEAD, &legacy_body);
    let effect = Effect::Github(GithubEffect::UpdateBase {
        ownership: authority,
        number: PrNumber::new(8).unwrap(),
        expected: HeadRef::parse("main").unwrap(),
        desired: HeadRef::parse("next").unwrap(),
    });
    let model = MutationModelGh::with_pr(
        legacy_pr(LEGACY_HEAD, "main", "open", &legacy_body),
        MutationOutcome::FailureAfterApply,
    );
    execute_persisted_plan(&root, &store, effect, &model).unwrap();
    assert_legacy_mutation(
        &model,
        "PATCH",
        "repos/target/project/pulls/8",
        json!({"base":"next"}),
    );
    fs::remove_dir_all(root).unwrap();

    let (root, store, authority) =
        validated_legacy_fixture("legacy-body-execution", LEGACY_HEAD, &legacy_body);
    let section = managed_section(CHANGE);
    let BodyMerge::Changed(desired_body) =
        ManagedBody::merge(&legacy_body, &section, limits().body_bytes_max()).unwrap()
    else {
        unreachable!()
    };
    assert_eq!(
        &desired_body.as_bytes()[..legacy_body.len()],
        legacy_body.as_bytes()
    );
    assert_eq!(
        &desired_body[legacy_body.len()..legacy_body.len() + 2],
        "\n\n"
    );
    let effect = Effect::Github(GithubEffect::UpdateBody {
        ownership: authority,
        number: PrNumber::new(8).unwrap(),
        expected_hash: BodyHash::of(legacy_body.as_bytes()),
        desired_hash: BodyHash::of(desired_body.as_bytes()),
        managed_section: section.render(),
    });
    let model = MutationModelGh::with_pr(
        legacy_pr(LEGACY_HEAD, "main", "open", &legacy_body),
        MutationOutcome::FailureAfterApply,
    );
    execute_persisted_plan(&root, &store, effect, &model).unwrap();
    assert_legacy_mutation(
        &model,
        "PATCH",
        "repos/target/project/pulls/8",
        json!({"body":desired_body.clone()}),
    );
    assert_eq!(
        model.pr.lock().unwrap().as_ref().unwrap()["body"].as_str(),
        Some(desired_body.as_str())
    );
    fs::remove_dir_all(root).unwrap();

    let (root, store, authority) =
        validated_legacy_fixture("legacy-lifecycle-execution", LEGACY_HEAD, &legacy_body);
    let effect = Effect::Github(GithubEffect::SetLifecycle {
        ownership: authority,
        number: PrNumber::new(8).unwrap(),
        expected: PrLifecycle::Open,
        desired: PrLifecycle::Closed,
    });
    let model = MutationModelGh::with_pr(
        legacy_pr(LEGACY_HEAD, "main", "open", &legacy_body),
        MutationOutcome::FailureAfterApply,
    );
    execute_persisted_plan(&root, &store, effect, &model).unwrap();
    assert_legacy_mutation(
        &model,
        "PATCH",
        "repos/target/project/pulls/8",
        json!({"state":"closed"}),
    );
    fs::remove_dir_all(root).unwrap();

    let (root, store, authority) =
        validated_legacy_fixture("legacy-delete-execution", LEGACY_HEAD, &legacy_body);
    let effect = Effect::Jj(JjEffect::DeleteHead {
        ownership: authority,
        expected: CommitId::parse(COMMIT).unwrap(),
    });
    let model = MutationModelGh::with_named_head(
        LEGACY_HEAD.to_owned(),
        MutationOutcome::FailureAfterApply,
    );
    execute_persisted_plan(&root, &store, effect, &model).unwrap();
    assert_legacy_mutation(
        &model,
        "DELETE",
        "repos/source/project/git/refs/heads/legacy%2Ftopic",
        json!({}),
    );
    assert!(!model
        .calls()
        .iter()
        .any(|call| call.args.iter().any(|argument| argument
            .to_string_lossy()
            .contains("target/project/git/refs"))));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn validated_legacy_managed_metadata_must_match_the_sealed_identity() {
    const LEGACY_HEAD: &str = "legacy/topic";
    let legacy_body = format!("exterior bytes\nChange ID: `{CHANGE}`");
    let current = ChangeId::parse(CHANGE).unwrap();
    let next = ChangeId::parse(NEXT).unwrap();
    let wrong_source = ManagedSection::new(
        RepositoryId::parse("github.com/other/project").unwrap(),
        current.clone(),
        vec![current].into_boxed_slice(),
        limits(),
    )
    .unwrap();
    let wrong_current = ManagedSection::new(
        scope().source_repository().clone(),
        next.clone(),
        vec![next].into_boxed_slice(),
        limits(),
    )
    .unwrap();

    for (label, section) in [
        ("wrong-source", wrong_source),
        ("wrong-current", wrong_current),
    ] {
        let BodyMerge::Changed(body) =
            ManagedBody::merge(&legacy_body, &section, limits().body_bytes_max()).unwrap()
        else {
            unreachable!()
        };
        assert!(body.starts_with(&legacy_body));
        let (root, store, authority) = validated_legacy_fixture(
            &format!("legacy-managed-{label}"),
            LEGACY_HEAD,
            &legacy_body,
        );
        let effect = Effect::Github(GithubEffect::UpdateBase {
            ownership: authority,
            number: PrNumber::new(8).unwrap(),
            expected: HeadRef::parse("main").unwrap(),
            desired: HeadRef::parse("next").unwrap(),
        });
        let model = MutationModelGh::with_pr(
            legacy_pr(LEGACY_HEAD, "main", "open", &body),
            MutationOutcome::SuccessApply,
        );
        let state = store.load().unwrap();
        let driver = client_with_state(&model, &root, &state);
        assert!(matches!(
            driver.observe_effect_precondition(&effect),
            Err(GithubError::OwnershipMissing)
        ));
        assert_eq!(model.mutation_count(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn validated_legacy_malformed_managed_markers_never_fall_back_to_prose() {
    const LEGACY_HEAD: &str = "legacy/topic";
    let legacy_body = format!("exterior bytes\nChange ID: `{CHANGE}`");
    let BodyMerge::Changed(canonical_body) = ManagedBody::merge(
        &legacy_body,
        &managed_section(CHANGE),
        limits().body_bytes_max(),
    )
    .unwrap() else {
        unreachable!()
    };
    let malformed_bodies = [
        format!("{legacy_body}\n<!-- almighty-push:stack:v2:start -->"),
        format!("{canonical_body}\n{canonical_body}"),
    ];

    for (index, body) in malformed_bodies.into_iter().enumerate() {
        let (root, store, authority) = validated_legacy_fixture(
            &format!("legacy-malformed-managed-{index}"),
            LEGACY_HEAD,
            &legacy_body,
        );
        let effect = Effect::Github(GithubEffect::UpdateBase {
            ownership: authority,
            number: PrNumber::new(8).unwrap(),
            expected: HeadRef::parse("main").unwrap(),
            desired: HeadRef::parse("next").unwrap(),
        });
        let model = MutationModelGh::with_pr(
            legacy_pr(LEGACY_HEAD, "main", "open", &body),
            MutationOutcome::SuccessApply,
        );
        let state = store.load().unwrap();
        let driver = client_with_state(&model, &root, &state);
        assert!(matches!(
            driver.observe_effect_precondition(&effect),
            Err(GithubError::Body(
                almighty_push::body::BodyError::MalformedMarkers
            ))
        ));
        assert_eq!(model.mutation_count(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn decisive_legacy_mismatches_are_sealed_rejections_and_ambiguity_stays_pending() {
    let cases = [
        (
            "short",
            "https://github.com/target/project/pull/8",
            "legacy-head",
            COMMIT,
            Some("short"),
            json!({
                "number":8,"state":"open","merged_at":null,"title":"legacy",
                "head_ref":"legacy-head","head_sha":COMMIT,"head_repository":"source/project",
                "base_ref":"main","base_repository":"target/project","body":"none"
            }),
            LegacyRejectionReason::ChangeIdentityMismatch,
        ),
        (
            CHANGE,
            "https://github.com/other/project/pull/8",
            "legacy-head",
            COMMIT,
            Some(CHANGE),
            json!({
                "number":8,"state":"open","merged_at":null,"title":"legacy",
                "head_ref":"legacy-head","head_sha":COMMIT,"head_repository":"source/project",
                "base_ref":"main","base_repository":"target/project","body":format!("Change ID: `{CHANGE}`")
            }),
            LegacyRejectionReason::RepositoryMismatch,
        ),
        (
            CHANGE,
            "https://github.com/target/project/pull/8",
            "legacy-head",
            COMMIT,
            Some(CHANGE),
            json!({
                "number":8,"state":"open","merged_at":null,"title":"legacy",
                "head_ref":"legacy-head","head_sha":COMMIT,"head_repository":"other/project",
                "base_ref":"main","base_repository":"target/project","body":format!("Change ID: `{CHANGE}`")
            }),
            LegacyRejectionReason::HeadMismatch,
        ),
        (
            CHANGE,
            "https://github.com/target/project/pull/8",
            "expected-head",
            COMMIT,
            Some(CHANGE),
            json!({
                "number":8,"state":"open","merged_at":null,"title":"legacy",
                "head_ref":"observed-head","head_sha":COMMIT,"head_repository":"source/project",
                "base_ref":"main","base_repository":"target/project","body":format!("Change ID: `{CHANGE}`")
            }),
            LegacyRejectionReason::HeadMismatch,
        ),
        (
            CHANGE,
            "https://github.com/target/project/pull/8",
            "legacy-head",
            "2222222222222222222222222222222222222222",
            Some(CHANGE),
            json!({
                "number":8,"state":"open","merged_at":null,"title":"legacy",
                "head_ref":"legacy-head","head_sha":COMMIT,"head_repository":"source/project",
                "base_ref":"main","base_repository":"target/project","body":format!("Change ID: `{CHANGE}`")
            }),
            LegacyRejectionReason::CommitMismatch,
        ),
        (
            CHANGE,
            "https://github.com/target/project/pull/8",
            "legacy-head",
            COMMIT,
            Some(CHANGE),
            json!({
                "number":8,"state":"open","merged_at":null,"title":"legacy",
                "head_ref":"legacy-head","head_sha":COMMIT,"head_repository":"source/project",
                "base_ref":"main","base_repository":"target/project","body":format!("prose Change ID: `{CHANGE}` suffix")
            }),
            LegacyRejectionReason::OwnershipMarkerMissing,
        ),
    ];
    for (index, (key, url, branch, commit, change, response, expected_reason)) in
        cases.into_iter().enumerate()
    {
        let root = workspace(&format!("legacy-rejection-{index}"));
        write_legacy(&root, key, url, branch, commit, change);
        let store = StateStore::open(&root, scope(), limits()).unwrap();
        let mut session = store.lock().unwrap();
        let fake = FakeGh::new([response]);
        let resolution = client(&fake, &root)
            .resolve_legacy_candidate(&session.state().legacy_candidates()[0])
            .unwrap();
        session.resolve_legacy(resolution).unwrap();
        assert!(matches!(
            session.state().legacy_resolutions()[0].disposition(),
            LegacyDisposition::Rejected { reason, .. } if *reason == expected_reason
        ));
        fs::remove_dir_all(root).unwrap();
    }

    let root = workspace("legacy-ambiguity-pending");
    write_legacy(
        &root,
        CHANGE,
        "https://github.com/target/project/pull/8",
        "legacy-head",
        COMMIT,
        Some(CHANGE),
    );
    let store = StateStore::open(&root, scope(), limits()).unwrap();
    let session = store.lock().unwrap();
    let duplicate = FakeGh::new([json!({
        "number":8,"state":"open","merged_at":null,"title":"legacy",
        "head_ref":"legacy-head","head_sha":COMMIT,"head_repository":"source/project",
        "base_ref":"main","base_repository":"target/project",
        "body":format!("Change ID: `{CHANGE}`\nChange ID: `{CHANGE}`")
    })]);
    assert!(matches!(
        client(&duplicate, &root).resolve_legacy_candidate(&session.state().legacy_candidates()[0]),
        Err(GithubError::LegacyAmbiguous)
    ));
    assert_eq!(session.state().legacy_candidates().len(), 1);
    assert!(session.state().legacy_resolutions().is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_operating_schema_scope_and_body_failures_never_become_rejections() {
    let root = workspace("legacy-errors-pending");
    write_legacy(
        &root,
        CHANGE,
        "https://github.com/target/project/pull/8",
        "legacy-head",
        COMMIT,
        Some(CHANGE),
    );
    let store = StateStore::open(&root, scope(), limits()).unwrap();
    let session = store.lock().unwrap();
    let candidate = &session.state().legacy_candidates()[0];

    let cases = [
        FakeGh::with_raw([Err(CommandError::Exit {
            status_code: Some(1),
            stdout: String::new(),
            stderr: "failed".to_owned(),
        })]),
        FakeGh::with_raw([Ok("{broken".to_owned())]),
        FakeGh::new([json!({
            "number":9,"state":"open","merged_at":null,"title":"legacy",
            "head_ref":"legacy-head","head_sha":COMMIT,"head_repository":"source/project",
            "base_ref":"main","base_repository":"target/project","body":format!("Change ID: `{CHANGE}`")
        })]),
        FakeGh::new([json!({
            "number":8,"state":"open","merged_at":null,"title":"legacy",
            "head_ref":"legacy-head","head_sha":COMMIT,"head_repository":"source/project",
            "base_ref":"main","base_repository":"other/project","body":format!("Change ID: `{CHANGE}`")
        })]),
        FakeGh::new([json!({
            "number":8,"state":"open","merged_at":null,"title":"legacy",
            "head_ref":"legacy-head","head_sha":COMMIT,"head_repository":"source/project",
            "base_ref":"main","base_repository":"target/project",
            "body":"x".repeat(limits().body_bytes_max() + 1)
        })]),
    ];
    for fake in &cases {
        assert!(client(fake, &root)
            .resolve_legacy_candidate(candidate)
            .is_err());
    }
    assert_eq!(session.state().legacy_candidates().len(), 1);
    assert!(session.state().legacy_resolutions().is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn normal_v1_observation_uses_managed_metadata_not_legacy_prose() {
    let mut response = detail(8, CHANGE, "main", "open");
    response["body"] = json!(format!(
        "legacy-looking prose\nChange ID: `{NEXT}`\n{}",
        managed_body(CHANGE)
    ));
    let fake = FakeGh::new([
        repository("source/project"),
        repository("target/project"),
        json!([]),
        json!([compact(8, CHANGE)]),
        response,
    ]);
    let root = workspace("normal-does-not-use-legacy");
    assert_eq!(client(&fake, &root).observe().unwrap().prs().len(), 1);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn executable_fake_gh_records_structured_argv_and_stdin_without_network() {
    let fixture = ExecutableGh::new(
        "structured-process",
        [
            repository("source/project").to_string(),
            repository("target/project").to_string(),
            "[]".to_owned(),
            "[]".to_owned(),
        ],
    );
    let root = workspace("structured-process-workspace");
    // SAFETY: this test owns every child process created for this test binary.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    let config = resolved(&root);
    let client = GithubClient::new(
        &runner,
        fixture.program.clone(),
        &config,
        &StateV3::empty(config.scope().clone()),
        fixture.environment(),
    );
    assert!(client.observe().unwrap().prs().is_empty());
    let calls = fixture.calls();
    assert_eq!(calls.len(), 4);
    assert!(calls.iter().all(|call| call.stdin.is_empty()));
    assert_eq!(calls[0].args[0], OsString::from("api"));
    assert!(calls[2].args.iter().any(|arg| arg
        .to_string_lossy()
        .contains("repos/source/project/git/matching-refs")));
    assert!(calls[3].args.iter().any(|arg| arg
        .to_string_lossy()
        .contains("repos/target/project/pulls?")));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn legacy_marker_must_be_one_exact_whole_line() {
    let embedded = FakeGh::new([json!({
        "number": 8,
        "state": "open",
        "merged_at": null,
        "title": "legacy",
        "head_ref": "legacy-head",
        "head_sha": COMMIT,
        "head_repository": "source/project",
        "base_ref": "main",
        "base_repository": "target/project",
        "body": format!("prose Change ID: `{CHANGE}` suffix"),
    })]);
    let root = workspace("legacy-exact-line");
    let observed = client(&embedded, &root)
        .observe_pr(PrNumber::new(8).unwrap())
        .unwrap();
    assert_eq!(observed.lifecycle(), PrLifecycle::Open);
    fs::remove_dir_all(root).unwrap();

    let forged: PrOwnership = serde_json::from_value(json!({
        "change_id": CHANGE,
        "head": "legacy-head",
        "proof": {
            "ValidatedLegacy": {
                "number": 8,
                "observation_digest": vec![0; 32],
            }
        }
    }))
    .unwrap();
    let forged_effect = Effect::Github(GithubEffect::UpdateBase {
        ownership: forged,
        number: PrNumber::new(8).unwrap(),
        expected: HeadRef::parse("main").unwrap(),
        desired: HeadRef::parse("next").unwrap(),
    });
    let no_authority = FakeGh::new(std::iter::empty::<Value>());
    let root = workspace("forged-legacy-authority");
    let driver = client(&no_authority, &root);
    assert!(matches!(
        driver.observe_effect_precondition(&forged_effect),
        Err(GithubError::Precondition)
    ));
    assert!(no_authority.calls().is_empty());
    fs::remove_dir_all(root).unwrap();

    let effect = Effect::Github(GithubEffect::UpdateBase {
        ownership: ownership(CHANGE),
        number: PrNumber::new(8).unwrap(),
        expected: HeadRef::parse("main").unwrap(),
        desired: HeadRef::parse("next").unwrap(),
    });
    for (index, body) in [
        format!("Change ID: `{CHANGE}`"),
        format!("Change ID: `{CHANGE}`\nChange ID: `{CHANGE}`"),
    ]
    .into_iter()
    .enumerate()
    {
        let unvalidated = FakeGh::new([json!({
            "number": 8,
            "state": "open",
            "merged_at": null,
            "title": "legacy",
            "head_ref": "legacy-head",
            "head_sha": COMMIT,
            "head_repository": "source/project",
            "base_ref": "main",
            "base_repository": "target/project",
            "body": body,
        })]);
        let root = workspace(&format!("unvalidated-legacy-{index}"));
        let driver = client(&unvalidated, &root);
        assert!(matches!(
            driver.observe_effect_precondition(&effect),
            Err(GithubError::OwnershipMissing)
        ));
        fs::remove_dir_all(root).unwrap();
    }
}

#[derive(Clone, Copy)]
enum MutationOutcome {
    SuccessApply,
    SuccessApplyWrongSource,
    SuccessApplyWrongHead,
    SuccessWithoutApply,
    FailureWithoutApply,
    TimeoutWithoutApply,
    FailureAfterApply,
    MalformedWithoutApply,
    MalformedAfterApply,
    FailureAfterApplyAndObservationFailure,
}

struct MutationModelGh {
    pr: Mutex<Option<Value>>,
    head_present: Mutex<bool>,
    ref_name: String,
    outcome: Mutex<MutationOutcome>,
    mutation_count: Mutex<usize>,
    fail_next_read: Mutex<bool>,
    calls: Mutex<Vec<Call>>,
}

impl MutationModelGh {
    fn with_pr(pr: Value, outcome: MutationOutcome) -> Self {
        let ref_name = pr["head_ref"].as_str().unwrap().to_owned();
        Self {
            pr: Mutex::new(Some(pr)),
            head_present: Mutex::new(true),
            ref_name,
            outcome: Mutex::new(outcome),
            mutation_count: Mutex::new(0),
            fail_next_read: Mutex::new(false),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn with_head(outcome: MutationOutcome) -> Self {
        Self::with_named_head(format!("almighty-push/{CHANGE}"), outcome)
    }

    fn with_named_head(ref_name: String, outcome: MutationOutcome) -> Self {
        Self {
            pr: Mutex::new(None),
            head_present: Mutex::new(true),
            ref_name,
            outcome: Mutex::new(outcome),
            mutation_count: Mutex::new(0),
            fail_next_read: Mutex::new(false),
            calls: Mutex::new(Vec::new()),
        }
    }

    fn set_outcome(&self, outcome: MutationOutcome) {
        *self.outcome.lock().unwrap() = outcome;
    }

    fn mutation_count(&self) -> usize {
        *self.mutation_count.lock().unwrap()
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn apply(&self, method: &str, payload: &Value) {
        if method == "DELETE" {
            *self.head_present.lock().unwrap() = false;
            return;
        }
        let mut pr = self.pr.lock().unwrap();
        if method == "POST" {
            let change = payload["head"]
                .as_str()
                .unwrap()
                .split_once(':')
                .unwrap()
                .1
                .strip_prefix("almighty-push/")
                .unwrap();
            *pr = Some(json!({
                "number":8,"state":"open","merged_at":null,"title":payload["title"],
                "head_ref":format!("almighty-push/{change}"),"head_sha":COMMIT,
                "head_repository":"source/project","base_ref":payload["base"],
                "base_repository":"target/project","body":payload["body"]
            }));
            return;
        }
        let pr = pr.as_mut().unwrap();
        if let Some(base) = payload.get("base") {
            pr["base_ref"] = base.clone();
        }
        if let Some(body) = payload.get("body") {
            pr["body"] = body.clone();
        }
        if let Some(state) = payload.get("state") {
            pr["state"] = state.clone();
            pr["merged_at"] = Value::Null;
        }
    }
}

impl CommandExecutor for MutationModelGh {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        self.calls.lock().unwrap().push(Call {
            args: spec.args().to_vec(),
            stdin: spec.stdin().to_vec(),
        });
        if std::mem::take(&mut *self.fail_next_read.lock().unwrap()) {
            return Err(CommandError::Exit {
                status_code: Some(29),
                stdout: String::new(),
                stderr: "postcondition observation failed".to_owned(),
            });
        }
        let args = spec
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let method_index = args.iter().position(|arg| arg == "--method").unwrap();
        let method = &args[method_index + 1];
        let endpoint = &args[method_index + 2];
        if method == "GET" {
            let response = if endpoint == "repos/source/project" {
                repository("source/project")
            } else if endpoint == "repos/target/project" {
                repository("target/project")
            } else if endpoint.contains("matching-refs") {
                if *self.head_present.lock().unwrap() {
                    json!([{
                        "ref": format!("refs/heads/{}", self.ref_name),
                        "object": {"type":"commit","sha":COMMIT}
                    }])
                } else {
                    json!([])
                }
            } else if endpoint.contains("pulls?") {
                self.pr.lock().unwrap().as_ref().map_or_else(
                    || json!([]),
                    |pr| {
                        json!([{
                            "number":pr["number"],"state":pr["state"],
                            "merged_at":pr["merged_at"],"title":pr["title"],
                            "head_ref":pr["head_ref"],"head_sha":pr["head_sha"],
                            "head_repository":pr["head_repository"],
                            "base_ref":pr["base_ref"],"base_repository":pr["base_repository"]
                        }])
                    },
                )
            } else if endpoint == "repos/target/project/pulls/8" {
                self.pr.lock().unwrap().clone().unwrap()
            } else {
                panic!("unexpected model GET {endpoint}")
            };
            return Ok(CommandOutput {
                stdout: response.to_string(),
                stderr: String::new(),
            });
        }

        *self.mutation_count.lock().unwrap() += 1;
        let payload: Value = serde_json::from_slice(spec.stdin()).unwrap();
        let outcome = *self.outcome.lock().unwrap();
        let apply = matches!(
            outcome,
            MutationOutcome::SuccessApply
                | MutationOutcome::SuccessApplyWrongSource
                | MutationOutcome::SuccessApplyWrongHead
                | MutationOutcome::FailureAfterApply
                | MutationOutcome::MalformedAfterApply
                | MutationOutcome::FailureAfterApplyAndObservationFailure
        );
        if apply {
            self.apply(method, &payload);
            if matches!(outcome, MutationOutcome::SuccessApplyWrongSource) {
                self.pr.lock().unwrap().as_mut().unwrap()["head_repository"] =
                    json!("attacker/project");
            }
            if matches!(outcome, MutationOutcome::SuccessApplyWrongHead) {
                self.pr.lock().unwrap().as_mut().unwrap()["head_ref"] = json!("attacker/head");
            }
        }
        if matches!(
            outcome,
            MutationOutcome::FailureAfterApplyAndObservationFailure
        ) {
            *self.fail_next_read.lock().unwrap() = true;
        }
        if matches!(outcome, MutationOutcome::TimeoutWithoutApply) {
            return Err(CommandError::Timeout {
                timeout: std::time::Duration::from_millis(100),
            });
        }
        if matches!(
            outcome,
            MutationOutcome::FailureWithoutApply
                | MutationOutcome::FailureAfterApply
                | MutationOutcome::FailureAfterApplyAndObservationFailure
        ) {
            return Err(CommandError::Exit {
                status_code: Some(1),
                stdout: String::new(),
                stderr: "mutation response lost".to_owned(),
            });
        }
        Ok(CommandOutput {
            stdout: if matches!(
                outcome,
                MutationOutcome::MalformedWithoutApply | MutationOutcome::MalformedAfterApply
            ) {
                "[]".to_owned()
            } else if method == "DELETE" {
                String::new()
            } else {
                json!({"number":8}).to_string()
            },
            stderr: String::new(),
        })
    }
}

fn execute_persisted(
    root: &Path,
    store: &StateStore,
    plan: &Plan,
    model: &MutationModelGh,
) -> Result<StageCompletion, ExecutorError<GithubError>> {
    let config = resolved(root);
    let state = store.load().unwrap();
    let mut driver = GithubClient::new(
        model,
        PathBuf::from("/fake/gh"),
        &config,
        &state,
        Box::new([]),
    );
    match state.checkpoint() {
        CheckpointState::Completed(checkpoint) if checkpoint.plan() != plan => {
            let completed = Executor::resume_stage(store, &mut driver)?;
            Executor::execute_next_stage(
                store,
                completed.acknowledgement(),
                plan.clone(),
                &mut driver,
            )
        }
        _ => Executor::execute_stage(store, plan.clone(), &mut driver),
    }
}

fn execute_persisted_plan(
    root: &Path,
    store: &StateStore,
    effect: Effect,
    model: &MutationModelGh,
) -> Result<StageCompletion, ExecutorError<GithubError>> {
    let plan = Plan::new(scope(), vec![effect].into_boxed_slice(), limits()).unwrap();
    let result = execute_persisted(root, store, &plan, model);
    if result.is_ok() {
        assert!(matches!(
            store.load().unwrap().checkpoint(),
            CheckpointState::Completed(checkpoint) if checkpoint.plan() == &plan
        ));
    }
    result
}

fn assert_persisted_retry(label: &str, effect: Effect, model: MutationModelGh) {
    let root = workspace(label);
    let store = StateStore::open(&root, scope(), limits()).unwrap();
    let plan = Plan::new(scope(), vec![effect].into_boxed_slice(), limits()).unwrap();

    assert!(matches!(
        execute_persisted(&root, &store, &plan, &model),
        Err(ExecutorError::Driver(GithubError::Command(_)))
    ));
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Executing(checkpoint)
            if checkpoint.plan().id() == plan.id()
                && checkpoint.next_effect_index() == 0
                && checkpoint.results() == [None]
    ));

    model.set_outcome(MutationOutcome::SuccessApply);
    assert_eq!(
        execute_persisted(&root, &store, &plan, &model).unwrap(),
        StageOutcome::Complete
    );
    assert!(matches!(
        store.load().unwrap().checkpoint(),
        CheckpointState::Completed(checkpoint) if checkpoint.plan() == &plan
    ));
    assert_eq!(model.mutation_count(), 2);
    fs::remove_dir_all(root).unwrap();
}

fn legacy_pr(head: &str, base: &str, state: &str, body: &str) -> Value {
    json!({
        "number":8,"state":state,"merged_at":null,"title":"legacy",
        "head_ref":head,"head_sha":COMMIT,"head_repository":"source/project",
        "base_ref":base,"base_repository":"target/project","body":body
    })
}

fn validated_legacy_fixture(
    label: &str,
    head: &str,
    body: &str,
) -> (PathBuf, StateStore, PrOwnership) {
    let root = workspace(label);
    write_legacy(
        &root,
        CHANGE,
        "https://github.com/target/project/pull/8",
        head,
        COMMIT,
        Some(CHANGE),
    );
    let store = StateStore::open(&root, scope(), limits()).unwrap();
    let mut session = store.lock().unwrap();
    let observation = FakeGh::new([legacy_pr(head, "main", "open", body)]);
    let resolution = client(&observation, &root)
        .resolve_legacy_candidate(&session.state().legacy_candidates()[0])
        .unwrap();
    session.resolve_legacy(resolution).unwrap();
    let authority = session
        .state()
        .verified()
        .get(&ChangeId::parse(CHANGE).unwrap())
        .unwrap()
        .ownership()
        .clone();
    assert!(authority.is_validated_legacy());
    session.finish_legacy_deletion().unwrap();
    drop(session);
    (root, store, authority)
}

fn assert_legacy_mutation(model: &MutationModelGh, method: &str, endpoint: &str, payload: Value) {
    assert_eq!(model.mutation_count(), 1);
    let calls = model.calls();
    let mutation = calls
        .iter()
        .find(|call| call.args.iter().any(|argument| argument == method))
        .unwrap();
    assert!(mutation.args.iter().any(|argument| argument == endpoint));
    assert_eq!(
        serde_json::from_slice::<Value>(&mutation.stdin).unwrap(),
        payload
    );
}

fn execute_model(
    label: &str,
    effect: Effect,
    model: &MutationModelGh,
) -> Result<StageCompletion, ExecutorError<GithubError>> {
    let root = workspace(label);
    let config = resolved(&root);
    let store = StateStore::open(&root, scope(), limits()).unwrap();
    let plan = Plan::new(scope(), vec![effect].into_boxed_slice(), limits()).unwrap();
    let mut driver = GithubClient::new(
        model,
        PathBuf::from("/fake/gh"),
        &config,
        &StateV3::empty(config.scope().clone()),
        Box::new([]),
    );
    let result = Executor::execute_stage(&store, plan, &mut driver);
    fs::remove_dir_all(root).unwrap();
    result
}

#[derive(Default)]
struct CreateModelGh {
    created: Mutex<Option<Value>>,
    mutation_count: Mutex<usize>,
}

impl CreateModelGh {
    fn mutation_count(&self) -> usize {
        *self.mutation_count.lock().unwrap()
    }
}

impl CommandExecutor for CreateModelGh {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        let args = spec
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let method_index = args.iter().position(|arg| arg == "--method").unwrap();
        let method = &args[method_index + 1];
        let endpoint = &args[method_index + 2];
        if method == "POST" {
            let payload: Value = serde_json::from_slice(spec.stdin()).unwrap();
            *self.created.lock().unwrap() = Some(payload);
            *self.mutation_count.lock().unwrap() += 1;
            return Err(CommandError::Exit {
                status_code: Some(1),
                stdout: String::new(),
                stderr: "response lost after create".to_owned(),
            });
        }
        let created = self.created.lock().unwrap().clone();
        let response = match endpoint.as_str() {
            "repos/source/project" => repository("source/project"),
            "repos/target/project" => repository("target/project"),
            endpoint if endpoint.contains("matching-refs") => json!([{
                "ref": format!("refs/heads/almighty-push/{CHANGE}"),
                "object": { "type": "commit", "sha": COMMIT },
            }]),
            endpoint if endpoint.contains("pulls?") => created.as_ref().map_or_else(
                || json!([]),
                |payload| {
                    json!([{
                        "number": 8,
                        "state": "open",
                        "merged_at": null,
                        "title": payload["title"],
                        "head_ref": format!("almighty-push/{CHANGE}"),
                        "head_sha": COMMIT,
                        "head_repository": "source/project",
                        "base_ref": payload["base"],
                        "base_repository": "target/project",
                    }])
                },
            ),
            "repos/target/project/pulls/8" => {
                let payload = created.as_ref().expect("detail follows create");
                json!({
                    "number": 8,
                    "state": "open",
                    "merged_at": null,
                    "title": payload["title"],
                    "head_ref": format!("almighty-push/{CHANGE}"),
                    "head_sha": COMMIT,
                    "head_repository": "source/project",
                    "base_ref": payload["base"],
                    "base_repository": "target/project",
                    "body": payload["body"],
                })
            }
            other => panic!("unexpected fake-gh endpoint {other}"),
        };
        Ok(CommandOutput {
            stdout: response.to_string(),
            stderr: String::new(),
        })
    }
}

struct ExecutableGh {
    root: PathBuf,
    program: PathBuf,
}

impl ExecutableGh {
    fn new(label: &str, replies: impl IntoIterator<Item = String>) -> Self {
        let root = workspace(label);
        let program = root.join("gh");
        fs::write(
            &program,
            r#"#!/bin/sh
set -eu
fixture=${ALMIGHTY_FAKE_GH_DIRECTORY:?}
counter=$fixture/counter
index=0
if test -f "$counter"; then index=$(cat "$counter"); fi
printf '%s' "$((index + 1))" > "$counter"
call=$fixture/call.$index
mkdir "$call"
printf '%s' "$#" > "$call/argc"
arg_index=0
for arg in "$@"; do
  printf '%s' "$arg" > "$call/arg.$arg_index"
  arg_index=$((arg_index + 1))
done
cat > "$call/stdin"
if test ! -f "$fixture/reply.$index"; then exit 99; fi
cat "$fixture/reply.$index"
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        for (index, reply) in replies.into_iter().enumerate() {
            fs::write(root.join(format!("reply.{index}")), reply).unwrap();
        }
        Self { root, program }
    }

    fn environment(&self) -> Box<[(OsString, OsString)]> {
        Box::new([
            (
                OsString::from("ALMIGHTY_FAKE_GH_DIRECTORY"),
                self.root.as_os_str().to_owned(),
            ),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ])
    }

    fn calls(&self) -> Vec<Call> {
        let count = fs::read_to_string(self.root.join("counter"))
            .unwrap()
            .parse::<usize>()
            .unwrap();
        (0..count)
            .map(|index| read_call(&self.root.join(format!("call.{index}"))))
            .collect()
    }
}

impl Drop for ExecutableGh {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

fn read_call(path: &Path) -> Call {
    let argc = fs::read_to_string(path.join("argc"))
        .unwrap()
        .parse::<usize>()
        .unwrap();
    let args = (0..argc)
        .map(|index| OsString::from(fs::read_to_string(path.join(format!("arg.{index}"))).unwrap()))
        .collect();
    Call {
        args,
        stdin: fs::read(path.join("stdin")).unwrap(),
    }
}
