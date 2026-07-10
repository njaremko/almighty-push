use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::time::Duration;

const CHANGE_ID_LENGTH: usize = 32;
const COMMIT_ID_LENGTH: usize = 40;
const HEAD_REF_BYTES_MAX: usize = 255;
const REMOTE_NAME_BYTES_MAX: usize = 255;
const REPOSITORY_ID_BYTES_MAX: usize = 512;
const SELECTION_BYTES_MAX: usize = 1024;
const DESCRIPTION_BYTES_MAX: usize = 4096;
const ERROR_PREVIEW_CHARS_MAX: usize = 64;
const GITHUB_LIST_ROW_BYTES_MAX: u64 = 8192;

const CHANGE_COUNT_MIN: u64 = 1;
const CHANGE_COUNT_MAX: u64 = 64;
const GITHUB_PAGE_COUNT_MIN: u64 = 1;
const GITHUB_PAGE_COUNT_MAX: u64 = 10;
const GITHUB_PAGE_SIZE_MIN: u64 = 1;
const GITHUB_PAGE_SIZE_MAX: u64 = 100;
const EFFECT_COUNT_MIN: u64 = 1;
const EFFECT_COUNT_MAX: u64 = 512;
const COMMAND_OUTPUT_BYTES_MIN: u64 = 64 * 1024;
const COMMAND_OUTPUT_BYTES_MAX: u64 = 4 * 1024 * 1024;
const STATE_BYTES_MIN: u64 = 64 * 1024;
const STATE_BYTES_MAX: u64 = 4 * 1024 * 1024;
const BODY_BYTES_MIN: u64 = 4 * 1024;
const BODY_BYTES_MAX: u64 = 64 * 1024;
const COMMAND_TIMEOUT_MS_MIN: u64 = 100;
const COMMAND_TIMEOUT_MS_MAX: u64 = 30_000;
const LOCK_WAIT_MS_MIN: u64 = 100;
const LOCK_WAIT_MS_MAX: u64 = 5_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextKind {
    ChangeId,
    CommitId,
    HeadRef,
    RemoteName,
    RepositoryId,
    Selection,
}

impl Display for TextKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::ChangeId => "full jj change ID",
            Self::CommitId => "Git commit ID",
            Self::HeadRef => "GitHub head ref",
            Self::RemoteName => "Git remote name",
            Self::RepositoryId => "repository identity",
            Self::Selection => "jj revision selection",
        };
        formatter.write_str(name)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DescriptionErrorReason {
    Empty,
    Nul,
    TooLong,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitField {
    ChangeCount,
    GithubPageCount,
    GithubPageSize,
    EffectCount,
    CommandOutputBytes,
    StateBytes,
    BodyBytes,
    CommandTimeoutMs,
    LockWaitMs,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DomainError {
    InvalidText {
        kind: TextKind,
        actual_bytes: usize,
        preview: String,
    },
    InvalidPrNumber {
        value: u64,
    },
    CrossHostRepositories {
        source_host: String,
        target_host: String,
    },
    InvalidDescription {
        change_id: ChangeId,
        reason: DescriptionErrorReason,
        actual_bytes: usize,
    },
    InvalidLimit {
        field: LimitField,
        value: u64,
        min: u64,
        max: u64,
    },
    IncompatibleLimits {
        page_bytes_max: u64,
        command_output_bytes_max: u64,
    },
    ChangeCountExceeded {
        count: usize,
        max: usize,
    },
    DuplicateChangeId {
        change_id: ChangeId,
    },
    SelectedMerge {
        change_id: ChangeId,
    },
    SelectedFork {
        parent_change_id: ChangeId,
    },
    SelectedCycle {
        change_id: ChangeId,
    },
    DisconnectedSelection {
        root_count: usize,
    },
}

impl Display for DomainError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidText {
                kind,
                actual_bytes,
                preview,
            } => write!(
                formatter,
                "invalid {kind} ({actual_bytes} bytes, preview {preview:?})"
            ),
            Self::InvalidPrNumber { value } => {
                write!(formatter, "invalid pull request number: {value}")
            }
            Self::CrossHostRepositories {
                source_host,
                target_host,
            } => write!(
                formatter,
                "source host {source_host} and target host {target_host} differ"
            ),
            Self::InvalidDescription {
                change_id,
                reason,
                actual_bytes,
            } => write!(
                formatter,
                "change {change_id} has an invalid description ({reason:?}, {actual_bytes} bytes)"
            ),
            Self::InvalidLimit {
                field,
                value,
                min,
                max,
            } => write!(
                formatter,
                "limit {field:?} value {value} is outside {min}..={max}"
            ),
            Self::IncompatibleLimits {
                page_bytes_max,
                command_output_bytes_max,
            } => write!(
                formatter,
                "one GitHub page may use {page_bytes_max} bytes, exceeding the command output bound {command_output_bytes_max}"
            ),
            Self::ChangeCountExceeded { count, max } => {
                write!(
                    formatter,
                    "selected {count} changes, exceeding the bound {max}"
                )
            }
            Self::DuplicateChangeId { change_id } => {
                write!(formatter, "selected change ID {change_id} more than once")
            }
            Self::SelectedMerge { change_id } => {
                write!(
                    formatter,
                    "selected change {change_id} has multiple parents"
                )
            }
            Self::SelectedFork { parent_change_id } => {
                write!(
                    formatter,
                    "selected change {parent_change_id} has multiple children"
                )
            }
            Self::SelectedCycle { change_id } => {
                write!(formatter, "selected change {change_id} participates in a cycle")
            }
            Self::DisconnectedSelection { root_count } => write!(
                formatter,
                "selected changes do not form one chain ({root_count} roots)"
            ),
        }
    }
}

impl Error for DomainError {}

fn invalid_text(kind: TextKind, value: &str) -> DomainError {
    DomainError::InvalidText {
        kind,
        actual_bytes: value.len(),
        preview: value.chars().take(ERROR_PREVIEW_CHARS_MAX).collect(),
    }
}

macro_rules! validated_string_type {
    ($name:ident, $kind:expr, $validate:ident) => {
        #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn parse(value: impl Into<String>) -> Result<Self, DomainError> {
                let value = value.into();
                $validate(&value)?;
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Display for $name {
            fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = DomainError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::parse(value)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

fn validate_change_id(value: &str) -> Result<(), DomainError> {
    let valid =
        value.len() == CHANGE_ID_LENGTH && value.bytes().all(|byte| (b'k'..=b'z').contains(&byte));
    if !valid {
        return Err(invalid_text(TextKind::ChangeId, value));
    }
    Ok(())
}

fn validate_commit_id(value: &str) -> Result<(), DomainError> {
    let valid = value.len() == COMMIT_ID_LENGTH
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if !valid {
        return Err(invalid_text(TextKind::CommitId, value));
    }
    Ok(())
}

fn validate_ref_like(kind: TextKind, value: &str, bytes_max: usize) -> Result<(), DomainError> {
    if value.is_empty() || value.len() > bytes_max {
        return Err(invalid_text(kind, value));
    }
    let invalid_byte = value.bytes().any(|byte| {
        byte <= b' '
            || byte == 0x7f
            || matches!(byte, b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
    });
    let invalid_component = value.split('/').any(|component| {
        component.is_empty() || component.starts_with('.') || component.ends_with(".lock")
    });
    let invalid = value == "@"
        || value.starts_with('/')
        || value.ends_with('/')
        || value.ends_with('.')
        || value.contains("..")
        || value.contains("@{")
        || invalid_byte
        || invalid_component;
    if invalid {
        return Err(invalid_text(kind, value));
    }
    Ok(())
}

fn validate_head_ref(value: &str) -> Result<(), DomainError> {
    validate_ref_like(TextKind::HeadRef, value, HEAD_REF_BYTES_MAX)
}

fn validate_remote_name(value: &str) -> Result<(), DomainError> {
    validate_ref_like(TextKind::RemoteName, value, REMOTE_NAME_BYTES_MAX)
}

validated_string_type!(ChangeId, TextKind::ChangeId, validate_change_id);
validated_string_type!(CommitId, TextKind::CommitId, validate_commit_id);
validated_string_type!(HeadRef, TextKind::HeadRef, validate_head_ref);
validated_string_type!(RemoteName, TextKind::RemoteName, validate_remote_name);

impl HeadRef {
    pub fn owned(change_id: &ChangeId) -> Result<Self, DomainError> {
        Self::parse(format!("almighty-push/{change_id}"))
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct PrNumber(u64);

impl PrNumber {
    pub fn new(value: u64) -> Result<Self, DomainError> {
        if value == 0 {
            return Err(DomainError::InvalidPrNumber { value });
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for PrNumber {
    type Error = DomainError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PrNumber> for u64 {
    fn from(value: PrNumber) -> Self {
        value.0
    }
}

impl Display for PrNumber {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RepositoryId {
    host: String,
    owner: String,
    name: String,
}

impl RepositoryId {
    pub fn parse(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if value.is_empty() || value.len() > REPOSITORY_ID_BYTES_MAX {
            return Err(invalid_text(TextKind::RepositoryId, &value));
        }

        let mut components = value.split('/');
        let Some(host) = components.next() else {
            return Err(invalid_text(TextKind::RepositoryId, &value));
        };
        let Some(owner) = components.next() else {
            return Err(invalid_text(TextKind::RepositoryId, &value));
        };
        let Some(name) = components.next() else {
            return Err(invalid_text(TextKind::RepositoryId, &value));
        };
        let valid = components.next().is_none()
            && validate_host(host)
            && validate_owner(owner)
            && validate_repository_name(name);
        if !valid {
            return Err(invalid_text(TextKind::RepositoryId, &value));
        }

        Ok(Self {
            host: host.to_ascii_lowercase(),
            owner: owner.to_ascii_lowercase(),
            name: name.to_ascii_lowercase(),
        })
    }

    pub fn canonical(&self) -> String {
        format!("{}/{}/{}", self.host, self.owner, self.name)
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

fn validate_host(value: &str) -> bool {
    if value.is_empty() || value.len() > 253 || !value.contains('.') {
        return false;
    }
    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn validate_owner(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 39
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        && !value.contains("--")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn validate_repository_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

impl Display for RepositoryId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}/{}", self.host, self.owner, self.name)
    }
}

impl TryFrom<String> for RepositoryId {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<RepositoryId> for String {
    fn from(value: RepositoryId) -> Self {
        value.canonical()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PrLifecycle {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "ScopeWire", into = "ScopeWire")]
pub struct Scope {
    source_repository: RepositoryId,
    target_repository: RepositoryId,
    remote: RemoteName,
    base: HeadRef,
    tip_revset: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeWire {
    source_repository: RepositoryId,
    target_repository: RepositoryId,
    remote: RemoteName,
    base: HeadRef,
    tip_revset: String,
}

impl Scope {
    pub fn new(
        source_repository: RepositoryId,
        target_repository: RepositoryId,
        remote: RemoteName,
        base: HeadRef,
        tip_revset: String,
    ) -> Result<Self, DomainError> {
        validate_selection(&tip_revset)?;
        if source_repository.host() != target_repository.host() {
            return Err(DomainError::CrossHostRepositories {
                source_host: source_repository.host().to_owned(),
                target_host: target_repository.host().to_owned(),
            });
        }
        Ok(Self {
            source_repository,
            target_repository,
            remote,
            base,
            tip_revset,
        })
    }

    pub fn validate_tip_revset(value: &str) -> Result<(), DomainError> {
        validate_selection(value)
    }

    pub fn source_repository(&self) -> &RepositoryId {
        &self.source_repository
    }

    pub fn target_repository(&self) -> &RepositoryId {
        &self.target_repository
    }

    pub fn remote(&self) -> &RemoteName {
        &self.remote
    }

    pub fn base(&self) -> &HeadRef {
        &self.base
    }

    pub fn tip_revset(&self) -> &str {
        &self.tip_revset
    }
}

fn validate_selection(value: &str) -> Result<(), DomainError> {
    if value.is_empty() || value.len() > SELECTION_BYTES_MAX {
        return Err(invalid_text(TextKind::Selection, value));
    }
    let valid = !value.trim().is_empty()
        && !value
            .bytes()
            .any(|byte| byte == 0 || byte == b'\n' || byte == b'\r');
    if !valid {
        return Err(invalid_text(TextKind::Selection, value));
    }
    Ok(())
}

impl TryFrom<ScopeWire> for Scope {
    type Error = DomainError;

    fn try_from(value: ScopeWire) -> Result<Self, Self::Error> {
        Self::new(
            value.source_repository,
            value.target_repository,
            value.remote,
            value.base,
            value.tip_revset,
        )
    }
}

impl From<Scope> for ScopeWire {
    fn from(value: Scope) -> Self {
        Self {
            source_repository: value.source_repository,
            target_repository: value.target_repository,
            remote: value.remote,
            base: value.base,
            tip_revset: value.tip_revset,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LimitValues {
    pub change_count_max: u64,
    pub github_page_count_max: u64,
    pub github_page_size: u64,
    pub effect_count_max: u64,
    pub command_output_bytes_max: u64,
    pub state_bytes_max: u64,
    pub body_bytes_max: u64,
    pub command_timeout_ms: u64,
    pub lock_wait_ms: u64,
}

impl Default for LimitValues {
    fn default() -> Self {
        Self {
            change_count_max: CHANGE_COUNT_MAX,
            github_page_count_max: GITHUB_PAGE_COUNT_MAX,
            github_page_size: GITHUB_PAGE_SIZE_MAX,
            effect_count_max: EFFECT_COUNT_MAX,
            command_output_bytes_max: COMMAND_OUTPUT_BYTES_MAX,
            state_bytes_max: STATE_BYTES_MAX,
            body_bytes_max: BODY_BYTES_MAX,
            command_timeout_ms: COMMAND_TIMEOUT_MS_MAX,
            lock_wait_ms: LOCK_WAIT_MS_MAX,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "LimitValues", into = "LimitValues")]
pub struct Limits {
    change_count_max: usize,
    github_page_count_max: usize,
    github_page_size: usize,
    effect_count_max: usize,
    command_output_bytes_max: usize,
    state_bytes_max: usize,
    body_bytes_max: usize,
    command_timeout_ms: u64,
    lock_wait_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self::new(LimitValues::default()).expect("compiled default limits are valid")
    }
}

impl Limits {
    pub fn new(values: LimitValues) -> Result<Self, DomainError> {
        let bounds = [
            (
                LimitField::ChangeCount,
                values.change_count_max,
                CHANGE_COUNT_MIN,
                CHANGE_COUNT_MAX,
            ),
            (
                LimitField::GithubPageCount,
                values.github_page_count_max,
                GITHUB_PAGE_COUNT_MIN,
                GITHUB_PAGE_COUNT_MAX,
            ),
            (
                LimitField::GithubPageSize,
                values.github_page_size,
                GITHUB_PAGE_SIZE_MIN,
                GITHUB_PAGE_SIZE_MAX,
            ),
            (
                LimitField::EffectCount,
                values.effect_count_max,
                EFFECT_COUNT_MIN,
                EFFECT_COUNT_MAX,
            ),
            (
                LimitField::CommandOutputBytes,
                values.command_output_bytes_max,
                COMMAND_OUTPUT_BYTES_MIN,
                COMMAND_OUTPUT_BYTES_MAX,
            ),
            (
                LimitField::StateBytes,
                values.state_bytes_max,
                STATE_BYTES_MIN,
                STATE_BYTES_MAX,
            ),
            (
                LimitField::BodyBytes,
                values.body_bytes_max,
                BODY_BYTES_MIN,
                BODY_BYTES_MAX,
            ),
            (
                LimitField::CommandTimeoutMs,
                values.command_timeout_ms,
                COMMAND_TIMEOUT_MS_MIN,
                COMMAND_TIMEOUT_MS_MAX,
            ),
            (
                LimitField::LockWaitMs,
                values.lock_wait_ms,
                LOCK_WAIT_MS_MIN,
                LOCK_WAIT_MS_MAX,
            ),
        ];
        for (field, value, min, max) in bounds {
            check_limit(field, value, min, max)?;
        }
        check_page_output_bound(values)?;

        Ok(Self::from_checked_values(values))
    }

    fn from_checked_values(values: LimitValues) -> Self {
        Self {
            change_count_max: values.change_count_max as usize,
            github_page_count_max: values.github_page_count_max as usize,
            github_page_size: values.github_page_size as usize,
            effect_count_max: values.effect_count_max as usize,
            command_output_bytes_max: values.command_output_bytes_max as usize,
            state_bytes_max: values.state_bytes_max as usize,
            body_bytes_max: values.body_bytes_max as usize,
            command_timeout_ms: values.command_timeout_ms,
            lock_wait_ms: values.lock_wait_ms,
        }
    }

    pub fn change_count_max(self) -> usize {
        self.change_count_max
    }

    pub fn github_page_count_max(self) -> usize {
        self.github_page_count_max
    }

    pub fn github_page_size(self) -> usize {
        self.github_page_size
    }

    pub fn effect_count_max(self) -> usize {
        self.effect_count_max
    }

    pub fn command_output_bytes_max(self) -> usize {
        self.command_output_bytes_max
    }

    pub fn state_bytes_max(self) -> usize {
        self.state_bytes_max
    }

    pub fn body_bytes_max(self) -> usize {
        self.body_bytes_max
    }

    pub fn command_timeout(self) -> Duration {
        Duration::from_millis(self.command_timeout_ms)
    }

    pub fn lock_wait(self) -> Duration {
        Duration::from_millis(self.lock_wait_ms)
    }
}

fn check_limit(field: LimitField, value: u64, min: u64, max: u64) -> Result<(), DomainError> {
    if !(min..=max).contains(&value) {
        return Err(DomainError::InvalidLimit {
            field,
            value,
            min,
            max,
        });
    }
    Ok(())
}

fn check_page_output_bound(values: LimitValues) -> Result<(), DomainError> {
    let page_bytes_max = values
        .github_page_size
        .saturating_mul(GITHUB_LIST_ROW_BYTES_MAX);
    if page_bytes_max > values.command_output_bytes_max {
        return Err(DomainError::IncompatibleLimits {
            page_bytes_max,
            command_output_bytes_max: values.command_output_bytes_max,
        });
    }
    Ok(())
}

impl TryFrom<LimitValues> for Limits {
    type Error = DomainError;

    fn try_from(value: LimitValues) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Limits> for LimitValues {
    fn from(value: Limits) -> Self {
        Self {
            change_count_max: value.change_count_max as u64,
            github_page_count_max: value.github_page_count_max as u64,
            github_page_size: value.github_page_size as u64,
            effect_count_max: value.effect_count_max as u64,
            command_output_bytes_max: value.command_output_bytes_max as u64,
            state_bytes_max: value.state_bytes_max as u64,
            body_bytes_max: value.body_bytes_max as u64,
            command_timeout_ms: value.command_timeout_ms,
            lock_wait_ms: value.lock_wait_ms,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Revision {
    change_id: ChangeId,
    commit_id: CommitId,
    description: String,
    parent_change_ids: Box<[ChangeId]>,
    conflict: bool,
}

impl Revision {
    pub fn new(
        change_id: ChangeId,
        commit_id: CommitId,
        description: String,
        parent_change_ids: Box<[ChangeId]>,
        conflict: bool,
    ) -> Result<Self, DomainError> {
        let actual_bytes = description.len();
        let reason = if actual_bytes > DESCRIPTION_BYTES_MAX {
            Some(DescriptionErrorReason::TooLong)
        } else if description.bytes().any(|byte| byte == 0) {
            Some(DescriptionErrorReason::Nul)
        } else if description.trim().is_empty() {
            Some(DescriptionErrorReason::Empty)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(DomainError::InvalidDescription {
                change_id,
                reason,
                actual_bytes,
            });
        }
        Ok(Self {
            change_id,
            commit_id,
            description,
            parent_change_ids,
            conflict,
        })
    }

    pub fn change_id(&self) -> &ChangeId {
        &self.change_id
    }

    pub fn commit_id(&self) -> &CommitId {
        &self.commit_id
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn parent_change_ids(&self) -> &[ChangeId] {
        &self.parent_change_ids
    }

    pub fn has_conflict(&self) -> bool {
        self.conflict
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedChain {
    scope: Scope,
    revisions_base_to_tip: Box<[Revision]>,
}

impl SelectedChain {
    pub fn new(
        scope: Scope,
        revisions: Box<[Revision]>,
        limits: Limits,
    ) -> Result<Self, DomainError> {
        if revisions.len() > limits.change_count_max() {
            return Err(DomainError::ChangeCountExceeded {
                count: revisions.len(),
                max: limits.change_count_max(),
            });
        }
        if revisions.is_empty() {
            return Ok(Self {
                scope,
                revisions_base_to_tip: revisions,
            });
        }

        let rows = index_revisions(revisions)?;
        let (root, child_by_parent) = index_chain_links(&rows)?;
        let ordered = order_chain(rows, root, &child_by_parent)?;

        Ok(Self {
            scope,
            revisions_base_to_tip: ordered,
        })
    }

    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    pub fn revisions(&self) -> &[Revision] {
        &self.revisions_base_to_tip
    }
}

fn index_revisions(
    revisions: Box<[Revision]>,
) -> Result<BTreeMap<ChangeId, Revision>, DomainError> {
    let mut rows = BTreeMap::new();
    let mut duplicate_ids = BTreeSet::new();
    let mut merge_ids = BTreeSet::new();
    for revision in Vec::from(revisions) {
        if revision.parent_change_ids.len() > 1 {
            merge_ids.insert(revision.change_id.clone());
        }
        let change_id = revision.change_id.clone();
        if rows.insert(change_id.clone(), revision).is_some() {
            duplicate_ids.insert(change_id);
        }
    }
    if let Some(change_id) = duplicate_ids.into_iter().next() {
        return Err(DomainError::DuplicateChangeId { change_id });
    }
    if let Some(change_id) = merge_ids.into_iter().next() {
        return Err(DomainError::SelectedMerge { change_id });
    }
    Ok(rows)
}

fn index_chain_links(
    rows: &BTreeMap<ChangeId, Revision>,
) -> Result<(ChangeId, BTreeMap<ChangeId, ChangeId>), DomainError> {
    let mut roots = Vec::new();
    let mut child_by_parent = BTreeMap::new();
    for revision in rows.values() {
        let selected_parent = revision
            .parent_change_ids
            .iter()
            .find(|parent| rows.contains_key(*parent));
        if let Some(parent) = selected_parent {
            if child_by_parent
                .insert(parent.clone(), revision.change_id.clone())
                .is_some()
            {
                return Err(DomainError::SelectedFork {
                    parent_change_id: parent.clone(),
                });
            }
        } else {
            roots.push(revision.change_id.clone());
        }
    }
    if roots.is_empty() {
        return Err(DomainError::SelectedCycle {
            change_id: rows.keys().next().expect("nonempty rows checked").clone(),
        });
    }
    if roots.len() != 1 {
        return Err(DomainError::DisconnectedSelection {
            root_count: roots.len(),
        });
    }
    Ok((roots.pop().expect("one root checked"), child_by_parent))
}

fn order_chain(
    mut rows: BTreeMap<ChangeId, Revision>,
    root: ChangeId,
    child_by_parent: &BTreeMap<ChangeId, ChangeId>,
) -> Result<Box<[Revision]>, DomainError> {
    let expected_count = rows.len();
    let mut ordered = Vec::with_capacity(expected_count);
    let mut next = Some(root);
    while let Some(change_id) = next {
        let revision = rows
            .remove(&change_id)
            .expect("child links reference selected rows");
        next = child_by_parent.get(&change_id).cloned();
        ordered.push(revision);
    }
    if let Some(change_id) = rows.keys().next().cloned() {
        return Err(DomainError::SelectedCycle { change_id });
    }
    assert_eq!(ordered.len(), expected_count);
    Ok(ordered.into_boxed_slice())
}
