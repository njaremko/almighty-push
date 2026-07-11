use crate::config::ResolvedConfig;
use crate::domain::{ChangeId, HeadRef, Limits, PrLifecycle, PrNumber, Scope};
use crate::lock::{LockError, RepositoryLock};
use crate::plan::{
    result_matches, Effect, EffectResult, Plan, PlanError, PlanId, PrOwnership, ReobserveBarrier,
};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::CString;
use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

const SCHEMA_VERSION: u32 = 3;
const LEGACY_TEXT_BYTES_MAX: usize = 512;
const STATE_FILE: &str = "state-v3.json";
const LEGACY_FILE: &str = ".almighty";
const TEMP_FILE: &str = "state-v3.tmp";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedPr {
    number: PrNumber,
    ownership: PrOwnership,
    lifecycle: PrLifecycle,
}

impl ManagedPr {
    fn generated(
        change_id: &ChangeId,
        number: PrNumber,
        lifecycle: PrLifecycle,
    ) -> Result<Self, StateError> {
        Ok(Self {
            number,
            ownership: PrOwnership::generated(change_id.clone()).map_err(StateError::Domain)?,
            lifecycle,
        })
    }

    fn validated_legacy(
        change_id: ChangeId,
        number: PrNumber,
        head: HeadRef,
        lifecycle: PrLifecycle,
        observation_digest: [u8; 32],
    ) -> Self {
        Self {
            number,
            ownership: PrOwnership::validated_legacy(change_id, head, number, observation_digest),
            lifecycle,
        }
    }

    pub fn number(&self) -> PrNumber {
        self.number
    }

    pub fn head(&self) -> &HeadRef {
        self.ownership.head()
    }

    pub fn ownership(&self) -> &PrOwnership {
        &self.ownership
    }

    pub fn lifecycle(&self) -> PrLifecycle {
        self.lifecycle
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyCandidate {
    legacy_key: String,
    pr_number: u64,
    pr_url: String,
    branch_name: String,
    commit_id: String,
    change_id: Option<String>,
}

pub enum LegacyCandidateResolution {
    Verified(LegacyValidation),
    Rejected(LegacyRejection),
}

pub struct LegacyRejection {
    pub(crate) candidate: LegacyCandidate,
    pub(crate) scope: Scope,
    pub(crate) reason: LegacyRejectionReason,
    pub(crate) observation_digest: [u8; 32],
}

pub struct LegacyValidation {
    pub(crate) candidate: LegacyCandidate,
    pub(crate) scope: Scope,
    pub(crate) change_id: ChangeId,
    pub(crate) number: PrNumber,
    pub(crate) head: HeadRef,
    pub(crate) lifecycle: PrLifecycle,
    pub(crate) observation_digest: [u8; 32],
}

impl LegacyCandidate {
    fn new(
        legacy_key: String,
        pr_number: u64,
        pr_url: String,
        branch_name: String,
        commit_id: String,
        change_id: Option<String>,
    ) -> Result<Self, StateError> {
        validate_legacy_fields(
            &legacy_key,
            &pr_url,
            &branch_name,
            &commit_id,
            change_id.as_deref(),
        )?;
        Ok(Self {
            legacy_key,
            pr_number,
            pr_url,
            branch_name,
            commit_id,
            change_id,
        })
    }

    pub fn legacy_key(&self) -> &str {
        &self.legacy_key
    }

    pub fn pr_number(&self) -> u64 {
        self.pr_number
    }

    pub fn pr_url(&self) -> &str {
        &self.pr_url
    }

    pub fn branch_name(&self) -> &str {
        &self.branch_name
    }

    pub fn commit_id(&self) -> &str {
        &self.commit_id
    }

    pub fn change_id(&self) -> Option<&str> {
        self.change_id.as_deref()
    }
}

fn validate_legacy_fields(
    legacy_key: &str,
    pr_url: &str,
    branch_name: &str,
    commit_id: &str,
    change_id: Option<&str>,
) -> Result<(), StateError> {
    for value in [legacy_key, pr_url, branch_name, commit_id]
        .into_iter()
        .chain(change_id)
    {
        if value.len() > LEGACY_TEXT_BYTES_MAX || value.bytes().any(|byte| byte == 0) {
            return Err(StateError::Malformed {
                message: "legacy candidate field is invalid".to_owned(),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyDigest([u8; 32]);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum LegacyDisposition {
    Verified {
        change_id: ChangeId,
        number: PrNumber,
        head: HeadRef,
        observation_digest: [u8; 32],
    },
    Rejected {
        reason: LegacyRejectionReason,
        observation_digest: [u8; 32],
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum LegacyRejectionReason {
    ChangeIdentityMismatch,
    RepositoryMismatch,
    HeadMismatch,
    CommitMismatch,
    OwnershipMarkerMissing,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LegacyResolutionRecord {
    candidate: LegacyCandidate,
    disposition: LegacyDisposition,
}

impl LegacyResolutionRecord {
    pub fn candidate(&self) -> &LegacyCandidate {
        &self.candidate
    }

    pub fn disposition(&self) -> &LegacyDisposition {
        &self.disposition
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum LegacyMigration {
    None,
    Pending {
        source_digest: LegacyDigest,
        candidates: Box<[LegacyCandidate]>,
        resolved: Box<[LegacyResolutionRecord]>,
    },
    ReadyToDelete {
        source_digest: LegacyDigest,
        resolved: Box<[LegacyResolutionRecord]>,
    },
    Complete {
        source_digest: LegacyDigest,
        resolved: Box<[LegacyResolutionRecord]>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
struct ExecutionOccurrence(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum CompletionOutcome {
    Complete,
    Reobserve(ReobserveBarrier),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompletionIdentity {
    occurrence: ExecutionOccurrence,
    plan_id: PlanId,
    proof_digest: [u8; 32],
    outcome: CompletionOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompletionReceiptData {
    identity: CompletionIdentity,
    generation: StateGeneration,
    store_identity: StorageIdentity,
}

struct CompletionProofWriter(Sha256);

impl CompletionProofWriter {
    fn new() -> Self {
        Self(Sha256::new())
    }

    fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

impl Write for CompletionProofWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        Digest::update(&mut self.0, buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl CompletionReceiptData {
    pub(crate) fn plan_id(&self) -> PlanId {
        self.identity.plan_id
    }

    pub(crate) fn outcome(&self) -> CompletionOutcome {
        self.identity.outcome
    }

    #[cfg(test)]
    pub(crate) fn corrupt_occurrence(mut self) -> Self {
        self.identity.occurrence.0 = self.identity.occurrence.0.wrapping_add(1);
        self
    }

    #[cfg(test)]
    pub(crate) fn corrupt_plan(mut self, plan_id: PlanId) -> Self {
        self.identity.plan_id = plan_id;
        self
    }

    #[cfg(test)]
    pub(crate) fn corrupt_proof(mut self) -> Self {
        self.identity.proof_digest[0] ^= 1;
        self
    }

    #[cfg(test)]
    pub(crate) fn corrupt_outcome(mut self) -> Self {
        self.identity.outcome = match self.identity.outcome {
            CompletionOutcome::Complete => CompletionOutcome::Reobserve(ReobserveBarrier::All),
            CompletionOutcome::Reobserve(_) => CompletionOutcome::Complete,
        };
        self
    }

    #[cfg(test)]
    pub(crate) fn corrupt_generation(mut self) -> Self {
        self.generation = match self.generation {
            StateGeneration::Missing => StateGeneration::Present([0; 32]),
            StateGeneration::Present(mut digest) => {
                digest[0] ^= 1;
                StateGeneration::Present(digest)
            }
        };
        self
    }

    #[cfg(test)]
    pub(crate) fn corrupt_store(mut self) -> Self {
        self.store_identity.inode = self.store_identity.inode.wrapping_add(1);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
enum ExecutionOrigin {
    Initial,
    Acknowledged(CompletionReceiptData),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionCheckpoint {
    occurrence: ExecutionOccurrence,
    origin: ExecutionOrigin,
    plan: Plan,
    results: Box<[Option<EffectResult>]>,
    next_effect_index: u32,
}

impl ExecutionCheckpoint {
    fn start(
        occurrence: ExecutionOccurrence,
        origin: ExecutionOrigin,
        plan: Plan,
        limits: Limits,
    ) -> Result<Self, StateError> {
        plan.validate(limits).map_err(StateError::Plan)?;
        let result_count = plan.effects().len();
        Ok(Self {
            occurrence,
            origin,
            plan,
            results: vec![None; result_count].into_boxed_slice(),
            next_effect_index: 0,
        })
    }

    pub fn occurrence(&self) -> u64 {
        self.occurrence.0
    }

    pub(crate) fn was_started_by(&self, receipt: &CompletionReceiptData) -> bool {
        matches!(&self.origin, ExecutionOrigin::Acknowledged(origin) if origin == receipt)
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    pub fn results(&self) -> &[Option<EffectResult>] {
        &self.results
    }

    pub fn next_effect_index(&self) -> u32 {
        self.next_effect_index
    }

    fn record_result(&mut self, result: EffectResult) -> Result<(), StateError> {
        let index = self.next_effect_index as usize;
        let effect = self
            .plan
            .effects()
            .get(index)
            .ok_or(StateError::InvalidCheckpoint)?;
        if !result_matches(effect, &result) || self.results[index].is_some() {
            return Err(StateError::InvalidCheckpoint);
        }
        self.results[index] = Some(result);
        self.next_effect_index = self
            .next_effect_index
            .checked_add(1)
            .ok_or(StateError::InvalidCheckpoint)?;
        Ok(())
    }

    fn validate(&self, limits: Limits) -> Result<(), StateError> {
        self.plan.validate(limits).map_err(StateError::Plan)?;
        if self.occurrence.0 == 0 {
            return Err(StateError::InvalidCheckpoint);
        }
        if let ExecutionOrigin::Acknowledged(receipt) = &self.origin {
            if receipt.identity.occurrence.0 == 0
                || receipt.identity.occurrence.0 >= self.occurrence.0
            {
                return Err(StateError::InvalidCheckpoint);
            }
        }
        let next = self.next_effect_index as usize;
        if self.results.len() != self.plan.effects().len() || next > self.results.len() {
            return Err(StateError::InvalidCheckpoint);
        }
        for (index, result) in self.results.iter().enumerate() {
            let should_be_satisfied = index < next;
            if result.is_some() != should_be_satisfied {
                return Err(StateError::InvalidCheckpoint);
            }
            if let Some(result) = result {
                if !result_matches(&self.plan.effects()[index], result) {
                    return Err(StateError::InvalidCheckpoint);
                }
            }
        }
        Ok(())
    }

    fn completion_identity(&self) -> Result<CompletionIdentity, StateError> {
        if self.next_effect_index as usize != self.plan.effects().len() {
            return Err(StateError::InvalidCheckpoint);
        }
        let outcome = match (self.plan.effects().last(), self.results.last()) {
            (
                Some(Effect::Reobserve(reason)),
                Some(Some(EffectResult::BarrierReached { reason: proof })),
            ) if reason == proof => CompletionOutcome::Reobserve(*reason),
            (Some(Effect::Reobserve(_)), _) => return Err(StateError::InvalidCheckpoint),
            _ => CompletionOutcome::Complete,
        };
        let mut proof = CompletionProofWriter::new();
        serde_json::to_writer(&mut proof, &(self.plan.id(), &self.results, outcome))
            .map_err(|_| StateError::InvalidCheckpoint)?;
        Ok(CompletionIdentity {
            occurrence: self.occurrence,
            plan_id: self.plan.id(),
            proof_digest: proof.finish(),
            outcome,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum CheckpointState {
    Idle,
    Executing(Box<ExecutionCheckpoint>),
    Completed(Box<ExecutionCheckpoint>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct StateV3 {
    schema_version: u32,
    scope: Scope,
    execution_occurrence: u64,
    verified: BTreeMap<ChangeId, ManagedPr>,
    historic: BTreeMap<ChangeId, ManagedPr>,
    legacy_migration: LegacyMigration,
    checkpoint: CheckpointState,
}

impl StateV3 {
    pub fn empty(scope: Scope) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            scope,
            execution_occurrence: 0,
            verified: BTreeMap::new(),
            historic: BTreeMap::new(),
            legacy_migration: LegacyMigration::None,
            checkpoint: CheckpointState::Idle,
        }
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn verified(&self) -> &BTreeMap<ChangeId, ManagedPr> {
        &self.verified
    }

    pub fn historic(&self) -> &BTreeMap<ChangeId, ManagedPr> {
        &self.historic
    }

    pub fn legacy_candidates(&self) -> &[LegacyCandidate] {
        match &self.legacy_migration {
            LegacyMigration::Pending { candidates, .. } => candidates,
            _ => &[],
        }
    }

    pub fn legacy_resolutions(&self) -> &[LegacyResolutionRecord] {
        match &self.legacy_migration {
            LegacyMigration::Pending { resolved, .. }
            | LegacyMigration::ReadyToDelete { resolved, .. }
            | LegacyMigration::Complete { resolved, .. } => resolved,
            LegacyMigration::None => &[],
        }
    }

    pub fn legacy_migration(&self) -> &LegacyMigration {
        &self.legacy_migration
    }

    pub fn checkpoint(&self) -> &CheckpointState {
        &self.checkpoint
    }

    /// Applies exact migration evidence to an in-memory preview. This grants no
    /// persistence or deletion capability and is used only by side-effect-free
    /// planning.
    pub fn resolve_legacy_preview(
        &mut self,
        resolution: LegacyCandidateResolution,
    ) -> Result<(), StateError> {
        self.resolve_legacy(resolution)
    }

    /// Completes an in-memory preview after every candidate was resolved. The
    /// durable path must instead publish and delete through a locked session.
    pub fn finish_legacy_preview(&mut self) -> Result<(), StateError> {
        let migration = std::mem::replace(&mut self.legacy_migration, LegacyMigration::None);
        let LegacyMigration::ReadyToDelete {
            source_digest,
            resolved,
        } = migration
        else {
            self.legacy_migration = migration;
            return Err(StateError::InvalidMigrationState);
        };
        self.legacy_migration = LegacyMigration::Complete {
            source_digest,
            resolved,
        };
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn insert_generated_fixture(
        &mut self,
        change_id: ChangeId,
        number: PrNumber,
        lifecycle: PrLifecycle,
        historic: bool,
    ) {
        let managed = ManagedPr::generated(&change_id, number, lifecycle)
            .expect("test fixture uses valid generated ownership");
        self.insert_fixture(change_id, managed, historic);
    }

    #[cfg(test)]
    pub(crate) fn insert_validated_legacy_fixture(
        &mut self,
        change_id: ChangeId,
        number: PrNumber,
        head: HeadRef,
        lifecycle: PrLifecycle,
        historic: bool,
    ) {
        let managed =
            ManagedPr::validated_legacy(change_id.clone(), number, head, lifecycle, [7; 32]);
        self.insert_fixture(change_id, managed, historic);
    }

    #[cfg(test)]
    fn insert_fixture(&mut self, change_id: ChangeId, managed: ManagedPr, historic: bool) {
        if historic {
            assert!(self.historic.insert(change_id, managed).is_none());
        } else {
            assert!(self.verified.insert(change_id, managed).is_none());
        }
    }

    pub(crate) fn validated_legacy_ownership(&self) -> impl Iterator<Item = &PrOwnership> {
        self.verified
            .values()
            .chain(self.historic.values())
            .map(ManagedPr::ownership)
            .filter(|ownership| ownership.is_validated_legacy())
    }

    fn start_plan(&mut self, plan: Plan, limits: Limits) -> Result<(), StateError> {
        if !matches!(self.checkpoint, CheckpointState::Idle) {
            return Err(StateError::InvalidCheckpoint);
        }
        self.install_plan(ExecutionOrigin::Initial, plan, limits)
    }

    fn start_next_plan(
        &mut self,
        completed_receipt: CompletionReceiptData,
        plan: Plan,
        limits: Limits,
    ) -> Result<(), StateError> {
        let CheckpointState::Completed(completed) = &self.checkpoint else {
            return Err(StateError::InvalidCheckpoint);
        };
        if completed.completion_identity()? != completed_receipt.identity {
            return Err(StateError::InvalidCheckpoint);
        }
        self.install_plan(
            ExecutionOrigin::Acknowledged(completed_receipt),
            plan,
            limits,
        )
    }

    fn install_plan(
        &mut self,
        origin: ExecutionOrigin,
        plan: Plan,
        limits: Limits,
    ) -> Result<(), StateError> {
        if !matches!(
            self.legacy_migration,
            LegacyMigration::None | LegacyMigration::Complete { .. }
        ) || plan.scope() != &self.scope
        {
            return Err(StateError::InvalidCheckpoint);
        }
        self.validate_plan_ownership(&plan)?;
        let occurrence = self
            .execution_occurrence
            .checked_add(1)
            .ok_or(StateError::OccurrenceExhausted)?;
        let checkpoint =
            ExecutionCheckpoint::start(ExecutionOccurrence(occurrence), origin, plan, limits)?;
        self.execution_occurrence = occurrence;
        self.checkpoint = CheckpointState::Executing(Box::new(checkpoint));
        Ok(())
    }

    fn completed_receipt(
        &self,
        generation: StateGeneration,
        store_identity: StorageIdentity,
    ) -> Result<CompletionReceiptData, StateError> {
        let CheckpointState::Completed(completed) = &self.checkpoint else {
            return Err(StateError::InvalidCheckpoint);
        };
        Ok(CompletionReceiptData {
            identity: completed.completion_identity()?,
            generation,
            store_identity,
        })
    }

    fn record_current_result(&mut self, proof: SatisfiedEffect) -> Result<(), StateError> {
        let (effect, prior_results) = match &self.checkpoint {
            CheckpointState::Executing(checkpoint) => {
                let index = checkpoint.next_effect_index() as usize;
                let effect = checkpoint
                    .plan()
                    .effects()
                    .get(index)
                    .cloned()
                    .ok_or(StateError::InvalidCheckpoint)?;
                (effect, checkpoint.results().to_vec())
            }
            CheckpointState::Idle | CheckpointState::Completed(_) => {
                return Err(StateError::InvalidCheckpoint)
            }
        };
        match (&effect, proof.source) {
            (crate::plan::Effect::Ownership(_), SatisfactionSource::OwnershipPrecondition) => {
                if self.ownership_effect_state(&effect, &prior_results)?
                    != OwnershipEffectState::Ready
                {
                    return Err(StateError::InvalidCheckpoint);
                }
                self.apply_ownership_effect(&effect, &prior_results)?;
                if self.ownership_effect_state(&effect, &prior_results)?
                    != OwnershipEffectState::Satisfied
                {
                    return Err(StateError::InvalidCheckpoint);
                }
            }
            (crate::plan::Effect::Ownership(_), SatisfactionSource::OwnershipPostcondition) => {
                if self.ownership_effect_state(&effect, &prior_results)?
                    != OwnershipEffectState::Satisfied
                {
                    return Err(StateError::InvalidCheckpoint);
                }
            }
            (crate::plan::Effect::Reobserve(_), SatisfactionSource::Barrier) => {}
            (
                crate::plan::Effect::Jj(_) | crate::plan::Effect::Github(_),
                SatisfactionSource::ExternalPostcondition,
            ) => {}
            _ => return Err(StateError::InvalidCheckpoint),
        }
        let CheckpointState::Executing(checkpoint) = &mut self.checkpoint else {
            return Err(StateError::InvalidCheckpoint);
        };
        checkpoint.record_result(proof.result)
    }

    fn publish_completion(&mut self) -> Result<(), StateError> {
        let executing = std::mem::replace(&mut self.checkpoint, CheckpointState::Idle);
        let CheckpointState::Executing(checkpoint) = executing else {
            self.checkpoint = executing;
            return Err(StateError::InvalidCheckpoint);
        };
        if checkpoint.next_effect_index() as usize != checkpoint.plan().effects().len() {
            self.checkpoint = CheckpointState::Executing(checkpoint);
            return Err(StateError::InvalidCheckpoint);
        }
        self.checkpoint = CheckpointState::Completed(checkpoint);
        Ok(())
    }

    fn insert_generated(
        &mut self,
        change_id: ChangeId,
        number: PrNumber,
        lifecycle: PrLifecycle,
    ) -> Result<bool, StateError> {
        let managed_pr = ManagedPr::generated(&change_id, number, lifecycle)?;
        self.insert_managed(change_id, managed_pr)
    }

    fn insert_managed(
        &mut self,
        change_id: ChangeId,
        managed_pr: ManagedPr,
    ) -> Result<bool, StateError> {
        if let Some(existing) = self.verified.get(&change_id) {
            return if existing == &managed_pr {
                Ok(false)
            } else {
                Err(StateError::OwnershipConflict)
            };
        }
        if self.historic.contains_key(&change_id)
            || self
                .verified
                .values()
                .chain(self.historic.values())
                .any(|existing| {
                    existing.number == managed_pr.number || existing.head() == managed_pr.head()
                })
        {
            return Err(StateError::OwnershipConflict);
        }
        self.verified.insert(change_id, managed_pr);
        Ok(true)
    }

    pub(crate) fn ownership_effect_state(
        &self,
        effect: &crate::plan::Effect,
        results: &[Option<EffectResult>],
    ) -> Result<OwnershipEffectState, StateError> {
        use crate::plan::{Effect, OwnershipEffect};

        let exact_row = |managed: &ManagedPr,
                         number: PrNumber,
                         ownership: Option<&PrOwnership>,
                         lifecycle: PrLifecycle| {
            managed.number == number
                && ownership.is_none_or(|expected| managed.ownership() == expected)
                && managed.lifecycle == lifecycle
        };
        let state = match effect {
            Effect::Ownership(OwnershipEffect::InstallCreated {
                change_id,
                expected_absent,
                create_effect_index,
                lifecycle,
            }) => {
                if !expected_absent {
                    OwnershipEffectState::Drift
                } else {
                    let Some(Some(EffectResult::CreatedPullRequest { number })) =
                        results.get(*create_effect_index as usize)
                    else {
                        return Err(StateError::InvalidCheckpoint);
                    };
                    let ownership =
                        PrOwnership::generated(change_id.clone()).map_err(StateError::Domain)?;
                    match (self.verified.get(change_id), self.historic.get(change_id)) {
                        (Some(managed), None)
                            if exact_row(managed, *number, Some(&ownership), *lifecycle) =>
                        {
                            OwnershipEffectState::Satisfied
                        }
                        (None, None) => OwnershipEffectState::Ready,
                        _ => OwnershipEffectState::Drift,
                    }
                }
            }
            Effect::Ownership(OwnershipEffect::InstallObserved {
                ownership,
                number,
                expected_absent,
                lifecycle,
            }) => {
                if !expected_absent {
                    OwnershipEffectState::Drift
                } else {
                    let change_id = ownership.change_id();
                    match (self.verified.get(change_id), self.historic.get(change_id)) {
                        (Some(managed), None)
                            if exact_row(managed, *number, Some(ownership), *lifecycle) =>
                        {
                            OwnershipEffectState::Satisfied
                        }
                        (None, None) => OwnershipEffectState::Ready,
                        _ => OwnershipEffectState::Drift,
                    }
                }
            }
            Effect::Ownership(OwnershipEffect::UpdateLifecycle {
                change_id,
                expected_number,
                expected,
                desired,
            }) => match self.verified.get(change_id) {
                Some(managed) if exact_row(managed, *expected_number, None, *desired) => {
                    OwnershipEffectState::Satisfied
                }
                Some(managed) if exact_row(managed, *expected_number, None, *expected) => {
                    OwnershipEffectState::Ready
                }
                _ => OwnershipEffectState::Drift,
            },
            Effect::Ownership(OwnershipEffect::Historicize {
                change_id,
                expected_number,
                expected_lifecycle,
            }) => match (self.verified.get(change_id), self.historic.get(change_id)) {
                (None, Some(managed))
                    if exact_row(managed, *expected_number, None, *expected_lifecycle) =>
                {
                    OwnershipEffectState::Satisfied
                }
                (Some(managed), None)
                    if exact_row(managed, *expected_number, None, *expected_lifecycle) =>
                {
                    OwnershipEffectState::Ready
                }
                _ => OwnershipEffectState::Drift,
            },
            Effect::Ownership(OwnershipEffect::Reactivate {
                change_id,
                expected_number,
                expected_lifecycle,
            }) => match (self.verified.get(change_id), self.historic.get(change_id)) {
                (Some(managed), None)
                    if exact_row(managed, *expected_number, None, *expected_lifecycle) =>
                {
                    OwnershipEffectState::Satisfied
                }
                (None, Some(managed))
                    if exact_row(managed, *expected_number, None, *expected_lifecycle) =>
                {
                    OwnershipEffectState::Ready
                }
                _ => OwnershipEffectState::Drift,
            },
            _ => return Err(StateError::InvalidCheckpoint),
        };
        Ok(state)
    }

    fn apply_ownership_effect(
        &mut self,
        effect: &crate::plan::Effect,
        results: &[Option<EffectResult>],
    ) -> Result<(), StateError> {
        use crate::plan::{Effect, OwnershipEffect};
        match effect {
            Effect::Ownership(OwnershipEffect::InstallCreated {
                change_id,
                expected_absent,
                create_effect_index,
                lifecycle,
            }) => {
                if !expected_absent || self.verified.contains_key(change_id) {
                    return Err(StateError::OwnershipConflict);
                }
                let Some(Some(EffectResult::CreatedPullRequest { number })) =
                    results.get(*create_effect_index as usize)
                else {
                    return Err(StateError::InvalidCheckpoint);
                };
                self.insert_generated(change_id.clone(), *number, *lifecycle)?;
            }
            Effect::Ownership(OwnershipEffect::InstallObserved {
                ownership,
                number,
                expected_absent,
                lifecycle,
            }) => {
                if !expected_absent
                    || self.verified.contains_key(ownership.change_id())
                    || self.historic.contains_key(ownership.change_id())
                {
                    return Err(StateError::OwnershipConflict);
                }
                self.insert_managed(
                    ownership.change_id().clone(),
                    ManagedPr {
                        number: *number,
                        ownership: ownership.clone(),
                        lifecycle: *lifecycle,
                    },
                )?;
            }
            Effect::Ownership(OwnershipEffect::UpdateLifecycle {
                change_id,
                expected_number,
                expected,
                desired,
            }) => {
                let managed = self
                    .verified
                    .get_mut(change_id)
                    .ok_or(StateError::OwnershipConflict)?;
                if managed.number != *expected_number || managed.lifecycle != *expected {
                    return Err(StateError::OwnershipConflict);
                }
                managed.lifecycle = *desired;
            }
            Effect::Ownership(OwnershipEffect::Historicize {
                change_id,
                expected_number,
                expected_lifecycle,
            }) => {
                if self.historic.contains_key(change_id) {
                    return Err(StateError::OwnershipConflict);
                }
                let managed = self
                    .verified
                    .remove(change_id)
                    .ok_or(StateError::OwnershipConflict)?;
                if managed.number != *expected_number || managed.lifecycle != *expected_lifecycle {
                    self.verified.insert(change_id.clone(), managed);
                    return Err(StateError::OwnershipConflict);
                }
                self.historic.insert(change_id.clone(), managed);
            }
            Effect::Ownership(OwnershipEffect::Reactivate {
                change_id,
                expected_number,
                expected_lifecycle,
            }) => {
                if self.verified.contains_key(change_id) {
                    return Err(StateError::OwnershipConflict);
                }
                let managed = self
                    .historic
                    .remove(change_id)
                    .ok_or(StateError::OwnershipConflict)?;
                if managed.number != *expected_number || managed.lifecycle != *expected_lifecycle {
                    self.historic.insert(change_id.clone(), managed);
                    return Err(StateError::OwnershipConflict);
                }
                self.verified.insert(change_id.clone(), managed);
            }
            _ => {}
        }
        Ok(())
    }

    fn resolve_legacy(&mut self, resolution: LegacyCandidateResolution) -> Result<(), StateError> {
        match resolution {
            LegacyCandidateResolution::Verified(validation) => self.promote_legacy(validation),
            LegacyCandidateResolution::Rejected(rejection) => self.reject_legacy(rejection),
        }
    }

    fn reject_legacy(&mut self, rejection: LegacyRejection) -> Result<(), StateError> {
        if rejection.scope != self.scope {
            return Err(StateError::LegacyValidationMismatch);
        }
        let LegacyMigration::Pending {
            source_digest,
            candidates,
            resolved,
        } = &mut self.legacy_migration
        else {
            return Err(StateError::InvalidMigrationState);
        };
        let Some(index) = candidates
            .iter()
            .position(|candidate| candidate == &rejection.candidate)
        else {
            return Err(StateError::LegacyValidationMismatch);
        };
        let mut remaining = Vec::from(std::mem::take(candidates));
        let candidate = remaining.remove(index);
        let mut completed = Vec::from(std::mem::take(resolved));
        completed.push(LegacyResolutionRecord {
            candidate,
            disposition: LegacyDisposition::Rejected {
                reason: rejection.reason,
                observation_digest: rejection.observation_digest,
            },
        });
        if remaining.is_empty() {
            self.legacy_migration = LegacyMigration::ReadyToDelete {
                source_digest: *source_digest,
                resolved: completed.into_boxed_slice(),
            };
        } else {
            *candidates = remaining.into_boxed_slice();
            *resolved = completed.into_boxed_slice();
        }
        Ok(())
    }

    fn promote_legacy(&mut self, validation: LegacyValidation) -> Result<(), StateError> {
        if validation.scope != self.scope {
            return Err(StateError::LegacyValidationMismatch);
        }
        let _observation_digest = validation.observation_digest;
        let LegacyMigration::Pending {
            source_digest,
            candidates,
            resolved,
        } = &mut self.legacy_migration
        else {
            return Err(StateError::InvalidMigrationState);
        };
        let Some(index) = candidates
            .iter()
            .position(|candidate| candidate == &validation.candidate)
        else {
            return Err(StateError::LegacyValidationMismatch);
        };
        let managed = ManagedPr::validated_legacy(
            validation.change_id.clone(),
            validation.number,
            validation.head.clone(),
            validation.lifecycle,
            validation.observation_digest,
        );
        if self.verified.contains_key(&validation.change_id)
            || self.historic.contains_key(&validation.change_id)
            || self
                .verified
                .values()
                .chain(self.historic.values())
                .any(|existing| {
                    existing.number == managed.number || existing.head() == managed.head()
                })
        {
            return Err(StateError::OwnershipConflict);
        }
        self.verified.insert(validation.change_id.clone(), managed);
        let mut remaining = Vec::from(std::mem::take(candidates));
        let candidate = remaining.remove(index);
        let mut completed = Vec::from(std::mem::take(resolved));
        completed.push(LegacyResolutionRecord {
            candidate,
            disposition: LegacyDisposition::Verified {
                change_id: validation.change_id,
                number: validation.number,
                head: validation.head,
                observation_digest: validation.observation_digest,
            },
        });
        if remaining.is_empty() {
            self.legacy_migration = LegacyMigration::ReadyToDelete {
                source_digest: *source_digest,
                resolved: completed.into_boxed_slice(),
            };
        } else {
            *candidates = remaining.into_boxed_slice();
            *resolved = completed.into_boxed_slice();
        }
        Ok(())
    }

    fn set_legacy_candidates(
        &mut self,
        source_digest: LegacyDigest,
        candidates: Vec<LegacyCandidate>,
    ) {
        self.legacy_migration = if candidates.is_empty() {
            LegacyMigration::ReadyToDelete {
                source_digest,
                resolved: Box::new([]),
            }
        } else {
            LegacyMigration::Pending {
                source_digest,
                candidates: candidates.into_boxed_slice(),
                resolved: Box::new([]),
            }
        };
    }

    fn validate(&self, expected_scope: &Scope, limits: Limits) -> Result<(), StateError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(StateError::UnsupportedSchema {
                version: self.schema_version,
            });
        }
        if &self.scope != expected_scope {
            return Err(StateError::ScopeMismatch {
                expected: Box::new(expected_scope.clone()),
                observed: Box::new(self.scope.clone()),
            });
        }
        match &self.checkpoint {
            CheckpointState::Idle => {}
            CheckpointState::Executing(checkpoint) | CheckpointState::Completed(checkpoint) => {
                checkpoint.validate(limits)?;
                if checkpoint.occurrence.0 != self.execution_occurrence {
                    return Err(StateError::InvalidCheckpoint);
                }
                self.validate_plan_ownership(checkpoint.plan())?;
                if checkpoint.plan().scope() != &self.scope
                    || !matches!(
                        self.legacy_migration,
                        LegacyMigration::None | LegacyMigration::Complete { .. }
                    )
                {
                    return Err(StateError::InvalidCheckpoint);
                }
                if matches!(&self.checkpoint, CheckpointState::Completed(_)) {
                    checkpoint.completion_identity()?;
                }
            }
        }
        let candidates = self.legacy_candidates();
        let resolved = self.legacy_resolutions();
        let legacy_count = candidates.len().saturating_add(resolved.len());
        let managed_count = self.verified.len().checked_add(self.historic.len()).ok_or(
            StateError::OwnershipCount {
                count: usize::MAX,
                max: limits.change_count_max(),
            },
        )?;
        if managed_count > limits.change_count_max() || legacy_count > limits.change_count_max() {
            return Err(StateError::OwnershipCount {
                count: managed_count.max(legacy_count),
                max: limits.change_count_max(),
            });
        }
        if self
            .verified
            .keys()
            .any(|change_id| self.historic.contains_key(change_id))
            || self
                .historic
                .values()
                .any(|managed| managed.lifecycle == PrLifecycle::Open)
        {
            return Err(StateError::OwnershipConflict);
        }
        let mut legacy_keys = std::collections::BTreeSet::new();
        let mut legacy_numbers = std::collections::BTreeSet::new();
        let mut legacy_heads = std::collections::BTreeSet::new();
        for candidate in candidates
            .iter()
            .chain(resolved.iter().map(LegacyResolutionRecord::candidate))
        {
            validate_legacy_fields(
                &candidate.legacy_key,
                &candidate.pr_url,
                &candidate.branch_name,
                &candidate.commit_id,
                candidate.change_id.as_deref(),
            )?;
            if !legacy_keys.insert(&candidate.legacy_key)
                || !legacy_numbers.insert(candidate.pr_number)
                || !legacy_heads.insert(&candidate.branch_name)
            {
                return Err(StateError::Malformed {
                    message: "duplicate legacy candidate identity".to_owned(),
                });
            }
        }
        for record in resolved {
            if let LegacyDisposition::Verified {
                change_id,
                number,
                head,
                ..
            } = record.disposition()
            {
                let Some(managed) = self
                    .verified
                    .get(change_id)
                    .or_else(|| self.historic.get(change_id))
                else {
                    return Err(StateError::LegacyValidationMismatch);
                };
                if managed.number != *number
                    || managed.head() != head
                    || record.candidate.legacy_key != change_id.as_str()
                {
                    return Err(StateError::LegacyValidationMismatch);
                }
            }
        }
        let mut pr_numbers = std::collections::BTreeSet::new();
        let mut heads = std::collections::BTreeSet::new();
        for (change_id, managed) in self.verified.iter().chain(&self.historic) {
            if managed.ownership.change_id() != change_id {
                return Err(StateError::OwnershipHeadMismatch {
                    change_id: change_id.clone(),
                });
            }
            if let Some((number, observation_digest)) = managed.ownership.legacy_evidence() {
                let exact_resolution = resolved.iter().any(|record| {
                    matches!(
                        record.disposition(),
                        LegacyDisposition::Verified {
                            change_id: resolved_change,
                            number: resolved_number,
                            head,
                            observation_digest: resolved_digest,
                            ..
                        } if resolved_change == change_id
                            && *resolved_number == number
                            && head == managed.head()
                            && *resolved_digest == observation_digest
                    )
                });
                if !exact_resolution {
                    return Err(StateError::LegacyValidationMismatch);
                }
            } else {
                let expected = HeadRef::owned(change_id).map_err(StateError::Domain)?;
                if managed.head() != &expected {
                    return Err(StateError::OwnershipHeadMismatch {
                        change_id: change_id.clone(),
                    });
                }
            }
            if !pr_numbers.insert(managed.number) || !heads.insert(managed.head()) {
                return Err(StateError::OwnershipConflict);
            }
        }
        Ok(())
    }

    fn validate_store_identity(&self, expected: StorageIdentity) -> Result<(), StateError> {
        if let CheckpointState::Executing(checkpoint) | CheckpointState::Completed(checkpoint) =
            &self.checkpoint
        {
            if let ExecutionOrigin::Acknowledged(receipt) = &checkpoint.origin {
                if receipt.store_identity != expected {
                    return Err(StateError::InvalidCheckpoint);
                }
            }
        }
        Ok(())
    }

    fn validate_plan_ownership(&self, plan: &Plan) -> Result<(), StateError> {
        use crate::plan::{Effect, GithubEffect, JjEffect};

        for effect in plan.effects() {
            let ownership = match effect {
                Effect::Github(
                    GithubEffect::UpdateBase { ownership, .. }
                    | GithubEffect::UpdateBody { ownership, .. }
                    | GithubEffect::SetLifecycle { ownership, .. },
                )
                | Effect::Jj(
                    JjEffect::PushHead { ownership, .. } | JjEffect::DeleteHead { ownership, .. },
                ) => ownership,
                _ => continue,
            };
            if !ownership.is_validated_legacy() {
                continue;
            }
            let Some(managed) = self
                .verified
                .get(ownership.change_id())
                .or_else(|| self.historic.get(ownership.change_id()))
            else {
                return Err(StateError::OwnershipConflict);
            };
            if managed.ownership() != ownership {
                return Err(StateError::OwnershipConflict);
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathDiagnostic {
    actual_bytes: usize,
    preview: String,
}

impl PathDiagnostic {
    pub(crate) fn new(path: &Path) -> Self {
        let bytes = path.as_os_str().as_bytes();
        Self {
            actual_bytes: bytes.len(),
            preview: String::from_utf8_lossy(bytes).chars().take(128).collect(),
        }
    }
}

impl Display for PathDiagnostic {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({} bytes)", self.preview, self.actual_bytes)
    }
}

pub enum StateError {
    Io {
        operation: StateOperation,
        source: io::Error,
    },
    UnsafePath {
        path: PathDiagnostic,
    },
    NamespaceDrift {
        path: PathDiagnostic,
    },
    SizeLimit {
        bytes: usize,
        max: usize,
    },
    Malformed {
        message: String,
    },
    FutureSchema {
        version: u32,
    },
    UnsupportedSchema {
        version: u32,
    },
    ScopeMismatch {
        expected: Box<Scope>,
        observed: Box<Scope>,
    },
    OwnershipCount {
        count: usize,
        max: usize,
    },
    OwnershipHeadMismatch {
        change_id: ChangeId,
    },
    OwnershipConflict,
    UnverifiedLegacy {
        count: usize,
    },
    LegacyEvidenceUntracked,
    LegacyDigestMismatch,
    LegacyValidationMismatch,
    InvalidMigrationState,
    LockMismatch,
    StaleState,
    ReloadRequired,
    InvalidCheckpoint,
    OccurrenceExhausted,
    Plan(PlanError),
    CommitDurabilityUnknown {
        source: io::Error,
    },
    LegacyDeletionDurabilityUnknown {
        source: io::Error,
    },
    LegacyDeletedStatePending {
        source: Box<StateError>,
    },
    Cleanup {
        primary: Box<StateError>,
        source: io::Error,
    },
    Domain(crate::domain::DomainError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateOperation {
    OpenDirectory,
    CreateDirectory,
    Open,
    Read,
    CreateTemporary,
    Write,
    Sync,
    Rename,
    Remove,
}

impl Display for StateError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { operation, source } => {
                write!(formatter, "state {operation:?} failed: {source}")
            }
            Self::UnsafePath { path } => write!(formatter, "unsafe state path: {path}"),
            Self::NamespaceDrift { path } => write!(
                formatter,
                "state namespace no longer names the opened directory: {}",
                path
            ),
            Self::SizeLimit { bytes, max } => {
                write!(
                    formatter,
                    "state exceeds {max} bytes (observed at least {bytes})"
                )
            }
            Self::Malformed { message } => write!(formatter, "malformed state: {message}"),
            Self::FutureSchema { version } => {
                write!(formatter, "state schema {version} is newer than supported")
            }
            Self::UnsupportedSchema { version } => {
                write!(formatter, "unsupported state schema {version}")
            }
            Self::ScopeMismatch { .. } => formatter.write_str("state scope does not match"),
            Self::OwnershipCount { count, max } => {
                write!(
                    formatter,
                    "state has {count} ownership rows, exceeding {max}"
                )
            }
            Self::OwnershipHeadMismatch { change_id } => {
                write!(formatter, "owned head does not match change {change_id}")
            }
            Self::OwnershipConflict => formatter.write_str("ownership identity conflicts"),
            Self::UnverifiedLegacy { count } => {
                write!(formatter, "{count} legacy candidates remain unverified")
            }
            Self::LegacyEvidenceUntracked => {
                formatter.write_str("legacy evidence exists outside migration state")
            }
            Self::LegacyDigestMismatch => formatter.write_str("legacy evidence digest changed"),
            Self::LegacyValidationMismatch => {
                formatter.write_str("legacy candidate does not match exact observation")
            }
            Self::InvalidMigrationState => formatter.write_str("invalid legacy migration state"),
            Self::LockMismatch => formatter.write_str("repository lock belongs to another store"),
            Self::StaleState => formatter.write_str("durable state changed since locked load"),
            Self::ReloadRequired => {
                formatter.write_str("state outcome is uncertain; reload required")
            }
            Self::InvalidCheckpoint => formatter.write_str("execution checkpoint is invalid"),
            Self::OccurrenceExhausted => {
                formatter.write_str("execution occurrence identity is exhausted")
            }
            Self::Plan(error) => Display::fmt(error, formatter),
            Self::CommitDurabilityUnknown { source } => write!(
                formatter,
                "state was installed but directory durability is unknown: {source}"
            ),
            Self::LegacyDeletionDurabilityUnknown { source } => write!(
                formatter,
                "legacy deletion may be installed but directory durability is unknown: {source}"
            ),
            Self::LegacyDeletedStatePending { source } => write!(
                formatter,
                "legacy evidence was durably deleted but Complete state is pending: {source}"
            ),
            Self::Cleanup { primary, source } => {
                write!(
                    formatter,
                    "{primary}; temporary cleanup also failed: {source}"
                )
            }
            Self::Domain(error) => Display::fmt(error, formatter),
        }
    }
}

impl fmt::Debug for StateError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl Error for StateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. }
            | Self::CommitDurabilityUnknown { source }
            | Self::LegacyDeletionDurabilityUnknown { source } => Some(source),
            Self::Cleanup { primary, .. } | Self::LegacyDeletedStatePending { source: primary } => {
                Some(primary)
            }
            Self::Domain(error) => Some(error),
            Self::Plan(error) => Some(error),
            _ => None,
        }
    }
}

impl StateError {
    fn requires_reload(&self) -> bool {
        match self {
            Self::CommitDurabilityUnknown { .. }
            | Self::LegacyDeletionDurabilityUnknown { .. }
            | Self::LegacyDeletedStatePending { .. } => true,
            Self::Cleanup { primary, .. } => primary.requires_reload(),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct StorageIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) enum StateGeneration {
    Missing,
    Present([u8; 32]),
}

#[derive(Clone)]
pub(crate) struct ExecutionSessionIdentity {
    anchor: std::sync::Arc<()>,
    generation: StateGeneration,
}

impl PartialEq for ExecutionSessionIdentity {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.anchor, &other.anchor) && self.generation == other.generation
    }
}

impl Eq for ExecutionSessionIdentity {}

impl fmt::Debug for ExecutionSessionIdentity {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionSessionIdentity")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

pub enum SessionError {
    Lock(LockError),
    State(StateError),
}

impl Display for SessionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lock(error) => Display::fmt(error, formatter),
            Self::State(error) => Display::fmt(error, formatter),
        }
    }
}

impl fmt::Debug for SessionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl Error for SessionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Lock(error) => Some(error),
            Self::State(error) => Some(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StorageFault {
    CreateTemporary,
    Write,
    FileSync,
    Rename,
    DirectorySync,
    LegacyUnlink,
    LegacyDirectorySync,
}

pub struct StateStore {
    workspace_directory: File,
    state_directory: File,
    workspace_root: PathBuf,
    state_directory_path: PathBuf,
    state_path: PathBuf,
    storage_identity: StorageIdentity,
    scope: Scope,
    limits: Limits,
    fault: std::sync::Mutex<Option<StorageFault>>,
}

impl StateStore {
    pub fn from_config(config: &ResolvedConfig) -> Result<Self, StateError> {
        if !config.has_configuration_lock() {
            return Err(StateError::LockMismatch);
        }
        let current_workspace = open_directory_no_follow(config.workspace_root())?;
        let current_jj = openat_directory(&current_workspace, ".jj")?;
        if identity(&current_workspace)? != identity(config.workspace_directory())?
            || identity(&current_jj)? != identity(config.jj_directory())?
        {
            return Err(StateError::NamespaceDrift {
                path: PathDiagnostic::new(config.workspace_root()),
            });
        }
        let workspace_directory =
            config
                .workspace_directory()
                .try_clone()
                .map_err(|source| StateError::Io {
                    operation: StateOperation::OpenDirectory,
                    source,
                })?;
        let jj_directory = config
            .jj_directory()
            .try_clone()
            .map_err(|source| StateError::Io {
                operation: StateOperation::OpenDirectory,
                source,
            })?;
        Self::from_open_directories(
            config.workspace_root(),
            workspace_directory,
            jj_directory,
            config.scope().clone(),
            config.limits(),
        )
    }

    pub fn open(workspace_root: &Path, scope: Scope, limits: Limits) -> Result<Self, StateError> {
        let canonical = workspace_root
            .canonicalize()
            .map_err(|source| StateError::Io {
                operation: StateOperation::OpenDirectory,
                source,
            })?;
        if canonical != workspace_root {
            return Err(StateError::UnsafePath {
                path: PathDiagnostic::new(workspace_root),
            });
        }
        let workspace_directory = open_directory_no_follow(workspace_root)?;
        let jj_directory = openat_directory(&workspace_directory, ".jj")?;
        Self::from_open_directories(
            workspace_root,
            workspace_directory,
            jj_directory,
            scope,
            limits,
        )
    }

    fn from_open_directories(
        workspace_root: &Path,
        workspace_directory: File,
        jj_directory: File,
        scope: Scope,
        limits: Limits,
    ) -> Result<Self, StateError> {
        let state_directory = open_or_create_directory(&jj_directory, "almighty-push")?;
        Self::from_state_directory(
            workspace_root,
            workspace_directory,
            state_directory,
            scope,
            limits,
        )
    }

    fn from_state_directory(
        workspace_root: &Path,
        workspace_directory: File,
        state_directory: File,
        scope: Scope,
        limits: Limits,
    ) -> Result<Self, StateError> {
        let state_directory_path = workspace_root.join(".jj/almighty-push");
        let storage_identity = identity(&state_directory)?;
        let metadata = state_directory
            .metadata()
            .map_err(|source| StateError::Io {
                operation: StateOperation::OpenDirectory,
                source,
            })?;
        // SAFETY: geteuid has no preconditions.
        let effective_uid = unsafe { libc::geteuid() };
        if metadata.uid() != effective_uid || metadata.mode() & 0o077 != 0 {
            return Err(StateError::UnsafePath {
                path: PathDiagnostic::new(&state_directory_path),
            });
        }
        Ok(Self {
            workspace_directory,
            state_directory,
            workspace_root: workspace_root.to_owned(),
            state_path: state_directory_path.join(STATE_FILE),
            state_directory_path,
            storage_identity,
            scope,
            limits,
            fault: std::sync::Mutex::new(None),
        })
    }

    /// Loads state for preview without creating the state namespace, lock, or
    /// any other repository file.
    pub fn load_read_only_from_config(config: &ResolvedConfig) -> Result<StateV3, StateError> {
        let current_workspace = open_directory_no_follow(config.workspace_root())?;
        let current_jj = openat_directory(&current_workspace, ".jj")?;
        if identity(&current_workspace)? != identity(config.workspace_directory())?
            || identity(&current_jj)? != identity(config.jj_directory())?
        {
            return Err(StateError::NamespaceDrift {
                path: PathDiagnostic::new(config.workspace_root()),
            });
        }
        match openat_directory(&current_jj, "almighty-push") {
            Ok(state_directory) => Self::from_state_directory(
                config.workspace_root(),
                current_workspace,
                state_directory,
                config.scope().clone(),
                config.limits(),
            )?
            .load(),
            Err(StateError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                let legacy = read_optional_file(
                    &current_workspace,
                    LEGACY_FILE,
                    config.limits().state_bytes_max(),
                )?;
                decode_legacy_or_empty(config.scope(), config.limits(), legacy)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn validate_namespace(&self) -> Result<(), StateError> {
        let current = open_directory_no_follow(&self.state_directory_path)?;
        let metadata = current.metadata().map_err(|source| StateError::Io {
            operation: StateOperation::OpenDirectory,
            source,
        })?;
        // SAFETY: geteuid has no preconditions.
        let effective_uid = unsafe { libc::geteuid() };
        let metadata_drift = metadata.uid() != effective_uid || metadata.mode() & 0o077 != 0;
        if identity(&current)? != self.storage_identity || metadata_drift {
            return Err(StateError::NamespaceDrift {
                path: PathDiagnostic::new(&self.state_directory_path),
            });
        }
        Ok(())
    }

    pub(crate) fn storage_identity(&self) -> StorageIdentity {
        self.storage_identity
    }

    pub(crate) fn state_directory_handle(&self) -> &File {
        &self.state_directory
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn state_directory_path(&self) -> &Path {
        &self.state_directory_path
    }

    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    pub fn load(&self) -> Result<StateV3, StateError> {
        self.load_with_generation().map(|(state, _)| state)
    }

    pub fn lock(&self) -> Result<LockedStateSession<'_>, SessionError> {
        LockedStateSession::acquire(self)
    }

    fn load_with_generation(&self) -> Result<(StateV3, StateGeneration), StateError> {
        match openat_file(&self.state_directory, STATE_FILE) {
            Ok(file) => {
                validate_private_regular(&file, Path::new(STATE_FILE))?;
                let bytes = read_bounded(file, self.limits.state_bytes_max())?;
                let generation = StateGeneration::Present(Sha256::digest(&bytes).into());
                self.decode_v3(&bytes).map(|state| (state, generation))
            }
            Err(StateError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => self
                .load_legacy_or_empty()
                .map(|state| (state, StateGeneration::Missing)),
            Err(error) => Err(error),
        }
    }

    fn publish(
        &self,
        lock: &RepositoryLock,
        expected: StateGeneration,
        state: &StateV3,
    ) -> Result<StateGeneration, StateError> {
        self.validate_lock(lock)?;
        self.validate_namespace()?;
        if self.current_generation()? != expected {
            return Err(StateError::StaleState);
        }
        state.validate(&self.scope, self.limits)?;
        state.validate_store_identity(self.storage_identity)?;
        self.validate_migration_source(state)?;
        validate_publish_target(&self.state_directory)?;
        let bytes = serialize_bounded(state, self.limits.state_bytes_max())?;
        self.fail_if_requested(StorageFault::CreateTemporary)
            .map_err(|source| StateError::Io {
                operation: StateOperation::CreateTemporary,
                source,
            })?;
        let (temporary_name, mut temporary) = create_unique_temporary(&self.state_directory)?;
        let preinstall = self
            .fail_if_requested(StorageFault::Write)
            .map_err(|source| StateError::Io {
                operation: StateOperation::Write,
                source,
            })
            .and_then(|()| {
                temporary
                    .write_all(&bytes)
                    .map_err(|source| StateError::Io {
                        operation: StateOperation::Write,
                        source,
                    })
            })
            .and_then(|()| {
                self.fail_if_requested(StorageFault::FileSync)
                    .map_err(|source| StateError::Io {
                        operation: StateOperation::Sync,
                        source,
                    })
            })
            .and_then(|()| sync_file(&temporary));
        if let Err(primary) = preinstall {
            return match removeat_if_exists(&self.state_directory, &temporary_name) {
                Ok(()) => Err(primary),
                Err(StateError::Io { source, .. }) => Err(StateError::Cleanup {
                    primary: Box::new(primary),
                    source,
                }),
                Err(other) => Err(other),
            };
        }
        self.fail_if_requested(StorageFault::Rename)
            .map_err(|source| StateError::Io {
                operation: StateOperation::Rename,
                source,
            })?;
        renameat(&self.state_directory, &temporary_name, STATE_FILE)?;
        self.fail_if_requested(StorageFault::DirectorySync)
            .and_then(|()| self.state_directory.sync_all())
            .map_err(|source| StateError::CommitDurabilityUnknown { source })?;
        let installed = openat_file(&self.state_directory, STATE_FILE)
            .map_err(|error| durability_unknown(error, "open installed state"))?;
        sync_file(&installed).map_err(|error| durability_unknown(error, "sync installed state"))?;
        self.state_directory
            .sync_all()
            .map_err(|source| StateError::CommitDurabilityUnknown { source })?;
        Ok(StateGeneration::Present(Sha256::digest(&bytes).into()))
    }

    fn current_generation(&self) -> Result<StateGeneration, StateError> {
        match openat_file(&self.state_directory, STATE_FILE) {
            Ok(file) => {
                validate_private_regular(&file, Path::new(STATE_FILE))?;
                let bytes = read_bounded(file, self.limits.state_bytes_max())?;
                Ok(StateGeneration::Present(Sha256::digest(&bytes).into()))
            }
            Err(StateError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Ok(StateGeneration::Missing)
            }
            Err(error) => Err(error),
        }
    }

    fn delete_legacy_after_verified(
        &self,
        lock: &RepositoryLock,
        expected: StateGeneration,
        mut state: StateV3,
    ) -> Result<(StateV3, StateGeneration), StateError> {
        self.validate_lock(lock)?;
        self.validate_namespace()?;
        let (source_digest, resolved) = match &state.legacy_migration {
            LegacyMigration::ReadyToDelete {
                source_digest,
                resolved,
            } => (*source_digest, resolved.clone()),
            _ => {
                let count = state.legacy_candidates().len();
                return if count == 0 {
                    Err(StateError::InvalidMigrationState)
                } else {
                    Err(StateError::UnverifiedLegacy { count })
                };
            }
        };
        let persisted = self.load()?;
        if persisted != state {
            return Err(StateError::Malformed {
                message: "persisted state differs before legacy deletion".to_owned(),
            });
        }
        match self.read_legacy_bytes()? {
            Some(bytes) if legacy_digest(&bytes) != source_digest => {
                return Err(StateError::LegacyDigestMismatch);
            }
            Some(_) => {
                self.fail_if_requested(StorageFault::LegacyUnlink)
                    .map_err(|source| StateError::Io {
                        operation: StateOperation::Remove,
                        source,
                    })?;
                removeat_if_exists(&self.workspace_directory, LEGACY_FILE)?;
                self.fail_if_requested(StorageFault::LegacyDirectorySync)
                    .and_then(|()| self.workspace_directory.sync_all())
                    .map_err(|source| StateError::LegacyDeletionDurabilityUnknown { source })?;
                let ready_state =
                    openat_file(&self.state_directory, STATE_FILE).map_err(|error| {
                        legacy_durability_unknown(error, "open migration state barrier")
                    })?;
                sync_file(&ready_state).map_err(|error| {
                    legacy_durability_unknown(error, "sync migration state barrier")
                })?;
            }
            None => {}
        }
        state.legacy_migration = LegacyMigration::Complete {
            source_digest,
            resolved,
        };
        match self.publish(lock, expected, &state) {
            Ok(generation) => Ok((state, generation)),
            Err(source) => Err(StateError::LegacyDeletedStatePending {
                source: Box::new(source),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn inject_fault(&self, fault: StorageFault) {
        *self.fault.lock().expect("fault mutex is not poisoned") = Some(fault);
    }

    fn fail_if_requested(&self, stage: StorageFault) -> Result<(), io::Error> {
        let mut fault = self.fault.lock().expect("fault mutex is not poisoned");
        if *fault == Some(stage) {
            *fault = None;
            return Err(io::Error::other(format!("injected {stage:?} failure")));
        }
        Ok(())
    }

    fn validate_lock(&self, lock: &RepositoryLock) -> Result<(), StateError> {
        if !lock.validates_store(self) {
            return Err(StateError::LockMismatch);
        }
        Ok(())
    }

    fn decode_v3(&self, bytes: &[u8]) -> Result<StateV3, StateError> {
        JsonShape::validate(bytes, self.limits)?;
        let probe: SchemaProbe =
            serde_json::from_slice(bytes).map_err(|error| StateError::Malformed {
                message: bounded_serde_error(&error),
            })?;
        if probe.schema_version > SCHEMA_VERSION {
            return Err(StateError::FutureSchema {
                version: probe.schema_version,
            });
        }
        if probe.schema_version != SCHEMA_VERSION {
            return Err(StateError::UnsupportedSchema {
                version: probe.schema_version,
            });
        }
        let wire: StateV3Wire =
            serde_json::from_slice(bytes).map_err(|error| StateError::Malformed {
                message: bounded_serde_error(&error),
            })?;
        let state = StateV3 {
            schema_version: wire.schema_version,
            scope: wire.scope,
            execution_occurrence: wire.execution_occurrence,
            verified: wire.verified.0,
            historic: wire.historic.0,
            legacy_migration: wire.legacy_migration,
            checkpoint: wire.checkpoint,
        };
        state.validate(&self.scope, self.limits)?;
        state.validate_store_identity(self.storage_identity)?;
        self.validate_migration_source(&state)?;
        Ok(state)
    }

    fn validate_migration_source(&self, state: &StateV3) -> Result<(), StateError> {
        let current = self.read_legacy_bytes()?;
        match (&state.legacy_migration, current) {
            (LegacyMigration::None, None) | (LegacyMigration::Complete { .. }, None) => Ok(()),
            (LegacyMigration::None | LegacyMigration::Complete { .. }, Some(_)) => {
                Err(StateError::LegacyEvidenceUntracked)
            }
            (LegacyMigration::Pending { source_digest, .. }, Some(bytes))
            | (LegacyMigration::ReadyToDelete { source_digest, .. }, Some(bytes)) => {
                if legacy_digest(&bytes) == *source_digest {
                    Ok(())
                } else {
                    Err(StateError::LegacyDigestMismatch)
                }
            }
            (LegacyMigration::ReadyToDelete { .. }, None) => Ok(()),
            (LegacyMigration::Pending { .. }, None) => Err(StateError::LegacyDigestMismatch),
        }
    }

    fn read_legacy_bytes(&self) -> Result<Option<Vec<u8>>, StateError> {
        match openat_file(&self.workspace_directory, LEGACY_FILE) {
            Ok(file) => read_bounded(file, self.limits.state_bytes_max()).map(Some),
            Err(StateError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn load_legacy_or_empty(&self) -> Result<StateV3, StateError> {
        decode_legacy_or_empty(&self.scope, self.limits, self.read_legacy_bytes()?)
    }
}

fn read_optional_file(
    directory: &File,
    name: &str,
    bytes_max: usize,
) -> Result<Option<Vec<u8>>, StateError> {
    match openat_file(directory, name) {
        Ok(file) => read_bounded(file, bytes_max).map(Some),
        Err(StateError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn decode_legacy_or_empty(
    scope: &Scope,
    limits: Limits,
    legacy_bytes: Option<Vec<u8>>,
) -> Result<StateV3, StateError> {
    let mut state = StateV3::empty(scope.clone());
    let Some(bytes) = legacy_bytes else {
        return Ok(state);
    };
    JsonShape::validate(&bytes, limits)?;
    let legacy: LegacyState =
        serde_json::from_slice(&bytes).map_err(|error| StateError::Malformed {
            message: bounded_serde_error(&error),
        })?;
    if legacy.version != 2 {
        return Err(StateError::UnsupportedSchema {
            version: legacy.version,
        });
    }
    legacy.validate(limits)?;
    let mut candidates = Vec::with_capacity(legacy.prs.0.len());
    for (legacy_key, pr) in legacy.prs.0 {
        candidates.push(LegacyCandidate::new(
            legacy_key,
            u64::from(pr.pr_number),
            pr.pr_url,
            pr.branch_name,
            pr.commit_id,
            pr.change_id,
        )?);
    }
    state.set_legacy_candidates(legacy_digest(&bytes), candidates);
    state.validate(scope, limits)?;
    Ok(state)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OwnershipEffectState {
    Satisfied,
    Ready,
    Drift,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SatisfactionSource {
    ExternalPostcondition,
    OwnershipPrecondition,
    OwnershipPostcondition,
    Barrier,
}

pub(crate) struct SatisfiedEffect {
    result: EffectResult,
    source: SatisfactionSource,
}

impl SatisfiedEffect {
    pub(crate) fn new(result: EffectResult, source: SatisfactionSource) -> Self {
        Self { result, source }
    }
}

pub struct LockedStateSession<'store> {
    store: &'store StateStore,
    lock: RepositoryLock,
    state: StateV3,
    generation: Option<StateGeneration>,
    session_anchor: std::sync::Arc<()>,
}

impl<'store> LockedStateSession<'store> {
    fn acquire(store: &'store StateStore) -> Result<Self, SessionError> {
        let lock = RepositoryLock::acquire(store, store.limits).map_err(SessionError::Lock)?;
        let (state, generation) = store.load_with_generation().map_err(SessionError::State)?;
        Ok(Self {
            store,
            lock,
            state,
            generation: Some(generation),
            session_anchor: std::sync::Arc::new(()),
        })
    }

    pub fn state(&self) -> &StateV3 {
        &self.state
    }

    pub(crate) fn store(&self) -> &StateStore {
        self.store
    }

    pub fn persist_loaded(&mut self) -> Result<(), StateError> {
        self.commit(self.state.clone())
    }

    pub fn start_plan(&mut self, plan: Plan) -> Result<(), StateError> {
        let mut next = self.state.clone();
        next.start_plan(plan, self.store.limits)?;
        self.commit(next)
    }

    pub(crate) fn start_next_plan(
        &mut self,
        completed_receipt: CompletionReceiptData,
        plan: Plan,
    ) -> Result<(), StateError> {
        if completed_receipt.generation != self.expected_generation()?
            || completed_receipt.store_identity != self.store.storage_identity
        {
            return Err(StateError::InvalidCheckpoint);
        }
        let mut next = self.state.clone();
        next.start_next_plan(completed_receipt, plan, self.store.limits)?;
        self.commit(next)
    }

    pub(crate) fn completion_receipt(&self) -> Result<CompletionReceiptData, StateError> {
        self.state
            .completed_receipt(self.expected_generation()?, self.store.storage_identity)
    }

    pub(crate) fn execution_identity(&self) -> Result<ExecutionSessionIdentity, StateError> {
        Ok(ExecutionSessionIdentity {
            anchor: self.session_anchor.clone(),
            generation: self.expected_generation()?,
        })
    }

    pub(crate) fn record_satisfied(&mut self, proof: SatisfiedEffect) -> Result<(), StateError> {
        let mut next = self.state.clone();
        next.record_current_result(proof)?;
        self.commit(next)
    }

    pub(crate) fn publish_completion(&mut self) -> Result<(), StateError> {
        let mut next = self.state.clone();
        next.publish_completion()?;
        self.commit(next)
    }

    pub fn resolve_legacy(
        &mut self,
        resolution: LegacyCandidateResolution,
    ) -> Result<(), StateError> {
        let mut next = self.state.clone();
        next.resolve_legacy(resolution)?;
        self.commit(next)
    }

    pub fn finish_legacy_deletion(&mut self) -> Result<(), StateError> {
        let expected = self.expected_generation()?;
        match self
            .store
            .delete_legacy_after_verified(&self.lock, expected, self.state.clone())
        {
            Ok((state, generation)) => {
                self.state = state;
                self.generation = Some(generation);
                Ok(())
            }
            Err(error) => {
                if error.requires_reload() {
                    self.generation = None;
                }
                Err(error)
            }
        }
    }

    fn commit(&mut self, next: StateV3) -> Result<(), StateError> {
        let expected = self.expected_generation()?;
        match self.store.publish(&self.lock, expected, &next) {
            Ok(generation) => {
                self.state = next;
                self.generation = Some(generation);
                Ok(())
            }
            Err(error) => {
                if error.requires_reload() {
                    self.generation = None;
                }
                Err(error)
            }
        }
    }

    fn expected_generation(&self) -> Result<StateGeneration, StateError> {
        self.generation.ok_or(StateError::ReloadRequired)
    }
}

impl fmt::Debug for LockedStateSession<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LockedStateSession")
            .field("store", self.store)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for StateStore {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StateStore")
            .field("workspace_root", &self.workspace_root)
            .field("state_path", &self.state_path)
            .finish_non_exhaustive()
    }
}

struct JsonShape<'a> {
    bytes: &'a [u8],
    cursor: usize,
    collection_max: usize,
    string_bytes_max: usize,
}

impl<'a> JsonShape<'a> {
    fn validate(bytes: &'a [u8], limits: Limits) -> Result<(), StateError> {
        let mut parser = Self {
            bytes,
            cursor: 0,
            collection_max: limits.effect_count_max(),
            string_bytes_max: limits.body_bytes_max(),
        };
        parser.value(0)?;
        parser.whitespace();
        if parser.cursor != bytes.len() {
            return Err(StateError::Malformed {
                message: "JSON contains trailing data".to_owned(),
            });
        }
        Ok(())
    }

    fn value(&mut self, depth: usize) -> Result<(), StateError> {
        if depth > 32 {
            return self.invalid("JSON nesting exceeds 32 levels");
        }
        self.whitespace();
        match self.peek() {
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'"') => self.string(),
            Some(b't') => self.literal(b"true"),
            Some(b'f') => self.literal(b"false"),
            Some(b'n') => self.literal(b"null"),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => self.invalid("JSON value is malformed"),
        }
    }

    fn object(&mut self, depth: usize) -> Result<(), StateError> {
        self.cursor += 1;
        self.whitespace();
        if self.take(b'}') {
            return Ok(());
        }
        let mut count = 0usize;
        loop {
            count += 1;
            if count > self.collection_max {
                return self.invalid("JSON object exceeds collection bound");
            }
            self.string()?;
            self.whitespace();
            if !self.take(b':') {
                return self.invalid("JSON object lacks a colon");
            }
            self.value(depth)?;
            self.whitespace();
            if self.take(b'}') {
                return Ok(());
            }
            if !self.take(b',') {
                return self.invalid("JSON object lacks a separator");
            }
            self.whitespace();
        }
    }

    fn array(&mut self, depth: usize) -> Result<(), StateError> {
        self.cursor += 1;
        self.whitespace();
        if self.take(b']') {
            return Ok(());
        }
        let mut count = 0usize;
        loop {
            count += 1;
            if count > self.collection_max {
                return self.invalid("JSON array exceeds collection bound");
            }
            self.value(depth)?;
            self.whitespace();
            if self.take(b']') {
                return Ok(());
            }
            if !self.take(b',') {
                return self.invalid("JSON array lacks a separator");
            }
        }
    }

    fn string(&mut self) -> Result<(), StateError> {
        self.whitespace();
        if !self.take(b'"') {
            return self.invalid("JSON object key is not a string");
        }
        let start = self.cursor;
        loop {
            let Some(byte) = self.peek() else {
                return self.invalid("JSON string is unterminated");
            };
            match byte {
                b'"' => {
                    let raw_bytes = self.cursor - start;
                    if raw_bytes > self.string_bytes_max {
                        return self.invalid("JSON string exceeds configured bound");
                    }
                    self.cursor += 1;
                    return Ok(());
                }
                b'\\' => {
                    self.cursor += 1;
                    let Some(escape) = self.peek() else {
                        return self.invalid("JSON escape is unterminated");
                    };
                    self.cursor += 1;
                    if escape == b'u' {
                        for _ in 0..4 {
                            if !self.peek().is_some_and(|digit| digit.is_ascii_hexdigit()) {
                                return self.invalid("JSON unicode escape is malformed");
                            }
                            self.cursor += 1;
                        }
                    } else if !matches!(
                        escape,
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't'
                    ) {
                        return self.invalid("JSON escape is malformed");
                    }
                }
                0..=0x1f => return self.invalid("JSON string contains a control byte"),
                _ => self.cursor += 1,
            }
            if self.cursor - start > self.string_bytes_max {
                return self.invalid("JSON string exceeds configured bound");
            }
        }
    }

    fn number(&mut self) -> Result<(), StateError> {
        let start = self.cursor;
        while self
            .peek()
            .is_some_and(|byte| matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9'))
        {
            self.cursor += 1;
        }
        if self.cursor == start {
            return self.invalid("JSON number is malformed");
        }
        Ok(())
    }

    fn literal(&mut self, literal: &[u8]) -> Result<(), StateError> {
        if self.bytes.get(self.cursor..self.cursor + literal.len()) != Some(literal) {
            return self.invalid("JSON literal is malformed");
        }
        self.cursor += literal.len();
        Ok(())
    }

    fn whitespace(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.cursor += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.cursor).copied()
    }

    fn take(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn invalid<T>(&self, message: &str) -> Result<T, StateError> {
        Err(StateError::Malformed {
            message: message.to_owned(),
        })
    }
}

struct UniqueMap<K, V>(BTreeMap<K, V>);

impl<K, V> Default for UniqueMap<K, V> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<'de, K, V> Deserialize<'de> for UniqueMap<K, V>
where
    K: Deserialize<'de> + Ord,
    V: Deserialize<'de>,
{
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueMapVisitor<K, V>(std::marker::PhantomData<(K, V)>);

        impl<'de, K, V> Visitor<'de> for UniqueMapVisitor<K, V>
        where
            K: Deserialize<'de> + Ord,
            V: Deserialize<'de>,
        {
            type Value = UniqueMap<K, V>;

            fn expecting(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map with unique bounded keys")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
                let mut rows = BTreeMap::new();
                while let Some(key) = access.next_key()? {
                    if rows.len() >= 64 || rows.contains_key(&key) {
                        return Err(serde::de::Error::custom(
                            "map exceeds 64 rows or contains a duplicate key",
                        ));
                    }
                    let value = access.next_value()?;
                    rows.insert(key, value);
                }
                Ok(UniqueMap(rows))
            }
        }

        deserializer.deserialize_map(UniqueMapVisitor(std::marker::PhantomData))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateV3Wire {
    schema_version: u32,
    scope: Scope,
    execution_occurrence: u64,
    verified: UniqueMap<ChangeId, ManagedPr>,
    historic: UniqueMap<ChangeId, ManagedPr>,
    legacy_migration: LegacyMigration,
    checkpoint: CheckpointState,
}

#[derive(Deserialize)]
struct SchemaProbe {
    schema_version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyState {
    version: u32,
    prs: UniqueMap<String, LegacyPr>,
    merged_prs: Vec<String>,
    closed_prs: Vec<String>,
    last_operation_id: Option<String>,
    #[serde(default)]
    stack_order: Vec<String>,
    #[serde(default)]
    operations: Vec<LegacyOperation>,
    #[serde(default)]
    last_updated: Option<String>,
    #[serde(default)]
    merged_into_pr: UniqueMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyOperation {
    id: String,
    op_type: String,
    timestamp: String,
    changes_affected: Vec<String>,
    success: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyPr {
    pr_number: u32,
    pr_url: String,
    branch_name: String,
    commit_id: String,
    change_id: Option<String>,
}

fn bounded_serde_error(error: &serde_json::Error) -> String {
    format!(
        "JSON {:?} at line {}, column {}",
        error.classify(),
        error.line(),
        error.column()
    )
}

fn identity(file: &File) -> Result<StorageIdentity, StateError> {
    let metadata = file.metadata().map_err(|source| StateError::Io {
        operation: StateOperation::OpenDirectory,
        source,
    })?;
    Ok(StorageIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

fn legacy_digest(bytes: &[u8]) -> LegacyDigest {
    LegacyDigest(Sha256::digest(bytes).into())
}

fn serialize_bounded(state: &StateV3, max: usize) -> Result<Vec<u8>, StateError> {
    let mut writer = BoundedWriter::new(max);
    serde_json::to_writer(&mut writer, state).map_err(|error| {
        if writer.exceeded {
            StateError::SizeLimit {
                bytes: max.saturating_add(1),
                max,
            }
        } else {
            StateError::Malformed {
                message: bounded_serde_error(&error),
            }
        }
    })?;
    Ok(writer.bytes)
}

struct BoundedWriter {
    bytes: Vec<u8>,
    max: usize,
    exceeded: bool,
}

impl BoundedWriter {
    fn new(max: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max,
            exceeded: false,
        }
    }
}

impl LegacyState {
    fn validate(&self, limits: Limits) -> Result<(), StateError> {
        if self.prs.0.len() > limits.change_count_max()
            || self.merged_prs.len() > limits.change_count_max()
            || self.closed_prs.len() > limits.change_count_max()
            || self.stack_order.len() > limits.change_count_max()
            || self.merged_into_pr.0.len() > limits.change_count_max()
            || self.operations.len() > limits.effect_count_max()
        {
            return Err(StateError::Malformed {
                message: "legacy collection exceeds configured bounds".to_owned(),
            });
        }
        reject_duplicate_text(&self.merged_prs)?;
        reject_duplicate_text(&self.closed_prs)?;
        reject_duplicate_text(&self.stack_order)?;
        validate_optional_legacy_text(self.last_operation_id.as_deref())?;
        validate_optional_legacy_text(self.last_updated.as_deref())?;
        for value in self
            .merged_prs
            .iter()
            .chain(&self.closed_prs)
            .chain(&self.stack_order)
            .chain(self.merged_into_pr.0.keys())
            .chain(self.merged_into_pr.0.values())
        {
            validate_optional_legacy_text(Some(value))?;
        }
        for operation in &self.operations {
            for value in [&operation.id, &operation.op_type, &operation.timestamp] {
                validate_optional_legacy_text(Some(value))?;
            }
            if operation.changes_affected.len() > limits.change_count_max() {
                return Err(StateError::Malformed {
                    message: "legacy operation exceeds change bound".to_owned(),
                });
            }
            reject_duplicate_text(&operation.changes_affected)?;
            for change in &operation.changes_affected {
                validate_optional_legacy_text(Some(change))?;
            }
            let _ = operation.success;
        }
        for pr in self.prs.0.values() {
            validate_optional_legacy_text(Some(&pr.pr_url))?;
        }
        Ok(())
    }
}

fn reject_duplicate_text(values: &[String]) -> Result<(), StateError> {
    let mut unique = std::collections::BTreeSet::new();
    if values.iter().all(|value| unique.insert(value)) {
        Ok(())
    } else {
        Err(StateError::Malformed {
            message: "legacy collection contains duplicate identity".to_owned(),
        })
    }
}

fn validate_optional_legacy_text(value: Option<&str>) -> Result<(), StateError> {
    if let Some(value) = value {
        if value.len() > LEGACY_TEXT_BYTES_MAX || value.bytes().any(|byte| byte == 0) {
            return Err(StateError::Malformed {
                message: "legacy field exceeds bound".to_owned(),
            });
        }
    }
    Ok(())
}

impl Write for BoundedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let next = self.bytes.len().saturating_add(buffer.len());
        if next > self.max {
            self.exceeded = true;
            return Err(io::Error::new(io::ErrorKind::FileTooLarge, "state limit"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn read_bounded(file: File, max: usize) -> Result<Vec<u8>, StateError> {
    let mut bytes = Vec::new();
    file.take((max as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|source| StateError::Io {
            operation: StateOperation::Read,
            source,
        })?;
    if bytes.len() > max {
        return Err(StateError::SizeLimit {
            bytes: bytes.len(),
            max,
        });
    }
    Ok(bytes)
}

fn open_directory_no_follow(path: &Path) -> Result<File, StateError> {
    let mut directory = File::open("/").map_err(|source| StateError::Io {
        operation: StateOperation::OpenDirectory,
        source,
    })?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            if matches!(component, Component::RootDir) {
                continue;
            }
            return Err(StateError::UnsafePath {
                path: PathDiagnostic::new(path),
            });
        };
        directory = openat_directory_os(&directory, name.as_bytes(), path)?;
    }
    Ok(directory)
}

fn openat_directory(parent: &File, name: &str) -> Result<File, StateError> {
    openat_directory_os(parent, name.as_bytes(), Path::new(name))
}

fn openat_directory_os(parent: &File, name: &[u8], display: &Path) -> Result<File, StateError> {
    let name = CString::new(name).map_err(|_| StateError::UnsafePath {
        path: PathDiagnostic::new(display),
    })?;
    // SAFETY: parent and component are valid; O_NOFOLLOW rejects link traversal.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(open_error(StateOperation::OpenDirectory, display));
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn open_or_create_directory(parent: &File, name: &str) -> Result<File, StateError> {
    match openat_directory(parent, name) {
        Ok(directory) => Ok(directory),
        Err(StateError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {
            let name_c = CString::new(name).expect("static directory name has no NUL");
            // SAFETY: parent and static name are valid.
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
                let source = io::Error::last_os_error();
                if source.kind() != io::ErrorKind::AlreadyExists {
                    return Err(StateError::Io {
                        operation: StateOperation::CreateDirectory,
                        source,
                    });
                }
            }
            let directory = openat_directory(parent, name)?;
            parent.sync_all().map_err(|source| StateError::Io {
                operation: StateOperation::Sync,
                source,
            })?;
            Ok(directory)
        }
        Err(error) => Err(error),
    }
}

fn openat_file(parent: &File, name: &str) -> Result<File, StateError> {
    let name_c = CString::new(name).expect("static file name has no NUL");
    // SAFETY: parent and static name are valid; O_NOFOLLOW rejects final symlinks.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error
            .raw_os_error()
            .is_some_and(|code| code == libc::ELOOP || code == libc::EISDIR)
        {
            return Err(StateError::UnsafePath {
                path: PathDiagnostic::new(Path::new(name)),
            });
        }
        return Err(StateError::Io {
            operation: StateOperation::Open,
            source: error,
        });
    }
    // SAFETY: openat returned a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    if !file
        .metadata()
        .map_err(|source| StateError::Io {
            operation: StateOperation::Open,
            source,
        })?
        .is_file()
    {
        return Err(StateError::UnsafePath {
            path: PathDiagnostic::new(Path::new(name)),
        });
    }
    Ok(file)
}

fn validate_private_regular(file: &File, path: &Path) -> Result<(), StateError> {
    let metadata = file.metadata().map_err(|source| StateError::Io {
        operation: StateOperation::Open,
        source,
    })?;
    // SAFETY: geteuid has no preconditions.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid || metadata.nlink() != 1 || metadata.mode() & 0o077 != 0 {
        return Err(StateError::UnsafePath {
            path: PathDiagnostic::new(path),
        });
    }
    Ok(())
}

fn validate_publish_target(parent: &File) -> Result<(), StateError> {
    match openat_file(parent, STATE_FILE) {
        Ok(file) => validate_private_regular(&file, Path::new(STATE_FILE)),
        Err(StateError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn create_unique_temporary(parent: &File) -> Result<(String, File), StateError> {
    match openat_file(parent, TEMP_FILE) {
        Ok(file) => {
            validate_private_regular(&file, Path::new(TEMP_FILE))?;
            removeat_if_exists(parent, TEMP_FILE)?;
            parent.sync_all().map_err(|source| StateError::Io {
                operation: StateOperation::Sync,
                source,
            })?;
        }
        Err(StateError::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let file = createat_file(parent, TEMP_FILE)?;
    Ok((TEMP_FILE.to_owned(), file))
}

fn createat_file(parent: &File, name: &str) -> Result<File, StateError> {
    let name_c = CString::new(name).map_err(|_| StateError::UnsafePath {
        path: PathDiagnostic::new(Path::new(name)),
    })?;
    // SAFETY: parent and name are valid; EXCL and NOFOLLOW prevent replacement.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(open_error(StateOperation::CreateTemporary, Path::new(name)));
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn durability_unknown(error: StateError, operation: &'static str) -> StateError {
    StateError::CommitDurabilityUnknown {
        source: io::Error::other(format!("{operation}: {error}")),
    }
}

fn legacy_durability_unknown(error: StateError, operation: &'static str) -> StateError {
    StateError::LegacyDeletionDurabilityUnknown {
        source: io::Error::other(format!("{operation}: {error}")),
    }
}

fn sync_file(file: &File) -> Result<(), StateError> {
    file.sync_all().map_err(|source| StateError::Io {
        operation: StateOperation::Sync,
        source,
    })?;
    #[cfg(target_os = "macos")]
    {
        // SAFETY: F_FULLFSYNC operates on the valid temporary file descriptor.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } != 0 {
            return Err(StateError::Io {
                operation: StateOperation::Sync,
                source: io::Error::last_os_error(),
            });
        }
    }
    Ok(())
}

fn renameat(parent: &File, source: &str, target: &str) -> Result<(), StateError> {
    let source = CString::new(source).expect("generated name has no NUL");
    let target = CString::new(target).expect("static name has no NUL");
    // SAFETY: both names are relative to the same valid directory descriptor.
    if unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            target.as_ptr(),
        )
    } != 0
    {
        return Err(StateError::Io {
            operation: StateOperation::Rename,
            source: io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn removeat_if_exists(parent: &File, name: &str) -> Result<(), StateError> {
    let name = CString::new(name).map_err(|_| StateError::UnsafePath {
        path: PathDiagnostic::new(Path::new("invalid name")),
    })?;
    // SAFETY: name is relative to the valid retained directory descriptor.
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } != 0 {
        let source = io::Error::last_os_error();
        if source.kind() != io::ErrorKind::NotFound {
            return Err(StateError::Io {
                operation: StateOperation::Remove,
                source,
            });
        }
    }
    Ok(())
}

fn open_error(operation: StateOperation, path: &Path) -> StateError {
    let source = io::Error::last_os_error();
    if source
        .raw_os_error()
        .is_some_and(|code| code == libc::ELOOP || code == libc::ENOTDIR)
    {
        StateError::UnsafePath {
            path: PathDiagnostic::new(path),
        }
    } else {
        StateError::Io { operation, source }
    }
}

#[cfg(test)]
mod state_machine_tests {
    use super::*;
    use crate::body::ManagedSection;
    use crate::domain::{LimitValues, RemoteName, RepositoryId};
    use crate::plan::{Effect, GithubEffect, OwnershipEffect};
    use std::os::unix::fs::PermissionsExt;

    fn limits() -> Limits {
        Limits::new(LimitValues::default()).unwrap()
    }

    fn scope() -> Scope {
        let repository = RepositoryId::parse("github.com/owner/project").unwrap();
        Scope::new(
            repository.clone(),
            repository,
            RemoteName::parse("origin").unwrap(),
            HeadRef::parse("main").unwrap(),
            "@".to_owned(),
        )
        .unwrap()
    }

    #[test]
    fn injected_publication_failures_preserve_or_truthfully_classify_the_outcome() {
        let root =
            std::env::temp_dir().join(format!("almighty-push-state-fault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        let root = root.canonicalize().unwrap();
        let store = StateStore::open(&root, scope(), limits()).unwrap();
        let mut session = store.lock().unwrap();
        session.persist_loaded().unwrap();
        let expected = session.state().clone();
        for fault in [
            StorageFault::CreateTemporary,
            StorageFault::Write,
            StorageFault::FileSync,
            StorageFault::Rename,
        ] {
            store.inject_fault(fault);
            assert!(session.persist_loaded().is_err());
            assert_eq!(store.load().unwrap(), expected);
        }
        store.inject_fault(StorageFault::DirectorySync);
        assert!(matches!(
            session.persist_loaded(),
            Err(StateError::CommitDurabilityUnknown { .. })
        ));
        assert_eq!(store.load().unwrap(), expected);
        assert!(matches!(
            session.persist_loaded(),
            Err(StateError::ReloadRequired)
        ));
        drop(session);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn injected_legacy_deletion_failures_are_restart_closed() {
        let root =
            std::env::temp_dir().join(format!("almighty-push-legacy-fault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        std::fs::write(
            root.join(LEGACY_FILE),
            r#"{"version":2,"prs":{},"merged_prs":[],"closed_prs":[],"last_operation_id":null}"#,
        )
        .unwrap();
        let root = root.canonicalize().unwrap();
        let store = StateStore::open(&root, scope(), limits()).unwrap();
        let mut session = store.lock().unwrap();
        session.persist_loaded().unwrap();
        store.inject_fault(StorageFault::LegacyUnlink);
        assert!(session.finish_legacy_deletion().is_err());
        assert!(root.join(LEGACY_FILE).exists());
        store.inject_fault(StorageFault::LegacyDirectorySync);
        assert!(matches!(
            session.finish_legacy_deletion(),
            Err(StateError::LegacyDeletionDurabilityUnknown { .. })
        ));
        assert!(!root.join(LEGACY_FILE).exists());
        assert!(matches!(
            session.finish_legacy_deletion(),
            Err(StateError::ReloadRequired)
        ));
        drop(session);
        let mut recovered = store.lock().unwrap();
        recovered.finish_legacy_deletion().unwrap();
        drop(recovered);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn execution_occurrence_exhaustion_fails_before_installing_a_checkpoint() {
        let mut state = StateV3::empty(scope());
        state.execution_occurrence = u64::MAX;
        let plan = Plan::new(
            scope(),
            vec![Effect::Reobserve(crate::plan::ReobserveBarrier::All)].into_boxed_slice(),
            limits(),
        )
        .unwrap();
        assert!(matches!(
            state.start_plan(plan, limits()),
            Err(StateError::OccurrenceExhausted)
        ));
        assert!(matches!(state.checkpoint(), CheckpointState::Idle));
        assert_eq!(state.execution_occurrence, u64::MAX);
    }

    #[test]
    fn create_result_is_bound_before_generated_ownership_is_installed() {
        let change = ChangeId::parse("kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk").unwrap();
        let create = Effect::Github(GithubEffect::CreatePullRequest {
            change_id: change.clone(),
            head: HeadRef::owned(&change).unwrap(),
            base: HeadRef::parse("main").unwrap(),
            title: "title".to_owned(),
            managed_section: ManagedSection::new(
                scope().source_repository().clone(),
                change.clone(),
                vec![change.clone()].into_boxed_slice(),
                limits(),
            )
            .unwrap()
            .render(),
        });
        let install = Effect::Ownership(OwnershipEffect::InstallCreated {
            change_id: change.clone(),
            expected_absent: true,
            create_effect_index: 0,
            lifecycle: PrLifecycle::Open,
        });
        let plan = Plan::new(scope(), vec![create, install].into_boxed_slice(), limits()).unwrap();
        let mut state = StateV3::empty(scope());
        state.start_plan(plan, limits()).unwrap();
        assert!(state.publish_completion().is_err());
        state
            .record_current_result(SatisfiedEffect::new(
                EffectResult::CreatedPullRequest {
                    number: PrNumber::new(7).unwrap(),
                },
                SatisfactionSource::ExternalPostcondition,
            ))
            .unwrap();
        assert!(state.verified().is_empty());
        state
            .record_current_result(SatisfiedEffect::new(
                EffectResult::Satisfied,
                SatisfactionSource::OwnershipPrecondition,
            ))
            .unwrap();
        assert_eq!(
            state.verified().get(&change).unwrap().number(),
            PrNumber::new(7).unwrap()
        );
        state.publish_completion().unwrap();
        assert!(matches!(
            state.checkpoint(),
            CheckpointState::Completed(checkpoint)
                if checkpoint.next_effect_index() as usize == checkpoint.plan().effects().len()
        ));
    }

    fn store_with_state(label: &str, state: &StateV3) -> (PathBuf, StateStore) {
        for sequence in 0..1_000 {
            let root = std::env::temp_dir().join(format!(
                "almighty-push-state-machine-{label}-{}-{sequence}",
                std::process::id()
            ));
            match std::fs::create_dir(&root) {
                Ok(()) => {
                    std::fs::create_dir(root.join(".jj")).unwrap();
                    let root = root.canonicalize().unwrap();
                    let store = StateStore::open(&root, scope(), limits()).unwrap();
                    let bytes = serialize_bounded(state, limits().state_bytes_max()).unwrap();
                    std::fs::write(store.state_path(), bytes).unwrap();
                    std::fs::set_permissions(
                        store.state_path(),
                        std::fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
                    return (root, store);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("fixture creation failed: {error}"),
            }
        }
        panic!("fixture attempts exhausted")
    }

    fn sealed_legacy_state(lifecycle: PrLifecycle) -> (StateV3, ChangeId, PrNumber) {
        let change = ChangeId::parse("kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk").unwrap();
        let number = PrNumber::new(7).unwrap();
        let head = HeadRef::parse("legacy/topic").unwrap();
        let digest = [7; 32];
        let candidate = LegacyCandidate::new(
            change.as_str().to_owned(),
            number.get(),
            "https://github.com/owner/project/pull/7".to_owned(),
            head.as_str().to_owned(),
            "1111111111111111111111111111111111111111".to_owned(),
            Some(change.as_str().to_owned()),
        )
        .unwrap();
        let mut state = StateV3::empty(scope());
        state.verified.insert(
            change.clone(),
            ManagedPr::validated_legacy(change.clone(), number, head.clone(), lifecycle, digest),
        );
        state.legacy_migration = LegacyMigration::Complete {
            source_digest: LegacyDigest([9; 32]),
            resolved: vec![LegacyResolutionRecord {
                candidate,
                disposition: LegacyDisposition::Verified {
                    change_id: change.clone(),
                    number,
                    head,
                    observation_digest: digest,
                },
            }]
            .into_boxed_slice(),
        };
        (state, change, number)
    }

    #[test]
    fn sealed_legacy_provenance_survives_close_historicize_reload_and_reopen() {
        let (state, change, number) = sealed_legacy_state(PrLifecycle::Open);
        let provenance = state.legacy_resolutions().to_vec();
        let authority = state.verified().get(&change).unwrap().ownership().clone();
        let (root, store) = store_with_state("legacy-lifecycle", &state);

        let close = Plan::new(
            scope(),
            vec![
                Effect::Github(GithubEffect::SetLifecycle {
                    ownership: authority.clone(),
                    number,
                    expected: PrLifecycle::Open,
                    desired: PrLifecycle::Closed,
                }),
                Effect::Ownership(OwnershipEffect::UpdateLifecycle {
                    change_id: change.clone(),
                    expected_number: number,
                    expected: PrLifecycle::Open,
                    desired: PrLifecycle::Closed,
                }),
                Effect::Ownership(OwnershipEffect::Historicize {
                    change_id: change.clone(),
                    expected_number: number,
                    expected_lifecycle: PrLifecycle::Closed,
                }),
            ]
            .into_boxed_slice(),
            limits(),
        )
        .unwrap();
        let mut session = store.lock().unwrap();
        session.start_plan(close).unwrap();
        for source in [
            SatisfactionSource::ExternalPostcondition,
            SatisfactionSource::OwnershipPrecondition,
            SatisfactionSource::OwnershipPrecondition,
        ] {
            session
                .record_satisfied(SatisfiedEffect::new(EffectResult::Satisfied, source))
                .unwrap();
            assert_eq!(store.load().unwrap().legacy_resolutions(), provenance);
        }
        session.publish_completion().unwrap();
        let close_receipt = session.completion_receipt().unwrap();
        drop(session);
        let closed = store.load().unwrap();
        assert_eq!(closed.legacy_resolutions(), provenance);
        assert_eq!(
            closed.historic().get(&change).unwrap().ownership(),
            &authority
        );

        let reopen = Plan::new(
            scope(),
            vec![
                Effect::Ownership(OwnershipEffect::Reactivate {
                    change_id: change.clone(),
                    expected_number: number,
                    expected_lifecycle: PrLifecycle::Closed,
                }),
                Effect::Github(GithubEffect::SetLifecycle {
                    ownership: authority.clone(),
                    number,
                    expected: PrLifecycle::Closed,
                    desired: PrLifecycle::Open,
                }),
                Effect::Ownership(OwnershipEffect::UpdateLifecycle {
                    change_id: change.clone(),
                    expected_number: number,
                    expected: PrLifecycle::Closed,
                    desired: PrLifecycle::Open,
                }),
            ]
            .into_boxed_slice(),
            limits(),
        )
        .unwrap();
        let mut session = store.lock().unwrap();
        session.start_next_plan(close_receipt, reopen).unwrap();
        for source in [
            SatisfactionSource::OwnershipPrecondition,
            SatisfactionSource::ExternalPostcondition,
            SatisfactionSource::OwnershipPrecondition,
        ] {
            session
                .record_satisfied(SatisfiedEffect::new(EffectResult::Satisfied, source))
                .unwrap();
        }
        session.publish_completion().unwrap();
        drop(session);
        let reopened = store.load().unwrap();
        assert_eq!(reopened.legacy_resolutions(), provenance);
        assert_eq!(
            reopened.verified().get(&change).unwrap().lifecycle(),
            PrLifecycle::Open
        );
        assert_eq!(
            reopened.verified().get(&change).unwrap().ownership(),
            &authority
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_and_historic_share_one_exact_managed_identity_bound() {
        let configured = Limits::new(LimitValues {
            change_count_max: 4,
            ..LimitValues::default()
        })
        .unwrap();
        let mut exact = StateV3::empty(scope());
        let change_at = |index: usize| {
            let mut bytes = [b'k'; 32];
            bytes[31] += index as u8;
            ChangeId::parse(std::str::from_utf8(&bytes).unwrap()).unwrap()
        };
        for index in 0..4 {
            exact.insert_generated_fixture(
                change_at(index),
                PrNumber::new((index + 1) as u64).unwrap(),
                PrLifecycle::Closed,
                index >= 2,
            );
        }
        assert!(exact.validate(&scope(), configured).is_ok());

        let extra = change_at(4);
        exact.insert_generated_fixture(extra, PrNumber::new(5).unwrap(), PrLifecycle::Closed, true);
        assert!(matches!(
            exact.validate(&scope(), configured),
            Err(StateError::OwnershipCount { count: 5, max: 4 })
        ));
    }

    #[test]
    fn historicize_and_reactivate_move_one_exact_row_without_duplication() {
        let change = ChangeId::parse("kkkkkkkkkkkkkkkkkkkkkkkkkkkkkkkk").unwrap();
        let number = PrNumber::new(7).unwrap();
        let mut state = StateV3::empty(scope());
        state.insert_generated_fixture(change.clone(), number, PrLifecycle::Closed, false);
        let historicize = Effect::Ownership(OwnershipEffect::Historicize {
            change_id: change.clone(),
            expected_number: number,
            expected_lifecycle: PrLifecycle::Closed,
        });
        let plan = Plan::new(scope(), vec![historicize].into_boxed_slice(), limits()).unwrap();
        state.start_plan(plan, limits()).unwrap();
        state
            .record_current_result(SatisfiedEffect::new(
                EffectResult::Satisfied,
                SatisfactionSource::OwnershipPrecondition,
            ))
            .unwrap();
        state.publish_completion().unwrap();
        let historicize_receipt = state
            .completed_receipt(
                StateGeneration::Missing,
                StorageIdentity {
                    device: 0,
                    inode: 0,
                },
            )
            .unwrap();
        assert!(!state.verified().contains_key(&change));
        assert_eq!(state.historic().get(&change).unwrap().number(), number);

        let reactivate = Effect::Ownership(OwnershipEffect::Reactivate {
            change_id: change.clone(),
            expected_number: number,
            expected_lifecycle: PrLifecycle::Closed,
        });
        let plan = Plan::new(scope(), vec![reactivate].into_boxed_slice(), limits()).unwrap();
        state
            .start_next_plan(historicize_receipt, plan, limits())
            .unwrap();
        state
            .record_current_result(SatisfiedEffect::new(
                EffectResult::Satisfied,
                SatisfactionSource::OwnershipPrecondition,
            ))
            .unwrap();
        state.publish_completion().unwrap();
        assert!(state.historic().is_empty());
        assert_eq!(state.verified().get(&change).unwrap().number(), number);
    }
}
