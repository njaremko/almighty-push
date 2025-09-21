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
use constants::DEFAULT_BASE_BRANCH;
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
        almighty.update_pr_details(&revisions)?;

        // Save state for next run
        if !args.dry_run {
            almighty.save_state(&revisions, &closed_prs)?;
        }
    }

    // Summary - show all PRs in the stack
    let open_prs: Vec<&types::Revision> = revisions
        .iter()
        .filter(|r| {
            r.pr_url.is_some()
                && matches!(
                    r.pr_state,
                    Some(types::PrState::Open) | None // None for backward compatibility
                )
        })
        .collect();

    if !open_prs.is_empty() {
        eprintln!("\nPull Request Stack Summary:");
        eprintln!("=============================");
        for (idx, rev) in open_prs.iter().enumerate() {
            if let Some(pr_url) = &rev.pr_url {
                let pr_number = rev
                    .pr_number
                    .unwrap_or_else(|| rev.extract_pr_number().unwrap_or(0));
                eprintln!(
                    "  [{}/{}] PR #{}: {}",
                    idx + 1,
                    open_prs.len(),
                    pr_number,
                    rev.description
                );
                eprintln!("        {}", pr_url);
            }
        }
    }

    Ok(())
}
