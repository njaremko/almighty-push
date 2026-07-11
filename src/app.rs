use crate::command::{CommandExecutor, CommandRunner};
use crate::config::{ConfigError, ConfigInput, ConfigResolver, ResolvedConfig, TipSelection};
use crate::domain::{DomainError, HeadRef, Limits, RemoteName, RepositoryId};
use crate::executor::{
    CompletionAcknowledgement, Executor, ExecutorError, FullEffectDriver, FullEffectDriverError,
    StageCompletion, StageOutcome,
};
use crate::github::{GithubClient, GithubError, GithubSnapshot};
use crate::jj::{JjClient, JjError};
use crate::plan::{
    derive_plan, DryRunMode, Effect, ExecutionMode, JjEffect, PlanningGithub, PlanningOutcome,
    RemoteRefState,
};
use crate::state::{
    CheckpointState, LegacyMigration, SessionError, StateError, StateStore, StateV3,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const STAGE_COUNT_MAX: usize = 4;
const PATH_COMPONENT_COUNT_MAX: usize = 128;
const PATH_BYTES_MAX: usize = 16 * 1024;
const CHILD_ENVIRONMENT_COMMON_KEYS: [&str; 5] = [
    "PATH",
    "HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "NO_COLOR",
];
const CHILD_ENVIRONMENT_JJ_KEYS: [&str; 3] = ["SSH_AUTH_SOCK", "JJ_CONFIG", "JJ_CONFIG_TOML"];
const CHILD_ENVIRONMENT_GITHUB_KEYS: [&str; 3] =
    ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN"];

struct ChildEnvironments {
    jj: Box<[(OsString, OsString)]>,
    github: Box<[(OsString, OsString)]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequestedMode {
    Full { delete_closed_heads: bool },
    NoPr,
    DryRunFull { delete_closed_heads: bool },
    DryRunNoPr,
}

impl RequestedMode {
    fn github_enabled(self) -> bool {
        matches!(self, Self::Full { .. } | Self::DryRunFull { .. })
    }

    pub fn is_dry_run(self) -> bool {
        matches!(self, Self::DryRunFull { .. } | Self::DryRunNoPr)
    }

    fn execution_mode(self) -> ExecutionMode {
        match self {
            Self::Full {
                delete_closed_heads,
            } => ExecutionMode::Full {
                delete_closed_heads,
            },
            Self::NoPr => ExecutionMode::NoPr,
            Self::DryRunFull {
                delete_closed_heads,
            } => ExecutionMode::DryRun(DryRunMode::Full {
                delete_closed_heads,
            }),
            Self::DryRunNoPr => ExecutionMode::DryRun(DryRunMode::NoPr),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Full { .. } => "full",
            Self::NoPr => "no-pr",
            Self::DryRunFull { .. } => "dry-run-full",
            Self::DryRunNoPr => "dry-run-no-pr",
        }
    }
}

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub remote: Option<RemoteName>,
    pub repository: Option<RepositoryId>,
    pub base: Option<HeadRef>,
    pub tip_revset: String,
    pub mode: RequestedMode,
    pub limits: Limits,
}

#[derive(Clone, Debug)]
struct ProgramPaths {
    jj: PathBuf,
    gh: Option<PathBuf>,
}

impl ProgramPaths {
    fn discover(github_enabled: bool) -> Result<Self, AppError> {
        let path = std::env::var_os("PATH").ok_or(AppError::MissingPath)?;
        let jj = resolve_program("jj", &path)?;
        let gh = github_enabled
            .then(|| resolve_program("gh", &path))
            .transpose()?;
        Ok(Self { jj, gh })
    }

    fn gh(&self) -> Result<&Path, AppError> {
        self.gh.as_deref().ok_or(AppError::GithubCapabilityMissing)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RunReport {
    mode: &'static str,
    source_repository: String,
    target_repository: String,
    remote: String,
    base: String,
    tip: String,
    stage_count: usize,
    stages: Vec<StageReport>,
    pr_urls: Vec<String>,
}

impl RunReport {
    pub fn mode(&self) -> &str {
        self.mode
    }

    pub fn stages(&self) -> &[StageReport] {
        &self.stages
    }

    pub fn pr_urls(&self) -> &[String] {
        &self.pr_urls
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StageReport {
    plan_id: String,
    effect_count: usize,
    outcome: &'static str,
    actions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    continuation: Option<&'static str>,
}

impl StageReport {
    pub fn actions(&self) -> &[String] {
        &self.actions
    }
}

pub enum AppError {
    MissingPath,
    PathTooLarge,
    ProgramNotFound { name: &'static str },
    ProgramUnsafe { path: PathBuf },
    GithubCapabilityMissing,
    Config(Box<ConfigError>),
    Domain(Box<DomainError>),
    State(Box<StateError>),
    Session(Box<SessionError>),
    Jj(Box<JjError>),
    Github(Box<GithubError>),
    Plan(Box<crate::plan::PlanError>),
    JjExecution(Box<ExecutorError<JjError>>),
    FullExecution(Box<ExecutorError<FullEffectDriverError>>),
    Conflict { change_id: crate::domain::ChangeId },
    PendingLegacyNoPr { count: usize },
    StageBound { max: usize },
    InvalidCheckpoint,
}

impl Display for AppError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingPath => formatter.write_str("PATH is required to locate jj and gh"),
            Self::PathTooLarge => formatter.write_str("PATH exceeds the compiled discovery bound"),
            Self::ProgramNotFound { name } => {
                write!(formatter, "required program {name} was not found in PATH")
            }
            Self::ProgramUnsafe { path } => write!(
                formatter,
                "resolved program is not an executable regular file: {}",
                path.display()
            ),
            Self::GithubCapabilityMissing => {
                formatter.write_str("GitHub capability is unavailable in this mode")
            }
            Self::Config(error) => Display::fmt(error, formatter),
            Self::Domain(error) => Display::fmt(error, formatter),
            Self::State(error) => Display::fmt(error, formatter),
            Self::Session(error) => Display::fmt(error, formatter),
            Self::Jj(error) => Display::fmt(error, formatter),
            Self::Github(error) => Display::fmt(error, formatter),
            Self::Plan(error) => Display::fmt(error, formatter),
            Self::JjExecution(error) => Display::fmt(error, formatter),
            Self::FullExecution(error) => Display::fmt(error, formatter),
            Self::Conflict { change_id } => write!(
                formatter,
                "selected change {change_id} has unresolved conflicts; resolve them and retry"
            ),
            Self::PendingLegacyNoPr { count } => write!(
                formatter,
                "{count} legacy ownership candidates require one full GitHub-enabled migration before --no-pr can execute"
            ),
            Self::StageBound { max } => write!(
                formatter,
                "reconciliation still requires re-observation after the maximum {max} stages"
            ),
            Self::InvalidCheckpoint => {
                formatter.write_str("durable checkpoint is not executable in the requested mode")
            }
        }
    }
}

impl fmt::Debug for AppError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl Error for AppError {}

macro_rules! boxed_from {
    ($source:ty, $variant:ident) => {
        impl From<$source> for AppError {
            fn from(error: $source) -> Self {
                Self::$variant(Box::new(error))
            }
        }
    };
}

boxed_from!(ConfigError, Config);
boxed_from!(DomainError, Domain);
boxed_from!(StateError, State);
boxed_from!(SessionError, Session);
boxed_from!(JjError, Jj);
boxed_from!(GithubError, Github);
boxed_from!(crate::plan::PlanError, Plan);

pub fn run(options: RunOptions, invocation_cwd: &Path) -> Result<RunReport, AppError> {
    let programs = ProgramPaths::discover(options.mode.github_enabled())?;
    let environments = child_environments()?;
    // SAFETY: the CLI creates no child process outside this one process owner.
    let runner = unsafe { CommandRunner::assume_process_child_authority() };
    run_with_executor(options, invocation_cwd, programs, environments, &runner)
}

fn run_with_executor<E: CommandExecutor>(
    options: RunOptions,
    invocation_cwd: &Path,
    programs: ProgramPaths,
    environments: ChildEnvironments,
    executor: &E,
) -> Result<RunReport, AppError> {
    let github_enabled = options.mode.github_enabled();
    let resolver = match programs.gh.as_ref() {
        Some(gh_program) => ConfigResolver::new_with_environments(
            executor,
            programs.jj.clone(),
            gh_program.clone(),
            environments.jj.clone(),
            environments.github.clone(),
        ),
        None => ConfigResolver::new_without_github(
            executor,
            programs.jj.clone(),
            environments.jj.clone(),
        ),
    };
    let input = ConfigInput {
        remote: options.remote,
        repository: options.repository,
        base: options.base,
        tip_selection: TipSelection::ExplicitRevset(options.tip_revset),
        limits: options.limits,
        github_enabled,
    };
    let config = if options.mode.is_dry_run() {
        resolver.resolve(&input, invocation_cwd)?
    } else {
        resolver.resolve_locked(&input, invocation_cwd)?
    };

    if options.mode.is_dry_run() {
        run_dry(&config, options.mode, &programs, environments, executor)
    } else {
        run_mutating(&config, options.mode, &programs, environments, executor)
    }
}

fn run_dry<E: CommandExecutor>(
    config: &ResolvedConfig,
    mode: RequestedMode,
    programs: &ProgramPaths,
    environments: ChildEnvironments,
    executor: &E,
) -> Result<RunReport, AppError> {
    let mut state = StateStore::load_read_only_from_config(config)?;
    if let Some(report) = pending_checkpoint_preview(config, mode, &state)? {
        return Ok(report);
    }
    let github_snapshot = if mode.github_enabled() {
        preview_legacy(
            config,
            programs.gh()?,
            &environments.github,
            executor,
            &mut state,
        )?;
        Some(
            GithubClient::new(
                executor,
                programs.gh()?.to_owned(),
                config,
                &state,
                environments.github.clone(),
            )
            .observe()?,
        )
    } else {
        reject_pending_legacy(&state)?;
        None
    };
    let jj = JjClient::new_with_ownership_state(
        executor,
        programs.jj.clone(),
        config,
        &state,
        environments.jj,
    );
    let (chain, mut remote_heads) = observe_inputs(&jj, github_snapshot.as_ref(), &state)?;
    let mut snapshot = github_snapshot;
    let mut stages = Vec::with_capacity(STAGE_COUNT_MAX);
    while stages.len() < STAGE_COUNT_MAX {
        let github = snapshot
            .as_ref()
            .map_or(PlanningGithub::NotObserved, PlanningGithub::Observed);
        let derived = derive_plan(
            &chain,
            &remote_heads,
            github,
            &state,
            mode.execution_mode(),
            config.limits(),
        )?;
        let PlanningOutcome::DryRun(preview) = derived.outcome() else {
            unreachable!("dry-run mode cannot produce executor capability")
        };
        let executable_mode = match mode {
            RequestedMode::DryRunFull {
                delete_closed_heads,
            } => ExecutionMode::Full {
                delete_closed_heads,
            },
            RequestedMode::DryRunNoPr => ExecutionMode::NoPr,
            RequestedMode::Full { .. } | RequestedMode::NoPr => unreachable!(),
        };
        let executable = derive_plan(
            &chain,
            &remote_heads,
            github,
            &state,
            executable_mode,
            config.limits(),
        )?;
        let PlanningOutcome::Executable(plan) = executable.outcome() else {
            unreachable!("executable companion plan is required for symbolic continuation")
        };
        assert_eq!(preview.id(), plan.id());
        assert_eq!(preview.action_count(), plan.effects().len());
        let mut stage = StageReport {
            plan_id: preview.id().to_hex(),
            effect_count: preview.action_count(),
            outcome: "planned",
            actions: preview.actions().to_vec(),
            continuation: None,
        };
        let Some(Effect::Reobserve(_)) = plan.effects().last() else {
            stages.push(stage);
            return Ok(report(config, mode, stages, Vec::new()));
        };
        if !symbolically_publish_heads(plan.effects(), &mut remote_heads, &mut snapshot, config)? {
            stage.continuation = Some(
                "later actions depend on exact post-mutation identities and will be replanned after this barrier",
            );
            stages.push(stage);
            return Ok(report(config, mode, stages, Vec::new()));
        }
        stages.push(stage);
    }
    Err(AppError::StageBound {
        max: STAGE_COUNT_MAX,
    })
}

fn pending_checkpoint_preview(
    config: &ResolvedConfig,
    mode: RequestedMode,
    state: &StateV3,
) -> Result<Option<RunReport>, AppError> {
    let (checkpoint, outcome, continuation) = match state.checkpoint() {
        CheckpointState::Idle => return Ok(None),
        CheckpointState::Executing(checkpoint) => (
            checkpoint.as_ref(),
            "resume-pending",
            "full execution will reobserve and resume this durable plan before deriving new work",
        ),
        CheckpointState::Completed(checkpoint) => (
            checkpoint.as_ref(),
            "completion-pending",
            "full execution will acknowledge this durable completion before deriving new work",
        ),
    };
    if !mode.github_enabled()
        && checkpoint
            .plan()
            .effects()
            .iter()
            .skip(checkpoint.next_effect_index() as usize)
            .any(|effect| matches!(effect, Effect::Github(_)))
    {
        return Err(AppError::InvalidCheckpoint);
    }
    let preview = checkpoint
        .plan()
        .dry_run_remaining(checkpoint.next_effect_index(), config.limits())?;
    Ok(Some(report(
        config,
        mode,
        vec![StageReport {
            plan_id: preview.id().to_hex(),
            effect_count: preview.action_count(),
            outcome,
            actions: preview.actions().to_vec(),
            continuation: Some(continuation),
        }],
        Vec::new(),
    )))
}

fn run_mutating<E: CommandExecutor>(
    config: &ResolvedConfig,
    mode: RequestedMode,
    programs: &ProgramPaths,
    environments: ChildEnvironments,
    executor: &E,
) -> Result<RunReport, AppError> {
    let store = StateStore::from_config(config)?;
    let mut session = store.lock()?;
    if mode.github_enabled() {
        migrate_locked(
            config,
            programs.gh()?,
            &environments.github,
            executor,
            &mut session,
        )?;
    } else {
        reject_pending_legacy(session.state())?;
    }

    let fetch = JjClient::new_with_ownership_state(
        executor,
        programs.jj.clone(),
        config,
        session.state(),
        environments.jj.clone(),
    );
    fetch.fetch()?;

    let mut stages = Vec::with_capacity(STAGE_COUNT_MAX);
    let mut acknowledgement = resume_checkpoint(
        config,
        mode,
        programs,
        &environments,
        executor,
        &mut session,
        &mut stages,
    )?;

    while stages.len() < STAGE_COUNT_MAX {
        let state = session.state().clone();
        let jj = JjClient::new_with_ownership_state(
            executor,
            programs.jj.clone(),
            config,
            &state,
            environments.jj.clone(),
        );
        let snapshot = if mode.github_enabled() {
            Some(
                GithubClient::new(
                    executor,
                    programs.gh()?.to_owned(),
                    config,
                    &state,
                    environments.github.clone(),
                )
                .observe()?,
            )
        } else {
            None
        };
        let (chain, remote_heads) = observe_inputs(&jj, snapshot.as_ref(), &state)?;
        let planning_github = snapshot
            .as_ref()
            .map_or(PlanningGithub::NotObserved, PlanningGithub::Observed);
        let derived = derive_plan(
            &chain,
            &remote_heads,
            planning_github,
            &state,
            mode.execution_mode(),
            config.limits(),
        )?;
        let PlanningOutcome::Executable(plan) = derived.outcome() else {
            unreachable!("mutating mode cannot produce a dry-run plan")
        };
        let plan = plan.clone();
        let completion = if mode.github_enabled() {
            let jj_driver = JjClient::new_with_ownership_state(
                executor,
                programs.jj.clone(),
                config,
                &state,
                environments.jj.clone(),
            );
            let github_driver = GithubClient::new(
                executor,
                programs.gh()?.to_owned(),
                config,
                &state,
                environments.github.clone(),
            );
            let mut driver = FullEffectDriver::new(jj_driver, github_driver);
            execute_full(&mut session, acknowledgement.take(), plan, &mut driver)?
        } else {
            let mut driver = JjClient::new_with_ownership_state(
                executor,
                programs.jj.clone(),
                config,
                &state,
                environments.jj.clone(),
            );
            execute_jj(&mut session, acknowledgement.take(), plan, &mut driver)?
        };
        stages.push(stage_from_completion(&session, &completion)?);
        if completion.outcome() == StageOutcome::Complete {
            return Ok(report(
                config,
                mode,
                stages,
                pr_urls(config, session.state()),
            ));
        }
        acknowledgement = Some(completion.acknowledgement());
    }
    Err(AppError::StageBound {
        max: STAGE_COUNT_MAX,
    })
}

fn resume_checkpoint<E: CommandExecutor>(
    config: &ResolvedConfig,
    mode: RequestedMode,
    programs: &ProgramPaths,
    environments: &ChildEnvironments,
    executor: &E,
    session: &mut crate::state::LockedStateSession<'_>,
    stages: &mut Vec<StageReport>,
) -> Result<Option<CompletionAcknowledgement>, AppError> {
    if matches!(session.state().checkpoint(), CheckpointState::Idle) {
        return Ok(None);
    }
    let state = session.state().clone();
    let checkpoint = match state.checkpoint() {
        CheckpointState::Executing(checkpoint) | CheckpointState::Completed(checkpoint) => {
            checkpoint
        }
        CheckpointState::Idle => return Ok(None),
    };
    if !mode.github_enabled()
        && checkpoint
            .plan()
            .effects()
            .iter()
            .skip(checkpoint.next_effect_index() as usize)
            .any(|effect| matches!(effect, Effect::Github(_)))
    {
        return Err(AppError::InvalidCheckpoint);
    }
    let completion = if mode.github_enabled() {
        let jj = JjClient::new_with_ownership_state(
            executor,
            programs.jj.clone(),
            config,
            &state,
            environments.jj.clone(),
        );
        let github = GithubClient::new(
            executor,
            programs.gh()?.to_owned(),
            config,
            &state,
            environments.github.clone(),
        );
        let mut driver = FullEffectDriver::new(jj, github);
        Executor::resume_stage_in_session(session, &mut driver)
            .map_err(|error| AppError::FullExecution(Box::new(error)))?
    } else {
        let mut driver = JjClient::new_with_ownership_state(
            executor,
            programs.jj.clone(),
            config,
            &state,
            environments.jj.clone(),
        );
        Executor::resume_stage_in_session(session, &mut driver)
            .map_err(|error| AppError::JjExecution(Box::new(error)))?
    };
    stages.push(stage_from_completion(session, &completion)?);
    if stages.len() >= STAGE_COUNT_MAX && completion.outcome() != StageOutcome::Complete {
        return Err(AppError::StageBound {
            max: STAGE_COUNT_MAX,
        });
    }
    Ok(Some(completion.acknowledgement()))
}

fn execute_full<E: CommandExecutor>(
    session: &mut crate::state::LockedStateSession<'_>,
    acknowledgement: Option<CompletionAcknowledgement>,
    plan: crate::plan::Plan,
    driver: &mut FullEffectDriver<'_, E, E>,
) -> Result<StageCompletion, AppError> {
    match acknowledgement {
        Some(acknowledgement) => {
            Executor::execute_next_stage_in_session(session, acknowledgement, plan, driver)
        }
        None => Executor::execute_stage_in_session(session, plan, driver),
    }
    .map_err(|error| AppError::FullExecution(Box::new(error)))
}

fn execute_jj<E: CommandExecutor>(
    session: &mut crate::state::LockedStateSession<'_>,
    acknowledgement: Option<CompletionAcknowledgement>,
    plan: crate::plan::Plan,
    driver: &mut JjClient<'_, E>,
) -> Result<StageCompletion, AppError> {
    match acknowledgement {
        Some(acknowledgement) => {
            Executor::execute_next_stage_in_session(session, acknowledgement, plan, driver)
        }
        None => Executor::execute_stage_in_session(session, plan, driver),
    }
    .map_err(|error| AppError::JjExecution(Box::new(error)))
}

fn observe_inputs<E: CommandExecutor>(
    jj: &JjClient<'_, E>,
    github: Option<&GithubSnapshot>,
    state: &StateV3,
) -> Result<
    (
        crate::domain::SelectedChain,
        BTreeMap<HeadRef, RemoteRefState>,
    ),
    AppError,
> {
    let chain = jj.observe_chain()?;
    if let Some(revision) = chain
        .revisions()
        .iter()
        .find(|revision| revision.has_conflict())
    {
        return Err(AppError::Conflict {
            change_id: revision.change_id().clone(),
        });
    }
    let mut heads = BTreeSet::new();
    for revision in chain.revisions() {
        let managed = state
            .verified()
            .get(revision.change_id())
            .or_else(|| state.historic().get(revision.change_id()));
        let head = match managed {
            Some(managed) => managed.head().clone(),
            None => HeadRef::owned(revision.change_id())?,
        };
        heads.insert(head);
    }
    let mut observed = jj.observe_remote_heads(&heads.into_iter().collect::<Vec<_>>())?;
    if let Some(snapshot) = github {
        let mut bases = snapshot
            .prs()
            .iter()
            .filter(|pr| pr.lifecycle() == crate::domain::PrLifecycle::Merged)
            .map(|pr| pr.base_ref().clone())
            .collect::<BTreeSet<_>>();
        bases.retain(|head| !observed.contains_key(head));
        let base_observations = jj.observe_remote_heads(&bases.into_iter().collect::<Vec<_>>())?;
        observed.extend(base_observations);
    }
    Ok((chain, observed))
}

fn symbolically_publish_heads(
    effects: &[Effect],
    remote_heads: &mut BTreeMap<HeadRef, RemoteRefState>,
    snapshot: &mut Option<GithubSnapshot>,
    config: &ResolvedConfig,
) -> Result<bool, AppError> {
    let mutation_count = effects.len().saturating_sub(1);
    if mutation_count == 0
        || !effects[..mutation_count]
            .iter()
            .all(|effect| matches!(effect, Effect::Jj(JjEffect::PushHead { .. })))
    {
        return Ok(false);
    }
    let mut owned_heads = snapshot
        .as_ref()
        .map(|observed| observed.owned_heads().clone())
        .unwrap_or_default();
    for effect in &effects[..mutation_count] {
        let Effect::Jj(JjEffect::PushHead {
            ownership, desired, ..
        }) = effect
        else {
            unreachable!("push-only stage was checked")
        };
        remote_heads.insert(
            ownership.head().clone(),
            RemoteRefState::At(desired.clone()),
        );
        if snapshot.is_some() {
            owned_heads.insert(ownership.head().clone(), desired.clone());
        }
    }
    if let Some(observed) = snapshot.as_ref() {
        *snapshot = Some(observed.with_preview_owned_heads(owned_heads, config.limits())?);
    }
    Ok(true)
}

fn preview_legacy<E: CommandExecutor>(
    config: &ResolvedConfig,
    gh_program: &Path,
    environment: &[(OsString, OsString)],
    executor: &E,
    state: &mut StateV3,
) -> Result<(), AppError> {
    let candidate_count_max = config.limits().change_count_max();
    for _ in 0..candidate_count_max {
        let Some(candidate) = state.legacy_candidates().first().cloned() else {
            break;
        };
        let client = GithubClient::new(
            executor,
            gh_program.to_owned(),
            config,
            state,
            environment.to_vec().into_boxed_slice(),
        );
        let resolution = client.resolve_legacy_candidate(&candidate)?;
        state.resolve_legacy_preview(resolution)?;
    }
    match state.legacy_migration() {
        LegacyMigration::ReadyToDelete { .. } => state.finish_legacy_preview()?,
        LegacyMigration::Pending { candidates, .. } => {
            return Err(AppError::PendingLegacyNoPr {
                count: candidates.len(),
            });
        }
        LegacyMigration::None | LegacyMigration::Complete { .. } => {}
    }
    Ok(())
}

fn migrate_locked<E: CommandExecutor>(
    config: &ResolvedConfig,
    gh_program: &Path,
    environment: &[(OsString, OsString)],
    executor: &E,
    session: &mut crate::state::LockedStateSession<'_>,
) -> Result<(), AppError> {
    let candidate_count_max = config.limits().change_count_max();
    for _ in 0..candidate_count_max {
        let Some(candidate) = session.state().legacy_candidates().first().cloned() else {
            break;
        };
        let client = GithubClient::new(
            executor,
            gh_program.to_owned(),
            config,
            session.state(),
            environment.to_vec().into_boxed_slice(),
        );
        let resolution = client.resolve_legacy_candidate(&candidate)?;
        session.resolve_legacy(resolution)?;
    }
    match session.state().legacy_migration() {
        LegacyMigration::ReadyToDelete { .. } => session.finish_legacy_deletion()?,
        LegacyMigration::Pending { candidates, .. } => {
            return Err(AppError::PendingLegacyNoPr {
                count: candidates.len(),
            });
        }
        LegacyMigration::None | LegacyMigration::Complete { .. } => {}
    }
    Ok(())
}

fn reject_pending_legacy(state: &StateV3) -> Result<(), AppError> {
    let count = match state.legacy_migration() {
        LegacyMigration::Pending { candidates, .. } => Some(candidates.len()),
        LegacyMigration::ReadyToDelete { resolved, .. } => Some(resolved.len()),
        LegacyMigration::None | LegacyMigration::Complete { .. } => None,
    };
    match count {
        Some(count) => Err(AppError::PendingLegacyNoPr { count }),
        None => Ok(()),
    }
}

fn stage_from_completion(
    session: &crate::state::LockedStateSession<'_>,
    completion: &StageCompletion,
) -> Result<StageReport, AppError> {
    let CheckpointState::Completed(checkpoint) = session.state().checkpoint() else {
        return Err(AppError::InvalidCheckpoint);
    };
    let outcome = match completion.outcome() {
        StageOutcome::Complete => "complete",
        StageOutcome::Reobserve(_) => "reobserve",
    };
    Ok(StageReport {
        plan_id: checkpoint.plan().id().to_hex(),
        effect_count: checkpoint.plan().effects().len(),
        outcome,
        actions: Vec::new(),
        continuation: None,
    })
}

fn pr_urls(config: &ResolvedConfig, state: &StateV3) -> Vec<String> {
    state
        .verified()
        .values()
        .map(|managed| {
            format!(
                "https://{}/{}/{}/pull/{}",
                config.target_repository().host(),
                config.target_repository().owner(),
                config.target_repository().name(),
                managed.number().get()
            )
        })
        .collect()
}

fn report(
    config: &ResolvedConfig,
    mode: RequestedMode,
    stages: Vec<StageReport>,
    pr_urls: Vec<String>,
) -> RunReport {
    RunReport {
        mode: mode.label(),
        source_repository: config.source_repository().canonical(),
        target_repository: config.target_repository().canonical(),
        remote: config.remote().as_str().to_owned(),
        base: config.base().as_str().to_owned(),
        tip: config.tip_revset().to_owned(),
        stage_count: stages.len(),
        stages,
        pr_urls,
    }
}

fn resolve_program(name: &'static str, path: &OsStr) -> Result<PathBuf, AppError> {
    if path.as_encoded_bytes().len() > PATH_BYTES_MAX {
        return Err(AppError::PathTooLarge);
    }
    for (index, directory) in std::env::split_paths(path).enumerate() {
        if index >= PATH_COMPONENT_COUNT_MAX {
            return Err(AppError::PathTooLarge);
        }
        if directory.as_os_str().is_empty() || !directory.is_absolute() {
            continue;
        }
        let candidate = directory.join(name);
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(AppError::ProgramUnsafe { path: candidate });
        }
        return candidate
            .canonicalize()
            .map_err(|_| AppError::ProgramUnsafe { path: candidate });
    }
    Err(AppError::ProgramNotFound { name })
}

fn child_environments() -> Result<ChildEnvironments, AppError> {
    let common = collect_environment(&CHILD_ENVIRONMENT_COMMON_KEYS)?;
    let mut jj = common.clone();
    jj.extend(collect_environment(&CHILD_ENVIRONMENT_JJ_KEYS)?);
    let mut github = common;
    github.extend(collect_environment(&CHILD_ENVIRONMENT_GITHUB_KEYS)?);
    validate_environment_bytes(&jj)?;
    validate_environment_bytes(&github)?;
    Ok(ChildEnvironments {
        jj: jj.into_boxed_slice(),
        github: github.into_boxed_slice(),
    })
}

fn collect_environment(keys: &[&str]) -> Result<Vec<(OsString, OsString)>, AppError> {
    let environment = keys
        .iter()
        .filter_map(|key| std::env::var_os(*key).map(|value| (OsString::from(*key), value)))
        .collect::<Vec<_>>();
    validate_environment_bytes(&environment)?;
    Ok(environment)
}

fn validate_environment_bytes(environment: &[(OsString, OsString)]) -> Result<(), AppError> {
    let encoded_bytes = environment.iter().try_fold(0usize, |total, (key, value)| {
        total
            .checked_add(key.as_encoded_bytes().len())?
            .checked_add(value.as_encoded_bytes().len())
    });
    if encoded_bytes.is_none_or(|bytes| bytes > PATH_BYTES_MAX) {
        Err(AppError::PathTooLarge)
    } else {
        Ok(())
    }
}
