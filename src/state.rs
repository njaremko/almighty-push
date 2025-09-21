use crate::constants::{CHANGES_BRANCH_PREFIX, PUSH_BRANCH_PREFIX, STATE_FILE};
use crate::types::{ClosedPrInfo, PrInfo, Revision, State, STATE_VERSION};
use anyhow::{Context, Result};
use chrono::Local;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Manages persistent state for almighty-push
pub struct StateManager {
    state_file: PathBuf,
}

impl Default for StateManager {
    fn default() -> Self {
        Self::new()
    }
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

    /// Load state from file with conflict resolution
    pub fn load(&self) -> Result<State> {
        if !self.state_file.exists() {
            return Ok(State::default());
        }

        let contents = fs::read_to_string(&self.state_file)
            .with_context(|| format!("Failed to read state file: {:?}", self.state_file))?;

        // Check for merge conflict markers
        if self.has_merge_conflicts(&contents) {
            eprintln!("  warning: detected merge conflicts in state file, attempting recovery...");
            return self.recover_from_conflicts(&contents);
        }

        // First parse as generic JSON to check version and handle legacy format
        let json_value: Value = match serde_json::from_str(&contents) {
            Ok(val) => val,
            Err(e) => {
                eprintln!("  warning: corrupted state file, resetting: {}", e);
                self.backup_corrupted_state(&contents)?;
                return Ok(State::default());
            }
        };

        let mut state = if let Some(_version) = json_value.get("version").and_then(|v| v.as_u64()) {
            // Has version field, parse normally
            serde_json::from_value(json_value)
                .with_context(|| format!("Failed to parse state file: {:?}", self.state_file))?
        } else {
            // No version field - this is v1 format with HashMaps
            self.load_v1_state(json_value)?
        };

        // Migrate state if needed
        self.migrate_state(&mut state)?;

        // Validate and clean up state
        self.validate_and_clean_state(&mut state)?;

        Ok(state)
    }

    /// Load v1 state format (with HashMaps)
    fn load_v1_state(&self, json_value: Value) -> Result<State> {
        // Parse the old v1 format
        #[derive(serde::Deserialize)]
        struct StateV1 {
            #[serde(default)]
            last_run: Option<chrono::DateTime<chrono::Local>>,
            #[serde(default)]
            prs: HashMap<String, serde_json::Value>,
            #[serde(default)]
            closed_prs_map: HashMap<String, serde_json::Value>,
            #[serde(default)]
            bookmarks: HashSet<String>,
        }

        let v1: StateV1 =
            serde_json::from_value(json_value).context("Failed to parse v1 state format")?;

        let mut state = State {
            version: 0, // Mark as v0/v1 for migration
            last_run: v1.last_run,
            ..Default::default()
        };

        // Convert PRs - parse without change_id field first
        #[derive(serde::Deserialize)]
        struct PrInfoV1 {
            pr_number: u32,
            pr_url: String,
            branch_name: String,
            commit_id: String,
            description: String,
            last_seen: chrono::DateTime<chrono::Local>,
        }

        for (change_id, pr_value) in v1.prs {
            if let Ok(pr_v1) = serde_json::from_value::<PrInfoV1>(pr_value) {
                state.prs.push(PrInfo {
                    change_id,
                    pr_number: pr_v1.pr_number,
                    pr_url: pr_v1.pr_url,
                    branch_name: pr_v1.branch_name,
                    commit_id: pr_v1.commit_id,
                    description: pr_v1.description,
                    last_seen: pr_v1.last_seen,
                });
            }
        }

        // Convert closed PRs - parse without branch_name field first
        #[derive(serde::Deserialize)]
        struct ClosedPrInfoV1 {
            pr_number: u32,
            closed_at: chrono::DateTime<chrono::Local>,
            reason: String,
        }

        for (branch_name, closed_pr_value) in v1.closed_prs_map {
            if let Ok(closed_v1) = serde_json::from_value::<ClosedPrInfoV1>(closed_pr_value) {
                state.closed_prs.push(ClosedPrInfo {
                    branch_name,
                    pr_number: closed_v1.pr_number,
                    closed_at: closed_v1.closed_at,
                    reason: closed_v1.reason,
                });
            }
        }

        // Convert bookmarks
        state.bookmarks = v1.bookmarks.into_iter().collect();

        Ok(state)
    }

    /// Migrate state to current version if needed
    fn migrate_state(&self, state: &mut State) -> Result<()> {
        let original_version = state.version;

        // Version 0/1 -> Version 2: Convert HashMaps to Vecs
        if state.version < 2 {
            eprintln!(
                "  Migrating state file from version {} to {}",
                state.version, STATE_VERSION
            );

            // Migrate from v1 format if needed
            state.migrate_from_v1();
            state.version = 2;
        }

        // Future migrations would go here
        // if state.version < 3 {
        //     // Migrate from v2 to v3
        //     state.version = 3;
        // }

        if state.version > STATE_VERSION {
            anyhow::bail!(
                "State file version {} is newer than supported version {}. Please update almighty-push.",
                state.version,
                STATE_VERSION
            );
        }

        // Save migrated state if version changed
        if original_version != state.version {
            self.write_state(state)?;
        }

        Ok(())
    }

    /// Save current state to file
    pub fn save(
        &self,
        revisions: &[Revision],
        closed_prs: &[(u32, String)],
        local_bookmarks: Option<&HashSet<String>>,
    ) -> Result<()> {
        let mut state = self.load()?;

        // Ensure we're always saving with the current version
        state.version = STATE_VERSION;
        state.last_run = Some(Local::now());

        // Save PR state as a sorted list
        state.prs.clear();
        for rev in revisions {
            if let Some(pr_url) = &rev.pr_url {
                state.prs.push(PrInfo {
                    change_id: rev.change_id.clone(),
                    pr_number: rev.extract_pr_number().unwrap_or(0),
                    pr_url: pr_url.clone(),
                    branch_name: rev.branch_name.clone().unwrap_or_default(),
                    commit_id: rev.commit_id.clone(),
                    description: rev.description.clone(),
                    last_seen: Local::now(),
                });
            }
        }
        // Sort for consistent ordering
        state.prs.sort_by(|a, b| a.change_id.cmp(&b.change_id));

        // Track closed PRs as a sorted list
        if !closed_prs.is_empty() {
            for (pr_num, branch_name) in closed_prs {
                // Remove any existing entry for this branch
                state.closed_prs.retain(|pr| pr.branch_name != *branch_name);

                state.closed_prs.push(ClosedPrInfo {
                    branch_name: branch_name.clone(),
                    pr_number: *pr_num,
                    closed_at: Local::now(),
                    reason: "squashed".to_string(),
                });
            }
            // Sort for consistent ordering
            state
                .closed_prs
                .sort_by(|a, b| a.branch_name.cmp(&b.branch_name));
        }

        // Save bookmarks as a sorted list
        if let Some(bookmarks) = local_bookmarks {
            state.bookmarks = bookmarks.iter().cloned().collect();
            state.bookmarks.sort();
        }

        self.write_state(&state)
    }

    /// Remove a closed PR entry after it has been reopened
    pub fn remove_closed_pr(&self, branch_name: &str) -> Result<()> {
        let mut state = self.load()?;

        let original_len = state.closed_prs.len();
        state.closed_prs.retain(|pr| pr.branch_name != branch_name);

        if state.closed_prs.len() != original_len {
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
        let previous_bookmarks: HashSet<String> = state.bookmarks.into_iter().collect();

        let disappeared: HashSet<String> = previous_bookmarks
            .difference(current_bookmarks)
            .filter(|b| b.starts_with(PUSH_BRANCH_PREFIX) || b.starts_with(CHANGES_BRANCH_PREFIX))
            .cloned()
            .collect();

        Ok(disappeared)
    }

    /// Check if content has merge conflict markers
    fn has_merge_conflicts(&self, contents: &str) -> bool {
        contents.contains("<<<<<<<") || contents.contains("=======") || contents.contains(">>>>>>>")
    }

    /// Attempt to recover from merge conflicts by taking the latest valid state
    fn recover_from_conflicts(&self, contents: &str) -> Result<State> {
        // Try to extract valid JSON sections between conflict markers
        let lines: Vec<&str> = contents.lines().collect();
        let mut json_sections = Vec::new();
        let mut current_section = String::new();
        let mut in_conflict = false;

        for line in lines {
            if line.starts_with("<<<<<<<") {
                if !current_section.trim().is_empty() {
                    json_sections.push(current_section.clone());
                    current_section.clear();
                }
                in_conflict = true;
            } else if line.starts_with("=======") {
                if !current_section.trim().is_empty() {
                    json_sections.push(current_section.clone());
                    current_section.clear();
                }
            } else if line.starts_with(">>>>>>>") {
                if !current_section.trim().is_empty() {
                    json_sections.push(current_section.clone());
                    current_section.clear();
                }
                in_conflict = false;
            } else if !in_conflict {
                current_section.push_str(line);
                current_section.push('\n');
            }
        }

        if !current_section.trim().is_empty() {
            json_sections.push(current_section);
        }

        // Try to parse each section and merge them
        let mut merged_state = State::default();
        let mut found_valid = false;

        for section in json_sections {
            if let Ok(state) = serde_json::from_str::<State>(&section) {
                found_valid = true;
                // Merge states, taking the most recent data
                self.merge_states(&mut merged_state, &state);
            }
        }

        if found_valid {
            eprintln!("  Successfully recovered state from conflicts");
            // Save the recovered state
            self.write_state(&merged_state)?;
            Ok(merged_state)
        } else {
            eprintln!("  Could not recover state from conflicts, using default");
            Ok(State::default())
        }
    }

    /// Merge two states, preferring newer data
    fn merge_states(&self, target: &mut State, source: &State) {
        // Keep the latest version
        target.version = target.version.max(source.version);

        // Keep the most recent run time
        if source.last_run > target.last_run {
            target.last_run = source.last_run;
        }

        // Merge PRs, deduplicating by change_id
        let mut pr_map: HashMap<String, PrInfo> = HashMap::new();
        for pr in &target.prs {
            pr_map.insert(pr.change_id.clone(), pr.clone());
        }
        for pr in &source.prs {
            pr_map.insert(pr.change_id.clone(), pr.clone());
        }
        target.prs = pr_map.into_values().collect();
        target.prs.sort_by(|a, b| a.change_id.cmp(&b.change_id));

        // Merge closed PRs, deduplicating by branch_name
        let mut closed_map: HashMap<String, ClosedPrInfo> = HashMap::new();
        for pr in &target.closed_prs {
            closed_map.insert(pr.branch_name.clone(), pr.clone());
        }
        for pr in &source.closed_prs {
            closed_map.insert(pr.branch_name.clone(), pr.clone());
        }
        target.closed_prs = closed_map.into_values().collect();
        target
            .closed_prs
            .sort_by(|a, b| a.branch_name.cmp(&b.branch_name));

        // Merge bookmarks
        let mut bookmark_set: HashSet<String> = target.bookmarks.iter().cloned().collect();
        bookmark_set.extend(source.bookmarks.iter().cloned());
        target.bookmarks = bookmark_set.into_iter().collect();
        target.bookmarks.sort();
    }

    /// Backup corrupted state file
    fn backup_corrupted_state(&self, contents: &str) -> Result<()> {
        let backup_path = self.state_file.with_extension("corrupted.bak");
        fs::write(&backup_path, contents)
            .with_context(|| format!("Failed to backup corrupted state to {:?}", backup_path))?;
        eprintln!("  Corrupted state backed up to {:?}", backup_path);
        Ok(())
    }

    /// Validate and clean up state
    fn validate_and_clean_state(&self, state: &mut State) -> Result<()> {
        // Remove duplicate PR entries
        let mut seen = HashSet::new();
        state.prs.retain(|pr| seen.insert(pr.change_id.clone()));

        // Remove duplicate closed PR entries
        seen.clear();
        state
            .closed_prs
            .retain(|pr| seen.insert(pr.branch_name.clone()));

        // Clean up stale closed PRs (older than 30 days)
        let cutoff = chrono::Local::now() - chrono::Duration::days(30);
        state.closed_prs.retain(|pr| pr.closed_at > cutoff);

        // Sort for consistency
        state.prs.sort_by(|a, b| a.change_id.cmp(&b.change_id));
        state
            .closed_prs
            .sort_by(|a, b| a.branch_name.cmp(&b.branch_name));
        state.bookmarks.sort();

        Ok(())
    }
}
