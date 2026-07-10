use crate::command::{CommandError, CommandExecutor, CommandOutput, CommandSpec, SpecError};
use crate::config::{ConfigError, ResolvedConfig};
use crate::domain::{
    ChangeId, CommitId, DomainError, HeadRef, RemoteName, Revision, Scope, SelectedChain,
};
use crate::executor::{DriverId, EffectCheck, EffectRequest, PreparedEffect};
use crate::plan::{Effect, EffectResult, JjEffect, PrOwnership, RemoteRefState};
use crate::state::StateV3;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::OsString;
use std::fmt::{self, Display, Formatter};
use std::path::PathBuf;

const COMMIT_JSON_TEMPLATE: &str = "json(self) ++ \"\\n\"";
const REMOTE_REF_JSON_TEMPLATE: &str = r#"if(remote, concat(
  "{\"name\":", json(name),
  ",\"remote\":", json(remote),
  ",\"present\":", json(present),
  ",\"conflict\":", json(conflict),
  ",\"normal_target\":", json(normal_target),
  "}\n",
))"#;
const PARENT_COUNT_SUPPORTED_MAX: usize = 2;
const PARENT_QUERY_ROW_COUNT_MAX: usize = PARENT_COUNT_SUPPORTED_MAX + 2;
const REMOTE_HEAD_QUERY_ROW_COUNT_MAX: usize = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JjQuery {
    Revisions,
    Parents,
    Conflicts,
    ExactChange,
    ExactCommit,
    RemoteHeads,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JjEffectKind {
    PushHead,
    DeleteHead,
    Rebase,
}

pub enum JjError {
    CommandSpec(Box<SpecError>),
    Command(Box<CommandError>),
    Config(Box<ConfigError>),
    Domain(Box<DomainError>),
    ExecutorOutputLimit {
        bytes: usize,
        max: usize,
    },
    MalformedJson {
        query: JjQuery,
        line: usize,
        source: Box<serde_json::Error>,
    },
    RowCountExceeded {
        query: JjQuery,
        count: usize,
        max: usize,
    },
    DuplicateRevision {
        change_id: ChangeId,
    },
    DuplicateConflict {
        change_id: ChangeId,
    },
    ObservationChanged {
        change_id: ChangeId,
    },
    SelectedParentVersionChanged {
        child_change_id: ChangeId,
        parent_change_id: ChangeId,
        selected_commit_id: CommitId,
        observed_commit_id: CommitId,
    },
    ParentCountExceeded {
        change_id: ChangeId,
        count: usize,
        max: usize,
    },
    MalformedRemoteHead {
        head: HeadRef,
    },
    DuplicateRemoteHead {
        head: HeadRef,
        count: usize,
    },
    ConflictedRemoteHead {
        head: HeadRef,
    },
    UnsupportedEffect {
        effect: JjEffectKind,
    },
    Precondition {
        effect: JjEffectKind,
    },
    AuthorityMismatch,
}

impl Display for JjError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::CommandSpec(error) => Display::fmt(error, formatter),
            Self::Command(error) => Display::fmt(error, formatter),
            Self::Config(error) => Display::fmt(error, formatter),
            Self::Domain(error) => Display::fmt(error, formatter),
            Self::ExecutorOutputLimit { bytes, max } => write!(
                formatter,
                "jj executor returned {bytes} bytes, exceeding the bound {max}"
            ),
            Self::MalformedJson { query, line, .. } => {
                write!(formatter, "jj {query:?} row {line} is malformed JSON")
            }
            Self::RowCountExceeded { query, count, max } => write!(
                formatter,
                "jj {query:?} returned at least {count} rows, exceeding the bound {max}"
            ),
            Self::DuplicateRevision { change_id } => {
                write!(
                    formatter,
                    "jj returned duplicate selected change {change_id}"
                )
            }
            Self::DuplicateConflict { change_id } => {
                write!(
                    formatter,
                    "jj returned duplicate conflict change {change_id}"
                )
            }
            Self::ObservationChanged { change_id } => write!(
                formatter,
                "jj change {change_id} changed during the observation epoch"
            ),
            Self::SelectedParentVersionChanged {
                child_change_id,
                parent_change_id,
                selected_commit_id,
                observed_commit_id,
            } => write!(
                formatter,
                "selected child {child_change_id} observed parent {parent_change_id} at commit {observed_commit_id}, not selected commit {selected_commit_id}"
            ),
            Self::ParentCountExceeded {
                change_id,
                count,
                max,
            } => write!(
                formatter,
                "jj change {change_id} has at least {count} parents, exceeding the bound {max}"
            ),
            Self::MalformedRemoteHead { head } => {
                write!(formatter, "jj returned malformed remote head {head}")
            }
            Self::DuplicateRemoteHead { head, count } => {
                write!(formatter, "jj returned {count} rows for remote head {head}")
            }
            Self::ConflictedRemoteHead { head } => {
                write!(formatter, "jj remote head {head} is conflicted")
            }
            Self::UnsupportedEffect { effect } => {
                write!(formatter, "jj adapter does not execute {effect:?}")
            }
            Self::Precondition { effect } => {
                write!(formatter, "jj {effect:?} precondition does not hold")
            }
            Self::AuthorityMismatch => {
                formatter.write_str("jj prepared authority does not match this exact effect")
            }
        }
    }
}

impl fmt::Debug for JjError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(self, formatter)
    }
}

impl Error for JjError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CommandSpec(error) => Some(error.as_ref()),
            Self::Command(error) => Some(error.as_ref()),
            Self::Config(error) => Some(error.as_ref()),
            Self::Domain(error) => Some(error.as_ref()),
            Self::MalformedJson { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

impl From<SpecError> for JjError {
    fn from(error: SpecError) -> Self {
        Self::CommandSpec(Box::new(error))
    }
}

impl From<CommandError> for JjError {
    fn from(error: CommandError) -> Self {
        Self::Command(Box::new(error))
    }
}

impl From<ConfigError> for JjError {
    fn from(error: ConfigError) -> Self {
        Self::Config(Box::new(error))
    }
}

impl From<DomainError> for JjError {
    fn from(error: DomainError) -> Self {
        Self::Domain(Box::new(error))
    }
}

pub struct JjClient<'a, E> {
    executor: &'a E,
    program: PathBuf,
    config: &'a ResolvedConfig,
    environment: Box<[(OsString, OsString)]>,
    validated_legacy_ownership: BTreeSet<PrOwnership>,
}

impl<'a, E: CommandExecutor> JjClient<'a, E> {
    pub fn new(
        executor: &'a E,
        program: PathBuf,
        config: &'a ResolvedConfig,
        environment: Box<[(OsString, OsString)]>,
    ) -> Self {
        Self {
            executor,
            program,
            config,
            environment,
            validated_legacy_ownership: BTreeSet::new(),
        }
    }

    pub fn new_with_ownership_state(
        executor: &'a E,
        program: PathBuf,
        config: &'a ResolvedConfig,
        ownership_state: &StateV3,
        environment: Box<[(OsString, OsString)]>,
    ) -> Self {
        assert_eq!(config.scope(), ownership_state.scope());
        Self {
            executor,
            program,
            config,
            environment,
            validated_legacy_ownership: ownership_state
                .validated_legacy_ownership()
                .cloned()
                .collect(),
        }
    }

    pub(crate) fn scope(&self) -> &Scope {
        self.config.scope()
    }

    pub(crate) fn workspace_root(&self) -> &std::path::Path {
        self.config.workspace_root()
    }

    pub(crate) fn check_current_effect(
        &self,
        request: &EffectRequest,
        effect: &Effect,
    ) -> Result<EffectCheck, JjError> {
        let Effect::Jj(effect_kind) = effect else {
            return Err(JjError::UnsupportedEffect {
                effect: JjEffectKind::PushHead,
            });
        };
        match effect_kind {
            JjEffect::PushHead {
                ownership,
                expected,
                desired,
            } => {
                let generated = HeadRef::owned(ownership.change_id())
                    .is_ok_and(|head| &head == ownership.head());
                let validated_legacy = ownership.is_validated_legacy()
                    && self.validated_legacy_ownership.contains(ownership);
                if !generated && !validated_legacy {
                    return Err(JjError::Precondition {
                        effect: JjEffectKind::PushHead,
                    });
                }
                let observed = self.observe_remote_head(ownership.head())?;
                if observed == RemoteRefState::At(desired.clone()) {
                    return Ok(EffectCheck::satisfied(
                        request,
                        effect,
                        EffectResult::Satisfied,
                    ));
                }
                if &observed != expected {
                    return Ok(EffectCheck::drift(request, effect));
                }
                let local = self.observe_unique_change(
                    ownership.change_id(),
                    JjQuery::ExactChange,
                    JjEffectKind::PushHead,
                )?;
                if &local.commit_id != desired {
                    return Ok(EffectCheck::drift(request, effect));
                }
                Ok(EffectCheck::prepared(request, effect))
            }
            JjEffect::Rebase {
                change_id,
                expected_commit,
                expected_parent,
                desired_parent,
            } => {
                let observed = self.observe_exact_change(change_id)?;
                if rebase_postcondition(&observed, desired_parent) {
                    return Ok(EffectCheck::satisfied(
                        request,
                        effect,
                        EffectResult::Rebased {
                            commit_id: observed.row.commit_id,
                        },
                    ));
                }
                let precondition_holds = observed.row.commit_id == *expected_commit
                    && !observed.conflict
                    && observed.parents.len() == 1
                    && observed.parents[0].commit_id == *expected_parent
                    && self
                        .observe_unique_commit(desired_parent, JjQuery::ExactCommit)?
                        .is_some();
                Ok(if precondition_holds {
                    EffectCheck::prepared(request, effect)
                } else {
                    EffectCheck::drift(request, effect)
                })
            }
            JjEffect::DeleteHead { .. } => Err(JjError::UnsupportedEffect {
                effect: JjEffectKind::DeleteHead,
            }),
        }
    }

    pub(crate) fn execute_checked_effect(
        &self,
        driver_id: DriverId,
        request: &EffectRequest,
        expected: &Effect,
        prepared: PreparedEffect,
    ) -> Result<(), JjError> {
        let effect = prepared
            .into_effect(request, expected, driver_id)
            .map_err(|_| JjError::AuthorityMismatch)?;
        let Effect::Jj(effect) = effect else {
            return Err(JjError::UnsupportedEffect {
                effect: JjEffectKind::PushHead,
            });
        };
        match &effect {
            JjEffect::PushHead { .. } => self.attempt_push_named_head(&effect),
            JjEffect::Rebase { .. } => self.attempt_rebase(&effect),
            JjEffect::DeleteHead { .. } => Err(JjError::UnsupportedEffect {
                effect: JjEffectKind::DeleteHead,
            }),
        }
    }

    pub fn observe_chain(&self) -> Result<SelectedChain, JjError> {
        let row_count_max = self.config.limits().change_count_max();
        let selection = self.selection_revset();
        let rows = self.query_commits(JjQuery::Revisions, &selection, row_count_max)?;
        let rows = index_unique_revisions(rows)?;

        let mut parent_ids = BTreeMap::new();
        let mut parent_version_changes = BTreeSet::new();
        for row in rows.values() {
            let parents = self.observe_parent_rows(row)?;
            for parent in &parents {
                let Some(selected_parent) = rows.get(&parent.change_id) else {
                    continue;
                };
                if selected_parent.commit_id != parent.commit_id {
                    parent_version_changes.insert((
                        row.change_id.clone(),
                        parent.change_id.clone(),
                        selected_parent.commit_id.clone(),
                        parent.commit_id.clone(),
                    ));
                }
            }
            parent_ids.insert(
                row.change_id.clone(),
                parents
                    .into_iter()
                    .map(|parent| parent.change_id)
                    .collect::<Vec<_>>()
                    .into_boxed_slice(),
            );
        }
        if let Some((child_change_id, parent_change_id, selected_commit_id, observed_commit_id)) =
            parent_version_changes.into_iter().next()
        {
            return Err(JjError::SelectedParentVersionChanged {
                child_change_id,
                parent_change_id,
                selected_commit_id,
                observed_commit_id,
            });
        }

        let conflict_rows = self.query_commits(
            JjQuery::Conflicts,
            &format!("({selection}) & conflicts()"),
            row_count_max,
        )?;
        let conflicts = index_conflicts(conflict_rows, &rows)?;
        let revisions = rows
            .into_values()
            .map(|row| {
                let parents = parent_ids
                    .remove(&row.change_id)
                    .expect("every indexed row received a parent observation");
                let conflict = conflicts.contains(&row.change_id);
                Revision::new(
                    row.change_id,
                    row.commit_id,
                    row.description,
                    parents,
                    conflict,
                )
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_boxed_slice();
        assert!(parent_ids.is_empty());
        SelectedChain::new(self.config.scope().clone(), revisions, self.config.limits())
            .map_err(Into::into)
    }

    pub fn observe_remote_heads(
        &self,
        heads: &[HeadRef],
    ) -> Result<BTreeMap<HeadRef, RemoteRefState>, JjError> {
        let max = self.config.limits().change_count_max();
        if heads.len() > max {
            return Err(JjError::RowCountExceeded {
                query: JjQuery::RemoteHeads,
                count: heads.len(),
                max,
            });
        }
        let unique = heads.iter().cloned().collect::<BTreeSet<_>>();
        let mut observed = BTreeMap::new();
        for head in unique {
            let state = self.observe_remote_head(&head)?;
            observed.insert(head, state);
        }
        Ok(observed)
    }

    pub fn fetch(&self) -> Result<(), JjError> {
        let args = os_args([
            "--ignore-working-copy",
            "git",
            "fetch",
            "--remote",
            self.config.remote().as_str(),
        ]);
        self.run(args).map(|_| ())
    }

    fn attempt_push_named_head(&self, effect: &JjEffect) -> Result<(), JjError> {
        let JjEffect::PushHead {
            ownership,
            expected,
            desired,
        } = effect
        else {
            return Err(JjError::UnsupportedEffect {
                effect: effect_kind(effect),
            });
        };
        let change_id = ownership.change_id();
        let head = ownership.head();
        let generated = HeadRef::owned(change_id).is_ok_and(|expected| &expected == head);
        let validated_legacy =
            ownership.is_validated_legacy() && self.validated_legacy_ownership.contains(ownership);
        if !generated && !validated_legacy {
            return Err(JjError::Precondition {
                effect: JjEffectKind::PushHead,
            });
        }

        let before = self.observe_remote_head(head)?;
        if before == RemoteRefState::At(desired.clone()) {
            return Ok(());
        }
        if &before != expected {
            return Err(JjError::Precondition {
                effect: JjEffectKind::PushHead,
            });
        }
        let local =
            self.observe_unique_change(change_id, JjQuery::ExactChange, JjEffectKind::PushHead)?;
        if &local.commit_id != desired {
            return Err(JjError::Precondition {
                effect: JjEffectKind::PushHead,
            });
        }

        let named = format!("{}={}", head.as_str(), change_id.as_str());
        self.run(os_args([
            "--ignore-working-copy",
            "git",
            "push",
            "--remote",
            self.config.remote().as_str(),
            "--named",
            &named,
        ]))
        .map(|_| ())
    }

    fn attempt_rebase(&self, effect: &JjEffect) -> Result<(), JjError> {
        let JjEffect::Rebase {
            change_id,
            expected_commit,
            expected_parent,
            desired_parent,
        } = effect
        else {
            return Err(JjError::UnsupportedEffect {
                effect: effect_kind(effect),
            });
        };

        let before = self.observe_exact_change(change_id)?;
        if rebase_postcondition(&before, desired_parent) {
            return Ok(());
        }
        let precondition_holds = before.row.commit_id == *expected_commit
            && !before.conflict
            && before.parents.len() == 1
            && before.parents[0].commit_id == *expected_parent;
        if !precondition_holds
            || self
                .observe_unique_commit(desired_parent, JjQuery::ExactCommit)?
                .is_none()
        {
            return Err(JjError::Precondition {
                effect: JjEffectKind::Rebase,
            });
        }

        let source_revset = commit_id_revset(expected_commit);
        let destination_revset = commit_id_revset(desired_parent);
        self.run(os_args([
            "--ignore-working-copy",
            "rebase",
            "--source",
            &source_revset,
            "--onto",
            &destination_revset,
        ]))
        .map(|_| ())
    }

    fn selection_revset(&self) -> String {
        let base = exact_pattern(self.config.base().as_str());
        let remote = exact_pattern(self.config.remote().as_str());
        format!(
            "(remote_bookmarks({base}, {remote}))..({})",
            self.config.tip_revset()
        )
    }

    fn observe_exact_change(&self, change_id: &ChangeId) -> Result<ObservedCommit, JjError> {
        let row =
            self.observe_unique_change(change_id, JjQuery::ExactChange, JjEffectKind::Rebase)?;
        let parents = self.observe_parent_rows(&row)?.into_boxed_slice();
        let conflict_revset = format!("{} & conflicts()", commit_id_revset(&row.commit_id));
        let conflicts = self.query_commits(JjQuery::Conflicts, &conflict_revset, 1)?;
        let conflict = match conflicts.as_slice() {
            [] => false,
            [observed]
                if observed.change_id == row.change_id && observed.commit_id == row.commit_id =>
            {
                true
            }
            _ => {
                return Err(JjError::ObservationChanged {
                    change_id: row.change_id,
                });
            }
        };
        Ok(ObservedCommit {
            row,
            parents,
            conflict,
        })
    }

    fn observe_parent_rows(&self, source: &CommitRow) -> Result<Vec<CommitRow>, JjError> {
        let source_revset = commit_id_revset(&source.commit_id);
        let revset = format!("{source_revset} | parents({source_revset})");
        let mut rows = self.query_commits(JjQuery::Parents, &revset, PARENT_QUERY_ROW_COUNT_MAX)?;
        let source_positions = rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| {
                (row.commit_id == source.commit_id && row.change_id == source.change_id)
                    .then_some(index)
            })
            .collect::<Vec<_>>();
        if source_positions.len() != 1 {
            return Err(JjError::ObservationChanged {
                change_id: source.change_id.clone(),
            });
        }
        rows.remove(source_positions[0]);
        if rows.len() > PARENT_COUNT_SUPPORTED_MAX {
            return Err(JjError::ParentCountExceeded {
                change_id: source.change_id.clone(),
                count: rows.len(),
                max: PARENT_COUNT_SUPPORTED_MAX,
            });
        }
        Ok(rows)
    }

    fn observe_unique_change(
        &self,
        change_id: &ChangeId,
        query: JjQuery,
        effect: JjEffectKind,
    ) -> Result<CommitRow, JjError> {
        let rows = self.query_commits(query, &change_id_revset(change_id), 2)?;
        match rows.as_slice() {
            [row] if &row.change_id == change_id => Ok(row.clone()),
            [] => Err(JjError::Precondition { effect }),
            _ => Err(JjError::DuplicateRevision {
                change_id: change_id.clone(),
            }),
        }
    }

    fn observe_unique_commit(
        &self,
        commit_id: &CommitId,
        query: JjQuery,
    ) -> Result<Option<CommitRow>, JjError> {
        let rows = self.query_commits(query, &commit_id_revset(commit_id), 2)?;
        match rows.as_slice() {
            [] => Ok(None),
            [row] if &row.commit_id == commit_id => Ok(Some(row.clone())),
            _ => Err(JjError::RowCountExceeded {
                query,
                count: rows.len(),
                max: 1,
            }),
        }
    }

    fn observe_remote_head(&self, head: &HeadRef) -> Result<RemoteRefState, JjError> {
        let remote_pattern = format!("exact:{}", self.config.remote().as_str());
        let head_pattern = format!("exact:{}", head.as_str());
        let args = vec![
            "--ignore-working-copy".into(),
            "bookmark".into(),
            "list".into(),
            "--remote".into(),
            remote_pattern.into(),
            "--template".into(),
            REMOTE_REF_JSON_TEMPLATE.into(),
            head_pattern.into(),
        ]
        .into_boxed_slice();
        let output = self.run(args)?;
        let rows: Vec<RemoteHeadRow> = parse_json_lines(
            JjQuery::RemoteHeads,
            &output.stdout,
            REMOTE_HEAD_QUERY_ROW_COUNT_MAX,
        )?;
        if rows.is_empty() {
            return Ok(RemoteRefState::Absent);
        }
        if rows.len() != 1 {
            return Err(JjError::DuplicateRemoteHead {
                head: head.clone(),
                count: rows.len(),
            });
        }
        let row = &rows[0];
        let observed_head = HeadRef::parse(row.name.clone())?;
        let observed_remote = row.remote.as_deref().map(RemoteName::parse).transpose()?;
        if observed_head != *head
            || observed_remote.as_ref() != Some(self.config.remote())
            || !row.present
        {
            return Err(JjError::MalformedRemoteHead { head: head.clone() });
        }
        if row.conflict {
            return Err(JjError::ConflictedRemoteHead { head: head.clone() });
        }
        let Some(target) = &row.normal_target else {
            return Err(JjError::MalformedRemoteHead { head: head.clone() });
        };
        Ok(RemoteRefState::At(target.commit_id.clone()))
    }

    fn query_commits(
        &self,
        query: JjQuery,
        revset: &str,
        row_count_max: usize,
    ) -> Result<Vec<CommitRow>, JjError> {
        let limit = row_count_max
            .checked_add(1)
            .expect("compiled row bounds fit usize")
            .to_string();
        let args = vec![
            "--ignore-working-copy".into(),
            "log".into(),
            "--no-graph".into(),
            "--revision".into(),
            revset.into(),
            "--limit".into(),
            limit.into(),
            "--template".into(),
            COMMIT_JSON_TEMPLATE.into(),
        ]
        .into_boxed_slice();
        let output = self.run(args)?;
        parse_json_lines(query, &output.stdout, row_count_max)
    }

    fn run(&self, args: Box<[OsString]>) -> Result<CommandOutput, JjError> {
        assert_eq!(
            args.first().map(OsString::as_os_str),
            Some(std::ffi::OsStr::new("--ignore-working-copy"))
        );
        let repository = self.config.duplicate_repository()?;
        let spec = CommandSpec::new_in_retained_repository(
            self.program.as_os_str().to_owned(),
            args,
            self.config.workspace_root().to_owned(),
            repository,
            Box::new([]),
            self.environment.clone(),
            self.config.limits(),
        )?;
        let output = self.executor.run(&spec)?;
        let bytes = output.stdout.len().saturating_add(output.stderr.len());
        let max = self.config.limits().command_output_bytes_max();
        if bytes > max {
            return Err(JjError::ExecutorOutputLimit { bytes, max });
        }
        Ok(output)
    }
}

#[derive(Clone, Deserialize)]
struct CommitRow {
    change_id: ChangeId,
    commit_id: CommitId,
    description: String,
}

#[derive(Deserialize)]
struct RemoteHeadRow {
    name: String,
    remote: Option<String>,
    present: bool,
    conflict: bool,
    normal_target: Option<CommitRow>,
}

struct ObservedCommit {
    row: CommitRow,
    parents: Box<[CommitRow]>,
    conflict: bool,
}

fn parse_json_lines<T: for<'de> Deserialize<'de>>(
    query: JjQuery,
    output: &str,
    row_count_max: usize,
) -> Result<Vec<T>, JjError> {
    let mut rows = Vec::with_capacity(row_count_max.min(64));
    for (index, line) in output.lines().enumerate() {
        let count = index + 1;
        if count > row_count_max {
            return Err(JjError::RowCountExceeded {
                query,
                count,
                max: row_count_max,
            });
        }
        let row = serde_json::from_str(line).map_err(|source| JjError::MalformedJson {
            query,
            line: count,
            source: Box::new(source),
        })?;
        rows.push(row);
    }
    Ok(rows)
}

fn index_unique_revisions(rows: Vec<CommitRow>) -> Result<BTreeMap<ChangeId, CommitRow>, JjError> {
    let mut indexed = BTreeMap::new();
    let mut duplicates = BTreeSet::new();
    for row in rows {
        let change_id = row.change_id.clone();
        if indexed.insert(change_id.clone(), row).is_some() {
            duplicates.insert(change_id);
        }
    }
    if let Some(change_id) = duplicates.into_iter().next() {
        return Err(JjError::DuplicateRevision { change_id });
    }
    Ok(indexed)
}

fn index_conflicts(
    rows: Vec<CommitRow>,
    revisions: &BTreeMap<ChangeId, CommitRow>,
) -> Result<BTreeSet<ChangeId>, JjError> {
    let mut conflicts = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    let mut changed = BTreeSet::new();
    for row in rows {
        let observation_changed = match revisions.get(&row.change_id) {
            Some(revision) => revision.commit_id != row.commit_id,
            None => true,
        };
        if observation_changed {
            changed.insert(row.change_id.clone());
        }
        if !conflicts.insert(row.change_id.clone()) {
            duplicates.insert(row.change_id);
        }
    }
    if let Some(change_id) = duplicates.into_iter().next() {
        return Err(JjError::DuplicateConflict { change_id });
    }
    if let Some(change_id) = changed.into_iter().next() {
        return Err(JjError::ObservationChanged { change_id });
    }
    Ok(conflicts)
}

fn rebase_postcondition(observed: &ObservedCommit, desired_parent: &CommitId) -> bool {
    !observed.conflict
        && observed.parents.len() == 1
        && observed.parents[0].commit_id == *desired_parent
}

fn exact_pattern(value: &str) -> String {
    format!(
        "exact:{}",
        serde_json::to_string(value).expect("validated UTF-8 string serializes")
    )
}

fn change_id_revset(change_id: &ChangeId) -> String {
    format!(
        "change_id({})",
        serde_json::to_string(change_id.as_str()).expect("validated change ID serializes")
    )
}

fn commit_id_revset(commit_id: &CommitId) -> String {
    format!(
        "commit_id({})",
        serde_json::to_string(commit_id.as_str()).expect("validated commit ID serializes")
    )
}

fn effect_kind(effect: &JjEffect) -> JjEffectKind {
    match effect {
        JjEffect::PushHead { .. } => JjEffectKind::PushHead,
        JjEffect::DeleteHead { .. } => JjEffectKind::DeleteHead,
        JjEffect::Rebase { .. } => JjEffectKind::Rebase,
    }
}

fn os_args<const N: usize>(args: [&str; N]) -> Box<[OsString]> {
    args.into_iter().map(OsString::from).collect()
}
