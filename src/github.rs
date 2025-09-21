use crate::command::CommandExecutor;
use crate::constants::{CHANGES_BRANCH_PREFIX, DEFAULT_REMOTE, PUSH_BRANCH_PREFIX};
use crate::jj::JujutsuClient;
use crate::state::StateManager;
use crate::types::{GithubPr, PrInfo, PrState, Revision, State};
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashMap, HashSet};

// Constants for better maintainability
const PR_LIST_LIMIT: &str = "200";
const PR_MERGED_MARKER: &str = " ✓";
const PR_CLOSED_MARKER: &str = " ✗";
const PR_NO_PR_MARKER: &str = " (no PR)";
const STACK_PR_ARROW: &str = "→";

// Lazy static regex for GitHub URL parsing
static GITHUB_URL_REGEX: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"github\.com[:/]([^/]+)/([^/\s]+?)(?:\.git)?$").expect("Invalid GitHub URL regex")
});

/// Repository information
#[derive(Clone, Debug)]
struct RepoInfo {
    owner: String,
    repo: String,
}

impl RepoInfo {
    fn spec(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// Cache for GitHub PR data
#[derive(Default)]
struct PrCache {
    prs_by_branch: HashMap<String, GithubPr>,
    loaded: bool,
}

/// Handles all GitHub operations via gh CLI
pub struct GitHubClient {
    executor: CommandExecutor,
    state_manager: StateManager,
    repo_info: Option<RepoInfo>,
    pr_cache: PrCache,
}

impl GitHubClient {
    /// Create a new GitHubClient
    pub fn new(executor: CommandExecutor, state_manager: StateManager) -> Self {
        Self {
            executor,
            state_manager,
            repo_info: None,
            pr_cache: PrCache::default(),
        }
    }

    /// Get repository spec in owner/repo format
    pub fn repo_spec(&mut self) -> Result<String> {
        self.ensure_repo_info()?;
        Ok(self.repo_info.as_ref().unwrap().spec())
    }

    /// Ensure repo info is loaded
    fn ensure_repo_info(&mut self) -> Result<()> {
        if self.repo_info.is_none() {
            self.load_repo_info()?;
        }
        Ok(())
    }

    /// Get repository info (for compatibility)
    pub fn get_repo_info(&mut self) -> Result<(String, String)> {
        self.ensure_repo_info()?;
        let info = self.repo_info.as_ref().unwrap();
        Ok((info.owner.clone(), info.repo.clone()))
    }

    /// Load GitHub repository owner and name from remote
    fn load_repo_info(&mut self) -> Result<()> {
        let url = self.fetch_remote_url()?;
        let (owner, repo) = self.parse_github_url(&url)?;

        self.repo_info = Some(RepoInfo { owner, repo });
        Ok(())
    }

    /// Fetch the remote URL from jj
    fn fetch_remote_url(&self) -> Result<String> {
        let output = self
            .executor
            .run_unchecked(&["jj", "git", "remote", "list"])?;

        if !output.success() || output.stdout.is_empty() {
            anyhow::bail!("Could not determine GitHub repository from remote");
        }

        output
            .stdout
            .lines()
            .find(|line| line.starts_with(DEFAULT_REMOTE))
            .and_then(|line| line.split_whitespace().nth(1))
            .map(String::from)
            .context(format!("Could not find {} remote", DEFAULT_REMOTE))
    }

    /// Parse GitHub URL to extract owner and repo
    fn parse_github_url(&self, url: &str) -> Result<(String, String)> {
        let caps = GITHUB_URL_REGEX
            .captures(url)
            .with_context(|| format!("Could not parse GitHub repository from URL: {}", url))?;

        let owner = caps[1].to_string();
        let repo = caps[2].to_string();

        Ok((owner, repo))
    }

    /// Load all managed PRs into cache
    pub fn load_pr_cache(&mut self) -> Result<()> {
        if self.pr_cache.loaded {
            return Ok(());
        }

        let repo_spec = match self.repo_spec() {
            Ok(spec) => spec,
            Err(_) => {
                self.pr_cache.loaded = true;
                return Ok(());
            }
        };

        // Fetch all PR states
        for state in &["open", "closed", "merged"] {
            self.fetch_and_cache_prs_by_state(&repo_spec, state)?;
        }

        self.pr_cache.loaded = true;
        Ok(())
    }

    /// Fetch and cache PRs for a specific state
    fn fetch_and_cache_prs_by_state(&mut self, repo_spec: &str, state: &str) -> Result<()> {
        let output = self.executor.run_unchecked(&[
            "gh",
            "pr",
            "list",
            "--repo",
            repo_spec,
            "--state",
            state,
            "--json",
            "number,headRefName,title,state,url,baseRefName",
            "--limit",
            PR_LIST_LIMIT,
        ])?;

        if !output.success() {
            return Ok(());
        }

        let prs: Vec<GithubPr> =
            serde_json::from_str(&output.stdout).unwrap_or_else(|_| Vec::new());

        for pr in prs {
            if !Self::is_managed_branch(&pr.head_ref_name) {
                continue;
            }

            // Update state tracking if needed
            if let Some(change_id) = self.extract_change_id_from_branch(&pr.head_ref_name) {
                match state {
                    "merged" => self.state_manager.mark_pr_as_merged(&change_id)?,
                    "closed" => self.state_manager.mark_pr_as_closed(&change_id)?,
                    _ => {}
                }
            }

            self.pr_cache
                .prs_by_branch
                .insert(pr.head_ref_name.clone(), pr);
        }

        Ok(())
    }

    /// Get existing branches from GitHub that match our patterns
    pub fn get_existing_branches(&mut self, verbose: bool) -> Result<HashMap<String, String>> {
        let output = self.fetch_branches_from_github()?;

        if !output.success() {
            if verbose {
                eprintln!("  warning: could not fetch existing branches from GitHub");
                eprintln!("           (OK if repo is private or not authenticated)");
            }
            return Ok(HashMap::new());
        }

        Ok(output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|branch| Self::is_managed_branch(branch))
            .map(|branch| (branch.to_string(), branch.to_string()))
            .collect())
    }

    /// Fetch branches from GitHub using API or fallback
    fn fetch_branches_from_github(&mut self) -> Result<crate::command::CommandOutput> {
        match self.get_repo_info() {
            Ok((owner, repo)) => self.executor.run_unchecked(&[
                "gh",
                "api",
                &format!("repos/{}/{}/branches", owner, repo),
                "--paginate",
                "-q",
                ".[].name",
            ]),
            Err(_) => self.executor.run_unchecked(&[
                "gh",
                "repo",
                "view",
                "--json",
                "defaultBranchRef,refs",
                "-q",
                ".refs.nodes[].name",
            ]),
        }
    }

    /// Populate PR states for all revisions in the stack
    pub fn populate_pr_states(&mut self, revisions: &mut [Revision]) -> Result<()> {
        self.load_pr_cache()?;

        for rev in revisions.iter_mut() {
            if let Some(branch_name) = &rev.branch_name {
                if let Some(pr) = self.pr_cache.prs_by_branch.get(branch_name) {
                    rev.pr_state = Some(Self::parse_pr_state(&pr.state));
                    rev.pr_url = Some(pr.url.clone());
                    rev.pr_number = Some(pr.number);
                }
            }
        }

        Ok(())
    }

    /// Parse PR state string into enum
    fn parse_pr_state(state: &str) -> PrState {
        match state.to_lowercase().as_str() {
            "merged" => PrState::Merged,
            "closed" => PrState::Closed,
            "open" | "" => PrState::Open,
            _ => PrState::Open,
        }
    }

    /// Check if a PR was previously closed and reopen it if needed
    pub fn reopen_pr_if_needed(&mut self, branch_name: &str) -> Result<bool> {
        let state = self.state_manager.load()?;

        let pr_info = match state.get_closed_pr(branch_name) {
            Some(info) => info,
            None => return Ok(false),
        };

        if !self.is_pr_closed(pr_info.pr_number)? {
            return Ok(false);
        }

        eprintln!("  Reopening PR #{} for {}", pr_info.pr_number, branch_name);

        if self.reopen_pr(pr_info.pr_number, branch_name)? {
            self.add_pr_comment(
                pr_info.pr_number,
                "This PR was automatically reopened because the commit has been separated back out in the stack."
            )?;
            self.state_manager.remove_closed_pr(branch_name)?;
            return Ok(true);
        }

        Ok(false)
    }

    /// Check if a PR is in closed state
    fn is_pr_closed(&mut self, pr_number: u32) -> Result<bool> {
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
        Ok(pr_data.state == "CLOSED")
    }

    /// Reopen a closed PR
    fn reopen_pr(&mut self, pr_number: u32, _branch_name: &str) -> Result<bool> {
        let repo_spec = self.repo_spec()?;
        let output = self.executor.run_unchecked(&[
            "gh",
            "pr",
            "reopen",
            &pr_number.to_string(),
            "--repo",
            &repo_spec,
        ])?;

        if output.success() && self.executor.verbose {
            eprintln!("    Reopened PR #{}", pr_number);
        }

        Ok(output.success())
    }

    /// Add a comment to a PR
    fn add_pr_comment(&mut self, pr_number: u32, comment: &str) -> Result<()> {
        let repo_spec = self.repo_spec()?;
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
        Ok(())
    }

    /// Close PRs whose branches no longer exist in jj (e.g., were squashed)
    pub fn close_orphaned_prs(
        &mut self,
        current_revisions: &[Revision],
        jj_client: &JujutsuClient,
        existing_branches: Option<&HashMap<String, String>>,
        delete_branches: bool,
    ) -> Result<Vec<(u32, String)>> {
        // Early return if we can't get repo info
        if self.repo_spec().is_err() {
            return Ok(Vec::new());
        }

        // Gather all necessary data
        let cleanup_data =
            self.gather_cleanup_data(current_revisions, jj_client, existing_branches)?;

        // Find orphaned PRs and branches
        let (orphaned_prs, branches_to_delete) =
            self.identify_orphaned_items(&cleanup_data, current_revisions)?;

        // Handle merged PR bookmarks
        self.handle_merged_pr_bookmarks(
            jj_client,
            &cleanup_data.previous_prs,
            &cleanup_data.local_bookmarks,
            delete_branches,
        )?;

        // Handle cleanup based on what we found
        if orphaned_prs.is_empty() {
            return self.handle_orphaned_branches_only(
                &branches_to_delete,
                jj_client,
                delete_branches,
            );
        }

        // Close orphaned PRs
        eprintln!("  Found {} orphaned PRs to close:", orphaned_prs.len());
        let closed_pr_info = self.close_prs(&orphaned_prs)?;

        // Clean up branches if requested
        self.cleanup_orphaned_branches(&branches_to_delete, jj_client, delete_branches)?;

        Ok(closed_pr_info)
    }

    /// Gather all data needed for cleanup operations
    fn gather_cleanup_data(
        &mut self,
        _current_revisions: &[Revision],
        jj_client: &JujutsuClient,
        existing_branches: Option<&HashMap<String, String>>,
    ) -> Result<CleanupData> {
        let existing_branches_map = existing_branches
            .cloned()
            .unwrap_or_else(|| self.get_existing_branches(false).unwrap_or_default());

        let local_bookmarks = jj_client.get_local_bookmarks()?;
        let disappeared_bookmarks = self
            .state_manager
            .get_disappeared_bookmarks(&local_bookmarks)?;
        let squashed_commits = jj_client.get_recently_squashed_commits()?;
        let bookmarks_on_same_commit = jj_client.get_bookmarks_on_same_commit()?;

        let state = self.state_manager.load()?;
        let tracked_branches = self.build_tracked_branches_set(&state);
        let managed_prs = self.fetch_open_managed_prs()?;

        let previous_prs = state
            .prs
            .into_iter()
            .map(|pr| (pr.change_id.clone(), pr))
            .collect();

        Ok(CleanupData {
            existing_branches_map,
            local_bookmarks,
            disappeared_bookmarks,
            squashed_commits,
            bookmarks_on_same_commit,
            previous_prs,
            tracked_branches,
            managed_prs,
        })
    }

    /// Build set of all branches we've ever tracked
    fn build_tracked_branches_set(&self, state: &State) -> HashSet<String> {
        let mut tracked = HashSet::new();

        // From current PRs
        tracked.extend(state.prs.iter().map(|pr| pr.branch_name.clone()));

        // From closed PRs
        tracked.extend(state.closed_prs.iter().map(|pr| pr.branch_name.clone()));

        // From bookmarks (if managed)
        tracked.extend(
            state
                .bookmarks
                .iter()
                .filter(|b| Self::is_managed_branch(b))
                .cloned(),
        );

        tracked
    }

    /// Fetch open PRs that match our managed patterns
    fn fetch_open_managed_prs(&mut self) -> Result<Vec<GithubPr>> {
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

        Ok(prs
            .into_iter()
            .filter(|pr| Self::is_managed_branch(&pr.head_ref_name) && Self::is_pr_open(&pr.state))
            .collect())
    }

    /// Check if PR state indicates it's open
    fn is_pr_open(state: &str) -> bool {
        state.is_empty() || state.eq_ignore_ascii_case("open")
    }

    /// Identify orphaned PRs and branches to delete
    fn identify_orphaned_items(
        &self,
        cleanup_data: &CleanupData,
        current_revisions: &[Revision],
    ) -> Result<(Vec<OrphanedPr>, Vec<String>)> {
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
            &cleanup_data.bookmarks_on_same_commit,
            &cleanup_data.managed_prs,
            &active_branches,
            &cleanup_data.existing_branches_map,
            &mut orphaned_prs,
            &mut branches_to_delete,
            &cleanup_data.tracked_branches,
        )?;

        let context = OrphanedPrContext {
            disappeared_bookmarks: &cleanup_data.disappeared_bookmarks,
            squashed_commits: &cleanup_data.squashed_commits,
            previous_prs: &cleanup_data.previous_prs,
            active_change_ids: &active_change_ids,
            local_bookmarks: &cleanup_data.local_bookmarks,
            active_branches: &active_branches,
        };

        // Check for other orphaned PRs
        for pr in &cleanup_data.managed_prs {
            if squashed_into_same.contains(&pr.head_ref_name) {
                continue;
            }

            let change_id = self.extract_change_id_from_branch(&pr.head_ref_name);

            if let Some(reason) =
                Self::should_close_pr(&pr.head_ref_name, change_id.as_deref(), &context)
            {
                orphaned_prs.push((pr.clone(), reason));
                if cleanup_data.tracked_branches.contains(&pr.head_ref_name) {
                    branches_to_delete.push(pr.head_ref_name.clone());
                }
            }
        }

        Ok((orphaned_prs, branches_to_delete))
    }

    /// Handle case where there are orphaned branches but no PRs to close
    fn handle_orphaned_branches_only(
        &self,
        branches_to_delete: &[String],
        jj_client: &JujutsuClient,
        delete_branches: bool,
    ) -> Result<Vec<ClosedPrInfo>> {
        if branches_to_delete.is_empty() {
            return Ok(Vec::new());
        }

        eprintln!(
            "  No PRs to close, but found {} orphaned branches we created",
            branches_to_delete.len()
        );

        for branch in branches_to_delete {
            eprintln!("    - {}", branch);
        }

        if delete_branches {
            eprintln!("\n  Deleting orphaned bookmarks we created...");
            if jj_client.delete_local_bookmarks(branches_to_delete)? {
                jj_client.push_deleted_bookmarks()?;
            }
        } else {
            eprintln!("    (use --delete-branches to remove)");
        }

        Ok(Vec::new())
    }

    /// Clean up orphaned branches if requested
    fn cleanup_orphaned_branches(
        &self,
        branches_to_delete: &[String],
        jj_client: &JujutsuClient,
        delete_branches: bool,
    ) -> Result<()> {
        if branches_to_delete.is_empty() {
            return Ok(());
        }

        if delete_branches {
            eprintln!("\n  Deleting orphaned bookmarks we created...");
            if jj_client.delete_local_bookmarks(branches_to_delete)? {
                jj_client.push_deleted_bookmarks()?;
            }
        } else {
            eprintln!("\n  Not deleting orphaned bookmarks (use --delete-branches)");
            for branch in branches_to_delete {
                eprintln!("    Keeping bookmark: {}", branch);
            }
        }

        Ok(())
    }

    /// Handle cleanup of bookmarks for merged PRs
    fn handle_merged_pr_bookmarks(
        &mut self,
        jj_client: &JujutsuClient,
        previous_prs: &HashMap<String, PrInfo>,
        local_bookmarks: &HashSet<String>,
        delete_branches: bool,
    ) -> Result<()> {
        // Early returns for empty inputs
        if previous_prs.is_empty() || local_bookmarks.is_empty() {
            return Ok(());
        }

        let merged_bookmarks = self.find_merged_pr_bookmarks(previous_prs, local_bookmarks)?;

        if merged_bookmarks.is_empty() {
            return Ok(());
        }

        self.report_merged_bookmarks(&merged_bookmarks);

        if delete_branches {
            self.delete_merged_bookmarks(&merged_bookmarks, jj_client)?;
        } else {
            eprintln!("  (use --delete-branches to remove merged bookmarks)");
        }

        Ok(())
    }

    /// Find bookmarks for merged PRs that still exist locally
    fn find_merged_pr_bookmarks(
        &mut self,
        previous_prs: &HashMap<String, PrInfo>,
        local_bookmarks: &HashSet<String>,
    ) -> Result<Vec<ClosedPrInfo>> {
        let merged_prs = self.get_managed_prs_by_state("merged")?;
        if merged_prs.is_empty() {
            return Ok(Vec::new());
        }

        // Get branches we've tracked
        let managed_branches: HashSet<String> = previous_prs
            .values()
            .map(|info| info.branch_name.clone())
            .collect();

        if managed_branches.is_empty() {
            return Ok(Vec::new());
        }

        // Find merged PRs with local bookmarks
        let mut seen = HashSet::new();
        let mut results = Vec::new();

        for pr in merged_prs {
            if managed_branches.contains(&pr.head_ref_name)
                && local_bookmarks.contains(&pr.head_ref_name)
                && seen.insert(pr.head_ref_name.clone())
            {
                results.push((pr.number, pr.head_ref_name));
            }
        }

        Ok(results)
    }

    /// Report found merged bookmarks to user
    fn report_merged_bookmarks(&self, merged_bookmarks: &[ClosedPrInfo]) {
        let count = merged_bookmarks.len();
        eprintln!(
            "\nFound {} merged PR{} with local bookmarks:",
            count,
            if count == 1 { "" } else { "s" }
        );

        for (pr_number, branch_name) in merged_bookmarks {
            eprintln!("  PR #{} ({})", pr_number, branch_name);
        }
    }

    /// Delete bookmarks for merged PRs
    fn delete_merged_bookmarks(
        &self,
        merged_bookmarks: &[ClosedPrInfo],
        jj_client: &JujutsuClient,
    ) -> Result<()> {
        eprintln!("Deleting merged PR bookmarks...");

        let bookmarks_to_delete: Vec<String> = merged_bookmarks
            .iter()
            .map(|(_, branch)| branch.clone())
            .collect();

        if jj_client.delete_local_bookmarks(&bookmarks_to_delete)? {
            eprintln!("  Pushing bookmark deletions to remote...");
            jj_client.push_deleted_bookmarks()?;

            let count = bookmarks_to_delete.len();
            eprintln!(
                "  Deleted {} merged PR bookmark{}",
                count,
                if count == 1 { "" } else { "s" }
            );
        }

        Ok(())
    }

    /// Get managed PRs by state
    fn get_managed_prs_by_state(&mut self, state: &str) -> Result<Vec<GithubPr>> {
        let repo_spec = self.repo_spec().ok().unwrap_or_default();
        if repo_spec.is_empty() {
            return Ok(Vec::new());
        }

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

        serde_json::from_str::<Vec<GithubPr>>(&output.stdout)
            .map(|prs| {
                prs.into_iter()
                    .filter(|pr| Self::is_managed_branch(&pr.head_ref_name))
                    .collect()
            })
            .or_else(|e| {
                eprintln!("  warning: could not parse PR list: {}", e);
                Ok(Vec::new())
            })
    }

    /// Check if a branch name matches our managed patterns
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
        orphaned_prs: &mut Vec<OrphanedPr>,
        branches_to_delete: &mut Vec<String>,
        tracked_branches: &HashSet<String>,
    ) -> Result<HashSet<String>> {
        let mut squashed_into_same = HashSet::new();

        for (commit_id, bookmarks) in bookmarks_on_same_commit {
            if bookmarks.len() <= 1 {
                continue;
            }

            // Process duplicate PRs on same commit
            let duplicate_context = DuplicatePrContext {
                orphaned_prs,
                branches_to_delete,
                tracked_branches,
            };
            let _duplicates_handled = self.handle_duplicate_prs_on_commit(
                commit_id,
                bookmarks,
                prs,
                duplicate_context,
                &mut squashed_into_same,
            )?;

            // Check for orphaned branches without PRs
            self.check_orphaned_branches_without_prs(
                bookmarks,
                existing_branches,
                active_branches,
                &squashed_into_same,
                tracked_branches,
                branches_to_delete,
            );
        }

        Ok(squashed_into_same)
    }

    /// Handle duplicate PRs on the same commit
    fn handle_duplicate_prs_on_commit(
        &self,
        commit_id: &str,
        bookmarks: &[String],
        prs: &[GithubPr],
        context: DuplicatePrContext<'_>,
        squashed_into_same: &mut HashSet<String>,
    ) -> Result<bool> {
        let mut prs_for_bookmarks = self.find_prs_for_bookmarks(bookmarks, prs);

        if prs_for_bookmarks.len() <= 1 {
            return Ok(false);
        }

        // Sort by PR number to keep the earliest
        prs_for_bookmarks.sort_by_key(|(pr_num, _, _)| *pr_num);

        eprintln!(
            "  Found {} bookmarks on commit {}",
            bookmarks.len(),
            commit_id
        );
        eprintln!(
            "    Keeping PR #{}, closing duplicates",
            prs_for_bookmarks[0].0
        );

        // Mark duplicates for closure
        for (_pr_num, bookmark, pr) in prs_for_bookmarks.into_iter().skip(1) {
            context.orphaned_prs.push((pr, "squashed into same commit as earlier PR".to_string()));
            if context.tracked_branches.contains(&bookmark) {
                context.branches_to_delete.push(bookmark.clone());
            }
            squashed_into_same.insert(bookmark);
        }

        Ok(true)
    }

    /// Find PRs for given bookmarks
    fn find_prs_for_bookmarks(
        &self,
        bookmarks: &[String],
        prs: &[GithubPr],
    ) -> Vec<(u32, String, GithubPr)> {
        let mut results = Vec::new();

        for bookmark in bookmarks {
            let clean_bookmark = bookmark.trim_end_matches('*');
            if let Some(pr) = prs.iter().find(|p| p.head_ref_name == clean_bookmark) {
                results.push((pr.number, clean_bookmark.to_string(), pr.clone()));
            }
        }

        results
    }

    /// Check for orphaned branches that don't have PRs
    fn check_orphaned_branches_without_prs(
        &self,
        bookmarks: &[String],
        existing_branches: &HashMap<String, String>,
        active_branches: &HashSet<String>,
        squashed_into_same: &HashSet<String>,
        tracked_branches: &HashSet<String>,
        branches_to_delete: &mut Vec<String>,
    ) {
        for bookmark in bookmarks {
            let clean_bookmark = bookmark.trim_end_matches('*');

            if existing_branches.contains_key(clean_bookmark)
                && !active_branches.contains(clean_bookmark)
                && !squashed_into_same.contains(clean_bookmark)
                && tracked_branches.contains(clean_bookmark)
            {
                branches_to_delete.push(clean_bookmark.to_string());
            }
        }
    }

    /// Extract change ID from branch name
    fn extract_change_id_from_branch(&self, branch_name: &str) -> Option<String> {
        branch_name
            .strip_prefix(PUSH_BRANCH_PREFIX)
            .or_else(|| branch_name.strip_prefix(CHANGES_BRANCH_PREFIX))
            .map(String::from)
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
    fn close_prs(&mut self, orphaned_prs: &[OrphanedPr]) -> Result<Vec<ClosedPrInfo>> {
        let mut closed_pr_info = Vec::new();
        let repo_spec = self.repo_spec()?;

        for (pr, reason) in orphaned_prs {
            if let Some(info) = self.close_single_pr(pr, reason, &repo_spec)? {
                closed_pr_info.push(info);
            }
        }

        Ok(closed_pr_info)
    }

    /// Close a single PR with comment
    fn close_single_pr(
        &mut self,
        pr: &GithubPr,
        reason: &str,
        repo_spec: &str,
    ) -> Result<Option<ClosedPrInfo>> {
        eprintln!(
            "    Closing PR #{} ({}): {}",
            pr.number, pr.head_ref_name, pr.title
        );
        eprintln!("      Reason: {}", reason);

        // Add explanatory comment
        let comment = format!(
            "This PR was automatically closed because the corresponding commits were {}.",
            reason
        );
        self.add_pr_comment(pr.number, &comment)?;

        // Close the PR
        let output = self.executor.run_unchecked(&[
            "gh",
            "pr",
            "close",
            &pr.number.to_string(),
            "--repo",
            repo_spec,
        ])?;

        if output.success() {
            eprintln!("      Closed PR #{}", pr.number);
            Ok(Some((pr.number, pr.head_ref_name.clone())))
        } else {
            eprintln!("      error: failed to close PR #{}", pr.number);
            if !output.stderr.is_empty() {
                eprintln!("             {}", output.stderr);
            }
            Ok(None)
        }
    }

    /// Create or update a pull request for a revision
    /// Returns (success, was_created) where was_created is true if a new PR was created
    pub fn create_pull_request(
        &mut self,
        revision: &mut Revision,
        base_branch: &str,
        stack_position: usize,
        all_revisions: &[Revision],
    ) -> Result<(bool, bool)> {
        // Validate prerequisites
        if !self.validate_revision_for_pr(revision)? {
            return Ok((false, false));
        }

        // Check if PR should be skipped due to state
        if self.should_skip_pr(revision)? {
            return Ok((true, false));
        }

        let branch_name = revision.branch_name.as_ref().unwrap().clone();

        // Check for existing PR
        let existing_pr = self.get_cached_or_fetch_pr(&branch_name)?;

        if let Some(existing_pr) = existing_pr {
            return self.handle_existing_pr(revision, &existing_pr, base_branch, &branch_name);
        }

        // Create new PR
        self.create_new_pr(
            revision,
            base_branch,
            stack_position,
            all_revisions,
            &branch_name,
        )
    }

    /// Validate that a revision is ready for PR creation
    fn validate_revision_for_pr(&self, revision: &Revision) -> Result<bool> {
        if revision.branch_name.is_none() {
            eprintln!(
                "  warning: skipping {}: no branch name",
                revision.short_change_id()
            );
            return Ok(false);
        }
        Ok(true)
    }

    /// Check if PR should be skipped based on state
    fn should_skip_pr(&mut self, revision: &mut Revision) -> Result<bool> {
        let state = self.state_manager.load()?;

        if state.merged_pr_change_ids.contains(&revision.change_id) {
            revision.pr_state = Some(PrState::Merged);
            if self.executor.verbose {
                eprintln!(
                    "  Skipping {} - already has merged PR",
                    revision.short_change_id()
                );
            }
            return Ok(true);
        }

        if state.closed_pr_change_ids.contains(&revision.change_id) {
            revision.pr_state = Some(PrState::Closed);
            if self.executor.verbose {
                eprintln!(
                    "  Skipping {} - already has closed PR",
                    revision.short_change_id()
                );
            }
            return Ok(true);
        }

        Ok(false)
    }

    /// Get PR from cache or fetch from GitHub
    fn get_cached_or_fetch_pr(&mut self, branch_name: &str) -> Result<Option<GithubPr>> {
        if self.pr_cache.loaded {
            Ok(self.pr_cache.prs_by_branch.get(branch_name).cloned())
        } else {
            self.get_existing_pr(branch_name)
        }
    }

    /// Handle an existing PR (update if needed)
    fn handle_existing_pr(
        &mut self,
        revision: &mut Revision,
        existing_pr: &GithubPr,
        base_branch: &str,
        branch_name: &str,
    ) -> Result<(bool, bool)> {
        let pr_state = Self::parse_pr_state(&existing_pr.state);
        revision.pr_state = Some(pr_state);

        // Update state tracking if needed
        match pr_state {
            PrState::Merged => {
                self.state_manager.mark_pr_as_merged(&revision.change_id)?;
                revision.pr_url = Some(existing_pr.url.clone());
                revision.pr_number = Some(existing_pr.number);
                return Ok((true, false));
            }
            PrState::Closed => {
                self.state_manager.mark_pr_as_closed(&revision.change_id)?;
                revision.pr_url = Some(existing_pr.url.clone());
                revision.pr_number = Some(existing_pr.number);
                return Ok((true, false));
            }
            PrState::Open => {
                // Update base if needed
                if let Some(ref current_base) = existing_pr.base_ref_name {
                    if current_base != base_branch {
                        if self.executor.verbose {
                            eprintln!(
                                "  PR base needs update: {} -> {}",
                                current_base, base_branch
                            );
                        }
                        self.update_pr_base(branch_name, base_branch)?;
                    }
                }
            }
        }

        revision.pr_url = Some(existing_pr.url.clone());
        revision.pr_number = Some(existing_pr.number);
        Ok((true, false))
    }

    /// Create a new PR
    fn create_new_pr(
        &mut self,
        revision: &mut Revision,
        base_branch: &str,
        stack_position: usize,
        all_revisions: &[Revision],
        branch_name: &str,
    ) -> Result<(bool, bool)> {
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
            revision.pr_url = Some(pr_url);
            revision.pr_state = Some(PrState::Open);
            Ok((true, true))
        } else {
            eprintln!(
                "  error: failed to create PR for {}",
                revision.short_change_id()
            );
            if !output.stderr.is_empty() {
                eprintln!("         {}", output.stderr);
            }
            Ok((false, false))
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

        if !output.success() {
            return Ok(None);
        }

        serde_json::from_str(&output.stdout).map(Some).or_else(|e| {
            eprintln!("error: could not parse PR from GitHub: {}", e);
            Ok(None)
        })
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
            eprintln!("  warning: failed to update PR base to {}", new_base);
            if !output.stderr.is_empty() {
                eprintln!("           {}", output.stderr);
            }
        } else if self.executor.verbose {
            eprintln!("  Successfully updated PR base to {}", new_base);
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
            let new_base = self.determine_new_base_for_pr(*pr_number, revisions);
            self.update_pr_base(branch_name, &new_base)?;
        }

        Ok(())
    }

    /// Determine the new base branch for a PR after reordering
    fn determine_new_base_for_pr(&self, pr_number: u32, revisions: &[Revision]) -> String {
        revisions
            .iter()
            .position(|r| r.pr_number == Some(pr_number))
            .and_then(|idx| {
                if idx == 0 {
                    None
                } else {
                    revisions[idx - 1].branch_name.clone()
                }
            })
            .unwrap_or_else(|| crate::constants::DEFAULT_BASE_BRANCH.to_string())
    }

    /// Enhanced orphaned PR detection with better squash/abandon detection
    #[allow(dead_code)]
    pub fn detect_orphaned_prs_enhanced(
        &mut self,
        current_revisions: &[Revision],
        jj: &JujutsuClient,
    ) -> Result<Vec<(u32, String, String)>> {
        let existing_branches = self.get_existing_branches(false)?;
        let squashed_commits = jj.get_recently_squashed_commits()?;
        let current_change_ids: HashSet<String> = current_revisions
            .iter()
            .map(|r| r.change_id.clone())
            .collect();

        let mut orphaned = Vec::new();

        for (branch_name, _) in existing_branches {
            if let Some(change_id) = self.extract_change_id_from_branch(&branch_name) {
                if let Some(reason) =
                    self.check_if_orphaned(&change_id, &current_change_ids, &squashed_commits)
                {
                    if let Some(pr) = self.get_existing_pr(&branch_name)? {
                        orphaned.push((pr.number, branch_name.clone(), reason));
                    }
                }
            }
        }

        Ok(orphaned)
    }

    /// Check if a change is orphaned and return the reason
    fn check_if_orphaned(
        &self,
        change_id: &str,
        current_change_ids: &HashSet<String>,
        squashed_commits: &HashSet<String>,
    ) -> Option<String> {
        if squashed_commits.iter().any(|s| change_id.starts_with(s)) {
            Some("squashed or abandoned".to_string())
        } else if !current_change_ids.contains(change_id) {
            Some("commit no longer in stack".to_string())
        } else {
            None
        }
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
            let prefix = if i == position { STACK_PR_ARROW } else { "  " };
            let state_marker = Self::get_pr_state_marker(&rev.pr_state, rev.pr_url.is_some());
            body.push_str(&format!(
                "{} {}. {}{}\n",
                prefix,
                i + 1,
                rev.description,
                state_marker
            ));
        }

        body.push_str(&format!("\nChange ID: `{}`\n", revision.change_id));
        body.push_str(&format!("Commit ID: `{}`\n", revision.commit_id));

        body
    }

    /// Get the appropriate marker for PR state
    fn get_pr_state_marker(pr_state: &Option<PrState>, has_pr_url: bool) -> &'static str {
        match pr_state {
            Some(PrState::Merged) => PR_MERGED_MARKER,
            Some(PrState::Closed) => PR_CLOSED_MARKER,
            Some(PrState::Open) if has_pr_url => "",
            _ if has_pr_url => "",
            _ => PR_NO_PR_MARKER,
        }
    }

    /// Update PR titles and bodies with stack information
    pub fn update_pr_details(&mut self, revisions: &[Revision]) -> Result<()> {
        let prs_to_update: Vec<(usize, &Revision)> = revisions
            .iter()
            .enumerate()
            .filter(|(_, r)| Self::should_update_pr(r))
            .collect();

        if prs_to_update.is_empty() {
            return Ok(());
        }

        let repo_spec = self.repo_spec()?;

        for (index, revision) in prs_to_update {
            self.update_single_pr(revision, index, revisions, &repo_spec)?;
        }

        Ok(())
    }

    /// Check if PR should be updated
    fn should_update_pr(revision: &Revision) -> bool {
        revision.pr_url.is_some()
            && revision.branch_name.is_some()
            && !matches!(revision.pr_state, Some(PrState::Merged))
    }

    /// Update a single PR's title and body
    fn update_single_pr(
        &mut self,
        revision: &Revision,
        index: usize,
        all_revisions: &[Revision],
        repo_spec: &str,
    ) -> Result<()> {
        let branch_name = revision.branch_name.as_ref().unwrap();

        if matches!(revision.pr_state, Some(PrState::Merged)) {
            if self.executor.verbose {
                eprintln!(
                    "  Skipping update for merged PR #{}",
                    revision.pr_number.unwrap_or(0)
                );
            }
            return Ok(());
        }

        let body = self.build_full_pr_body(revision, index, all_revisions);
        let title = &revision.description;

        let output = self.executor.run_unchecked(&[
            "gh",
            "pr",
            "edit",
            branch_name,
            "--repo",
            repo_spec,
            "--title",
            title,
            "--body",
            &body,
        ])?;

        if !output.success() {
            eprintln!(
                "  warning: failed to update PR for {}",
                revision.short_change_id()
            );
            if !output.stderr.is_empty() {
                eprintln!("           {}", output.stderr);
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
        self.append_stack_section(&mut body, position, all_revisions);

        // Description section
        self.append_description_section(&mut body, revision);

        // Metadata section
        self.append_metadata_section(&mut body, revision);

        body
    }

    /// Append stack information to PR body
    fn append_stack_section(&self, body: &mut String, position: usize, revisions: &[Revision]) {
        body.push_str("## Stack\n\n");

        for (i, rev) in revisions.iter().enumerate() {
            if rev.pr_url.is_none() {
                continue;
            }

            let marker = if i == position { STACK_PR_ARROW } else { "  " };
            let pr_number = rev.extract_pr_number().unwrap_or(0);
            let state_marker = Self::get_pr_state_marker(&rev.pr_state, true);

            body.push_str(&format!(
                "{} **#{}**: {}{}\n",
                marker, pr_number, rev.description, state_marker
            ));
        }
    }

    /// Append description section to PR body
    fn append_description_section(&self, body: &mut String, revision: &Revision) {
        if let Some(full_desc) = &revision.full_description {
            let additional_content: String = full_desc
                .lines()
                .skip(1)
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();

            if !additional_content.is_empty() {
                body.push_str("\n## Description\n\n");
                body.push_str(&additional_content);
                body.push('\n');
            }
        }
    }

    /// Append metadata section to PR body
    fn append_metadata_section(&self, body: &mut String, revision: &Revision) {
        body.push_str("\n---\n");
        body.push_str(&format!("Change ID: `{}`\n", revision.change_id));
        body.push_str(&format!("Commit ID: `{}`\n", revision.commit_id));
    }
}

/// Type alias for PR information with reason for orphaning
type OrphanedPr = (GithubPr, String);

/// Type alias for closed PR information
type ClosedPrInfo = (u32, String);

/// Data collected for cleanup operations
struct CleanupData {
    existing_branches_map: HashMap<String, String>,
    local_bookmarks: HashSet<String>,
    disappeared_bookmarks: HashSet<String>,
    squashed_commits: HashSet<String>,
    bookmarks_on_same_commit: HashMap<String, Vec<String>>,
    previous_prs: HashMap<String, PrInfo>,
    tracked_branches: HashSet<String>,
    managed_prs: Vec<GithubPr>,
}

/// Context for determining if a PR is orphaned
struct OrphanedPrContext<'a> {
    disappeared_bookmarks: &'a HashSet<String>,
    squashed_commits: &'a HashSet<String>,
    previous_prs: &'a HashMap<String, PrInfo>,
    active_change_ids: &'a HashSet<String>,
    local_bookmarks: &'a HashSet<String>,
    active_branches: &'a HashSet<String>,
}

/// Context for handling duplicate PRs
struct DuplicatePrContext<'a> {
    orphaned_prs: &'a mut Vec<OrphanedPr>,
    branches_to_delete: &'a mut Vec<String>,
    tracked_branches: &'a HashSet<String>,
}
