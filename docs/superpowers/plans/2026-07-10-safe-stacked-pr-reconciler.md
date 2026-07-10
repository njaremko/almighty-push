# Safe Stacked PR Reconciler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the current fail-open prototype with a bounded, typed,
repository-scoped reconciler that safely maintains jj-backed GitHub PR stacks and
their bases.

**Architecture:** A pure planner consumes validated local, GitHub, remote-ref, and
persisted observations and produces a deterministic bounded plan. Narrow jj, gh,
filesystem, clock, and lock adapters execute typed effects with explicit
preconditions, postconditions, timeouts, and durable checkpoints; mutation barriers
force re-observation after a local rebase. The old `src/main.rs` implementation is
deleted when the new CLI becomes the only runtime path.

**Tech Stack:** Rust 2021 on macOS/Linux, Cargo, `clap`, `serde`, `serde_json`,
`anyhow`, `fs2`, jj 0.41+ CLI, GitHub `gh` CLI/REST API, and deterministic
fake-command integration tests. `fs2` is the one new runtime dependency: OS-backed
file locking is required so crashed processes release exclusion without unsafe PID
or age guessing; its license and transitive tree are recorded with the change.

## Global Constraints

- Use `jj`, never raw `git`, for repository operations.
- Safety, then performance, then developer experience.
- Initial managed identity is `(source_repository_id, full_jj_change_id)`; commit
  IDs, prefixes, prose, mutable heads, and scraped PR numbers are not identity.
- A distinct target repository on the same GitHub host is supported as a fork PR:
  owned refs live in the source repository, PR numbers/bases live in the target,
  and every observed PR head repository must equal the configured source.
- The selected induced graph must be one bounded parent-child chain; reject merge
  nodes, forks, disconnected components, duplicate identities, and ambiguity.
- Default bounds: 64 selected changes, 10 GitHub pages of 100 PRs, 512 effects,
  4 MiB command output, 4 MiB state, 64 KiB PR body, 30-second commands, and a
  5-second lock wait. Every CLI override remains within a compiled maximum.
- `--dry-run` creates no lock or file, performs no fetch/rebase/ref/PR/state
  mutation, and renders the same logical plan including re-observation barriers.
- `--no-pr` executes no `gh` process and preserves ownership/lifecycle state.
- External failure, timeout, malformed output, incomplete pagination, ambiguity,
  drift, and lost responses fail closed.
- Persisted compatibility exists only as one explicit v2-to-v3 migration. The old
  runtime and old state write path are deleted after migration is implemented.
- No live GitHub mutations are used for tests.
- Every Rust task uses a red-green test cycle and ends with the full crate suite,
  formatter, check, and Clippy passing before its jj commit.

## Resource sketch

For a stack of `N <= 64` and at most `P <= 1,000` observed PRs:

- Local parsing and chain validation are `O(N log N)` time and `O(N)` memory.
- GitHub observation uses at most 10 PR pages plus one repository and one matching
  refs request per observation epoch.
- Planning is `O((N + P) log(N + P))`, bounded by 512 effects.
- Execution is sequential because later effects depend on earlier external
  postconditions; worst-case mutation traffic is `O(N + stale_owned_prs)`.
- Stack-section generation is bounded `O(N^2)` text across all PRs, at most 64 KiB
  per body, and unchanged bodies are not written.
- State and checkpoint serialization are capped at 4 MiB and replaced atomically.

---

### Task 1: Precise domain model and selected-chain laws

**Files:**
- Create: `src/lib.rs`
- Create: `src/domain.rs`
- Create: `tests/domain_model.rs`
- Modify: `src/main.rs` only for the five existing Clippy diagnostics, preserving
  runtime behavior until Task 9 deletes the prototype

**Interfaces:**
- Produces: `RepositoryId`, `ChangeId`, `CommitId`, `HeadRef`, `PrNumber`,
  `PrLifecycle`, `Revision`, `SelectedChain`, `Scope`, `Limits`, and
  `DomainError`.
- `SelectedChain::new(scope, revisions, limits) -> Result<Self, DomainError>` is
  the only constructor for an executable stack.

- [ ] **Step 1: Write compile-failing domain tests**

Create `tests/domain_model.rs` with table-driven tests that construct exact IDs,
accept zero/one/linear-many revisions, and reject duplicate IDs, merge nodes,
forks, disconnected revisions, ambiguous roots, and 65 revisions. Use observable
error variants rather than message strings:

```rust
use almighty_push::domain::{
    ChangeId, CommitId, DomainError, HeadRef, Limits, RepositoryId, Revision,
    Scope, SelectedChain,
};

fn revision(id: &str, commit: &str, parents: &[&str]) -> Revision {
    Revision::new(
        ChangeId::parse(id).unwrap(),
        CommitId::parse(commit).unwrap(),
        format!("change {id}"),
        parents
            .iter()
            .map(|parent| ChangeId::parse(parent).unwrap())
            .collect::<Box<[_]>>(),
    )
    .unwrap()
}

#[test]
fn selected_chain_rejects_a_merge_node() {
    let repository = RepositoryId::parse("github.com/owner/project").unwrap();
    let scope = Scope::new(
        repository.clone(),
        repository,
        "origin".to_owned(),
        HeadRef::parse("main").unwrap(),
        "@".to_owned(),
    )
    .unwrap();
    let revisions = Box::new([
        revision(
            "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk",
            "1111111111111111111111111111111111111111",
            &[],
        ),
        revision(
            "llllllllllllllllllllllllllllllll",
            "2222222222222222222222222222222222222222",
            &[
                "kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk",
                "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm",
            ],
        ),
    ]);

    assert_eq!(
        SelectedChain::new(scope, revisions, Limits::default()).unwrap_err(),
        DomainError::SelectedMerge {
            change_id: ChangeId::parse("llllllllllllllllllllllllllllllll").unwrap(),
        }
    );
}
```

- [ ] **Step 2: Run the domain test and observe the expected compile failure**

Run: `cargo test --test domain_model`

Expected: failure because `almighty_push::domain` does not exist.

- [ ] **Step 3: Implement newtypes, lifecycle enums, bounds, and chain validation**

Create `src/domain.rs` with private string fields, validating constructors, and a
single-pass parent/child relation check. Use `Box<[T]>` for immutable observations
and `BTreeMap`/`BTreeSet` for deterministic indexes. The central types are:

```rust
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct ChangeId(String);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct CommitId(String);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct HeadRef(String);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct PrNumber(u64);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct RepositoryId {
    host: String,
    owner: String,
    name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PrLifecycle {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Revision {
    change_id: ChangeId,
    commit_id: CommitId,
    description: String,
    parent_change_ids: Box<[ChangeId]>,
    conflict: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedChain {
    scope: Scope,
    revisions_base_to_tip: Box<[Revision]>,
}
```

`ChangeId::parse` accepts exactly 32 jj reverse-hex letters in `k..=z`; `CommitId`
accepts exactly 40 lowercase hexadecimal characters; `HeadRef` rejects empty,
leading/trailing slash, `..`, control characters, spaces, and Git-invalid endings.
`SelectedChain::new` asserts that each non-root revision has exactly one selected
parent and that every revision except the tip has exactly one selected child.

- [ ] **Step 4: Export the domain and run exhaustive small-chain tests**

Create `src/lib.rs` containing `pub mod domain;`. Extend the test to enumerate all
parent assignments for up to four revisions and assert that acceptance occurs iff
the graph is empty or one unique chain. In `src/main.rs`, remove the redundant
single-component imports, construct the PR-number regex once outside its loop, use
`next_back()` for the URL segment, and accept `&mut [Revision]` in
`handle_merged_prs`; these mechanical changes remove the five baseline Clippy
failures without changing behavior.

Run: `cargo test --test domain_model`

Expected: all domain tests pass.

- [ ] **Step 5: Run repository checks and commit**

Run:

```bash
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
jj commit -m $'Model selected stacks with exact identities\n\nValidate repository scope, full change and commit IDs, bounded resources, and a unique linear selected chain.'
```

Expected: all commands before `jj commit` exit zero.

---

### Task 2: Bounded subprocess boundary

**Files:**
- Create: `src/command.rs`
- Create: `tests/command_runner.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Limits`.
- Produces: `CommandRunner::run(&CommandSpec) -> Result<CommandOutput,
  CommandError>`, with distinct spawn, timeout, output-limit, exit-status, and
  UTF-8 failures.

- [ ] **Step 1: Write failing executable-boundary tests**

Create macOS/Linux tests using temporary shell scripts that produce stdout,
stderr, nonzero status, output beyond the cap, and a sleep beyond the timeout.
Assert structured variants and that timeout/output-limit children are killed and
reaped:

```rust
#[test]
fn nonzero_exit_is_never_an_empty_success() {
    let fixture = ScriptFixture::new("printf 'missing' >&2; exit 23");
    let error = CommandRunner::default()
        .run(&fixture.spec(Duration::from_secs(1), 1024))
        .unwrap_err();

    assert!(matches!(
        error,
        CommandError::Exit {
            status_code: Some(23),
            ..
        }
    ));
}
```

- [ ] **Step 2: Run and observe the missing-module failure**

Run: `cargo test --test command_runner`

Expected: compile failure for `almighty_push::command`.

- [ ] **Step 3: Implement bounded concurrent pipe draining**

Implement `CommandSpec` with program, boxed arguments, canonical CWD, bounded
stdin, timeout, and output limit. Spawn with piped streams; one thread drains each
output into a capped buffer while continuing to drain after the cap. The parent
polls `try_wait` every 10 ms, kills on timeout or either cap flag, joins both
readers, reaps the child, then returns one precise result:

```rust
pub struct CommandSpec {
    pub program: OsString,
    pub args: Box<[OsString]>,
    pub cwd: PathBuf,
    pub stdin: Box<[u8]>,
    pub timeout: Duration,
    pub output_limit_bytes: usize,
}

pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

pub enum CommandError {
    Spawn { program: OsString, source: io::Error },
    Timeout { timeout: Duration },
    OutputLimit { limit_bytes: usize },
    Exit { status_code: Option<i32>, stdout: String, stderr: String },
    Utf8 { stream: OutputStream },
    Io { operation: IoOperation, source: io::Error },
}
```

Reject stdin larger than the command output bound. Keep error stdout/stderr capped.

- [ ] **Step 4: Run the runner tests including timeout and cap controls**

Run: `cargo test --test command_runner`

Expected: all runner tests pass in under five seconds with no surviving fixture
process.

- [ ] **Step 5: Run repository checks and commit**

Run the four global Cargo checks, then:

```bash
jj commit -m $'Bound and type external command execution\n\nDistinguish process failures, enforce time and output limits, and kill and reap commands that exceed their contract.'
```

---

### Task 3: Repository/configuration discovery without ambiguous defaults

**Files:**
- Create: `src/config.rs`
- Create: `tests/config_resolution.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `CommandRunner`, CLI-provided `ConfigInput`.
- Produces: `ResolvedConfig`, canonical workspace root, exact Git remote, parsed
  source/target repository IDs, target base, selection tip/revset, and bounded
  limits.

- [ ] **Step 1: Write failing remote/parser and config tests**

Cover scp, HTTPS, and `ssh://` remotes; same-repository and same-host fork targets;
reject non-GitHub authority containing a `github.com/` path, cross-host PR targets,
remote `original` when `origin` was requested, duplicate remotes, missing base,
invalid host/owner/name, a symlinked `.jj`, and unsafe CLI bounds. Assert `--no-pr`
resolution does not execute `gh`.

- [ ] **Step 2: Run and observe the missing configuration API**

Run: `cargo test --test config_resolution`

Expected: compile failure for `almighty_push::config`.

- [ ] **Step 3: Implement strict remote URL and canonical-root resolution**

Define:

```rust
pub struct ConfigInput {
    pub remote: Option<String>,
    pub repository: Option<RepositoryId>,
    pub base: Option<HeadRef>,
    pub tip_revset: String,
    pub limits: Limits,
    pub github_enabled: bool,
}

pub struct ResolvedConfig {
    pub workspace_root: PathBuf,
    pub state_directory: PathBuf,
    pub source_repository: RepositoryId,
    pub target_repository: RepositoryId,
    pub remote: String,
    pub base: HeadRef,
    pub tip_revset: String,
    pub limits: Limits,
}
```

Resolve the root with `jj workspace root`, canonicalize it, verify `.jj` and its
parents are directories rather than symlinks, parse `jj git remote list` into exact
name/URL pairs, and select a remote only when explicitly named or exactly one
remote exists. In GitHub-enabled mode, verify/discover repository and default base
through read-only repository calls. When target differs from source, require the
same GitHub host and preserve both scopes; owned refs are observed/mutated in the
source while PRs and bases are observed/mutated in the target. In `--no-pr` mode,
derive the source repository from the selected remote and require `--base` only if
needed for selection; execute no `gh` process.

- [ ] **Step 4: Add tests for every resolution branch and bound**

Run: `cargo test --test config_resolution`

Expected: exact resolution succeeds; ambiguous or unsafe inputs fail before any
mutation-capable command.

- [ ] **Step 5: Run repository checks and commit**

Run the four global Cargo checks, then commit with:

```bash
jj commit -m $'Resolve repository scope before mutation\n\nValidate the canonical jj workspace, exact remote and GitHub target, configured base and selection, and compiled resource bounds.'
```

---

### Task 4: Atomic namespaced state, ownership-safe lock, and one-time migration

**Files:**
- Create: `src/state.rs`
- Create: `src/lock.rs`
- Create: `tests/state_store.rs`
- Create: `tests/repository_lock.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: canonical workspace root, `Scope`, limits, and a monotonic clock.
- Produces: `StateStore`, `StateV3`, `OwnershipRecord`, `CheckpointState`,
  `RepositoryLock`, and `LegacyCandidate`.

- [ ] **Step 1: Write failing state/lock tests**

Test missing state, exact v3 round-trip, malformed JSON, future schema, wrong scope,
oversized state, symlink/non-file state, interrupted temp write, atomic replacement,
lock contention timeout, automatic release after process exit, and CWD independence.
Add v2 fixtures for
one valid candidate and for key/change/head mismatches that must remain
`LegacyCandidate` until GitHub validation.

- [ ] **Step 2: Run and observe missing state/lock APIs**

Run: `cargo test --test state_store --test repository_lock`

Expected: compile failure for the new modules.

- [ ] **Step 3: Implement explicit persisted states**

Use:

```rust
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StateV3 {
    pub schema_version: StateSchemaVersion,
    pub scope: Scope,
    pub ownership: BTreeMap<ChangeId, OwnershipRecord>,
    pub checkpoint: CheckpointState,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum OwnershipRecord {
    Verified(ManagedPr),
    LegacyCandidate(LegacyCandidate),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CheckpointState {
    Idle,
    Executing(ExecutionCheckpoint),
}
```

Store at `<workspace>/.jj/almighty-push/state-v3.json`. Reject symlinks and
unexpected types for `.jj`, state directory, state file, and temp file. Serialize
into a uniquely create-new temp file in the same directory, enforce 4 MiB before
write, `sync_all`, rename, then sync the directory.

Parse root `.almighty` only when v3 is absent. Convert v2 entries into
`LegacyCandidate` without granting mutation authority. Delete `.almighty` only
after every candidate is validated by the GitHub adapter and a v3 state has been
atomically persisted.

- [ ] **Step 4: Implement OS-released bounded locking**

Validate `<workspace>/.jj/almighty-push/lock` as a regular non-symlink path, open it
without truncation, and use `fs2::FileExt::try_lock_exclusive` every 10 ms for at
most five seconds. Keep the locked file handle inside `RepositoryLock`; Drop unlocks
the handle and never unlinks the path. OS lock release on process death avoids stale
PID files, age guessing, unsafe deletion, and ABA races. Add `fs2` to `Cargo.toml`
and document its MIT/Apache-2.0 licensing, small platform-specific transitive tree,
and blocking/advisory semantics in `CHANGELOG.md` when documentation is rewritten.

- [ ] **Step 5: Run state/lock tests and crash simulations**

Run:

```bash
cargo test --test state_store --test repository_lock
```

Expected: all state and lock cases pass; failed writes leave the previous state
byte-for-byte intact.

- [ ] **Step 6: Run repository checks and commit**

Run the four global Cargo checks, then:

```bash
jj commit -m $'Persist ownership and progress atomically\n\nNamespace state to the canonical workspace, validate paths and schemas, stage legacy migration safely, and use crash-released advisory locking.'
```

---

### Task 5: Structured jj observation and exact owned-head pushing

**Files:**
- Create: `src/jj.rs`
- Create: `tests/jj_adapter.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `ResolvedConfig`, `CommandRunner`, `SelectedChain`, typed effects.
- Produces: `JjClient::observe_chain`, `observe_remote_heads`, `fetch`,
  `rebase`, and `push_named_head`.

- [ ] **Step 1: Write failing fake-jj adapter tests**

Use deterministic fake `jj` executables. Cover full IDs, descriptions containing
`|` and control characters, conflicts, parent maps, empty selections, a fork,
merge node, malformed JSON, duplicate rows, output caps, configurable jj bookmark
templates, exact `--named` pushes, and nonzero failures.

- [ ] **Step 2: Run and observe the missing adapter**

Run: `cargo test --test jj_adapter`

Expected: compile failure for `almighty_push::jj`.

- [ ] **Step 3: Implement structured multi-query observation**

Use `json(self)` for one JSON revision per line, a separate fixed-ID parent query,
and a separate `conflicts()` intersection query. Never parse descriptions with a
custom delimiter. Construct the selection from the configured base remote ref and
tip revset, enforce output/row bounds, join by exact full change ID, and pass the
result through `SelectedChain::new`.

Use explicit effects:

```rust
pub enum JjEffect {
    Fetch { remote: String },
    Rebase {
        source: ChangeId,
        expected_source_commit: CommitId,
        destination_revset: String,
        expected_destination_commit: CommitId,
    },
    PushNamedHead {
        change_id: ChangeId,
        expected_commit: CommitId,
        head_ref: HeadRef,
        expected_remote: RemoteHeadExpectation,
    },
}
```

Push with `jj git push --remote <remote> --named <head>=<change-id>`. Observe before
and after; never use `jj git push --change` or infer the configured generated name.

- [ ] **Step 4: Add a temporary real-jj test for custom bookmark templates**

Create a non-colocated temporary jj repository and local jj-backed remote. Configure
`templates.git_push_bookmark` to a non-default value, execute the adapter's named
push, and assert only the explicitly owned head exists and points to the expected
commit. Do not use raw Git commands or network access.

- [ ] **Step 5: Run adapter tests and commit**

Run the four global Cargo checks plus `cargo test --test jj_adapter`, then:

```bash
jj commit -m $'Observe jj stacks without lossy parsing\n\nJoin structured revisions, exact parent links, and conflict observations, validate one selected chain, and push collision-safe named heads.'
```

---

### Task 6: GitHub observation, ownership, lifecycle, and managed body sections

**Files:**
- Create: `src/github.rs`
- Create: `src/body.rs`
- Create: `tests/github_adapter.rs`
- Create: `tests/managed_body.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `RepositoryId`, `Scope`, `CommandRunner`, limits.
- Produces: bounded `GithubSnapshot`, `ObservedPr`, exact ownership validation,
  and checked create/base/body/lifecycle/head-delete operations.

- [ ] **Step 1: Write failing body and GitHub boundary tests**

Cover body append/replace, duplicate/malformed markers, user-content preservation,
64 KiB bounds, PR pagination ending before the cap, exactly-full final page failure,
duplicate managed heads, malformed JSON, API nonzero exit, lost create response with
postcondition recovery, stale base precondition, close/reopen, and exact head delete.

- [ ] **Step 2: Run and observe missing APIs**

Run: `cargo test --test managed_body --test github_adapter`

Expected: compile failure for `body` and `github`.

- [ ] **Step 3: Implement a bounded versioned managed section**

Use exact delimiters:

```text
<!-- almighty-push:stack:v1:start -->
...
<!-- almighty-push:stack:v1:end -->
```

`ManagedBody::merge(user_body, section, limit)` rejects duplicate, reversed, nested,
or partial delimiters; preserves every byte outside the one managed section; and
returns `Unchanged` when the merged result matches the observation. The section
contains source repository, full change ID, current marker, and the bounded active
stack ordered base-to-tip.

- [ ] **Step 4: Implement complete paged observations and typed effects**

Read repository metadata, matching owned refs, and pull pages with manual
`page=1..=10&per_page=100`. A full tenth page is `ObservationIncomplete`, never a
complete empty/partial set. Parse exact REST JSON into:

```rust
pub struct GithubSnapshot {
    pub source_repository: RepositoryId,
    pub target_repository: RepositoryId,
    pub default_base: HeadRef,
    pub owned_heads: BTreeMap<HeadRef, CommitId>,
    pub prs: Box<[ObservedPr]>,
}

pub struct ObservedPr {
    pub number: PrNumber,
    pub lifecycle: PrLifecycle,
    pub head_repository: RepositoryId,
    pub head_ref: HeadRef,
    pub base_ref: HeadRef,
    pub title: String,
    pub body: String,
    pub head_commit: CommitId,
}
```

Owned refs are listed/deleted in the source repository. PRs are listed/mutated in
the target repository; create sends `head=<source-owner>:<owned-head-ref>` and
validates the returned head repository as well as the ref. All mutations use
`gh api --method` with JSON on stdin. The executor checks the expected old value
immediately before mutation and re-reads the exact PR/ref after
any success, failure, or malformed response. An already-satisfied postcondition is
success; otherwise the original command failure remains fatal.

- [ ] **Step 5: Validate legacy candidates without heuristic runtime matching**

Migration validation requires one exact PR number in the configured repository,
its exact observed head matching the legacy branch, and one exact legacy
`Change ID: \`<full-id>\`` body marker. Ambiguity or mismatch aborts before
mutation.
Successful validation converts the candidate to `Verified`; this parser is used
only during the one-time v2 migration and is deleted from normal reconciliation
flow.

- [ ] **Step 6: Run GitHub/body tests and commit**

Run the four global Cargo checks plus both focused tests, then:

```bash
jj commit -m $'Observe and mutate GitHub with checked ownership\n\nBound pagination, preserve user-authored bodies, validate lifecycle and head ownership, and verify every remote postcondition.'
```

---

### Task 7: Pure deterministic planner and complete stack adjacency

**Files:**
- Create: `src/plan.rs`
- Create: `tests/planner.rs`
- Create: `tests/planner_model.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `SelectedChain`, `GithubSnapshot`, verified `StateV3`, execution mode.
- Produces: bounded `Plan`, typed `Effect`, `ReobserveBarrier`, and `PlanError`.

- [ ] **Step 1: Write failing planner laws and transition tables**

Cover zero/one/many/maximum stacks; create/update/no-op; reorder; open/closed/merged;
interior merged predecessor; exact split (surviving original plus new ID); squash
(disappearing source plus surviving target); deletion; empty-stack cleanup; duplicate
ownership; missing state rebuilt from exact managed refs/markers; body preservation;
`--no-pr`; head deletion; and every effect bound.

Encode these laws as exhaustive small models for chains up to four:

```rust
#[test]
fn desired_bases_are_exact_chain_adjacency() {
    for length in 0..=4 {
        let fixture = PlannerFixture::linear(length);
        let plan = plan(&fixture.input()).unwrap();
        let desired = plan.desired_prs();

        for (index, pr) in desired.iter().enumerate() {
            let expected = if index == 0 {
                fixture.base_ref()
            } else {
                fixture.head_ref(index - 1)
            };
            assert_eq!(pr.base_ref(), &expected);
        }
    }
}
```

- [ ] **Step 2: Run and observe the missing planner**

Run: `cargo test --test planner --test planner_model`

Expected: compile failure for `almighty_push::plan`.

- [ ] **Step 3: Implement desired state and typed effects**

Define:

```rust
pub enum ExecutionMode {
    Full { delete_closed_heads: bool },
    NoPr,
    DryRun { include_prs: bool, delete_closed_heads: bool },
}

pub enum Effect {
    Jj(JjEffect),
    Github(GithubEffect),
    PersistOwnership(PersistedTransition),
    Reobserve(ReobserveBarrier),
}

pub struct Plan {
    pub id: PlanId,
    pub scope: Scope,
    pub effects: Box<[Effect]>,
}
```

Construct owned heads as
`almighty-push/<source-host>/<source-owner>/<source-repo>/<full-change-id>`.
Resolve verified
legacy heads by their exact persisted ref. Derive the whole desired PR relation in
one pass. Existing open PRs receive base/body effects only when observed values
differ; closed exact owned PRs reopen; merged PRs become historical.

For the lowest merged revision with an active child, emit one checked local rebase
of that child onto the merged PR's observed base remote ref, followed by a mandatory
`LocalHistoryChanged` re-observation barrier; emit no later mutation in that concrete
stage. Reject a merged tip with no active child using a precise operator action,
rather than synthesizing history.

Disappearing IDs close their open PRs. Split and squash require no prose heuristic:
the surviving exact ID retains ownership, newly observed IDs create ownership, and
disappearing IDs close ownership.

- [ ] **Step 4: Enforce dry-run/no-pr capability laws**

Assert structurally that a no-PR plan contains no `GithubEffect`; a dry-run plan
contains effect descriptions but no executable mutation capability; and every plan
has at most 512 effects. Plan IDs use a specified FNV-1a hash over canonical JSON,
with test vectors so IDs remain stable across runs.

- [ ] **Step 5: Run planner/model tests and commit**

Run the four global Cargo checks plus focused planner tests, then:

```bash
jj commit -m $'Derive complete PR stacks with a pure planner\n\nCompute exact heads, lifecycle transitions, full base adjacency, bounded effects, and explicit re-observation barriers from validated snapshots.'
```

---

### Task 8: Checked execution and durable resume

**Files:**
- Create: `src/executor.rs`
- Create: `tests/executor.rs`
- Create: `tests/restart_model.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: concrete `Plan`, `StateStore`, `JjClient`, `GithubClient`.
- Produces: `Executor::execute_stage`, durable checkpoints, postcondition recovery,
  drift errors, and `StageOutcome`.

- [ ] **Step 1: Write failing execution/restart tests**

For each effect type, inject failure before command, command nonzero before effect,
lost successful response, postcondition mismatch, checkpoint write failure before
effect, checkpoint write failure after postcondition, process restart at every
index, and external drift. Assert no effect runs without a durable checkpoint and
that retry never duplicates a satisfied effect.

- [ ] **Step 2: Run and observe the missing executor**

Run: `cargo test --test executor --test restart_model`

Expected: compile failure for `almighty_push::executor`.

- [ ] **Step 3: Implement checkpoint-before-effect execution**

Persist:

```rust
pub struct ExecutionCheckpoint {
    pub plan_id: PlanId,
    pub scope: Scope,
    pub effects: Box<[Effect]>,
    pub next_effect_index: usize,
}
```

Before effect zero, atomically write the full bounded concrete plan. Before each
mutation, re-read its current external value and require either the postcondition
(already complete) or precondition (safe to execute). After command completion or
failure, read the postcondition. Persist the incremented index only after the
postcondition holds. Clear the checkpoint atomically after the last effect.

On startup, a matching-scope checkpoint resumes independently of a newly derived
plan. Scope mismatch, malformed checkpoint, invalid index, changed precondition, or
unprovable ownership stops without mutation.

- [ ] **Step 4: Implement re-observation outcomes and strict modes**

`ReobserveBarrier` completes the current checkpoint and returns
`StageOutcome::Reobserve(reason)`. Dry-run uses a renderer and never constructs an
executor or state writer. No-PR execution is parameterized by a backend that has no
GitHub client, making `gh` invocation impossible by type and test.

- [ ] **Step 5: Run restart model and commit**

Run the four global Cargo checks plus focused executor tests, then:

```bash
jj commit -m $'Resume checked effects instead of replaying mutations\n\nPersist bounded plans before execution, validate every precondition and postcondition, and stop safely on external drift.'
```

---

### Task 9: Application orchestration and honest CLI

**Files:**
- Create: `src/app.rs`
- Rewrite: `src/main.rs`
- Create: `tests/cli_safety.rs`
- Create: `tests/cli_workflow.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: all validated adapters/planner/executor APIs.
- Produces: one top-level `run(args) -> Result<RunReport, AppError>` and CLI output
  with machine-readable plan/report options.

- [ ] **Step 1: Write failing end-to-end fake-command regressions**

Port and strengthen the audit fixtures. Assert:

- dry-run creates no lock/state and executes no `jj git fetch`, rebase, push, or
  mutating `gh api`;
- no-PR executes no `gh` process and leaves ownership bytes unchanged;
- empty stack closes exactly owned open PRs in full mode;
- malformed/future/cross-repository state stops before mutation;
- failed observation does not create a PR;
- a pipe in a description cannot alter conflict state;
- post-rebase conflict stops before push;
- partial base edit resumes without flattening or duplicate create;
- user body content is byte-preserved;
- unsupported selected DAGs fail before mutation.

- [ ] **Step 2: Run regressions against the old `main.rs` and record failures**

Run: `cargo test --test cli_safety --test cli_workflow`

Expected: tests compile against the new library but fail while the old binary remains
the runtime entry point.

- [ ] **Step 3: Replace the old runtime completely**

Rewrite `src/main.rs` as a thin parser and call into `app`. CLI fields are explicit:

```rust
#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    remote: Option<String>,
    #[arg(long)]
    repo: Option<String>,
    #[arg(long)]
    base: Option<String>,
    #[arg(long, default_value = "@")]
    tip: String,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    no_pr: bool,
    #[arg(long)]
    delete_branches: bool,
    #[arg(long)]
    json: bool,
    #[arg(short, long)]
    verbose: bool,
}
```

Full execution acquires the repository lock, resumes a checkpoint if present,
fetches only after validated scope, observes, validates conflicts before any other
mutation, plans, checkpoints, executes, and repeats at most four re-observation
barriers. Dry-run performs read-only observation and prints the complete logical
plan without lock/state writer. No-PR resolves without GitHub, pushes named heads,
and preserves PR state.

Delete every old type/function in `src/main.rs`; do not retain fallback or
compatibility execution paths.

- [ ] **Step 4: Implement stable output contracts**

Human output goes to stderr and PR URLs/JSON reports to stdout. Reports distinguish
planned, executed, already-satisfied, drifted, and failed effects. Dry-run visibly
labels mutation-free symbolic effects after a rebase barrier. Errors name the exact
scope, observation/effect, and operator action without printing secrets or complete
user bodies.

- [ ] **Step 5: Run CLI regressions and full suite**

Run:

```bash
cargo test --test cli_safety --test cli_workflow
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Expected: all checks pass and at least one nonzero test count is reported.

- [ ] **Step 6: Commit the sole runtime path**

```bash
jj commit -m $'Replace the prototype with staged reconciliation\n\nMake dry-run mutation-free, isolate no-PR execution, validate conflicts and scope before effects, and drive the typed planner and resumable executor.'
```

---

### Task 10: Full lifecycle, failure matrix, and bounded performance validation

**Files:**
- Create: `tests/support/mod.rs`
- Create: `tests/lifecycle_matrix.rs`
- Create: `tests/failure_matrix.rs`
- Create: `tests/bounds.rs`
- Create: `tests/determinism.rs`
- Modify: earlier tests to share structured fixtures without source assertions

**Interfaces:**
- Consumes: public CLI and library contracts.
- Produces: deterministic whole-feature evidence across every specified transition
  and fail-closed boundary.

- [ ] **Step 1: Build a structured fake environment**

The fixture owns typed local revisions, remote heads, PR rows, state bytes, command
log rows, injected failures, and a simulated clock. Fake executables exchange JSON
files; assertions inspect structured fixture state and process outcomes, never source
or generated command text.

- [ ] **Step 2: Add the lifecycle matrix**

Cover zero, one, 64, and rejected 65 changes; creation; update; no-op idempotence;
reorder; merged first/interior/tip; closed return; split; squash; deletion; empty
cleanup; head deletion; legacy migration success/rejection; and restart after every
effect.

- [ ] **Step 3: Add the complete failure matrix**

Inject spawn, timeout, output cap, nonzero, malformed JSON, truncated page, lost
response, stale precondition, wrong postcondition, state-write failure, and lock
contention at every adapter boundary. Assert no false absence/success and no mutation
past the failed boundary.

- [ ] **Step 4: Add determinism and resource-bound tests**

Permute observation order and assert byte-identical canonical plans/IDs. Validate
maximum body, state, command output, page, effect, barrier, and lock-wait bounds.
Use a test clock so wall-clock sleeps are limited to the command-runner process test.

- [ ] **Step 5: Run the complete validation surface and commit**

Run the four global Cargo checks and all tests, then:

```bash
jj commit -m $'Exercise every reconciliation transition and fault boundary\n\nAdd structured lifecycle, restart, determinism, malformed-observation, partial-effect, and maximum-bound coverage.'
```

---

### Task 11: Documentation, packaging, and CI truthfulness

**Files:**
- Rewrite: `README.md`
- Rewrite: `CHANGELOG.md`
- Delete: `PLAN.md`
- Delete: `ANALYSIS.md`
- Modify: `Cargo.toml`
- Create: `.github/workflows/ci.yml`
- Modify: `.gitignore`
- Modify: `AGENTS.md` only where present-tense hazards are no longer true

**Interfaces:**
- Consumes: verified final CLI and behavior.
- Produces: install/use/recovery documentation, accurate Unreleased changes,
  package metadata, and CI running the repository's four checks.

- [ ] **Step 1: Replace false documentation with current behavior**

README sections must cover installation, prerequisites, explicit/discovered scope,
normal workflow, exact owned refs, base law, dry-run/no-PR guarantees, state path and
schema, migration failure/recovery, limits, merge-tip intervention, output contracts,
and safety. Every command example must run against `--help` syntax.

CHANGELOG must contain only implemented Unreleased behavior and explicitly call out
state v3, owned-ref namespace, removal of heuristic identity/split/squash parsing,
and changed safety semantics. Delete the aspirational PLAN and stale ANALYSIS rather
than preserving conflicting authorities.

- [ ] **Step 2: Complete Cargo package metadata and dependency evidence**

Set MIT license, repository/homepage, readme, keywords, categories, and a tested
Rust version. Remove `chrono` and `regex` when no new module uses them. Retain `fs2`
only for advisory crash-safe file locking and document its dual license, current
maintenance, transitive tree, and platform failure behavior. Run
`cargo tree --depth 2` and record the final direct dependency set and the `fs2`
decision in the changelog.

- [ ] **Step 3: Add bounded CI**

Create `.github/workflows/ci.yml` triggered on pushes and pull requests. It installs
jj 0.41 or newer for the isolated real-jj test; fake GitHub tests do not require a
live `gh` installation or credentials. Then run:

```yaml
- run: cargo fmt --all -- --check
- run: cargo check --all-targets --all-features
- run: cargo clippy --all-targets --all-features -- -D warnings
- run: cargo test --all-targets --all-features
```

Use default Cargo test parallelism.

- [ ] **Step 4: Verify documentation and package behavior**

Run every README non-mutating example, `cargo run -- --help`, `cargo run --
--version`, `cargo metadata --no-deps --format-version 1`, and the four global Cargo
checks. Search tracked files for stale module names, unsupported `--delete`, old
CWD `.almighty` writes, `ignore_errors`, title `(#N)` identity, prefix matching,
and obsolete aspirational claims; reconcile every result.

- [ ] **Step 5: Commit truthful docs and delivery metadata**

```bash
jj commit -m $'Document and continuously verify the safe reconciler\n\nReplace aspirational documents with the implemented contract, complete package metadata, and run all quality gates in CI.'
```

---

### Task 12: Final whole-feature audit and semantic/performance review

**Files:**
- Modify only files required to address review findings.
- Create outside the repository: `/tmp/almighty-push-final-review.md`

**Interfaces:**
- Consumes: final diff, AGENTS target invariants, all test/check evidence.
- Produces: addressed Hickey, algebra-driven, and data-oriented review; clean final
  commit; no obsolete path or known unsupported semantic case.

- [ ] **Step 1: Run the complete local verification from a clean build state**

Run:

```bash
cargo clean
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
cargo run --quiet -- --help
cargo run --quiet -- --version
```

Expected: every command exits zero and tests report nonzero coverage.

- [ ] **Step 2: Run negative source and artifact searches**

Use `rg` and `jj diff --git` to prove there is one runtime path; no
`ignore_errors`, prefix identity, title/operation-prose identity, unbounded external
process, old root-state writes, false transaction claim, normal-runtime legacy
fallback or dual state writer, dead-code allowance, stale PLAN/ANALYSIS authority,
or unsupported delete invocation.

- [ ] **Step 3: Request one bounded post-implementation review workflow**

Provide reviewers the final diff, plan, AGENTS invariants, resource sketch, and test
evidence. Require Hickey complexity, algebra/denotation, and data-oriented
performance lenses plus adversarial correctness. Write the returned report
immediately to `/tmp/almighty-push-final-review.md`.

- [ ] **Step 4: Address every review comment**

For each comment, either change code/tests/docs and rerun the narrow/full checks, or
record an evidence-based non-applicability decision in the review report. No Critical
or Important comment may remain unresolved.

- [ ] **Step 5: Run final verification and commit review fixes**

Run the full commands from Step 1 again, inspect `jj diff --git`, then commit any
review-driven changes:

```bash
jj commit -m $'Close the safe reconciliation review\n\nAddress semantic, fault-model, data-flow, performance-bound, and adversarial findings and record final validation.'
```

Expected: clean working copy, passing checks, and an evidence ledger showing every
AGENTS invariant proven or explicitly bounded.
