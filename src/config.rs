use crate::command::{CommandError, CommandExecutor, CommandSpec, RetainedRepository, SpecError};
use crate::domain::{DomainError, HeadRef, Limits, RemoteName, RepositoryId, Scope};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::{CString, OsString};
use std::fmt::{self, Display, Formatter};
use std::fs::{self, File, TryLockError};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const WORKING_COPY_JSON_TEMPLATE: &str = r#"concat(
  "{\"empty\":", json(empty),
  ",\"description\":", json(description),
  "}\n"
)"#;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TipSelection {
    ImplicitWorkingCopy,
    ExplicitRevset(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigInput {
    pub remote: Option<RemoteName>,
    pub repository: Option<RepositoryId>,
    pub base: Option<HeadRef>,
    pub tip_selection: TipSelection,
    pub limits: Limits,
    pub github_enabled: bool,
}

#[derive(Clone, Debug)]
pub struct ResolvedConfig {
    workspace_root: PathBuf,
    state_directory: PathBuf,
    workspace_directory: Arc<File>,
    jj_directory: Arc<File>,
    configuration_lock: Option<Arc<File>>,
    scope: Scope,
    limits: Limits,
}

impl ResolvedConfig {
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn state_directory(&self) -> &Path {
        &self.state_directory
    }

    pub fn source_repository(&self) -> &RepositoryId {
        self.scope.source_repository()
    }

    pub fn target_repository(&self) -> &RepositoryId {
        self.scope.target_repository()
    }

    pub fn remote(&self) -> &RemoteName {
        self.scope.remote()
    }

    pub fn base(&self) -> &HeadRef {
        self.scope.base()
    }

    pub fn tip_revset(&self) -> &str {
        self.scope.tip_revset()
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub(crate) fn workspace_directory(&self) -> &File {
        &self.workspace_directory
    }

    pub(crate) fn jj_directory(&self) -> &File {
        &self.jj_directory
    }

    pub(crate) fn has_configuration_lock(&self) -> bool {
        self.configuration_lock.is_some()
    }

    pub(crate) fn duplicate_repository(&self) -> Result<RetainedRepository, ConfigError> {
        let current_workspace = open_directory_no_follow(&self.workspace_root)?;
        let current_jj = openat_directory(&current_workspace, ".jj")?;
        let workspace_matches = directory_identity(&current_workspace)?
            == directory_identity(&self.workspace_directory)?;
        let jj_matches =
            directory_identity(&current_jj)? == directory_identity(&self.jj_directory)?;
        if !workspace_matches || !jj_matches {
            return Err(ConfigError::WorkspaceIdentityChanged {
                path: self.workspace_root.clone(),
            });
        }
        Ok(RetainedRepository::new(
            duplicate_directory(&self.workspace_directory, &self.workspace_root)?,
            duplicate_directory(&self.jj_directory, &self.workspace_root.join(".jj"))?,
        ))
    }
}

pub enum ConfigError {
    CommandSpec(Box<SpecError>),
    Command(Box<CommandError>),
    Domain(Box<DomainError>),
    InvalidWorkspaceRoot {
        actual_bytes: usize,
        preview: String,
    },
    WorkspaceIo {
        operation: WorkspaceIoOperation,
        path_preview: String,
        source: Box<std::io::Error>,
    },
    UnsafeWorkspaceMetadata {
        path: PathBuf,
    },
    WorkspaceIdentityChanged {
        path: PathBuf,
    },
    ConfigurationLockContended {
        wait: Duration,
    },
    InvalidRemoteRow {
        bytes: usize,
    },
    WorkingCopyJson {
        source: Box<serde_json::Error>,
    },
    WorkingCopyRowCount {
        count: usize,
    },
    DuplicateRemote {
        remote: RemoteName,
    },
    MissingRemote {
        remote: RemoteName,
    },
    NoRemotes,
    AmbiguousRemotes {
        count: usize,
    },
    RemoteCountExceeded {
        count: usize,
        max: usize,
    },
    ExecutorOutputLimit {
        bytes: usize,
        max: usize,
    },
    InvalidRemoteUrl {
        actual_bytes: usize,
    },
    MissingBaseWithoutGithub,
    GithubRepositoryMismatch {
        expected: Box<RepositoryId>,
        observed: Box<RepositoryId>,
    },
    GithubJson {
        source: Box<serde_json::Error>,
    },
    GithubResponse {
        message: String,
    },
    ConfiguredBaseUnavailable {
        base: HeadRef,
        source: Box<CommandError>,
    },
    GithubCapabilityMissing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceIoOperation {
    InvocationCanonicalize,
    ReportedRootCanonicalize,
    MetadataInspect,
    OpenDirectory,
}

impl Display for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommandSpec(error) => Display::fmt(error, formatter),
            Self::Command(error) => Display::fmt(error, formatter),
            Self::Domain(error) => Display::fmt(error, formatter),
            Self::InvalidWorkspaceRoot {
                actual_bytes,
                preview,
            } => write!(
                formatter,
                "jj returned an invalid {actual_bytes}-byte workspace root: {preview:?}"
            ),
            Self::WorkspaceIo {
                operation,
                path_preview,
                source,
            } => write!(
                formatter,
                "workspace {operation:?} failed for {path_preview:?}: {source}"
            ),
            Self::UnsafeWorkspaceMetadata { path } => write!(
                formatter,
                "workspace metadata is not a real directory: {}",
                path.display()
            ),
            Self::WorkspaceIdentityChanged { path } => write!(
                formatter,
                "resolved workspace identity changed at {}",
                path.display()
            ),
            Self::ConfigurationLockContended { wait } => write!(
                formatter,
                "repository configuration lock remained contended for {} ms",
                wait.as_millis()
            ),
            Self::InvalidRemoteRow { bytes } => {
                write!(formatter, "invalid {bytes}-byte jj remote row")
            }
            Self::WorkingCopyJson { source } => {
                write!(formatter, "invalid jj working-copy JSON: {source}")
            }
            Self::WorkingCopyRowCount { count } => write!(
                formatter,
                "jj working-copy qualification returned {count} rows, expected exactly one"
            ),
            Self::DuplicateRemote { remote } => write!(formatter, "duplicate remote {remote}"),
            Self::MissingRemote { remote } => write!(formatter, "remote {remote} does not exist"),
            Self::NoRemotes => formatter.write_str("no Git remotes were observed"),
            Self::AmbiguousRemotes { count } => write!(
                formatter,
                "remote must be explicit because {count} remotes were observed"
            ),
            Self::RemoteCountExceeded { count, max } => write!(
                formatter,
                "observed {count} remotes, exceeding the bound {max}"
            ),
            Self::ExecutorOutputLimit { bytes, max } => write!(
                formatter,
                "executor returned {bytes} bytes, exceeding the bound {max}"
            ),
            Self::InvalidRemoteUrl { actual_bytes } => {
                write!(formatter, "invalid {actual_bytes}-byte remote URL")
            }
            Self::MissingBaseWithoutGithub => {
                formatter.write_str("--base is required when GitHub access is disabled")
            }
            Self::GithubRepositoryMismatch { expected, observed } => write!(
                formatter,
                "GitHub returned repository {observed}, expected {expected}"
            ),
            Self::GithubJson { source } => {
                write!(formatter, "invalid GitHub JSON: {source}")
            }
            Self::GithubResponse { message } => {
                write!(formatter, "invalid GitHub repository response: {message}")
            }
            Self::ConfiguredBaseUnavailable { base, .. } => {
                write!(formatter, "configured base {base} could not be verified")
            }
            Self::GithubCapabilityMissing => {
                formatter.write_str("GitHub configuration capability is unavailable")
            }
        }
    }
}

impl fmt::Debug for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CommandSpec(error) => Some(error.as_ref()),
            Self::Command(error) => Some(error.as_ref()),
            Self::Domain(error) => Some(error.as_ref()),
            Self::ConfiguredBaseUnavailable { source, .. } => Some(source.as_ref()),
            Self::WorkspaceIo { source, .. } => Some(source.as_ref()),
            Self::WorkingCopyJson { source } => Some(source.as_ref()),
            Self::GithubJson { source } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<SpecError> for ConfigError {
    fn from(error: SpecError) -> Self {
        Self::CommandSpec(Box::new(error))
    }
}

impl From<CommandError> for ConfigError {
    fn from(error: CommandError) -> Self {
        Self::Command(Box::new(error))
    }
}

impl From<DomainError> for ConfigError {
    fn from(error: DomainError) -> Self {
        Self::Domain(Box::new(error))
    }
}

pub struct ConfigResolver<'a, E> {
    executor: &'a E,
    jj_program: PathBuf,
    gh_program: Option<PathBuf>,
    jj_environment: Box<[(OsString, OsString)]>,
    gh_environment: Box<[(OsString, OsString)]>,
}

impl<'a, E: CommandExecutor> ConfigResolver<'a, E> {
    pub fn new(
        executor: &'a E,
        jj_program: PathBuf,
        gh_program: PathBuf,
        environment: Box<[(OsString, OsString)]>,
    ) -> Self {
        Self::new_with_environments(
            executor,
            jj_program,
            gh_program,
            environment.clone(),
            environment,
        )
    }

    pub fn new_with_environments(
        executor: &'a E,
        jj_program: PathBuf,
        gh_program: PathBuf,
        jj_environment: Box<[(OsString, OsString)]>,
        gh_environment: Box<[(OsString, OsString)]>,
    ) -> Self {
        Self {
            executor,
            jj_program,
            gh_program: Some(gh_program),
            jj_environment,
            gh_environment,
        }
    }

    pub fn new_without_github(
        executor: &'a E,
        jj_program: PathBuf,
        jj_environment: Box<[(OsString, OsString)]>,
    ) -> Self {
        Self {
            executor,
            jj_program,
            gh_program: None,
            jj_environment,
            gh_environment: Box::new([]),
        }
    }

    pub fn resolve(
        &self,
        input: &ConfigInput,
        invocation_cwd: &Path,
    ) -> Result<ResolvedConfig, ConfigError> {
        self.resolve_with_lock(input, invocation_cwd, false)
    }

    pub fn resolve_locked(
        &self,
        input: &ConfigInput,
        invocation_cwd: &Path,
    ) -> Result<ResolvedConfig, ConfigError> {
        self.resolve_with_lock(input, invocation_cwd, true)
    }

    fn resolve_with_lock(
        &self,
        input: &ConfigInput,
        invocation_cwd: &Path,
        lock_before_observation: bool,
    ) -> Result<ResolvedConfig, ConfigError> {
        if let TipSelection::ExplicitRevset(revset) = &input.tip_selection {
            Scope::validate_tip_revset(revset)?;
        }
        let invocation_cwd =
            invocation_cwd
                .canonicalize()
                .map_err(|source| ConfigError::WorkspaceIo {
                    operation: WorkspaceIoOperation::InvocationCanonicalize,
                    path_preview: bounded_preview(&invocation_cwd.display().to_string()),
                    source: Box::new(source),
                })?;
        let root_output = self.run(
            &self.jj_program,
            ["--ignore-working-copy", "workspace", "root"],
            &invocation_cwd,
            input.limits,
        )?;
        let workspace_root = parse_workspace_root(&root_output.stdout)?;
        if !invocation_cwd.starts_with(&workspace_root) {
            return Err(invalid_workspace_root(&root_output.stdout));
        }
        validate_workspace_metadata(&workspace_root)?;
        let workspace_directory = Arc::new(open_directory_no_follow(&workspace_root)?);
        let jj_directory = Arc::new(openat_directory(&workspace_directory, ".jj")?);
        let configuration_lock = if lock_before_observation {
            Some(Arc::new(acquire_configuration_lock(
                &jj_directory,
                input.limits.lock_wait(),
            )?))
        } else {
            None
        };

        let remote_output = self.run(
            &self.jj_program,
            ["--ignore-working-copy", "git", "remote", "list"],
            &workspace_root,
            input.limits,
        )?;
        let remotes = parse_remote_list(&remote_output.stdout, input.limits.change_count_max())?;
        let (remote, remote_url) = select_remote(&remotes, input.remote.as_ref())?;
        let source_repository = repository_from_remote_url(remote_url)?;
        let target_repository = input
            .repository
            .clone()
            .unwrap_or_else(|| source_repository.clone());
        if source_repository.host() != target_repository.host() {
            return Err(DomainError::CrossHostRepositories {
                source_host: source_repository.host().to_owned(),
                target_host: target_repository.host().to_owned(),
            }
            .into());
        }

        let tip_revset =
            self.resolve_tip_revset(&input.tip_selection, &workspace_root, input.limits)?;
        let base = if input.github_enabled {
            self.resolve_github_base(
                &target_repository,
                input.base.as_ref(),
                &workspace_root,
                input.limits,
            )?
        } else {
            input
                .base
                .clone()
                .ok_or(ConfigError::MissingBaseWithoutGithub)?
        };
        let scope = Scope::new(
            source_repository,
            target_repository,
            remote,
            base,
            tip_revset,
        )?;
        Ok(ResolvedConfig {
            state_directory: workspace_root.join(".jj/almighty-push"),
            workspace_root,
            workspace_directory,
            jj_directory,
            configuration_lock,
            scope,
            limits: input.limits,
        })
    }

    fn resolve_tip_revset(
        &self,
        selection: &TipSelection,
        workspace_root: &Path,
        limits: Limits,
    ) -> Result<String, ConfigError> {
        if let TipSelection::ExplicitRevset(revset) = selection {
            return Ok(revset.clone());
        }
        let output = self.run(
            &self.jj_program,
            [
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "--revision",
                "@",
                "--limit",
                "2",
                "--template",
                WORKING_COPY_JSON_TEMPLATE,
            ],
            workspace_root,
            limits,
        )?;
        let mut rows = output.stdout.lines();
        let Some(row) = rows.next() else {
            return Err(ConfigError::WorkingCopyRowCount { count: 0 });
        };
        let row: WorkingCopyRow =
            serde_json::from_str(row).map_err(|source| ConfigError::WorkingCopyJson {
                source: Box::new(source),
            })?;
        if rows.next().is_some() {
            return Err(ConfigError::WorkingCopyRowCount { count: 2 });
        }
        if row.empty && row.description.is_empty() {
            Ok("@-".to_owned())
        } else {
            Ok("@".to_owned())
        }
    }

    fn resolve_github_base(
        &self,
        target: &RepositoryId,
        configured_base: Option<&HeadRef>,
        cwd: &Path,
        limits: Limits,
    ) -> Result<HeadRef, ConfigError> {
        let repository = format!("{}/{}", target.owner(), target.name());
        let endpoint = format!("repos/{repository}");
        let gh_program = self
            .gh_program
            .as_deref()
            .ok_or(ConfigError::GithubCapabilityMissing)?;
        let output = self.run_os(
            gh_program,
            vec![
                "api".into(),
                "--hostname".into(),
                target.host().into(),
                endpoint.into(),
            ],
            cwd,
            limits,
        )?;
        let response: GithubRepository = parse_github_json(&output.stdout)?;
        let observed = RepositoryId::parse(format!("{}/{}", target.host(), response.full_name))?;
        if observed != *target {
            return Err(ConfigError::GithubRepositoryMismatch {
                expected: Box::new(target.clone()),
                observed: Box::new(observed),
            });
        }
        if let Some(base) = configured_base {
            let endpoint = format!(
                "repos/{repository}/branches/{}",
                percent_encode_path_segment(base.as_str())
            );
            let output = self
                .run_os(
                    gh_program,
                    vec![
                        "api".into(),
                        "--hostname".into(),
                        target.host().into(),
                        endpoint.into(),
                    ],
                    cwd,
                    limits,
                )
                .map_err(|error| match error {
                    ConfigError::Command(source) => ConfigError::ConfiguredBaseUnavailable {
                        base: base.clone(),
                        source,
                    },
                    other => other,
                })?;
            let observed_base: GithubBranch = parse_github_json(&output.stdout)?;
            let observed_base = HeadRef::parse(observed_base.name)?;
            if observed_base != *base {
                return Err(ConfigError::GithubResponse {
                    message: "branch identity mismatch".to_owned(),
                });
            }
            return Ok(base.clone());
        }
        let default_branch =
            response
                .default_branch
                .ok_or_else(|| ConfigError::GithubResponse {
                    message: "repository has no default branch".to_owned(),
                })?;
        HeadRef::parse(default_branch).map_err(Into::into)
    }

    fn run<const N: usize>(
        &self,
        program: &Path,
        args: [&str; N],
        cwd: &Path,
        limits: Limits,
    ) -> Result<crate::command::CommandOutput, ConfigError> {
        self.run_os(
            program,
            args.into_iter().map(OsString::from).collect(),
            cwd,
            limits,
        )
    }

    fn run_os(
        &self,
        program: &Path,
        args: Vec<OsString>,
        cwd: &Path,
        limits: Limits,
    ) -> Result<crate::command::CommandOutput, ConfigError> {
        let environment = if program == self.jj_program.as_path() {
            self.jj_environment.clone()
        } else {
            assert_eq!(Some(program), self.gh_program.as_deref());
            self.gh_environment.clone()
        };
        let spec = CommandSpec::new(
            program.as_os_str().to_owned(),
            args.into_boxed_slice(),
            cwd.to_owned(),
            Box::new([]),
            environment,
            limits,
        )?;
        let output = self.executor.run(&spec)?;
        let bytes = output.stdout.len().saturating_add(output.stderr.len());
        let max = limits.command_output_bytes_max();
        if bytes > max {
            return Err(ConfigError::ExecutorOutputLimit { bytes, max });
        }
        Ok(output)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkingCopyRow {
    empty: bool,
    description: String,
}

#[derive(Deserialize)]
struct GithubRepository {
    full_name: String,
    default_branch: Option<String>,
}

#[derive(Deserialize)]
struct GithubBranch {
    name: String,
}

fn parse_github_json<T: for<'de> Deserialize<'de>>(output: &str) -> Result<T, ConfigError> {
    serde_json::from_str(output).map_err(|source| ConfigError::GithubJson {
        source: Box::new(source),
    })
}

fn percent_encode_path_segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

fn parse_workspace_root(output: &str) -> Result<PathBuf, ConfigError> {
    let value = output.strip_suffix('\n').unwrap_or(output);
    if value.is_empty() || value.contains('\n') || value.contains('\r') {
        return Err(invalid_workspace_root(value));
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(invalid_workspace_root(value));
    }
    let canonical = path
        .canonicalize()
        .map_err(|source| ConfigError::WorkspaceIo {
            operation: WorkspaceIoOperation::ReportedRootCanonicalize,
            path_preview: bounded_preview(value),
            source: Box::new(source),
        })?;
    if canonical != path {
        return Err(invalid_workspace_root(value));
    }
    Ok(path)
}

fn invalid_workspace_root(value: &str) -> ConfigError {
    ConfigError::InvalidWorkspaceRoot {
        actual_bytes: value.len(),
        preview: bounded_preview(value),
    }
}

fn acquire_configuration_lock(directory: &File, wait: Duration) -> Result<File, ConfigError> {
    let lock = directory
        .try_clone()
        .map_err(|source| ConfigError::WorkspaceIo {
            operation: WorkspaceIoOperation::OpenDirectory,
            path_preview: ".jj".to_owned(),
            source: Box::new(source),
        })?;
    let deadline = Instant::now() + wait;
    let mut first_attempt = true;
    loop {
        if !first_attempt && Instant::now() >= deadline {
            return Err(ConfigError::ConfigurationLockContended { wait });
        }
        first_attempt = false;
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) => thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            ),
            Err(TryLockError::Error(source)) => {
                return Err(ConfigError::WorkspaceIo {
                    operation: WorkspaceIoOperation::OpenDirectory,
                    path_preview: ".jj".to_owned(),
                    source: Box::new(source),
                });
            }
        }
    }
}

fn open_directory_no_follow(path: &Path) -> Result<File, ConfigError> {
    let mut directory = File::open("/").map_err(|source| ConfigError::WorkspaceIo {
        operation: WorkspaceIoOperation::OpenDirectory,
        path_preview: "/".to_owned(),
        source: Box::new(source),
    })?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            return Err(invalid_workspace_root(&path.display().to_string()));
        };
        directory = openat_directory_os(&directory, name.as_bytes(), path)?;
    }
    Ok(directory)
}

fn openat_directory(parent: &File, name: &str) -> Result<File, ConfigError> {
    openat_directory_os(parent, name.as_bytes(), Path::new(name))
}

fn openat_directory_os(parent: &File, name: &[u8], display: &Path) -> Result<File, ConfigError> {
    let name =
        CString::new(name).map_err(|_| invalid_workspace_root(&display.display().to_string()))?;
    // SAFETY: parent and component are valid; no-follow rejects link traversal.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(ConfigError::WorkspaceIo {
            operation: WorkspaceIoOperation::OpenDirectory,
            path_preview: bounded_preview(&display.display().to_string()),
            source: Box::new(std::io::Error::last_os_error()),
        });
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn duplicate_directory(directory: &File, path: &Path) -> Result<File, ConfigError> {
    directory
        .try_clone()
        .map_err(|source| ConfigError::WorkspaceIo {
            operation: WorkspaceIoOperation::OpenDirectory,
            path_preview: bounded_preview(&path.display().to_string()),
            source: Box::new(source),
        })
}

fn directory_identity(directory: &File) -> Result<(u64, u64), ConfigError> {
    let metadata = directory
        .metadata()
        .map_err(|source| ConfigError::WorkspaceIo {
            operation: WorkspaceIoOperation::MetadataInspect,
            path_preview: "retained directory".to_owned(),
            source: Box::new(source),
        })?;
    Ok((metadata.dev(), metadata.ino()))
}

fn bounded_preview(value: &str) -> String {
    value.chars().take(64).collect()
}

fn validate_workspace_metadata(root: &Path) -> Result<(), ConfigError> {
    let metadata_path = root.join(".jj");
    let metadata =
        fs::symlink_metadata(&metadata_path).map_err(|source| ConfigError::WorkspaceIo {
            operation: WorkspaceIoOperation::MetadataInspect,
            path_preview: bounded_preview(&metadata_path.display().to_string()),
            source: Box::new(source),
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(ConfigError::UnsafeWorkspaceMetadata {
            path: metadata_path,
        });
    }
    Ok(())
}

fn parse_remote_list(
    output: &str,
    remote_count_max: usize,
) -> Result<BTreeMap<RemoteName, String>, ConfigError> {
    let mut remotes = BTreeMap::new();
    for (index, row) in output.lines().enumerate() {
        let count = index + 1;
        if count > remote_count_max {
            return Err(ConfigError::RemoteCountExceeded {
                count,
                max: remote_count_max,
            });
        }
        let Some((name, url)) = row.split_once(char::is_whitespace) else {
            return Err(ConfigError::InvalidRemoteRow { bytes: row.len() });
        };
        let url = url.trim();
        if url.is_empty() {
            return Err(ConfigError::InvalidRemoteRow { bytes: row.len() });
        }
        let remote = RemoteName::parse(name)?;
        if remotes.insert(remote.clone(), url.to_owned()).is_some() {
            return Err(ConfigError::DuplicateRemote { remote });
        }
    }
    Ok(remotes)
}

fn select_remote<'a>(
    remotes: &'a BTreeMap<RemoteName, String>,
    requested: Option<&RemoteName>,
) -> Result<(RemoteName, &'a str), ConfigError> {
    if let Some(remote) = requested {
        let url = remotes
            .get(remote)
            .ok_or_else(|| ConfigError::MissingRemote {
                remote: remote.clone(),
            })?;
        return Ok((remote.clone(), url));
    }
    if remotes.is_empty() {
        return Err(ConfigError::NoRemotes);
    }
    if remotes.len() != 1 {
        return Err(ConfigError::AmbiguousRemotes {
            count: remotes.len(),
        });
    }
    let (remote, url) = remotes.first_key_value().expect("one remote checked");
    Ok((remote.clone(), url))
}

pub fn repository_from_remote_url(url: &str) -> Result<RepositoryId, ConfigError> {
    let invalid = || ConfigError::InvalidRemoteUrl {
        actual_bytes: url.len(),
    };
    if url.is_empty()
        || url
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_whitespace())
        || url.contains(['?', '#', '%'])
    {
        return Err(invalid());
    }

    let (host, path) = if let Some(remainder) = url.strip_prefix("https://") {
        let (authority, path) = remainder.split_once('/').ok_or_else(&invalid)?;
        if authority.contains('@') || authority.contains(':') {
            return Err(invalid());
        }
        (authority, path)
    } else if let Some(remainder) = url.strip_prefix("ssh://") {
        let (authority, path) = remainder.split_once('/').ok_or_else(&invalid)?;
        let (user, host) = authority.split_once('@').ok_or_else(&invalid)?;
        if user != "git" || host.contains(['@', ':']) {
            return Err(invalid());
        }
        (host, path)
    } else {
        let (authority, path) = url.split_once(':').ok_or_else(&invalid)?;
        let (user, host) = authority.split_once('@').ok_or_else(&invalid)?;
        if user != "git" || host.contains(['@', ':']) {
            return Err(invalid());
        }
        (host, path)
    };
    if path.starts_with('/') || path.ends_with('/') || path.contains("//") {
        return Err(invalid());
    }
    let mut components = path.split('/');
    let owner = components.next().ok_or_else(&invalid)?;
    let repository = components.next().ok_or_else(&invalid)?;
    if owner.is_empty() || repository.is_empty() || components.next().is_some() {
        return Err(invalid());
    }
    let repository = repository.strip_suffix(".git").unwrap_or(repository);
    if repository.is_empty() {
        return Err(invalid());
    }
    RepositoryId::parse(format!("{host}/{owner}/{repository}")).map_err(|_| invalid())
}
