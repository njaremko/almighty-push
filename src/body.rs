use crate::domain::{ChangeId, Limits, RepositoryId};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt::{self, Display, Formatter};

const START: &str = "<!-- almighty-push:stack:v1:start -->";
const END: &str = "<!-- almighty-push:stack:v1:end -->";
const RESERVED: &str = "almighty-push:stack";
const SOURCE_PREFIX: &str = "Source repository: `";
const CHANGE_PREFIX: &str = "Change ID: `";
const STACK_HEADER: &str = "Active stack (base to tip):";
const STACK_PREFIX: &str = "- `";
const CURRENT_SUFFIX: &str = " (current)";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedSection {
    source_repository: RepositoryId,
    current_change: ChangeId,
    active_stack_base_to_tip: Box<[ChangeId]>,
}

impl ManagedSection {
    pub fn new(
        source_repository: RepositoryId,
        current_change: ChangeId,
        active_stack_base_to_tip: Box<[ChangeId]>,
        limits: Limits,
    ) -> Result<Self, BodyError> {
        if active_stack_base_to_tip.is_empty()
            || active_stack_base_to_tip.len() > limits.change_count_max()
        {
            return Err(BodyError::InvalidSection);
        }
        let unique = active_stack_base_to_tip.iter().collect::<BTreeSet<_>>();
        let current_count = active_stack_base_to_tip
            .iter()
            .filter(|change| *change == &current_change)
            .count();
        if unique.len() != active_stack_base_to_tip.len() || current_count != 1 {
            return Err(BodyError::InvalidSection);
        }
        Ok(Self {
            source_repository,
            current_change,
            active_stack_base_to_tip,
        })
    }

    pub fn parse(value: &str, limits: Limits) -> Result<Self, BodyError> {
        if value.is_empty()
            || value.len() > limits.body_bytes_max()
            || value.contains('\0')
            || value.contains(RESERVED)
            || value.ends_with('\n')
            || value.contains('\r')
        {
            return Err(BodyError::InvalidSection);
        }
        let lines = value.split('\n').collect::<Vec<_>>();
        if lines.len() < 4 || lines[2] != STACK_HEADER {
            return Err(BodyError::InvalidSection);
        }
        let source = parse_backticked(lines[0], SOURCE_PREFIX)?;
        let current = parse_backticked(lines[1], CHANGE_PREFIX)?;
        let source_repository =
            RepositoryId::parse(source).map_err(|_| BodyError::InvalidSection)?;
        let current_change = ChangeId::parse(current).map_err(|_| BodyError::InvalidSection)?;

        let mut stack = Vec::with_capacity(lines.len() - 3);
        let mut marked_current = None;
        for line in &lines[3..] {
            let (row, is_current) = line
                .strip_suffix(CURRENT_SUFFIX)
                .map_or((*line, false), |row| (row, true));
            let change = parse_backticked(row, STACK_PREFIX)?;
            let change = ChangeId::parse(change).map_err(|_| BodyError::InvalidSection)?;
            if is_current && marked_current.replace(change.clone()).is_some() {
                return Err(BodyError::InvalidSection);
            }
            stack.push(change);
        }
        if marked_current.as_ref() != Some(&current_change) {
            return Err(BodyError::InvalidSection);
        }
        let section = Self::new(
            source_repository,
            current_change,
            stack.into_boxed_slice(),
            limits,
        )?;
        if section.render() != value {
            return Err(BodyError::InvalidSection);
        }
        Ok(section)
    }

    pub fn render(&self) -> String {
        let row_bytes = self
            .active_stack_base_to_tip
            .len()
            .saturating_mul(CHANGE_PREFIX.len() + 40);
        let mut output = String::with_capacity(
            SOURCE_PREFIX.len()
                + self.source_repository.canonical().len()
                + STACK_HEADER.len()
                + row_bytes,
        );
        output.push_str(SOURCE_PREFIX);
        output.push_str(&self.source_repository.canonical());
        output.push_str("`\n");
        output.push_str(CHANGE_PREFIX);
        output.push_str(self.current_change.as_str());
        output.push_str("`\n");
        output.push_str(STACK_HEADER);
        for change in &self.active_stack_base_to_tip {
            output.push('\n');
            output.push_str(STACK_PREFIX);
            output.push_str(change.as_str());
            output.push('`');
            if change == &self.current_change {
                output.push_str(CURRENT_SUFFIX);
            }
        }
        output
    }

    pub fn source_repository(&self) -> &RepositoryId {
        &self.source_repository
    }

    pub fn current_change(&self) -> &ChangeId {
        &self.current_change
    }

    pub fn active_stack_base_to_tip(&self) -> &[ChangeId] {
        &self.active_stack_base_to_tip
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BodyMerge {
    Unchanged,
    Changed(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyError {
    MalformedMarkers,
    InvalidSection,
    BodyTooLarge { bytes: usize, max: usize },
}

impl Display for BodyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedMarkers => formatter.write_str("managed body markers are malformed"),
            Self::InvalidSection => formatter.write_str("managed section is not canonical v1 data"),
            Self::BodyTooLarge { bytes, max } => {
                write!(
                    formatter,
                    "merged body uses {bytes} bytes; maximum is {max}"
                )
            }
        }
    }
}

impl Error for BodyError {}

pub struct ManagedBody;

impl ManagedBody {
    pub fn merge(
        user_body: &str,
        section: &ManagedSection,
        max: usize,
    ) -> Result<BodyMerge, BodyError> {
        let rendered = section.render();
        if rendered.len() > max {
            return Err(BodyError::BodyTooLarge {
                bytes: rendered.len(),
                max,
            });
        }
        let interval = managed_interval(user_body)?;
        let (prefix, suffix, separator) = match interval {
            None => (
                user_body,
                "",
                if user_body.is_empty() { "" } else { "\n\n" },
            ),
            Some((start, end)) => {
                let interior_start = start + START.len() + 1;
                let interior_end = end - END.len() - 1;
                ManagedSection::parse(&user_body[interior_start..interior_end], Limits::default())?;
                (&user_body[..start], &user_body[end..], "")
            }
        };
        let bytes = prefix
            .len()
            .checked_add(separator.len())
            .and_then(|size| size.checked_add(START.len() + 1))
            .and_then(|size| size.checked_add(rendered.len()))
            .and_then(|size| size.checked_add(1 + END.len()))
            .and_then(|size| size.checked_add(suffix.len()))
            .ok_or(BodyError::BodyTooLarge {
                bytes: usize::MAX,
                max,
            })?;
        if bytes > max {
            return Err(BodyError::BodyTooLarge { bytes, max });
        }

        let mut merged = String::with_capacity(bytes);
        merged.push_str(prefix);
        merged.push_str(separator);
        merged.push_str(START);
        merged.push('\n');
        merged.push_str(&rendered);
        merged.push('\n');
        merged.push_str(END);
        merged.push_str(suffix);
        assert_eq!(merged.len(), bytes);
        if merged == user_body {
            Ok(BodyMerge::Unchanged)
        } else {
            Ok(BodyMerge::Changed(merged))
        }
    }

    pub fn section(body: &str, limits: Limits) -> Result<Option<ManagedSection>, BodyError> {
        let Some((start, end)) = managed_interval(body)? else {
            return Ok(None);
        };
        let interior_start = start + START.len() + 1;
        let interior_end = end - END.len() - 1;
        ManagedSection::parse(&body[interior_start..interior_end], limits).map(Some)
    }
}

fn parse_backticked<'a>(line: &'a str, prefix: &str) -> Result<&'a str, BodyError> {
    let value = line
        .strip_prefix(prefix)
        .and_then(|value| value.strip_suffix('`'))
        .ok_or(BodyError::InvalidSection)?;
    if value.is_empty() || value.contains('`') {
        return Err(BodyError::InvalidSection);
    }
    Ok(value)
}

fn managed_interval(body: &str) -> Result<Option<(usize, usize)>, BodyError> {
    let start = unique_marker(body, START)?;
    let end = unique_marker(body, END)?;
    match (start, end) {
        (None, None) => {
            if body.contains(RESERVED) {
                Err(BodyError::MalformedMarkers)
            } else {
                Ok(None)
            }
        }
        (Some(start), Some(end)) => {
            let suffix_start = end
                .checked_add(END.len())
                .ok_or(BodyError::MalformedMarkers)?;
            let interior_start = start
                .checked_add(START.len())
                .ok_or(BodyError::MalformedMarkers)?;
            let ordered = interior_start < end;
            let exact_newlines = body.as_bytes().get(interior_start) == Some(&b'\n')
                && body.as_bytes().get(end.wrapping_sub(1)) == Some(&b'\n');
            let reserved_count = body.match_indices(RESERVED).count();
            if !ordered || !exact_newlines || reserved_count != 2 {
                return Err(BodyError::MalformedMarkers);
            }
            Ok(Some((start, suffix_start)))
        }
        _ => Err(BodyError::MalformedMarkers),
    }
}

fn unique_marker(body: &str, marker: &str) -> Result<Option<usize>, BodyError> {
    let mut matches = body.match_indices(marker);
    let first = matches.next().map(|(index, _)| index);
    if matches.next().is_some() {
        return Err(BodyError::MalformedMarkers);
    }
    Ok(first)
}
