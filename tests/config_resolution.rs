use almighty_push::command::{CommandError, CommandExecutor, CommandOutput, CommandSpec};
use almighty_push::config::{repository_from_remote_url, ConfigError, ConfigInput, ConfigResolver};
use almighty_push::domain::{HeadRef, LimitValues, Limits, RemoteName, RepositoryId};
use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

struct FakeExecutor {
    replies: Mutex<VecDeque<CommandOutput>>,
    programs: Mutex<Vec<OsString>>,
    arguments: Mutex<Vec<Vec<OsString>>>,
    contracts: Mutex<Vec<(PathBuf, usize, Duration, usize)>>,
    failure_at: Option<usize>,
}

impl FakeExecutor {
    fn new<S: AsRef<str>>(replies: impl IntoIterator<Item = S>) -> Self {
        Self {
            replies: Mutex::new(
                replies
                    .into_iter()
                    .map(|stdout| CommandOutput {
                        stdout: stdout.as_ref().to_owned(),
                        stderr: String::new(),
                    })
                    .collect(),
            ),
            programs: Mutex::new(Vec::new()),
            arguments: Mutex::new(Vec::new()),
            contracts: Mutex::new(Vec::new()),
            failure_at: None,
        }
    }

    fn failing<S: AsRef<str>>(replies: impl IntoIterator<Item = S>, failure_at: usize) -> Self {
        let mut executor = Self::new(replies);
        executor.failure_at = Some(failure_at);
        executor
    }

    fn program_count(&self, program: &str) -> usize {
        self.programs
            .lock()
            .unwrap()
            .iter()
            .filter(|observed| observed == &&OsString::from(program))
            .count()
    }

    fn arguments(&self) -> Vec<Vec<OsString>> {
        self.arguments.lock().unwrap().clone()
    }

    fn contracts(&self) -> Vec<(PathBuf, usize, Duration, usize)> {
        self.contracts.lock().unwrap().clone()
    }
}

impl CommandExecutor for FakeExecutor {
    fn run(&self, spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
        let call_index = self.programs.lock().unwrap().len();
        self.programs
            .lock()
            .unwrap()
            .push(spec.program().to_owned());
        self.arguments.lock().unwrap().push(spec.args().to_vec());
        self.contracts.lock().unwrap().push((
            spec.cwd().to_owned(),
            spec.environment_count(),
            spec.timeout(),
            spec.output_limit_bytes(),
        ));
        if self.failure_at == Some(call_index) {
            return Err(CommandError::Exit {
                status_code: Some(1),
                stdout: String::new(),
                stderr: "not found".to_owned(),
            });
        }
        Ok(self.replies.lock().unwrap().pop_front().unwrap())
    }
}

fn limits() -> Limits {
    Limits::new(LimitValues {
        github_page_size: 8,
        command_output_bytes_max: 65_536,
        command_timeout_ms: 1_000,
        ..LimitValues::default()
    })
    .unwrap()
}

fn input(github_enabled: bool) -> ConfigInput {
    ConfigInput {
        remote: Some(RemoteName::parse("origin").unwrap()),
        repository: None,
        base: Some(HeadRef::parse("main").unwrap()),
        tip_revset: "@".to_owned(),
        limits: limits(),
        github_enabled,
    }
}

#[test]
fn remote_urls_preserve_the_actual_authority_and_repository() {
    let expected = RepositoryId::parse("github.com/owner/project").unwrap();
    for url in [
        "git@github.com:Owner/Project.git",
        "https://github.com/Owner/Project.git",
        "ssh://git@github.com/Owner/Project.git",
    ] {
        assert_eq!(repository_from_remote_url(url).unwrap(), expected);
    }
    for invalid in [
        "https://evil.example/github.com/owner/project.git",
        "nonsense://github.com/owner/project.git",
        "file://github.com/owner/project.git",
        "https://token@github.com/owner/project.git",
        "https://github.com:443/owner/project.git",
        "https://github.com/owner/project.git/",
        "https://github.com//owner/project.git",
        "https://github.com/owner/project.git?token=secret",
        "https://github.com/owner/project.git#fragment",
        "https://github.com/owner/pro%6Aect.git",
        "https://github.com/owner//project.git",
        "https://github.com/owner/extra/project.git",
        "git@github.com:owner/project.git/",
        "ssh://git@github.com:22/owner/project.git",
        "ssh://root@github.com/owner/project.git",
        "git@@github.com:owner/project.git",
    ] {
        assert!(repository_from_remote_url(invalid).is_err(), "{invalid}");
        assert!(
            !format!("{:?}", repository_from_remote_url(invalid).unwrap_err()).contains("secret")
        );
    }
}

#[test]
fn locked_resolution_acquires_before_remote_observation() {
    let workspace = workspace();
    let first_executor = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin https://github.com/owner/project.git\n",
    ]);
    let first_resolver = ConfigResolver::new(
        &first_executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut locked_input = input(false);
    locked_input.limits = Limits::new(LimitValues {
        lock_wait_ms: 100,
        ..LimitValues::default()
    })
    .unwrap();
    let locked = first_resolver
        .resolve_locked(&locked_input, &workspace)
        .unwrap();

    let second_executor = FakeExecutor::new([workspace.to_str().unwrap()]);
    let second_resolver = ConfigResolver::new(
        &second_executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    assert!(matches!(
        second_resolver.resolve_locked(&locked_input, &workspace),
        Err(ConfigError::ConfigurationLockContended { .. })
    ));
    assert_eq!(second_executor.arguments().len(), 1);
    drop(locked);
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn exact_remote_and_no_pr_mode_resolve_without_gh() {
    let workspace = workspace();
    let executor = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin https://github.com/owner/project.git\nupstream https://github.com/team/project.git\n",
    ]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );

    let resolved = resolver.resolve(&input(false), &workspace).unwrap();

    assert_eq!(resolved.workspace_root(), workspace);
    assert_eq!(resolved.remote().as_str(), "origin");
    assert_eq!(
        resolved.source_repository().canonical(),
        "github.com/owner/project"
    );
    assert_eq!(resolved.target_repository(), resolved.source_repository());
    assert_eq!(executor.program_count("/fake/gh"), 0);
    assert_eq!(
        executor.arguments(),
        vec![
            vec!["--ignore-working-copy", "workspace", "root"],
            vec!["--ignore-working-copy", "git", "remote", "list"],
        ]
        .into_iter()
        .map(|row| row.into_iter().map(OsString::from).collect::<Vec<_>>())
        .collect::<Vec<_>>()
    );
    assert_eq!(
        executor.contracts(),
        vec![
            (workspace.clone(), 0, Duration::from_secs(1), 65_536),
            (workspace.clone(), 0, Duration::from_secs(1), 65_536),
        ]
    );
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn configuration_keeps_github_credentials_out_of_every_jj_process() {
    let workspace = workspace();
    let executor = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin https://github.com/owner/project.git\n",
        r#"{"full_name":"owner/project","default_branch":"main"}"#,
        r#"{"name":"main"}"#,
    ]);
    let resolver = ConfigResolver::new_with_environments(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([(OsString::from("PATH"), OsString::from("/bin"))]),
        Box::new([
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("GH_TOKEN"), OsString::from("secret")),
        ]),
    );

    resolver.resolve(&input(true), &workspace).unwrap();

    let environment_counts = executor
        .contracts()
        .into_iter()
        .map(|(_, count, _, _)| count)
        .collect::<Vec<_>>();
    assert_eq!(environment_counts, vec![1, 1, 2, 2]);
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn github_mode_verifies_same_host_fork_target_and_default_base() {
    let workspace = workspace();
    let executor = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin git@github.com:source/project.git\n",
        r#"{"full_name":"target/project","default_branch":"trunk"}"#,
    ]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut config = input(true);
    config.repository = Some(RepositoryId::parse("github.com/target/project").unwrap());
    config.base = None;

    let resolved = resolver.resolve(&config, &workspace).unwrap();

    assert_eq!(resolved.source_repository().owner(), "source");
    assert_eq!(resolved.target_repository().owner(), "target");
    assert_eq!(resolved.base().as_str(), "trunk");
    assert_eq!(executor.program_count("/fake/gh"), 1);
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn github_mode_verifies_a_configured_base_with_an_encoded_endpoint() {
    let workspace = workspace();
    let executor = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin https://github.com/target/project.git\n",
        r#"{"full_name":"target/project","default_branch":null}"#,
        r#"{"name":"release/v1"}"#,
    ]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut config = input(true);
    config.repository = Some(RepositoryId::parse("github.com/target/project").unwrap());
    config.base = Some(HeadRef::parse("release/v1").unwrap());

    let resolved = resolver.resolve(&config, &workspace).unwrap();

    assert_eq!(resolved.base().as_str(), "release/v1");
    let calls = executor.arguments();
    assert_eq!(
        calls[2],
        vec!["api", "--hostname", "github.com", "repos/target/project"]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        calls[3],
        vec![
            "api",
            "--hostname",
            "github.com",
            "repos/target/project/branches/release%2Fv1",
        ]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>()
    );
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn configured_base_command_failure_retains_its_source() {
    let workspace = workspace();
    let executor = FakeExecutor::failing(
        [
            workspace.to_str().unwrap(),
            "origin https://github.com/target/project.git\n",
            r#"{"full_name":"target/project","default_branch":"main"}"#,
        ],
        3,
    );
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut config = input(true);
    config.base = Some(HeadRef::parse("missing").unwrap());

    let error = resolver.resolve(&config, &workspace).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::ConfiguredBaseUnavailable { .. }
    ));
    assert!(std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<CommandError>()
        .is_some());
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn github_null_default_and_malformed_json_have_precise_failures() {
    let workspace = workspace();
    let null_default = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin https://github.com/target/project.git\n",
        r#"{"full_name":"target/project","default_branch":null}"#,
    ]);
    let resolver = ConfigResolver::new(
        &null_default,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut config = input(true);
    config.base = None;
    assert!(matches!(
        resolver.resolve(&config, &workspace),
        Err(ConfigError::GithubResponse { .. })
    ));

    let malformed = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin https://github.com/target/project.git\n",
        "{broken",
    ]);
    let resolver = ConfigResolver::new(
        &malformed,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let error = resolver.resolve(&config, &workspace).unwrap_err();
    assert!(matches!(error, ConfigError::GithubJson { .. }));
    assert!(std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<serde_json::Error>()
        .is_some());
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn github_identity_mismatch_fails_before_accepting_a_base() {
    let workspace = workspace();
    let executor = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin https://github.com/source/project.git\n",
        r#"{"full_name":"other/project","default_branch":"main"}"#,
    ]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut config = input(true);
    config.repository = Some(RepositoryId::parse("github.com/target/project").unwrap());

    assert!(matches!(
        resolver.resolve(&config, &workspace),
        Err(ConfigError::GithubRepositoryMismatch { .. })
    ));
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn ambiguous_duplicate_and_wrong_remote_names_fail_closed() {
    let workspace = workspace();
    let executor = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin https://github.com/a/r.git\norigin https://github.com/b/r.git\n",
    ]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    assert!(matches!(
        resolver.resolve(&input(false), &workspace),
        Err(ConfigError::DuplicateRemote { .. })
    ));
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn one_remote_is_selected_but_zero_or_many_are_ambiguous() {
    let workspace = workspace();
    let only = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "sole https://github.com/a/r.git\n",
    ]);
    let resolver = ConfigResolver::new(
        &only,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut config = input(false);
    config.remote = None;
    assert_eq!(
        resolver
            .resolve(&config, &workspace)
            .unwrap()
            .remote()
            .as_str(),
        "sole"
    );

    let many = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "one https://github.com/a/r.git\ntwo https://github.com/a/r.git\n",
    ]);
    let resolver = ConfigResolver::new(
        &many,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    assert!(matches!(
        resolver.resolve(&config, &workspace),
        Err(ConfigError::AmbiguousRemotes { count: 2 })
    ));

    let none = FakeExecutor::new([workspace.to_str().unwrap(), ""]);
    let resolver = ConfigResolver::new(
        &none,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    assert!(matches!(
        resolver.resolve(&config, &workspace),
        Err(ConfigError::NoRemotes)
    ));
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn requested_missing_and_malformed_remote_rows_are_precise() {
    let workspace = workspace();
    let missing = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "upstream https://github.com/a/r.git\n",
    ]);
    let resolver = ConfigResolver::new(
        &missing,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    assert!(matches!(
        resolver.resolve(&input(false), &workspace),
        Err(ConfigError::MissingRemote { .. })
    ));

    let malformed = FakeExecutor::new([workspace.to_str().unwrap(), "origin\n"]);
    let resolver = ConfigResolver::new(
        &malformed,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    assert!(matches!(
        resolver.resolve(&input(false), &workspace),
        Err(ConfigError::InvalidRemoteRow { bytes: 6 })
    ));
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn remote_enumeration_is_bounded_before_tree_growth() {
    let workspace = workspace();
    let accepted_rows = (0..64)
        .map(|index| format!("remote{index} https://github.com/a/r.git"))
        .collect::<Vec<_>>()
        .join("\n");
    let accepted = FakeExecutor::new(vec![
        workspace.to_string_lossy().into_owned(),
        accepted_rows,
    ]);
    let resolver = ConfigResolver::new(
        &accepted,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut config = input(false);
    config.remote = Some(RemoteName::parse("remote63").unwrap());
    assert!(resolver.resolve(&config, &workspace).is_ok());

    let rows = (0..65)
        .map(|index| format!("remote{index} https://github.com/a/r.git"))
        .collect::<Vec<_>>()
        .join("\n");
    let executor = FakeExecutor::new(vec![workspace.to_string_lossy().into_owned(), rows]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut config = input(false);
    config.remote = None;

    assert!(matches!(
        resolver.resolve(&config, &workspace),
        Err(ConfigError::RemoteCountExceeded { count: 65, max: 64 })
    ));
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn no_pr_requires_explicit_base_and_cross_host_targets_are_rejected() {
    let workspace = workspace();
    let executor = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin https://github.com/a/r.git\n",
    ]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut config = input(false);
    config.base = None;
    assert!(matches!(
        resolver.resolve(&config, &workspace),
        Err(ConfigError::MissingBaseWithoutGithub)
    ));

    let executor = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin https://github.com/a/r.git\n",
    ]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut config = input(false);
    config.repository = Some(RepositoryId::parse("git.example.com/a/r").unwrap());
    assert!(matches!(
        resolver.resolve(&config, &workspace),
        Err(ConfigError::Domain(_))
    ));
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn workspace_root_must_be_canonical_and_contain_the_invocation() {
    let workspace = workspace();
    let other = unique_workspace();
    let executor = FakeExecutor::new([other.to_str().unwrap()]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    assert!(matches!(
        resolver.resolve(&input(false), &workspace),
        Err(ConfigError::InvalidWorkspaceRoot { .. })
    ));

    let executor = FakeExecutor::new([format!("{}\n\n", workspace.display())]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    assert!(matches!(
        resolver.resolve(&input(false), &workspace),
        Err(ConfigError::InvalidWorkspaceRoot { .. })
    ));
    fs::remove_dir_all(workspace).unwrap();
    fs::remove_dir_all(other).unwrap();
}

#[test]
fn invalid_tip_is_rejected_before_any_observation() {
    let workspace = workspace();
    for invalid in [
        String::new(),
        "   ".to_owned(),
        "contains\0nul".to_owned(),
        "contains\nnewline".to_owned(),
        "x".repeat(1_025),
    ] {
        let executor = FakeExecutor::new(std::iter::empty::<&str>());
        let resolver = ConfigResolver::new(
            &executor,
            PathBuf::from("/fake/jj"),
            PathBuf::from("/fake/gh"),
            Box::new([]),
        );
        let mut config = input(false);
        config.tip_revset = invalid;
        assert!(matches!(
            resolver.resolve(&config, &workspace),
            Err(ConfigError::Domain(_))
        ));
        assert!(executor.arguments().is_empty());
    }

    let executor = FakeExecutor::new([
        workspace.to_str().unwrap(),
        "origin https://github.com/a/r.git\n",
    ]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let mut config = input(false);
    config.tip_revset = "x".repeat(1_024);
    assert!(resolver.resolve(&config, &workspace).is_ok());
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn executor_and_error_resources_are_bounded_and_typed() {
    assert!(std::mem::size_of::<ConfigError>() <= 40);
    let workspace = workspace();
    let oversized = FakeExecutor::new(["x".repeat(65_537)]);
    let resolver = ConfigResolver::new(
        &oversized,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    assert!(matches!(
        resolver.resolve(&input(false), &workspace),
        Err(ConfigError::ExecutorOutputLimit {
            bytes: 65_537,
            max: 65_536,
        })
    ));

    let missing = workspace.join("missing");
    let executor = FakeExecutor::new(std::iter::empty::<&str>());
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    let error = resolver.resolve(&input(false), &missing).unwrap_err();
    assert!(matches!(error, ConfigError::WorkspaceIo { .. }));
    assert!(std::error::Error::source(&error)
        .unwrap()
        .downcast_ref::<std::io::Error>()
        .is_some());
    fs::remove_dir_all(workspace).unwrap();
}

#[cfg(unix)]
#[test]
fn symlinked_jj_metadata_is_rejected() {
    use std::os::unix::fs::symlink;

    let workspace = unique_directory("symlink");
    let metadata = workspace.join("metadata");
    fs::create_dir(&metadata).unwrap();
    symlink(&metadata, workspace.join(".jj")).unwrap();
    let executor = FakeExecutor::new([workspace.to_str().unwrap()]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );

    assert!(matches!(
        resolver.resolve(&input(false), &workspace),
        Err(ConfigError::UnsafeWorkspaceMetadata { .. })
    ));
    fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn missing_and_file_jj_metadata_have_distinct_failures() {
    let missing = unique_directory("missing-jj");
    let executor = FakeExecutor::new([missing.to_str().unwrap()]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    assert!(matches!(
        resolver.resolve(&input(false), &missing),
        Err(ConfigError::WorkspaceIo { .. })
    ));

    let file = unique_directory("file-jj");
    fs::write(file.join(".jj"), "not a directory").unwrap();
    let executor = FakeExecutor::new([file.to_str().unwrap()]);
    let resolver = ConfigResolver::new(
        &executor,
        PathBuf::from("/fake/jj"),
        PathBuf::from("/fake/gh"),
        Box::new([]),
    );
    assert!(matches!(
        resolver.resolve(&input(false), &file),
        Err(ConfigError::UnsafeWorkspaceMetadata { .. })
    ));
    fs::remove_dir_all(missing).unwrap();
    fs::remove_dir_all(file).unwrap();
}

fn workspace() -> PathBuf {
    unique_workspace()
}

fn unique_workspace() -> PathBuf {
    let root = unique_directory("workspace");
    fs::create_dir(root.join(".jj")).unwrap();
    root
}

fn unique_directory(label: &str) -> PathBuf {
    for sequence in 0..1000 {
        let path = std::env::temp_dir().join(format!(
            "almighty-push-config-{label}-{}-{sequence}",
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => return path.canonicalize().unwrap(),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!("fixture creation failed: {error}"),
        }
    }
    panic!("fixture attempts exhausted")
}
