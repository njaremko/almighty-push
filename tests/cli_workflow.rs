mod support;

use almighty_push::domain::{HeadRef, LimitValues, Limits, RemoteName, RepositoryId, Scope};
use almighty_push::plan::{Effect, Plan, ReobserveBarrier};
use almighty_push::state::StateStore;
use serde_json::Value;
use std::fs;
use support::{CliFixture, CliFixtureOptions};

fn pending_checkpoint_state() -> Box<[u8]> {
    let root =
        std::env::temp_dir().join(format!("almighty-push-cli-pending-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join(".jj")).unwrap();
    let root = root.canonicalize().unwrap();
    let repository = RepositoryId::parse("github.com/source/project").unwrap();
    let scope = Scope::new(
        repository.clone(),
        repository,
        RemoteName::parse("origin").unwrap(),
        HeadRef::parse("main").unwrap(),
        "@".to_owned(),
    )
    .unwrap();
    let limits = Limits::new(LimitValues::default()).unwrap();
    let store = StateStore::open(&root, scope.clone(), limits).unwrap();
    let mut session = store.lock().unwrap();
    let plan = Plan::new(
        scope,
        vec![Effect::Reobserve(ReobserveBarrier::RemoteRefs)].into_boxed_slice(),
        limits,
    )
    .unwrap();
    session.start_plan(plan).unwrap();
    let bytes = fs::read(store.state_path()).unwrap().into_boxed_slice();
    drop(session);
    fs::remove_dir_all(root).unwrap();
    bytes
}

fn fixture(label: &str) -> CliFixture {
    CliFixture::new(label, CliFixtureOptions::default())
}

#[test]
fn omitted_tip_reports_parent_for_a_fresh_jj_working_copy() {
    let fixture = CliFixture::new(
        "omitted-fresh-working-copy",
        CliFixtureOptions {
            working_copy_empty: true,
            ..CliFixtureOptions::default()
        },
    );

    let output = fixture.run(&["--dry-run", "--no-pr", "--base", "main", "--json"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = CliFixture::json_stdout(&output);
    assert_eq!(report["tip"], "@-");
    let records = fixture.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| record.operation == "qualify-tip")
            .count(),
        1
    );
    assert!(records
        .iter()
        .any(|record| record.operation == "stack-parent"));
    assert!(records.iter().all(|record| !record.lock_path_present));
    assert!(records.iter().all(|record| matches!(
        record.operation.as_str(),
        "read" | "qualify-tip" | "stack-parent"
    )));
    assert!(!fixture.state_directory().exists());
    assert!(!fixture.remote_was_mutated());
    assert!(!fixture.pr_was_mutated());
}

#[test]
fn omitted_fresh_working_copy_persists_parent_tip_in_no_pr_mode() {
    let fixture = CliFixture::new(
        "omitted-fresh-working-copy-no-pr",
        CliFixtureOptions {
            working_copy_empty: true,
            ..CliFixtureOptions::default()
        },
    );

    let output = fixture.run(&["--no-pr", "--base", "main", "--json"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = CliFixture::json_stdout(&output);
    assert_eq!(report["tip"], "@-");
    let state_path = fixture.workspace().join(".jj/almighty-push/state-v3.json");
    let state: Value = serde_json::from_slice(&fs::read(state_path).unwrap()).unwrap();
    assert_eq!(state["scope"]["tip_revset"], "@-");

    let records = fixture.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| record.operation == "qualify-tip")
            .count(),
        1
    );
    assert!(records
        .iter()
        .any(|record| record.operation == "stack-parent"));
    assert!(records.iter().all(|record| record.program == "jj"));
    assert!(records
        .iter()
        .filter(|record| matches!(record.operation.as_str(), "fetch" | "push" | "rebase"))
        .all(|record| record.lock_path_present));
    assert!(fixture.remote_was_mutated());
    assert!(!fixture.pr_was_mutated());
}

#[test]
fn explicit_at_remains_exact_for_a_fresh_jj_working_copy() {
    let fixture = CliFixture::new(
        "explicit-at-fresh-working-copy",
        CliFixtureOptions {
            working_copy_empty: true,
            ..CliFixtureOptions::default()
        },
    );

    let output = fixture.run(&[
        "--dry-run",
        "--no-pr",
        "--base",
        "main",
        "--tip",
        "@",
        "--json",
    ]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = CliFixture::json_stdout(&output);
    assert_eq!(report["tip"], "@");
    let records = fixture.records();
    assert!(records
        .iter()
        .all(|record| record.operation != "qualify-tip"));
    assert!(records.iter().any(|record| record.operation == "stack-at"));
}

#[test]
fn no_pr_executes_only_locked_jj_stages_and_preserves_pr_ownership() {
    let fixture = fixture("no-pr-workflow");

    let output = fixture.run(&["--no-pr", "--base", "main", "--json"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = CliFixture::json_stdout(&output);
    assert_eq!(report["mode"], "no-pr");
    assert_eq!(report["stage_count"], 2);
    assert!(fixture.remote_was_mutated());
    assert!(!fixture.pr_was_mutated());
    let state_path = fixture.workspace().join(".jj/almighty-push/state-v3.json");
    let state: Value = serde_json::from_slice(&fs::read(state_path).unwrap()).unwrap();
    assert_eq!(state["verified"], serde_json::json!({}));
    assert_eq!(state["historic"], serde_json::json!({}));
    assert!(fixture
        .records()
        .iter()
        .all(|record| record.program == "jj"));
    assert!(fixture
        .records()
        .iter()
        .filter(|record| matches!(record.operation.as_str(), "fetch" | "push" | "rebase"))
        .all(|record| record.lock_path_present));
}

#[test]
fn full_mode_locks_before_fetch_observation_and_mutation_then_completes_reobservation() {
    let fixture = fixture("full-workflow");

    let output = fixture.run(&["--base", "main", "--json"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = CliFixture::json_stdout(&output);
    assert_eq!(report["mode"], "full");
    assert_eq!(report["stage_count"], 2);
    assert!(fixture.remote_was_mutated());
    assert!(fixture.pr_was_mutated());
    let state_path = fixture.workspace().join(".jj/almighty-push/state-v3.json");
    let state: Value = serde_json::from_slice(&fs::read(state_path).unwrap()).unwrap();
    assert_eq!(state["schema_version"], 3);
    assert_eq!(
        state["scope"]["source_repository"],
        "github.com/source/project"
    );
    assert_eq!(
        state["scope"]["target_repository"],
        "github.com/source/project"
    );
    assert!(state["verified"]
        .as_object()
        .unwrap()
        .contains_key("kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk"));

    let records = fixture.records();
    let first_fetch = records
        .iter()
        .position(|record| record.operation == "fetch")
        .expect("full mode fetched");
    assert!(records[first_fetch..]
        .iter()
        .all(|record| record.lock_path_present));
    assert!(records.iter().any(|record| record.operation == "gh-mutate"));
}

#[test]
fn dry_run_renders_the_exact_pending_checkpoint_before_new_observation() {
    let options = CliFixtureOptions {
        initial_state: Some(pending_checkpoint_state()),
        ..CliFixtureOptions::default()
    };
    let fixture = CliFixture::new("dry-pending-checkpoint", options);

    let output = fixture.run(&["--dry-run", "--base", "main", "--json"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = CliFixture::json_stdout(&output);
    assert_eq!(report["stage_count"], 1);
    assert_eq!(report["stages"][0]["outcome"], "resume-pending");
    assert_eq!(report["stages"][0]["effect_count"], 1);
    assert!(!fixture.remote_was_mutated());
    assert!(!fixture.pr_was_mutated());
    assert!(fixture.records().iter().all(|record| !matches!(
        record.operation.as_str(),
        "fetch" | "jj-mutate" | "gh-mutate"
    )));
}

#[test]
fn dry_run_full_observes_github_but_never_fetches_locks_or_mutates() {
    let fixture = fixture("dry-run-full");

    let output = fixture.run(&["--dry-run", "--base", "main", "--json"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = CliFixture::json_stdout(&output);
    assert_eq!(report["mode"], "dry-run-full");
    assert_eq!(report["stage_count"], 2);
    assert!(report["stages"][1]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action.as_str().unwrap().contains("CreatePullRequest")));
    assert!(!fixture.state_directory().exists());
    assert!(!fixture.remote_was_mutated());
    assert!(!fixture.pr_was_mutated());
    let records = fixture.records();
    assert!(records.iter().any(|record| record.program == "gh"));
    assert!(records.iter().all(|record| matches!(
        record.operation.as_str(),
        "read" | "qualify-tip" | "stack-at" | "gh-read"
    )));
}

#[test]
fn every_scope_and_output_flag_composes_on_the_full_path() {
    let fixture = fixture("all-full-flags");

    let output = fixture.run(&[
        "--remote",
        "origin",
        "--repo",
        "github.com/source/project",
        "--base",
        "main",
        "--tip",
        "@",
        "--json",
        "--verbose",
    ]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = CliFixture::json_stdout(&output);
    assert_eq!(report["remote"], "origin");
    assert_eq!(report["base"], "main");
    assert_eq!(report["tip"], "@");
    assert!(String::from_utf8_lossy(&output.stderr).contains("completed"));
}

#[test]
fn cross_host_target_fails_before_state_or_mutation() {
    let fixture = fixture("cross-host");

    let output = fixture.run(&[
        "--repo",
        "example.com/target/project",
        "--base",
        "main",
        "--json",
    ]);

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("differ"));
    assert!(!fixture.state_directory().exists());
    assert!(!fixture.remote_was_mutated());
    assert!(!fixture.pr_was_mutated());
}
