use crate::command::CommandExecutor;
use crate::constants::{CHANGES_BRANCH_PREFIX, DEFAULT_BASE_BRANCH, PUSH_BRANCH_PREFIX};
use crate::edge_cases::{EdgeCaseHandler, RecoveryPlan};
use crate::github::GitHubClient;
use crate::jj::JujutsuClient;
use crate::state::StateManager;
use crate::types::Revision;
use anyhow::Result;
use std::collections::HashMap;

/// Main orchestrator for almighty-push operations
pub struct AlmightyPush {
    pub executor: CommandExecutor,
    jj: JujutsuClient,
    github: GitHubClient,
    state: StateManager,
    edge_handler: EdgeCaseHandler,
}

impl AlmightyPush {
    /// Create a new AlmightyPush instance
    pub fn new(
        executor: CommandExecutor,
        jj: JujutsuClient,
        github: GitHubClient,
        state: StateManager,
    ) -> Self {
        let edge_handler = EdgeCaseHandler::new(executor.clone());
        Self {
            executor,
            jj,
            github,
            state,
            edge_handler,
        }
    }

    /// Push all revisions to GitHub and return existing branches
    pub fn push_revisions(
        &mut self,
        revisions: &mut [Revision],
    ) -> Result<HashMap<String, String>> {
        if revisions.is_empty() {
            return Ok(HashMap::new());
        }

        eprintln!(
            "\nPushing {} revision{} to GitHub...",
            revisions.len(),
            if revisions.len() == 1 { "" } else { "s" }
        );

        let existing_branches = self.github.get_existing_branches(false)?;

        // Categorize revisions
        let (to_create, to_update) = self.categorize_revisions(revisions, &existing_branches)?;
        let created_count = to_create.len();
        let updated_count = to_update.len();

        // Check for PRs to reopen
        let mut updated_to_update = to_update;
        self.check_pr_reopening(revisions, &existing_branches, &mut updated_to_update)?;

        // Combine the lists back for pushing
        let mut all_revisions = Vec::new();
        all_revisions.extend(to_create);
        all_revisions.extend(updated_to_update);

        // Push branches
        self.jj.push_revisions(&mut all_revisions)?;

        // Copy updated branch names back to original revisions using change-id lookup
        let branch_map: HashMap<_, _> = all_revisions
            .iter()
            .filter_map(|rev| {
                rev.branch_name
                    .as_ref()
                    .map(|branch| (rev.change_id.clone(), branch.clone()))
            })
            .collect();

        for rev in revisions.iter_mut() {
            if let Some(branch_name) = branch_map.get(&rev.change_id) {
                rev.branch_name = Some(branch_name.clone());
            }
        }

        // Print summary
        self.print_push_summary(created_count, updated_count)?;

        Ok(existing_branches)
    }

    /// Separate revisions into those needing new branches vs updates
    fn categorize_revisions(
        &self,
        revisions: &[Revision],
        existing_branches: &HashMap<String, String>,
    ) -> Result<(Vec<Revision>, Vec<Revision>)> {
        let mut to_create = Vec::new();
        let mut to_update = Vec::new();

        for rev in revisions {
            if let Some(branch_found) = self.find_existing_branch(rev, existing_branches) {
                let mut updated_rev = rev.clone();
                updated_rev.branch_name = Some(branch_found.clone());
                to_update.push(updated_rev);
                if self.executor.verbose {
                    eprintln!(
                        "  -> Found existing branch {}: {}",
                        rev.short_change_id(),
                        branch_found
                    );
                }
            } else {
                to_create.push(rev.clone());
                if self.executor.verbose {
                    eprintln!("  -> Creating branch for {}", rev.short_change_id());
                }
            }
        }

        Ok((to_create, to_update))
    }

    /// Find an existing branch for a revision
    fn find_existing_branch(
        &self,
        revision: &Revision,
        existing_branches: &HashMap<String, String>,
    ) -> Option<String> {
        existing_branches
            .keys()
            .find(|branch_name| Self::branch_matches_change(branch_name, &revision.change_id))
            .cloned()
    }

    /// Check if any PRs need to be reopened
    fn check_pr_reopening(
        &mut self,
        revisions: &[Revision],
        existing_branches: &HashMap<String, String>,
        to_update: &mut Vec<Revision>,
    ) -> Result<()> {
        if self.github.repo_spec().is_err() {
            return Ok(());
        }

        for rev in revisions {
            for branch_name in existing_branches.keys() {
                if Self::branch_matches_change(branch_name, &rev.change_id) {
                    if self.github.reopen_pr_if_needed(branch_name)? {
                        // Add to update list if not already there
                        if !to_update.iter().any(|r| r.change_id == rev.change_id) {
                            let mut updated_rev = rev.clone();
                            updated_rev.branch_name = Some(branch_name.clone());
                            to_update.push(updated_rev);
                        }
                    }
                    break;
                }
            }
        }

        Ok(())
    }

    /// Check whether a managed branch corresponds to the given change id
    fn branch_matches_change(branch_name: &str, change_id: &str) -> bool {
        let prefixes = [PUSH_BRANCH_PREFIX, CHANGES_BRANCH_PREFIX];

        for prefix in prefixes {
            if let Some(stripped) = branch_name.strip_prefix(prefix) {
                let len = stripped.len().min(change_id.len());
                return stripped[..len].eq_ignore_ascii_case(&change_id[..len]);
            }
        }

        false
    }

    /// Print summary of push operations
    fn print_push_summary(&self, created_count: usize, updated_count: usize) -> Result<()> {
        if created_count == 0 && updated_count == 0 {
            return Ok(());
        }

        if created_count > 0 && updated_count > 0 {
            eprintln!(
                "  Pushed: {} new, {} updated branches",
                created_count, updated_count
            );
        } else if created_count > 0 {
            eprintln!(
                "  Pushed: {} new branch{}",
                created_count,
                if created_count == 1 { "" } else { "es" }
            );
        } else {
            eprintln!(
                "  Pushed: {} updated branch{}",
                updated_count,
                if updated_count == 1 { "" } else { "es" }
            );
        }

        Ok(())
    }

    /// Create or update GitHub pull requests for all revisions
    pub fn create_pull_requests(&mut self, revisions: &mut [Revision]) -> Result<()> {
        if revisions.is_empty() {
            return Ok(());
        }

        eprintln!("\nManaging pull requests...");

        match self.github.repo_spec() {
            Ok(repo_spec) => {
                if self.executor.verbose {
                    eprintln!("  Repository: {}", repo_spec);
                }
            }
            Err(e) => {
                eprintln!("  warning: {}", e);
                eprintln!("  Cannot create PRs without repository information");
                return Ok(());
            }
        }

        // Load PR cache to efficiently check for existing PRs
        self.github.load_pr_cache()?;

        // Check for PRs to reopen
        for rev in revisions.iter() {
            if let Some(branch_name) = &rev.branch_name {
                self.github.reopen_pr_if_needed(branch_name)?;
            }
        }

        // Create/update PRs
        for i in 0..revisions.len() {
            let base_branch = if i == 0 {
                DEFAULT_BASE_BRANCH.to_string()
            } else {
                revisions[i - 1]
                    .branch_name
                    .clone()
                    .unwrap_or_else(|| DEFAULT_BASE_BRANCH.to_string())
            };

            if revisions[i].branch_name.is_none() {
                if self.executor.verbose {
                    eprintln!(
                        "  warning: cannot create PR for {}: no branch",
                        revisions[i].short_change_id()
                    );
                }
                continue;
            }

            // Track if this revision already had a PR
            let had_pr = revisions[i].pr_url.is_some();

            // Clone the revisions list to avoid borrowing issues
            let all_revisions = revisions.to_vec();
            self.github
                .create_pull_request(&mut revisions[i], &base_branch, i, &all_revisions)?;

            // Count created vs updated
            if revisions[i].pr_url.is_some() {
                if had_pr {
                    eprintln!(
                        "  -> Updated PR #{}: {}",
                        revisions[i].pr_number.unwrap_or(0),
                        revisions[i].description
                    );
                } else {
                    eprintln!(
                        "  -> Created PR #{}: {}",
                        revisions[i].pr_number.unwrap_or(0),
                        revisions[i].description
                    );
                }
                // Always show the PR URL
                if let Some(pr_url) = &revisions[i].pr_url {
                    eprintln!("     {}", pr_url);
                }
            }
        }

        // Print summary
        let pr_count = revisions.iter().filter(|r| r.pr_url.is_some()).count();
        if pr_count > 0 {
            eprintln!(
                "  Processed {} PR{}",
                pr_count,
                if pr_count == 1 { "" } else { "s" }
            );
        }

        Ok(())
    }

    /// Close PRs for commits that were squashed or removed
    pub fn close_orphaned_prs(
        &mut self,
        revisions: &[Revision],
        existing_branches: Option<&HashMap<String, String>>,
        delete_branches: bool,
    ) -> Result<Vec<(u32, String)>> {
        self.github
            .close_orphaned_prs(revisions, &self.jj, existing_branches, delete_branches)
    }

    /// Update PR titles and bodies with stack information
    pub fn update_pr_details(&mut self, revisions: &[Revision]) -> Result<()> {
        self.github.update_pr_details(revisions)
    }

    /// Verify that PR base branches are correct
    pub fn verify_pr_bases(&mut self, revisions: &[Revision]) -> Result<()> {
        let mut issues = Vec::new();

        for i in 0..revisions.len() {
            if revisions[i].pr_url.is_none() || revisions[i].branch_name.is_none() {
                continue;
            }

            // Skip verification for merged/closed PRs
            if let Some(crate::types::PrState::Merged | crate::types::PrState::Closed) =
                &revisions[i].pr_state
            {
                continue;
            }

            let expected_base = if i == 0 {
                DEFAULT_BASE_BRANCH.to_string()
            } else {
                revisions[i - 1]
                    .branch_name
                    .clone()
                    .unwrap_or_else(|| DEFAULT_BASE_BRANCH.to_string())
            };

            let branch_name = revisions[i].branch_name.as_ref().unwrap();

            // Check the actual PR base (only for open PRs)
            if let Some(existing_pr) = self.github.get_existing_pr(branch_name)? {
                // Skip verification for closed/merged PRs (default to "open" if empty)
                let pr_state = if existing_pr.state.is_empty() {
                    "open".to_string()
                } else {
                    existing_pr.state.to_lowercase()
                };

                if pr_state != "open" {
                    continue;
                }

                if let Some(current_base) = existing_pr.base_ref_name {
                    if current_base != expected_base {
                        issues.push(format!(
                            "{} has incorrect base: {} (expected {})",
                            revisions[i].short_change_id(),
                            current_base,
                            expected_base
                        ));
                    }
                }
            }
        }

        if !issues.is_empty() {
            eprintln!("\nWarning: PR stack verification issues:");
            for issue in &issues {
                eprintln!("  - {}", issue);
            }
        }

        Ok(())
    }

    /// Save the current state
    pub fn save_state(&self, revisions: &[Revision], closed_prs: &[(u32, String)]) -> Result<()> {
        let local_bookmarks = self.jj.get_local_bookmarks()?;
        self.state
            .save(revisions, closed_prs, Some(&local_bookmarks))
    }

    /// Apply recovery plan actions
    pub fn apply_recovery_plan(
        &mut self,
        recovery_plan: &RecoveryPlan,
        revisions: &[Revision],
    ) -> Result<()> {
        if !recovery_plan.update_pr_bases.is_empty() {
            eprintln!("\nApplying recovery plan PR base updates...");
            self.github
                .update_pr_bases_for_reorder(revisions, &recovery_plan.update_pr_bases)?;
        }
        Ok(())
    }

    /// Detect and handle edge cases before processing
    pub fn detect_and_handle_edge_cases(&mut self, revisions: &[Revision]) -> Result<RecoveryPlan> {
        let state = self.state.load()?;

        // Detect squashed/abandoned commits
        let squash_detection = self.edge_handler.detect_squashed_commits(&state)?;
        if !squash_detection.orphaned_prs.is_empty() && self.executor.verbose {
            eprintln!(
                "\nDetected {} orphaned PRs from squashed/abandoned commits",
                squash_detection.orphaned_prs.len()
            );
            for (pr_num, branch) in &squash_detection.orphaned_prs {
                eprintln!("  - PR #{} (branch: {})", pr_num, branch);
            }
        }

        // Analyze commit evolution (splits/merges) - splits are now disabled to avoid false positives
        let _evolution = self.edge_handler.analyze_commit_evolution(revisions)?;

        // Detect reordered commits
        let reorder_detection = self
            .edge_handler
            .detect_reordered_commits(revisions, &state)?;
        if !reorder_detection.reordered_commits.is_empty() && self.executor.verbose {
            eprintln!(
                "\nDetected {} reordered commits:",
                reorder_detection.reordered_commits.len()
            );
            for (change_id, info) in &reorder_detection.reordered_commits {
                eprintln!(
                    "  - {} moved from position {} to {}",
                    &change_id[..8.min(change_id.len())],
                    info.old_position,
                    info.new_position
                );
            }
        }

        // Validate state consistency
        let validation = self
            .edge_handler
            .validate_state_consistency(&state, revisions)?;
        if !validation.orphaned_pr_entries.is_empty() && self.executor.verbose {
            eprintln!(
                "\nFound {} orphaned PR entries in state",
                validation.orphaned_pr_entries.len()
            );
        }

        // Generate recovery plan
        let recovery_plan = self
            .edge_handler
            .recover_from_issues(&validation, &reorder_detection)?;

        // Execute recovery actions if needed
        if !recovery_plan.update_pr_bases.is_empty() && self.executor.verbose {
            eprintln!("\nUpdating PR base branches for reordered commits...");
            for pr_num in recovery_plan.update_pr_bases.keys() {
                eprintln!("  - Will update base for PR #{}", pr_num);
            }
        }

        Ok(recovery_plan)
    }
}
