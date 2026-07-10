use crate::command::CommandExecutor;
use crate::domain::Scope;
use crate::github::{GithubClient, GithubError};
use crate::jj::{JjClient, JjError};
use crate::plan::{Effect, EffectResult, JjEffect, Plan, PlanId, ReobserveBarrier};
use crate::state::{
    CheckpointState, CompletionOutcome, CompletionReceiptData, ExecutionSessionIdentity,
    OwnershipEffectState, SatisfactionSource, SatisfiedEffect, SessionError, StateError,
    StateStore,
};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::path::{Path, PathBuf};

mod private {
    pub trait Sealed {}
}

/// The immutable execution boundary owned by one sealed driver.
pub struct DriverScope<'a> {
    scope: &'a Scope,
    workspace_root: &'a Path,
}

impl<'a> DriverScope<'a> {
    pub(crate) fn new(scope: &'a Scope, workspace_root: &'a Path) -> Self {
        Self {
            scope,
            workspace_root,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverId(usize);

/// A checked effect backend. Implementations are sealed because only the
/// observing adapter that owns an external boundary may mint `PreparedEffect`.
pub trait EffectDriver: private::Sealed {
    type Error: Error + Send + Sync + 'static;

    fn execution_scope(&self) -> DriverScope<'_>;

    fn driver_id(&self) -> DriverId
    where
        Self: Sized,
    {
        DriverId(self as *const Self as usize)
    }

    fn check_effect(
        &mut self,
        request: &EffectRequest,
        effect: &Effect,
    ) -> Result<EffectCheck, Self::Error>;

    fn execute_prepared(
        &mut self,
        request: &EffectRequest,
        effect: &Effect,
        prepared: PreparedEffect,
    ) -> Result<(), Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectRequest {
    driver_id: DriverId,
    scope: Scope,
    workspace_root: PathBuf,
    plan_id: PlanId,
    effect_index: u32,
    session: ExecutionSessionIdentity,
}

impl EffectRequest {
    fn new(
        driver_id: DriverId,
        scope: Scope,
        workspace_root: PathBuf,
        plan_id: PlanId,
        effect_index: usize,
        session: ExecutionSessionIdentity,
    ) -> Self {
        let effect_index = u32::try_from(effect_index)
            .expect("validated effect count and cursor fit the durable u32 index");
        Self {
            driver_id,
            scope,
            workspace_root,
            plan_id,
            effect_index,
            session,
        }
    }

    #[cfg(test)]
    fn with_driver_id(&self, driver_id: DriverId) -> Self {
        Self {
            driver_id,
            ..self.clone()
        }
    }

    #[cfg(test)]
    fn with_scope(&self, scope: Scope) -> Self {
        Self {
            scope,
            ..self.clone()
        }
    }

    #[cfg(test)]
    fn with_workspace_root(&self, workspace_root: PathBuf) -> Self {
        Self {
            workspace_root,
            ..self.clone()
        }
    }

    #[cfg(test)]
    fn with_plan_id(&self, plan_id: PlanId) -> Self {
        Self {
            plan_id,
            ..self.clone()
        }
    }

    #[cfg(test)]
    fn with_effect_index(&self, effect_index: u32) -> Self {
        Self {
            effect_index,
            ..self.clone()
        }
    }

    #[cfg(test)]
    fn with_session(&self, session: ExecutionSessionIdentity) -> Self {
        Self {
            session,
            ..self.clone()
        }
    }
}

pub struct EffectCheck {
    request: EffectRequest,
    effect: Effect,
    state: EffectCheckState,
}

enum EffectCheckState {
    Satisfied(EffectResult),
    Prepared,
    Drift,
}

enum ExternalEffectState {
    Satisfied(EffectResult),
    Prepared(Box<PreparedEffect>),
    Drift,
}

pub struct PreparedEffect {
    request: EffectRequest,
    effect: Effect,
}

impl EffectCheck {
    pub(crate) fn satisfied(
        request: &EffectRequest,
        effect: &Effect,
        result: EffectResult,
    ) -> Self {
        assert!(crate::plan::result_matches(effect, &result));
        Self {
            request: request.clone(),
            effect: effect.clone(),
            state: EffectCheckState::Satisfied(result),
        }
    }

    pub(crate) fn prepared(request: &EffectRequest, effect: &Effect) -> Self {
        Self {
            request: request.clone(),
            effect: effect.clone(),
            state: EffectCheckState::Prepared,
        }
    }

    pub(crate) fn drift(request: &EffectRequest, effect: &Effect) -> Self {
        Self {
            request: request.clone(),
            effect: effect.clone(),
            state: EffectCheckState::Drift,
        }
    }

    fn into_state(
        self,
        request: &EffectRequest,
        expected: &Effect,
    ) -> Result<ExternalEffectState, DriverProofError> {
        if &self.request != request || &self.effect != expected {
            return Err(DriverProofError { _private: () });
        }
        Ok(match self.state {
            EffectCheckState::Satisfied(result) => ExternalEffectState::Satisfied(result),
            EffectCheckState::Prepared => ExternalEffectState::Prepared(Box::new(PreparedEffect {
                request: self.request,
                effect: self.effect,
            })),
            EffectCheckState::Drift => ExternalEffectState::Drift,
        })
    }
}

impl PreparedEffect {
    pub(crate) fn into_effect(
        self,
        request: &EffectRequest,
        expected: &Effect,
        consuming_driver_id: DriverId,
    ) -> Result<Effect, DriverProofError> {
        if &self.request != request
            || &self.effect != expected
            || request.driver_id != consuming_driver_id
        {
            return Err(DriverProofError { _private: () });
        }
        Ok(self.effect)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageOutcome {
    Complete,
    Reobserve(ReobserveBarrier),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionAcknowledgement {
    data: CompletionReceiptData,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StageCompletion {
    outcome: StageOutcome,
    acknowledgement: CompletionAcknowledgement,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingCompletion {
    plan_id: PlanId,
    outcome: StageOutcome,
    acknowledgement: CompletionAcknowledgement,
}

impl PendingCompletion {
    pub fn plan_id(&self) -> PlanId {
        self.plan_id
    }

    pub fn outcome(&self) -> StageOutcome {
        self.outcome
    }

    pub fn acknowledgement(&self) -> CompletionAcknowledgement {
        self.acknowledgement.clone()
    }
}

impl StageCompletion {
    pub fn outcome(&self) -> StageOutcome {
        self.outcome
    }

    pub fn acknowledgement(&self) -> CompletionAcknowledgement {
        self.acknowledgement.clone()
    }
}

impl PartialEq<StageOutcome> for StageCompletion {
    fn eq(&self, other: &StageOutcome) -> bool {
        self.outcome == *other
    }
}

impl PartialEq<StageCompletion> for StageOutcome {
    fn eq(&self, other: &StageCompletion) -> bool {
        *self == other.outcome
    }
}

impl CompletionAcknowledgement {
    #[cfg(test)]
    fn corrupt_occurrence_for_test(&self) -> Self {
        Self {
            data: self.data.clone().corrupt_occurrence(),
        }
    }

    #[cfg(test)]
    fn corrupt_plan_for_test(&self, plan_id: PlanId) -> Self {
        Self {
            data: self.data.clone().corrupt_plan(plan_id),
        }
    }

    #[cfg(test)]
    fn corrupt_proof_for_test(&self) -> Self {
        Self {
            data: self.data.clone().corrupt_proof(),
        }
    }

    #[cfg(test)]
    fn corrupt_outcome_for_test(&self) -> Self {
        Self {
            data: self.data.clone().corrupt_outcome(),
        }
    }

    #[cfg(test)]
    fn corrupt_generation_for_test(&self) -> Self {
        Self {
            data: self.data.clone().corrupt_generation(),
        }
    }

    #[cfg(test)]
    fn corrupt_store_for_test(&self) -> Self {
        Self {
            data: self.data.clone().corrupt_store(),
        }
    }
}

pub struct Executor;

impl Executor {
    pub fn execute_stage<D: EffectDriver>(
        store: &StateStore,
        plan: Plan,
        driver: &mut D,
    ) -> Result<StageCompletion, ExecutorError<D::Error>> {
        Self::run(store, Some(plan), None, driver)
    }

    pub fn execute_next_stage<D: EffectDriver>(
        store: &StateStore,
        acknowledgement: CompletionAcknowledgement,
        plan: Plan,
        driver: &mut D,
    ) -> Result<StageCompletion, ExecutorError<D::Error>> {
        Self::run(store, Some(plan), Some(acknowledgement), driver)
    }

    pub fn resume_stage<D: EffectDriver>(
        store: &StateStore,
        driver: &mut D,
    ) -> Result<StageCompletion, ExecutorError<D::Error>> {
        Self::run(store, None, None, driver)
    }

    pub fn execute_stage_in_session<D: EffectDriver>(
        session: &mut crate::state::LockedStateSession<'_>,
        plan: Plan,
        driver: &mut D,
    ) -> Result<StageCompletion, ExecutorError<D::Error>> {
        Self::run_locked(session, Some(plan), None, driver)
    }

    pub fn execute_next_stage_in_session<D: EffectDriver>(
        session: &mut crate::state::LockedStateSession<'_>,
        acknowledgement: CompletionAcknowledgement,
        plan: Plan,
        driver: &mut D,
    ) -> Result<StageCompletion, ExecutorError<D::Error>> {
        Self::run_locked(session, Some(plan), Some(acknowledgement), driver)
    }

    pub fn resume_stage_in_session<D: EffectDriver>(
        session: &mut crate::state::LockedStateSession<'_>,
        driver: &mut D,
    ) -> Result<StageCompletion, ExecutorError<D::Error>> {
        Self::run_locked(session, None, None, driver)
    }

    fn run<D: EffectDriver>(
        store: &StateStore,
        requested_plan: Option<Plan>,
        acknowledgement: Option<CompletionAcknowledgement>,
        driver: &mut D,
    ) -> Result<StageCompletion, ExecutorError<D::Error>> {
        let mut session = store.lock().map_err(ExecutorError::Session)?;
        Self::run_locked(&mut session, requested_plan, acknowledgement, driver)
    }

    fn run_locked<D: EffectDriver>(
        session: &mut crate::state::LockedStateSession<'_>,
        requested_plan: Option<Plan>,
        acknowledgement: Option<CompletionAcknowledgement>,
        driver: &mut D,
    ) -> Result<StageCompletion, ExecutorError<D::Error>> {
        let expected_scope = session.store().scope().clone();
        let expected_workspace = session.store().workspace_root().to_owned();
        let driver_scope = driver.execution_scope();
        let plan_scope_matches = requested_plan
            .as_ref()
            .is_none_or(|plan| plan.scope() == &expected_scope);
        if driver_scope.scope != &expected_scope
            || driver_scope.workspace_root != expected_workspace
            || !plan_scope_matches
        {
            return Err(ExecutorError::ScopeMismatch);
        }

        match (session.state().checkpoint(), requested_plan) {
            (CheckpointState::Idle, Some(plan)) => {
                if acknowledgement.is_some() {
                    return Err(ExecutorError::CompletionAcknowledgementMismatch);
                }
                session.start_plan(plan).map_err(ExecutorError::State)?;
            }
            (CheckpointState::Idle, None) => return Err(ExecutorError::NoCheckpoint),
            (CheckpointState::Executing(checkpoint), Some(plan)) => {
                if checkpoint.plan() != &plan {
                    return Err(ExecutorError::PlanMismatch {
                        expected: checkpoint.plan().id(),
                        observed: plan.id(),
                    });
                }
                if acknowledgement
                    .as_ref()
                    .is_some_and(|receipt| !checkpoint.was_started_by(&receipt.data))
                {
                    return Err(ExecutorError::CompletionAcknowledgementMismatch);
                }
            }
            (CheckpointState::Executing(_), None) => {}
            (CheckpointState::Completed(_), None) => {
                return stage_completion(session).map_err(ExecutorError::State);
            }
            (CheckpointState::Completed(checkpoint), Some(plan)) => {
                let completed_plan = checkpoint.plan().clone();
                let completion = stage_completion(session).map_err(ExecutorError::State)?;
                let Some(acknowledgement) = acknowledgement else {
                    if completed_plan == plan {
                        return Ok(completion);
                    }
                    return Err(ExecutorError::CompletionPending(Box::new(
                        PendingCompletion {
                            plan_id: completion.acknowledgement.data.plan_id(),
                            outcome: completion.outcome,
                            acknowledgement: completion.acknowledgement,
                        },
                    )));
                };
                if acknowledgement != completion.acknowledgement {
                    return Err(ExecutorError::CompletionAcknowledgementMismatch);
                }
                session
                    .start_next_plan(acknowledgement.data, plan)
                    .map_err(ExecutorError::State)?;
            }
        }

        let effect_count = match session.state().checkpoint() {
            CheckpointState::Executing(checkpoint) => checkpoint.plan().effects().len(),
            CheckpointState::Idle | CheckpointState::Completed(_) => {
                return Err(ExecutorError::State(StateError::InvalidCheckpoint))
            }
        };
        let visit_count_max = effect_count
            .checked_add(1)
            .ok_or(ExecutorError::State(StateError::InvalidCheckpoint))?;
        let mut visit_count = 0usize;

        loop {
            visit_count = visit_count
                .checked_add(1)
                .ok_or(ExecutorError::State(StateError::InvalidCheckpoint))?;
            if visit_count > visit_count_max {
                return Err(ExecutorError::State(StateError::InvalidCheckpoint));
            }

            let CheckpointState::Executing(checkpoint) = session.state().checkpoint() else {
                return Err(ExecutorError::State(StateError::InvalidCheckpoint));
            };
            let index = checkpoint.next_effect_index() as usize;
            if index == checkpoint.plan().effects().len() {
                session.publish_completion().map_err(ExecutorError::State)?;
                return stage_completion(session).map_err(ExecutorError::State);
            }
            let effect = checkpoint.plan().effects()[index].clone();

            match &effect {
                Effect::Ownership(_) => {
                    let prior_results = checkpoint.results().to_vec();
                    let state = session
                        .state()
                        .ownership_effect_state(&effect, &prior_results)
                        .map_err(ExecutorError::State)?;
                    match state {
                        OwnershipEffectState::Satisfied => session
                            .record_satisfied(SatisfiedEffect::new(
                                EffectResult::Satisfied,
                                SatisfactionSource::OwnershipPostcondition,
                            ))
                            .map_err(ExecutorError::State)?,
                        OwnershipEffectState::Ready => session
                            .record_satisfied(SatisfiedEffect::new(
                                EffectResult::Satisfied,
                                SatisfactionSource::OwnershipPrecondition,
                            ))
                            .map_err(ExecutorError::State)?,
                        OwnershipEffectState::Drift => {
                            return Err(ExecutorError::Drift {
                                effect_index: index,
                            });
                        }
                    }
                }
                Effect::Reobserve(reason) => {
                    session
                        .record_satisfied(SatisfiedEffect::new(
                            EffectResult::BarrierReached { reason: *reason },
                            SatisfactionSource::Barrier,
                        ))
                        .map_err(ExecutorError::State)?;
                }
                _ => execute_external(session, driver, index, &effect)?,
            }
        }
    }
}

fn stage_completion(
    session: &crate::state::LockedStateSession<'_>,
) -> Result<StageCompletion, StateError> {
    let data = session.completion_receipt()?;
    let outcome = match data.outcome() {
        CompletionOutcome::Complete => StageOutcome::Complete,
        CompletionOutcome::Reobserve(reason) => StageOutcome::Reobserve(reason),
    };
    Ok(StageCompletion {
        outcome,
        acknowledgement: CompletionAcknowledgement { data },
    })
}

fn execute_external<D: EffectDriver>(
    session: &mut crate::state::LockedStateSession<'_>,
    driver: &mut D,
    effect_index: usize,
    effect: &Effect,
) -> Result<(), ExecutorError<D::Error>> {
    let request = EffectRequest::new(
        driver.driver_id(),
        session.state().scope().clone(),
        driver.execution_scope().workspace_root.to_owned(),
        current_plan_id(session).map_err(ExecutorError::State)?,
        effect_index,
        session.execution_identity().map_err(ExecutorError::State)?,
    );
    let checked = driver
        .check_effect(&request, effect)
        .map_err(ExecutorError::Driver)?;
    match checked
        .into_state(&request, effect)
        .map_err(ExecutorError::DriverProof)?
    {
        ExternalEffectState::Satisfied(result) => {
            session
                .record_satisfied(SatisfiedEffect::new(
                    result,
                    SatisfactionSource::ExternalPostcondition,
                ))
                .map_err(ExecutorError::State)?;
            Ok(())
        }
        ExternalEffectState::Drift => Err(ExecutorError::Drift { effect_index }),
        ExternalEffectState::Prepared(prepared) => {
            let execution = driver.execute_prepared(&request, effect, *prepared);
            let observation = driver.check_effect(&request, effect);
            finish_external_attempt(
                session,
                effect_index,
                &request,
                effect,
                execution,
                observation,
            )
        }
    }
}

fn current_plan_id(session: &crate::state::LockedStateSession<'_>) -> Result<PlanId, StateError> {
    let CheckpointState::Executing(checkpoint) = session.state().checkpoint() else {
        return Err(StateError::InvalidCheckpoint);
    };
    Ok(checkpoint.plan().id())
}

fn finish_external_attempt<E>(
    session: &mut crate::state::LockedStateSession<'_>,
    effect_index: usize,
    request: &EffectRequest,
    effect: &Effect,
    execution: Result<(), E>,
    observation: Result<EffectCheck, E>,
) -> Result<(), ExecutorError<E>> {
    let observed_state = match observation {
        Ok(checked) => checked
            .into_state(request, effect)
            .map_err(ExecutorError::DriverProof)?,
        Err(observation) => {
            return match execution {
                Err(execution) => Err(ExecutorError::DriverAfterExecution {
                    execution,
                    observation,
                }),
                Ok(()) => Err(ExecutorError::ObservationAfterExecution(observation)),
            };
        }
    };

    if let ExternalEffectState::Satisfied(result) = observed_state {
        session
            .record_satisfied(SatisfiedEffect::new(
                result,
                SatisfactionSource::ExternalPostcondition,
            ))
            .map_err(ExecutorError::State)?;
        return Ok(());
    }
    match execution {
        Err(error) => Err(ExecutorError::Driver(error)),
        Ok(()) => Err(ExecutorError::Postcondition { effect_index }),
    }
}

#[derive(Debug)]
pub struct DriverProofError {
    _private: (),
}

impl Display for DriverProofError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("effect driver proof provenance does not match execution authority")
    }
}

impl Error for DriverProofError {}

pub enum ExecutorError<E> {
    Session(SessionError),
    State(StateError),
    Driver(E),
    ObservationAfterExecution(E),
    DriverAfterExecution { execution: E, observation: E },
    DriverProof(DriverProofError),
    ScopeMismatch,
    CompletionPending(Box<PendingCompletion>),
    CompletionAcknowledgementMismatch,
    NoCheckpoint,
    PlanMismatch { expected: PlanId, observed: PlanId },
    Drift { effect_index: usize },
    Postcondition { effect_index: usize },
}

impl<E: Display> Display for ExecutorError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Session(error) => Display::fmt(error, formatter),
            Self::State(error) => Display::fmt(error, formatter),
            Self::Driver(error) => write!(formatter, "effect driver failed: {error}"),
            Self::ObservationAfterExecution(error) => {
                write!(formatter, "post-effect observation failed: {error}")
            }
            Self::DriverAfterExecution {
                execution,
                observation,
            } => write!(
                formatter,
                "effect execution failed: {execution}; post-effect observation also failed: {observation}"
            ),
            Self::DriverProof(error) => Display::fmt(error, formatter),
            Self::ScopeMismatch => {
                formatter.write_str("effect driver, plan, and state store scopes do not match")
            }
            Self::CompletionPending(_) => formatter
                .write_str("a durable completion must be recovered and acknowledged first"),
            Self::CompletionAcknowledgementMismatch => formatter
                .write_str("completion acknowledgement does not match the durable receipt"),
            Self::NoCheckpoint => formatter.write_str("no durable execution checkpoint exists"),
            Self::PlanMismatch { .. } => {
                formatter.write_str("durable checkpoint belongs to another exact plan")
            }
            Self::Drift { effect_index } => {
                write!(formatter, "effect {effect_index} precondition drifted")
            }
            Self::Postcondition { effect_index } => write!(
                formatter,
                "effect {effect_index} postcondition was not established"
            ),
        }
    }
}

impl<E: fmt::Debug + Display> fmt::Debug for ExecutorError<E> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl<E: Error + 'static> Error for ExecutorError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Session(error) => Some(error),
            Self::State(error) => Some(error),
            Self::Driver(error) | Self::ObservationAfterExecution(error) => Some(error),
            Self::DriverAfterExecution { execution, .. } => Some(execution),
            Self::DriverProof(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub struct NoExternalEffects {
    scope: Scope,
    workspace_root: PathBuf,
}

impl NoExternalEffects {
    pub fn for_store(store: &StateStore) -> Self {
        Self {
            scope: store.scope().clone(),
            workspace_root: store.workspace_root().to_owned(),
        }
    }
}

#[derive(Debug)]
pub struct NoExternalEffectError;

impl Display for NoExternalEffectError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str("this executor has no external effect capability")
    }
}

impl Error for NoExternalEffectError {}

impl private::Sealed for NoExternalEffects {}

impl EffectDriver for NoExternalEffects {
    type Error = NoExternalEffectError;

    fn execution_scope(&self) -> DriverScope<'_> {
        DriverScope::new(&self.scope, &self.workspace_root)
    }

    fn check_effect(
        &mut self,
        _request: &EffectRequest,
        _effect: &Effect,
    ) -> Result<EffectCheck, Self::Error> {
        Err(NoExternalEffectError)
    }

    fn execute_prepared(
        &mut self,
        _request: &EffectRequest,
        _effect: &Effect,
        _prepared: PreparedEffect,
    ) -> Result<(), Self::Error> {
        Err(NoExternalEffectError)
    }
}

impl<E: CommandExecutor> private::Sealed for JjClient<'_, E> {}

impl<E: CommandExecutor> EffectDriver for JjClient<'_, E> {
    type Error = JjError;

    fn execution_scope(&self) -> DriverScope<'_> {
        DriverScope::new(self.scope(), self.workspace_root())
    }

    fn check_effect(
        &mut self,
        request: &EffectRequest,
        effect: &Effect,
    ) -> Result<EffectCheck, Self::Error> {
        if request.driver_id != self.driver_id() {
            return Err(JjError::AuthorityMismatch);
        }
        self.check_current_effect(request, effect)
    }

    fn execute_prepared(
        &mut self,
        request: &EffectRequest,
        effect: &Effect,
        prepared: PreparedEffect,
    ) -> Result<(), Self::Error> {
        let driver_id = self.driver_id();
        self.execute_checked_effect(driver_id, request, effect, prepared)
    }
}

impl<E: CommandExecutor> private::Sealed for GithubClient<'_, E> {}

impl<E: CommandExecutor> EffectDriver for GithubClient<'_, E> {
    type Error = GithubError;

    fn execution_scope(&self) -> DriverScope<'_> {
        DriverScope::new(self.scope(), self.workspace_root())
    }

    fn check_effect(
        &mut self,
        request: &EffectRequest,
        effect: &Effect,
    ) -> Result<EffectCheck, Self::Error> {
        if request.driver_id != self.driver_id() {
            return Err(GithubError::AuthorityMismatch);
        }
        self.check_current_effect(request, effect)
    }

    fn execute_prepared(
        &mut self,
        request: &EffectRequest,
        effect: &Effect,
        prepared: PreparedEffect,
    ) -> Result<(), Self::Error> {
        let driver_id = self.driver_id();
        self.execute_checked_effect(driver_id, request, effect, prepared)
    }
}

#[derive(Clone, Copy)]
enum EffectRoute {
    Jj,
    Github,
}

pub struct FullEffectDriver<'config, J, G> {
    jj: JjClient<'config, J>,
    github: GithubClient<'config, G>,
}

impl<'config, J: CommandExecutor, G: CommandExecutor> FullEffectDriver<'config, J, G> {
    pub fn new(jj: JjClient<'config, J>, github: GithubClient<'config, G>) -> Self {
        assert_eq!(jj.scope(), github.scope());
        assert_eq!(jj.workspace_root(), github.workspace_root());
        Self { jj, github }
    }
}

#[derive(Debug)]
pub enum FullEffectDriverError {
    Jj(JjError),
    Github(GithubError),
}

impl Display for FullEffectDriverError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Jj(error) => Display::fmt(error, formatter),
            Self::Github(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for FullEffectDriverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Jj(error) => Some(error),
            Self::Github(error) => Some(error),
        }
    }
}

impl<J: CommandExecutor, G: CommandExecutor> private::Sealed for FullEffectDriver<'_, J, G> {}

impl<J: CommandExecutor, G: CommandExecutor> EffectDriver for FullEffectDriver<'_, J, G> {
    type Error = FullEffectDriverError;

    fn execution_scope(&self) -> DriverScope<'_> {
        DriverScope::new(self.jj.scope(), self.jj.workspace_root())
    }

    fn check_effect(
        &mut self,
        request: &EffectRequest,
        effect: &Effect,
    ) -> Result<EffectCheck, Self::Error> {
        if request.driver_id != self.driver_id() {
            return Err(FullEffectDriverError::Jj(JjError::AuthorityMismatch));
        }
        match effect {
            Effect::Jj(JjEffect::PushHead { .. } | JjEffect::Rebase { .. }) => self
                .jj
                .check_current_effect(request, effect)
                .map_err(FullEffectDriverError::Jj),
            Effect::Jj(JjEffect::DeleteHead { .. }) | Effect::Github(_) => self
                .github
                .check_current_effect(request, effect)
                .map_err(FullEffectDriverError::Github),
            Effect::Ownership(_) | Effect::Reobserve(_) => {
                unreachable!("executor owns state and barrier effects")
            }
        }
    }

    fn execute_prepared(
        &mut self,
        request: &EffectRequest,
        effect: &Effect,
        prepared: PreparedEffect,
    ) -> Result<(), Self::Error> {
        let driver_id = self.driver_id();
        if request.driver_id != driver_id {
            return Err(FullEffectDriverError::Jj(JjError::AuthorityMismatch));
        }
        let route = match effect {
            Effect::Jj(JjEffect::PushHead { .. } | JjEffect::Rebase { .. }) => EffectRoute::Jj,
            Effect::Jj(JjEffect::DeleteHead { .. }) | Effect::Github(_) => EffectRoute::Github,
            Effect::Ownership(_) | Effect::Reobserve(_) => {
                unreachable!("executor owns state and barrier effects")
            }
        };
        match route {
            EffectRoute::Jj => self
                .jj
                .execute_checked_effect(driver_id, request, effect, prepared)
                .map_err(FullEffectDriverError::Jj),
            EffectRoute::Github => self
                .github
                .execute_checked_effect(driver_id, request, effect, prepared)
                .map_err(FullEffectDriverError::Github),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::ManagedSection;
    use crate::domain::{
        ChangeId, CommitId, HeadRef, LimitValues, Limits, PrLifecycle, PrNumber, RemoteName,
        RepositoryId, Scope,
    };
    use crate::plan::{BodyHash, GithubEffect, OwnershipEffect, PrOwnership, RemoteRefState};
    use crate::state::StorageFault;
    use std::fs;
    use std::io;
    use std::path::PathBuf;

    #[derive(Clone, Copy)]
    enum AttemptMode {
        Success,
        LostSuccess,
        Failure,
        InterruptAfterApply,
    }

    #[derive(Debug)]
    struct ModelError(&'static str);

    impl Display for ModelError {
        fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.0)
        }
    }

    impl Error for ModelError {}

    struct ModelDriver {
        scope: Scope,
        workspace_root: PathBuf,
        satisfied: Vec<Effect>,
        mode: AttemptMode,
        execute_count: usize,
        check_count: usize,
        fail_check_at: Option<usize>,
        interrupt_check_at: Option<usize>,
        interrupt_execute_at: Option<usize>,
    }

    impl ModelDriver {
        fn for_store(mode: AttemptMode, store: &StateStore) -> Self {
            Self {
                scope: store.scope().clone(),
                workspace_root: store.workspace_root().to_owned(),
                satisfied: Vec::new(),
                mode,
                execute_count: 0,
                check_count: 0,
                fail_check_at: None,
                interrupt_check_at: None,
                interrupt_execute_at: None,
            }
        }

        fn result(effect: &Effect) -> EffectResult {
            match effect {
                Effect::Github(GithubEffect::CreatePullRequest { .. }) => {
                    EffectResult::CreatedPullRequest {
                        number: PrNumber::new(7).unwrap(),
                    }
                }
                Effect::Jj(JjEffect::Rebase { .. }) => EffectResult::Rebased {
                    commit_id: commit('4'),
                },
                _ => EffectResult::Satisfied,
            }
        }
    }

    impl private::Sealed for ModelDriver {}

    impl EffectDriver for ModelDriver {
        type Error = ModelError;

        fn execution_scope(&self) -> DriverScope<'_> {
            DriverScope::new(&self.scope, &self.workspace_root)
        }

        fn check_effect(
            &mut self,
            request: &EffectRequest,
            effect: &Effect,
        ) -> Result<EffectCheck, Self::Error> {
            if request.driver_id != self.driver_id() {
                return Err(ModelError("observing driver provenance mismatch"));
            }
            self.check_count += 1;
            if self.interrupt_check_at == Some(self.check_count) {
                panic!("simulated process interruption before observation")
            }
            if self.fail_check_at == Some(self.check_count) {
                return Err(ModelError("injected observation failure"));
            }
            if self.satisfied.contains(effect) {
                Ok(EffectCheck::satisfied(
                    request,
                    effect,
                    Self::result(effect),
                ))
            } else {
                Ok(EffectCheck::prepared(request, effect))
            }
        }

        fn execute_prepared(
            &mut self,
            request: &EffectRequest,
            expected: &Effect,
            prepared: PreparedEffect,
        ) -> Result<(), Self::Error> {
            self.execute_count += 1;
            let driver_id = self.driver_id();
            let effect = prepared
                .into_effect(request, expected, driver_id)
                .map_err(|_| ModelError("prepared authority provenance mismatch"))?;
            if self.interrupt_execute_at == Some(self.execute_count) {
                self.satisfied.push(effect);
                panic!("simulated process interruption after indexed effect")
            }
            match self.mode {
                AttemptMode::Success => {
                    self.satisfied.push(effect);
                    Ok(())
                }
                AttemptMode::LostSuccess => {
                    self.satisfied.push(effect);
                    Err(ModelError("lost response"))
                }
                AttemptMode::Failure => Err(ModelError("command failed")),
                AttemptMode::InterruptAfterApply => {
                    self.satisfied.push(effect);
                    panic!("simulated process interruption after effect")
                }
            }
        }
    }

    fn limits() -> Limits {
        Limits::new(LimitValues::default()).unwrap()
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

    fn change(value: char) -> ChangeId {
        ChangeId::parse(std::iter::repeat_n(value, 32).collect::<String>()).unwrap()
    }

    fn commit(value: char) -> CommitId {
        CommitId::parse(std::iter::repeat_n(value, 40).collect::<String>()).unwrap()
    }

    fn ownership(value: char) -> PrOwnership {
        PrOwnership::generated(change(value)).unwrap()
    }

    fn managed_section(value: char) -> String {
        let change_id = change(value);
        ManagedSection::new(
            scope().source_repository().clone(),
            change_id.clone(),
            vec![change_id].into_boxed_slice(),
            limits(),
        )
        .unwrap()
        .render()
    }

    fn external_effects() -> Vec<Effect> {
        vec![
            Effect::Jj(JjEffect::PushHead {
                ownership: ownership('k'),
                expected: RemoteRefState::Absent,
                desired: commit('1'),
            }),
            Effect::Jj(JjEffect::DeleteHead {
                ownership: ownership('k'),
                expected: commit('1'),
            }),
            Effect::Jj(JjEffect::Rebase {
                change_id: change('k'),
                expected_commit: commit('1'),
                expected_parent: commit('2'),
                desired_parent: commit('3'),
            }),
            Effect::Github(GithubEffect::CreatePullRequest {
                change_id: change('k'),
                head: HeadRef::owned(&change('k')).unwrap(),
                base: HeadRef::parse("main").unwrap(),
                title: "title".to_owned(),
                managed_section: managed_section('k'),
            }),
            Effect::Github(GithubEffect::UpdateBase {
                ownership: ownership('k'),
                number: PrNumber::new(7).unwrap(),
                expected: HeadRef::parse("main").unwrap(),
                desired: HeadRef::parse("next").unwrap(),
            }),
            Effect::Github(GithubEffect::UpdateBody {
                ownership: ownership('k'),
                number: PrNumber::new(7).unwrap(),
                expected_hash: BodyHash::of(b"old"),
                desired_hash: BodyHash::of(b"new"),
                managed_section: managed_section('k'),
            }),
            Effect::Github(GithubEffect::SetLifecycle {
                ownership: ownership('k'),
                number: PrNumber::new(7).unwrap(),
                expected: PrLifecycle::Open,
                desired: PrLifecycle::Closed,
            }),
        ]
    }

    #[test]
    fn driver_scope_mismatch_fails_before_checkpoint_or_observation_for_every_scope_dimension() {
        let fixture = Fixture::new("driver-scope-mismatch");
        let effect = external_effects().remove(4);
        let plan = Plan::new(scope(), vec![effect].into_boxed_slice(), limits()).unwrap();
        let source_other = RepositoryId::parse("github.com/other-source/project").unwrap();
        let target_other = RepositoryId::parse("github.com/other-target/project").unwrap();
        let source = scope().source_repository().clone();
        let target = scope().target_repository().clone();
        let mismatches = [
            Scope::new(
                source_other,
                target.clone(),
                RemoteName::parse("origin").unwrap(),
                HeadRef::parse("main").unwrap(),
                "@".to_owned(),
            )
            .unwrap(),
            Scope::new(
                source.clone(),
                target_other,
                RemoteName::parse("origin").unwrap(),
                HeadRef::parse("main").unwrap(),
                "@".to_owned(),
            )
            .unwrap(),
            Scope::new(
                source.clone(),
                target.clone(),
                RemoteName::parse("upstream").unwrap(),
                HeadRef::parse("main").unwrap(),
                "@".to_owned(),
            )
            .unwrap(),
            Scope::new(
                source.clone(),
                target,
                RemoteName::parse("origin").unwrap(),
                HeadRef::parse("trunk").unwrap(),
                "@".to_owned(),
            )
            .unwrap(),
        ];

        for mismatch in mismatches {
            let mut driver = ModelDriver::for_store(AttemptMode::Success, &fixture.store);
            driver.scope = mismatch;
            assert!(matches!(
                Executor::execute_stage(&fixture.store, plan.clone(), &mut driver),
                Err(ExecutorError::ScopeMismatch)
            ));
            assert_eq!(driver.check_count, 0);
            assert_eq!(driver.execute_count, 0);
            assert!(!fixture.store.state_path().exists());
        }

        let mut workspace_mismatch = ModelDriver::for_store(AttemptMode::Success, &fixture.store);
        workspace_mismatch.workspace_root = fixture.root.join("different-workspace");
        assert!(matches!(
            Executor::execute_stage(&fixture.store, plan, &mut workspace_mismatch),
            Err(ExecutorError::ScopeMismatch)
        ));
        assert_eq!(workspace_mismatch.check_count, 0);
        assert_eq!(workspace_mismatch.execute_count, 0);
        assert!(!fixture.store.state_path().exists());

        let other_plan = Plan::new(
            Scope::new(
                RepositoryId::parse("github.com/other/project").unwrap(),
                scope().target_repository().clone(),
                RemoteName::parse("origin").unwrap(),
                HeadRef::parse("main").unwrap(),
                "@".to_owned(),
            )
            .unwrap(),
            vec![external_effects().remove(4)].into_boxed_slice(),
            limits(),
        )
        .unwrap();
        let mut exact_driver = ModelDriver::for_store(AttemptMode::Success, &fixture.store);
        assert!(matches!(
            Executor::execute_stage(&fixture.store, other_plan, &mut exact_driver),
            Err(ExecutorError::ScopeMismatch)
        ));
        assert_eq!(exact_driver.check_count, 0);
        assert_eq!(exact_driver.execute_count, 0);
        assert!(!fixture.store.state_path().exists());
    }

    #[test]
    fn prepared_authority_rejects_cross_effect_scope_driver_plan_index_and_session_use() {
        let fixture = Fixture::new("prepared-provenance");
        let effects = external_effects()
            .into_iter()
            .filter(|effect| !matches!(effect, Effect::Jj(JjEffect::Rebase { .. })))
            .take(2)
            .collect::<Vec<_>>();
        let plan = Plan::new(scope(), effects.clone().into_boxed_slice(), limits()).unwrap();
        let mut session = fixture.store.lock().unwrap();
        session.start_plan(plan.clone()).unwrap();
        let mut driver = ModelDriver::for_store(AttemptMode::Success, &fixture.store);
        let exact = EffectRequest::new(
            driver.driver_id(),
            fixture.store.scope().clone(),
            fixture.store.workspace_root().to_owned(),
            plan.id(),
            0,
            session.execution_identity().unwrap(),
        );

        let mint = |driver: &mut ModelDriver| {
            let checked = driver.check_effect(&exact, &effects[0]).unwrap();
            let ExternalEffectState::Prepared(prepared) =
                checked.into_state(&exact, &effects[0]).unwrap()
            else {
                panic!("fixture precondition must mint one prepared authority")
            };
            *prepared
        };
        assert_eq!(
            mint(&mut driver)
                .into_effect(&exact, &effects[0], driver.driver_id())
                .unwrap(),
            effects[0]
        );
        assert!(mint(&mut driver)
            .into_effect(&exact, &effects[1], driver.driver_id())
            .is_err());

        let other_driver = ModelDriver::for_store(AttemptMode::Success, &fixture.store);
        let wrong_driver = exact.with_driver_id(other_driver.driver_id());
        assert!(mint(&mut driver)
            .into_effect(&wrong_driver, &effects[0], driver.driver_id())
            .is_err());
        assert!(mint(&mut driver)
            .into_effect(&exact, &effects[0], other_driver.driver_id())
            .is_err());

        let other_plan = Plan::new(
            scope(),
            vec![effects[1].clone()].into_boxed_slice(),
            limits(),
        )
        .unwrap();
        assert!(mint(&mut driver)
            .into_effect(
                &exact.with_plan_id(other_plan.id()),
                &effects[0],
                driver.driver_id(),
            )
            .is_err());
        assert!(mint(&mut driver)
            .into_effect(&exact.with_effect_index(1), &effects[0], driver.driver_id(),)
            .is_err());

        let wrong_scope = Scope::new(
            RepositoryId::parse("github.com/other/project").unwrap(),
            scope().target_repository().clone(),
            RemoteName::parse("origin").unwrap(),
            HeadRef::parse("main").unwrap(),
            "@".to_owned(),
        )
        .unwrap();
        assert!(mint(&mut driver)
            .into_effect(
                &exact.with_scope(wrong_scope),
                &effects[0],
                driver.driver_id(),
            )
            .is_err());
        assert!(mint(&mut driver)
            .into_effect(
                &exact.with_workspace_root(fixture.root.join("other-workspace")),
                &effects[0],
                driver.driver_id(),
            )
            .is_err());
        drop(session);

        let resumed = fixture.store.lock().unwrap();
        assert!(mint(&mut driver)
            .into_effect(
                &exact.with_session(resumed.execution_identity().unwrap()),
                &effects[0],
                driver.driver_id(),
            )
            .is_err());
        drop(resumed);
        assert_eq!(other_driver.execute_count, 0);
    }

    #[test]
    fn completion_acknowledgement_rejects_every_wrong_receipt_dimension() {
        let fixture = Fixture::new("completion-receipt-dimensions");
        let plan = Plan::new(
            scope(),
            vec![Effect::Reobserve(ReobserveBarrier::Github)].into_boxed_slice(),
            limits(),
        )
        .unwrap();
        let next = Plan::new(
            scope(),
            vec![Effect::Reobserve(ReobserveBarrier::All)].into_boxed_slice(),
            limits(),
        )
        .unwrap();
        let mut driver = NoExternalEffects::for_store(&fixture.store);
        let completed = Executor::execute_stage(&fixture.store, plan.clone(), &mut driver).unwrap();
        let exact = completed.acknowledgement();

        for wrong in [
            exact.corrupt_occurrence_for_test(),
            exact.corrupt_plan_for_test(next.id()),
            exact.corrupt_proof_for_test(),
            exact.corrupt_outcome_for_test(),
            exact.corrupt_generation_for_test(),
            exact.corrupt_store_for_test(),
        ] {
            assert!(matches!(
                Executor::execute_next_stage(&fixture.store, wrong, next.clone(), &mut driver,),
                Err(ExecutorError::CompletionAcknowledgementMismatch)
            ));
            assert!(matches!(
                fixture.store.load().unwrap().checkpoint(),
                CheckpointState::Completed(checkpoint) if checkpoint.plan() == &plan
            ));
        }
    }

    #[test]
    fn exact_acknowledgement_reexecutes_equal_plan_after_precondition_restoration() {
        let fixture = Fixture::new("same-plan-restored-precondition");
        let effect = external_effects().remove(4);
        let plan = Plan::new(scope(), vec![effect.clone()].into_boxed_slice(), limits()).unwrap();
        let mut driver = ModelDriver::for_store(AttemptMode::Success, &fixture.store);
        let first = Executor::execute_stage(&fixture.store, plan.clone(), &mut driver).unwrap();
        assert_eq!(driver.execute_count, 1);

        driver.satisfied.retain(|observed| observed != &effect);
        let recovered = Executor::execute_stage(&fixture.store, plan.clone(), &mut driver).unwrap();
        assert_eq!(recovered.acknowledgement(), first.acknowledgement());
        assert_eq!(driver.execute_count, 1);

        let second = Executor::execute_next_stage(
            &fixture.store,
            first.acknowledgement(),
            plan,
            &mut driver,
        )
        .unwrap();
        assert_eq!(second, StageOutcome::Complete);
        assert_ne!(second.acknowledgement(), first.acknowledgement());
        assert_eq!(driver.execute_count, 2);
    }

    #[test]
    fn acknowledged_start_is_retry_safe_at_both_publication_fault_boundaries() {
        for same_plan in [false, true] {
            let mut effects = external_effects();
            let completed_effect = effects.remove(4);
            let next_effect = if same_plan {
                completed_effect.clone()
            } else {
                effects.remove(4)
            };
            let completed_plan = Plan::new(
                scope(),
                vec![completed_effect.clone()].into_boxed_slice(),
                limits(),
            )
            .unwrap();
            let next_plan =
                Plan::new(scope(), vec![next_effect].into_boxed_slice(), limits()).unwrap();

            let before = Fixture::new(&format!("ack-write-{same_plan}"));
            let mut driver = ModelDriver::for_store(AttemptMode::Success, &before.store);
            let completed =
                Executor::execute_stage(&before.store, completed_plan.clone(), &mut driver)
                    .unwrap();
            if same_plan {
                driver
                    .satisfied
                    .retain(|observed| observed != &completed_effect);
            }
            before.store.inject_fault(StorageFault::Write);
            assert!(matches!(
                Executor::execute_next_stage(
                    &before.store,
                    completed.acknowledgement(),
                    next_plan.clone(),
                    &mut driver,
                ),
                Err(ExecutorError::State(StateError::Io { .. }))
            ));
            assert_eq!(driver.execute_count, 1);
            assert!(matches!(
                before.store.load().unwrap().checkpoint(),
                CheckpointState::Completed(checkpoint) if checkpoint.plan() == &completed_plan
            ));
            Executor::execute_next_stage(
                &before.store,
                completed.acknowledgement(),
                next_plan.clone(),
                &mut driver,
            )
            .unwrap();
            assert_eq!(driver.execute_count, 2);

            let uncertain = Fixture::new(&format!("ack-directory-sync-{same_plan}"));
            let mut driver = ModelDriver::for_store(AttemptMode::Success, &uncertain.store);
            let completed =
                Executor::execute_stage(&uncertain.store, completed_plan.clone(), &mut driver)
                    .unwrap();
            if same_plan {
                driver
                    .satisfied
                    .retain(|observed| observed != &completed_effect);
            }
            uncertain.store.inject_fault(StorageFault::DirectorySync);
            assert!(matches!(
                Executor::execute_next_stage(
                    &uncertain.store,
                    completed.acknowledgement(),
                    next_plan.clone(),
                    &mut driver,
                ),
                Err(ExecutorError::State(
                    StateError::CommitDurabilityUnknown { .. }
                ))
            ));
            assert_eq!(driver.execute_count, 1);
            assert!(matches!(
                uncertain.store.load().unwrap().checkpoint(),
                CheckpointState::Executing(checkpoint)
                    if checkpoint.plan() == &next_plan && checkpoint.next_effect_index() == 0
            ));
            assert!(std::panic::catch_unwind(|| {
                panic!("simulated interruption after acknowledged start publication")
            })
            .is_err());
            Executor::execute_next_stage(
                &uncertain.store,
                completed.acknowledgement(),
                next_plan,
                &mut driver,
            )
            .unwrap();
            assert_eq!(driver.execute_count, 2);
        }
    }

    #[test]
    fn directory_sync_uncertainty_before_effect_zero_is_resumed_through_executor() {
        let fixture = Fixture::new("start-directory-sync");
        let effect = external_effects().remove(4);
        let plan = Plan::new(scope(), vec![effect].into_boxed_slice(), limits()).unwrap();
        let mut driver = ModelDriver::for_store(AttemptMode::Success, &fixture.store);
        fixture.store.inject_fault(StorageFault::DirectorySync);
        assert!(matches!(
            Executor::execute_stage(&fixture.store, plan.clone(), &mut driver),
            Err(ExecutorError::State(
                StateError::CommitDurabilityUnknown { .. }
            ))
        ));
        assert_eq!(driver.check_count, 0);
        assert_eq!(driver.execute_count, 0);
        assert!(matches!(
            fixture.store.load().unwrap().checkpoint(),
            CheckpointState::Executing(checkpoint)
                if checkpoint.plan() == &plan && checkpoint.next_effect_index() == 0
        ));
        assert!(std::panic::catch_unwind(|| {
            panic!("simulated interruption after uncertain effect-zero checkpoint")
        })
        .is_err());
        Executor::resume_stage(&fixture.store, &mut driver).unwrap();
        assert_eq!(driver.execute_count, 1);
    }

    fn canonical_stage(effect: Effect) -> Box<[Effect]> {
        if matches!(effect, Effect::Jj(JjEffect::Rebase { .. })) {
            vec![effect, Effect::Reobserve(ReobserveBarrier::LocalHistory)].into_boxed_slice()
        } else {
            vec![effect].into_boxed_slice()
        }
    }

    fn expected_stage_outcome(effect: &Effect) -> StageOutcome {
        if matches!(effect, Effect::Jj(JjEffect::Rebase { .. })) {
            StageOutcome::Reobserve(ReobserveBarrier::LocalHistory)
        } else {
            StageOutcome::Complete
        }
    }

    #[test]
    fn every_external_variant_accepts_postcondition_recovers_lost_success_and_stops_on_failure() {
        for (index, effect) in external_effects().into_iter().enumerate() {
            let fixture = Fixture::new(&format!("external-{index}"));
            let plan = Plan::new(scope(), canonical_stage(effect.clone()), limits()).unwrap();
            let mut lost = ModelDriver::for_store(AttemptMode::LostSuccess, &fixture.store);
            assert_eq!(
                Executor::execute_stage(&fixture.store, plan.clone(), &mut lost).unwrap(),
                expected_stage_outcome(&effect)
            );
            assert_eq!(lost.execute_count, 1);

            assert_eq!(
                Executor::execute_stage(&fixture.store, plan.clone(), &mut lost).unwrap(),
                expected_stage_outcome(&effect)
            );
            assert_eq!(lost.execute_count, 1, "variant {index} replayed");

            let failure_fixture = Fixture::new(&format!("external-failure-{index}"));
            let mut failed = ModelDriver::for_store(AttemptMode::Failure, &failure_fixture.store);
            assert!(matches!(
                Executor::execute_stage(&failure_fixture.store, plan, &mut failed),
                Err(ExecutorError::Driver(ModelError("command failed")))
            ));
            assert_eq!(failed.execute_count, 1);
            assert!(matches!(
                failure_fixture.store.load().unwrap().checkpoint(),
                CheckpointState::Executing(checkpoint)
                    if checkpoint.next_effect_index() == 0
            ));
        }
    }

    #[test]
    fn every_external_variant_resumes_after_interruption_before_and_after_the_effect() {
        for (index, effect) in external_effects().into_iter().enumerate() {
            let plan = Plan::new(scope(), canonical_stage(effect), limits()).unwrap();
            let before = Fixture::new(&format!("interrupt-before-{index}"));
            let mut interrupted = ModelDriver::for_store(AttemptMode::Success, &before.store);
            interrupted.interrupt_check_at = Some(1);
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = Executor::execute_stage(&before.store, plan.clone(), &mut interrupted);
            }))
            .is_err());
            assert!(matches!(
                before.store.load().unwrap().checkpoint(),
                CheckpointState::Executing(checkpoint)
                    if checkpoint.next_effect_index() == 0
            ));
            interrupted.interrupt_check_at = None;
            Executor::resume_stage(&before.store, &mut interrupted).unwrap();

            let after = Fixture::new(&format!("interrupt-after-{index}"));
            let mut interrupted =
                ModelDriver::for_store(AttemptMode::InterruptAfterApply, &after.store);
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = Executor::execute_stage(&after.store, plan.clone(), &mut interrupted);
            }))
            .is_err());
            assert!(matches!(
                after.store.load().unwrap().checkpoint(),
                CheckpointState::Executing(checkpoint)
                    if checkpoint.next_effect_index() == 0
            ));
            interrupted.mode = AttemptMode::Success;
            Executor::resume_stage(&after.store, &mut interrupted).unwrap();
            assert_eq!(interrupted.execute_count, 1, "variant {index} replayed");
        }
    }

    #[test]
    fn restart_model_interrupts_and_fails_cursor_publication_at_every_effect_index() {
        let effects = external_effects()
            .into_iter()
            .filter(|effect| !matches!(effect, Effect::Jj(JjEffect::Rebase { .. })))
            .collect::<Vec<_>>();
        let plan = Plan::new(scope(), effects.clone().into_boxed_slice(), limits()).unwrap();

        for target_index in 0..effects.len() {
            let before = Fixture::new(&format!("every-index-before-{target_index}"));
            let mut driver = ModelDriver::for_store(AttemptMode::Success, &before.store);
            driver.interrupt_check_at = Some(target_index * 2 + 1);
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = Executor::execute_stage(&before.store, plan.clone(), &mut driver);
            }))
            .is_err());
            assert!(matches!(
                before.store.load().unwrap().checkpoint(),
                CheckpointState::Executing(checkpoint)
                    if checkpoint.next_effect_index() as usize == target_index
            ));
            driver.interrupt_check_at = None;
            before.store.inject_fault(StorageFault::Write);
            assert!(matches!(
                Executor::resume_stage(&before.store, &mut driver),
                Err(ExecutorError::State(StateError::Io { .. }))
            ));
            assert!(matches!(
                before.store.load().unwrap().checkpoint(),
                CheckpointState::Executing(checkpoint)
                    if checkpoint.next_effect_index() as usize == target_index
            ));
            Executor::resume_stage(&before.store, &mut driver).unwrap();
            assert_eq!(driver.execute_count, effects.len());

            let after = Fixture::new(&format!("every-index-after-{target_index}"));
            let mut driver = ModelDriver::for_store(AttemptMode::Success, &after.store);
            driver.interrupt_execute_at = Some(target_index + 1);
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = Executor::execute_stage(&after.store, plan.clone(), &mut driver);
            }))
            .is_err());
            assert!(matches!(
                after.store.load().unwrap().checkpoint(),
                CheckpointState::Executing(checkpoint)
                    if checkpoint.next_effect_index() as usize == target_index
            ));
            driver.interrupt_execute_at = None;
            Executor::resume_stage(&after.store, &mut driver).unwrap();
            assert_eq!(driver.execute_count, effects.len());
        }
    }

    #[test]
    fn every_external_variant_recovers_checkpoint_publication_failures_before_and_after_effects() {
        for (index, effect) in external_effects().into_iter().enumerate() {
            let plan = Plan::new(scope(), canonical_stage(effect), limits()).unwrap();
            let before = Fixture::new(&format!("publication-before-{index}"));
            before.store.inject_fault(StorageFault::Write);
            let mut driver = ModelDriver::for_store(AttemptMode::Success, &before.store);
            assert!(matches!(
                Executor::execute_stage(&before.store, plan.clone(), &mut driver),
                Err(ExecutorError::State(StateError::Io { .. }))
            ));
            assert_eq!(driver.execute_count, 0);

            let after = Fixture::new(&format!("publication-after-{index}"));
            let mut session = after.store.lock().unwrap();
            session.start_plan(plan.clone()).unwrap();
            drop(session);
            after.store.inject_fault(StorageFault::Write);
            let mut driver = ModelDriver::for_store(AttemptMode::Success, &after.store);
            assert!(matches!(
                Executor::execute_stage(&after.store, plan.clone(), &mut driver),
                Err(ExecutorError::State(StateError::Io { .. }))
            ));
            assert_eq!(driver.execute_count, 1);
            Executor::resume_stage(&after.store, &mut driver).unwrap();
            assert_eq!(driver.execute_count, 1, "variant {index} replayed");

            let uncertain = Fixture::new(&format!("publication-uncertain-{index}"));
            let mut session = uncertain.store.lock().unwrap();
            session.start_plan(plan).unwrap();
            drop(session);
            uncertain.store.inject_fault(StorageFault::DirectorySync);
            let mut driver = ModelDriver::for_store(AttemptMode::Success, &uncertain.store);
            assert!(matches!(
                Executor::resume_stage(&uncertain.store, &mut driver),
                Err(ExecutorError::State(
                    StateError::CommitDurabilityUnknown { .. }
                ))
            ));
            Executor::resume_stage(&uncertain.store, &mut driver).unwrap();
            assert_eq!(driver.execute_count, 1);
        }
    }

    #[test]
    fn execution_and_observation_failures_remain_separate_and_cursor_does_not_advance() {
        let fixture = Fixture::new("dual-error");
        let mut effects = external_effects();
        let effect = effects.remove(4);
        let plan = Plan::new(scope(), vec![effect].into_boxed_slice(), limits()).unwrap();
        let mut driver = ModelDriver::for_store(AttemptMode::Failure, &fixture.store);
        driver.fail_check_at = Some(2);
        assert!(matches!(
            Executor::execute_stage(&fixture.store, plan, &mut driver),
            Err(ExecutorError::DriverAfterExecution {
                execution: ModelError("command failed"),
                observation: ModelError("injected observation failure"),
            })
        ));
        assert!(matches!(
            fixture.store.load().unwrap().checkpoint(),
            CheckpointState::Executing(checkpoint)
                if checkpoint.next_effect_index() == 0
        ));
    }

    #[test]
    fn every_ownership_variant_recovers_publication_failure_and_accepts_its_exact_postcondition() {
        let fixture = Fixture::new("ownership-variants");
        let change_id = change('k');
        let number = PrNumber::new(7).unwrap();
        let install = Plan::new(
            scope(),
            vec![Effect::Ownership(OwnershipEffect::InstallObserved {
                ownership: ownership('k'),
                number,
                expected_absent: true,
                lifecycle: PrLifecycle::Open,
            })]
            .into_boxed_slice(),
            limits(),
        )
        .unwrap();
        let mut driver = NoExternalEffects::for_store(&fixture.store);
        let mut session = fixture.store.lock().unwrap();
        session.start_plan(install.clone()).unwrap();
        drop(session);
        fixture.store.inject_fault(StorageFault::Write);
        assert!(matches!(
            Executor::resume_stage(&fixture.store, &mut driver),
            Err(ExecutorError::State(StateError::Io { .. }))
        ));
        Executor::resume_stage(&fixture.store, &mut driver).unwrap();
        Executor::execute_stage(&fixture.store, install.clone(), &mut driver).unwrap();
        assert_ownership_directory_sync_restart(&fixture.store, &install, &mut driver);

        let update = Plan::new(
            scope(),
            vec![Effect::Ownership(OwnershipEffect::UpdateLifecycle {
                change_id: change_id.clone(),
                expected_number: number,
                expected: PrLifecycle::Open,
                desired: PrLifecycle::Closed,
            })]
            .into_boxed_slice(),
            limits(),
        )
        .unwrap();
        assert_ownership_publication_restart(&fixture.store, &update, &mut driver);
        Executor::execute_stage(&fixture.store, update.clone(), &mut driver).unwrap();
        assert_ownership_directory_sync_restart(&fixture.store, &update, &mut driver);

        let historicize = Plan::new(
            scope(),
            vec![Effect::Ownership(OwnershipEffect::Historicize {
                change_id: change_id.clone(),
                expected_number: number,
                expected_lifecycle: PrLifecycle::Closed,
            })]
            .into_boxed_slice(),
            limits(),
        )
        .unwrap();
        assert_ownership_publication_restart(&fixture.store, &historicize, &mut driver);
        Executor::execute_stage(&fixture.store, historicize.clone(), &mut driver).unwrap();
        assert_ownership_directory_sync_restart(&fixture.store, &historicize, &mut driver);

        let reactivate = Plan::new(
            scope(),
            vec![Effect::Ownership(OwnershipEffect::Reactivate {
                change_id,
                expected_number: number,
                expected_lifecycle: PrLifecycle::Closed,
            })]
            .into_boxed_slice(),
            limits(),
        )
        .unwrap();
        assert_ownership_publication_restart(&fixture.store, &reactivate, &mut driver);
        Executor::execute_stage(&fixture.store, reactivate.clone(), &mut driver).unwrap();
        assert_ownership_directory_sync_restart(&fixture.store, &reactivate, &mut driver);

        let created_fixture = Fixture::new("install-created");
        let created_change = change('l');
        let create = Effect::Github(GithubEffect::CreatePullRequest {
            change_id: created_change.clone(),
            head: HeadRef::owned(&created_change).unwrap(),
            base: HeadRef::parse("main").unwrap(),
            title: "created".to_owned(),
            managed_section: managed_section('l'),
        });
        let plan = Plan::new(
            scope(),
            vec![
                create,
                Effect::Ownership(OwnershipEffect::InstallCreated {
                    change_id: created_change.clone(),
                    expected_absent: true,
                    create_effect_index: 0,
                    lifecycle: PrLifecycle::Open,
                }),
            ]
            .into_boxed_slice(),
            limits(),
        )
        .unwrap();
        let mut session = created_fixture.store.lock().unwrap();
        session.start_plan(plan.clone()).unwrap();
        session
            .record_satisfied(SatisfiedEffect::new(
                EffectResult::CreatedPullRequest { number },
                SatisfactionSource::ExternalPostcondition,
            ))
            .unwrap();
        drop(session);
        created_fixture.store.inject_fault(StorageFault::Write);
        let mut created_driver = NoExternalEffects::for_store(&created_fixture.store);
        assert!(matches!(
            Executor::resume_stage(&created_fixture.store, &mut created_driver),
            Err(ExecutorError::State(StateError::Io { .. }))
        ));
        Executor::resume_stage(&created_fixture.store, &mut created_driver).unwrap();
        assert_eq!(
            created_fixture
                .store
                .load()
                .unwrap()
                .verified()
                .get(&created_change)
                .unwrap()
                .number(),
            number
        );

        let uncertain_created = Fixture::new("install-created-directory-sync");
        let mut session = uncertain_created.store.lock().unwrap();
        session.start_plan(plan).unwrap();
        session
            .record_satisfied(SatisfiedEffect::new(
                EffectResult::CreatedPullRequest { number },
                SatisfactionSource::ExternalPostcondition,
            ))
            .unwrap();
        drop(session);
        uncertain_created
            .store
            .inject_fault(StorageFault::DirectorySync);
        let mut uncertain_driver = NoExternalEffects::for_store(&uncertain_created.store);
        assert!(matches!(
            Executor::resume_stage(&uncertain_created.store, &mut uncertain_driver),
            Err(ExecutorError::State(
                StateError::CommitDurabilityUnknown { .. }
            ))
        ));
        assert!(std::panic::catch_unwind(|| {
            panic!("simulated interruption after InstallCreated cursor rename")
        })
        .is_err());
        Executor::resume_stage(&uncertain_created.store, &mut uncertain_driver).unwrap();
        assert_eq!(
            uncertain_created
                .store
                .load()
                .unwrap()
                .verified()
                .get(&created_change)
                .unwrap()
                .number(),
            number
        );
    }

    fn assert_ownership_publication_restart(
        store: &StateStore,
        plan: &Plan,
        driver: &mut NoExternalEffects,
    ) {
        let mut session = store.lock().unwrap();
        let completed_receipt = session.completion_receipt().unwrap();
        session
            .start_next_plan(completed_receipt, plan.clone())
            .unwrap();
        drop(session);
        store.inject_fault(StorageFault::Write);
        assert!(matches!(
            Executor::resume_stage(store, driver),
            Err(ExecutorError::State(StateError::Io { .. }))
        ));
        Executor::resume_stage(store, driver).unwrap();
    }

    fn assert_ownership_directory_sync_restart(
        store: &StateStore,
        plan: &Plan,
        driver: &mut NoExternalEffects,
    ) {
        let completed = Executor::resume_stage(store, driver).unwrap();
        let mut session = store.lock().unwrap();
        session
            .start_next_plan(completed.acknowledgement().data.clone(), plan.clone())
            .unwrap();
        drop(session);
        store.inject_fault(StorageFault::DirectorySync);
        assert!(matches!(
            Executor::resume_stage(store, driver),
            Err(ExecutorError::State(
                StateError::CommitDurabilityUnknown { .. }
            ))
        ));
        assert!(std::panic::catch_unwind(|| {
            panic!("simulated interruption after ownership cursor rename")
        })
        .is_err());
        Executor::resume_stage(store, driver).unwrap();
    }

    #[test]
    fn rebase_result_is_durable_before_the_dependent_terminal_barrier() {
        let fixture = Fixture::new("rebase-result-binding");
        let rebase = external_effects()
            .into_iter()
            .find(|effect| matches!(effect, Effect::Jj(JjEffect::Rebase { .. })))
            .unwrap();
        let plan = Plan::new(
            scope(),
            vec![rebase, Effect::Reobserve(ReobserveBarrier::LocalHistory)].into_boxed_slice(),
            limits(),
        )
        .unwrap();
        let mut session = fixture.store.lock().unwrap();
        session.start_plan(plan).unwrap();
        drop(session);
        fixture.store.inject_fault(StorageFault::DirectorySync);
        let mut driver = ModelDriver::for_store(AttemptMode::Success, &fixture.store);
        assert!(matches!(
            Executor::resume_stage(&fixture.store, &mut driver),
            Err(ExecutorError::State(
                StateError::CommitDurabilityUnknown { .. }
            ))
        ));
        assert!(matches!(
            fixture.store.load().unwrap().checkpoint(),
            CheckpointState::Executing(checkpoint)
                if checkpoint.next_effect_index() == 1
                    && matches!(
                        checkpoint.results()[0].as_ref(),
                        Some(EffectResult::Rebased { .. })
                    )
                    && checkpoint.results()[1].is_none()
        ));

        assert!(std::panic::catch_unwind(|| {
            panic!("simulated process interruption at Rebase terminal barrier index")
        })
        .is_err());
        fixture.store.inject_fault(StorageFault::Write);
        assert!(matches!(
            Executor::resume_stage(&fixture.store, &mut driver),
            Err(ExecutorError::State(StateError::Io { .. }))
        ));
        assert!(matches!(
            fixture.store.load().unwrap().checkpoint(),
            CheckpointState::Executing(checkpoint)
                if checkpoint.next_effect_index() == 1
                    && checkpoint.results()[1].is_none()
        ));
        assert_eq!(
            Executor::resume_stage(&fixture.store, &mut driver).unwrap(),
            StageOutcome::Reobserve(ReobserveBarrier::LocalHistory)
        );
        assert_eq!(driver.execute_count, 1);
    }

    #[test]
    fn external_completion_publication_retry_never_repeats_the_proven_effect() {
        for fault in [StorageFault::Write, StorageFault::DirectorySync] {
            let fixture = Fixture::new(&format!("external-completion-{fault:?}"));
            let effect = external_effects().remove(4);
            let plan = Plan::new(scope(), vec![effect].into_boxed_slice(), limits()).unwrap();
            let mut session = fixture.store.lock().unwrap();
            session.start_plan(plan.clone()).unwrap();
            session
                .record_satisfied(SatisfiedEffect::new(
                    EffectResult::Satisfied,
                    SatisfactionSource::ExternalPostcondition,
                ))
                .unwrap();
            drop(session);

            let mut driver = ModelDriver::for_store(AttemptMode::Success, &fixture.store);
            driver.execute_count = 1;
            fixture.store.inject_fault(fault);
            assert!(Executor::resume_stage(&fixture.store, &mut driver).is_err());
            assert_eq!(
                Executor::resume_stage(&fixture.store, &mut driver).unwrap(),
                StageOutcome::Complete
            );
            assert_eq!(driver.execute_count, 1);
            assert!(matches!(
                fixture.store.load().unwrap().checkpoint(),
                CheckpointState::Completed(checkpoint) if checkpoint.plan() == &plan
            ));
        }
    }

    #[test]
    fn every_terminal_outcome_survives_final_publication_failure_and_restart() {
        let outcomes = [
            (None, StageOutcome::Complete),
            (
                Some(ReobserveBarrier::LocalHistory),
                StageOutcome::Reobserve(ReobserveBarrier::LocalHistory),
            ),
            (
                Some(ReobserveBarrier::RemoteRefs),
                StageOutcome::Reobserve(ReobserveBarrier::RemoteRefs),
            ),
            (
                Some(ReobserveBarrier::Github),
                StageOutcome::Reobserve(ReobserveBarrier::Github),
            ),
            (
                Some(ReobserveBarrier::All),
                StageOutcome::Reobserve(ReobserveBarrier::All),
            ),
        ];

        for (index, (barrier, expected)) in outcomes.into_iter().enumerate() {
            for fault in [StorageFault::Write, StorageFault::DirectorySync] {
                let fixture = Fixture::new(&format!("completion-{index}-{fault:?}"));
                let effect = match barrier {
                    Some(reason) => Effect::Reobserve(reason),
                    None => Effect::Ownership(OwnershipEffect::InstallObserved {
                        ownership: ownership('k'),
                        number: PrNumber::new(7).unwrap(),
                        expected_absent: true,
                        lifecycle: PrLifecycle::Open,
                    }),
                };
                let result = match barrier {
                    Some(reason) => EffectResult::BarrierReached { reason },
                    None => EffectResult::Satisfied,
                };
                let source = if barrier.is_some() {
                    SatisfactionSource::Barrier
                } else {
                    SatisfactionSource::OwnershipPrecondition
                };
                let plan = Plan::new(scope(), vec![effect].into_boxed_slice(), limits()).unwrap();
                let mut session = fixture.store.lock().unwrap();
                session.start_plan(plan.clone()).unwrap();
                session
                    .record_satisfied(SatisfiedEffect::new(result, source))
                    .unwrap();
                drop(session);

                fixture.store.inject_fault(fault);
                let mut driver = NoExternalEffects::for_store(&fixture.store);
                let failed = Executor::resume_stage(&fixture.store, &mut driver);
                assert!(matches!(
                    (fault, failed),
                    (
                        StorageFault::Write,
                        Err(ExecutorError::State(StateError::Io { .. }))
                    ) | (
                        StorageFault::DirectorySync,
                        Err(ExecutorError::State(
                            StateError::CommitDurabilityUnknown { .. }
                        ))
                    )
                ));

                let mut restarted = NoExternalEffects::for_store(&fixture.store);
                assert_eq!(
                    Executor::resume_stage(&fixture.store, &mut restarted).unwrap(),
                    expected
                );
                assert_eq!(
                    Executor::resume_stage(&fixture.store, &mut restarted).unwrap(),
                    expected
                );
                assert!(matches!(
                    fixture.store.load().unwrap().checkpoint(),
                    CheckpointState::Completed(checkpoint) if checkpoint.plan() == &plan
                ));
            }
        }
    }

    #[test]
    fn exact_maximum_effect_count_executes_iteratively_and_one_more_is_rejected() {
        let effects = (0..limits().effect_count_max())
            .map(|index| {
                Effect::Github(GithubEffect::UpdateBase {
                    ownership: ownership('k'),
                    number: PrNumber::new((index + 1) as u64).unwrap(),
                    expected: HeadRef::parse("main").unwrap(),
                    desired: HeadRef::parse("next").unwrap(),
                })
            })
            .collect::<Vec<_>>();
        let maximum = Plan::new(scope(), effects.clone().into_boxed_slice(), limits()).unwrap();
        let fixture = Fixture::new("maximum-effects");
        let mut driver = ModelDriver::for_store(AttemptMode::Success, &fixture.store);
        assert_eq!(
            Executor::execute_stage(&fixture.store, maximum, &mut driver).unwrap(),
            StageOutcome::Complete
        );
        assert_eq!(driver.execute_count, limits().effect_count_max());
        assert!(Plan::new(
            scope(),
            effects
                .into_iter()
                .chain(std::iter::once(Effect::Github(GithubEffect::UpdateBase {
                    ownership: ownership('k'),
                    number: PrNumber::new(10_000).unwrap(),
                    expected: HeadRef::parse("main").unwrap(),
                    desired: HeadRef::parse("next").unwrap(),
                })))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            limits(),
        )
        .is_err());
    }

    struct Fixture {
        root: PathBuf,
        store: StateStore,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            for sequence in 0..1_000 {
                let root = std::env::temp_dir().join(format!(
                    "almighty-push-executor-model-{label}-{}-{sequence}",
                    std::process::id()
                ));
                match fs::create_dir(&root) {
                    Ok(()) => {
                        fs::create_dir(root.join(".jj")).unwrap();
                        let root = root.canonicalize().unwrap();
                        let store = StateStore::open(&root, scope(), limits()).unwrap();
                        return Self { root, store };
                    }
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("fixture creation failed: {error}"),
                }
            }
            panic!("fixture attempts exhausted")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
}
