use crate::body::{BodyError, BodyMerge, ManagedBody, ManagedSection};
use crate::domain::{
    ChangeId, CommitId, HeadRef, Limits, PrLifecycle, PrNumber, Scope, SelectedChain,
};
use crate::github::{GithubSnapshot, ObservedPr};
use crate::state::{ManagedPr, StateV3};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Write};

const TITLE_BYTES_MAX: usize = 512;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct PlanId([u8; 32]);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BodyHash([u8; 32]);

impl BodyHash {
    pub fn of(body: &[u8]) -> Self {
        Self(Sha256::digest(body).into())
    }
}

impl PlanId {
    pub fn to_hex(self) -> String {
        use std::fmt::Write as _;

        let mut output = String::with_capacity(64);
        for byte in self.0 {
            write!(output, "{byte:02x}").expect("writing to String cannot fail");
        }
        output
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RemoteRefState {
    Absent,
    At(CommitId),
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrOwnership {
    change_id: ChangeId,
    head: HeadRef,
    proof: PrOwnershipProof,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
enum PrOwnershipProof {
    Generated,
    ValidatedLegacy {
        number: PrNumber,
        observation_digest: [u8; 32],
    },
}

impl PrOwnership {
    pub fn generated(change_id: ChangeId) -> Result<Self, crate::domain::DomainError> {
        let head = HeadRef::owned(&change_id)?;
        Ok(Self {
            change_id,
            head,
            proof: PrOwnershipProof::Generated,
        })
    }

    pub fn change_id(&self) -> &ChangeId {
        &self.change_id
    }

    pub fn head(&self) -> &HeadRef {
        &self.head
    }

    pub fn is_validated_legacy(&self) -> bool {
        matches!(self.proof, PrOwnershipProof::ValidatedLegacy { .. })
    }

    pub(crate) fn validated_legacy(
        change_id: ChangeId,
        head: HeadRef,
        number: PrNumber,
        observation_digest: [u8; 32],
    ) -> Self {
        Self {
            change_id,
            head,
            proof: PrOwnershipProof::ValidatedLegacy {
                number,
                observation_digest,
            },
        }
    }

    pub(crate) fn legacy_evidence(&self) -> Option<(PrNumber, [u8; 32])> {
        match self.proof {
            PrOwnershipProof::Generated => None,
            PrOwnershipProof::ValidatedLegacy {
                number,
                observation_digest,
            } => Some((number, observation_digest)),
        }
    }

    fn validate_for_pr(&self, number: PrNumber) -> Result<(), PlanError> {
        match self.proof {
            PrOwnershipProof::Generated => {
                if self.head
                    != HeadRef::owned(&self.change_id).map_err(|_| PlanError::InvalidEffect)?
                {
                    return Err(PlanError::InvalidEffect);
                }
            }
            PrOwnershipProof::ValidatedLegacy {
                number: evidence_number,
                ..
            } if evidence_number == number => {}
            PrOwnershipProof::ValidatedLegacy { .. } => return Err(PlanError::InvalidEffect),
        }
        Ok(())
    }

    fn validate_for_head(&self) -> Result<(), PlanError> {
        match self.proof {
            PrOwnershipProof::Generated => {
                if self.head
                    != HeadRef::owned(&self.change_id).map_err(|_| PlanError::InvalidEffect)?
                {
                    return Err(PlanError::InvalidEffect);
                }
            }
            PrOwnershipProof::ValidatedLegacy { .. } => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Effect {
    Jj(JjEffect),
    Github(GithubEffect),
    Ownership(OwnershipEffect),
    Reobserve(ReobserveBarrier),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum JjEffect {
    PushHead {
        ownership: PrOwnership,
        expected: RemoteRefState,
        desired: CommitId,
    },
    DeleteHead {
        ownership: PrOwnership,
        expected: CommitId,
    },
    Rebase {
        change_id: ChangeId,
        expected_commit: CommitId,
        expected_parent: CommitId,
        desired_parent: CommitId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum GithubEffect {
    CreatePullRequest {
        change_id: ChangeId,
        head: HeadRef,
        base: HeadRef,
        title: String,
        managed_section: String,
    },
    UpdateBase {
        ownership: PrOwnership,
        number: PrNumber,
        expected: HeadRef,
        desired: HeadRef,
    },
    UpdateBody {
        ownership: PrOwnership,
        number: PrNumber,
        expected_hash: BodyHash,
        desired_hash: BodyHash,
        managed_section: String,
    },
    SetLifecycle {
        ownership: PrOwnership,
        number: PrNumber,
        expected: PrLifecycle,
        desired: PrLifecycle,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum OwnershipEffect {
    InstallCreated {
        change_id: ChangeId,
        expected_absent: bool,
        create_effect_index: u32,
        lifecycle: PrLifecycle,
    },
    InstallObserved {
        ownership: PrOwnership,
        number: PrNumber,
        expected_absent: bool,
        lifecycle: PrLifecycle,
    },
    UpdateLifecycle {
        change_id: ChangeId,
        expected_number: PrNumber,
        expected: PrLifecycle,
        desired: PrLifecycle,
    },
    Historicize {
        change_id: ChangeId,
        expected_number: PrNumber,
        expected_lifecycle: PrLifecycle,
    },
    Reactivate {
        change_id: ChangeId,
        expected_number: PrNumber,
        expected_lifecycle: PrLifecycle,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ReobserveBarrier {
    LocalHistory,
    RemoteRefs,
    Github,
    All,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DryRunMode {
    Full { delete_closed_heads: bool },
    NoPr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionMode {
    Full { delete_closed_heads: bool },
    NoPr,
    DryRun(DryRunMode),
}

#[derive(Clone, Copy, Debug)]
pub enum PlanningGithub<'a> {
    Observed(&'a GithubSnapshot),
    NotObserved,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesiredPr {
    change_id: ChangeId,
    head: HeadRef,
    base: HeadRef,
}

impl DesiredPr {
    pub fn change_id(&self) -> &ChangeId {
        &self.change_id
    }

    pub fn head(&self) -> &HeadRef {
        &self.head
    }

    pub fn base(&self) -> &HeadRef {
        &self.base
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DryRunPlan {
    id: PlanId,
    scope: Scope,
    actions: Box<[String]>,
}

impl DryRunPlan {
    pub fn id(&self) -> PlanId {
        self.id
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn actions(&self) -> &[String] {
        &self.actions
    }

    pub fn action_count(&self) -> usize {
        self.actions.len()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanningOutcome {
    Executable(Plan),
    DryRun(DryRunPlan),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DerivedPlan {
    desired_prs: Box<[DesiredPr]>,
    outcome: PlanningOutcome,
}

impl DerivedPlan {
    pub fn desired_prs(&self) -> &[DesiredPr] {
        &self.desired_prs
    }

    pub fn outcome(&self) -> &PlanningOutcome {
        &self.outcome
    }

    pub fn action_count(&self) -> usize {
        match &self.outcome {
            PlanningOutcome::Executable(plan) => plan.effects().len(),
            PlanningOutcome::DryRun(plan) => plan.action_count(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EffectResult {
    Satisfied,
    CreatedPullRequest { number: PrNumber },
    Rebased { commit_id: CommitId },
    BarrierReached { reason: ReobserveBarrier },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    id: PlanId,
    scope: Scope,
    effects: Box<[Effect]>,
}

impl Plan {
    pub fn new(scope: Scope, effects: Box<[Effect]>, limits: Limits) -> Result<Self, PlanError> {
        validate_effects(&scope, &effects, limits)?;
        let id = compute_id(&scope, &effects, limits)?;
        Ok(Self { id, scope, effects })
    }

    pub fn id(&self) -> PlanId {
        self.id
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn effects(&self) -> &[Effect] {
        &self.effects
    }

    /// One publication installs the plan, one follows every proven effect, and
    /// one installs its durable completion receipt. At 512 effects this is 514
    /// bounded state publications; each is capped by `state_bytes_max`.
    pub fn publication_count_max(&self) -> usize {
        self.effects.len().saturating_add(2)
    }

    pub fn dry_run_remaining(
        &self,
        next_effect_index: u32,
        limits: Limits,
    ) -> Result<DryRunPlan, PlanError> {
        self.validate(limits)?;
        let index = next_effect_index as usize;
        let remaining = self.effects.get(index..).ok_or(PlanError::InvalidEffect)?;
        Ok(DryRunPlan {
            id: self.id,
            scope: self.scope.clone(),
            actions: render_actions(remaining, limits.state_bytes_max())?,
        })
    }

    pub(crate) fn validate(&self, limits: Limits) -> Result<(), PlanError> {
        validate_effects(&self.scope, &self.effects, limits)?;
        if compute_id(&self.scope, &self.effects, limits)? != self.id {
            return Err(PlanError::IdentityMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanError {
    EffectCount,
    InvalidEffect,
    InvalidCreateReference,
    BarrierNotTerminal,
    IdentityMismatch,
    Serialization,
    EncodedBytes {
        bytes: usize,
        max: usize,
    },
    ScopeMismatch,
    GithubObservationRequired,
    GithubObservationForbidden,
    RemoteObservationMissing {
        head: HeadRef,
    },
    ObservationDrift {
        head: HeadRef,
    },
    OwnershipConflict {
        change_id: ChangeId,
    },
    OwnershipMissing {
        change_id: ChangeId,
    },
    LifecycleDrift {
        change_id: ChangeId,
        persisted: PrLifecycle,
        observed: PrLifecycle,
    },
    UnownedHead {
        head: HeadRef,
    },
    HistoryBound,
    MergedTip {
        change_id: ChangeId,
    },
    RebaseBaseMissing {
        base: HeadRef,
    },
    TitleInvalid {
        change_id: ChangeId,
    },
    Body(BodyError),
}

impl Display for PlanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EffectCount => formatter.write_str("plan effect count is outside configured bounds"),
            Self::InvalidEffect => formatter.write_str("plan contains an invalid or unbounded effect"),
            Self::InvalidCreateReference => formatter.write_str(
                "ownership effect does not reference its exact prior create effect",
            ),
            Self::BarrierNotTerminal => {
                formatter.write_str("re-observation barrier must terminate the plan")
            }
            Self::IdentityMismatch => formatter
                .write_str("plan identity does not match its canonical scope and effects"),
            Self::Serialization => formatter.write_str("canonical plan serialization failed"),
            Self::EncodedBytes { bytes, max } => write!(
                formatter,
                "canonical plan encoding uses {bytes} bytes; maximum is {max}"
            ),
            Self::ScopeMismatch => formatter.write_str("planner observations do not share one scope"),
            Self::GithubObservationRequired => {
                formatter.write_str("this execution mode requires a complete GitHub snapshot")
            }
            Self::GithubObservationForbidden => {
                formatter.write_str("no-pr planning forbids a GitHub snapshot")
            }
            Self::RemoteObservationMissing { head } => {
                write!(formatter, "exact remote observation is missing for {head}")
            }
            Self::ObservationDrift { head } => {
                write!(formatter, "jj and GitHub observations disagree for {head}")
            }
            Self::OwnershipConflict { change_id } => {
                write!(formatter, "managed ownership conflicts for change {change_id}")
            }
            Self::OwnershipMissing { change_id } => {
                write!(formatter, "managed ownership observation is missing for change {change_id}")
            }
            Self::LifecycleDrift {
                change_id,
                persisted,
                observed,
            } => write!(
                formatter,
                "managed change {change_id} cannot move from {persisted:?} to observed {observed:?}"
            ),
            Self::UnownedHead { head } => {
                write!(formatter, "observed head {head} has no exact managed identity")
            }
            Self::HistoryBound => {
                formatter.write_str("managed PR identity bound would be exceeded")
            }
            Self::MergedTip { change_id } => write!(
                formatter,
                "merged selected tip {change_id} has no active child; remove it from the selected stack",
            ),
            Self::RebaseBaseMissing { base } => {
                write!(formatter, "merged PR base {base} has no exact remote commit observation")
            }
            Self::TitleInvalid { change_id } => {
                write!(formatter, "change {change_id} has no bounded pull-request title")
            }
            Self::Body(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for PlanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Body(error) => Some(error),
            _ => None,
        }
    }
}

pub(crate) fn result_matches(effect: &Effect, result: &EffectResult) -> bool {
    if let (Effect::Reobserve(expected), EffectResult::BarrierReached { reason }) = (effect, result)
    {
        return expected == reason;
    }
    matches!(
        (effect, result),
        (
            Effect::Jj(JjEffect::Rebase { .. }),
            EffectResult::Rebased { .. }
        ) | (
            Effect::Github(GithubEffect::CreatePullRequest { .. }),
            EffectResult::CreatedPullRequest { .. }
        ) | (
            Effect::Jj(JjEffect::PushHead { .. } | JjEffect::DeleteHead { .. }),
            EffectResult::Satisfied
        ) | (
            Effect::Github(
                GithubEffect::UpdateBase { .. }
                    | GithubEffect::UpdateBody { .. }
                    | GithubEffect::SetLifecycle { .. }
            ),
            EffectResult::Satisfied
        ) | (Effect::Ownership(_), EffectResult::Satisfied)
    )
}

fn validate_effects(scope: &Scope, effects: &[Effect], limits: Limits) -> Result<(), PlanError> {
    if effects.len() > limits.effect_count_max() {
        return Err(PlanError::EffectCount);
    }
    let mut aggregate_text_bytes = 0usize;
    let mut contains_rebase = false;
    for (index, effect) in effects.iter().enumerate() {
        match effect {
            Effect::Jj(JjEffect::PushHead { ownership, .. }) => {
                ownership.validate_for_head()?;
            }
            Effect::Jj(JjEffect::DeleteHead { ownership, .. }) => {
                ownership.validate_for_head()?;
            }
            Effect::Jj(JjEffect::Rebase {
                expected_commit,
                expected_parent,
                desired_parent,
                ..
            }) => {
                if expected_commit == desired_parent || expected_parent == desired_parent {
                    return Err(PlanError::InvalidEffect);
                }
                contains_rebase = true;
            }
            Effect::Github(GithubEffect::CreatePullRequest {
                change_id,
                head,
                title,
                managed_section,
                ..
            }) => {
                if title.is_empty() || title.len() > TITLE_BYTES_MAX || title.contains('\0') {
                    return Err(PlanError::InvalidEffect);
                }
                validate_generated_ownership(scope, change_id, head, managed_section, limits)?;
                aggregate_text_bytes = aggregate_text_bytes
                    .checked_add(title.len())
                    .and_then(|size| size.checked_add(managed_section.len()))
                    .ok_or(PlanError::InvalidEffect)?;
            }
            Effect::Github(GithubEffect::UpdateBase {
                ownership,
                number,
                expected,
                desired,
            }) => {
                ownership.validate_for_pr(*number)?;
                if expected == desired {
                    return Err(PlanError::InvalidEffect);
                }
            }
            Effect::Github(GithubEffect::UpdateBody {
                ownership,
                number,
                expected_hash,
                desired_hash,
                managed_section,
            }) => {
                ownership.validate_for_pr(*number)?;
                let section = ManagedSection::parse(managed_section, limits)
                    .map_err(|_| PlanError::InvalidEffect)?;
                if section.source_repository() != scope.source_repository()
                    || section.current_change() != ownership.change_id()
                    || expected_hash == desired_hash
                {
                    return Err(PlanError::InvalidEffect);
                }
                aggregate_text_bytes = aggregate_text_bytes
                    .checked_add(managed_section.len())
                    .ok_or(PlanError::InvalidEffect)?;
            }
            Effect::Github(GithubEffect::SetLifecycle {
                ownership,
                number,
                expected,
                desired,
            }) => {
                ownership.validate_for_pr(*number)?;
                if !matches!(
                    (expected, desired),
                    (PrLifecycle::Open, PrLifecycle::Closed)
                        | (PrLifecycle::Closed, PrLifecycle::Open)
                ) {
                    return Err(PlanError::InvalidEffect);
                }
            }
            Effect::Ownership(OwnershipEffect::UpdateLifecycle {
                expected, desired, ..
            }) if expected == desired || *expected == PrLifecycle::Merged => {
                return Err(PlanError::InvalidEffect);
            }
            Effect::Ownership(OwnershipEffect::InstallObserved {
                ownership,
                number,
                expected_absent,
                ..
            }) => {
                ownership.validate_for_pr(*number)?;
                if ownership.is_validated_legacy() || !expected_absent {
                    return Err(PlanError::InvalidEffect);
                }
            }
            Effect::Ownership(
                OwnershipEffect::Historicize {
                    expected_lifecycle, ..
                }
                | OwnershipEffect::Reactivate {
                    expected_lifecycle, ..
                },
            ) if *expected_lifecycle == PrLifecycle::Open => {
                return Err(PlanError::InvalidEffect);
            }
            Effect::Ownership(OwnershipEffect::InstallCreated {
                change_id,
                expected_absent,
                create_effect_index,
                ..
            }) => {
                let create_index = *create_effect_index as usize;
                let Some(Effect::Github(GithubEffect::CreatePullRequest {
                    change_id: created_change,
                    ..
                })) = effects.get(create_index)
                else {
                    return Err(PlanError::InvalidCreateReference);
                };
                if !expected_absent || create_index >= index || created_change != change_id {
                    return Err(PlanError::InvalidCreateReference);
                }
            }
            Effect::Reobserve(_) if index + 1 != effects.len() => {
                return Err(PlanError::BarrierNotTerminal);
            }
            _ => {}
        }
    }
    if aggregate_text_bytes > limits.state_bytes_max() {
        return Err(PlanError::InvalidEffect);
    }
    if contains_rebase
        && !matches!(
            effects.last(),
            Some(Effect::Reobserve(ReobserveBarrier::LocalHistory))
        )
    {
        return Err(PlanError::BarrierNotTerminal);
    }
    Ok(())
}

fn validate_generated_ownership(
    scope: &Scope,
    change_id: &ChangeId,
    head: &HeadRef,
    managed_section: &str,
    limits: Limits,
) -> Result<(), PlanError> {
    if head != &HeadRef::owned(change_id).map_err(|_| PlanError::InvalidEffect)? {
        return Err(PlanError::InvalidEffect);
    }
    let section =
        ManagedSection::parse(managed_section, limits).map_err(|_| PlanError::InvalidEffect)?;
    if section.source_repository() != scope.source_repository()
        || section.current_change() != change_id
    {
        return Err(PlanError::InvalidEffect);
    }
    Ok(())
}

/// Ownership joins are `O((C + P + S) log S)`. Rendering the complete stack
/// into every managed PR section is necessarily `O(C²)` encoded ID bytes; `C`
/// is capped at 64 and every body and canonical plan encoding has a byte bound.
pub fn derive_plan(
    chain: &SelectedChain,
    remote_heads: &BTreeMap<HeadRef, RemoteRefState>,
    github: PlanningGithub<'_>,
    state: &StateV3,
    mode: ExecutionMode,
    limits: Limits,
) -> Result<DerivedPlan, PlanError> {
    validate_planner_scope(chain, github, state)?;
    validate_managed_count(state, limits)?;
    let resolved = resolve_mode(github, mode)?;
    let desired_prs = derive_desired_prs(chain, state)?;
    let snapshot = resolved.snapshot();
    let observed = snapshot
        .map(|snapshot| index_observed_prs(snapshot, state, limits))
        .transpose()?;

    if let Some(observed) = &observed {
        validate_final_managed_count(chain, state, observed, limits)?;
        if let Some(stage) = merged_rebase_stage(chain, remote_heads, observed, limits)? {
            return finish_derived(chain.scope(), desired_prs, stage, resolved.output, limits);
        }
    }

    let push_stage = derive_push_stage(chain, remote_heads, snapshot, state)?;
    if !push_stage.is_empty() {
        let mut effects = push_stage;
        let barrier = if snapshot.is_some() {
            ReobserveBarrier::All
        } else {
            ReobserveBarrier::RemoteRefs
        };
        effects.push(Effect::Reobserve(barrier));
        return finish_derived(chain.scope(), desired_prs, effects, resolved.output, limits);
    }

    let Some(snapshot) = snapshot else {
        return finish_derived(
            chain.scope(),
            desired_prs,
            Vec::new(),
            resolved.output,
            limits,
        );
    };
    let observed = observed.expect("full capability always indexes its snapshot");
    let effects = derive_pr_effects(
        chain,
        snapshot,
        state,
        &observed,
        resolved.delete_closed_heads(),
        limits,
    )?;
    finish_derived(chain.scope(), desired_prs, effects, resolved.output, limits)
}

fn validate_planner_scope(
    chain: &SelectedChain,
    github: PlanningGithub<'_>,
    state: &StateV3,
) -> Result<(), PlanError> {
    if chain.scope() != state.scope() {
        return Err(PlanError::ScopeMismatch);
    }
    if let PlanningGithub::Observed(snapshot) = github {
        let scope = chain.scope();
        if snapshot.source_repository() != scope.source_repository()
            || snapshot.target_repository() != scope.target_repository()
            || snapshot.configured_base() != scope.base()
        {
            return Err(PlanError::ScopeMismatch);
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum PlanningCapability<'a> {
    Full {
        snapshot: &'a GithubSnapshot,
        delete_closed_heads: bool,
    },
    NoPr,
}

#[derive(Clone, Copy)]
enum PlanOutput {
    Executable,
    DryRun,
}

struct ResolvedMode<'a> {
    capability: PlanningCapability<'a>,
    output: PlanOutput,
}

impl ResolvedMode<'_> {
    fn snapshot(&self) -> Option<&GithubSnapshot> {
        match self.capability {
            PlanningCapability::Full { snapshot, .. } => Some(snapshot),
            PlanningCapability::NoPr => None,
        }
    }

    fn delete_closed_heads(&self) -> bool {
        match self.capability {
            PlanningCapability::Full {
                delete_closed_heads,
                ..
            } => delete_closed_heads,
            PlanningCapability::NoPr => false,
        }
    }
}

fn resolve_mode<'a>(
    github: PlanningGithub<'a>,
    mode: ExecutionMode,
) -> Result<ResolvedMode<'a>, PlanError> {
    let (full_policy, output) = match mode {
        ExecutionMode::Full {
            delete_closed_heads,
        } => (Some(delete_closed_heads), PlanOutput::Executable),
        ExecutionMode::NoPr => (None, PlanOutput::Executable),
        ExecutionMode::DryRun(DryRunMode::Full {
            delete_closed_heads,
        }) => (Some(delete_closed_heads), PlanOutput::DryRun),
        ExecutionMode::DryRun(DryRunMode::NoPr) => (None, PlanOutput::DryRun),
    };
    let capability = match (full_policy, github) {
        (Some(delete_closed_heads), PlanningGithub::Observed(snapshot)) => {
            PlanningCapability::Full {
                snapshot,
                delete_closed_heads,
            }
        }
        (Some(_), PlanningGithub::NotObserved) => {
            return Err(PlanError::GithubObservationRequired);
        }
        (None, PlanningGithub::NotObserved) => PlanningCapability::NoPr,
        (None, PlanningGithub::Observed(_)) => {
            return Err(PlanError::GithubObservationForbidden);
        }
    };
    Ok(ResolvedMode { capability, output })
}

fn validate_managed_count(state: &StateV3, limits: Limits) -> Result<(), PlanError> {
    let count = state
        .verified()
        .len()
        .checked_add(state.historic().len())
        .ok_or(PlanError::HistoryBound)?;
    if count > limits.change_count_max() {
        return Err(PlanError::HistoryBound);
    }
    Ok(())
}

fn derive_desired_prs(
    chain: &SelectedChain,
    state: &StateV3,
) -> Result<Box<[DesiredPr]>, PlanError> {
    let mut desired: Vec<DesiredPr> = Vec::with_capacity(chain.revisions().len());
    for (index, revision) in chain.revisions().iter().enumerate() {
        let head = state_managed(state, revision.change_id())
            .map(|(_, managed)| managed.head().clone())
            .unwrap_or(HeadRef::owned(revision.change_id()).map_err(|_| PlanError::InvalidEffect)?);
        let base = if index == 0 {
            chain.scope().base().clone()
        } else {
            desired[index - 1].head.clone()
        };
        desired.push(DesiredPr {
            change_id: revision.change_id().clone(),
            head,
            base,
        });
    }
    Ok(desired.into_boxed_slice())
}

fn derive_push_stage(
    chain: &SelectedChain,
    remote_heads: &BTreeMap<HeadRef, RemoteRefState>,
    snapshot: Option<&GithubSnapshot>,
    state: &StateV3,
) -> Result<Vec<Effect>, PlanError> {
    let mut effects = Vec::with_capacity(chain.revisions().len());
    for revision in chain.revisions() {
        let ownership = state_managed(state, revision.change_id())
            .map(|(_, managed)| managed.ownership().clone())
            .map_or_else(
                || {
                    PrOwnership::generated(revision.change_id().clone())
                        .map_err(|_| PlanError::InvalidEffect)
                },
                Ok,
            )?;
        let head = ownership.head();
        let remote = remote_heads
            .get(head)
            .ok_or_else(|| PlanError::RemoteObservationMissing { head: head.clone() })?;
        if let Some(snapshot) = snapshot {
            let github_state = snapshot
                .owned_heads()
                .get(head)
                .map_or(RemoteRefState::Absent, |commit| {
                    RemoteRefState::At(commit.clone())
                });
            if &github_state != remote {
                return Err(PlanError::ObservationDrift { head: head.clone() });
            }
        }
        let desired = RemoteRefState::At(revision.commit_id().clone());
        if remote != &desired {
            effects.push(Effect::Jj(JjEffect::PushHead {
                ownership,
                expected: remote.clone(),
                desired: revision.commit_id().clone(),
            }));
        }
    }
    Ok(effects)
}

struct ObservedManaged<'a> {
    pr: &'a ObservedPr,
    ownership: PrOwnership,
    state_location: StateLocation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StateLocation {
    Active,
    Historic,
    Missing,
}

struct StateOwnershipIndex<'a> {
    exact: BTreeMap<(PrNumber, HeadRef), (StateLocation, &'a ChangeId, &'a ManagedPr)>,
    numbers: BTreeMap<PrNumber, &'a ChangeId>,
    heads: BTreeMap<HeadRef, &'a ChangeId>,
}

impl<'a> StateOwnershipIndex<'a> {
    fn new(state: &'a StateV3) -> Result<Self, PlanError> {
        let mut index = Self {
            exact: BTreeMap::new(),
            numbers: BTreeMap::new(),
            heads: BTreeMap::new(),
        };
        for (location, change_id, managed) in state
            .verified()
            .iter()
            .map(|(change, managed)| (StateLocation::Active, change, managed))
            .chain(
                state
                    .historic()
                    .iter()
                    .map(|(change, managed)| (StateLocation::Historic, change, managed)),
            )
        {
            let duplicate = index
                .exact
                .insert(
                    (managed.number(), managed.head().clone()),
                    (location, change_id, managed),
                )
                .is_some()
                || index.numbers.insert(managed.number(), change_id).is_some()
                || index
                    .heads
                    .insert(managed.head().clone(), change_id)
                    .is_some();
            if duplicate {
                return Err(PlanError::OwnershipConflict {
                    change_id: change_id.clone(),
                });
            }
        }
        Ok(index)
    }

    fn exact(
        &self,
        number: PrNumber,
        head: &HeadRef,
    ) -> Option<(StateLocation, &'a ChangeId, &'a ManagedPr)> {
        self.exact.get(&(number, head.clone())).copied()
    }

    fn conflict(&self, number: PrNumber, head: &HeadRef) -> Option<&'a ChangeId> {
        self.numbers
            .get(&number)
            .copied()
            .or_else(|| self.heads.get(head).copied())
    }
}

fn index_observed_prs<'a>(
    snapshot: &'a GithubSnapshot,
    state: &'a StateV3,
    limits: Limits,
) -> Result<BTreeMap<ChangeId, ObservedManaged<'a>>, PlanError> {
    if snapshot.prs().len() > limits.change_count_max() {
        return Err(PlanError::HistoryBound);
    }
    let state_index = StateOwnershipIndex::new(state)?;
    let mut rows = BTreeMap::new();
    let mut numbers = BTreeSet::new();
    let mut heads = BTreeSet::new();
    for pr in snapshot.prs() {
        if !numbers.insert(pr.number()) || !heads.insert(pr.head_ref()) {
            return Err(PlanError::InvalidEffect);
        }
        let exact_state = state_index.exact(pr.number(), pr.head_ref());
        let (change_id, ownership, state_location) =
            if let Some((location, change, managed)) = exact_state {
                (change.clone(), managed.ownership().clone(), location)
            } else {
                if let Some(conflict) = state_index.conflict(pr.number(), pr.head_ref()) {
                    return Err(PlanError::OwnershipConflict {
                        change_id: conflict.clone(),
                    });
                }
                let change_id =
                    parse_generated_head(pr.head_ref()).ok_or_else(|| PlanError::UnownedHead {
                        head: pr.head_ref().clone(),
                    })?;
                let section = ManagedBody::section(pr.body(), limits)
                    .map_err(PlanError::Body)?
                    .ok_or_else(|| PlanError::OwnershipMissing {
                        change_id: change_id.clone(),
                    })?;
                let exact_ref = snapshot.owned_heads().get(pr.head_ref()) == Some(pr.head_commit());
                if section.source_repository() != snapshot.source_repository()
                    || section.current_change() != &change_id
                    || !exact_ref
                {
                    return Err(PlanError::OwnershipMissing { change_id });
                }
                let ownership = PrOwnership::generated(change_id.clone())
                    .map_err(|_| PlanError::InvalidEffect)?;
                (change_id, ownership, StateLocation::Missing)
            };
        if rows
            .insert(
                change_id.clone(),
                ObservedManaged {
                    pr,
                    ownership,
                    state_location,
                },
            )
            .is_some()
        {
            return Err(PlanError::OwnershipConflict { change_id });
        }
    }
    Ok(rows)
}

fn validate_final_managed_count(
    chain: &SelectedChain,
    state: &StateV3,
    observed: &BTreeMap<ChangeId, ObservedManaged<'_>>,
    limits: Limits,
) -> Result<(), PlanError> {
    let mut identities = state
        .verified()
        .keys()
        .chain(state.historic().keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    identities.extend(observed.keys().cloned());
    identities.extend(
        chain
            .revisions()
            .iter()
            .map(|revision| revision.change_id().clone()),
    );
    if identities.len() > limits.change_count_max() {
        return Err(PlanError::HistoryBound);
    }
    Ok(())
}

fn merged_rebase_stage(
    chain: &SelectedChain,
    remote_heads: &BTreeMap<HeadRef, RemoteRefState>,
    observed: &BTreeMap<ChangeId, ObservedManaged<'_>>,
    limits: Limits,
) -> Result<Option<Vec<Effect>>, PlanError> {
    for (index, revision) in chain.revisions().iter().enumerate() {
        let Some(managed) = observed.get(revision.change_id()) else {
            continue;
        };
        if managed.pr.lifecycle() != PrLifecycle::Merged {
            continue;
        }
        let Some(child) = chain.revisions().get(index + 1) else {
            return Err(PlanError::MergedTip {
                change_id: revision.change_id().clone(),
            });
        };
        let desired_parent = match remote_heads.get(managed.pr.base_ref()) {
            Some(RemoteRefState::At(commit)) => commit.clone(),
            _ => {
                return Err(PlanError::RebaseBaseMissing {
                    base: managed.pr.base_ref().clone(),
                });
            }
        };
        if &desired_parent == revision.commit_id() {
            return Err(PlanError::InvalidEffect);
        }
        let effects = vec![
            Effect::Jj(JjEffect::Rebase {
                change_id: child.change_id().clone(),
                expected_commit: child.commit_id().clone(),
                expected_parent: revision.commit_id().clone(),
                desired_parent,
            }),
            Effect::Reobserve(ReobserveBarrier::LocalHistory),
        ];
        Plan::new(
            chain.scope().clone(),
            effects.clone().into_boxed_slice(),
            limits,
        )?;
        return Ok(Some(effects));
    }
    Ok(None)
}

fn derive_pr_effects(
    chain: &SelectedChain,
    snapshot: &GithubSnapshot,
    state: &StateV3,
    observed: &BTreeMap<ChangeId, ObservedManaged<'_>>,
    delete_closed_heads: bool,
    limits: Limits,
) -> Result<Vec<Effect>, PlanError> {
    let active_stack = chain
        .revisions()
        .iter()
        .map(|revision| revision.change_id().clone())
        .collect::<Vec<_>>()
        .into_boxed_slice();
    let selected = active_stack.iter().cloned().collect::<BTreeSet<_>>();
    let historic_reactivated = state
        .historic()
        .keys()
        .filter(|change_id| selected.contains(*change_id))
        .count();
    let active_historicized = state
        .verified()
        .keys()
        .filter(|change_id| !selected.contains(*change_id))
        .count();
    let rebuilt_historicized = observed
        .keys()
        .filter(|change_id| {
            !selected.contains(*change_id) && state_managed(state, change_id).is_none()
        })
        .count();
    let historic_count_final = state
        .historic()
        .len()
        .saturating_sub(historic_reactivated)
        .saturating_add(active_historicized)
        .saturating_add(rebuilt_historicized);
    if historic_count_final > limits.change_count_max() {
        return Err(PlanError::HistoryBound);
    }
    let mut effects = Vec::new();

    for (index, revision) in chain.revisions().iter().enumerate() {
        let desired_head = state_managed(state, revision.change_id())
            .map(|(_, managed)| managed.head().clone())
            .unwrap_or(HeadRef::owned(revision.change_id()).map_err(|_| PlanError::InvalidEffect)?);
        let desired_base = if index == 0 {
            chain.scope().base().clone()
        } else {
            state_managed(state, chain.revisions()[index - 1].change_id())
                .map(|(_, managed)| managed.head().clone())
                .unwrap_or(
                    HeadRef::owned(chain.revisions()[index - 1].change_id())
                        .map_err(|_| PlanError::InvalidEffect)?,
                )
        };
        let section = ManagedSection::new(
            chain.scope().source_repository().clone(),
            revision.change_id().clone(),
            active_stack.clone(),
            limits,
        )
        .map_err(PlanError::Body)?;

        if let Some(managed) = observed.get(revision.change_id()) {
            if managed.ownership.head() != &desired_head {
                return Err(PlanError::OwnershipConflict {
                    change_id: revision.change_id().clone(),
                });
            }
            prepare_observed_ownership(&mut effects, revision.change_id(), managed, state)?;
            let mut persisted_lifecycle = state_managed(state, revision.change_id())
                .map(|(_, row)| row.lifecycle())
                .unwrap_or(managed.pr.lifecycle());
            if persisted_lifecycle == PrLifecycle::Merged
                && persisted_lifecycle != managed.pr.lifecycle()
            {
                return Err(PlanError::LifecycleDrift {
                    change_id: revision.change_id().clone(),
                    persisted: persisted_lifecycle,
                    observed: managed.pr.lifecycle(),
                });
            }
            if persisted_lifecycle != managed.pr.lifecycle() {
                effects.push(Effect::Ownership(OwnershipEffect::UpdateLifecycle {
                    change_id: revision.change_id().clone(),
                    expected_number: managed.pr.number(),
                    expected: persisted_lifecycle,
                    desired: managed.pr.lifecycle(),
                }));
                persisted_lifecycle = managed.pr.lifecycle();
            }
            if managed.pr.lifecycle() == PrLifecycle::Closed {
                effects.push(Effect::Github(GithubEffect::SetLifecycle {
                    ownership: managed.ownership.clone(),
                    number: managed.pr.number(),
                    expected: PrLifecycle::Closed,
                    desired: PrLifecycle::Open,
                }));
                effects.push(Effect::Ownership(OwnershipEffect::UpdateLifecycle {
                    change_id: revision.change_id().clone(),
                    expected_number: managed.pr.number(),
                    expected: persisted_lifecycle,
                    desired: PrLifecycle::Open,
                }));
            }
            if managed.pr.base_ref() != &desired_base {
                effects.push(Effect::Github(GithubEffect::UpdateBase {
                    ownership: managed.ownership.clone(),
                    number: managed.pr.number(),
                    expected: managed.pr.base_ref().clone(),
                    desired: desired_base,
                }));
            }
            if let BodyMerge::Changed(desired_body) =
                ManagedBody::merge(managed.pr.body(), &section, limits.body_bytes_max())
                    .map_err(PlanError::Body)?
            {
                effects.push(Effect::Github(GithubEffect::UpdateBody {
                    ownership: managed.ownership.clone(),
                    number: managed.pr.number(),
                    expected_hash: BodyHash::of(managed.pr.body().as_bytes()),
                    desired_hash: BodyHash::of(desired_body.as_bytes()),
                    managed_section: section.render(),
                }));
            }
        } else {
            if state_managed(state, revision.change_id()).is_some() {
                return Err(PlanError::OwnershipMissing {
                    change_id: revision.change_id().clone(),
                });
            }
            if snapshot.owned_heads().get(&desired_head) != Some(revision.commit_id()) {
                return Err(PlanError::ObservationDrift { head: desired_head });
            }
            let title = pr_title(revision.change_id(), revision.description())?;
            let create_index = u32::try_from(effects.len()).map_err(|_| PlanError::EffectCount)?;
            effects.push(Effect::Github(GithubEffect::CreatePullRequest {
                change_id: revision.change_id().clone(),
                head: desired_head,
                base: desired_base,
                title,
                managed_section: section.render(),
            }));
            effects.push(Effect::Ownership(OwnershipEffect::InstallCreated {
                change_id: revision.change_id().clone(),
                expected_absent: true,
                create_effect_index: create_index,
                lifecycle: PrLifecycle::Open,
            }));
        }
    }

    for (change_id, managed) in state.verified() {
        if selected.contains(change_id) {
            continue;
        }
        let observed = observed
            .get(change_id)
            .ok_or_else(|| PlanError::OwnershipMissing {
                change_id: change_id.clone(),
            })?;
        cleanup_active(
            &mut effects,
            change_id,
            managed,
            observed,
            snapshot,
            delete_closed_heads,
        )?;
    }

    for (change_id, managed) in state.historic() {
        if selected.contains(change_id) {
            continue;
        }
        let observed = observed
            .get(change_id)
            .ok_or_else(|| PlanError::OwnershipMissing {
                change_id: change_id.clone(),
            })?;
        if managed.lifecycle() == observed.pr.lifecycle() {
            continue;
        }
        if managed.lifecycle() == PrLifecycle::Merged {
            return Err(PlanError::LifecycleDrift {
                change_id: change_id.clone(),
                persisted: managed.lifecycle(),
                observed: observed.pr.lifecycle(),
            });
        }
        effects.push(Effect::Ownership(OwnershipEffect::Reactivate {
            change_id: change_id.clone(),
            expected_number: managed.number(),
            expected_lifecycle: managed.lifecycle(),
        }));
        cleanup_active(
            &mut effects,
            change_id,
            managed,
            observed,
            snapshot,
            delete_closed_heads,
        )?;
    }

    for (change_id, managed) in observed {
        if selected.contains(change_id) || state_managed(state, change_id).is_some() {
            continue;
        }
        effects.push(Effect::Ownership(OwnershipEffect::InstallObserved {
            ownership: managed.ownership.clone(),
            number: managed.pr.number(),
            expected_absent: true,
            lifecycle: managed.pr.lifecycle(),
        }));
        let synthetic = ManagedPrView {
            number: managed.pr.number(),
            ownership: &managed.ownership,
            lifecycle: managed.pr.lifecycle(),
        };
        cleanup_active_view(
            &mut effects,
            change_id,
            synthetic,
            managed,
            snapshot,
            delete_closed_heads,
        )?;
    }

    if delete_closed_heads {
        let mut scheduled = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Jj(JjEffect::DeleteHead { ownership, .. }) => {
                    Some(ownership.head().clone())
                }
                _ => None,
            })
            .collect::<BTreeSet<_>>();
        let selected_heads = chain
            .revisions()
            .iter()
            .map(|revision| {
                state_managed(state, revision.change_id())
                    .map(|(_, managed)| managed.head().clone())
                    .unwrap_or(
                        HeadRef::owned(revision.change_id()).expect("validated ID makes a head"),
                    )
            })
            .collect::<BTreeSet<_>>();
        let state_heads = state
            .verified()
            .values()
            .chain(state.historic().values())
            .map(|managed| managed.head().clone())
            .collect::<BTreeSet<_>>();
        let historic_heads = state
            .historic()
            .values()
            .map(|managed| (managed.head().clone(), managed))
            .collect::<BTreeMap<_, _>>();
        for (head, commit) in snapshot.owned_heads() {
            if selected_heads.contains(head) || scheduled.contains(head) {
                continue;
            }
            if state_heads.contains(head) {
                if let Some(managed) = historic_heads.get(head) {
                    effects.push(Effect::Jj(JjEffect::DeleteHead {
                        ownership: managed.ownership().clone(),
                        expected: commit.clone(),
                    }));
                    scheduled.insert(head.clone());
                }
                continue;
            }
            // Generated-looking names are not authority. Untracked heads are
            // preserved until exact persisted state or managed PR metadata proves ownership.
        }
    }

    if effects
        .iter()
        .any(|effect| matches!(effect, Effect::Jj(JjEffect::DeleteHead { .. })))
    {
        effects.push(Effect::Reobserve(ReobserveBarrier::All));
    }
    Ok(effects)
}

fn prepare_observed_ownership(
    effects: &mut Vec<Effect>,
    change_id: &ChangeId,
    observed: &ObservedManaged<'_>,
    state: &StateV3,
) -> Result<(), PlanError> {
    match observed.state_location {
        StateLocation::Active => {}
        StateLocation::Historic => {
            if observed.pr.lifecycle() == PrLifecycle::Merged {
                return Err(PlanError::MergedTip {
                    change_id: change_id.clone(),
                });
            }
            effects.push(Effect::Ownership(OwnershipEffect::Reactivate {
                change_id: change_id.clone(),
                expected_number: observed.pr.number(),
                expected_lifecycle: state
                    .historic()
                    .get(change_id)
                    .expect("indexed historic row exists")
                    .lifecycle(),
            }));
        }
        StateLocation::Missing => {
            effects.push(Effect::Ownership(OwnershipEffect::InstallObserved {
                ownership: observed.ownership.clone(),
                number: observed.pr.number(),
                expected_absent: true,
                lifecycle: observed.pr.lifecycle(),
            }));
        }
    }
    Ok(())
}

fn cleanup_active(
    effects: &mut Vec<Effect>,
    change_id: &ChangeId,
    managed: &ManagedPr,
    observed: &ObservedManaged<'_>,
    snapshot: &GithubSnapshot,
    delete_closed_heads: bool,
) -> Result<(), PlanError> {
    cleanup_active_view(
        effects,
        change_id,
        ManagedPrView {
            number: managed.number(),
            ownership: managed.ownership(),
            lifecycle: managed.lifecycle(),
        },
        observed,
        snapshot,
        delete_closed_heads,
    )
}

struct ManagedPrView<'a> {
    number: PrNumber,
    ownership: &'a PrOwnership,
    lifecycle: PrLifecycle,
}

fn cleanup_active_view(
    effects: &mut Vec<Effect>,
    change_id: &ChangeId,
    managed: ManagedPrView<'_>,
    observed: &ObservedManaged<'_>,
    snapshot: &GithubSnapshot,
    delete_closed_heads: bool,
) -> Result<(), PlanError> {
    if managed.number != observed.pr.number() || managed.ownership != &observed.ownership {
        return Err(PlanError::OwnershipConflict {
            change_id: change_id.clone(),
        });
    }
    let mut lifecycle = managed.lifecycle;
    if lifecycle != observed.pr.lifecycle() {
        effects.push(Effect::Ownership(OwnershipEffect::UpdateLifecycle {
            change_id: change_id.clone(),
            expected_number: managed.number,
            expected: lifecycle,
            desired: observed.pr.lifecycle(),
        }));
        lifecycle = observed.pr.lifecycle();
    }
    if lifecycle == PrLifecycle::Open {
        effects.push(Effect::Github(GithubEffect::SetLifecycle {
            ownership: managed.ownership.clone(),
            number: managed.number,
            expected: PrLifecycle::Open,
            desired: PrLifecycle::Closed,
        }));
        effects.push(Effect::Ownership(OwnershipEffect::UpdateLifecycle {
            change_id: change_id.clone(),
            expected_number: managed.number,
            expected: PrLifecycle::Open,
            desired: PrLifecycle::Closed,
        }));
        lifecycle = PrLifecycle::Closed;
    }
    effects.push(Effect::Ownership(OwnershipEffect::Historicize {
        change_id: change_id.clone(),
        expected_number: managed.number,
        expected_lifecycle: lifecycle,
    }));
    if delete_closed_heads {
        if let Some(commit) = snapshot.owned_heads().get(managed.ownership.head()) {
            effects.push(Effect::Jj(JjEffect::DeleteHead {
                ownership: managed.ownership.clone(),
                expected: commit.clone(),
            }));
        }
    }
    Ok(())
}

fn finish_derived(
    scope: &Scope,
    desired_prs: Box<[DesiredPr]>,
    effects: Vec<Effect>,
    output: PlanOutput,
    limits: Limits,
) -> Result<DerivedPlan, PlanError> {
    let plan = Plan::new(scope.clone(), effects.into_boxed_slice(), limits)?;
    let outcome = match output {
        PlanOutput::DryRun => PlanningOutcome::DryRun(DryRunPlan {
            id: plan.id(),
            scope: plan.scope().clone(),
            actions: render_actions(plan.effects(), limits.state_bytes_max())?,
        }),
        PlanOutput::Executable => PlanningOutcome::Executable(plan),
    };
    Ok(DerivedPlan {
        desired_prs,
        outcome,
    })
}

fn state_managed<'a>(
    state: &'a StateV3,
    change_id: &ChangeId,
) -> Option<(StateLocation, &'a ManagedPr)> {
    state
        .verified()
        .get(change_id)
        .map(|managed| (StateLocation::Active, managed))
        .or_else(|| {
            state
                .historic()
                .get(change_id)
                .map(|managed| (StateLocation::Historic, managed))
        })
}

fn parse_generated_head(head: &HeadRef) -> Option<ChangeId> {
    let change_id = ChangeId::parse(head.as_str().strip_prefix("almighty-push/")?).ok()?;
    (HeadRef::owned(&change_id).ok()? == *head).then_some(change_id)
}

fn pr_title(change_id: &ChangeId, description: &str) -> Result<String, PlanError> {
    let title = description
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| PlanError::TitleInvalid {
            change_id: change_id.clone(),
        })?;
    if title.len() > TITLE_BYTES_MAX || title.contains('\0') {
        return Err(PlanError::TitleInvalid {
            change_id: change_id.clone(),
        });
    }
    Ok(title.to_owned())
}

fn compute_id(scope: &Scope, effects: &[Effect], limits: Limits) -> Result<PlanId, PlanError> {
    #[derive(Serialize)]
    struct Identity<'a> {
        scope: &'a Scope,
        effects: &'a [Effect],
    }

    let mut writer = HashWriter::new(limits.state_bytes_max());
    serde_json::to_writer(&mut writer, &Identity { scope, effects })
        .map_err(|_| PlanError::Serialization)?;
    writer.finish()
}

struct HashWriter {
    hasher: Sha256,
    bytes: usize,
    max: usize,
}

impl HashWriter {
    fn new(max: usize) -> Self {
        Self {
            hasher: Sha256::new(),
            bytes: 0,
            max,
        }
    }

    fn finish(self) -> Result<PlanId, PlanError> {
        if self.bytes > self.max {
            return Err(PlanError::EncodedBytes {
                bytes: self.bytes,
                max: self.max,
            });
        }
        Ok(PlanId(self.hasher.finalize().into()))
    }
}

impl Write for HashWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("canonical plan byte count overflow"))?;
        self.hasher.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn render_actions(effects: &[Effect], max: usize) -> Result<Box<[String]>, PlanError> {
    let mut actions = Vec::with_capacity(effects.len());
    let mut total_bytes = 0usize;
    for effect in effects {
        let mut writer = ActionWriter::new(total_bytes, max);
        serde_json::to_writer(&mut writer, effect).map_err(|_| PlanError::Serialization)?;
        total_bytes = writer.total_bytes;
        if let Some(bytes) = writer.finish() {
            actions.push(String::from_utf8(bytes).map_err(|_| PlanError::Serialization)?);
        }
    }
    if total_bytes > max {
        return Err(PlanError::EncodedBytes {
            bytes: total_bytes,
            max,
        });
    }
    assert_eq!(actions.len(), effects.len());
    Ok(actions.into_boxed_slice())
}

struct ActionWriter {
    bytes: Vec<u8>,
    total_bytes: usize,
    max: usize,
    exceeded: bool,
}

impl ActionWriter {
    fn new(total_bytes: usize, max: usize) -> Self {
        Self {
            bytes: Vec::new(),
            total_bytes,
            max,
            exceeded: total_bytes > max,
        }
    }

    fn finish(self) -> Option<Vec<u8>> {
        (!self.exceeded).then_some(self.bytes)
    }
}

impl Write for ActionWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.total_bytes = self
            .total_bytes
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("dry-run byte count overflow"))?;
        if self.total_bytes <= self.max {
            self.bytes.extend_from_slice(buffer);
        } else {
            self.exceeded = true;
            self.bytes.clear();
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod planner_tests {
    use super::*;
    use crate::body::ManagedBody;
    use crate::domain::{LimitValues, RemoteName, RepositoryId, Revision};
    use crate::github::{GithubSnapshot, ObservedPr, ObservedPrFixture};

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

    fn change(index: usize) -> ChangeId {
        let mut bytes = [b'k'; 32];
        bytes[30] += (index / 16) as u8;
        bytes[31] += (index % 16) as u8;
        ChangeId::parse(std::str::from_utf8(&bytes).unwrap()).unwrap()
    }

    fn commit(index: usize) -> CommitId {
        CommitId::parse(format!("{index:040x}")).unwrap()
    }

    fn chain(ids: &[usize]) -> SelectedChain {
        let rows = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                Revision::new(
                    change(*id),
                    commit(*id + 1),
                    format!("change {id}"),
                    if index == 0 {
                        Box::new([])
                    } else {
                        vec![change(ids[index - 1])].into_boxed_slice()
                    },
                    false,
                )
                .unwrap()
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        SelectedChain::new(scope(), rows, limits()).unwrap()
    }

    fn section_body(current: usize, stack: &[usize], user: &str) -> String {
        let section = ManagedSection::new(
            scope().source_repository().clone(),
            change(current),
            stack
                .iter()
                .map(|id| change(*id))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            limits(),
        )
        .unwrap();
        let BodyMerge::Changed(body) =
            ManagedBody::merge(user, &section, limits().body_bytes_max()).unwrap()
        else {
            panic!("fixture section must be new")
        };
        body
    }

    fn pr(id: usize, number: u64, lifecycle: PrLifecycle, base: &str, body: String) -> ObservedPr {
        ObservedPr::fixture(ObservedPrFixture {
            number: PrNumber::new(number).unwrap(),
            lifecycle,
            head_repository: scope().source_repository().clone(),
            head_ref: HeadRef::owned(&change(id)).unwrap(),
            base_ref: HeadRef::parse(base).unwrap(),
            title: format!("change {id}"),
            body,
            head_commit: commit(id + 1),
        })
    }

    fn snapshot(prs: Vec<ObservedPr>, owned: &[usize]) -> GithubSnapshot {
        GithubSnapshot::fixture(
            scope().source_repository().clone(),
            scope().target_repository().clone(),
            HeadRef::parse("main").unwrap(),
            scope().base().clone(),
            owned
                .iter()
                .map(|id| (HeadRef::owned(&change(*id)).unwrap(), commit(*id + 1)))
                .collect(),
            prs.into_boxed_slice(),
        )
    }

    fn remote(ids: &[usize]) -> BTreeMap<HeadRef, RemoteRefState> {
        ids.iter()
            .map(|id| {
                (
                    HeadRef::owned(&change(*id)).unwrap(),
                    RemoteRefState::At(commit(*id + 1)),
                )
            })
            .collect()
    }

    fn executable(derived: &DerivedPlan) -> &Plan {
        let PlanningOutcome::Executable(plan) = derived.outcome() else {
            panic!("fixture requested executable planning")
        };
        plan
    }

    #[test]
    fn execution_modes_require_exactly_their_allowed_observations() {
        let chain = chain(&[]);
        let state = StateV3::empty(scope());
        let snapshot = snapshot(Vec::new(), &[]);
        assert!(matches!(
            derive_plan(
                &chain,
                &BTreeMap::new(),
                PlanningGithub::NotObserved,
                &state,
                ExecutionMode::Full {
                    delete_closed_heads: false,
                },
                limits(),
            ),
            Err(PlanError::GithubObservationRequired)
        ));
        assert!(matches!(
            derive_plan(
                &chain,
                &BTreeMap::new(),
                PlanningGithub::Observed(&snapshot),
                &state,
                ExecutionMode::NoPr,
                limits(),
            ),
            Err(PlanError::GithubObservationForbidden)
        ));
    }

    #[test]
    fn full_create_uses_exact_adjacency_and_binds_each_created_number() {
        let chain = chain(&[0, 1, 2]);
        let state = StateV3::empty(scope());
        let snapshot = snapshot(Vec::new(), &[0, 1, 2]);
        let derived = derive_plan(
            &chain,
            &remote(&[0, 1, 2]),
            PlanningGithub::Observed(&snapshot),
            &state,
            ExecutionMode::Full {
                delete_closed_heads: false,
            },
            limits(),
        )
        .unwrap();
        assert_eq!(derived.desired_prs().len(), 3);
        assert_eq!(derived.desired_prs()[0].base(), scope().base());
        assert_eq!(
            derived.desired_prs()[1].base(),
            &HeadRef::owned(&change(0)).unwrap()
        );
        assert_eq!(executable(&derived).effects().len(), 6);
        for pair in executable(&derived).effects().chunks_exact(2) {
            assert!(matches!(
                pair[0],
                Effect::Github(GithubEffect::CreatePullRequest { .. })
            ));
            assert!(matches!(
                pair[1],
                Effect::Ownership(OwnershipEffect::InstallCreated { .. })
            ));
        }
    }

    #[test]
    fn exact_generated_metadata_rebuilds_state_without_duplicate_create() {
        let chain = chain(&[0]);
        let state = StateV3::empty(scope());
        let observed = pr(
            0,
            7,
            PrLifecycle::Open,
            "main",
            section_body(0, &[0], "user"),
        );
        let snapshot = snapshot(vec![observed], &[0]);
        let derived = derive_plan(
            &chain,
            &remote(&[0]),
            PlanningGithub::Observed(&snapshot),
            &state,
            ExecutionMode::Full {
                delete_closed_heads: false,
            },
            limits(),
        )
        .unwrap();
        assert!(matches!(
            executable(&derived).effects(),
            [Effect::Ownership(OwnershipEffect::InstallObserved { .. })]
        ));
    }

    #[test]
    fn reorder_updates_every_changed_base_and_preserves_user_body_bytes() {
        let chain = chain(&[1, 0, 2]);
        let mut state = StateV3::empty(scope());
        for (id, number) in [(0, 10), (1, 11), (2, 12)] {
            state.insert_generated_fixture(
                change(id),
                PrNumber::new(number).unwrap(),
                PrLifecycle::Open,
                false,
            );
        }
        let snapshot = snapshot(
            vec![
                pr(
                    0,
                    10,
                    PrLifecycle::Open,
                    "main",
                    section_body(0, &[0, 1, 2], "alpha λ"),
                ),
                pr(
                    1,
                    11,
                    PrLifecycle::Open,
                    HeadRef::owned(&change(0)).unwrap().as_str(),
                    section_body(1, &[0, 1, 2], "beta 🙂"),
                ),
                pr(
                    2,
                    12,
                    PrLifecycle::Open,
                    HeadRef::owned(&change(1)).unwrap().as_str(),
                    section_body(2, &[0, 1, 2], "gamma"),
                ),
            ],
            &[0, 1, 2],
        );
        let derived = derive_plan(
            &chain,
            &remote(&[0, 1, 2]),
            PlanningGithub::Observed(&snapshot),
            &state,
            ExecutionMode::Full {
                delete_closed_heads: false,
            },
            limits(),
        )
        .unwrap();
        let effects = executable(&derived).effects();
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Github(GithubEffect::UpdateBase { .. })))
                .count(),
            3
        );
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Github(GithubEffect::UpdateBody { .. })))
                .count(),
            3
        );
    }

    #[test]
    fn closed_selected_pr_reactivates_reopens_and_repairs_metadata() {
        let chain = chain(&[0]);
        let mut state = StateV3::empty(scope());
        state.insert_generated_fixture(
            change(0),
            PrNumber::new(7).unwrap(),
            PrLifecycle::Closed,
            true,
        );
        let snapshot = snapshot(
            vec![pr(
                0,
                7,
                PrLifecycle::Closed,
                "other",
                section_body(0, &[0], "user"),
            )],
            &[0],
        );
        let derived = derive_plan(
            &chain,
            &remote(&[0]),
            PlanningGithub::Observed(&snapshot),
            &state,
            ExecutionMode::Full {
                delete_closed_heads: false,
            },
            limits(),
        )
        .unwrap();
        let effects = executable(&derived).effects();
        assert!(matches!(
            effects[0],
            Effect::Ownership(OwnershipEffect::Reactivate { .. })
        ));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Github(GithubEffect::SetLifecycle {
                expected: PrLifecycle::Closed,
                desired: PrLifecycle::Open,
                ..
            })
        )));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::Github(GithubEffect::UpdateBase { .. }))));
    }

    #[test]
    fn merged_predecessor_produces_only_checked_rebase_and_terminal_local_barrier() {
        let chain = chain(&[0, 1]);
        let mut state = StateV3::empty(scope());
        state.insert_generated_fixture(
            change(0),
            PrNumber::new(7).unwrap(),
            PrLifecycle::Open,
            false,
        );
        state.insert_generated_fixture(
            change(1),
            PrNumber::new(8).unwrap(),
            PrLifecycle::Open,
            false,
        );
        let merged = pr(
            0,
            7,
            PrLifecycle::Merged,
            "main",
            section_body(0, &[0, 1], ""),
        );
        let child = pr(
            1,
            8,
            PrLifecycle::Open,
            HeadRef::owned(&change(0)).unwrap().as_str(),
            section_body(1, &[0, 1], ""),
        );
        let snapshot = snapshot(vec![merged, child], &[0, 1]);
        let mut observed_remote = remote(&[0, 1]);
        observed_remote.insert(scope().base().clone(), RemoteRefState::At(commit(99)));
        let derived = derive_plan(
            &chain,
            &observed_remote,
            PlanningGithub::Observed(&snapshot),
            &state,
            ExecutionMode::Full {
                delete_closed_heads: false,
            },
            limits(),
        )
        .unwrap();
        assert!(matches!(
            executable(&derived).effects(),
            [
                Effect::Jj(JjEffect::Rebase { .. }),
                Effect::Reobserve(ReobserveBarrier::LocalHistory)
            ]
        ));
    }

    #[test]
    fn split_squash_and_disappearing_ids_follow_set_membership_only() {
        let chain = chain(&[1, 2]);
        let mut state = StateV3::empty(scope());
        for (id, number) in [(0, 7), (1, 8)] {
            state.insert_generated_fixture(
                change(id),
                PrNumber::new(number).unwrap(),
                PrLifecycle::Open,
                false,
            );
        }
        let snapshot = snapshot(
            vec![
                pr(
                    0,
                    7,
                    PrLifecycle::Open,
                    "main",
                    section_body(0, &[0, 1], "old"),
                ),
                pr(
                    1,
                    8,
                    PrLifecycle::Open,
                    HeadRef::owned(&change(0)).unwrap().as_str(),
                    section_body(1, &[0, 1], "survivor"),
                ),
            ],
            &[0, 1, 2],
        );
        let derived = derive_plan(
            &chain,
            &remote(&[0, 1, 2]),
            PlanningGithub::Observed(&snapshot),
            &state,
            ExecutionMode::Full {
                delete_closed_heads: true,
            },
            limits(),
        )
        .unwrap();
        let effects = executable(&derived).effects();
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Github(GithubEffect::CreatePullRequest { change_id, .. }) if change_id == &change(2)
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Github(GithubEffect::SetLifecycle { number, desired: PrLifecycle::Closed, .. })
                if *number == PrNumber::new(7).unwrap()
        )));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Ownership(OwnershipEffect::Historicize { change_id, .. }) if change_id == &change(0)
        )));
        assert!(matches!(
            effects.last(),
            Some(Effect::Reobserve(ReobserveBarrier::All))
        ));
    }

    #[test]
    fn empty_stack_closes_historicizes_and_optionally_deletes_exact_heads() {
        let chain = chain(&[]);
        let mut state = StateV3::empty(scope());
        state.insert_generated_fixture(
            change(0),
            PrNumber::new(7).unwrap(),
            PrLifecycle::Open,
            false,
        );
        let snapshot = snapshot(
            vec![pr(
                0,
                7,
                PrLifecycle::Open,
                "main",
                section_body(0, &[0], "user"),
            )],
            &[0],
        );
        let derived = derive_plan(
            &chain,
            &BTreeMap::new(),
            PlanningGithub::Observed(&snapshot),
            &state,
            ExecutionMode::Full {
                delete_closed_heads: true,
            },
            limits(),
        )
        .unwrap();
        let effects = executable(&derived).effects();
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::Github(GithubEffect::SetLifecycle { .. }))));
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Ownership(OwnershipEffect::Historicize { .. })
        )));
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::Jj(JjEffect::DeleteHead { .. }))));
    }

    #[test]
    fn generated_looking_head_without_state_or_managed_pr_is_preserved() {
        let chain = chain(&[]);
        let state = StateV3::empty(scope());
        let snapshot = snapshot(Vec::new(), &[0]);
        let derived = derive_plan(
            &chain,
            &BTreeMap::new(),
            PlanningGithub::Observed(&snapshot),
            &state,
            ExecutionMode::Full {
                delete_closed_heads: true,
            },
            limits(),
        )
        .unwrap();
        assert!(!executable(&derived)
            .effects()
            .iter()
            .any(|effect| matches!(effect, Effect::Jj(JjEffect::DeleteHead { .. }))));
    }

    #[test]
    fn exact_open_stack_is_a_no_op() {
        let chain = chain(&[0]);
        let mut state = StateV3::empty(scope());
        state.insert_generated_fixture(
            change(0),
            PrNumber::new(7).unwrap(),
            PrLifecycle::Open,
            false,
        );
        let snapshot = snapshot(
            vec![pr(
                0,
                7,
                PrLifecycle::Open,
                "main",
                section_body(0, &[0], "user"),
            )],
            &[0],
        );
        let derived = derive_plan(
            &chain,
            &remote(&[0]),
            PlanningGithub::Observed(&snapshot),
            &state,
            ExecutionMode::Full {
                delete_closed_heads: false,
            },
            limits(),
        )
        .unwrap();
        assert!(executable(&derived).effects().is_empty());
    }

    #[test]
    fn merged_tip_requires_explicit_operator_action() {
        let chain = chain(&[0]);
        let mut state = StateV3::empty(scope());
        state.insert_generated_fixture(
            change(0),
            PrNumber::new(7).unwrap(),
            PrLifecycle::Open,
            false,
        );
        let snapshot = snapshot(
            vec![pr(
                0,
                7,
                PrLifecycle::Merged,
                "main",
                section_body(0, &[0], ""),
            )],
            &[0],
        );
        assert!(matches!(
            derive_plan(
                &chain,
                &remote(&[0]),
                PlanningGithub::Observed(&snapshot),
                &state,
                ExecutionMode::Full {
                    delete_closed_heads: false,
                },
                limits(),
            ),
            Err(PlanError::MergedTip { change_id }) if change_id == change(0)
        ));
    }

    fn limits_with_change_count(change_count_max: u64) -> Limits {
        Limits::new(LimitValues {
            change_count_max,
            ..LimitValues::default()
        })
        .unwrap()
    }

    #[test]
    fn selected_historic_pr_observed_open_reactivates_without_redundant_github_reopen() {
        let chain = chain(&[0]);
        let mut state = StateV3::empty(scope());
        state.insert_generated_fixture(
            change(0),
            PrNumber::new(7).unwrap(),
            PrLifecycle::Closed,
            true,
        );
        let snapshot = snapshot(
            vec![pr(
                0,
                7,
                PrLifecycle::Open,
                "main",
                section_body(0, &[0], "user"),
            )],
            &[0],
        );
        let derived = derive_plan(
            &chain,
            &remote(&[0]),
            PlanningGithub::Observed(&snapshot),
            &state,
            ExecutionMode::Full {
                delete_closed_heads: false,
            },
            limits(),
        )
        .unwrap();
        assert!(matches!(
            executable(&derived).effects(),
            [
                Effect::Ownership(OwnershipEffect::Reactivate { .. }),
                Effect::Ownership(OwnershipEffect::UpdateLifecycle {
                    expected: PrLifecycle::Closed,
                    desired: PrLifecycle::Open,
                    ..
                })
            ]
        ));
    }

    #[test]
    fn nonselected_historic_pr_observed_open_is_closed_and_historicized_again() {
        let chain = chain(&[]);
        let mut state = StateV3::empty(scope());
        state.insert_generated_fixture(
            change(0),
            PrNumber::new(7).unwrap(),
            PrLifecycle::Closed,
            true,
        );
        let snapshot = snapshot(
            vec![pr(
                0,
                7,
                PrLifecycle::Open,
                "main",
                section_body(0, &[0], "user"),
            )],
            &[0],
        );
        let derived = derive_plan(
            &chain,
            &BTreeMap::new(),
            PlanningGithub::Observed(&snapshot),
            &state,
            ExecutionMode::Full {
                delete_closed_heads: false,
            },
            limits(),
        )
        .unwrap();
        assert!(matches!(
            executable(&derived).effects(),
            [
                Effect::Ownership(OwnershipEffect::Reactivate { .. }),
                Effect::Ownership(OwnershipEffect::UpdateLifecycle {
                    expected: PrLifecycle::Closed,
                    desired: PrLifecycle::Open,
                    ..
                }),
                Effect::Github(GithubEffect::SetLifecycle {
                    expected: PrLifecycle::Open,
                    desired: PrLifecycle::Closed,
                    ..
                }),
                Effect::Ownership(OwnershipEffect::UpdateLifecycle {
                    expected: PrLifecycle::Open,
                    desired: PrLifecycle::Closed,
                    ..
                }),
                Effect::Ownership(OwnershipEffect::Historicize { .. })
            ]
        ));
    }

    #[test]
    fn final_managed_identity_set_accepts_exact_maximum_and_rejects_one_more() {
        for historic_count in [3, 4] {
            let configured = limits_with_change_count(4);
            let chain = chain(&[4]);
            let mut state = StateV3::empty(scope());
            let mut prs = Vec::new();
            for id in 0..historic_count {
                state.insert_generated_fixture(
                    change(id),
                    PrNumber::new((id + 1) as u64).unwrap(),
                    PrLifecycle::Closed,
                    true,
                );
                prs.push(pr(
                    id,
                    (id + 1) as u64,
                    PrLifecycle::Closed,
                    "main",
                    section_body(id, &[id], "historic"),
                ));
            }
            let snapshot = snapshot(prs, &[4]);
            let result = derive_plan(
                &chain,
                &remote(&[4]),
                PlanningGithub::Observed(&snapshot),
                &state,
                ExecutionMode::Full {
                    delete_closed_heads: false,
                },
                configured,
            );
            if historic_count == 3 {
                assert!(result.is_ok());
            } else {
                assert!(matches!(result, Err(PlanError::HistoryBound)));
            }
        }
    }

    fn escaping_create_effects(count: usize) -> Vec<Effect> {
        (0..count)
            .map(|index| {
                let current = change(index);
                Effect::Github(GithubEffect::CreatePullRequest {
                    change_id: current.clone(),
                    head: HeadRef::owned(&current).unwrap(),
                    base: HeadRef::parse("main").unwrap(),
                    title: "\n\t\"".repeat(100),
                    managed_section: ManagedSection::new(
                        scope().source_repository().clone(),
                        current.clone(),
                        vec![current].into_boxed_slice(),
                        limits(),
                    )
                    .unwrap()
                    .render(),
                })
            })
            .collect()
    }

    #[test]
    fn canonical_hash_and_preview_count_json_escaping_at_exact_bound_and_one_more() {
        #[derive(Serialize)]
        struct Identity<'a> {
            scope: &'a Scope,
            effects: &'a [Effect],
        }

        let effects = escaping_create_effects(200);
        let identity_bytes = serde_json::to_vec(&Identity {
            scope: &scope(),
            effects: &effects,
        })
        .unwrap()
        .len();
        assert!(identity_bytes >= 64 * 1024);
        let exact_limits = Limits::new(LimitValues {
            state_bytes_max: identity_bytes as u64,
            ..LimitValues::default()
        })
        .unwrap();
        let one_less_limits = Limits::new(LimitValues {
            state_bytes_max: (identity_bytes - 1) as u64,
            ..LimitValues::default()
        })
        .unwrap();
        assert!(compute_id(&scope(), &effects, exact_limits).is_ok());
        assert!(matches!(
            compute_id(&scope(), &effects, one_less_limits),
            Err(PlanError::EncodedBytes {
                bytes,
                max
            }) if bytes == identity_bytes && max + 1 == identity_bytes
        ));

        let action_bytes = effects
            .iter()
            .map(|effect| serde_json::to_vec(effect).unwrap().len())
            .sum::<usize>();
        assert_eq!(
            render_actions(&effects, action_bytes).unwrap().len(),
            effects.len()
        );
        assert!(matches!(
            render_actions(&effects, action_bytes - 1),
            Err(PlanError::EncodedBytes { bytes, max })
                if bytes == action_bytes && max + 1 == action_bytes
        ));
    }

    #[test]
    fn forged_duplicate_pr_number_is_a_deterministic_conflict() {
        let chain = chain(&[0, 1]);
        let state = StateV3::empty(scope());
        let snapshot = snapshot(
            vec![
                pr(
                    0,
                    7,
                    PrLifecycle::Open,
                    "main",
                    section_body(0, &[0, 1], ""),
                ),
                pr(
                    1,
                    7,
                    PrLifecycle::Open,
                    "main",
                    section_body(1, &[0, 1], ""),
                ),
            ],
            &[0, 1],
        );
        assert!(matches!(
            derive_plan(
                &chain,
                &remote(&[0, 1]),
                PlanningGithub::Observed(&snapshot),
                &state,
                ExecutionMode::Full {
                    delete_closed_heads: false,
                },
                limits(),
            ),
            Err(PlanError::InvalidEffect)
        ));
    }
}

#[cfg(test)]
#[path = "plan_oracle_tests.rs"]
mod planner_oracle_tests;
