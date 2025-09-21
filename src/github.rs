use crate::command::CommandExecutor;
use crate::constants::{CHANGES_BRANCH_PREFIX, DEFAULT_REMOTE, PUSH_BRANCH_PREFIX};
use crate::jj::JujutsuClient;
use crate::state::StateManager;
use crate::types::{GithubPr, PrInfo, Revision};
use anyhow::{Context, Result};
use regex::Regex;
use std::collections::{HashMap, HashSet};

/// Handles all GitHub operations via gh CLI
pub struct GitHubClient {
    executor: CommandExecutor,
    state_manager: StateManager,
    repo_info: Option<(String, String)>,         // (owner, repo)
    pr_cache: Option<HashMap<String, GithubPr>>, // Cache of PRs by branch name
}

impl GitHubClient {
    /// Create a new GitHubClient
    pub fn new(executor: CommandExecutor, state_manager: StateManager) -> Self {
        Self {
            executor,
            state_manager,
            repo_info: None,
            pr_cache: None,
        }
    }

    /// Get repository spec in owner/repo format
    pub fn repo_spec(&mut self) -> Result<String> {
        let (owner, repo) = self.get_repo_info()?;
        Ok(format!("{}/{}", owner, repo))
    }

    /// Get GitHub repository owner and name from remote
    pub fn get_repo_info(&mut self) -> Result<(String, String)> {
        if let Some(info) = &self.repo_info {
            return Ok(info.clone());
        }

        // Use jj git remote list to get the URL
        let list_output = self
            .executor
            .run_unchecked(&["jj", "git", "remote", "list"])?;

        let url = if list_output.success() && !list_output.stdout.is_empty() {
            let mut found_url = None;
            for line in list_output.stdout.lines() {
                if line.starts_with(DEFAULT_REMOTE) {
                    let parts: Vec<&str> = line.split_whitespace().collect();
                    if parts.len() > 1 {
                        found_url = Some(parts[1].to_string());
                        break;
                    }
                }
            }
            found_url.context(format!("Could not find {} remote", DEFAULT_REMOTE))?
        } else {
            anyhow::bail!("Could not determine GitHub repository from remote");
        };

        // Parse GitHub URL
        let re = Regex::new(r"github\.com[:/]([^/]+)/([^/\s]+?)(?:\.git)?$")?;
        let caps = re
            .captures(&url)
            .with_context(|| format!("Could not parse GitHub repository from URL: {}", url))?;

        let owner = caps.get(1).unwrap().as_str().to_string();
        let repo = caps.get(2).unwrap().as_str().to_string();

        self.repo_info = Some((owner.clone(), repo.clone()));
        Ok((owner, repo))
    }

    /// Load all managed PRs into cache
    pub fn load_pr_cache(&mut self) -> Result<()> {
        if self.pr_cache.is_some() {
            return Ok(());
        }

        let repo_spec = match self.repo_spec() {
            Ok(spec) => spec,
            Err(_) => {
                self.pr_cache = Some(HashMap::new());
                return Ok(());
            }
        };

        let mut pr_map = HashMap::new();

        // Fetch all PR states to detect merged and closed PRs
        for state in &["open", "closed", "merged"] {
            let output = self.executor.run_unchecked(&[
                "gh",
                "pr",
                "list",
                "--repo",
                &repo_spec,
                "--state",
                state,
                "--json",
                "number,headRefName,title,state,url,baseRefName",
                "--limit",
                "200", // Reasonable limit for managed PRs
            ])?;

            if output.success() {
                if let Ok(prs) = serde_json::from_str::<Vec<GithubPr>>(&output.stdout) {
                    for pr in prs {
                        // Only cache PRs with our managed branch prefixes
                        if Self::is_managed_branch(&pr.head_ref_name) {
                            if let Some(change_id) =
                                self.extract_change_id_from_branch(&pr.head_ref_name)
                            {
                                if *state == "merged" {
                                    // Mark merged PRs permanently
                                    self.state_manager.mark_pr_as_merged(&change_id)?;
                                } else if *state == "closed" {
                                    // Mark closed PRs permanently
                                    self.state_manager.mark_pr_as_closed(&change_id)?;
                                }
                            }
                            // Cache all PRs for reference
                            pr_map.insert(pr.head_ref_name.clone(), pr);
                        }
                    }
                }
            }
        }

        self.pr_cache = Some(pr_map);
        Ok(())
    }

    /// Get existing branches from GitHub that match our patterns
    pub fn get_existing_branches(&mut self, verbose: bool) -> Result<HashMap<String, String>> {
        let cmd_result = match self.get_repo_info() {
            Ok((owner, repo)) => self.executor.run_unchecked(&[
                "gh",
                "api",
                &format!("repos/{}/{}/branches", owner, repo),
                "--paginate",
                "-q",
                ".[].name",
            ]),
            Err(_) => {
                // Fallback to repo view
                self.executor.run_unchecked(&[
                    "gh",
                    "repo",
                    "view",
                    "--json",
                    "defaultBranchRef,refs",
                    "-q",
                    ".refs.nodes[].name",
                ])
            }
        };

        let output = cmd_result?;

        if !output.success() {
            if verbose {
                eprintln!("  warning: could not fetch existing branches from GitHub");
                eprintln!("           (OK if repo is private or not authenticated)");
            }
            return Ok(HashMap::new());
        }

        let branches = output
            .stdout
            .lines()
            .filter_map(|branch| {
                let branch = branch.trim();
                if branch.starts_with(PUSH_BRANCH_PREFIX)
                    || branch.starts_with(CHANGES_BRANCH_PREFIX)
                {
                    Some((branch.to_string(), branch.to_string()))
                } else {
                    None
                }
            })
            .collect();

        Ok(branches)
    }

    /// Check if a PR was previously closed and reopen it if needed
    pub fn reopen_pr_if_needed(&mut self, branch_name: &str) -> Result<bool> {
        let state = self.state_manager.load()?;

        let pr_info = match state.get_closed_pr(branch_name) {
            Some(info) => info,
            None => return Ok(false),
        };
        let pr_number = pr_info.pr_number;

        // Check PR state
        let repo_spec = self.repo_spec()?;
        let output = self.executor.run_unchecked(&[
            "gh",
            "pr",
            "view",
            &pr_number.to_string(),
            "--repo",
            &repo_spec,
            "--json",
            "state",
        ])?;

        if !output.success() {
            return Ok(false);
        }

        let pr_data: GithubPr = serde_json::from_str(&output.stdout)?;
        if pr_data.state != "CLOSED" {
            return Ok(false);
        }

        eprintln!("  Reopening PR #{} for {}", pr_number, branch_name);

        // Reopen PR
        let reopen_output = self.executor.run_unchecked(&[
            "gh",
            "pr",
            "reopen",
            &pr_number.to_string(),
            "--repo",
            &repo_spec,
        ])?;

        if reopen_output.success() {
            if self.executor.verbose {
                eprintln!("    Reopened PR #{}", pr_number);
            }

            // Add comment
            let comment = "This PR was automatically reopened because the commit has been separated back out in the stack.";
            self.executor.run_unchecked(&[
                "gh",
                "pr",
                "comment",
                &pr_number.to_string(),
                "--repo",
                &repo_spec,
                "--body",
                comment,
            ])?;

            // Update state
            self.state_manager.remove_closed_pr(branch_name)?;

            return Ok(true);
        }

        Ok(false)
    }

    /// Close PRs whose branches no longer exist in jj (e.g., were squashed)
    pub fn close_orphaned_prs(
        &mut self,
        current_revisions: &[Revision],
        jj_client: &JujutsuClient,
        existing_branches: Option<&HashMap<String, String>>,
        delete_branches: bool,
    ) -> Result<Vec<(u32, String)>> {
        // Check if we can get repo info
        if self.repo_spec().is_err() {
            return Ok(Vec::new());
        }

        let existing_branches_map = match existing_branches {
            Some(branches) => branches.clone(),
            None => self.get_existing_branches(false)?,
        };

        let local_bookmarks = jj_client.get_local_bookmarks()?;
        let disappeared_bookmarks = self
            .state_manager
            .get_disappeared_bookmarks(&local_bookmarks)?;
        let squashed_commits = jj_client.get_recently_squashed_commits()?;
        let bookmarks_on_same_commit = jj_client.get_bookmarks_on_same_commit()?;

        let state = self.state_manager.load()?;

        // Build a map of change_id -> PrInfo for quick lookups
        let previous_prs: HashMap<String, &PrInfo> = state
            .prs
            .iter()
            .map(|pr| (pr.change_id.clone(), pr))
            .collect();

        // Get all branches we've ever tracked from state
        let mut tracked_branches: HashSet<String> =
            state.prs.iter().map(|pr| pr.branch_name.clone()).collect();

        // Also include branches from closed PRs
        for closed_pr in &state.closed_prs {
            tracked_branches.insert(closed_pr.branch_name.clone());
        }

        // And include branches from current bookmarks state
        for branch in &state.bookmarks {
            if Self::is_managed_branch(branch) {
                tracked_branches.insert(branch.clone());
            }
        }

        // Get all open PRs from GitHub
        let repo_spec = self.repo_spec()?;
        let output = self.executor.run_unchecked(&[
            "gh",
            "pr",
            "list",
            "--repo",
            &repo_spec,
            "--state",
            "open",
            "--json",
            "number,headRefName,title",
            "--limit",
            "100",
        ])?;

        if !output.success() {
            eprintln!("  warning: could not fetch open PRs from GitHub");
            return Ok(Vec::new());
        }

        let prs: Vec<GithubPr> =
            serde_json::from_str(&output.stdout).context("Could not parse PR list from GitHub")?;

        // Filter to managed PRs that are actually open
        let managed_prs: Vec<GithubPr> = prs
            .into_iter()
            .filter(|pr| {
                (pr.head_ref_name.starts_with(PUSH_BRANCH_PREFIX)
                    || pr.head_ref_name.starts_with(CHANGES_BRANCH_PREFIX))
                    && (pr.state.is_empty() || pr.state.to_uppercase() == "OPEN")
            })
            .collect();

        let active_branches: HashSet<String> = current_revisions
            .iter()
            .filter_map(|rev| rev.branch_name.clone())
            .collect();

        let active_change_ids: HashSet<String> = current_revisions
            .iter()
            .map(|rev| rev.change_id.clone())
            .collect();

        let mut orphaned_prs = Vec::new();
        let mut branches_to_delete = Vec::new();

        // Handle bookmarks squashed into same commit
        let squashed_into_same = self.handle_squashed_bookmarks(
            &bookmarks_on_same_commit,
            &managed_prs,
            &active_branches,
            &existing_branches_map,
            &mut orphaned_prs,
            &mut branches_to_delete,
            &tracked_branches,
        )?;

        let context = OrphanedPrContext {
            disappeared_bookmarks: &disappeared_bookmarks,
            squashed_commits: &squashed_commits,
            previous_prs: &previous_prs,
            active_change_ids: &active_change_ids,
            local_bookmarks: &local_bookmarks,
            active_branches: &active_branches,
        };

        // Check for other orphaned PRs
        for pr in &managed_prs {
            let branch_name = &pr.head_ref_name;

            if squashed_into_same.contains(branch_name) {
                continue;
            }

            let change_id = self.extract_change_id_from_branch(branch_name);

            if let Some(reason) = Self::should_close_pr(branch_name, change_id.as_deref(), &context)
            {
                orphaned_prs.push((pr.clone(), reason));
                // Only delete branches that we've tracked
                if tracked_branches.contains(branch_name) {
                    branches_to_delete.push(branch_name.clone());
                }
            }
        }

        self.handle_merged_pr_bookmarks(
            jj_client,
            &previous_prs,
            &local_bookmarks,
            delete_branches,
        )?;

        if orphaned_prs.is_empty() {
            if !branches_to_delete.is_empty() {
                eprintln!(
                    "  No PRs to close, but found {} orphaned branches we created",
                    branches_to_delete.len()
                );
                for branch in &branches_to_delete {
                    eprintln!("    - {}", branch);
                }
                if delete_branches {
                    // Delete orphaned bookmarks even when there are no PRs to close
                    eprintln!("\n  Deleting orphaned bookmarks we created...");
                    if jj_client.delete_local_bookmarks(&branches_to_delete)? {
                        // Push all deletions to remote using --deleted
                        jj_client.push_deleted_bookmarks()?;
                    }
                } else {
                    eprintln!("    (use --delete-branches to remove)");
                }
            }
            return Ok(Vec::new());
        }

        eprintln!("  Found {} orphaned PRs to close:", orphaned_prs.len());
        let closed_pr_info = self.close_prs(&orphaned_prs)?;

        if !branches_to_delete.is_empty() && delete_branches {
            // Delete local orphan bookmarks first
            eprintln!("\n  Deleting orphaned bookmarks we created...");
            if jj_client.delete_local_bookmarks(&branches_to_delete)? {
                // Push all deletions to remote using --deleted
                jj_client.push_deleted_bookmarks()?;
            }
        } else if !branches_to_delete.is_empty() {
            eprintln!("\n  Not deleting orphaned bookmarks (use --delete-branches)");
            for branch in &branches_to_delete {
                eprintln!("    Keeping bookmark: {}", branch);
            }
        }

        Ok(closed_pr_info)
    }

    fn handle_merged_pr_bookmarks(
        &mut self,
        jj_client: &JujutsuClient,
        previous_prs: &HashMap<String, &crate::types::PrInfo>,
        local_bookmarks: &HashSet<String>,
        delete_branches: bool,
    ) -> Result<()> {
        if previous_prs.is_empty() || local_bookmarks.is_empty() {
            return Ok(());
        }

        let merged_prs = self.get_managed_prs_by_state("merged")?;
        if merged_prs.is_empty() {
            return Ok(());
        }

        // Only consider branches we've tracked in our state
        let managed_branches: HashSet<String> = previous_prs
            .values()
            .map(|info| info.branch_name.clone())
            .collect();

        if managed_branches.is_empty() {
            return Ok(());
        }

        let mut seen_branches = HashSet::new();
        let mut merged_bookmarks = Vec::new();

        for pr in merged_prs {
            let branch_name = pr.head_ref_name;

            // Only process branches we've previously tracked
            if !managed_branches.contains(&branch_name) {
                continue;
            }

            // And that still exist locally
            if !local_bookmarks.contains(&branch_name) {
                continue;
            }

            if !seen_branches.insert(branch_name.clone()) {
                continue;
            }

            merged_bookmarks.push((pr.number, branch_name));
        }

        if merged_bookmarks.is_empty() {
            return Ok(());
        }

        eprintln!(
            "\n  Found {} merged PR{} with local bookmarks:",
            merged_bookmarks.len(),
            if merged_bookmarks.len() == 1 { "" } else { "s" }
        );

        for (pr_number, branch_name) in &merged_bookmarks {
            eprintln!("    PR #{} ({})", pr_number, branch_name);
        }

        if delete_branches {
            eprintln!("  Deleting merged PR bookmarks...");
            let bookmarks_to_delete: Vec<String> = merged_bookmarks
                .iter()
                .map(|(_, branch)| branch.clone())
                .collect();

            if jj_client.delete_local_bookmarks(&bookmarks_to_delete)? {
                eprintln!("  Pushing bookmark deletions to remote...");
                jj_client.push_deleted_bookmarks()?;
                eprintln!(
                    "  Deleted {} merged PR bookmark{}",
                    bookmarks_to_delete.len(),
                    if bookmarks_to_delete.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                );
            }
        } else {
            eprintln!("    (use --delete-branches to remove merged bookmarks)");
        }

        Ok(())
    }

    fn get_managed_prs_by_state(&mut self, state: &str) -> Result<Vec<GithubPr>> {
        let repo_spec = match self.repo_spec() {
            Ok(spec) => spec,
            Err(_) => return Ok(Vec::new()),
        };

        let output = self.executor.run_unchecked(&[
            "gh",
            "pr",
            "list",
            "--repo",
            &repo_spec,
            "--state",
            state,
            "--json",
            "number,headRefName,title,state",
            "--limit",
            "100",
        ])?;

        if !output.success() {
            eprintln!("  warning: could not fetch {} PRs from GitHub", state);
            return Ok(Vec::new());
        }

        let prs: Vec<GithubPr> =
            serde_json::from_str(&output.stdout).context("Could not parse PR list from GitHub")?;

        Ok(prs
            .into_iter()
            .filter(|pr| Self::is_managed_branch(&pr.head_ref_name))
            .collect())
    }

    fn is_managed_branch(branch_name: &str) -> bool {
        branch_name.starts_with(PUSH_BRANCH_PREFIX)
            || branch_name.starts_with(CHANGES_BRANCH_PREFIX)
    }

    /// Handle bookmarks that were squashed into the same commit
    #[allow(clippy::too_many_arguments)]
    fn handle_squashed_bookmarks(
        &self,
        bookmarks_on_same_commit: &HashMap<String, Vec<String>>,
        prs: &[GithubPr],
        active_branches: &HashSet<String>,
        existing_branches: &HashMap<String, String>,
        orphaned_prs: &mut Vec<(GithubPr, String)>,
        branches_to_delete: &mut Vec<String>,
        tracked_branches: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        let mut squashed_into_same = HashSet::new();

        for (_commit_id, bookmarks) in bookmarks_on_same_commit {
            if bookmarks.len() <= 1 {
                continue;
            }

            let mut pr_numbers_for_bookmarks = Vec::new();
            for bookmark in bookmarks {
                let clean_bookmark = bookmark.trim_end_matches('*');
                for pr in prs {
                    if pr.head_ref_name == clean_bookmark {
                        pr_numbers_for_bookmarks.push((
                            pr.number,
                            clean_bookmark.to_string(),
                            pr.clone(),
                        ));
                        break;
                    }
                }
            }

            if pr_numbers_for_bookmarks.len() > 1 {
                pr_numbers_for_bookmarks.sort_by_key(|x| x.0);

                eprintln!(
                    "  Found {} bookmarks on commit {}",
                    bookmarks.len(),
                    _commit_id
                );
                eprintln!(
                    "    Keeping PR #{}, closing duplicates",
                    pr_numbers_for_bookmarks[0].0
                );

                for (_pr_num, bookmark, pr) in pr_numbers_for_bookmarks.into_iter().skip(1) {
                    orphaned_prs.push((pr, "squashed into same commit as earlier PR".to_string()));
                    // Only delete branches that we've tracked
                    if tracked_branches.contains(&bookmark) {
                        branches_to_delete.push(bookmark.clone());
                    }
                    squashed_into_same.insert(bookmark);
                }
            }

            // Check for orphaned branches without PRs
            for bookmark in bookmarks {
                let clean_bookmark = bookmark.trim_end_matches('*');
                if existing_branches.contains_key(clean_bookmark)
                    && !active_branches.contains(clean_bookmark)
                    && !squashed_into_same.contains(clean_bookmark)
                    && tracked_branches.contains(clean_bookmark)
                // Only delete branches we've tracked
                {
                    branches_to_delete.push(clean_bookmark.to_string());
                }
            }
        }

        Ok(squashed_into_same)
    }

    /// Extract change ID from branch name
    fn extract_change_id_from_branch(&self, branch_name: &str) -> Option<String> {
        if let Some(stripped) = branch_name.strip_prefix(PUSH_BRANCH_PREFIX) {
            Some(stripped.to_string())
        } else {
            branch_name
                .strip_prefix(CHANGES_BRANCH_PREFIX)
                .map(|stripped| stripped.to_string())
        }
    }

    /// Determine if a PR should be closed and why
    fn should_close_pr(
        branch_name: &str,
        change_id: Option<&str>,
        context: &OrphanedPrContext<'_>,
    ) -> Option<String> {
        if context.disappeared_bookmarks.contains(branch_name) {
            return Some("bookmark was deleted (likely squashed or abandoned)".to_string());
        }

        let change_id = change_id?;

        if context.squashed_commits.contains(change_id) {
            return Some("squashed or abandoned according to operation log".to_string());
        }

        if context.previous_prs.contains_key(change_id)
            && !context.active_change_ids.contains(change_id)
        {
            return Some("no longer in the current stack".to_string());
        }

        if !context.local_bookmarks.contains(branch_name)
            && !context.active_branches.contains(branch_name)
            && !context.active_change_ids.contains(change_id)
        {
            return Some("removed from the stack".to_string());
        }

        None
    }

    /// Close the given PRs with explanatory comments
    fn close_prs(&mut self, orphaned_prs: &[(GithubPr, String)]) -> Result<Vec<(u32, String)>> {
        let mut closed_pr_info = Vec::new();
        let repo_spec = self.repo_spec()?;

        for (pr, reason) in orphaned_prs {
            let pr_number = pr.number;
            let branch_name = &pr.head_ref_name;
            let title = &pr.title;

            eprintln!("    Closing PR #{} ({}): {}", pr_number, branch_name, title);
            eprintln!("      Reason: {}", reason);

            // Add comment
            let comment = format!(
                "This PR was automatically closed because the corresponding commits were {}.",
                reason
            );
            self.executor.run_unchecked(&[
                "gh",
                "pr",
                "comment",
                &pr_number.to_string(),
                "--repo",
                &repo_spec,
                "--body",
                &comment,
            ])?;

            // Close PR
            let output = self.executor.run_unchecked(&[
                "gh",
                "pr",
                "close",
                &pr_number.to_string(),
                "--repo",
                &repo_spec,
            ])?;

            if output.success() {
                eprintln!("      Closed PR #{}", pr_number);
                closed_pr_info.push((pr_number, branch_name.clone()));
            } else {
                eprintln!("      error: failed to close PR #{}", pr_number);
                if !output.stderr.is_empty() {
                    eprintln!("             {}", output.stderr);
                }
            }
        }

        Ok(closed_pr_info)
    }

    /// Create or update a pull request for a revision
    pub fn create_pull_request(
        &mut self,
        revision: &mut Revision,
        base_branch: &str,
        stack_position: usize,
        all_revisions: &[Revision],
    ) -> Result<bool> {
        if revision.branch_name.is_none() {
            eprintln!(
                "  warning: skipping {}: no branch name",
                revision.short_change_id()
            );
            return Ok(false);
        }

        let branch_name = revision.branch_name.as_ref().unwrap();

        // Check if this change ID has a merged or closed PR (permanent skip)
        let state = self.state_manager.load()?;
        if state.merged_pr_change_ids.contains(&revision.change_id) {
            // This change has a merged PR, skip it
            revision.pr_state = Some(crate::types::PrState::Merged);
            if self.executor.verbose {
                eprintln!(
                    "  Skipping {} - already has merged PR",
                    revision.short_change_id()
                );
            }
            return Ok(true);
        }

        if state.closed_pr_change_ids.contains(&revision.change_id) {
            // This change has a closed PR, skip it
            revision.pr_state = Some(crate::types::PrState::Closed);
            if self.executor.verbose {
                eprintln!(
                    "  Skipping {} - already has closed PR",
                    revision.short_change_id()
                );
            }
            return Ok(true);
        }

        // Check cache first
        let existing_pr = if let Some(cache) = &self.pr_cache {
            cache.get(branch_name).cloned()
        } else {
            // Fallback to individual lookup if cache not loaded
            self.get_existing_pr(branch_name)?
        };

        // Check if PR already exists
        if let Some(existing_pr) = existing_pr {
            // Check PR state - default to "open" if empty
            let pr_state = if existing_pr.state.is_empty() {
                "open"
            } else {
                &existing_pr.state.to_lowercase()
            };

            // Set PR state in revision
            revision.pr_state = match pr_state {
                "merged" => {
                    // Save to state that this change ID has a merged PR
                    self.state_manager.mark_pr_as_merged(&revision.change_id)?;
                    Some(crate::types::PrState::Merged)
                }
                "closed" => {
                    // Save to state that this change ID has a closed PR
                    self.state_manager.mark_pr_as_closed(&revision.change_id)?;
                    Some(crate::types::PrState::Closed)
                }
                _ => Some(crate::types::PrState::Open),
            };

            // Skip updating merged/closed PRs
            if pr_state == "merged" || pr_state == "closed" {
                revision.pr_url = Some(existing_pr.url);
                revision.pr_number = Some(existing_pr.number);
                return Ok(true);
            }

            // Only update base if PR is open
            if let Some(current_base) = existing_pr.base_ref_name {
                if current_base != base_branch {
                    self.update_pr_base(branch_name, base_branch)?;
                }
            }

            revision.pr_url = Some(existing_pr.url);
            revision.pr_number = Some(existing_pr.number);
            return Ok(true);
        }

        // Create new PR
        let title = &revision.description;
        let body = self.build_pr_body(revision, stack_position, all_revisions);
        let repo_spec = self.repo_spec()?;

        let output = self.executor.run_unchecked(&[
            "gh",
            "pr",
            "create",
            "--repo",
            &repo_spec,
            "--head",
            branch_name,
            "--base",
            base_branch,
            "--title",
            title,
            "--body",
            &body,
        ])?;

        if output.success() {
            let pr_url = output.stdout.trim().to_string();
            revision.pr_url = Some(pr_url.clone());
            revision.pr_state = Some(crate::types::PrState::Open);
            eprintln!("  Created PR for {}", revision.short_change_id());
            Ok(true)
        } else {
            eprintln!(
                "  error: failed to create PR for {}",
                revision.short_change_id()
            );
            if !output.stderr.is_empty() {
                eprintln!("         {}", output.stderr);
            }
            Ok(false)
        }
    }

    /// Check if a PR already exists for the given branch
    pub fn get_existing_pr(&mut self, branch_name: &str) -> Result<Option<GithubPr>> {
        let repo_spec = self.repo_spec()?;
        let output = self.executor.run_unchecked(&[
            "gh",
            "pr",
            "view",
            branch_name,
            "--repo",
            &repo_spec,
            "--json",
            "url,baseRefName,headRefName,number,state",
        ])?;

        if output.success() {
            match serde_json::from_str(&output.stdout) {
                Ok(pr) => Ok(Some(pr)),
                Err(e) => {
                    eprintln!(
                        "error: could not parse PR from GitHub: {} {}",
                        e, output.stdout
                    );
                    Err(anyhow::anyhow!("Could not parse PR from GitHub"))
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Update the base branch of a PR
    pub fn update_pr_base(&mut self, branch_name: &str, new_base: &str) -> Result<()> {
        let repo_spec = self.repo_spec()?;
        let output = self.executor.run_unchecked(&[
            "gh",
            "pr",
            "edit",
            branch_name,
            "--repo",
            &repo_spec,
            "--base",
            new_base,
        ])?;

        if !output.success() {
            eprintln!("    warning: failed to update PR base to {}", new_base);
            if !output.stderr.is_empty() {
                eprintln!("             {}", output.stderr);
            }
        } else {
            eprintln!("    Successfully updated PR base to {}", new_base);
        }

        Ok(())
    }

    /// Update multiple PR bases for reordered commits
    pub fn update_pr_bases_for_reorder(
        &mut self,
        revisions: &[Revision],
        pr_updates: &HashMap<u32, String>,
    ) -> Result<()> {
        if pr_updates.is_empty() {
            return Ok(());
        }

        eprintln!("\nUpdating PR base branches for reordered commits...");

        for (pr_number, branch_name) in pr_updates {
            // Find the revision for this PR
            let revision_idx = revisions
                .iter()
                .position(|r| r.pr_number == Some(*pr_number));

            if let Some(idx) = revision_idx {
                let new_base = if idx == 0 {
                    crate::constants::DEFAULT_BASE_BRANCH.to_string()
                } else {
                    revisions[idx - 1]
                        .branch_name
                        .clone()
                        .unwrap_or_else(|| crate::constants::DEFAULT_BASE_BRANCH.to_string())
                };

                self.update_pr_base(branch_name, &new_base)?;
            }
        }

        Ok(())
    }

    /// Enhanced orphaned PR detection with better squash/abandon detection
    #[allow(dead_code)]
    pub fn detect_orphaned_prs_enhanced(
        &mut self,
        current_revisions: &[Revision],
        jj: &JujutsuClient,
    ) -> Result<Vec<(u32, String, String)>> {
        let mut orphaned = Vec::new();

        // Get all our managed branches from GitHub
        let existing_branches = self.get_existing_branches(false)?;

        // Get recently squashed/abandoned commits
        let squashed_commits = jj.get_recently_squashed_commits()?;

        // Build set of current change IDs
        let current_change_ids: HashSet<String> = current_revisions
            .iter()
            .map(|r| r.change_id.clone())
            .collect();

        // Check each existing branch
        for (branch_name, _) in existing_branches {
            // Extract change ID from branch name
            let change_id = self.extract_change_id_from_branch(&branch_name);

            if let Some(change_id) = change_id {
                // Check if this change still exists
                let is_orphaned = !current_change_ids.contains(&change_id)
                    || squashed_commits.iter().any(|s| change_id.starts_with(s));

                if is_orphaned {
                    // Get PR info
                    if let Some(pr) = self.get_existing_pr(&branch_name)? {
                        let reason = if squashed_commits.iter().any(|s| change_id.starts_with(s)) {
                            "squashed or abandoned".to_string()
                        } else {
                            "commit no longer in stack".to_string()
                        };

                        orphaned.push((pr.number, branch_name.clone(), reason));
                    }
                }
            }
        }

        Ok(orphaned)
    }

    /// Build the PR body with stack information
    fn build_pr_body(
        &self,
        revision: &Revision,
        position: usize,
        all_revisions: &[Revision],
    ) -> String {
        let mut body = format!("**Stack PR #{}**\n\n", position + 1);
        body.push_str("Part of stack:\n");

        for (i, rev) in all_revisions.iter().enumerate() {
            let prefix = if i == position { "→ " } else { "  " };
            body.push_str(&format!("{} {}. {}\n", prefix, i + 1, rev.description));
        }

        body.push_str(&format!("\nChange ID: `{}`\n", revision.change_id));
        body.push_str(&format!("Commit ID: `{}`\n", revision.commit_id));

        body
    }

    /// Update PR titles and bodies with stack information
    pub fn update_pr_details(&mut self, revisions: &[Revision]) -> Result<()> {
        if revisions.is_empty() || !revisions.iter().any(|r| r.pr_url.is_some()) {
            return Ok(());
        }

        let repo_spec = self.repo_spec()?;

        for (i, rev) in revisions.iter().enumerate() {
            if rev.pr_url.is_none() || rev.branch_name.is_none() {
                continue;
            }

            let branch_name = rev.branch_name.as_ref().unwrap();

            // Check if PR is open before updating
            if let Some(existing_pr) = self.get_existing_pr(branch_name)? {
                let pr_state = if existing_pr.state.is_empty() {
                    "open".to_string()
                } else {
                    existing_pr.state.to_lowercase()
                };

                if pr_state != "open" {
                    continue;
                }
            }

            let body = self.build_full_pr_body(rev, i, revisions);
            let title = &rev.description;

            let output = self.executor.run_unchecked(&[
                "gh",
                "pr",
                "edit",
                branch_name,
                "--repo",
                &repo_spec,
                "--title",
                title,
                "--body",
                &body,
            ])?;

            if output.success() {
                let pr_number = rev.extract_pr_number().unwrap_or(0);
                eprintln!("  Updated PR #{} for {}", pr_number, rev.short_change_id());
            } else {
                eprintln!(
                    "  warning: failed to update PR for {}",
                    rev.short_change_id()
                );
                if !output.stderr.is_empty() {
                    eprintln!("           {}", output.stderr);
                }
            }
        }

        Ok(())
    }

    /// Build complete PR body with stack info and full description
    fn build_full_pr_body(
        &self,
        revision: &Revision,
        position: usize,
        all_revisions: &[Revision],
    ) -> String {
        let mut body = String::new();

        // Stack section
        body.push_str("## Stack\n\n");
        for (j, r) in all_revisions.iter().enumerate() {
            if r.pr_url.is_some() {
                let marker = if j == position { "→" } else { "  " };
                let pr_number = r.extract_pr_number().unwrap_or(0);
                body.push_str(&format!(
                    "{} **#{}**: {}\n",
                    marker, pr_number, r.description
                ));
            }
        }

        // Description section
        if let Some(full_desc) = &revision.full_description {
            let lines: Vec<&str> = full_desc.lines().collect();
            if lines.len() > 1 {
                let additional_lines = lines[1..].join("\n").trim().to_string();
                if !additional_lines.is_empty() {
                    body.push_str("\n## Description\n\n");
                    body.push_str(&additional_lines);
                    body.push('\n');
                }
            }
        }

        // Metadata
        body.push_str("\n---\n");
        body.push_str(&format!("Change ID: `{}`\n", revision.change_id));
        body.push_str(&format!("Commit ID: `{}`\n", revision.commit_id));

        body
    }
}

struct OrphanedPrContext<'a> {
    disappeared_bookmarks: &'a HashSet<String>,
    squashed_commits: &'a HashSet<String>,
    previous_prs: &'a HashMap<String, &'a PrInfo>,
    active_change_ids: &'a HashSet<String>,
    local_bookmarks: &'a HashSet<String>,
    active_branches: &'a HashSet<String>,
}
