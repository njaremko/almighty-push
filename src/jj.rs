use crate::command::CommandExecutor;
use crate::constants::{
    CHANGES_BRANCH_PREFIX, DEFAULT_REMOTE, MAX_OPS_TO_CHECK, PUSH_BRANCH_PREFIX,
};
use crate::types::Revision;
use anyhow::Result;
use regex::Regex;
use std::collections::{HashMap, HashSet};

/// Handles all Jujutsu (jj) operations
pub struct JujutsuClient {
    executor: CommandExecutor,
}

impl JujutsuClient {
    /// Create a new JujutsuClient
    pub fn new(executor: CommandExecutor) -> Self {
        Self { executor }
    }

    /// Get bookmarks that point to the same commit
    pub fn get_bookmarks_on_same_commit(&self) -> Result<HashMap<String, Vec<String>>> {
        let output = self.executor.run_unchecked(&[
            "jj",
            "log",
            "-r",
            "bookmarks()",
            "--no-graph",
            "--template",
            r#"commit_id.short() ++ " " ++ bookmarks.join(" ") ++ "\n""#,
        ])?;

        if !output.success() {
            return Ok(HashMap::new());
        }

        let mut commit_to_bookmarks = HashMap::new();

        for line in output.stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            if parts.len() != 2 {
                continue;
            }

            let commit_id = parts[0].to_string();
            let bookmarks = self.parse_bookmarks(parts[1]);

            if bookmarks.len() > 1 {
                commit_to_bookmarks.insert(commit_id, bookmarks);
            }
        }

        Ok(commit_to_bookmarks)
    }

    /// Parse bookmark string into list of relevant bookmarks
    fn parse_bookmarks(&self, bookmarks_str: &str) -> Vec<String> {
        let mut bookmarks = Vec::new();

        for bookmark in bookmarks_str.split_whitespace() {
            if Self::is_managed_bookmark(bookmark) {
                if !bookmarks.contains(&bookmark.to_string()) {
                    bookmarks.push(bookmark.to_string());
                }
            } else if bookmark.contains('@') {
                let base_name = bookmark.split('@').next().unwrap_or("");
                if Self::is_managed_bookmark(base_name) {
                    let remote_bookmark = format!("{}*", base_name);
                    if !bookmarks.contains(&remote_bookmark) {
                        bookmarks.push(remote_bookmark);
                    }
                }
            }
        }

        bookmarks
    }

    /// Check if bookmark is managed by almighty-push
    fn is_managed_bookmark(name: &str) -> bool {
        name.starts_with(PUSH_BRANCH_PREFIX) || name.starts_with(CHANGES_BRANCH_PREFIX)
    }

    /// Get all revisions in the current stack above the base bookmark
    pub fn get_revisions_above_base(&self, base_branch: &str) -> Result<Vec<Revision>> {
        let output = self.executor.run(&[
            "jj",
            "log",
            "-r",
            &format!("{}@{}..@", base_branch, DEFAULT_REMOTE),
            "--no-graph",
            "--template",
            r#"change_id.short() ++ " " ++ commit_id.short() ++ " " ++ if(empty, "EMPTY", "NOTEMPTY") ++ " " ++ description.first_line() ++ "\n""#,
        ])?;

        if output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }

        let mut revisions = Vec::new();
        let mut skipped_empty = Vec::new();

        for line in output.stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some(revision) = self.parse_revision_line(line) {
                if revision.description == "EMPTY" {
                    skipped_empty.push(format!(
                        "{} ({})",
                        revision.short_change_id(),
                        revision.commit_id
                    ));
                    continue;
                }
                revisions.push(revision);
            }
        }

        // Reverse to get bottom-up order (oldest first)
        revisions.reverse();

        eprintln!("Found {} revisions to push", revisions.len());

        if !skipped_empty.is_empty() {
            eprintln!("  (Skipped empty working copy: {})", skipped_empty[0]);
        }

        // Validate revisions
        self.validate_revisions(&revisions)?;

        // Fetch full descriptions
        self.fetch_full_descriptions(&mut revisions)?;
        self.ensure_linear_stack(base_branch, &revisions)?;

        Ok(revisions)
    }

    fn ensure_linear_stack(&self, base_branch: &str, revisions: &[Revision]) -> Result<()> {
        if revisions.is_empty() {
            return Ok(());
        }

        let revset = Self::stack_revset(base_branch);

        let heads_expr = format!("heads({})", revset);
        let heads = self.collect_revset_entries(&heads_expr, 10)?;
        if heads.len() > 1 {
            let preview = heads.join(", ");
            anyhow::bail!(
                "Multiple stack heads detected above {}: {}. Resolve the divergence (for example, by restacking with `jj rebase`) and try again. Inspect with: jj log -r \"{}\" --no-graph",
                base_branch,
                preview,
                heads_expr
            );
        }

        let roots_expr = format!("roots({})", revset);
        let roots = self.collect_revset_entries(&roots_expr, 10)?;
        if roots.len() > 1 {
            let preview = roots.join(", ");
            anyhow::bail!(
                "Multiple independent roots detected above {}: {}. almighty-push requires a single linear stack. Inspect with: jj log -r \"{}\" --no-graph",
                base_branch,
                preview,
                roots_expr
            );
        }

        Ok(())
    }

    fn collect_revset_entries(&self, revset_expr: &str, limit: usize) -> Result<Vec<String>> {
        let limit_str = limit.to_string();
        let output = self.executor.run_unchecked(&[
            "jj",
            "log",
            "-r",
            revset_expr,
            "--no-graph",
            "--limit",
            &limit_str,
            "--template",
            r#"change_id.short() ++ " " ++ description.first_line() ++ "\n""#,
        ])?;

        if !output.success() {
            return Ok(Vec::new());
        }

        let entries = output
            .stdout
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            })
            .collect();

        Ok(entries)
    }

    fn stack_revset(base_branch: &str) -> String {
        format!("{}@{}..@", base_branch, DEFAULT_REMOTE)
    }

    /// Parse a single revision line from jj log output
    fn parse_revision_line(&self, line: &str) -> Option<Revision> {
        let parts: Vec<&str> = line.splitn(4, ' ').collect();
        if parts.len() < 3 {
            return None;
        }

        let change_id = parts[0].to_string();
        let commit_id = parts[1].to_string();
        let is_empty = parts[2];
        let description = if parts.len() > 3 {
            parts[3].trim().to_string()
        } else {
            "(no description)".to_string()
        };

        if is_empty == "EMPTY" {
            // Return a marker revision for empty commits
            return Some(Revision::new(change_id, commit_id, "EMPTY".to_string()));
        }

        Some(Revision::new(change_id, commit_id, description))
    }

    /// Validate that all revisions have descriptions
    fn validate_revisions(&self, revisions: &[Revision]) -> Result<()> {
        let missing_descriptions: Vec<&Revision> = revisions
            .iter()
            .filter(|rev| rev.description == "(no description)")
            .collect();

        if !missing_descriptions.is_empty() {
            eprintln!("\nerror: the following commits have no description:");
            for rev in &missing_descriptions {
                eprintln!("  - {} ({})", rev.short_change_id(), rev.commit_id);
            }
            eprintln!("\nAdd descriptions to all commits before pushing.");
            eprintln!("Use: jj describe -r <change_id> -m \"Your description\"");
            anyhow::bail!("Commits without descriptions found");
        }

        Ok(())
    }

    /// Fetch full multi-line descriptions for all revisions
    fn fetch_full_descriptions(&self, revisions: &mut [Revision]) -> Result<()> {
        for rev in revisions {
            let output = self.executor.run_unchecked(&[
                "jj",
                "log",
                "-r",
                &rev.change_id,
                "--no-graph",
                "--template",
                "description",
            ])?;

            if output.success() && !output.stdout.is_empty() {
                rev.full_description = Some(output.stdout.trim().to_string());
            } else {
                rev.full_description = Some(rev.description.clone());
            }
        }

        Ok(())
    }

    /// Get all local bookmarks from jj
    pub fn get_local_bookmarks(&self) -> Result<HashSet<String>> {
        let output = self.executor.run_unchecked(&[
            "jj",
            "bookmark",
            "list",
            "--template",
            r#"name ++ "\n""#,
        ])?;

        if !output.success() {
            return Ok(HashSet::new());
        }

        let bookmarks = output
            .stdout
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if !line.is_empty() && Self::is_managed_bookmark(line) {
                    Some(line.to_string())
                } else {
                    None
                }
            })
            .collect();

        Ok(bookmarks)
    }

    /// Delete local bookmarks for merged PRs
    pub fn delete_local_bookmarks(&self, bookmarks: &[String]) -> Result<bool> {
        if bookmarks.is_empty() {
            return Ok(false);
        }

        eprintln!("\n  Deleting local bookmarks for merged PRs...");

        let mut args = vec!["jj", "bookmark", "delete"];
        for bookmark in bookmarks {
            args.push(bookmark.as_str());
        }

        let output = self.executor.run_unchecked(&args)?;

        if output.success() {
            for bookmark in bookmarks {
                eprintln!("    Deleted local bookmark: {}", bookmark);
            }
            Ok(true)
        } else {
            eprintln!(
                "    warning: failed to delete local bookmarks: {}",
                bookmarks.join(", ")
            );
            if !output.stderr.is_empty() {
                eprintln!("             {}", output.stderr);
            }
            Ok(false)
        }
    }

    /// Propagate bookmark deletions to the remote
    pub fn push_deleted_bookmarks(&self) -> Result<()> {
        eprintln!("    Running 'jj git push --deleted' to propagate deletions...");

        let output = self
            .executor
            .run_unchecked(&["jj", "git", "push", "--deleted"])?;

        if output.success() {
            eprintln!("    Propagated bookmark deletions to remote");
        } else {
            eprintln!("    warning: failed to push bookmark deletions to remote");
            if !output.stderr.is_empty() {
                eprintln!("             {}", output.stderr);
            }
        }

        Ok(())
    }

    /// Push revisions to remote using jj git push
    pub fn push_revisions(&self, revisions: &mut [Revision]) -> Result<()> {
        if revisions.is_empty() {
            return Ok(());
        }

        let (to_create, to_update): (Vec<_>, Vec<_>) = revisions
            .iter_mut()
            .partition(|rev| rev.branch_name.is_none());

        if !to_create.is_empty() {
            self.push_new_branches(to_create)?;
        }

        if !to_update.is_empty() {
            self.update_existing_branches(to_update)?;
        }

        Ok(())
    }

    /// Push revisions that don't have branches yet
    fn push_new_branches(&self, revisions: Vec<&mut Revision>) -> Result<()> {
        let mut args = vec!["jj", "git", "push"];

        for rev in &revisions {
            args.push("--change");
            args.push(&rev.change_id);
        }

        let output = self.executor.run(&args)?;
        self.parse_push_output(&output, revisions)?;

        Ok(())
    }

    /// Update existing branches
    fn update_existing_branches(&self, revisions: Vec<&mut Revision>) -> Result<()> {
        for rev in revisions {
            if let Some(branch_name) = &rev.branch_name {
                let output =
                    self.executor
                        .run_unchecked(&["jj", "git", "push", "-b", branch_name])?;

                if !output.success() {
                    // Try with --change as fallback
                    self.executor.run_unchecked(&[
                        "jj",
                        "git",
                        "push",
                        "--change",
                        &rev.change_id,
                    ])?;
                }
            }
        }

        Ok(())
    }

    /// Parse jj git push output to extract branch names
    fn parse_push_output(
        &self,
        output: &crate::command::CommandOutput,
        revisions: Vec<&mut Revision>,
    ) -> Result<()> {
        let combined = output.combined_output();

        let patterns = [
            r"(?:Creating branch|Created branch|Branch) (push-\w+|changes/\w+)",
            r"(push-\w+|changes/\w+) for revision",
            r"branch[:\s]+(push-\w+|changes/\w+)",
        ];

        let mut branches_found = Vec::new();
        for pattern_str in &patterns {
            let pattern = Regex::new(pattern_str)?;
            for cap in pattern.captures_iter(&combined) {
                if let Some(branch) = cap.get(1) {
                    branches_found.push(branch.as_str().to_string());
                }
            }
        }

        for rev in revisions {
            for branch in &branches_found {
                let change_id_short = &rev.change_id;
                if [6, 8, 12].iter().any(|&n| {
                    let len = change_id_short.len().min(n);
                    branch.contains(&change_id_short[..len])
                }) {
                    rev.branch_name = Some(branch.clone());
                    eprintln!("  Pushed {} as branch {}", rev.short_change_id(), branch);
                    break;
                }
            }

            if rev.branch_name.is_none() {
                // Assume standard pattern
                let branch_name = format!(
                    "{}{}",
                    PUSH_BRANCH_PREFIX,
                    &rev.change_id[..12.min(rev.change_id.len())]
                );
                rev.branch_name = Some(branch_name.clone());
                eprintln!("  warning: assuming branch name: {}", branch_name);
            }
        }

        Ok(())
    }

    /// Use jj op log to find commits that were recently squashed or abandoned
    pub fn get_recently_squashed_commits(&self) -> Result<HashSet<String>> {
        let output = self.executor.run_unchecked(&[
            "jj",
            "op",
            "log",
            "--limit",
            &MAX_OPS_TO_CHECK.to_string(),
            "--no-graph",
            "--template",
            r#"id.short() ++ " " ++ description ++ "\n""#,
        ])?;

        if !output.success() {
            return Ok(HashSet::new());
        }

        let mut squashed_change_ids = HashSet::new();

        for line in output.stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let line_lower = line.to_lowercase();
            if line_lower.contains("squash") || line_lower.contains("abandon") {
                squashed_change_ids.extend(Self::extract_change_ids(line));
            }
        }

        Ok(squashed_change_ids)
    }

    /// Extract potential change IDs from text
    fn extract_change_ids(text: &str) -> HashSet<String> {
        let mut change_ids = HashSet::new();

        for word in text.split_whitespace() {
            // Check if word looks like a change ID (8-12 hex chars)
            if word.len() >= 8 && word.len() <= 12 {
                let word_lower = word.to_lowercase();
                if word_lower
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || "klmnopqrstuvwxyz".contains(c))
                {
                    change_ids.insert(word_lower);
                }
            }
        }

        change_ids
    }

    /// Delete remote branches
    pub fn delete_remote_branches(&self, branches: &[String]) -> Result<()> {
        eprintln!("\n  Deleting remote branches for closed PRs...");

        for branch in branches {
            let output = self
                .executor
                .run_unchecked(&["jj", "git", "push", "--branch", branch, "--delete"])?;

            if output.success() {
                eprintln!("    Deleted remote branch: {}", branch);
            } else {
                eprintln!("    warning: failed to delete remote branch: {}", branch);
                if !output.stderr.is_empty() {
                    eprintln!("             {}", output.stderr);
                }
            }
        }

        Ok(())
    }
}
