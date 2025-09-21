use crate::command::CommandExecutor;
use crate::constants::{
    CHANGES_BRANCH_PREFIX, DEFAULT_REMOTE, MAX_OPS_TO_CHECK, PUSH_BRANCH_PREFIX,
};
use crate::types::Revision;
use anyhow::Result;
use regex::Regex;
use std::collections::{HashMap, HashSet};

const FIELD_SEPARATOR: char = '|';
const REVISION_TEMPLATE: &str = r#"change_id.short() ++ "|" ++ change_id ++ "|" ++ commit_id.short() ++ "|" ++ if(empty, "EMPTY", "NOTEMPTY") ++ "|" ++ parents.map(|p| p.change_id()).join(",") ++ "|" ++ description.first_line() ++ "\n""#;

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
        let revset = format!("{}@{}..@", base_branch, DEFAULT_REMOTE);
        let output = self.executor.run(&[
            "jj",
            "log",
            "-r",
            &revset,
            "--no-graph",
            "--template",
            REVISION_TEMPLATE,
        ])?;

        if output.stdout.trim().is_empty() {
            return Ok(Vec::new());
        }

        let mut parsed_revisions = Vec::new();
        let mut skipped_empty = Vec::new();

        for line in output.stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            if let Some(parsed) = self.parse_revision_line(line) {
                if parsed.is_empty {
                    skipped_empty.push(format!(
                        "jj:{} (git:{})",
                        parsed.revision.short_change_id(),
                        parsed.revision.commit_id
                    ));
                    continue;
                }
                parsed_revisions.push(parsed);
            }
        }

        if parsed_revisions.is_empty() {
            if !skipped_empty.is_empty() {
                eprintln!(
                    "  (Skipped empty working copy: {})",
                    skipped_empty.join(", ")
                );
            }
            return Ok(Vec::new());
        }

        let mut revisions = self.linearize_stack(parsed_revisions, base_branch)?;

        if self.executor.verbose {
            eprintln!(
                "Found {} revision{} to push",
                revisions.len(),
                if revisions.len() == 1 { "" } else { "s" }
            );

            if !skipped_empty.is_empty() {
                eprintln!(
                    "  (Skipped empty working copy: {})",
                    skipped_empty.join(", ")
                );
            }
        }

        // Validate revisions
        self.validate_revisions(&revisions)?;

        // Fetch full descriptions
        self.fetch_full_descriptions(&mut revisions)?;

        Ok(revisions)
    }

    /// Parse a single revision line from jj log output
    fn parse_revision_line(&self, line: &str) -> Option<ParsedRevision> {
        let parts: Vec<&str> = line.splitn(6, FIELD_SEPARATOR).collect();
        if parts.len() < 5 {
            return None;
        }

        let change_id = parts[0].to_string();
        let full_change_id = parts[1].to_string();
        let commit_id = parts[2].to_string();
        let is_empty = parts[3] == "EMPTY";
        let parent_change_ids = if parts[4].trim().is_empty() {
            Vec::new()
        } else {
            parts[4]
                .split(',')
                .filter(|parent| !parent.trim().is_empty())
                .map(|parent| parent.trim().to_string())
                .collect()
        };

        let description = if parts.len() > 5 {
            let desc = parts[5].trim();
            if desc.is_empty() {
                "(no description)".to_string()
            } else {
                desc.to_string()
            }
        } else {
            "(no description)".to_string()
        };

        Some(ParsedRevision {
            revision: Revision::new(
                change_id,
                commit_id,
                if is_empty {
                    "EMPTY".to_string()
                } else {
                    description
                },
            ),
            full_change_id,
            parent_change_ids,
            is_empty,
        })
    }

    fn linearize_stack(
        &self,
        parsed_revisions: Vec<ParsedRevision>,
        base_branch: &str,
    ) -> Result<Vec<Revision>> {
        if parsed_revisions.is_empty() {
            return Ok(Vec::new());
        }

        let mut id_to_index = HashMap::new();
        for (index, parsed) in parsed_revisions.iter().enumerate() {
            id_to_index.insert(parsed.full_change_id.clone(), index);
        }

        let mut child_map: HashMap<String, String> = HashMap::new();
        let mut roots = Vec::new();

        for parsed in &parsed_revisions {
            let mut parents_in_stack = Vec::new();
            for parent in &parsed.parent_change_ids {
                if id_to_index.contains_key(parent) {
                    parents_in_stack.push(parent.clone());
                }
            }

            if parents_in_stack.len() > 1 {
                let parent_labels: Vec<String> = parents_in_stack
                    .iter()
                    .filter_map(|parent| id_to_index.get(parent))
                    .map(|index| {
                        parsed_revisions[*index]
                            .revision
                            .short_change_id()
                            .to_string()
                    })
                    .collect();
                anyhow::bail!(
                    "Commit {} merges multiple stack entries ({}). Stacks must be linear.",
                    parsed.revision.short_change_id(),
                    parent_labels.join(", ")
                );
            }

            if let Some(parent) = parents_in_stack.first() {
                if let Some(existing_child) =
                    child_map.insert(parent.clone(), parsed.full_change_id.clone())
                {
                    let existing = &parsed_revisions[*id_to_index
                        .get(&existing_child)
                        .expect("existing child must exist")];
                    let parent_rev =
                        &parsed_revisions[*id_to_index.get(parent).expect("parent must exist")];
                    anyhow::bail!(
                        "Stack branches at {} ({} and {} both depend on it). Rebase your stack to be linear before running almighty-push.",
                        parent_rev.revision.short_change_id(),
                        existing.revision.short_change_id(),
                        parsed.revision.short_change_id()
                    );
                }
            } else {
                roots.push(parsed.full_change_id.clone());
            }
        }

        if roots.is_empty() {
            anyhow::bail!(
                "Could not determine the base of your stack. Ensure your commits are descendants of {}@{}.",
                base_branch,
                DEFAULT_REMOTE
            );
        }

        if roots.len() > 1 {
            let root_labels: Vec<String> = roots
                .iter()
                .filter_map(|root| id_to_index.get(root))
                .map(|index| {
                    parsed_revisions[*index]
                        .revision
                        .short_change_id()
                        .to_string()
                })
                .collect();
            anyhow::bail!(
                "Found multiple stack roots ({}). Rebase onto a single {}@{} ancestor before pushing.",
                root_labels.join(", "),
                base_branch,
                DEFAULT_REMOTE
            );
        }

        let root_id = roots[0].clone();
        let mut ordered_ids = Vec::new();
        let mut current = root_id.clone();
        let mut visited = HashSet::new();

        loop {
            if !visited.insert(current.clone()) {
                let rev =
                    &parsed_revisions[*id_to_index.get(&current).expect("cycle node must exist")];
                anyhow::bail!(
                    "Detected a cycle while traversing the stack at {}. Rebase your stack to be linear.",
                    rev.revision.short_change_id()
                );
            }

            ordered_ids.push(current.clone());

            if let Some(next) = child_map.get(&current) {
                current = next.clone();
            } else {
                break;
            }
        }

        if visited.len() != parsed_revisions.len() {
            let missing: Vec<String> = parsed_revisions
                .iter()
                .filter(|parsed| !visited.contains(&parsed.full_change_id))
                .map(|parsed| parsed.revision.short_change_id().to_string())
                .collect();
            anyhow::bail!(
                "Could not connect all commits into a single stack (unreachable: {}). Rebase your stack to be linear before pushing.",
                missing.join(", ")
            );
        }

        let mut ordered_revisions = Vec::with_capacity(parsed_revisions.len());
        for id in ordered_ids {
            let index = id_to_index
                .get(&id)
                .copied()
                .expect("ordered id must exist in map");
            ordered_revisions.push(parsed_revisions[index].revision.clone());
        }

        Ok(ordered_revisions)
    }
    /// Validate that all revisions have descriptions
    fn validate_revisions(&self, revisions: &[Revision]) -> Result<()> {
        let missing_descriptions: Vec<&Revision> = revisions
            .iter()
            .filter(|rev| rev.description == "(no description)")
            .collect();

        if !missing_descriptions.is_empty() {
            // Check if the last commit is the one without description (likely the working copy)
            let is_working_copy = revisions
                .last()
                .map(|last| {
                    missing_descriptions
                        .iter()
                        .any(|rev| rev.change_id == last.change_id)
                })
                .unwrap_or(false);

            eprintln!("\nerror: the following commits have no description:");
            for rev in &missing_descriptions {
                let is_this_working_copy = revisions
                    .last()
                    .map(|last| last.change_id == rev.change_id)
                    .unwrap_or(false);

                if is_this_working_copy {
                    eprintln!(
                        "  - jj:{} (git:{}) [working copy @]",
                        rev.short_change_id(),
                        rev.commit_id
                    );
                } else {
                    eprintln!("  - jj:{} (git:{})", rev.short_change_id(), rev.commit_id);
                }
            }

            if is_working_copy {
                eprintln!("\nYour working copy (@) has uncommitted changes with no description.");
                eprintln!("To push your stack, you can:");
                eprintln!("  1. Squash into the previous commit: jj squash");
                eprintln!("  2. Add a description: jj describe -m \"Your changes\"");
                eprintln!("  3. Abandon your working copy: jj abandon @");
                if revisions.len() > 1 {
                    eprintln!("  4. Move to the previous commit: jj edit @-");
                }
            } else {
                eprintln!("\nAdd descriptions to all commits before pushing.");
                eprintln!("Use: jj describe -r <change_id> -m \"Your description\"");
            }
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

    /// Delete local bookmarks
    pub fn delete_local_bookmarks(&self, bookmarks: &[String]) -> Result<bool> {
        if bookmarks.is_empty() {
            return Ok(false);
        }

        if self.executor.verbose {
            eprintln!("  Deleting local bookmarks...");
        }

        let mut args = vec!["jj", "bookmark", "delete"];
        for bookmark in bookmarks {
            args.push(bookmark.as_str());
        }

        let output = self.executor.run_unchecked(&args)?;

        if output.success() {
            for bookmark in bookmarks {
                if self.executor.verbose {
                    eprintln!("    Deleted local bookmark: {}", bookmark);
                }
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
        if self.executor.verbose {
            eprintln!("    Running 'jj git push --deleted' to propagate deletions...");
        }

        let output = self
            .executor
            .run_unchecked(&["jj", "git", "push", "--deleted"])?;

        if output.success() {
            if self.executor.verbose {
                eprintln!("    Propagated bookmark deletions to remote");
            }
        } else {
            eprintln!("    warning: failed to push bookmark deletions to remote");
            if !output.stderr.is_empty() {
                eprintln!("             {}", output.stderr);
            }
        }

        Ok(())
    }

    /// Rebase a source revision and its descendants onto a destination
    pub fn rebase_revision(&self, source_change_id: &str, destination: &str) -> Result<()> {
        if self.executor.verbose {
            eprintln!(
                "  Rebasing {} and descendants onto {}",
                &source_change_id[..8.min(source_change_id.len())],
                destination
            );
        }

        let output =
            self.executor
                .run(&["jj", "rebase", "-s", source_change_id, "-d", destination])?;

        if self.executor.verbose && !output.stdout.is_empty() {
            eprintln!("    {}", output.stdout.trim());
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
        let mut success_count = 0;
        let mut failure_count = 0;

        for rev in revisions {
            if let Some(branch_name) = &rev.branch_name {
                // First check if we need to track the remote bookmark
                let track_output = self.executor.run_unchecked(&[
                    "jj",
                    "bookmark",
                    "track",
                    &format!("{}@{}", branch_name, DEFAULT_REMOTE),
                ])?;

                if track_output.success() && self.executor.verbose {
                    eprintln!(
                        "  Tracked remote bookmark: {}@{}",
                        branch_name, DEFAULT_REMOTE
                    );
                }

                // If bookmark is conflicted, try to fix it by setting it to the current commit
                let list_output = self.executor.run_unchecked(&[
                    "jj",
                    "bookmark",
                    "list",
                    branch_name,
                    "--template",
                    "name ++ if(conflict, ' (conflicted)', '')",
                ])?;

                if list_output.success() && list_output.stdout.contains("conflicted") {
                    // Set the bookmark to point to our commit using commit_id to avoid ambiguity
                    let set_output = self.executor.run_unchecked(&[
                        "jj",
                        "bookmark",
                        "set",
                        branch_name,
                        "-r",
                        &rev.commit_id,
                    ])?;

                    if set_output.success() && self.executor.verbose {
                        eprintln!(
                            "  Resolved bookmark conflict: {} -> {}",
                            branch_name,
                            rev.short_change_id()
                        );
                    }
                }

                // Now try to push by bookmark name
                let output =
                    self.executor
                        .run_unchecked(&["jj", "git", "push", "-b", branch_name])?;

                let mut push_success = output.success();

                if !push_success {
                    // If bookmark push failed, try different strategies
                    if output.stderr.contains("Bookmark already exists") {
                        // This might mean the bookmark exists but points to a different commit
                        // Try to move it
                        eprintln!(
                            "  warning: bookmark {} exists, trying to move it",
                            branch_name
                        );
                        let move_output = self.executor.run_unchecked(&[
                            "jj",
                            "bookmark",
                            "move",
                            branch_name,
                            "-r",
                            &rev.commit_id,
                        ])?;

                        if move_output.success() {
                            // Try pushing again after moving
                            let retry_output = self.executor.run_unchecked(&[
                                "jj",
                                "git",
                                "push",
                                "-b",
                                branch_name,
                            ])?;
                            push_success = retry_output.success();
                        }
                    } else if output.stderr.contains("Non-tracking remote bookmark") {
                        // This shouldn't happen after tracking, but try change push as fallback
                        eprintln!(
                            "  warning: bookmark {} still not tracking, pushing by change ID",
                            branch_name
                        );
                    } else if output.stderr.contains("conflicted") {
                        eprintln!(
                            "  warning: bookmark {} is still conflicted, pushing by change ID",
                            branch_name
                        );
                    } else if output.stderr.contains("No such bookmark") {
                        eprintln!(
                            "  warning: bookmark {} doesn't exist, pushing by change ID",
                            branch_name
                        );
                    } else {
                        eprintln!(
                            "  warning: unexpected push failure for {}, trying by change ID",
                            branch_name
                        );
                    }

                    // Fallback: try pushing by change ID
                    if !push_success {
                        let change_output = self.executor.run_unchecked(&[
                            "jj",
                            "git",
                            "push",
                            "--change",
                            &rev.change_id,
                        ])?;

                        push_success = change_output.success();

                        // Handle case where multiple revisions have the same change ID
                        if !push_success
                            && change_output
                                .stderr
                                .contains("resolved to more than one revision")
                        {
                            eprintln!("  warning: multiple revisions with change ID {}, using commit ID instead", rev.short_change_id());

                            // Try to abandon duplicate commits first
                            self.abandon_duplicate_changes(&rev.change_id, &rev.commit_id)?;

                            // Then push using the specific commit ID
                            let commit_push_output = self.executor.run_unchecked(&[
                                "jj",
                                "git",
                                "push",
                                "--change",
                                &rev.commit_id,
                            ])?;
                            push_success = commit_push_output.success();
                        }

                        // If that fails too, try forcing the bookmark
                        if !push_success && change_output.stderr.contains("Bookmark already exists")
                        {
                            eprintln!("  warning: forcing bookmark {} update", branch_name);
                            let force_output = self.executor.run_unchecked(&[
                                "jj",
                                "git",
                                "push",
                                "-b",
                                branch_name,
                                "--allow-backwards",
                            ])?;
                            push_success = force_output.success();
                        }
                    }
                }

                if push_success {
                    success_count += 1;
                    if self.executor.verbose {
                        eprintln!("  Successfully pushed {}", rev.short_change_id());
                    }
                } else {
                    failure_count += 1;
                    eprintln!("  error: failed to push {}", rev.short_change_id());
                    if !output.stderr.is_empty() {
                        eprintln!("         {}", output.stderr.trim());
                    }
                }
            }
        }

        if failure_count > 0 && success_count == 0 {
            anyhow::bail!("Failed to push any branches");
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
                    if self.executor.verbose {
                        eprintln!("  Pushed {} as branch {}", rev.short_change_id(), branch);
                    }
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
                if self.executor.verbose {
                    eprintln!("  warning: assuming branch name: {}", branch_name);
                }
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
            // Enhanced detection for various operations that remove commits
            if line_lower.contains("squash")
                || line_lower.contains("abandon")
                || line_lower.contains("fold")
                || line_lower.contains("amend") && line_lower.contains("into")
            {
                squashed_change_ids.extend(Self::extract_change_ids(line));
            }
        }

        Ok(squashed_change_ids)
    }

    /// Extract potential change IDs from text
    fn extract_change_ids(text: &str) -> HashSet<String> {
        let mut change_ids = HashSet::new();

        for word in text.split_whitespace() {
            // Check if word looks like a change ID (8-32 chars, more flexible)
            if word.len() >= 8 && word.len() <= 32 {
                let word_lower = word.to_lowercase();
                // JJ change IDs use specific character set
                if word_lower
                    .chars()
                    .all(|c| "klmnopqrstuvwxyz".contains(c) || c.is_ascii_digit())
                {
                    change_ids.insert(word_lower);
                }
            }
        }

        change_ids
    }

    /// Get detailed history of a specific commit including evolution
    #[allow(dead_code)]
    pub fn get_commit_history(&self, change_id: &str) -> Result<CommitHistory> {
        let mut history = CommitHistory::default();

        // Note: predecessors() function doesn't exist in jj templates
        // For now, we'll leave the predecessors list empty
        // This functionality could be implemented using jj obslog or operation log analysis
        // history.predecessors = vec![];

        // Get operation history for this change
        let op_output = self.executor.run_unchecked(&[
            "jj",
            "op",
            "log",
            "--limit",
            "20",
            "--no-graph",
            "--template",
            r#"if(description.contains(change_id), description ++ "\n", "")"#
                .replace("change_id", change_id)
                .as_str(),
        ])?;

        if op_output.success() {
            history.operations = op_output
                .stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|s| s.to_string())
                .collect();
        }

        Ok(history)
    }

    /// Abandon duplicate commits with the same change ID, keeping only the specified commit
    fn abandon_duplicate_changes(&self, change_id: &str, keep_commit_id: &str) -> Result<()> {
        // Get all commits with this change ID
        let output = self.executor.run_unchecked(&[
            "jj",
            "log",
            "-r",
            change_id,
            "--no-graph",
            "--template",
            r#"commit_id ++ "
""#,
        ])?;

        if !output.success() {
            return Ok(()); // No duplicates found
        }

        let commit_ids: Vec<String> = output
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|s| s.to_string())
            .collect();

        // Abandon all commits except the one we want to keep
        for commit_id in commit_ids {
            if commit_id != keep_commit_id {
                if self.executor.verbose {
                    eprintln!("  Abandoning duplicate commit: {}", &commit_id[..12]);
                }

                let abandon_output = self
                    .executor
                    .run_unchecked(&["jj", "abandon", "-r", &commit_id])?;

                if !abandon_output.success() {
                    eprintln!(
                        "  warning: failed to abandon duplicate commit {}",
                        &commit_id[..12]
                    );
                }
            }
        }

        Ok(())
    }

    /// Check if a change ID exists in the current repository
    #[allow(dead_code)]
    pub fn change_exists(&self, change_id: &str) -> Result<bool> {
        let output = self.executor.run_unchecked(&[
            "jj",
            "log",
            "-r",
            change_id,
            "--no-graph",
            "--template",
            "change_id",
        ])?;

        Ok(output.success() && !output.stdout.trim().is_empty())
    }

    /// Get all commits that were present in a previous operation
    #[allow(dead_code)]
    pub fn get_commits_at_operation(&self, op_id: &str) -> Result<HashSet<String>> {
        let output = self.executor.run_unchecked(&[
            "jj",
            "log",
            "--at-op",
            op_id,
            "-r",
            "all()",
            "--no-graph",
            "--template",
            r#"change_id ++ "\n""#,
        ])?;

        if !output.success() {
            return Ok(HashSet::new());
        }

        Ok(output
            .stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|s| s.trim().to_string())
            .collect())
    }
}

#[derive(Clone)]
struct ParsedRevision {
    revision: Revision,
    full_change_id: String,
    parent_change_ids: Vec<String>,
    is_empty: bool,
}

/// Detailed history of a commit
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct CommitHistory {
    pub predecessors: Vec<String>,
    pub operations: Vec<String>,
}
