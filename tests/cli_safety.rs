mod support;

use support::{CliFixture, CliFixtureOptions};

fn fixture(label: &str) -> CliFixture {
    CliFixture::new(label, CliFixtureOptions::default())
}

#[test]
fn dry_run_no_pr_renders_canonical_actions_without_any_persistent_or_external_mutation() {
    let fixture = fixture("dry-run-no-pr");

    let output = fixture.run(&["--dry-run", "--no-pr", "--base", "main", "--json"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report = CliFixture::json_stdout(&output);
    assert_eq!(report["mode"], "dry-run-no-pr");
    assert_eq!(report["stage_count"], 2);
    assert!(report["stages"][0]["actions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|action| action.as_str().unwrap().contains("PushHead")));
    assert!(!fixture.state_directory().exists());
    assert!(!fixture.workspace().join(".almighty").exists());
    assert!(!fixture.remote_was_mutated());
    assert!(!fixture.pr_was_mutated());
    let records = fixture.records();
    assert!(records.iter().all(|record| record.program == "jj"));
    assert!(records.iter().all(|record| matches!(
        record.operation.as_str(),
        "read" | "qualify-tip" | "stack-at"
    )));
}

#[test]
fn no_pr_requires_explicit_base_before_any_gh_process_or_mutation() {
    let fixture = fixture("no-pr-base");

    let output = fixture.run(&["--no-pr", "--json"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--base is required"));
    assert!(fixture.records().iter().all(|record| record.program != "gh"
        && matches!(record.operation.as_str(), "read" | "qualify-tip")));
    assert!(!fixture.state_directory().exists());
    assert!(!fixture.remote_was_mutated());
}

#[test]
fn no_pr_rejects_github_only_branch_deletion_policy_at_the_cli_boundary() {
    let fixture = fixture("no-pr-delete");

    let output = fixture.run(&["--no-pr", "--delete-branches", "--base", "main", "--json"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(fixture.records().is_empty());
    assert!(!fixture.state_directory().exists());
}

#[test]
fn malformed_scope_flags_fail_precisely_before_external_execution() {
    for (label, args) in [
        ("remote", vec!["--remote", "@"]),
        ("repo", vec!["--repo", "not-a-repository"]),
        ("base", vec!["--base", "refs/../unsafe"]),
        ("tip", vec!["--tip", "line\nbreak"]),
    ] {
        let fixture = fixture(label);
        let output = fixture.run(&args);
        assert_eq!(output.status.code(), Some(2), "{label}");
        assert!(fixture.records().is_empty(), "{label}");
        assert!(!fixture.state_directory().exists(), "{label}");
    }
}

#[test]
fn help_and_version_are_successful_without_discovery() {
    for flag in ["--help", "--version"] {
        let fixture = fixture(flag.trim_start_matches('-'));
        let output = fixture.run(&[flag]);
        assert!(output.status.success());
        assert!(fixture.records().is_empty());
        assert!(!fixture.state_directory().exists());
    }
}

#[test]
fn malformed_future_and_cross_scope_state_stop_before_reconciliation_mutation() {
    let fixtures = [
        ("malformed", "{broken".to_owned()),
        ("future", r#"{"schema_version":4}"#.to_owned()),
        (
            "cross-scope",
            r#"{"schema_version":3,"scope":{"source_repository":"github.com/other/project","target_repository":"github.com/other/project","remote":"origin","base":"main","tip_revset":"@"},"execution_occurrence":0,"verified":{},"historic":{},"legacy_migration":"None","checkpoint":"Idle"}"#.to_owned(),
        ),
    ];
    for (label, bytes) in fixtures {
        let fixture = CliFixture::new(
            label,
            CliFixtureOptions {
                initial_state: Some(bytes.into_bytes().into_boxed_slice()),
                ..CliFixtureOptions::default()
            },
        );
        let output = fixture.run(&["--no-pr", "--base", "main", "--json"]);
        assert_eq!(output.status.code(), Some(1), "{label}");
        assert!(!fixture.remote_was_mutated(), "{label}");
        assert!(!fixture.pr_was_mutated(), "{label}");
        assert!(
            fixture
                .records()
                .iter()
                .all(|record| matches!(record.operation.as_str(), "read" | "qualify-tip")),
            "{label}"
        );
    }
}

#[test]
fn failed_github_observation_never_creates_a_pull_request_or_state_namespace() {
    let fixture = CliFixture::new(
        "github-observation-failure",
        CliFixtureOptions {
            fail_github_observations: true,
            ..CliFixtureOptions::default()
        },
    );

    let output = fixture.run(&["--base", "main", "--json"]);

    assert_eq!(output.status.code(), Some(1));
    assert!(!fixture.state_directory().exists());
    assert!(!fixture.remote_was_mutated());
    assert!(!fixture.pr_was_mutated());
    assert!(fixture
        .records()
        .iter()
        .all(|record| record.operation != "gh-mutate"));
}
