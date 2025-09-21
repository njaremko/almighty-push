use crate::constants::{CHANGES_BRANCH_PREFIX, PUSH_BRANCH_PREFIX, STATE_FILE};
use crate::types::{ClosedPrInfo, PrInfo, Revision, State};
use anyhow::{Context, Result};
use chrono::Local;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Manages persistent state for almighty-push
pub struct StateManager {
    state_file: PathBuf,
}

impl StateManager {
    /// Create a new StateManager
    pub fn new() -> Self {
        Self {
            state_file: PathBuf::from(STATE_FILE),
        }
    }

    /// Create a StateManager with a custom state file path
    #[allow(dead_code)]
    pub fn with_file(state_file: impl AsRef<Path>) -> Self {
        Self {
            state_file: state_file.as_ref().to_path_buf(),
        }
    }

    /// Load state from file
    pub fn load(&self) -> Result<State> {
        if !self.state_file.exists() {
            return Ok(State::default());
        }

        let contents = fs::read_to_string(&self.state_file)
            .with_context(|| format!("Failed to read state file: {:?}", self.state_file))?;

        let state = serde_json::from_str(&contents)
            .with_context(|| format!("Failed to parse state file: {:?}", self.state_file))?;

        Ok(state)
    }

    /// Save current state to file
    pub fn save(
        &self,
        revisions: &[Revision],
        closed_prs: &[(u32, String)],
        local_bookmarks: Option<&HashSet<String>>,
    ) -> Result<()> {
        let mut state = self.load()?;

        state.last_run = Some(Local::now());

        // Save PR state
        state.prs.clear();
        for rev in revisions {
            if let Some(pr_url) = &rev.pr_url {
                state.prs.insert(
                    rev.change_id.clone(),
                    PrInfo {
                        pr_number: rev.extract_pr_number().unwrap_or(0),
                        pr_url: pr_url.clone(),
                        branch_name: rev.branch_name.clone().unwrap_or_default(),
                        commit_id: rev.commit_id.clone(),
                        description: rev.description.clone(),
                        last_seen: Local::now(),
                    },
                );
            }
        }

        // Track closed PRs
        if !closed_prs.is_empty() {
            for (pr_num, branch_name) in closed_prs {
                state.closed_prs_map.insert(
                    branch_name.clone(),
                    ClosedPrInfo {
                        pr_number: *pr_num,
                        closed_at: Local::now(),
                        reason: "squashed".to_string(),
                    },
                );
            }
        }

        // Save bookmarks
        if let Some(bookmarks) = local_bookmarks {
            state.bookmarks = bookmarks.clone();
        }

        self.write_state(&state)
    }

    /// Remove a closed PR entry after it has been reopened
    pub fn remove_closed_pr(&self, branch_name: &str) -> Result<()> {
        let mut state = self.load()?;

        if state.closed_prs_map.remove(branch_name).is_some() {
            self.write_state(&state)?;
        }

        Ok(())
    }

    fn write_state(&self, state: &State) -> Result<()> {
        let contents = serde_json::to_string_pretty(state).context("Failed to serialize state")?;

        fs::write(&self.state_file, contents)
            .with_context(|| format!("Failed to write state file: {:?}", self.state_file))
    }

    /// Get bookmarks that existed in the last run but don't exist now
    pub fn get_disappeared_bookmarks(
        &self,
        current_bookmarks: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        let state = self.load()?;
        let previous_bookmarks = &state.bookmarks;

        let disappeared: HashSet<String> = previous_bookmarks
            .difference(current_bookmarks)
            .filter(|b| b.starts_with(PUSH_BRANCH_PREFIX) || b.starts_with(CHANGES_BRANCH_PREFIX))
            .cloned()
            .collect();

        Ok(disappeared)
    }
}
