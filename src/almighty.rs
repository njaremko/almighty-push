use crate::command::CommandExecutor;
use crate::constants::DEFAULT_BASE_BRANCH;
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
}

impl AlmightyPush {
    /// Create a new AlmightyPush instance
    pub fn new(
        executor: CommandExecutor,
        jj: JujutsuClient,
        github: GitHubClient,
        state: StateManager,
    ) -> Self {
        Self {
            executor,
            jj,
            github,
            state,
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

        eprintln!("\nPushing {} revisions to GitHub...", revisions.len());

        let existing_branches = self.github.get_existing_branches(false)?;

        // Categorize revisions
        let (to_create, to_update) = self.categorize_revisions(revisions, &existing_branches)?;

        // Check for PRs to reopen
        let mut updated_to_update = to_update;
        self.check_pr_reopening(revisions, &existing_branches, &mut updated_to_update)?;

        // Combine the lists back for pushing
        let mut all_revisions = Vec::new();
        all_revisions.extend(to_create);
        all_revisions.extend(updated_to_update);

        // Push branches
        self.jj.push_revisions(&mut all_revisions)?;

        // Copy updated branch names back to original revisions
        for (i, rev) in all_revisions.iter().enumerate() {
            if let Some(branch_name) = &rev.branch_name {
                revisions[i].branch_name = Some(branch_name.clone());
            }
        }

        // Print summary
        self.print_push_summary(&all_revisions)?;

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
                eprintln!(
                    "  Found existing branch {}: {}",
                    rev.short_change_id(),
                    branch_found
                );
            } else {
                to_create.push(rev.clone());
                eprintln!("  Creating branch for {}", rev.short_change_id());
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
        for branch_name in existing_branches.keys() {
            for n in &[8, 12] {
                let len = revision.change_id.len().min(*n);
                if branch_name.contains(&revision.change_id[..len]) {
                    return Some(branch_name.clone());
                }
            }
        }
        None
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
                for n in &[8, 12] {
                    let len = rev.change_id.len().min(*n);
                    if branch_name.contains(&rev.change_id[..len]) {
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
        }

        Ok(())
    }

    /// Print summary of push operations
    fn print_push_summary(&self, revisions: &[Revision]) -> Result<()> {
        if revisions.is_empty() {
            return Ok(());
        }

        let (created, updated): (Vec<_>, Vec<_>) = revisions
            .iter()
            .partition(|r| r.branch_name.is_some() && !r.has_pr());

        let created_count = created.len();
        let updated_count = updated.len();
        let total = created_count + updated_count;

        if total > 0 {
            if created_count > 0 && updated_count > 0 {
                eprintln!(
                    "  Created {} branches, updated {}",
                    created_count, updated_count
                );
            } else if created_count > 0 {
                eprintln!("  Created {} new branches", created_count);
            } else {
                eprintln!("  Updated {} existing branches", updated_count);
            }
        }

        Ok(())
    }

    /// Create or update GitHub pull requests for all revisions
    pub fn create_pull_requests(&mut self, revisions: &mut [Revision]) -> Result<()> {
        if revisions.is_empty() {
            return Ok(());
        }

        eprintln!("\nCreating pull requests...");

        match self.github.repo_spec() {
            Ok(repo_spec) => {
                eprintln!("  Repository: {}", repo_spec);
            }
            Err(e) => {
                eprintln!("  warning: {}", e);
                eprintln!("  Cannot create PRs without repository information");
                return Ok(());
            }
        }

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
                eprintln!(
                    "  warning: cannot create PR for {}: no branch",
                    revisions[i].short_change_id()
                );
                continue;
            }

            // Clone the revisions list to avoid borrowing issues
            let all_revisions = revisions.to_vec();
            self.github
                .create_pull_request(&mut revisions[i], &base_branch, i, &all_revisions)?;
        }

        // Print summary
        let created_count = revisions.iter().filter(|r| r.pr_url.is_some()).count();
        if created_count > 0 {
            eprintln!("  Created/updated {} PRs", created_count);
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
            eprintln!("\nwarning: PR stack verification found issues:");
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
}
