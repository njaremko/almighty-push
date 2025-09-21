mod almighty;
mod command;
mod constants;
mod edge_cases;
mod github;
mod jj;
mod state;
mod types;

use almighty::AlmightyPush;
use anyhow::Result;
use clap::Parser;
use command::CommandExecutor;
use constants::{DEFAULT_BASE_BRANCH, PUSH_BRANCH_PREFIX, CHANGES_BRANCH_PREFIX};
use github::GitHubClient;
use jj::JujutsuClient;
use state::StateManager;

/// Automated jj stack pusher and PR creator for GitHub
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about,
    long_about = "Almighty Push - Automated jj stack pusher and PR creator for GitHub.
Pushes all changes in current stack above main and creates properly stacked PRs."
)]
#[command(after_help = "Examples:
  almighty-push                    # Push stack and create/update PRs
  almighty-push --dry-run          # Show what would be done
  almighty-push --no-pr            # Only push branches
  almighty-push --delete-branches  # Also delete orphaned branches")]
struct Args {
    /// Show what would be done without actually doing it
    #[arg(long)]
    dry_run: bool,

    /// Only push branches, don't create or update PRs
    #[arg(long)]
    no_pr: bool,

    /// Don't close PRs for squashed or removed commits
    #[arg(long)]
    no_close_orphaned: bool,

    /// Delete remote branches when closing orphaned PRs (default: keep branches)
    #[arg(long)]
    delete_branches: bool,

    /// Enable verbose output
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    if args.verbose {
        eprintln!("almighty-push (verbose mode)");
    }

    // Initialize components
    let executor = CommandExecutor::new_verbose(args.verbose).with_dry_run(args.dry_run);
    let jj_client = JujutsuClient::new(executor.clone());
    let state_manager = StateManager::new();
    let github_client = GitHubClient::new(executor.clone(), state_manager);
    let mut almighty = AlmightyPush::new(executor, jj_client, github_client, StateManager::new());

    // Run the main logic
    match run_almighty(args, &mut almighty) {
        Ok(()) => Ok(()),
        Err(e) => {
            eprintln!("\nerror: {}", e);
            std::process::exit(1);
        }
    }
}

fn run_almighty(args: Args, almighty: &mut AlmightyPush) -> Result<()> {
    // We need to create a new JujutsuClient for getting revisions
    let executor =
        CommandExecutor::new_verbose(almighty.executor.verbose).with_dry_run(args.dry_run);
    let jj_client = JujutsuClient::new(executor.clone());

    // Fetch latest changes from remote
    if args.verbose {
        eprintln!("Fetching from remote...");
    }
    executor.run(&["jj", "git", "fetch"])?;

    // Get revisions in the stack
    let mut revisions = jj_client.get_revisions_above_base(DEFAULT_BASE_BRANCH)?;

    // Detect and handle edge cases early (if we have revisions)
    let recovery_plan = if !revisions.is_empty() {
        Some(almighty.detect_and_handle_edge_cases(&revisions)?)
    } else {
        None
    };

    if revisions.is_empty() {
        if args.verbose {
            eprintln!("No revisions to push");
        }

        // Still check for orphaned PRs even if no new revisions
        if !args.no_pr && !args.no_close_orphaned {
            let closed_prs = almighty.close_orphaned_prs(&[], None, args.delete_branches)?;

            // Save state even when no revisions
            if !args.dry_run {
                almighty.save_state(&[], &closed_prs)?;
            }
        }

        return Ok(());
    }

    if args.dry_run {
        eprintln!("\n[DRY RUN] No changes will be made");
    }

    // Early detection of merged PRs for rebasing
    // We need to get existing branches and populate PR states first
    let existing_branches = {
        let mut github_client = GitHubClient::new(executor.clone(), StateManager::new());
        github_client.get_existing_branches(false)?
    };

    // Assign branch names to revisions so we can check PR states
    for rev in revisions.iter_mut() {
        for branch_name in existing_branches.keys() {
            let prefix_options = [PUSH_BRANCH_PREFIX, CHANGES_BRANCH_PREFIX];
            for prefix in &prefix_options {
                if let Some(stripped) = branch_name.strip_prefix(prefix) {
                    let len = stripped.len().min(rev.change_id.len());
                    if stripped[..len].eq_ignore_ascii_case(&rev.change_id[..len]) {
                        rev.branch_name = Some(branch_name.clone());
                        break;
                    }
                }
            }
            if rev.branch_name.is_some() {
                break;
            }
        }
    }

    // Load PR cache and populate states to detect merged PRs
    {
        let mut github_client = GitHubClient::new(executor.clone(), StateManager::new());
        github_client.load_pr_cache()?;
        github_client.populate_pr_states(&mut revisions)?;
    }

    // Check if we need to rebase to skip merged commits
    let did_rebase = almighty.rebase_stack_over_merged(&revisions)?;

    if did_rebase {
        // Refresh the revision list after rebasing
        revisions = jj_client.get_revisions_above_base(DEFAULT_BASE_BRANCH)?;

        // Re-assign branch names and PR states after refresh
        for rev in revisions.iter_mut() {
            for branch_name in existing_branches.keys() {
                let prefix_options = [PUSH_BRANCH_PREFIX, CHANGES_BRANCH_PREFIX];
                for prefix in &prefix_options {
                    if let Some(stripped) = branch_name.strip_prefix(prefix) {
                        let len = stripped.len().min(rev.change_id.len());
                        if stripped[..len].eq_ignore_ascii_case(&rev.change_id[..len]) {
                            rev.branch_name = Some(branch_name.clone());
                            break;
                        }
                    }
                }
                if rev.branch_name.is_some() {
                    break;
                }
            }
        }

        // Re-populate PR states after refresh
        let mut github_client = GitHubClient::new(executor.clone(), StateManager::new());
        github_client.load_pr_cache()?;
        github_client.populate_pr_states(&mut revisions)?;
    }

    // Push all revisions
    let existing_branches = almighty.push_revisions(&mut revisions)?;

    if !args.no_pr {
        // Close orphaned PRs first (before creating new ones)
        let closed_prs = if !args.no_close_orphaned {
            almighty.close_orphaned_prs(
                &revisions,
                Some(&existing_branches),
                args.delete_branches,
            )?
        } else {
            Vec::new()
        };

        // Create PRs
        almighty.create_pull_requests(&mut revisions)?;

        // Update PR bases to create proper stack
        almighty.verify_pr_bases(&revisions)?;

        // Apply recovery plan if we have one
        if let Some(recovery_plan) = &recovery_plan {
            if !args.dry_run {
                almighty.apply_recovery_plan(recovery_plan, &revisions)?;
            }
        }

        // Update PR titles and bodies with stack information
        almighty.update_pr_details(&mut revisions)?;

        // Save state for next run
        if !args.dry_run {
            almighty.save_state(&revisions, &closed_prs)?;
        }
    }

    // Summary - show all PRs in the stack
    let all_prs: Vec<&types::Revision> = revisions
        .iter()
        .filter(|r| r.pr_url.is_some())
        .collect();

    if !all_prs.is_empty() {
        // Count PR states
        let open_count = all_prs.iter().filter(|r|
            matches!(r.pr_state, Some(types::PrState::Open) | None)
        ).count();
        let merged_count = all_prs.iter().filter(|r|
            matches!(r.pr_state, Some(types::PrState::Merged))
        ).count();
        let closed_count = all_prs.iter().filter(|r|
            matches!(r.pr_state, Some(types::PrState::Closed))
        ).count();

        eprintln!();
        let mut summary = format!("Stack: {} PR{}", all_prs.len(), if all_prs.len() == 1 { "" } else { "s" });

        let mut parts = Vec::new();
        if open_count > 0 {
            parts.push(format!("{} open", open_count));
        }
        if merged_count > 0 {
            parts.push(format!("{} merged", merged_count));
        }
        if closed_count > 0 {
            parts.push(format!("{} closed", closed_count));
        }

        if !parts.is_empty() && all_prs.len() > 1 {
            summary.push_str(&format!(" ({})", parts.join(", ")));
        }

        eprintln!("{}", summary);

        // Show detailed stack in verbose mode
        if args.verbose {
            for (idx, rev) in all_prs.iter().enumerate() {
                if let Some(pr_url) = &rev.pr_url {
                    let pr_number = rev
                        .pr_number
                        .unwrap_or_else(|| rev.extract_pr_number().unwrap_or(0));
                    let state_marker = match &rev.pr_state {
                        Some(types::PrState::Merged) => " ✓",
                        Some(types::PrState::Closed) => " ✗",
                        _ => "",
                    };
                    eprintln!(
                        "  [{}/{}] PR #{}: {}{}",
                        idx + 1,
                        all_prs.len(),
                        pr_number,
                        rev.description,
                        state_marker
                    );
                    eprintln!("        {}", pr_url);
                }
            }
        }
    }

    Ok(())
}
