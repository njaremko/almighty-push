use anyhow::{bail, Context, Result};
use chrono;
use clap::Parser;
use regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::process::{self, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Push jj stacks to GitHub as PRs
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = "Almighty Push - Automated jj stack pusher and PR creator for GitHub.\nPushes all changes in current stack above main and creates properly stacked PRs.")]
struct Args {
    /// Show what would be done without actually doing it
    #[arg(long)]
    dry_run: bool,

    /// Delete remote branches when closing orphaned PRs
    #[arg(long)]
    delete_branches: bool,

    /// Only push branches, don't create or update PRs
    #[arg(long)]
    no_pr: bool,

    /// Enable verbose output
    #[arg(short, long)]
    verbose: bool,
}

#[derive(Debug, Clone)]
struct Revision {
    change_id: String,
    commit_id: String,
    description: String,
    branch_name: Option<String>,
    pr_number: Option<u32>,
    pr_url: Option<String>,
    pr_state: Option<String>,
    has_conflicts: bool,
    parent_change_ids: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct State {
    version: u32,
    prs: HashMap<String, PrInfo>,
    merged_prs: HashSet<String>,
    closed_prs: HashSet<String>,
    last_operation_id: Option<String>,
    #[serde(default)]
    stack_order: Vec<String>,
    #[serde(default)]
    operations: Vec<Operation>,
    #[serde(default)]
    last_updated: Option<String>,
    #[serde(default)]
    merged_into_pr: HashMap<String, String>,  // Maps change_id -> PR branch it was merged into
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Operation {
    id: String,
    op_type: String,
    timestamp: String,
    changes_affected: Vec<String>,
    success: bool,
}

const STATE_VERSION: u32 = 2;
const LOCK_FILE: &str = ".almighty.lock";
const LOCK_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PrInfo {
    pr_number: u32,
    pr_url: String,
    branch_name: String,
    commit_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    change_id: Option<String>,
}


fn main() -> Result<()> {
    let args = Args::parse();

    if args.verbose {
        eprintln!("almighty-push v{}", env!("CARGO_PKG_VERSION"));
    }

    // Get repository info from jj remote
    let repo_info = get_repo_info(args.verbose)?;
    if args.verbose {
        eprintln!("Repository: {}", repo_info);
    }

    // Acquire lock to prevent concurrent execution
    let _lock = acquire_lock()?;

    // Fetch latest from remote
    if args.verbose {
        eprintln!("Fetching from remote...");
    }
    run_command(&["jj", "git", "fetch"], false, args.verbose)?;
    
    // Load and migrate state
    let mut state = load_state()?;
    migrate_state(&mut state)?;

    // Get current stack
    let mut revisions = get_stack_revisions(args.verbose)?;
    if revisions.is_empty() {
        if args.verbose {
            eprintln!("No revisions to push");
        }
        return Ok(());
    }

    // Track operation for recovery
    let op_id = track_operation_start(&mut state, "push_stack", &revisions)?;

    // Detect various edge cases
    let squashed = detect_squashed_commits(&mut revisions, &state, args.verbose)?;
    let conflicts = check_for_conflicts(&mut revisions, args.verbose)?;
    let reordered = detect_reordered_stack(&revisions, &state)?;
    let splits = detect_split_commits(&revisions, &state, args.verbose)?;
    
    // Check for merged PRs and handle them
    let merged = detect_merged_prs(&mut revisions, &state, &repo_info, args.verbose)?;
    if !merged.is_empty() {
        // Separate PRs that are still in stack from those that were merged into other PRs
        let in_stack: Vec<_> = merged.iter()
            .filter(|(idx, _, _)| *idx != usize::MAX)
            .cloned()
            .collect();

        let merged_into_others: Vec<_> = merged.iter()
            .filter(|(idx, _, _)| *idx == usize::MAX)
            .cloned()
            .collect();

        // Handle PRs that are still in the stack (need rebasing)
        if !in_stack.is_empty() {
            handle_merged_prs(&in_stack, &mut revisions, args.verbose)?;

            // Handle out-of-order merges for PRs in stack
            for (_, change_id, base_branch) in &in_stack {
                if let Some(ref base) = base_branch {
                    if base.starts_with("push-") && base != "main" {
                        // Track that this PR was merged into another PR branch
                        state.merged_into_pr.insert(change_id.clone(), base.clone());
                        if args.verbose {
                            eprintln!("Tracking {} as merged into {}", &change_id[..8], base);
                        }
                    }
                }

                if let Some(pr_info) = state.prs.get(change_id) {
                    handle_out_of_order_merge(pr_info, &state, &repo_info, args.dry_run, args.verbose)?;
                }
            }

            // Re-fetch stack after rebasing
            revisions = get_stack_revisions(args.verbose)?;
            // Re-check for conflicts after rebase
            check_for_conflicts(&mut revisions, args.verbose)?;
        }

        // Handle PRs merged into other PRs but no longer in stack (just track them)
        for (_, change_id, base_branch) in &merged_into_others {
            if let Some(ref base) = base_branch {
                if base.starts_with("push-") && base != "main" {
                    // Track that this PR was merged into another PR branch
                    state.merged_into_pr.insert(change_id.clone(), base.clone());
                    if args.verbose {
                        eprintln!("PR {} was merged into {} (no longer in stack)", &change_id[..8], base);
                    }

                    // Mark this PR as merged in state
                    state.merged_prs.insert(change_id.clone());
                }
            }
        }
    }

    // Handle squashed commits
    if !squashed.is_empty() && args.verbose {
        eprintln!("Detected {} squashed commits", squashed.len());
    }

    // Handle split commits if detected
    if !splits.is_empty() {
        handle_split_commits(&splits, &mut revisions, &mut state, args.dry_run, args.verbose)?;
    }

    // Handle reordered stack if detected
    if reordered && args.verbose {
        eprintln!("Stack was reordered, updating PR bases...");
    }

    // Block on conflicts if any
    if !conflicts.is_empty() {
        eprintln!("\n⚠️  Cannot push: {} commit{} have conflicts",
                 conflicts.len(), if conflicts.len() == 1 { "" } else { "s" });
        for rev_id in &conflicts {
            if let Some(rev) = revisions.iter().find(|r| &r.change_id == rev_id) {
                eprintln!("  - {} ({})", rev.description, &rev.change_id[..8]);
            }
        }
        eprintln!("\nResolve conflicts and re-run almighty-push");
        bail!("Conflicts detected");
    }
    
    // Push branches with force-push detection
    push_branches(&mut revisions, args.dry_run, args.verbose)?;

    if !args.no_pr {
        // Try to reopen previously closed PRs if they're back in the stack
        reopen_prs(&mut revisions, &state, &repo_info, args.dry_run, args.verbose)?;

        // Create/update PRs
        create_or_update_prs(&mut revisions, &state, &repo_info, args.dry_run, args.verbose)?;

        // Detect and fix PR dependency cycles
        detect_and_fix_cycles(&revisions, &repo_info, args.dry_run, args.verbose)?;

        // Update PR descriptions with stack info
        update_pr_descriptions(&revisions, &repo_info, args.dry_run, args.verbose)?;

        // Close orphaned PRs (including squashed ones)
        close_orphaned_prs(&revisions, &mut state, &squashed, &repo_info, args.delete_branches, args.dry_run, args.verbose)?;
    }
    
    // Mark operation as successful
    track_operation_end(&mut state, &op_id, true)?;

    // Save state with garbage collection
    save_state(&mut state, &revisions)?;
    garbage_collect_state(&mut state)?;

    // Print summary
    if !args.no_pr {
        let open_count = revisions.iter().filter(|r| r.pr_state.as_deref() == Some("OPEN")).count();
        let merged_count = revisions.iter().filter(|r| r.pr_state.as_deref() == Some("MERGED")).count();

        if open_count > 0 || merged_count > 0 {
            eprintln!("\nStack: {} PRs ({} open, {} merged)",
                     revisions.len(), open_count, merged_count);
        }

        for rev in &revisions {
            if let Some(url) = &rev.pr_url {
                println!("{}", url);
            }
        }
    }

    Ok(())
}

// Lock management
fn acquire_lock() -> Result<FileLock> {
    FileLock::acquire()
}

struct FileLock {
    _file: File,
}

impl FileLock {
    fn acquire() -> Result<Self> {
        let start = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(LOCK_FILE) {
                Ok(mut file) => {
                    let pid = process::id();
                    writeln!(file, "{}", pid)?;
                    return Ok(Self { _file: file });
                }
                Err(_) if start.elapsed() > LOCK_TIMEOUT => {
                    bail!("Failed to acquire lock after {} seconds", LOCK_TIMEOUT.as_secs());
                }
                Err(_) => {
                    // Check if stale
                    if let Ok(mut file) = File::open(LOCK_FILE) {
                        let mut content = String::new();
                        file.read_to_string(&mut content)?;
                        if let Ok(_pid) = content.trim().parse::<u32>() {
                            // Simple check - in production would verify process exists
                            let age = fs::metadata(LOCK_FILE)?.modified()?;
                            if SystemTime::now().duration_since(age)? > Duration::from_secs(600) {
                                fs::remove_file(LOCK_FILE)?;
                                continue;
                            }
                        }
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(LOCK_FILE);
    }
}

fn get_stack_revisions(verbose: bool) -> Result<Vec<Revision>> {
    let output = run_command(&[
        "jj", "log", "-r", "main@origin..@", "--no-graph",
        "--template", r#"change_id ++ "|" ++ commit_id ++ "|" ++ if(description, description.first_line(), "(no description)") ++ "|" ++ if(conflict, "true", "false") ++ "|" ++ parents.map(|p| p.change_id()).join(",") ++ "\n""#
    ], false, verbose)?;

    let mut revisions = Vec::new();
    let mut skipped_count = 0;

    for line in output.lines() {
        if line.trim().is_empty() { continue; }
        let parts: Vec<&str> = line.split('|').collect();
        if parts.len() >= 5 {
            let change_id = parts[0].to_string();
            if change_id == "zzzzzzzzzzzz" { continue; } // Skip root

            let parent_ids = if parts[4].is_empty() {
                Vec::new()
            } else {
                parts[4].split(',').map(|s| s.to_string()).collect()
            };

            let description = parts[2].to_string();

            // Skip commits without descriptions as jj won't push them
            if description == "(no description)" {
                skipped_count += 1;
                if verbose {
                    eprintln!("  Skipping commit {} with no description", &change_id[..8]);
                }
                continue;
            }

            revisions.push(Revision {
                change_id,
                commit_id: parts[1].to_string(),
                description,
                has_conflicts: parts[3] == "true",
                parent_change_ids: parent_ids,
                branch_name: None,
                pr_number: None,
                pr_url: None,
                pr_state: None,
            });
        }
    }

    if skipped_count > 0 {
        eprintln!("⚠️  Skipped {} commit(s) without descriptions", skipped_count);
    }

    revisions.reverse(); // Bottom to top order
    Ok(revisions)
}

// Detect squashed commits by checking jj op log
fn detect_squashed_commits(revisions: &mut [Revision], _state: &State, verbose: bool) -> Result<HashSet<String>> {
    let mut squashed = HashSet::new();

    // Check operation log for squash operations
    let output = run_command(&[
        "jj", "op", "log", "--limit", "50", "--no-graph",
        "--template", r#"description ++ "\n""#
    ], true, verbose)?;

    for line in output.lines() {
        if line.contains("squash") || line.contains("abandon") {
            // Extract change IDs from operation description
            for word in line.split_whitespace() {
                if word.len() >= 8 && word.chars().all(|c| c.is_alphanumeric()) {
                    // Check if this looks like a change ID that's not in current stack
                    if !revisions.iter().any(|r| r.change_id.starts_with(word)) {
                        squashed.insert(word.to_string());
                    }
                }
            }
        }
    }

    Ok(squashed)
}

// Check for conflicts in revisions
fn check_for_conflicts(revisions: &mut [Revision], verbose: bool) -> Result<HashSet<String>> {
    let mut conflicts = HashSet::new();

    for rev in revisions.iter() {
        if rev.has_conflicts {
            conflicts.insert(rev.change_id.clone());
            if verbose {
                eprintln!("  Conflict detected in: {}", rev.description);
            }
        }
    }

    Ok(conflicts)
}

// Detect if stack was reordered
fn detect_reordered_stack(revisions: &[Revision], state: &State) -> Result<bool> {
    if state.stack_order.is_empty() {
        return Ok(false);
    }

    let current_order: Vec<String> = revisions.iter().map(|r| r.change_id.clone()).collect();
    Ok(current_order != state.stack_order)
}

// State migration
fn migrate_state(state: &mut State) -> Result<()> {
    if state.version < STATE_VERSION {
        eprintln!("Migrating state from version {} to {}", state.version, STATE_VERSION);
        state.version = STATE_VERSION;
        // Add migration logic here as needed
    }
    Ok(())
}

fn push_branches(revisions: &mut [Revision], dry_run: bool, verbose: bool) -> Result<()> {
    eprintln!("Pushing {} branches...", revisions.len());
    
    for rev in revisions {
        let branch_name = format!("push-{}", &rev.change_id[..12.min(rev.change_id.len())]);
        rev.branch_name = Some(branch_name.clone());
        
        if !dry_run {
            // Check if we need to force push
            let needs_force = check_needs_force_push(&branch_name, &rev.commit_id, verbose)?;

            if needs_force {
                if verbose {
                    eprintln!("  Force pushing {} (remote has diverged)", branch_name);
                }
                // jj automatically force pushes when needed, no --force flag required
                run_command(&["jj", "git", "push", "-b", &branch_name], false, verbose)?;
            } else {
                // Try to push normally
                let output = run_command(&["jj", "git", "push", "--change", &rev.change_id], true, verbose)?;
                if !output.contains("Creating") && !output.contains("Moving") {
                    // Try pushing by branch if change push failed
                    run_command(&["jj", "git", "push", "-b", &branch_name], true, verbose)?;
                }
            }
        }
    }
    
    Ok(())
}

// Check if force push is needed
fn check_needs_force_push(branch_name: &str, local_commit: &str, verbose: bool) -> Result<bool> {
    // Check if branch exists on remote
    let output = run_command(&[
        "jj", "log", "-r", &format!("{}@origin", branch_name),
        "--no-graph", "--template", "commit_id", "--limit", "1"
    ], true, verbose)?;

    if output.trim().is_empty() || output.contains("doesn't exist") || output.contains("Error:") {
        return Ok(false); // New branch or doesn't exist on remote
    }

    let remote_commit = output.trim();
    if remote_commit == local_commit {
        return Ok(false); // Same commit
    }

    // Check if remote is ancestor of local (normal push)
    let output = run_command(&[
        "jj", "log", "-r", &format!("{}::{}", remote_commit, local_commit),
        "--no-graph", "--limit", "1"
    ], true, verbose)?;

    // If output contains error or is empty, need force push
    Ok(output.trim().is_empty() || output.contains("Error:"))
}

fn create_or_update_prs(revisions: &mut [Revision], state: &State, repo: &str, dry_run: bool, verbose: bool) -> Result<()> {
    eprintln!("Managing pull requests...");

    // Get existing PRs
    let existing_prs = get_existing_prs(repo, verbose)?;

    // First pass: determine base branches
    let mut base_branches = Vec::new();
    for i in 0..revisions.len() {
        let base = if i == 0 {
            "main".to_string()
        } else {
            // Check if the previous revision was merged into another PR branch
            // This handles the case where PRs are merged into each other rather than main
            let prev_change_id = &revisions[i-1].change_id;
            if let Some(merged_into_branch) = state.merged_into_pr.iter()
                .find(|(id, _)| id.starts_with(prev_change_id) || prev_change_id.starts_with(id.as_str()))
                .map(|(_, branch)| branch.clone()) {
                // The previous PR was merged into another branch, use that as the base
                merged_into_branch
            } else if revisions[i].parent_change_ids.len() > 1 {
                // Handle merge commits with multiple parents
                let primary_parent = &revisions[i].parent_change_ids[0];
                if let Some(parent_rev) = revisions.iter().find(|r| r.change_id == *primary_parent) {
                    parent_rev.branch_name.clone().unwrap_or_else(|| "main".to_string())
                } else {
                    revisions[i-1].branch_name.as_ref().unwrap().clone()
                }
            } else {
                revisions[i-1].branch_name.as_ref().unwrap().clone()
            }
        };
        base_branches.push(base);
    }

    // Collect PR info from previous revisions to avoid borrow conflicts
    let prev_pr_info: Vec<(Option<u32>, Option<String>)> = revisions.iter()
        .map(|r| (r.pr_number, r.pr_state.clone()))
        .collect();

    // Second pass: create/update PRs
    for (i, rev) in revisions.iter_mut().enumerate() {
        let branch_name = rev.branch_name.as_ref().context("No branch name")?;
        let base_branch = &base_branches[i];

        // Check if this commit represents a PR that was merged into another PR
        // This happens when PRs are merged into each other rather than main
        // The merged commit will have the PR number in its description (e.g., "second (#31)")
        let pr_regex = regex::Regex::new(r"\(#(\d+)\)").unwrap();
        let mut skip_pr_creation = false;

        // First check if this is the HEAD of an existing PR
        // This happens after merging one PR into another - the merged commit becomes the new HEAD
        if i > 0 {
            // Check if the previous revision has a PR and if this commit is now its HEAD
            if let Some(prev_pr_num) = prev_pr_info[i-1].0 {
                // Check if this commit is the current HEAD of that PR's branch
                let pr_head_output = run_command(&[
                    "gh", "pr", "view", &prev_pr_num.to_string(),
                    "-R", repo,
                    "--json", "headRefName", "-q", ".headRefName"
                ], true, verbose)?;

                let pr_branch = pr_head_output.trim();
                if !pr_branch.is_empty() {
                    // Check if this commit is the HEAD of that branch
                    let branch_head = run_command(&[
                        "jj", "log", "-r", &format!("{}@origin", pr_branch),
                        "--no-graph", "--template", "change_id", "--limit", "1"
                    ], true, verbose)?;

                    if branch_head.trim().starts_with(&rev.change_id) || rev.change_id.starts_with(branch_head.trim()) {
                        skip_pr_creation = true;
                        // This commit is part of the previous PR
                        rev.pr_number = Some(prev_pr_num);
                        rev.pr_state = prev_pr_info[i-1].1.clone();
                        if verbose {
                            eprintln!("  Skipping PR creation for {} - already HEAD of PR #{}",
                                     &rev.change_id[..8], prev_pr_num);
                        }
                    }
                }
            }
        }

        // Also check if the description indicates this was a merged PR
        if !skip_pr_creation {
            if let Some(captures) = pr_regex.captures(&rev.description) {
                if let Some(pr_num_str) = captures.get(1) {
                    if let Ok(pr_num) = pr_num_str.as_str().parse::<u32>() {
                        // Check if this PR was merged
                        let pr_status = run_command(&[
                            "gh", "pr", "view", &pr_num.to_string(),
                            "-R", repo,
                            "--json", "state,mergedAt", "-q", ".state"
                        ], true, verbose)?;

                        if pr_status.trim() == "MERGED" {
                            skip_pr_creation = true;
                            rev.pr_number = Some(pr_num);
                            rev.pr_state = Some("MERGED".to_string());
                            if verbose {
                                eprintln!("  Skipping PR creation for {} - PR #{} was already merged",
                                         &rev.change_id[..8], pr_num);
                            }
                        }
                    }
                }
            }
        }

        if skip_pr_creation {
            continue;
        }

        // Check if PR exists by branch name
        if let Some(pr) = existing_prs.get(branch_name) {
            rev.pr_number = Some(pr.0);
            rev.pr_url = Some(pr.1.clone());
            rev.pr_state = Some(pr.2.clone());

            // Update base if needed and PR is open
            if pr.2 == "OPEN" && &pr.3 != base_branch && !dry_run {
                if verbose {
                    eprintln!("  Updating PR #{} base from {} to {}", pr.0, pr.3, base_branch);
                }
                run_command(&["gh", "pr", "edit", &pr.0.to_string(), "-R", repo, "--base", base_branch], true, verbose)?;
            }
        }
        // Also check if we have a PR for this change ID in state (might have different branch name)
        else if let Some(existing_pr) = state.prs.iter()
            .find(|(id, _)| id.starts_with(&rev.change_id) || rev.change_id.starts_with(id.as_str()))
            .map(|(_, info)| info) {

            // PR exists in state but not found by branch name - might have been renamed
            rev.pr_number = Some(existing_pr.pr_number);
            rev.pr_url = Some(existing_pr.pr_url.clone());

            if verbose {
                eprintln!("  Found existing PR #{} for change {}", existing_pr.pr_number, &rev.change_id[..8]);
            }
        } else if !dry_run {
            // Create new PR
            let title = &rev.description;

            // Build PR body with merge commit info if applicable
            let mut body = format!("Change ID: {}\n\n", rev.change_id);

            if rev.parent_change_ids.len() > 1 {
                body.push_str("**Note**: This is a merge commit with multiple parents:\n");
                for (idx, parent_id) in rev.parent_change_ids.iter().enumerate() {
                    if idx == 0 {
                        body.push_str(&format!("- Primary: `{}`\n", &parent_id[..12.min(parent_id.len())]));
                    } else {
                        body.push_str(&format!("- Additional: `{}`\n", &parent_id[..12.min(parent_id.len())]));
                    }
                }
                body.push('\n');
            }

            let output = run_command(&[
                "gh", "pr", "create",
                "-R", repo,
                "--head", branch_name,
                "--base", base_branch,
                "--title", title,
                "--body", &body,
            ], false, verbose)?;

            // Extract PR URL
            if let Some(url) = output.lines().find(|l| l.contains("github.com")) {
                rev.pr_url = Some(url.to_string());
                if let Some(num) = url.split('/').last() {
                    rev.pr_number = num.parse().ok();
                }
            }
        }
    }

    Ok(())
}

// Detect and fix PR dependency cycles
fn detect_and_fix_cycles(revisions: &[Revision], repo: &str, dry_run: bool, verbose: bool) -> Result<()> {
    let mut dependencies = HashMap::new();
    for (i, rev) in revisions.iter().enumerate() {
        if let Some(pr_num) = rev.pr_number {
            if i > 0 {
                if let Some(prev_pr) = revisions[i-1].pr_number {
                    dependencies.insert(pr_num, prev_pr);
                }
            }
        }
    }

    // Simple cycle detection using visited set
    for &start in dependencies.keys() {
        let mut visited = HashSet::new();
        let mut current = start;

        while let Some(&next) = dependencies.get(&current) {
            if !visited.insert(current) {
                // Cycle detected
                if verbose {
                    eprintln!("  Cycle detected involving PR #{}", current);
                }
                if !dry_run {
                    // Break cycle by updating base to main
                    run_command(&[
                        "gh", "pr", "edit", &current.to_string(),
                        "-R", repo,
                        "--base", "main"
                    ], true, verbose)?;
                }
                break;
            }
            current = next;
        }
    }

    Ok(())
}

fn update_pr_descriptions(revisions: &[Revision], repo: &str, dry_run: bool, verbose: bool) -> Result<()> {
    eprintln!("Updating PR descriptions...");
    
    for (i, rev) in revisions.iter().enumerate() {
        if let Some(pr_number) = rev.pr_number {
            // Skip merged/closed PRs
            if let Some(state) = &rev.pr_state {
                if state != "OPEN" { continue; }
            }
            
            let mut body = String::new();
            body.push_str("## Stack\n\n");
            
            for (j, r) in revisions.iter().enumerate() {
                let marker = if i == j { "→" } else { "  " };
                let state_icon = match r.pr_state.as_deref() {
                    Some("MERGED") => "✓",
                    Some("CLOSED") => "✗",
                    _ => "",
                };
                body.push_str(&format!("{} #{}: {} {}\n", 
                    marker, 
                    r.pr_number.unwrap_or(0), 
                    r.description,
                    state_icon
                ));
            }
            
            body.push_str(&format!("\n---\nChange ID: `{}`\n", rev.change_id));
            
            if !dry_run {
                run_command(&["gh", "pr", "edit", &pr_number.to_string(), "-R", repo, "--body", &body], true, verbose)?;
            }
        }
    }
    
    Ok(())
}

fn detect_merged_prs(revisions: &mut [Revision], state: &State, repo: &str, verbose: bool) -> Result<Vec<(usize, String, Option<String>)>> {
    let mut merged = Vec::new();

    // Check PRs from state
    for (change_id, pr_info) in &state.prs {
        // Check if PR is merged on GitHub and get its base branch
        let output = run_command(&[
            "gh", "pr", "view", &pr_info.pr_number.to_string(),
            "-R", repo,
            "--json", "state,mergedAt,baseRefName"
        ], true, verbose)?;

        if output.contains("\"mergedAt\":") && !output.contains("\"mergedAt\":null") || output.contains("\"state\":\"MERGED\"") {
            // Extract base branch from JSON
            let base_branch = if let Ok(json) = serde_json::from_str::<serde_json::Value>(&output) {
                json["baseRefName"].as_str().map(String::from)
            } else {
                None
            };

            // Find position in current stack using prefix matching
            if let Some(pos) = revisions.iter().position(|r| {
                change_id.starts_with(&r.change_id) || r.change_id.starts_with(change_id)
            }) {
                merged.push((pos, change_id.clone(), base_branch.clone()));
                revisions[pos].pr_state = Some("MERGED".to_string());
            }

            // If merged but not in current stack, it might have been merged into another PR
            // We still need to track this for later
            if revisions.iter().position(|r| {
                change_id.starts_with(&r.change_id) || r.change_id.starts_with(change_id)
            }).is_none() && base_branch.is_some() {
                // This PR was merged but is no longer in the stack
                // It might have been incorporated into another branch
                merged.push((usize::MAX, change_id.clone(), base_branch));
            }
        }
    }

    Ok(merged)
}

fn handle_merged_prs(merged: &[(usize, String, Option<String>)], revisions: &mut Vec<Revision>, verbose: bool) -> Result<()> {
    eprintln!("Handling {} merged PRs...", merged.len());

    // Filter out merged PRs that are no longer in the stack (marked with usize::MAX)
    // and sort remaining by position (top to bottom) to handle out-of-order merges
    let mut sorted_merged: Vec<_> = merged.iter()
        .filter(|(idx, _, _)| *idx != usize::MAX)
        .cloned()
        .collect();
    sorted_merged.sort_by_key(|(idx, _, _)| *idx);

    for (idx, change_id, base_branch) in sorted_merged {
        if verbose {
            eprintln!("  Processing merged PR at position {} (change {})", idx, &change_id[..8]);
            if let Some(ref base) = base_branch {
                eprintln!("    Merged into: {}", base);
            }
        }

        if idx + 1 < revisions.len() {
            // Rebase commits above the merged one
            let source = &revisions[idx + 1].change_id;

            // Determine destination based on where this PR was merged
            let destination = if let Some(ref base) = base_branch {
                if base.starts_with("push-") && base != "main" {
                    // PR was merged into another PR branch - rebase onto that branch's current state
                    if verbose {
                        eprintln!("    PR was merged into another PR branch ({}), rebasing onto {}@origin", base, base);
                    }
                    format!("{}@origin", base)
                } else {
                    // PR was merged into main
                    "main@origin".to_string()
                }
            } else if idx == 0 {
                "main@origin".to_string()
            } else {
                // For out-of-order merges to main, find the previous unmerged commit
                let mut dest_idx = idx - 1;
                while dest_idx > 0 && revisions[dest_idx].pr_state.as_deref() == Some("MERGED") {
                    dest_idx -= 1;
                }

                if revisions[dest_idx].pr_state.as_deref() == Some("MERGED") {
                    "main@origin".to_string()
                } else {
                    revisions[dest_idx].change_id.clone()
                }
            };

            if verbose {
                eprintln!("  Rebasing {} onto {}", &source[..8], destination);
            }
            run_command(&["jj", "rebase", "-s", source, "-d", &destination], false, verbose)?;
        }
    }

    Ok(())
}

fn close_orphaned_prs(current: &[Revision], state: &mut State, squashed: &HashSet<String>, repo: &str, delete_branches: bool, dry_run: bool, verbose: bool) -> Result<()> {
    let current_change_ids: HashSet<_> = current.iter().map(|r| r.change_id.clone()).collect();

    for (change_id, pr_info) in &state.prs {
        // Check if this PR's change is still in the stack
        // Compare using prefix matching since jj may return short change IDs
        let still_in_stack = current_change_ids.iter().any(|current_id| {
            change_id.starts_with(current_id) || current_id.starts_with(change_id)
        });

        let is_merged = state.merged_prs.iter().any(|merged_id| {
            change_id.starts_with(merged_id) || merged_id.starts_with(change_id)
        });

        let was_squashed = squashed.iter().any(|s| change_id.starts_with(s));

        // Close if: removed from stack (and not merged), or was squashed
        let should_close = (!still_in_stack && !is_merged) || was_squashed;

        if should_close {
            if !dry_run {
                // First check PR state to avoid closing already closed/merged PRs
                let pr_status = run_command(&[
                    "gh", "pr", "view", &pr_info.pr_number.to_string(),
                    "-R", repo,
                    "--json", "state", "-q", ".state"
                ], true, verbose)?;

                let status = pr_status.trim();
                if status == "OPEN" {
                    eprintln!("Closing orphaned PR #{}", pr_info.pr_number);

                    let comment = if squashed.iter().any(|s| change_id.starts_with(s)) {
                        "This PR was closed because the commit was squashed"
                    } else {
                        "This PR was closed because the commit was removed from the stack"
                    };

                    run_command(&[
                        "gh", "pr", "close", &pr_info.pr_number.to_string(),
                        "-R", repo,
                        "--comment", comment
                    ], true, verbose)?;

                    // Track closed PR for potential reopening
                    state.closed_prs.insert(change_id.clone());

                    if delete_branches {
                        run_command(&[
                            "jj", "git", "push", "-b", &pr_info.branch_name, "--delete"
                        ], true, verbose)?;
                    }
                } else if verbose {
                    eprintln!("  Skipping PR #{} (already {})", pr_info.pr_number, status.to_lowercase());
                }
            } else {
                eprintln!("Would close orphaned PR #{}", pr_info.pr_number);
            }
        }
    }

    Ok(())
}

// Reopen previously closed PRs if they're back in the stack
fn reopen_prs(revisions: &mut [Revision], state: &State, repo: &str, dry_run: bool, verbose: bool) -> Result<()> {
    for rev in revisions {
        // Check if this change was previously closed (using prefix matching)
        let was_closed = state.closed_prs.iter().any(|closed_id| {
            closed_id.starts_with(&rev.change_id) || rev.change_id.starts_with(closed_id)
        });

        if was_closed {
            // Look for the closed PR (using prefix matching)
            let pr_info = state.prs.iter()
                .find(|(id, _)| id.starts_with(&rev.change_id) || rev.change_id.starts_with(id.as_str()))
                .map(|(_, info)| info);

            if let Some(pr_info) = pr_info {
                if verbose {
                    eprintln!("Reopening previously closed PR #{} for {}",
                             pr_info.pr_number, &rev.change_id[..8]);
                }

                if !dry_run {
                    // Check if PR is actually closed
                    let pr_status = run_command(&[
                        "gh", "pr", "view", &pr_info.pr_number.to_string(),
                        "-R", repo,
                        "--json", "state", "-q", ".state"
                    ], true, verbose)?;

                    if pr_status.trim() == "CLOSED" {
                        // Reopen the PR
                        let result = run_command(&[
                            "gh", "pr", "reopen", &pr_info.pr_number.to_string(),
                            "-R", repo
                        ], true, verbose);

                        if result.is_ok() {
                            // Update revision with PR info
                            rev.pr_number = Some(pr_info.pr_number);
                            rev.pr_url = Some(pr_info.pr_url.clone());
                            rev.pr_state = Some("OPEN".to_string());
                            eprintln!("  Successfully reopened PR #{}", pr_info.pr_number);
                        } else if verbose {
                            eprintln!("  Failed to reopen PR #{}", pr_info.pr_number);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn get_existing_prs(repo: &str, verbose: bool) -> Result<HashMap<String, (u32, String, String, String)>> {
    let output = run_command(&[
        "gh", "pr", "list", "-R", repo, "--state", "all", "--limit", "1000",
        "--json", "number,url,state,headRefName,baseRefName"
    ], true, verbose)?;
    
    let mut prs = HashMap::new();
    
    if let Ok(json) = serde_json::from_str::<Vec<serde_json::Value>>(&output) {
        for pr in json {
            if let (Some(head_ref), Some(number), Some(url), Some(state), Some(base_ref)) = (
                pr["headRefName"].as_str(),
                pr["number"].as_u64(),
                pr["url"].as_str(),
                pr["state"].as_str(),
                pr["baseRefName"].as_str(),
            ) {
                if head_ref.starts_with("push-") {
                    prs.insert(
                        head_ref.to_string(), 
                        (number as u32, url.to_string(), state.to_string(), base_ref.to_string())
                    );
                }
            }
        }
    }
    
    Ok(prs)
}

fn load_state() -> Result<State> {
    match fs::read_to_string(".almighty") {
        Ok(content) => serde_json::from_str(&content).context("Failed to parse state"),
        Err(_) => Ok(State::default()),
    }
}

fn save_state(state: &mut State, revisions: &[Revision]) -> Result<()> {
    state.version = STATE_VERSION;
    state.last_updated = Some(chrono::Utc::now().to_rfc3339());
    // Save current stack order
    state.stack_order = revisions.iter().map(|r| r.change_id.clone()).collect();

    // Update PRs in state, preserving full change IDs where we have them
    let mut new_prs = HashMap::new();
    for rev in revisions {
        if let Some(pr_number) = rev.pr_number {
            // Try to find existing entry with full change ID
            let full_change_id = state.prs.iter()
                .find(|(id, info)| {
                    info.pr_number == pr_number ||
                    id.starts_with(&rev.change_id) ||
                    rev.change_id.starts_with(id.as_str())
                })
                .map(|(id, _)| id.clone())
                .unwrap_or_else(|| {
                    // If no existing entry, use the change ID we have
                    // (it might be short from jj, but that's OK)
                    rev.change_id.clone()
                });

            new_prs.insert(
                full_change_id.clone(),
                PrInfo {
                    pr_number,
                    pr_url: rev.pr_url.clone().unwrap_or_default(),
                    branch_name: rev.branch_name.clone().unwrap_or_default(),
                    commit_id: rev.commit_id.clone(),
                    change_id: Some(full_change_id),
                },
            );
            
            if let Some(st) = &rev.pr_state {
                if st == "MERGED" {
                    state.merged_prs.insert(rev.change_id.clone());
                } else if st == "CLOSED" {
                    state.closed_prs.insert(rev.change_id.clone());
                }
            }
        }
    }

    // Replace the PRs map with the new one
    state.prs = new_prs;

    let content = serde_json::to_string_pretty(&state)?;
    fs::write(".almighty", content)?;
    Ok(())
}

// Extract GitHub repo info from jj remote
fn get_repo_info(verbose: bool) -> Result<String> {
    let output = run_command(&["jj", "git", "remote", "list"], false, verbose)?;

    for line in output.lines() {
        if line.starts_with("origin") {
            // Parse GitHub URL formats:
            // - git@github.com:owner/repo.git
            // - https://github.com/owner/repo.git
            // - https://github.com/owner/repo
            let url = line.split_whitespace().nth(1).unwrap_or("");

            if let Some(repo) = extract_github_repo(url) {
                return Ok(repo);
            }
        }
    }

    bail!("Could not determine GitHub repository from jj remotes")
}

fn extract_github_repo(url: &str) -> Option<String> {
    // Handle git@github.com:owner/repo.git
    if url.starts_with("git@github.com:") {
        let path = url.strip_prefix("git@github.com:")?;
        let repo = path.strip_suffix(".git").unwrap_or(path);
        return Some(repo.to_string());
    }

    // Handle https://github.com/owner/repo[.git]
    if url.contains("github.com/") {
        let parts: Vec<&str> = url.split("github.com/").collect();
        if parts.len() > 1 {
            let repo = parts[1].strip_suffix(".git").unwrap_or(parts[1]);
            return Some(repo.to_string());
        }
    }

    None
}

fn run_command(args: &[&str], ignore_errors: bool, verbose: bool) -> Result<String> {
    if verbose {
        eprintln!("[debug] Running: {}", args.join(" "));
    }

    let output = Command::new(args[0])
        .args(&args[1..])
        .output()
        .with_context(|| format!("Failed to run: {}", args.join(" ")))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if verbose && (!stderr.is_empty() || !output.status.success()) {
        eprintln!("[debug] stderr: {}", stderr);
    }

    if !output.status.success() && !ignore_errors {
        bail!("Command failed: {}\nStderr: {}", args.join(" "), stderr);
    }

    Ok(stdout + &stderr)
}

// Track operation start for recovery
fn track_operation_start(state: &mut State, op_type: &str, revisions: &[Revision]) -> Result<String> {
    let op_id = format!("op-{}", SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs());
    let timestamp = chrono::Utc::now().to_rfc3339();

    state.operations.push(Operation {
        id: op_id.clone(),
        op_type: op_type.to_string(),
        timestamp,
        changes_affected: revisions.iter().map(|r| r.change_id.clone()).collect(),
        success: false,
    });

    // Keep only last 50 operations
    if state.operations.len() > 50 {
        state.operations = state.operations.split_off(state.operations.len() - 50);
    }

    Ok(op_id)
}

// Mark operation as completed
fn track_operation_end(state: &mut State, op_id: &str, success: bool) -> Result<()> {
    if let Some(op) = state.operations.iter_mut().find(|o| o.id == op_id) {
        op.success = success;
    }
    state.last_operation_id = Some(op_id.to_string());
    Ok(())
}

// Detect split commits
fn detect_split_commits(current: &[Revision], _state: &State, verbose: bool) -> Result<Vec<SplitOperation>> {
    let mut splits = Vec::new();
    let split_pattern = regex::Regex::new(r"^\((\d+)/(\d+)\)\s+(.+)").unwrap();

    // Group commits by base description
    let mut groups: HashMap<String, Vec<&Revision>> = HashMap::new();

    for rev in current {
        if let Some(captures) = split_pattern.captures(&rev.description) {
            let base_msg = captures.get(3).unwrap().as_str().to_string();
            groups.entry(base_msg).or_default().push(rev);
        }
    }

    // Create split operations for grouped commits
    for (base_msg, revs) in groups {
        if revs.len() > 1 {
            if verbose {
                eprintln!("  Detected split commit: '{}' split into {} parts", base_msg, revs.len());
            }
            splits.push(SplitOperation {
                original_message: base_msg,
                new_change_ids: revs.iter().map(|r| r.change_id.clone()).collect(),
            });
        }
    }

    Ok(splits)
}

#[derive(Debug)]
struct SplitOperation {
    original_message: String,
    new_change_ids: Vec<String>,
}

// Handle split commits
fn handle_split_commits(
    splits: &[SplitOperation],
    revisions: &mut [Revision],
    _state: &mut State,
    dry_run: bool,
    verbose: bool
) -> Result<()> {
    for split in splits {
        if verbose {
            eprintln!("Handling split commit: {} -> {} parts",
                     split.original_message, split.new_change_ids.len());
        }

        // Mark revisions as part of a split
        for rev in revisions.iter_mut() {
            if split.new_change_ids.contains(&rev.change_id) {
                // Add note to PR description about split
                if !dry_run && rev.pr_number.is_some() {
                    // This will be handled in PR description update
                    if verbose {
                        eprintln!("  Marking {} as part of split", &rev.change_id[..8]);
                    }
                }
            }
        }
    }
    Ok(())
}

// Handle out-of-order merged PRs
fn handle_out_of_order_merge(
    merged_pr: &PrInfo,
    state: &State,
    repo: &str,
    dry_run: bool,
    verbose: bool
) -> Result<()> {
    // Find PRs that depend on the merged one
    let children: Vec<_> = state.prs.iter()
        .filter(|(change_id, p)| {
            // Check if this PR's base branch matches the merged PR's branch
            p.branch_name != merged_pr.branch_name &&
            state.stack_order.iter()
                .position(|id| id == *change_id)
                .map(|pos| {
                    // Find merged PR's change_id by matching the PrInfo
                    let merged_change_id = state.prs.iter()
                        .find(|(_, pr)| pr.pr_number == merged_pr.pr_number)
                        .map(|(id, _)| id);

                    if let Some(merged_id) = merged_change_id {
                        pos > state.stack_order.iter()
                            .position(|id| id == merged_id)
                            .unwrap_or(usize::MAX)
                    } else {
                        false
                    }
                })
                .unwrap_or(false)
        })
        .map(|(_, p)| p)
        .collect();

    if children.is_empty() {
        return Ok(());
    }

    if verbose {
        eprintln!("  Handling out-of-order merge for PR #{}", merged_pr.pr_number);
        eprintln!("  Found {} dependent PRs to update", children.len());
    }

    // Determine new base
    // Find merged PR's change_id
    let merged_change_id = state.prs.iter()
        .find(|(_, pr)| pr.pr_number == merged_pr.pr_number)
        .map(|(id, _)| id.clone());

    let new_base = if let Some(merged_id) = merged_change_id {
        if let Some(parent_pos) = state.stack_order.iter()
            .position(|id| id == &merged_id)
            .and_then(|pos| if pos > 0 { Some(pos - 1) } else { None }) {

            state.prs.get(&state.stack_order[parent_pos])
                .map(|p| p.branch_name.clone())
                .unwrap_or_else(|| "main".to_string())
        } else {
            "main".to_string()
        }
    } else {
        "main".to_string()
    };

    // Update children bases
    for child in children {
        if verbose {
            eprintln!("    Updating PR #{} base to {}", child.pr_number, new_base);
        }

        if !dry_run {
            run_command(&[
                "gh", "pr", "edit", &child.pr_number.to_string(),
                "-R", repo,
                "--base", &new_base
            ], true, verbose)?;
        }
    }

    Ok(())
}

// Garbage collect old state entries
fn garbage_collect_state(state: &mut State) -> Result<()> {
    let cutoff = SystemTime::now() - Duration::from_secs(30 * 24 * 60 * 60); // 30 days

    // Remove old closed PRs
    state.closed_prs.retain(|change_id| {
        // Keep if we have recent activity
        state.operations.iter()
            .filter(|op| op.changes_affected.contains(change_id))
            .any(|op| {
                chrono::DateTime::parse_from_rfc3339(&op.timestamp)
                    .ok()
                    .and_then(|dt| {
                        SystemTime::now().duration_since(UNIX_EPOCH).ok()
                            .map(|_now| {
                                let op_time = dt.timestamp() as u64;
                                let cutoff_time = cutoff.duration_since(UNIX_EPOCH).unwrap().as_secs();
                                op_time > cutoff_time
                            })
                    })
                    .unwrap_or(false)
            })
    });

    // Remove old operations
    if state.operations.len() > 100 {
        state.operations = state.operations.split_off(state.operations.len() - 100);
    }

    Ok(())
}

