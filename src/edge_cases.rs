use crate::command::CommandExecutor;
use crate::types::{PrInfo, Revision, State};
use anyhow::Result;
use chrono::{Duration, Local};
use std::collections::{HashMap, HashSet};

/// Handles edge cases and recovery scenarios for jj operations
pub struct EdgeCaseHandler {
    executor: CommandExecutor,
}

impl EdgeCaseHandler {
    pub fn new(executor: CommandExecutor) -> Self {
        Self { executor }
    }

    /// Detect commits that were squashed by examining jj op log and evolution history
    pub fn detect_squashed_commits(&self, state: &State) -> Result<SquashDetectionResult> {
        let mut result = SquashDetectionResult::default();

        // Get operation history with more detail
        let output = self.executor.run_unchecked(&[
            "jj",
            "op",
            "log",
            "--limit",
            "100",
            "--no-graph",
            "--template",
            r#"id.short() ++ "|" ++ description ++ "|" ++ time.start().ago() ++ "\n""#,
        ])?;

        if !output.success() {
            return Ok(result);
        }

        // Parse operations to find squash/abandon/rebase events
        for line in output.stdout.lines() {
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() < 2 {
                continue;
            }

            let description = parts[1].to_lowercase();

            // Detect various squash patterns
            if description.contains("squash") {
                result.squash_operations.push(description.clone());
                self.extract_affected_changes(&description, &mut result.potentially_squashed)?;
            }

            // Detect abandon operations
            if description.contains("abandon") {
                result.abandon_operations.push(description.clone());
                self.extract_affected_changes(&description, &mut result.potentially_abandoned)?;
            }

            // Detect rebase operations that might affect PR stack
            if description.contains("rebase") && !description.contains("auto-rebase") {
                result.rebase_operations.push(description.clone());
            }
        }

        // Cross-reference with current PRs to find orphans
        for pr in &state.prs {
            if result.potentially_squashed.contains(&pr.change_id)
                || result.potentially_abandoned.contains(&pr.change_id)
            {
                result
                    .orphaned_prs
                    .insert(pr.pr_number, pr.branch_name.clone());
            }
        }

        Ok(result)
    }

    /// Extract change IDs from operation descriptions
    fn extract_affected_changes(
        &self,
        description: &str,
        target: &mut HashSet<String>,
    ) -> Result<()> {
        // Look for patterns like "squash rlvkpnrz into kmnopqrs"
        let patterns = [
            r"\b([klmnopqrstuvwxyz]{8,32})\b", // jj change IDs
            r"change\s+([a-z0-9]{8,})",
            r"revision\s+([a-z0-9]{8,})",
        ];

        for pattern_str in patterns {
            let pattern = regex::Regex::new(pattern_str)?;
            for cap in pattern.captures_iter(description) {
                if let Some(change_id) = cap.get(1) {
                    let id = change_id.as_str();
                    // Validate it looks like a change ID
                    if id.len() >= 8
                        && id
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
                    {
                        target.insert(id.to_string());
                    }
                }
            }
        }

        Ok(())
    }

    /// Analyze commit evolution to detect splits and merges
    pub fn analyze_commit_evolution(&self, revisions: &[Revision]) -> Result<EvolutionAnalysis> {
        let mut analysis = EvolutionAnalysis::default();

        // Build a map of change IDs to revisions for quick lookup
        let _change_map: HashMap<String, &Revision> =
            revisions.iter().map(|r| (r.change_id.clone(), r)).collect();

        // Get evolution information for each revision
        for rev in revisions {
            let evolution = self.get_revision_evolution(&rev.change_id)?;

            // Detect splits (one change became multiple)
            if evolution.successors.len() > 1 {
                analysis
                    .split_commits
                    .insert(rev.change_id.clone(), evolution.successors.clone());
            }

            // Detect merges (multiple changes became one)
            if evolution.predecessors.len() > 1 {
                analysis
                    .merged_commits
                    .insert(rev.change_id.clone(), evolution.predecessors.clone());
            }

            // Track rewritten commits
            if !evolution.predecessors.is_empty() && evolution.predecessors[0] != rev.change_id {
                analysis
                    .rewritten_commits
                    .insert(evolution.predecessors[0].clone(), rev.change_id.clone());
            }
        }

        Ok(analysis)
    }

    /// Get evolution information for a specific revision
    fn get_revision_evolution(&self, change_id: &str) -> Result<RevisionEvolution> {
        let mut evolution = RevisionEvolution::default();

        // Try to get predecessor information
        let pred_output = self.executor.run_unchecked(&[
            "jj",
            "log",
            "-r",
            change_id,
            "--no-graph",
            "--template",
            r#"predecessors().map(|p| p.change_id()).join(",")"#,
        ])?;

        if pred_output.success() && !pred_output.stdout.trim().is_empty() {
            evolution.predecessors = pred_output
                .stdout
                .trim()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
        }

        // Note: Proper split detection would require analyzing successors,
        // but jj doesn't expose this information directly. The obslog shows
        // the evolution history of the same change, not actual splits.
        // For now, we'll skip successor detection to avoid false positives.
        evolution.successors = Vec::new();

        Ok(evolution)
    }

    /// Detect and handle reordered commits in the stack
    pub fn detect_reordered_commits(
        &self,
        current_revisions: &[Revision],
        state: &State,
    ) -> Result<ReorderDetection> {
        let mut detection = ReorderDetection::default();

        // Build maps for quick lookup
        let current_positions: HashMap<String, usize> = current_revisions
            .iter()
            .enumerate()
            .map(|(i, r)| (r.change_id.clone(), i))
            .collect();

        let previous_positions: HashMap<String, usize> = state
            .prs
            .iter()
            .enumerate()
            .map(|(i, pr)| (pr.change_id.clone(), i))
            .collect();

        // Find commits that exist in both but have different positions
        for (change_id, current_pos) in &current_positions {
            if let Some(&prev_pos) = previous_positions.get(change_id) {
                if current_pos != &prev_pos {
                    detection.reordered_commits.insert(
                        change_id.clone(),
                        ReorderInfo {
                            old_position: prev_pos,
                            new_position: *current_pos,
                            requires_pr_update: true,
                        },
                    );
                }
            }
        }

        // Determine which PRs need their base branch updated
        for rev in current_revisions {
            if let Some(reorder_info) = detection.reordered_commits.get(&rev.change_id) {
                if reorder_info.requires_pr_update {
                    if let Some(pr) = state.prs.iter().find(|p| p.change_id == rev.change_id) {
                        detection
                            .prs_needing_rebase
                            .insert(pr.pr_number, pr.branch_name.clone());
                    }
                }
            }
        }

        Ok(detection)
    }

    /// Validate state consistency and suggest repairs
    pub fn validate_state_consistency(
        &self,
        state: &State,
        current_revisions: &[Revision],
    ) -> Result<StateValidation> {
        let mut validation = StateValidation::default();

        // Check for PRs without corresponding revisions
        let current_change_ids: HashSet<String> = current_revisions
            .iter()
            .map(|r| r.change_id.clone())
            .collect();

        for pr in &state.prs {
            if !current_change_ids.contains(&pr.change_id) {
                validation.orphaned_pr_entries.push(pr.clone());
            }
        }

        // Check for duplicate PR entries
        let mut seen_change_ids = HashSet::new();
        for pr in &state.prs {
            if !seen_change_ids.insert(pr.change_id.clone()) {
                validation.duplicate_entries.push(pr.change_id.clone());
            }
        }

        // Check for stale closed PRs (older than 30 days)
        let cutoff = Local::now() - Duration::days(30);
        for closed_pr in &state.closed_prs {
            if closed_pr.closed_at < cutoff {
                validation
                    .stale_closed_prs
                    .push(closed_pr.branch_name.clone());
            }
        }

        // Check for inconsistent branch names
        for pr in &state.prs {
            if !self.validate_branch_name(&pr.branch_name, &pr.change_id) {
                validation
                    .inconsistent_branches
                    .push((pr.branch_name.clone(), pr.change_id.clone()));
            }
        }

        Ok(validation)
    }

    /// Validate that a branch name matches the expected pattern for a change ID
    fn validate_branch_name(&self, branch_name: &str, change_id: &str) -> bool {
        // Check if branch contains the change ID (various lengths)
        for len in [6, 8, 12] {
            let truncated = &change_id[..len.min(change_id.len())];
            if branch_name.contains(truncated) {
                return true;
            }
        }
        false
    }

    /// Attempt to recover from detected issues
    pub fn recover_from_issues(
        &self,
        validation: &StateValidation,
        detection: &ReorderDetection,
    ) -> Result<RecoveryPlan> {
        let mut plan = RecoveryPlan::default();

        // Plan to remove orphaned PR entries
        for pr in &validation.orphaned_pr_entries {
            plan.remove_pr_entries.push(pr.change_id.clone());
        }

        // Plan to update PR bases for reordered commits
        for (pr_number, branch_name) in &detection.prs_needing_rebase {
            plan.update_pr_bases.insert(*pr_number, branch_name.clone());
        }

        // Plan to clean up stale closed PRs
        plan.clean_stale_closed = validation.stale_closed_prs.clone();

        // Plan to fix inconsistent branches
        for (branch, change_id) in &validation.inconsistent_branches {
            let expected_branch = format!("push-{}", &change_id[..12.min(change_id.len())]);
            plan.rename_branches.insert(branch.clone(), expected_branch);
        }

        Ok(plan)
    }
}

/// Result of squash detection analysis
#[derive(Debug, Default)]
pub struct SquashDetectionResult {
    pub squash_operations: Vec<String>,
    pub abandon_operations: Vec<String>,
    pub rebase_operations: Vec<String>,
    pub potentially_squashed: HashSet<String>,
    pub potentially_abandoned: HashSet<String>,
    pub orphaned_prs: HashMap<u32, String>, // PR number -> branch name
}

/// Analysis of commit evolution (splits, merges, rewrites)
#[derive(Debug, Default)]
pub struct EvolutionAnalysis {
    pub split_commits: HashMap<String, Vec<String>>, // original -> successors
    pub merged_commits: HashMap<String, Vec<String>>, // result -> predecessors
    pub rewritten_commits: HashMap<String, String>,  // old -> new change ID
}

/// Information about revision evolution
#[derive(Debug, Default)]
struct RevisionEvolution {
    predecessors: Vec<String>,
    successors: Vec<String>,
}

/// Detection of reordered commits
#[derive(Debug, Default)]
pub struct ReorderDetection {
    pub reordered_commits: HashMap<String, ReorderInfo>,
    pub prs_needing_rebase: HashMap<u32, String>, // PR number -> branch name
}

#[derive(Debug)]
pub struct ReorderInfo {
    pub old_position: usize,
    pub new_position: usize,
    pub requires_pr_update: bool,
}

/// State validation results
#[derive(Debug, Default)]
pub struct StateValidation {
    pub orphaned_pr_entries: Vec<PrInfo>,
    pub duplicate_entries: Vec<String>,
    pub stale_closed_prs: Vec<String>,
    pub inconsistent_branches: Vec<(String, String)>, // (branch_name, change_id)
}

/// Recovery plan for detected issues
#[derive(Debug, Default)]
pub struct RecoveryPlan {
    pub remove_pr_entries: Vec<String>,
    pub update_pr_bases: HashMap<u32, String>,
    pub clean_stale_closed: Vec<String>,
    pub rename_branches: HashMap<String, String>, // old -> new
}
