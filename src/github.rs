use crate::body::{BodyError, BodyMerge, ManagedBody, ManagedSection};
use crate::command::{CommandError, CommandExecutor, CommandOutput, CommandSpec, SpecError};
use crate::config::ResolvedConfig;
use crate::domain::{
    ChangeId, CommitId, HeadRef, Limits, PrLifecycle, PrNumber, RepositoryId, Scope,
};
use crate::executor::{DriverId, EffectCheck, EffectRequest, PreparedEffect};
use crate::plan::{BodyHash, Effect, EffectResult, GithubEffect, JjEffect, PrOwnership};
use crate::state::{
    LegacyCandidate, LegacyCandidateResolution, LegacyRejection, LegacyRejectionReason,
    LegacyValidation, StateV3,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt::{self, Display, Formatter};
use std::path::PathBuf;

const REPOSITORY_PROJECTION: &str = "{full_name,default_branch}";
const REF_PROJECTION: &str = "[.[] | {ref,object:{type:.object.type,sha:.object.sha}}]";
const COMPACT_PR_PROJECTION: &str = "[.[] | {number,state,merged_at,title,head_ref:.head.ref,head_sha:.head.sha,head_repository:.head.repo.full_name,base_ref:.base.ref,base_repository:.base.repo.full_name}]";
const FULL_PR_PROJECTION: &str = "{number,state,merged_at,title,head_ref:.head.ref,head_sha:.head.sha,head_repository:.head.repo.full_name,base_ref:.base.ref,base_repository:.base.repo.full_name,body}";
const TITLE_BYTES_MAX: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GithubQuery {
    SourceRepository,
    TargetRepository,
    OwnedRefs,
    PullRequests,
    PullRequestDetail,
    MutationResponse,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedPr {
    number: PrNumber,
    lifecycle: PrLifecycle,
    head_repository: RepositoryId,
    head_ref: HeadRef,
    base_ref: HeadRef,
    title: String,
    body: String,
    head_commit: CommitId,
}

#[cfg(test)]
pub(crate) struct ObservedPrFixture {
    pub(crate) number: PrNumber,
    pub(crate) lifecycle: PrLifecycle,
    pub(crate) head_repository: RepositoryId,
    pub(crate) head_ref: HeadRef,
    pub(crate) base_ref: HeadRef,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) head_commit: CommitId,
}

impl ObservedPr {
    #[cfg(test)]
    pub(crate) fn fixture(fixture: ObservedPrFixture) -> Self {
        Self {
            number: fixture.number,
            lifecycle: fixture.lifecycle,
            head_repository: fixture.head_repository,
            head_ref: fixture.head_ref,
            base_ref: fixture.base_ref,
            title: fixture.title,
            body: fixture.body,
            head_commit: fixture.head_commit,
        }
    }

    pub fn number(&self) -> PrNumber {
        self.number
    }

    pub fn lifecycle(&self) -> PrLifecycle {
        self.lifecycle
    }

    pub fn head_repository(&self) -> &RepositoryId {
        &self.head_repository
    }

    pub fn head_ref(&self) -> &HeadRef {
        &self.head_ref
    }

    pub fn base_ref(&self) -> &HeadRef {
        &self.base_ref
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn body(&self) -> &str {
        &self.body
    }

    pub fn head_commit(&self) -> &CommitId {
        &self.head_commit
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GithubSnapshot {
    source_repository: RepositoryId,
    target_repository: RepositoryId,
    target_default_base: HeadRef,
    configured_base: HeadRef,
    owned_heads: BTreeMap<HeadRef, CommitId>,
    prs: Box<[ObservedPr]>,
}

impl GithubSnapshot {
    #[cfg(test)]
    pub(crate) fn fixture(
        source_repository: RepositoryId,
        target_repository: RepositoryId,
        target_default_base: HeadRef,
        configured_base: HeadRef,
        owned_heads: BTreeMap<HeadRef, CommitId>,
        prs: Box<[ObservedPr]>,
    ) -> Self {
        Self {
            source_repository,
            target_repository,
            target_default_base,
            configured_base,
            owned_heads,
            prs,
        }
    }

    pub fn source_repository(&self) -> &RepositoryId {
        &self.source_repository
    }

    pub fn target_repository(&self) -> &RepositoryId {
        &self.target_repository
    }

    pub fn target_default_base(&self) -> &HeadRef {
        &self.target_default_base
    }

    pub fn configured_base(&self) -> &HeadRef {
        &self.configured_base
    }

    pub fn owned_heads(&self) -> &BTreeMap<HeadRef, CommitId> {
        &self.owned_heads
    }

    pub fn prs(&self) -> &[ObservedPr] {
        &self.prs
    }

    /// Returns an in-memory observation used only to continue a dry-run after
    /// symbolically applying exact owned-head publications.
    pub fn with_preview_owned_heads(
        &self,
        owned_heads: BTreeMap<HeadRef, CommitId>,
        limits: Limits,
    ) -> Result<Self, GithubError> {
        if owned_heads.len() > limits.change_count_max().saturating_mul(2) {
            return Err(GithubError::Bound {
                query: GithubQuery::OwnedRefs,
                count: owned_heads.len(),
                max: limits.change_count_max().saturating_mul(2),
            });
        }
        Ok(Self {
            source_repository: self.source_repository.clone(),
            target_repository: self.target_repository.clone(),
            target_default_base: self.target_default_base.clone(),
            configured_base: self.configured_base.clone(),
            owned_heads,
            prs: self.prs.clone(),
        })
    }
}

pub struct GithubClient<'a, E> {
    executor: &'a E,
    program: PathBuf,
    workspace_root: PathBuf,
    scope: Scope,
    limits: Limits,
    environment: Box<[(OsString, OsString)]>,
    validated_legacy_ownership: BTreeSet<PrOwnership>,
}

impl<'a, E: CommandExecutor> GithubClient<'a, E> {
    pub fn new(
        executor: &'a E,
        program: PathBuf,
        config: &ResolvedConfig,
        ownership_state: &StateV3,
        environment: Box<[(OsString, OsString)]>,
    ) -> Self {
        assert_eq!(config.scope(), ownership_state.scope());
        let validated_legacy_ownership = ownership_state
            .validated_legacy_ownership()
            .cloned()
            .collect();
        Self {
            executor,
            program,
            workspace_root: config.workspace_root().to_owned(),
            scope: config.scope().clone(),
            limits: config.limits(),
            environment,
            validated_legacy_ownership,
        }
    }

    pub(crate) fn scope(&self) -> &Scope {
        &self.scope
    }

    pub(crate) fn workspace_root(&self) -> &std::path::Path {
        &self.workspace_root
    }

    pub fn observe_effect_precondition(&self, effect: &Effect) -> Result<bool, GithubError> {
        self.precondition_holds_effect(effect)
    }

    pub fn observe_effect_postcondition(
        &self,
        effect: &Effect,
    ) -> Result<Option<EffectResult>, GithubError> {
        self.observe_effect_postcondition_inner(effect)
    }

    pub(crate) fn check_current_effect(
        &self,
        request: &EffectRequest,
        effect: &Effect,
    ) -> Result<EffectCheck, GithubError> {
        if let Some(result) = self.observe_effect_postcondition_inner(effect)? {
            return Ok(EffectCheck::satisfied(request, effect, result));
        }
        if self.precondition_holds_effect(effect)? {
            Ok(EffectCheck::prepared(request, effect))
        } else {
            Ok(EffectCheck::drift(request, effect))
        }
    }

    pub(crate) fn execute_checked_effect(
        &self,
        driver_id: DriverId,
        request: &EffectRequest,
        expected: &Effect,
        prepared: PreparedEffect,
    ) -> Result<(), GithubError> {
        let effect = prepared
            .into_effect(request, expected, driver_id)
            .map_err(|_| GithubError::AuthorityMismatch)?;
        self.execute_effect(&effect)
    }

    pub fn observe(&self) -> Result<GithubSnapshot, GithubError> {
        let source = self.observe_repository(
            self.scope.source_repository(),
            GithubQuery::SourceRepository,
        )?;
        let target = self.observe_repository(
            self.scope.target_repository(),
            GithubQuery::TargetRepository,
        )?;
        let owned_heads = self.observe_owned_refs()?;
        let prs = self.observe_managed_prs()?;
        Ok(GithubSnapshot {
            source_repository: source.repository,
            target_repository: target.repository,
            target_default_base: target.default_base,
            configured_base: self.scope.base().clone(),
            owned_heads,
            prs,
        })
    }

    pub fn observe_pr(&self, number: PrNumber) -> Result<ObservedPr, GithubError> {
        self.observe_pr_detail(number).map(|detail| detail.pr)
    }

    pub fn resolve_legacy_candidate(
        &self,
        candidate: &LegacyCandidate,
    ) -> Result<LegacyCandidateResolution, GithubError> {
        let number =
            PrNumber::new(candidate.pr_number()).map_err(|_| GithubError::LegacyMismatch)?;
        let observed = self.observe_pr(number)?;
        resolve_legacy(&self.scope, candidate, &observed)
    }

    fn observe_repository(
        &self,
        repository: &RepositoryId,
        query: GithubQuery,
    ) -> Result<RepositoryObservation, GithubError> {
        let endpoint = format!("repos/{}/{}", repository.owner(), repository.name());
        let output = self.get(repository.host(), endpoint, REPOSITORY_PROJECTION)?;
        let wire: RepositoryWire = parse_json(&output.stdout, query)?;
        let observed = repository_from_full_name(repository.host(), &wire.full_name, query)?;
        if &observed != repository {
            return Err(GithubError::ScopeMismatch { query });
        }
        let default_base = HeadRef::parse(
            wire.default_branch
                .0
                .ok_or(GithubError::Malformed { query })?,
        )
        .map_err(|_| GithubError::Malformed { query })?;
        Ok(RepositoryObservation {
            repository: observed,
            default_base,
        })
    }

    fn observe_owned_refs(&self) -> Result<BTreeMap<HeadRef, CommitId>, GithubError> {
        let repository = self.scope.source_repository();
        let mut rows = BTreeMap::new();
        let page_size = self.limits.github_page_size();
        let page_count_max = self.limits.github_page_count_max();
        for page in 1..=page_count_max {
            let endpoint = format!(
                "repos/{}/{}/git/matching-refs/heads/almighty-push%2F?per_page={page_size}&page={page}",
                repository.owner(),
                repository.name()
            );
            let output = self.get(repository.host(), endpoint, REF_PROJECTION)?;
            let page_rows: Vec<RefWire> = parse_json(&output.stdout, GithubQuery::OwnedRefs)?;
            validate_page_len(GithubQuery::OwnedRefs, page, page_rows.len(), self.limits)?;
            for wire in &page_rows {
                if wire.object.kind != "commit" {
                    return Err(GithubError::Malformed {
                        query: GithubQuery::OwnedRefs,
                    });
                }
                let name = wire
                    .name
                    .strip_prefix("refs/heads/")
                    .ok_or(GithubError::Malformed {
                        query: GithubQuery::OwnedRefs,
                    })?;
                let head = HeadRef::parse(name).map_err(|_| GithubError::Malformed {
                    query: GithubQuery::OwnedRefs,
                })?;
                let change = parse_owned_head(&head).ok_or(GithubError::Malformed {
                    query: GithubQuery::OwnedRefs,
                })?;
                if HeadRef::owned(&change).map_err(|_| GithubError::Malformed {
                    query: GithubQuery::OwnedRefs,
                })? != head
                {
                    return Err(GithubError::Malformed {
                        query: GithubQuery::OwnedRefs,
                    });
                }
                let commit =
                    CommitId::parse(&wire.object.sha).map_err(|_| GithubError::Malformed {
                        query: GithubQuery::OwnedRefs,
                    })?;
                if rows.insert(head, commit).is_some() {
                    return Err(GithubError::Duplicate {
                        query: GithubQuery::OwnedRefs,
                    });
                }
                if rows.len() > self.limits.change_count_max() {
                    return Err(GithubError::Bound {
                        query: GithubQuery::OwnedRefs,
                        count: rows.len(),
                        max: self.limits.change_count_max(),
                    });
                }
            }
            if page_rows.len() < page_size {
                break;
            }
        }
        for ownership in &self.validated_legacy_ownership {
            if !ownership.is_validated_legacy() || rows.contains_key(ownership.head()) {
                continue;
            }
            if let Some(commit) = self.observe_exact_ref(ownership.head())? {
                rows.insert(ownership.head().clone(), commit);
            }
        }
        let count_max = self.limits.change_count_max().saturating_mul(2);
        if rows.len() > count_max {
            return Err(GithubError::Bound {
                query: GithubQuery::OwnedRefs,
                count: rows.len(),
                max: count_max,
            });
        }
        Ok(rows)
    }

    fn observe_exact_ref(&self, expected_head: &HeadRef) -> Result<Option<CommitId>, GithubError> {
        let repository = self.scope.source_repository();
        let page_size = self.limits.github_page_size();
        let page_count_max = self.limits.github_page_count_max();
        let row_count_max = page_size
            .checked_mul(page_count_max)
            .expect("validated GitHub page bounds fit usize");
        let encoded_head = percent_encode_path_segment(expected_head.as_str());
        let mut observed = None;
        let mut names = BTreeSet::new();
        let mut row_count = 0usize;
        for page in 1..=page_count_max {
            let endpoint = format!(
                "repos/{}/{}/git/matching-refs/heads/{encoded_head}?per_page={page_size}&page={page}",
                repository.owner(),
                repository.name()
            );
            let output = self.get(repository.host(), endpoint, REF_PROJECTION)?;
            let page_rows: Vec<RefWire> = parse_json(&output.stdout, GithubQuery::OwnedRefs)?;
            validate_page_len(GithubQuery::OwnedRefs, page, page_rows.len(), self.limits)?;
            for wire in &page_rows {
                row_count = row_count.checked_add(1).ok_or(GithubError::Bound {
                    query: GithubQuery::OwnedRefs,
                    count: usize::MAX,
                    max: row_count_max,
                })?;
                if row_count > row_count_max {
                    return Err(GithubError::Bound {
                        query: GithubQuery::OwnedRefs,
                        count: row_count,
                        max: row_count_max,
                    });
                }
                if wire.object.kind != "commit" {
                    return Err(GithubError::Malformed {
                        query: GithubQuery::OwnedRefs,
                    });
                }
                let name = wire
                    .name
                    .strip_prefix("refs/heads/")
                    .ok_or(GithubError::Malformed {
                        query: GithubQuery::OwnedRefs,
                    })?;
                let head = HeadRef::parse(name).map_err(|_| GithubError::Malformed {
                    query: GithubQuery::OwnedRefs,
                })?;
                if !head.as_str().starts_with(expected_head.as_str()) {
                    return Err(GithubError::Malformed {
                        query: GithubQuery::OwnedRefs,
                    });
                }
                if !names.insert(head.clone()) {
                    return Err(GithubError::Duplicate {
                        query: GithubQuery::OwnedRefs,
                    });
                }
                let commit =
                    CommitId::parse(&wire.object.sha).map_err(|_| GithubError::Malformed {
                        query: GithubQuery::OwnedRefs,
                    })?;
                if &head == expected_head {
                    observed = Some(commit);
                }
            }
            if page_rows.len() < page_size {
                return Ok(observed);
            }
        }
        unreachable!("full final pages return ObservationIncomplete")
    }

    fn observe_compact_prs(&self) -> Result<Vec<CompactPr>, GithubError> {
        let repository = self.scope.target_repository();
        let page_size = self.limits.github_page_size();
        let page_count_max = self.limits.github_page_count_max();
        let row_count_max = page_size
            .checked_mul(page_count_max)
            .expect("validated GitHub page bounds fit usize");
        let mut rows = Vec::with_capacity(row_count_max.min(1_000));
        let mut numbers = BTreeSet::new();
        for page in 1..=page_count_max {
            let endpoint = format!(
                "repos/{}/{}/pulls?state=all&sort=created&direction=asc&per_page={page_size}&page={page}",
                repository.owner(),
                repository.name()
            );
            let output = self.get(repository.host(), endpoint, COMPACT_PR_PROJECTION)?;
            let page_rows: Vec<CompactPrWire> =
                parse_json(&output.stdout, GithubQuery::PullRequests)?;
            validate_page_len(
                GithubQuery::PullRequests,
                page,
                page_rows.len(),
                self.limits,
            )?;
            for wire in &page_rows {
                let row = parse_compact_pr(wire, &self.scope)?;
                if !numbers.insert(row.number) {
                    return Err(GithubError::Duplicate {
                        query: GithubQuery::PullRequests,
                    });
                }
                rows.push(row);
                if rows.len() > row_count_max {
                    return Err(GithubError::Bound {
                        query: GithubQuery::PullRequests,
                        count: rows.len(),
                        max: row_count_max,
                    });
                }
            }
            if page_rows.len() < page_size {
                rows.sort_by_key(|row| row.number);
                return Ok(rows);
            }
        }
        unreachable!("full final pages return ObservationIncomplete")
    }

    fn observe_managed_prs(&self) -> Result<Box<[ObservedPr]>, GithubError> {
        let compact = self.observe_compact_prs()?;
        let mut candidates = Vec::new();
        let mut heads = BTreeSet::new();
        let mut legacy_seen = BTreeSet::new();
        for row in compact {
            let exact_source = row.head_repository.as_ref() == Some(self.scope.source_repository());
            let generated_change = parse_owned_head(&row.head_ref);
            if row.head_ref.as_str().starts_with("almighty-push/") && row.head_repository.is_none()
            {
                return Err(GithubError::Malformed {
                    query: GithubQuery::PullRequests,
                });
            }
            if generated_change.is_some() && row.head_repository.is_some() && !exact_source {
                return Err(GithubError::ScopeMismatch {
                    query: GithubQuery::PullRequests,
                });
            }
            let legacy = self.validated_legacy_ownership.iter().find(|ownership| {
                ownership
                    .legacy_evidence()
                    .is_some_and(|(number, _)| number == row.number)
                    && ownership.head() == &row.head_ref
            });
            let ownership = match (exact_source, generated_change, legacy) {
                (true, Some(change_id), None) => PrOwnership::generated(change_id)
                    .map_err(|_| GithubError::OwnershipAmbiguous)?,
                (true, None, Some(ownership)) => {
                    legacy_seen.insert(ownership.clone());
                    ownership.clone()
                }
                (true, Some(_), Some(_)) => return Err(GithubError::OwnershipAmbiguous),
                (false, _, Some(_)) => {
                    return Err(GithubError::ScopeMismatch {
                        query: GithubQuery::PullRequests,
                    });
                }
                _ => continue,
            };
            if row.base_repository != *self.scope.target_repository() {
                return Err(GithubError::ScopeMismatch {
                    query: GithubQuery::PullRequests,
                });
            }
            if !heads.insert(row.head_ref.clone()) {
                return Err(GithubError::Duplicate {
                    query: GithubQuery::PullRequests,
                });
            }
            candidates.push((row, ownership));
        }
        if legacy_seen.len()
            != self
                .validated_legacy_ownership
                .iter()
                .filter(|ownership| ownership.is_validated_legacy())
                .count()
        {
            return Err(GithubError::OwnershipMissing);
        }
        if candidates.len() > self.limits.change_count_max() {
            return Err(GithubError::Bound {
                query: GithubQuery::PullRequestDetail,
                count: candidates.len(),
                max: self.limits.change_count_max(),
            });
        }

        let mut observed = Vec::with_capacity(candidates.len());
        let mut identities = BTreeSet::new();
        for (compact, ownership) in candidates {
            let detail = self.observe_pr_detail(compact.number)?;
            if !detail.matches_compact(&compact) {
                return Err(GithubError::ObservationChanged {
                    number: compact.number,
                });
            }
            if !self.ownership_holds(&detail.pr, &ownership)?
                || !identities.insert(ownership.change_id().clone())
            {
                return Err(GithubError::OwnershipAmbiguous);
            }
            observed.push(detail.pr);
        }
        observed.sort_by_key(ObservedPr::number);
        Ok(observed.into_boxed_slice())
    }

    fn observe_pr_detail(&self, number: PrNumber) -> Result<PrDetail, GithubError> {
        let repository = self.scope.target_repository();
        let endpoint = format!(
            "repos/{}/{}/pulls/{}",
            repository.owner(),
            repository.name(),
            number.get()
        );
        let output = self.get(repository.host(), endpoint, FULL_PR_PROJECTION)?;
        let wire: FullPrWire = parse_json(&output.stdout, GithubQuery::PullRequestDetail)?;
        let detail = parse_full_pr(&wire, &self.scope, self.limits)?;
        if detail.pr.number() != number {
            return Err(GithubError::ObservationChanged { number });
        }
        Ok(detail)
    }

    fn get(
        &self,
        host: &str,
        endpoint: String,
        projection: &str,
    ) -> Result<CommandOutput, GithubError> {
        self.run(
            vec![
                "api".into(),
                "--hostname".into(),
                host.into(),
                "--method".into(),
                "GET".into(),
                endpoint.into(),
                "--jq".into(),
                projection.into(),
            ]
            .into_boxed_slice(),
            Box::new([]),
        )
    }

    fn mutate(
        &self,
        repository: &RepositoryId,
        method: &str,
        endpoint: String,
        payload: serde_json::Value,
        response_required: bool,
    ) -> Result<(), GithubError> {
        let stdin = serde_json::to_vec(&payload).map_err(|_| GithubError::Malformed {
            query: GithubQuery::MutationResponse,
        })?;
        let output = self.run(
            vec![
                "api".into(),
                "--hostname".into(),
                repository.host().into(),
                "--method".into(),
                method.into(),
                endpoint.into(),
                "--input".into(),
                "-".into(),
            ]
            .into_boxed_slice(),
            stdin.into_boxed_slice(),
        )?;
        if response_required || !output.stdout.trim().is_empty() {
            let response: serde_json::Value =
                parse_json(&output.stdout, GithubQuery::MutationResponse)?;
            if !response.is_object() {
                return Err(GithubError::Malformed {
                    query: GithubQuery::MutationResponse,
                });
            }
        }
        Ok(())
    }

    fn run(&self, args: Box<[OsString]>, stdin: Box<[u8]>) -> Result<CommandOutput, GithubError> {
        let spec = CommandSpec::new(
            self.program.as_os_str().to_owned(),
            args,
            self.workspace_root.clone(),
            stdin,
            self.environment.clone(),
            self.limits,
        )
        .map_err(|error| GithubError::Spec(Box::new(error)))?;
        let output = self
            .executor
            .run(&spec)
            .map_err(|error| GithubError::Command(Box::new(error)))?;
        let bytes = output.stdout.len().saturating_add(output.stderr.len());
        if bytes > self.limits.command_output_bytes_max() {
            return Err(GithubError::Bound {
                query: GithubQuery::MutationResponse,
                count: bytes,
                max: self.limits.command_output_bytes_max(),
            });
        }
        Ok(output)
    }

    fn observe_effect_pr(
        &self,
        ownership: &PrOwnership,
        number: PrNumber,
    ) -> Result<ObservedPr, GithubError> {
        let pr = self.observe_pr(number)?;
        if !self.ownership_holds(&pr, ownership)? {
            return Err(GithubError::OwnershipMissing);
        }
        Ok(pr)
    }

    fn ownership_holds(
        &self,
        pr: &ObservedPr,
        ownership: &PrOwnership,
    ) -> Result<bool, GithubError> {
        if pr.head_repository() != self.scope.source_repository()
            || pr.head_ref() != ownership.head()
        {
            return Ok(false);
        }
        let Some((number, _observation_digest)) = ownership.legacy_evidence() else {
            let section =
                ManagedBody::section(pr.body(), self.limits).map_err(GithubError::Body)?;
            return Ok(section.is_some_and(|section| {
                section.source_repository() == self.scope.source_repository()
                    && section.current_change() == ownership.change_id()
                    && HeadRef::owned(ownership.change_id())
                        .is_ok_and(|head| &head == ownership.head())
            }));
        };
        if number != pr.number() || !self.validated_legacy_ownership.contains(ownership) {
            return Ok(false);
        }
        let section = ManagedBody::section(pr.body(), self.limits).map_err(GithubError::Body)?;
        if let Some(section) = section {
            return Ok(
                section.source_repository() == self.scope.source_repository()
                    && section.current_change() == ownership.change_id(),
            );
        }
        let marker = legacy_marker(ownership.change_id());
        let count = pr.body().lines().filter(|line| *line == marker).count();
        if count > 1 {
            return Err(GithubError::LegacyAmbiguous);
        }
        Ok(count == 1)
    }

    fn find_create_postcondition(
        &self,
        change_id: &ChangeId,
        head: &HeadRef,
        base: &HeadRef,
        title: &str,
        managed_section: &str,
    ) -> Result<Option<PrNumber>, GithubError> {
        let snapshot = self.observe()?;
        if !snapshot.owned_heads().contains_key(head) {
            return Ok(None);
        }
        let matches = snapshot
            .prs()
            .iter()
            .filter(|pr| {
                pr.head_repository() == self.scope.source_repository() && pr.head_ref() == head
            })
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            return Err(GithubError::OwnershipAmbiguous);
        }
        let Some(pr) = matches.first() else {
            return Ok(None);
        };
        let section =
            ManagedSection::parse(managed_section, self.limits).map_err(GithubError::Body)?;
        let desired_body = merged_body("", &section, self.limits.body_bytes_max())?;
        let satisfied = pr.lifecycle() == PrLifecycle::Open
            && pr.base_ref() == base
            && pr.title() == title
            && pr.body() == desired_body
            && self.ownership_holds(
                pr,
                &PrOwnership::generated(change_id.clone())
                    .map_err(|_| GithubError::Precondition)?,
            )?;
        Ok(satisfied.then_some(pr.number()))
    }

    fn create_precondition(
        &self,
        change_id: &ChangeId,
        head: &HeadRef,
    ) -> Result<bool, GithubError> {
        let snapshot = self.observe()?;
        if !snapshot.owned_heads().contains_key(head) {
            return Ok(false);
        }
        for pr in snapshot.prs() {
            if pr.head_repository() == self.scope.source_repository() && pr.head_ref() == head {
                return Ok(false);
            }
            let section = ManagedBody::section(pr.body(), self.limits)
                .map_err(GithubError::Body)?
                .ok_or(GithubError::OwnershipMissing)?;
            if section.current_change() == change_id {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn update_body_desired(
        &self,
        pr: &ObservedPr,
        managed_section: &str,
        desired_hash: BodyHash,
    ) -> Result<String, GithubError> {
        let section =
            ManagedSection::parse(managed_section, self.limits).map_err(GithubError::Body)?;
        let body = merged_body(pr.body(), &section, self.limits.body_bytes_max())?;
        if BodyHash::of(body.as_bytes()) != desired_hash {
            return Err(GithubError::Precondition);
        }
        Ok(body)
    }

    fn execute_effect(&self, effect: &Effect) -> Result<(), GithubError> {
        self.validate_effect_boundary(effect)?;
        if !self.precondition_holds_effect(effect)? {
            return Err(GithubError::Precondition);
        }
        match effect {
            Effect::Github(GithubEffect::CreatePullRequest {
                head,
                base,
                title,
                managed_section,
                ..
            }) => {
                let section = ManagedSection::parse(managed_section, self.limits)
                    .map_err(GithubError::Body)?;
                let body = merged_body("", &section, self.limits.body_bytes_max())?;
                let repository = self.scope.target_repository();
                let endpoint = format!("repos/{}/{}/pulls", repository.owner(), repository.name());
                self.mutate(
                    repository,
                    "POST",
                    endpoint,
                    serde_json::json!({
                        "title": title,
                        "head": format!("{}:{}", self.scope.source_repository().owner(), head),
                        "base": base.as_str(),
                        "body": body,
                    }),
                    true,
                )
            }
            Effect::Github(GithubEffect::UpdateBase {
                number, desired, ..
            }) => {
                let repository = self.scope.target_repository();
                let endpoint = format!(
                    "repos/{}/{}/pulls/{}",
                    repository.owner(),
                    repository.name(),
                    number.get()
                );
                self.mutate(
                    repository,
                    "PATCH",
                    endpoint,
                    serde_json::json!({ "base": desired.as_str() }),
                    true,
                )
            }
            Effect::Github(GithubEffect::UpdateBody {
                ownership,
                number,
                desired_hash,
                managed_section,
                ..
            }) => {
                let pr = self.observe_effect_pr(ownership, *number)?;
                let body = self.update_body_desired(&pr, managed_section, *desired_hash)?;
                let repository = self.scope.target_repository();
                let endpoint = format!(
                    "repos/{}/{}/pulls/{}",
                    repository.owner(),
                    repository.name(),
                    number.get()
                );
                self.mutate(
                    repository,
                    "PATCH",
                    endpoint,
                    serde_json::json!({ "body": body }),
                    true,
                )
            }
            Effect::Github(GithubEffect::SetLifecycle {
                number, desired, ..
            }) => {
                let state = match desired {
                    PrLifecycle::Open => "open",
                    PrLifecycle::Closed => "closed",
                    PrLifecycle::Merged => return Err(GithubError::UnsupportedEffect),
                };
                let repository = self.scope.target_repository();
                let endpoint = format!(
                    "repos/{}/{}/pulls/{}",
                    repository.owner(),
                    repository.name(),
                    number.get()
                );
                self.mutate(
                    repository,
                    "PATCH",
                    endpoint,
                    serde_json::json!({ "state": state }),
                    true,
                )
            }
            Effect::Jj(JjEffect::DeleteHead { ownership, .. }) => {
                let head = ownership.head();
                let repository = self.scope.source_repository();
                let endpoint = format!(
                    "repos/{}/{}/git/refs/heads/{}",
                    repository.owner(),
                    repository.name(),
                    percent_encode_path_segment(head.as_str())
                );
                self.mutate(repository, "DELETE", endpoint, serde_json::json!({}), false)
            }
            _ => Err(GithubError::UnsupportedEffect),
        }
    }

    fn precondition_holds_effect(&self, effect: &Effect) -> Result<bool, GithubError> {
        self.validate_effect_boundary(effect)?;
        match effect {
            Effect::Github(GithubEffect::CreatePullRequest {
                change_id, head, ..
            }) => self.create_precondition(change_id, head),
            Effect::Github(GithubEffect::UpdateBase {
                ownership,
                number,
                expected,
                ..
            }) => self
                .observe_effect_pr(ownership, *number)
                .map(|pr| pr.base_ref() == expected),
            Effect::Github(GithubEffect::UpdateBody {
                ownership,
                number,
                expected_hash,
                ..
            }) => self
                .observe_effect_pr(ownership, *number)
                .map(|pr| BodyHash::of(pr.body().as_bytes()) == *expected_hash),
            Effect::Github(GithubEffect::SetLifecycle {
                ownership,
                number,
                expected,
                ..
            }) => self
                .observe_effect_pr(ownership, *number)
                .map(|pr| pr.lifecycle() == *expected),
            Effect::Jj(JjEffect::DeleteHead {
                ownership,
                expected,
            }) => self
                .observe_exact_ref(ownership.head())
                .map(|observed| observed.as_ref() == Some(expected)),
            _ => Err(GithubError::UnsupportedEffect),
        }
    }

    fn observe_effect_postcondition_inner(
        &self,
        effect: &Effect,
    ) -> Result<Option<EffectResult>, GithubError> {
        self.validate_effect_boundary(effect)?;
        match effect {
            Effect::Github(GithubEffect::CreatePullRequest {
                change_id,
                head,
                base,
                title,
                managed_section,
            }) => self
                .find_create_postcondition(change_id, head, base, title, managed_section)
                .map(|number| number.map(|number| EffectResult::CreatedPullRequest { number })),
            Effect::Github(GithubEffect::UpdateBase {
                ownership,
                number,
                desired,
                ..
            }) => self
                .observe_effect_pr(ownership, *number)
                .map(|pr| (pr.base_ref() == desired).then_some(EffectResult::Satisfied)),
            Effect::Github(GithubEffect::UpdateBody {
                ownership,
                number,
                desired_hash,
                ..
            }) => self.observe_effect_pr(ownership, *number).map(|pr| {
                (BodyHash::of(pr.body().as_bytes()) == *desired_hash)
                    .then_some(EffectResult::Satisfied)
            }),
            Effect::Github(GithubEffect::SetLifecycle {
                ownership,
                number,
                desired,
                ..
            }) => self
                .observe_effect_pr(ownership, *number)
                .map(|pr| (pr.lifecycle() == *desired).then_some(EffectResult::Satisfied)),
            Effect::Jj(JjEffect::DeleteHead { ownership, .. }) => self
                .observe_exact_ref(ownership.head())
                .map(|observed| observed.is_none().then_some(EffectResult::Satisfied)),
            _ => Err(GithubError::UnsupportedEffect),
        }
    }

    fn validate_effect_boundary(&self, effect: &Effect) -> Result<(), GithubError> {
        match effect {
            Effect::Github(GithubEffect::CreatePullRequest {
                change_id,
                head,
                title,
                managed_section,
                ..
            }) => {
                if HeadRef::owned(change_id).map_err(|_| GithubError::Precondition)? != *head
                    || title.is_empty()
                    || title.len() > TITLE_BYTES_MAX
                    || title.contains('\0')
                {
                    return Err(GithubError::Precondition);
                }
                self.validate_managed_section(change_id, managed_section)
            }
            Effect::Github(GithubEffect::UpdateBase {
                ownership,
                number,
                expected,
                desired,
            }) if expected != desired => self.validate_pr_ownership(ownership, *number),
            Effect::Github(GithubEffect::UpdateBody {
                ownership,
                number,
                expected_hash,
                desired_hash,
                managed_section,
            }) if expected_hash != desired_hash => {
                self.validate_pr_ownership(ownership, *number)?;
                self.validate_managed_section(ownership.change_id(), managed_section)
            }
            Effect::Github(GithubEffect::SetLifecycle {
                ownership,
                number,
                expected,
                desired,
            }) if matches!(
                (expected, desired),
                (PrLifecycle::Open, PrLifecycle::Closed) | (PrLifecycle::Closed, PrLifecycle::Open)
            ) =>
            {
                self.validate_pr_ownership(ownership, *number)
            }
            Effect::Jj(JjEffect::DeleteHead { ownership, .. }) => {
                self.validate_head_ownership(ownership)
            }
            Effect::Github(_) => Err(GithubError::Precondition),
            _ => Err(GithubError::UnsupportedEffect),
        }
    }

    fn validate_pr_ownership(
        &self,
        ownership: &PrOwnership,
        number: PrNumber,
    ) -> Result<(), GithubError> {
        self.validate_head_ownership(ownership)?;
        if ownership
            .legacy_evidence()
            .is_some_and(|(evidence_number, _)| evidence_number != number)
        {
            return Err(GithubError::Precondition);
        }
        Ok(())
    }

    fn validate_head_ownership(&self, ownership: &PrOwnership) -> Result<(), GithubError> {
        if ownership.legacy_evidence().is_some() {
            if self.validated_legacy_ownership.contains(ownership) {
                return Ok(());
            }
            return Err(GithubError::Precondition);
        }
        if !HeadRef::owned(ownership.change_id()).is_ok_and(|head| &head == ownership.head()) {
            return Err(GithubError::Precondition);
        }
        Ok(())
    }

    fn validate_managed_section(
        &self,
        change_id: &ChangeId,
        managed_section: &str,
    ) -> Result<(), GithubError> {
        let section =
            ManagedSection::parse(managed_section, self.limits).map_err(GithubError::Body)?;
        if section.source_repository() != self.scope.source_repository()
            || section.current_change() != change_id
        {
            return Err(GithubError::Precondition);
        }
        Ok(())
    }
}

pub enum GithubError {
    Spec(Box<SpecError>),
    Command(Box<CommandError>),
    Body(BodyError),
    Malformed {
        query: GithubQuery,
    },
    Bound {
        query: GithubQuery,
        count: usize,
        max: usize,
    },
    ScopeMismatch {
        query: GithubQuery,
    },
    Duplicate {
        query: GithubQuery,
    },
    ObservationIncomplete {
        query: GithubQuery,
        page: usize,
        per_page: usize,
    },
    ObservationChanged {
        number: PrNumber,
    },
    OwnershipMissing,
    OwnershipAmbiguous,
    LegacyMismatch,
    LegacyAmbiguous,
    Precondition,
    AuthorityMismatch,
    UnsupportedEffect,
}

impl Display for GithubError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spec(error) => Display::fmt(error, formatter),
            Self::Command(error) => Display::fmt(error, formatter),
            Self::Body(error) => Display::fmt(error, formatter),
            Self::Malformed { query } => {
                write!(formatter, "GitHub {query:?} response is malformed")
            }
            Self::Bound { query, count, max } => {
                write!(
                    formatter,
                    "GitHub {query:?} returned {count}, exceeding {max}"
                )
            }
            Self::ScopeMismatch { query } => {
                write!(
                    formatter,
                    "GitHub {query:?} response is outside configured scope"
                )
            }
            Self::Duplicate { query } => {
                write!(formatter, "GitHub {query:?} identity is duplicated")
            }
            Self::ObservationIncomplete {
                query,
                page,
                per_page,
            } => write!(
                formatter,
                "GitHub {query:?} page {page} was full at {per_page}; observation is incomplete"
            ),
            Self::ObservationChanged { number } => {
                write!(
                    formatter,
                    "pull request {number} changed during observation"
                )
            }
            Self::OwnershipMissing => {
                formatter.write_str("exact current GitHub ownership is missing")
            }
            Self::OwnershipAmbiguous => formatter.write_str("GitHub ownership is ambiguous"),
            Self::LegacyMismatch => {
                formatter.write_str("legacy candidate decisively mismatches GitHub")
            }
            Self::LegacyAmbiguous => formatter.write_str("legacy ownership marker is ambiguous"),
            Self::Precondition => formatter.write_str("GitHub effect precondition does not hold"),
            Self::AuthorityMismatch => {
                formatter.write_str("GitHub prepared authority does not match this exact effect")
            }
            Self::UnsupportedEffect => {
                formatter.write_str("GitHub adapter does not execute this effect")
            }
        }
    }
}

impl fmt::Debug for GithubError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl Error for GithubError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Spec(error) => Some(error.as_ref()),
            Self::Command(error) => Some(error.as_ref()),
            Self::Body(error) => Some(error),
            _ => None,
        }
    }
}

struct RepositoryObservation {
    repository: RepositoryId,
    default_base: HeadRef,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryWire {
    full_name: String,
    default_branch: RequiredNullableString,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefWire {
    #[serde(rename = "ref")]
    name: String,
    object: RefObjectWire,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefObjectWire {
    #[serde(rename = "type")]
    kind: String,
    sha: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactPrWire {
    number: u64,
    state: String,
    merged_at: RequiredNullableString,
    title: String,
    head_ref: String,
    head_sha: String,
    head_repository: RequiredNullableString,
    base_ref: String,
    base_repository: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FullPrWire {
    number: u64,
    state: String,
    merged_at: RequiredNullableString,
    title: String,
    head_ref: String,
    head_sha: String,
    head_repository: RequiredNullableString,
    base_ref: String,
    base_repository: String,
    body: String,
}

#[derive(Deserialize)]
#[serde(transparent)]
struct RequiredNullableString(Option<String>);

#[derive(Clone)]
struct CompactPr {
    number: PrNumber,
    lifecycle: PrLifecycle,
    head_repository: Option<RepositoryId>,
    head_ref: HeadRef,
    head_commit: CommitId,
    base_repository: RepositoryId,
    base_ref: HeadRef,
    title: String,
}

struct PrDetail {
    pr: ObservedPr,
    base_repository: RepositoryId,
}

impl PrDetail {
    fn matches_compact(&self, compact: &CompactPr) -> bool {
        self.pr.number == compact.number
            && self.pr.lifecycle == compact.lifecycle
            && Some(&self.pr.head_repository) == compact.head_repository.as_ref()
            && self.pr.head_ref == compact.head_ref
            && self.pr.head_commit == compact.head_commit
            && self.base_repository == compact.base_repository
            && self.pr.base_ref == compact.base_ref
            && self.pr.title == compact.title
    }
}

fn parse_compact_pr(wire: &CompactPrWire, scope: &Scope) -> Result<CompactPr, GithubError> {
    let query = GithubQuery::PullRequests;
    validate_title(&wire.title, query)?;
    if wire
        .merged_at
        .0
        .as_ref()
        .is_some_and(|value| value.len() > 64)
    {
        return Err(GithubError::Bound {
            query: GithubQuery::PullRequests,
            count: wire.merged_at.0.as_ref().map_or(0, String::len),
            max: 64,
        });
    }
    let number = PrNumber::new(wire.number).map_err(|_| GithubError::Malformed { query })?;
    let lifecycle = parse_lifecycle(&wire.state, wire.merged_at.0.as_deref(), query)?;
    let head_repository = wire
        .head_repository
        .0
        .as_deref()
        .map(|full_name| {
            repository_from_full_name(scope.source_repository().host(), full_name, query)
        })
        .transpose()?;
    let base_repository = repository_from_full_name(
        scope.target_repository().host(),
        &wire.base_repository,
        query,
    )?;
    Ok(CompactPr {
        number,
        lifecycle,
        head_repository,
        head_ref: HeadRef::parse(&wire.head_ref).map_err(|_| GithubError::Malformed { query })?,
        head_commit: CommitId::parse(&wire.head_sha)
            .map_err(|_| GithubError::Malformed { query })?,
        base_repository,
        base_ref: HeadRef::parse(&wire.base_ref).map_err(|_| GithubError::Malformed { query })?,
        title: wire.title.clone(),
    })
}

fn parse_full_pr(
    wire: &FullPrWire,
    scope: &Scope,
    limits: Limits,
) -> Result<PrDetail, GithubError> {
    let query = GithubQuery::PullRequestDetail;
    validate_title(&wire.title, query)?;
    if wire.body.len() > limits.body_bytes_max() {
        return Err(GithubError::Bound {
            query,
            count: wire.body.len(),
            max: limits.body_bytes_max(),
        });
    }
    if wire.body.contains('\0') {
        return Err(GithubError::Malformed { query });
    }
    let merged_at_bytes = wire.merged_at.0.as_ref().map_or(0, String::len);
    if merged_at_bytes > 64 {
        return Err(GithubError::Bound {
            query,
            count: merged_at_bytes,
            max: 64,
        });
    }
    let head_repository = wire
        .head_repository
        .0
        .as_deref()
        .ok_or(GithubError::Malformed { query })
        .and_then(|full_name| {
            repository_from_full_name(scope.source_repository().host(), full_name, query)
        })?;
    let base_repository = repository_from_full_name(
        scope.target_repository().host(),
        &wire.base_repository,
        query,
    )?;
    if base_repository != *scope.target_repository() {
        return Err(GithubError::ScopeMismatch { query });
    }
    Ok(PrDetail {
        pr: ObservedPr {
            number: PrNumber::new(wire.number).map_err(|_| GithubError::Malformed { query })?,
            lifecycle: parse_lifecycle(&wire.state, wire.merged_at.0.as_deref(), query)?,
            head_repository,
            head_ref: HeadRef::parse(&wire.head_ref)
                .map_err(|_| GithubError::Malformed { query })?,
            base_ref: HeadRef::parse(&wire.base_ref)
                .map_err(|_| GithubError::Malformed { query })?,
            title: wire.title.clone(),
            body: wire.body.clone(),
            head_commit: CommitId::parse(&wire.head_sha)
                .map_err(|_| GithubError::Malformed { query })?,
        },
        base_repository,
    })
}

fn parse_lifecycle(
    state: &str,
    merged_at: Option<&str>,
    query: GithubQuery,
) -> Result<PrLifecycle, GithubError> {
    if let Some(merged_at) = merged_at {
        if merged_at.is_empty() || state != "closed" {
            return Err(GithubError::Malformed { query });
        }
        return Ok(PrLifecycle::Merged);
    }
    match state {
        "open" => Ok(PrLifecycle::Open),
        "closed" => Ok(PrLifecycle::Closed),
        _ => Err(GithubError::Malformed { query }),
    }
}

fn validate_title(title: &str, query: GithubQuery) -> Result<(), GithubError> {
    if title.is_empty() || title.contains('\0') {
        return Err(GithubError::Malformed { query });
    }
    if title.len() > TITLE_BYTES_MAX {
        return Err(GithubError::Bound {
            query,
            count: title.len(),
            max: TITLE_BYTES_MAX,
        });
    }
    Ok(())
}

fn validate_page_len(
    query: GithubQuery,
    page: usize,
    count: usize,
    limits: Limits,
) -> Result<(), GithubError> {
    let per_page = limits.github_page_size();
    if count > per_page {
        return Err(GithubError::Bound {
            query,
            count,
            max: per_page,
        });
    }
    if page == limits.github_page_count_max() && count == per_page {
        return Err(GithubError::ObservationIncomplete {
            query,
            page,
            per_page,
        });
    }
    Ok(())
}

fn parse_json<T: for<'de> Deserialize<'de>>(
    output: &str,
    query: GithubQuery,
) -> Result<T, GithubError> {
    serde_json::from_str(output).map_err(|_| GithubError::Malformed { query })
}

fn parse_owned_head(head: &HeadRef) -> Option<ChangeId> {
    let change = head.as_str().strip_prefix("almighty-push/")?;
    let change = ChangeId::parse(change).ok()?;
    (HeadRef::owned(&change).ok()? == *head).then_some(change)
}

fn resolve_legacy(
    scope: &Scope,
    candidate: &LegacyCandidate,
    observed: &ObservedPr,
) -> Result<LegacyCandidateResolution, GithubError> {
    let canonical = serde_json::to_vec(&LegacyObservation {
        number: observed.number().get(),
        lifecycle: observed.lifecycle(),
        head_repository: observed.head_repository().canonical(),
        head_ref: observed.head_ref().as_str(),
        head_commit: observed.head_commit().as_str(),
        base_ref: observed.base_ref().as_str(),
        title: observed.title(),
        body: observed.body(),
    })
    .map_err(|_| GithubError::Malformed {
        query: GithubQuery::PullRequestDetail,
    })?;
    let observation_digest = Sha256::digest(canonical).into();
    let rejection = |reason| {
        Ok(LegacyCandidateResolution::Rejected(LegacyRejection {
            candidate: candidate.clone(),
            scope: scope.clone(),
            reason,
            observation_digest,
        }))
    };
    let Some(change_text) = candidate.change_id() else {
        return rejection(LegacyRejectionReason::ChangeIdentityMismatch);
    };
    let Ok(change_id) = ChangeId::parse(change_text) else {
        return rejection(LegacyRejectionReason::ChangeIdentityMismatch);
    };
    if candidate.legacy_key() != change_id.as_str()
        || observed.number().get() != candidate.pr_number()
    {
        return rejection(LegacyRejectionReason::ChangeIdentityMismatch);
    }
    let expected_url = format!(
        "https://{}/{}/{}/pull/{}",
        scope.target_repository().host(),
        scope.target_repository().owner(),
        scope.target_repository().name(),
        observed.number().get()
    );
    if candidate.pr_url() != expected_url {
        return rejection(LegacyRejectionReason::RepositoryMismatch);
    }
    if observed.head_repository() != scope.source_repository()
        || candidate.branch_name() != observed.head_ref().as_str()
    {
        return rejection(LegacyRejectionReason::HeadMismatch);
    }
    if CommitId::parse(candidate.commit_id())
        .map_or(true, |commit| &commit != observed.head_commit())
    {
        return rejection(LegacyRejectionReason::CommitMismatch);
    }
    let marker = legacy_marker(&change_id);
    let count = observed
        .body()
        .lines()
        .filter(|line| *line == marker)
        .count();
    if count > 1 {
        return Err(GithubError::LegacyAmbiguous);
    }
    if count == 0 {
        return rejection(LegacyRejectionReason::OwnershipMarkerMissing);
    }
    Ok(LegacyCandidateResolution::Verified(LegacyValidation {
        candidate: candidate.clone(),
        scope: scope.clone(),
        change_id,
        number: observed.number(),
        head: observed.head_ref().clone(),
        lifecycle: observed.lifecycle(),
        observation_digest,
    }))
}

#[derive(Serialize)]
struct LegacyObservation<'a> {
    number: u64,
    lifecycle: PrLifecycle,
    head_repository: String,
    head_ref: &'a str,
    head_commit: &'a str,
    base_ref: &'a str,
    title: &'a str,
    body: &'a str,
}

fn legacy_marker(change_id: &ChangeId) -> String {
    format!("Change ID: `{change_id}`")
}

fn merged_body(current: &str, section: &ManagedSection, max: usize) -> Result<String, GithubError> {
    match ManagedBody::merge(current, section, max).map_err(GithubError::Body)? {
        BodyMerge::Changed(body) => Ok(body),
        BodyMerge::Unchanged => Ok(current.to_owned()),
    }
}

fn repository_from_full_name(
    host: &str,
    full_name: &str,
    query: GithubQuery,
) -> Result<RepositoryId, GithubError> {
    RepositoryId::parse(format!("{host}/{full_name}")).map_err(|_| GithubError::Malformed { query })
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
