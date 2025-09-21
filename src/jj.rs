use crate::command::CommandExecutor;
use crate::constants::{
    CHANGES_BRANCH_PREFIX, DEFAULT_REMOTE, MAX_OPS_TO_CHECK, PUSH_BRANCH_PREFIX,
};
use crate::types::Revision;
use anyhow::Result;
use regex::Regex;
use std::collections::{HashMap, HashSet};

const FIELD_SEPARATOR: char = '\u{1f}';
const REVISION_TEMPLATE: &str = concat!(
    "change_id.short() ++ \"",
    "\u{1f}",
    "\" ++ change_id() ++ \"",
    "\u{1f}",
    "\" ++ commit_id.short() ++ \"",
    "\u{1f}",
    "\" ++ if(empty, \"EMPTY\", \"NOTEMPTY\") ++ \"",
    "\u{1f}",
    "\" ++ parents.map(|p| p.change_id()).join(\",\") ++ \"",
    "\u{1f}",
    "\" ++ description.first_line() ++ \"\\n\"",
);

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
                        "{} ({})",
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

        eprintln!("Found {} revisions to push", revisions.len());

        if !skipped_empty.is_empty() {
            eprintln!(
                "  (Skipped empty working copy: {})",
                skipped_empty.join(", ")
            );
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

#[derive(Clone)]
struct ParsedRevision {
    revision: Revision,
    full_change_id: String,
    parent_change_ids: Vec<String>,
    is_empty: bool,
}
