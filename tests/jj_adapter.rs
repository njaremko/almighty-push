use almighty_push::command::{
    CommandError, CommandExecutor, CommandOutput, CommandRunner, CommandSpec,
};
use almighty_push::config::{ConfigError, ConfigInput, ConfigResolver, ResolvedConfig};
use almighty_push::domain::{
    ChangeId, CommitId, DomainError, HeadRef, LimitValues, Limits, RemoteName, RepositoryId,
};
use almighty_push::executor::{Executor, ExecutorError, StageOutcome};
use almighty_push::jj::{JjClient, JjError, JjQuery};
use almighty_push::plan::{Effect, JjEffect, Plan, PrOwnership, RemoteRefState, ReobserveBarrier};
use almighty_push::state::StateStore;
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const ROOT_CHANGE: &str = "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk";
const CHILD_CHANGE: &str = "llllllllllllllllllllllllllllllll";
const OTHER_CHANGE: &str = "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm";
const ROOT_COMMIT: &str = "1111111111111111111111111111111111111111";
const CHILD_COMMIT: &str = "2222222222222222222222222222222222222222";
const OTHER_COMMIT: &str = "3333333333333333333333333333333333333333";

#[derive(Clone)]
struct Reply {
    stdout: String,
    status: i32,
}

struct FakeJj {
    root: PathBuf,
    program: PathBuf,
}

impl FakeJj {
    fn new(label: &str, replies: impl IntoIterator<Item = Reply>) -> Self {
        let root = unique_directory(label);
        let program = root.join("jj");
        fs::write(
            &program,
            r#"#!/bin/sh
set -eu
fixture=${ALMIGHTY_FAKE_JJ_DIRECTORY:?}
counter=$fixture/counter
index=0
if test -f "$counter"; then
  index=$(cat "$counter")
fi
next=$((index + 1))
printf '%s' "$next" > "$counter"
if test ! -f "$fixture/$index.stdout"; then
  printf 'unexpected fake jj invocation %s\n' "$index" >&2
  exit 99
fi
cat "$fixture/$index.stdout"
status=$(cat "$fixture/$index.status")
exit "$status"
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        for (index, reply) in replies.into_iter().enumerate() {
            fs::write(root.join(format!("{index}.stdout")), reply.stdout).unwrap();
            fs::write(
                root.join(format!("{index}.status")),
                reply.status.to_string(),
            )
            .unwrap();
        }
        Self { root, program }
    }

    fn environment(&self) -> Box<[(OsString, OsString)]> {
        Box::new([
            (
                OsString::from("ALMIGHTY_FAKE_JJ_DIRECTORY"),
                self.root.as_os_str().to_owned(),
            ),
            (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        ])
    }
}

impl Drop for FakeJj {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.root).unwrap();
    }
}

struct RecordingExecutor<E> {
    inner: E,
    calls: Mutex<Vec<Vec<OsString>>>,
}

impl<E> RecordingExecutor<E> {
    fn new(inner: E) -> Self {
        Self {
            inner,
            calls: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> Vec<Vec<OsString>> {
        self.calls.lock().unwrap().clone()
    }
}

impl<E: CommandExecutor> CommandExecutor for RecordingExecutor<E> {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        self.calls.lock().unwrap().push(spec.args().to_vec());
        self.inner.run(spec)
    }
}

struct WorkspaceSwapExecutor<E> {
    inner: E,
    workspace: PathBuf,
    moved_workspace: PathBuf,
}

impl<E: CommandExecutor> CommandExecutor for WorkspaceSwapExecutor<E> {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        fs::rename(&self.workspace, &self.moved_workspace).unwrap();
        fs::create_dir(&self.workspace).unwrap();
        self.inner.run(spec)
    }
}

struct MetadataSwapExecutor<E> {
    inner: E,
    workspace: PathBuf,
    moved_jj: PathBuf,
    swapped: Mutex<bool>,
}

impl<E> MetadataSwapExecutor<E> {
    fn new(inner: E, workspace: PathBuf, moved_jj: PathBuf) -> Self {
        Self {
            inner,
            workspace,
            moved_jj,
            swapped: Mutex::new(false),
        }
    }
}

impl<E: CommandExecutor> CommandExecutor for MetadataSwapExecutor<E> {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        let mut swapped = self.swapped.lock().unwrap();
        if !*swapped {
            fs::rename(self.workspace.join(".jj"), &self.moved_jj).unwrap();
            fs::create_dir(self.workspace.join(".jj")).unwrap();
            *swapped = true;
        }
        drop(swapped);
        self.inner.run(spec)
    }
}

struct CommandMetadataPointerSwapExecutor<E> {
    inner: E,
    pointer: PathBuf,
    moved_pointer: PathBuf,
}

impl<E: CommandExecutor> CommandExecutor for CommandMetadataPointerSwapExecutor<E> {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        fs::rename(&self.pointer, &self.moved_pointer).unwrap();
        fs::create_dir(&self.pointer).unwrap();
        self.inner.run(spec)
    }
}

struct ConfigExecutor {
    replies: Mutex<VecDeque<CommandOutput>>,
}

impl ConfigExecutor {
    fn new(workspace: &Path) -> Self {
        Self {
            replies: Mutex::new(VecDeque::from([
                CommandOutput {
                    stdout: workspace.display().to_string(),
                    stderr: String::new(),
                },
                CommandOutput {
                    stdout: "origin https://github.com/source/project.git\n".to_owned(),
                    stderr: String::new(),
                },
            ])),
        }
    }
}

impl CommandExecutor for ConfigExecutor {
    fn run(&self, _spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        Ok(self.replies.lock().unwrap().pop_front().unwrap())
    }
}

fn limits() -> Limits {
    Limits::new(LimitValues {
        github_page_size: 8,
        command_output_bytes_max: 65_536,
        command_timeout_ms: 2_000,
        ..LimitValues::default()
    })
    .unwrap()
}

fn resolver_config(workspace: &Path, locked: bool) -> ResolvedConfig {
    let executor = ConfigExecutor::new(workspace);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let input = ConfigInput {
        remote: Some(RemoteName::parse("origin").unwrap()),
        repository: Some(RepositoryId::parse("github.com/source/project").unwrap()),
        base: Some(HeadRef::parse("main").unwrap()),
        tip_revset: "@".to_owned(),
        limits: limits(),
        github_enabled: false,
    };
    if locked {
        resolver.resolve_locked(&input, workspace).unwrap()
    } else {
        resolver.resolve(&input, workspace).unwrap()
    }
}

fn resolve(workspace: &Path) -> ResolvedConfig {
    resolver_config(workspace, false)
}

fn resolve_locked(workspace: &Path) -> ResolvedConfig {
    resolver_config(workspace, true)
}

fn workspace(label: &str) -> PathBuf {
    let root = unique_directory(label);
    fs::create_dir(root.join(".jj")).unwrap();
    root
}

fn reply(stdout: impl Into<String>) -> Reply {
    Reply {
        stdout: stdout.into(),
        status: 0,
    }
}

fn failed_reply(status: i32) -> Reply {
    Reply {
        stdout: String::new(),
        status,
    }
}

fn row(change_id: &str, commit_id: &str, description: &str) -> Value {
    json!({
        "change_id": change_id,
        "commit_id": commit_id,
        "description": description,
    })
}

fn jsonl(rows: impl IntoIterator<Item = Value>) -> String {
    rows.into_iter().map(|value| format!("{value}\n")).collect()
}

fn parent_observation(source: Value, parents: impl IntoIterator<Item = Value>) -> String {
    jsonl(std::iter::once(source).chain(parents))
}

fn remote_row(head: &str, commit_id: &str) -> String {
    jsonl([json!({
        "name": head,
        "remote": "origin",
        "present": true,
        "conflict": false,
        "normal_target": row(ROOT_CHANGE, commit_id, "remote target"),
    })])
}

#[test]
fn structured_queries_preserve_full_descriptions_parents_and_conflicts() {
    let workspace = workspace("structured");
    let config = resolve(&workspace);
    let description = "subject | exact\n\tcontrol: \u{1b}[31m\r\nbody";
    let revisions = jsonl([
        row(ROOT_CHANGE, ROOT_COMMIT, description),
        row(CHILD_CHANGE, CHILD_COMMIT, "child"),
    ]);
    let fake = FakeJj::new(
        "structured-command",
        [
            reply(revisions),
            reply(parent_observation(
                row(ROOT_CHANGE, ROOT_COMMIT, description),
                [row(OTHER_CHANGE, OTHER_COMMIT, "outside parent")],
            )),
            reply(parent_observation(
                row(CHILD_CHANGE, CHILD_COMMIT, "child"),
                [row(ROOT_CHANGE, ROOT_COMMIT, description)],
            )),
            reply(jsonl([row(CHILD_CHANGE, CHILD_COMMIT, "child")])),
        ],
    );
    // SAFETY: this test owns every child process it creates.
    let executor =
        RecordingExecutor::new(unsafe { CommandRunner::assume_process_child_authority() });
    let client = JjClient::new(&executor, fake.program.clone(), &config, fake.environment());

    let chain = client.observe_chain().unwrap();

    assert_eq!(chain.revisions().len(), 2);
    assert_eq!(chain.revisions()[0].change_id().as_str(), ROOT_CHANGE);
    assert_eq!(chain.revisions()[0].commit_id().as_str(), ROOT_COMMIT);
    assert_eq!(chain.revisions()[0].description(), description);
    assert_eq!(
        chain.revisions()[0].parent_change_ids()[0].as_str(),
        OTHER_CHANGE
    );
    assert!(chain.revisions()[1].has_conflict());
    let calls = executor.calls();
    assert_eq!(calls.len(), 4);
    assert!(calls[0].iter().any(|arg| {
        arg == OsStr::new("(remote_bookmarks(exact:\"main\", exact:\"origin\"))..(@)")
    }));
    assert!(calls[0]
        .iter()
        .any(|arg| arg == OsStr::new("json(self) ++ \"\\n\"")));
    assert!(calls[1].iter().any(|arg| {
        arg.to_string_lossy()
            .contains(&format!("commit_id(\"{ROOT_COMMIT}\")"))
    }));
    assert!(calls[3]
        .iter()
        .any(|arg| arg.to_string_lossy().contains("conflicts()")));
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn selected_parent_commit_versions_must_match_deterministically() {
    let workspace = workspace("parent-version-divergence");
    let config = resolve(&workspace);
    let fake = FakeJj::new(
        "parent-version-divergence-command",
        [
            reply(jsonl([
                row(OTHER_CHANGE, OTHER_COMMIT, "other"),
                row(ROOT_CHANGE, ROOT_COMMIT, "root"),
                row(CHILD_CHANGE, CHILD_COMMIT, "child"),
            ])),
            reply(parent_observation(
                row(ROOT_CHANGE, ROOT_COMMIT, "root"),
                std::iter::empty(),
            )),
            reply(parent_observation(
                row(CHILD_CHANGE, CHILD_COMMIT, "child"),
                [row(ROOT_CHANGE, OTHER_COMMIT, "divergent root version")],
            )),
            reply(parent_observation(
                row(OTHER_CHANGE, OTHER_COMMIT, "other"),
                [row(CHILD_CHANGE, ROOT_COMMIT, "divergent child version")],
            )),
        ],
    );
    // SAFETY: this test owns every child process it creates.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };

    let error = JjClient::new(&runner, fake.program.clone(), &config, fake.environment())
        .observe_chain()
        .unwrap_err();

    assert!(matches!(
        error,
        JjError::SelectedParentVersionChanged {
            child_change_id,
            parent_change_id,
            selected_commit_id,
            observed_commit_id,
        } if child_change_id.as_str() == CHILD_CHANGE
            && parent_change_id.as_str() == ROOT_CHANGE
            && selected_commit_id.as_str() == ROOT_COMMIT
            && observed_commit_id.as_str() == OTHER_COMMIT
    ));
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn empty_selection_and_selected_fork_and_merge_use_selected_chain_laws() {
    let empty_workspace = workspace("empty");
    let empty_config = resolve(&empty_workspace);
    let empty_fake = FakeJj::new("empty-command", [reply(""), reply("")]);
    // SAFETY: this test owns every child process it creates.
    let empty_runner = unsafe { CommandRunner::assume_process_child_authority() };
    let empty = JjClient::new(
        &empty_runner,
        empty_fake.program.clone(),
        &empty_config,
        empty_fake.environment(),
    )
    .observe_chain()
    .unwrap();
    assert!(empty.revisions().is_empty());
    fs::remove_dir_all(empty_workspace).unwrap();

    let fork_workspace = workspace("fork");
    let fork_config = resolve(&fork_workspace);
    let fork_fake = FakeJj::new(
        "fork-command",
        [
            reply(jsonl([
                row(ROOT_CHANGE, ROOT_COMMIT, "root"),
                row(CHILD_CHANGE, CHILD_COMMIT, "child"),
                row(OTHER_CHANGE, OTHER_COMMIT, "other"),
            ])),
            reply(parent_observation(
                row(ROOT_CHANGE, ROOT_COMMIT, "root"),
                std::iter::empty(),
            )),
            reply(parent_observation(
                row(CHILD_CHANGE, CHILD_COMMIT, "child"),
                [row(ROOT_CHANGE, ROOT_COMMIT, "root")],
            )),
            reply(parent_observation(
                row(OTHER_CHANGE, OTHER_COMMIT, "other"),
                [row(ROOT_CHANGE, ROOT_COMMIT, "root")],
            )),
            reply(""),
        ],
    );
    // SAFETY: this test owns every child process it creates.
    let fork_runner = unsafe { CommandRunner::assume_process_child_authority() };
    assert!(matches!(
        JjClient::new(
            &fork_runner,
            fork_fake.program.clone(),
            &fork_config,
            fork_fake.environment(),
        )
        .observe_chain(),
        Err(JjError::Domain(error))
            if matches!(*error, DomainError::SelectedFork { .. })
    ));
    fs::remove_dir_all(fork_workspace).unwrap();

    let merge_workspace = workspace("merge");
    let merge_config = resolve(&merge_workspace);
    let merge_fake = FakeJj::new(
        "merge-command",
        [
            reply(jsonl([row(CHILD_CHANGE, CHILD_COMMIT, "merge")])),
            reply(parent_observation(
                row(CHILD_CHANGE, CHILD_COMMIT, "merge"),
                [
                    row(ROOT_CHANGE, ROOT_COMMIT, "first parent"),
                    row(OTHER_CHANGE, OTHER_COMMIT, "second parent"),
                ],
            )),
            reply(""),
        ],
    );
    // SAFETY: this test owns every child process it creates.
    let merge_runner = unsafe { CommandRunner::assume_process_child_authority() };
    assert!(matches!(
        JjClient::new(
            &merge_runner,
            merge_fake.program.clone(),
            &merge_config,
            merge_fake.environment(),
        )
        .observe_chain(),
        Err(JjError::Domain(error))
            if matches!(*error, DomainError::SelectedMerge { .. })
    ));
    fs::remove_dir_all(merge_workspace).unwrap();
}

#[test]
fn duplicate_malformed_row_bound_output_bound_and_nonzero_fail_deterministically() {
    let duplicate_workspace = workspace("duplicate");
    let duplicate_config = resolve(&duplicate_workspace);
    let duplicate_fake = FakeJj::new(
        "duplicate-command",
        [reply(jsonl([
            row(CHILD_CHANGE, CHILD_COMMIT, "first"),
            row(ROOT_CHANGE, ROOT_COMMIT, "root first"),
            row(ROOT_CHANGE, OTHER_COMMIT, "root duplicate"),
            row(CHILD_CHANGE, OTHER_COMMIT, "child duplicate"),
        ]))],
    );
    // SAFETY: this test owns every child process it creates.
    let duplicate_runner = unsafe { CommandRunner::assume_process_child_authority() };
    assert!(matches!(
        JjClient::new(
            &duplicate_runner,
            duplicate_fake.program.clone(),
            &duplicate_config,
            duplicate_fake.environment(),
        )
        .observe_chain(),
        Err(JjError::DuplicateRevision { change_id }) if change_id.as_str() == ROOT_CHANGE
    ));
    fs::remove_dir_all(duplicate_workspace).unwrap();

    let malformed_workspace = workspace("malformed");
    let malformed_config = resolve(&malformed_workspace);
    let malformed_fake = FakeJj::new("malformed-command", [reply("{broken\n")]);
    // SAFETY: this test owns every child process it creates.
    let malformed_runner = unsafe { CommandRunner::assume_process_child_authority() };
    assert!(matches!(
        JjClient::new(
            &malformed_runner,
            malformed_fake.program.clone(),
            &malformed_config,
            malformed_fake.environment(),
        )
        .observe_chain(),
        Err(JjError::MalformedJson {
            query: JjQuery::Revisions,
            line: 1,
            ..
        })
    ));
    fs::remove_dir_all(malformed_workspace).unwrap();

    let row_bound_workspace = workspace("row-bound");
    let row_bound_config = resolve(&row_bound_workspace);
    let rows = (0..65).map(|index| {
        let digit = format!("{index:040x}");
        let mut id = [b'k'; 32];
        id[31] = b'k' + (index % 16) as u8;
        id[30] = b'k' + (index / 16) as u8;
        row(std::str::from_utf8(&id).unwrap(), &digit, "bounded")
    });
    let row_bound_fake = FakeJj::new("row-bound-command", [reply(jsonl(rows))]);
    // SAFETY: this test owns every child process it creates.
    let row_bound_runner = unsafe { CommandRunner::assume_process_child_authority() };
    assert!(matches!(
        JjClient::new(
            &row_bound_runner,
            row_bound_fake.program.clone(),
            &row_bound_config,
            row_bound_fake.environment(),
        )
        .observe_chain(),
        Err(JjError::RowCountExceeded {
            query: JjQuery::Revisions,
            count: 65,
            max: 64,
        })
    ));
    fs::remove_dir_all(row_bound_workspace).unwrap();

    let output_bound_workspace = workspace("output-bound");
    let output_bound_config = resolve(&output_bound_workspace);
    let output_bound_fake = FakeJj::new("output-bound-command", [reply("x".repeat(65_537))]);
    // SAFETY: this test owns every child process it creates.
    let output_bound_runner = unsafe { CommandRunner::assume_process_child_authority() };
    assert!(matches!(
        JjClient::new(
            &output_bound_runner,
            output_bound_fake.program.clone(),
            &output_bound_config,
            output_bound_fake.environment(),
        )
        .observe_chain(),
        Err(JjError::Command(error))
            if matches!(*error, CommandError::OutputLimit { limit_bytes: 65_536 })
    ));
    fs::remove_dir_all(output_bound_workspace).unwrap();

    let failed_workspace = workspace("nonzero");
    let failed_config = resolve(&failed_workspace);
    let failed_fake = FakeJj::new("nonzero-command", [failed_reply(23)]);
    // SAFETY: this test owns every child process it creates.
    let failed_runner = unsafe { CommandRunner::assume_process_child_authority() };
    assert!(matches!(
        JjClient::new(
            &failed_runner,
            failed_fake.program.clone(),
            &failed_config,
            failed_fake.environment(),
        )
        .observe_chain(),
        Err(JjError::Command(error))
            if matches!(*error, CommandError::Exit { status_code: Some(23), .. })
    ));
    fs::remove_dir_all(failed_workspace).unwrap();
}

#[test]
fn fetch_and_push_use_the_resolved_remote_and_exact_owned_name() {
    let workspace = workspace("push");
    let config = resolve_locked(&workspace);
    let head = HeadRef::owned(&ChangeId::parse(ROOT_CHANGE).unwrap()).unwrap();
    let effect = JjEffect::PushHead {
        ownership: PrOwnership::generated(ChangeId::parse(ROOT_CHANGE).unwrap()).unwrap(),
        expected: RemoteRefState::Absent,
        desired: CommitId::parse(ROOT_COMMIT).unwrap(),
    };
    let fake = FakeJj::new(
        "push-command",
        [
            reply("fetched"),
            reply(""),
            reply(jsonl([row(ROOT_CHANGE, ROOT_COMMIT, "local")])),
            reply(""),
            reply(jsonl([row(ROOT_CHANGE, ROOT_COMMIT, "local")])),
            reply("pushed"),
            reply(remote_row(head.as_str(), ROOT_COMMIT)),
        ],
    );
    // SAFETY: this test owns every child process it creates.
    let executor =
        RecordingExecutor::new(unsafe { CommandRunner::assume_process_child_authority() });
    let mut client = JjClient::new(&executor, fake.program.clone(), &config, fake.environment());
    let store = StateStore::from_config(&config).unwrap();
    let plan = Plan::new(
        config.scope().clone(),
        vec![Effect::Jj(effect)].into_boxed_slice(),
        limits(),
    )
    .unwrap();

    client.fetch().unwrap();
    assert_eq!(
        Executor::execute_stage(&store, plan, &mut client).unwrap(),
        StageOutcome::Complete
    );

    let calls = executor.calls();
    assert_eq!(
        calls[0],
        os_args([
            "--ignore-working-copy",
            "git",
            "fetch",
            "--remote",
            "origin"
        ])
    );
    assert_eq!(
        calls[5],
        os_args([
            "--ignore-working-copy",
            "git",
            "push",
            "--remote",
            "origin",
            "--named",
            &format!("{}={ROOT_CHANGE}", head.as_str()),
        ])
    );
    assert!(!calls[5].iter().any(|arg| arg == OsStr::new("--change")));
    assert!(calls[1].iter().any(|arg| arg == OsStr::new("--remote")));
    assert!(calls[1].iter().any(|arg| arg == OsStr::new("exact:origin")));
    let head_pattern = OsString::from(format!("exact:{}", head.as_str()));
    assert!(calls[1].iter().any(|arg| arg == &head_pattern));
    assert!(!calls[1]
        .iter()
        .any(|arg| arg == OsStr::new("--all-remotes")));
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn remote_absence_requires_a_successful_empty_exact_observation() {
    let absent_workspace = workspace("remote-absent");
    let absent_config = resolve(&absent_workspace);
    let head = HeadRef::owned(&ChangeId::parse(ROOT_CHANGE).unwrap()).unwrap();
    let absent_fake = FakeJj::new("remote-absent-command", [reply("")]);
    // SAFETY: this test owns every child process it creates.
    let absent_runner = unsafe { CommandRunner::assume_process_child_authority() };
    let absent = JjClient::new(
        &absent_runner,
        absent_fake.program.clone(),
        &absent_config,
        absent_fake.environment(),
    )
    .observe_remote_heads(std::slice::from_ref(&head))
    .unwrap();
    assert_eq!(absent[&head], RemoteRefState::Absent);
    fs::remove_dir_all(absent_workspace).unwrap();

    let failed_workspace = workspace("remote-failed");
    let failed_config = resolve(&failed_workspace);
    let failed_fake = FakeJj::new("remote-failed-command", [failed_reply(7)]);
    // SAFETY: this test owns every child process it creates.
    let failed_runner = unsafe { CommandRunner::assume_process_child_authority() };
    assert!(matches!(
        JjClient::new(
            &failed_runner,
            failed_fake.program.clone(),
            &failed_config,
            failed_fake.environment(),
        )
        .observe_remote_heads(std::slice::from_ref(&head)),
        Err(JjError::Command(error))
            if matches!(*error, CommandError::Exit { status_code: Some(7), .. })
    ));
    fs::remove_dir_all(failed_workspace).unwrap();

    let malformed_workspace = workspace("remote-malformed");
    let malformed_config = resolve(&malformed_workspace);
    let malformed_fake = FakeJj::new(
        "remote-malformed-command",
        [reply(jsonl([json!({
            "name": head.as_str(),
            "remote": "origin",
            "present": false,
            "conflict": false,
            "normal_target": null,
        })]))],
    );
    // SAFETY: this test owns every child process it creates.
    let malformed_runner = unsafe { CommandRunner::assume_process_child_authority() };
    assert!(matches!(
        JjClient::new(
            &malformed_runner,
            malformed_fake.program.clone(),
            &malformed_config,
            malformed_fake.environment(),
        )
        .observe_remote_heads(std::slice::from_ref(&head)),
        Err(JjError::MalformedRemoteHead { .. })
    ));
    fs::remove_dir_all(malformed_workspace).unwrap();
}

#[test]
fn first_observation_setup_creates_no_repository_entries() {
    let workspace = workspace("side-effect-free-observation");
    let before = snapshot_repository(&workspace);
    let config = resolve(&workspace);
    let fake = FakeJj::new(
        "side-effect-free-observation-command",
        [reply(""), reply("")],
    );
    // SAFETY: this test owns every child process it creates.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    let client = JjClient::new(&runner, fake.program.clone(), &config, fake.environment());

    assert!(client.observe_chain().unwrap().revisions().is_empty());

    drop(client);
    drop(config);
    assert!(!workspace.join(".jj/almighty-push").exists());
    assert_eq!(snapshot_repository(&workspace), before);
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn replaced_workspace_path_fails_before_any_jj_subprocess() {
    let workspace = workspace("workspace-replacement");
    let moved_workspace = workspace.with_extension("retained");
    let config = resolve(&workspace);
    fs::rename(&workspace, &moved_workspace).unwrap();
    fs::create_dir(&workspace).unwrap();
    fs::create_dir(workspace.join(".jj")).unwrap();
    let fake = FakeJj::new("workspace-replacement-command", [reply("unexpected")]);
    // SAFETY: this test owns every child process it creates.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    let client = JjClient::new(&runner, fake.program.clone(), &config, fake.environment());

    assert!(matches!(
        client.fetch(),
        Err(JjError::Config(error))
            if matches!(*error, ConfigError::WorkspaceIdentityChanged { .. })
    ));
    assert!(!fake.root.join("counter").exists());

    drop(client);
    drop(config);
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(moved_workspace).unwrap();
}

#[test]
fn replaced_jj_metadata_fails_before_any_jj_subprocess() {
    let workspace = workspace("jj-replacement");
    let moved_jj = workspace.join(".jj-retained");
    let config = resolve(&workspace);
    fs::rename(workspace.join(".jj"), &moved_jj).unwrap();
    fs::create_dir(workspace.join(".jj")).unwrap();
    let fake = FakeJj::new("jj-replacement-command", [reply("unexpected")]);
    // SAFETY: this test owns every child process it creates.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    let client = JjClient::new(&runner, fake.program.clone(), &config, fake.environment());

    assert!(matches!(
        client.fetch(),
        Err(JjError::Config(error))
            if matches!(*error, ConfigError::WorkspaceIdentityChanged { .. })
    ));
    assert!(!fake.root.join("counter").exists());

    drop(client);
    drop(config);
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn workspace_swap_after_spec_validation_cannot_reach_a_mutating_child() {
    let workspace = workspace("workspace-boundary-replacement");
    let moved_workspace = workspace.with_extension("retained");
    let config = resolve(&workspace);
    let fake_root = unique_directory("workspace-boundary-replacement-command");
    let child_marker = fake_root.join("child-spawned");
    let program = fake_root.join("jj");
    fs::write(
        &program,
        "#!/bin/sh\nset -eu\nprintf child > \"$ALMIGHTY_CHILD_MARKER\"\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    // SAFETY: this test owns every child process it creates.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    let executor = WorkspaceSwapExecutor {
        inner: runner,
        workspace: workspace.clone(),
        moved_workspace: moved_workspace.clone(),
    };
    let client = JjClient::new(
        &executor,
        program,
        &config,
        Box::new([(
            OsString::from("ALMIGHTY_CHILD_MARKER"),
            child_marker.as_os_str().to_owned(),
        )]),
    );

    assert!(matches!(
        client.fetch(),
        Err(JjError::Command(error))
            if matches!(*error, CommandError::RepositoryIdentityChanged)
    ));
    assert!(!child_marker.exists());

    drop(client);
    drop(config);
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(moved_workspace).unwrap();
    fs::remove_dir_all(fake_root).unwrap();
}

#[test]
fn metadata_swap_after_spec_validation_cannot_reach_a_mutating_child() {
    let workspace = workspace("jj-boundary-replacement");
    let moved_jj = workspace.join(".jj-retained");
    let config = resolve(&workspace);
    let fake_root = unique_directory("jj-boundary-replacement-command");
    let program = fake_root.join("jj");
    fs::write(
        &program,
        "#!/bin/sh\nset -eu\nprintf mutation > .jj/mutation-marker\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    // SAFETY: this test owns every child process it creates.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    let executor = MetadataSwapExecutor::new(runner, workspace.clone(), moved_jj.clone());
    let client = JjClient::new(&executor, program, &config, Box::new([]));

    assert!(matches!(
        client.fetch(),
        Err(JjError::Command(error))
            if matches!(*error, CommandError::RepositoryIdentityChanged)
    ));
    assert!(!workspace.join(".jj/mutation-marker").exists());
    assert!(!moved_jj.join("mutation-marker").exists());

    drop(client);
    drop(config);
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(fake_root).unwrap();
}

#[test]
fn synthetic_metadata_pointer_swap_after_validation_cannot_spawn_or_mutate() {
    let workspace = workspace("command-metadata-pointer-replacement");
    let config = resolve(&workspace);
    let command_workspace = workspace.join(".jj/almighty-push/jj-command-workspace");
    let pointer = command_workspace.join(".jj");
    let moved_pointer = command_workspace.join(".jj-retained");
    // The safe boundary has no synthetic metadata pointer. While this bridge
    // exists, replacing it after spec validation must still fail before spawn.
    let pointer_metadata = match fs::symlink_metadata(&pointer) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            assert!(!workspace.join(".jj/almighty-push").exists());
            drop(config);
            fs::remove_dir_all(workspace).unwrap();
            return;
        }
        Err(error) => panic!("metadata pointer inspection failed: {error}"),
    };
    assert!(pointer_metadata.file_type().is_symlink());

    let fake_root = unique_directory("command-metadata-pointer-replacement-command");
    let child_marker = fake_root.join("child-spawned");
    let program = fake_root.join("jj");
    fs::write(
        &program,
        "#!/bin/sh\nset -eu\nprintf child > \"$ALMIGHTY_CHILD_MARKER\"\nprintf mutation > .jj/mutation-marker\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    // SAFETY: this test owns every child process it creates.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    let executor = CommandMetadataPointerSwapExecutor {
        inner: runner,
        pointer: pointer.clone(),
        moved_pointer,
    };
    let client = JjClient::new(
        &executor,
        program,
        &config,
        Box::new([(
            OsString::from("ALMIGHTY_CHILD_MARKER"),
            child_marker.as_os_str().to_owned(),
        )]),
    );

    assert!(matches!(
        client.fetch(),
        Err(JjError::Command(error))
            if matches!(*error, CommandError::RepositoryIdentityChanged)
    ));
    assert!(!child_marker.exists());
    assert!(!pointer.join("mutation-marker").exists());

    drop(client);
    drop(config);
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(fake_root).unwrap();
}

#[test]
fn adapter_commands_execute_from_the_retained_workspace_directory() {
    let workspace = workspace("retained-workspace-cwd");
    let config = resolve(&workspace);
    let fake_root = unique_directory("retained-workspace-cwd-command");
    let cwd_output = fake_root.join("cwd");
    let program = fake_root.join("jj");
    fs::write(
        &program,
        "#!/bin/sh\nset -eu\npwd -P > \"$ALMIGHTY_CWD_OUTPUT\"\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    // SAFETY: this test owns every child process it creates.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    let client = JjClient::new(
        &runner,
        program,
        &config,
        Box::new([(
            OsString::from("ALMIGHTY_CWD_OUTPUT"),
            cwd_output.as_os_str().to_owned(),
        )]),
    );

    client.fetch().unwrap();
    assert_eq!(
        Path::new(fs::read_to_string(&cwd_output).unwrap().trim()),
        workspace
    );

    drop(client);
    drop(config);
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(fake_root).unwrap();
}

#[test]
fn identity_drift_during_a_command_prevents_success() {
    let workspace = workspace("post-command-identity-drift");
    let config = resolve(&workspace);
    let fake_root = unique_directory("post-command-identity-drift-command");
    let program = fake_root.join("jj");
    fs::write(
        &program,
        "#!/bin/sh\nset -eu\n/bin/mv \"$ALMIGHTY_WORKSPACE/.jj\" \"$ALMIGHTY_WORKSPACE/.jj-after-command\"\n/bin/mkdir \"$ALMIGHTY_WORKSPACE/.jj\"\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    // SAFETY: this test owns every child process it creates.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    let client = JjClient::new(
        &runner,
        program,
        &config,
        Box::new([(
            OsString::from("ALMIGHTY_WORKSPACE"),
            workspace.as_os_str().to_owned(),
        )]),
    );

    assert!(matches!(
        client.fetch(),
        Err(JjError::Command(error))
            if matches!(*error, CommandError::RepositoryIdentityChanged)
    ));

    drop(client);
    drop(config);
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(fake_root).unwrap();
}

#[test]
fn command_failure_and_identity_drift_are_both_preserved() {
    let workspace = workspace("failed-command-identity-drift");
    let config = resolve(&workspace);
    let fake_root = unique_directory("failed-command-identity-drift-command");
    let program = fake_root.join("jj");
    fs::write(
        &program,
        "#!/bin/sh\nset -eu\n/bin/mv \"$ALMIGHTY_WORKSPACE/.jj\" \"$ALMIGHTY_WORKSPACE/.jj-after-command\"\n/bin/mkdir \"$ALMIGHTY_WORKSPACE/.jj\"\nexit 23\n",
    )
    .unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    // SAFETY: this test owns every child process it creates.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    let client = JjClient::new(
        &runner,
        program,
        &config,
        Box::new([(
            OsString::from("ALMIGHTY_WORKSPACE"),
            workspace.as_os_str().to_owned(),
        )]),
    );

    assert!(matches!(
        client.fetch(),
        Err(JjError::Command(error))
            if matches!(
                error.as_ref(),
                CommandError::CommandAndRepositoryIdentityChanged { command }
                    if matches!(command.as_ref(), CommandError::Exit { status_code: Some(23), .. })
            )
    ));

    drop(client);
    drop(config);
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(fake_root).unwrap();
}

#[test]
fn push_rejects_remote_drift_and_recovers_a_lost_success_by_exact_postcondition() {
    let drift_workspace = workspace("push-drift");
    let drift_config = resolve_locked(&drift_workspace);
    let head = HeadRef::owned(&ChangeId::parse(ROOT_CHANGE).unwrap()).unwrap();
    let effect = JjEffect::PushHead {
        ownership: PrOwnership::generated(ChangeId::parse(ROOT_CHANGE).unwrap()).unwrap(),
        expected: RemoteRefState::Absent,
        desired: CommitId::parse(ROOT_COMMIT).unwrap(),
    };
    let drift_fake = FakeJj::new(
        "push-drift-command",
        [reply(remote_row(head.as_str(), OTHER_COMMIT))],
    );
    // SAFETY: this test owns every child process it creates.
    let drift_executor =
        RecordingExecutor::new(unsafe { CommandRunner::assume_process_child_authority() });
    let drift_store = StateStore::from_config(&drift_config).unwrap();
    let drift_plan = Plan::new(
        drift_config.scope().clone(),
        vec![Effect::Jj(effect.clone())].into_boxed_slice(),
        limits(),
    )
    .unwrap();
    let mut drift_client = JjClient::new(
        &drift_executor,
        drift_fake.program.clone(),
        &drift_config,
        drift_fake.environment(),
    );
    assert!(matches!(
        Executor::execute_stage(&drift_store, drift_plan, &mut drift_client),
        Err(ExecutorError::Drift { effect_index: 0 })
    ));
    assert_eq!(drift_executor.calls().len(), 1);
    fs::remove_dir_all(drift_workspace).unwrap();

    let recovery_workspace = workspace("push-recovery");
    let recovery_config = resolve_locked(&recovery_workspace);
    let recovery_fake = FakeJj::new(
        "push-recovery-command",
        [
            reply(""),
            reply(jsonl([row(ROOT_CHANGE, ROOT_COMMIT, "local")])),
            reply(""),
            reply(jsonl([row(ROOT_CHANGE, ROOT_COMMIT, "local")])),
            failed_reply(1),
            reply(remote_row(head.as_str(), ROOT_COMMIT)),
        ],
    );
    // SAFETY: this test owns every child process it creates.
    let recovery_runner = unsafe { CommandRunner::assume_process_child_authority() };
    let recovery_store = StateStore::from_config(&recovery_config).unwrap();
    let recovery_plan = Plan::new(
        recovery_config.scope().clone(),
        vec![Effect::Jj(effect)].into_boxed_slice(),
        limits(),
    )
    .unwrap();
    let mut recovery_client = JjClient::new(
        &recovery_runner,
        recovery_fake.program.clone(),
        &recovery_config,
        recovery_fake.environment(),
    );
    assert_eq!(
        Executor::execute_stage(&recovery_store, recovery_plan, &mut recovery_client).unwrap(),
        StageOutcome::Complete
    );
    fs::remove_dir_all(recovery_workspace).unwrap();
}

#[test]
fn executor_uses_the_jj_client_sealed_check_execute_reobserve_path() {
    let workspace = workspace("executor-push");
    let config = resolve(&workspace);
    let change_id = ChangeId::parse(ROOT_CHANGE).unwrap();
    let head = HeadRef::owned(&change_id).unwrap();
    let effect = Effect::Jj(JjEffect::PushHead {
        ownership: PrOwnership::generated(change_id).unwrap(),
        expected: RemoteRefState::Absent,
        desired: CommitId::parse(ROOT_COMMIT).unwrap(),
    });
    let fake = FakeJj::new(
        "executor-push-command",
        [
            reply(""),
            reply(jsonl([row(ROOT_CHANGE, ROOT_COMMIT, "local")])),
            reply(""),
            reply(jsonl([row(ROOT_CHANGE, ROOT_COMMIT, "local")])),
            reply("pushed"),
            reply(remote_row(head.as_str(), ROOT_COMMIT)),
            reply(remote_row(head.as_str(), ROOT_COMMIT)),
            reply(remote_row(head.as_str(), ROOT_COMMIT)),
        ],
    );
    let store = StateStore::open(&workspace, config.scope().clone(), limits()).unwrap();
    let plan = Plan::new(
        config.scope().clone(),
        vec![effect].into_boxed_slice(),
        limits(),
    )
    .unwrap();
    // SAFETY: this test owns every child process it creates.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    let mut driver = JjClient::new(&runner, fake.program.clone(), &config, fake.environment());

    assert_eq!(
        Executor::execute_stage(&store, plan, &mut driver).unwrap(),
        StageOutcome::Complete
    );
    assert_eq!(
        driver
            .observe_remote_heads(std::slice::from_ref(&head))
            .unwrap()[&head],
        RemoteRefState::At(CommitId::parse(ROOT_COMMIT).unwrap())
    );
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn typed_rebase_checks_exact_source_and_parent_then_returns_the_new_commit() {
    let workspace = workspace("rebase");
    let config = resolve_locked(&workspace);
    let rebased_commit = "4444444444444444444444444444444444444444";
    let effect = JjEffect::Rebase {
        change_id: ChangeId::parse(CHILD_CHANGE).unwrap(),
        expected_commit: CommitId::parse(CHILD_COMMIT).unwrap(),
        expected_parent: CommitId::parse(ROOT_COMMIT).unwrap(),
        desired_parent: CommitId::parse(OTHER_COMMIT).unwrap(),
    };
    let fake = FakeJj::new(
        "rebase-command",
        [
            reply(jsonl([row(CHILD_CHANGE, CHILD_COMMIT, "child")])),
            reply(parent_observation(
                row(CHILD_CHANGE, CHILD_COMMIT, "child"),
                [row(ROOT_CHANGE, ROOT_COMMIT, "old parent")],
            )),
            reply(""),
            reply(jsonl([row(OTHER_CHANGE, OTHER_COMMIT, "destination")])),
            reply(jsonl([row(CHILD_CHANGE, CHILD_COMMIT, "child")])),
            reply(parent_observation(
                row(CHILD_CHANGE, CHILD_COMMIT, "child"),
                [row(ROOT_CHANGE, ROOT_COMMIT, "old parent")],
            )),
            reply(""),
            reply(jsonl([row(OTHER_CHANGE, OTHER_COMMIT, "destination")])),
            reply("rebased"),
            reply(jsonl([row(CHILD_CHANGE, rebased_commit, "child")])),
            reply(parent_observation(
                row(CHILD_CHANGE, rebased_commit, "child"),
                [row(OTHER_CHANGE, OTHER_COMMIT, "destination")],
            )),
            reply(""),
        ],
    );
    // SAFETY: this test owns every child process it creates.
    let executor =
        RecordingExecutor::new(unsafe { CommandRunner::assume_process_child_authority() });
    let mut client = JjClient::new(&executor, fake.program.clone(), &config, fake.environment());
    let store = StateStore::from_config(&config).unwrap();
    let plan = Plan::new(
        config.scope().clone(),
        vec![
            Effect::Jj(effect),
            Effect::Reobserve(ReobserveBarrier::LocalHistory),
        ]
        .into_boxed_slice(),
        limits(),
    )
    .unwrap();

    assert_eq!(
        Executor::execute_stage(&store, plan, &mut client).unwrap(),
        StageOutcome::Reobserve(ReobserveBarrier::LocalHistory)
    );
    let calls = executor.calls();
    assert_eq!(
        calls[8],
        os_args([
            "--ignore-working-copy",
            "rebase",
            "--source",
            &format!("commit_id(\"{CHILD_COMMIT}\")"),
            "--onto",
            &format!("commit_id(\"{OTHER_COMMIT}\")"),
        ])
    );
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn fixture_cleanup_failure_is_observable_and_retryable() {
    let mut fixture = TemporaryDirectory::new_with_cleanup_failure("cleanup-failure");
    let path = fixture.path().to_owned();
    fs::write(path.join("owned"), "fixture").unwrap();

    let error = fixture.cleanup().unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert!(error
        .to_string()
        .contains("injected fixture cleanup failure"));
    assert!(path.exists());

    fixture.cleanup().unwrap();
    assert!(!path.exists());

    let unwinding_fixture = TemporaryDirectory::new_with_cleanup_failure("cleanup-unwind");
    let unwinding_path = unwinding_fixture.path().to_owned();
    let primary = std::panic::catch_unwind(move || {
        let _fixture = unwinding_fixture;
        panic!("primary fixture failure");
    })
    .unwrap_err();
    assert_eq!(
        primary.downcast_ref::<&str>(),
        Some(&"primary fixture failure")
    );
    assert!(unwinding_path.exists());
    remove_fixture_tree(&unwinding_path).unwrap();
    assert!(!unwinding_path.exists());
}

#[test]
fn real_jj_named_push_ignores_the_configured_bookmark_template() {
    let Some(jj_program) = executable_on_path("jj") else {
        eprintln!("skipping real-jj test: jj executable not found on PATH");
        return;
    };
    let fixture = TemporaryDirectory::new("real-jj");
    let root = fixture.path();
    let remote = root.join("remote");
    let source = root.join("source");
    let environment = isolated_jj_environment(root);
    // SAFETY: this test routes every child process it creates through this runner.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };

    run_jj(
        &runner,
        &jj_program,
        root,
        &environment,
        ["git", "init", "--colocate", remote.to_str().unwrap()],
    );
    run_jj(
        &runner,
        &jj_program,
        root,
        &environment,
        ["git", "init", "--no-colocate", source.to_str().unwrap()],
    );
    run_jj(
        &runner,
        &jj_program,
        &source,
        &environment,
        ["config", "set", "--repo", "user.name", "Adapter Test"],
    );
    run_jj(
        &runner,
        &jj_program,
        &source,
        &environment,
        [
            "config",
            "set",
            "--repo",
            "user.email",
            "adapter@example.invalid",
        ],
    );
    run_jj(
        &runner,
        &jj_program,
        &source,
        &environment,
        ["git", "remote", "add", "origin", remote.to_str().unwrap()],
    );
    run_jj(
        &runner,
        &jj_program,
        &source,
        &environment,
        ["describe", "--message", "real adapter change"],
    );
    run_jj(
        &runner,
        &jj_program,
        &source,
        &environment,
        [
            "config",
            "set",
            "--repo",
            "templates.git_push_bookmark",
            "\"\\\"configured-\\\" ++ change_id.short()\"",
        ],
    );
    let observed = run_jj_output(
        &runner,
        &jj_program,
        &source,
        &environment,
        [
            "--ignore-working-copy",
            "log",
            "--no-graph",
            "--revision",
            "@",
            "--limit",
            "1",
            "--template",
            "json(self) ++ \"\\n\"",
        ],
    );
    let value: Value = serde_json::from_str(observed.trim()).unwrap();
    let change_id = ChangeId::parse(value["change_id"].as_str().unwrap()).unwrap();
    let commit_id = CommitId::parse(value["commit_id"].as_str().unwrap()).unwrap();
    let head = HeadRef::owned(&change_id).unwrap();

    let config = resolve_locked(&source);
    let mut client = JjClient::new(&runner, jj_program.clone(), &config, environment.clone());
    let effect = JjEffect::PushHead {
        ownership: PrOwnership::generated(change_id.clone()).unwrap(),
        expected: RemoteRefState::Absent,
        desired: commit_id.clone(),
    };
    assert_eq!(
        client
            .observe_remote_heads(std::slice::from_ref(&head))
            .unwrap()[&head],
        RemoteRefState::Absent
    );
    let store = StateStore::from_config(&config).unwrap();
    let plan = Plan::new(
        config.scope().clone(),
        vec![Effect::Jj(effect)].into_boxed_slice(),
        limits(),
    )
    .unwrap();
    assert_eq!(
        Executor::execute_stage(&store, plan, &mut client).unwrap(),
        StageOutcome::Complete
    );
    assert_eq!(
        client
            .observe_remote_heads(std::slice::from_ref(&head))
            .unwrap()[&head],
        RemoteRefState::At(commit_id)
    );

    let bookmarks = run_jj_output(
        &runner,
        &jj_program,
        &source,
        &environment,
        [
            "--ignore-working-copy",
            "bookmark",
            "list",
            "--remote",
            "exact:origin",
            "--template",
            "if(remote, json(self) ++ \"\\n\")",
        ],
    );
    let rows = bookmarks
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"].as_str(), Some(head.as_str()));
    assert_eq!(rows[0]["remote"].as_str(), Some("origin"));

    drop(client);
    drop(config);
    fixture.close().unwrap();
}

fn os_args<const N: usize>(args: [&str; N]) -> Vec<OsString> {
    args.into_iter().map(OsString::from).collect()
}

fn isolated_jj_environment(root: &Path) -> Box<[(OsString, OsString)]> {
    let home = root.join("home");
    let config_home = root.join("config");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&config_home).unwrap();
    Box::new([
        (OsString::from("HOME"), home.into_os_string()),
        (
            OsString::from("XDG_CONFIG_HOME"),
            config_home.into_os_string(),
        ),
        (OsString::from("NO_COLOR"), OsString::from("1")),
        (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
        (OsString::from("USER"), OsString::from("almighty-test")),
    ])
}

fn run_jj<const N: usize>(
    runner: &CommandRunner,
    program: &Path,
    cwd: &Path,
    environment: &[(OsString, OsString)],
    args: [&str; N],
) {
    drop(run_jj_output(runner, program, cwd, environment, args));
}

fn run_jj_output<const N: usize>(
    runner: &CommandRunner,
    program: &Path,
    cwd: &Path,
    environment: &[(OsString, OsString)],
    args: [&str; N],
) -> String {
    let rendered_args = args.join(" ");
    let spec = CommandSpec::new(
        program.as_os_str().to_owned(),
        args.into_iter().map(OsString::from).collect(),
        cwd.to_owned(),
        Box::new([]),
        environment.to_vec().into_boxed_slice(),
        limits(),
    )
    .unwrap();
    match runner.run(&spec) {
        Ok(output) => output.stdout,
        Err(error) => panic!(
            "isolated jj command {rendered_args:?} failed: {error}; bounded output: {}",
            bounded_command_output(&error)
        ),
    }
}

fn bounded_command_output(error: &CommandError) -> String {
    let exit = match error {
        CommandError::Exit { stdout, stderr, .. } => Some((stdout, stderr)),
        CommandError::Cleanup { outcome, .. } => match outcome.as_ref() {
            almighty_push::command::CommandOutcome::Failure(primary) => match primary.as_ref() {
                CommandError::Exit { stdout, stderr, .. } => Some((stdout, stderr)),
                _ => None,
            },
            almighty_push::command::CommandOutcome::Success(output) => {
                Some((&output.stdout, &output.stderr))
            }
        },
        _ => None,
    };
    exit.map_or_else(
        || "unavailable".to_owned(),
        |(stdout, stderr)| format!("stdout={stdout:?}, stderr={stderr:?}"),
    )
}

const FIXTURE_ENTRY_COUNT_MAX: usize = 4_096;
const FIXTURE_DEPTH_MAX: usize = 16;

struct TemporaryDirectory {
    path: Option<PathBuf>,
    fail_cleanup_once: bool,
}

impl TemporaryDirectory {
    fn new(label: &str) -> Self {
        Self {
            path: Some(unique_directory(label)),
            fail_cleanup_once: false,
        }
    }

    fn new_with_cleanup_failure(label: &str) -> Self {
        Self {
            path: Some(unique_directory(label)),
            fail_cleanup_once: true,
        }
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("fixture is live")
    }

    fn cleanup(&mut self) -> io::Result<()> {
        let Some(path) = self.path.as_ref() else {
            return Ok(());
        };
        if self.fail_cleanup_once {
            self.fail_cleanup_once = false;
            return Err(io::Error::other("injected fixture cleanup failure"));
        }
        remove_fixture_tree(path)?;
        self.path = None;
        Ok(())
    }

    fn close(mut self) -> io::Result<()> {
        self.cleanup()
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if let Err(error) = self.cleanup() {
            let fixture = self
                .path
                .as_deref()
                .and_then(Path::file_name)
                .and_then(OsStr::to_str)
                .unwrap_or("unprintable-fixture");
            // Reporting is best-effort because panicking from Drop while the
            // test is already unwinding would erase the primary failure.
            drop(writeln!(
                io::stderr().lock(),
                "fixture cleanup failed for {fixture:?}: {error}"
            ));
        }
    }
}

fn remove_fixture_tree(root: &Path) -> io::Result<()> {
    let root_metadata = fs::symlink_metadata(root)?;
    if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
        return Err(io::Error::other("fixture root is not a real directory"));
    }

    let mut pending = vec![(root.to_owned(), 0_usize)];
    let mut directories = Vec::new();
    let mut entry_count = 0_usize;
    while let Some((directory, depth)) = pending.pop() {
        if depth > FIXTURE_DEPTH_MAX {
            return Err(io::Error::other("fixture cleanup depth bound exceeded"));
        }
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        directories.push(directory.clone());
        for entry in fs::read_dir(&directory)? {
            entry_count = entry_count
                .checked_add(1)
                .ok_or_else(|| io::Error::other("fixture entry count overflow"))?;
            if entry_count > FIXTURE_ENTRY_COUNT_MAX {
                return Err(io::Error::other("fixture cleanup entry bound exceeded"));
            }
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                pending.push((path, depth + 1));
            } else if metadata.is_file() || metadata.file_type().is_symlink() {
                fs::remove_file(path)?;
            } else {
                return Err(io::Error::other("fixture contains an unexpected file type"));
            }
        }
    }
    for directory in directories.into_iter().rev() {
        fs::remove_dir(directory)?;
    }
    Ok(())
}

fn executable_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let current_directory = std::env::current_dir().expect("test process has a current directory");
    std::env::split_paths(&path).find_map(|directory| {
        let candidate = directory.join(name);
        if !candidate.is_file() {
            return None;
        }
        Some(if candidate.is_absolute() {
            candidate
        } else {
            current_directory.join(candidate)
        })
    })
}

#[derive(Debug, Eq, PartialEq)]
enum RepositoryEntry {
    Directory,
    File(Box<[u8]>),
    Symlink(PathBuf),
}

fn snapshot_repository(root: &Path) -> BTreeMap<PathBuf, RepositoryEntry> {
    const ENTRY_COUNT_MAX: usize = 128;
    const DEPTH_MAX: usize = 8;
    const FILE_BYTES_MAX: u64 = 65_536;

    let mut entries = BTreeMap::new();
    let mut pending = vec![(root.to_owned(), 0_usize)];
    while let Some((directory, depth)) = pending.pop() {
        assert!(
            depth <= DEPTH_MAX,
            "repository snapshot exceeded depth bound"
        );
        for entry in fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            let metadata = fs::symlink_metadata(&path).unwrap();
            let value = if metadata.file_type().is_symlink() {
                RepositoryEntry::Symlink(fs::read_link(&path).unwrap())
            } else if metadata.is_dir() {
                pending.push((path, depth + 1));
                RepositoryEntry::Directory
            } else {
                assert!(
                    metadata.is_file(),
                    "repository contains unexpected file type"
                );
                assert!(
                    metadata.len() <= FILE_BYTES_MAX,
                    "repository snapshot exceeded file bound"
                );
                RepositoryEntry::File(fs::read(path).unwrap().into_boxed_slice())
            };
            assert!(entries.insert(relative, value).is_none());
            assert!(
                entries.len() <= ENTRY_COUNT_MAX,
                "repository snapshot exceeded entry bound"
            );
        }
    }
    entries
}

fn unique_directory(label: &str) -> PathBuf {
    for sequence in 0..1_000 {
        let path = std::env::temp_dir().join(format!(
            "almighty-push-jj-{label}-{}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return path.canonicalize().unwrap(),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("fixture creation failed: {error}"),
        }
    }
    panic!("fixture attempts exhausted")
}
