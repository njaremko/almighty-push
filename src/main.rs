use almighty_push::app::{self, RequestedMode, RunOptions};
use almighty_push::domain::{HeadRef, Limits, RemoteName, RepositoryId, Scope};
use clap::Parser;
use std::process::ExitCode;

/// Reconcile one exact jj change stack with owned GitHub pull requests.
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Exact jj Git remote. Required when the workspace has multiple remotes.
    #[arg(long, value_parser = parse_remote)]
    remote: Option<RemoteName>,

    /// Exact target repository as HOST/OWNER/NAME. Defaults to the source remote.
    #[arg(long = "repo", value_parser = parse_repository)]
    repository: Option<RepositoryId>,

    /// Exact target base branch. Required with --no-pr.
    #[arg(long, value_parser = parse_base)]
    base: Option<HeadRef>,

    /// Bounded jj revision at the tip of the selected stack.
    #[arg(long, default_value = "@", value_parser = parse_tip)]
    tip: String,

    /// Render canonical actions without locking, fetching, or mutating anything.
    #[arg(long)]
    dry_run: bool,

    /// Push exact owned heads without executing or observing GitHub.
    #[arg(long)]
    no_pr: bool,

    /// Delete exact owned heads after their pull requests become historical.
    #[arg(long, conflicts_with = "no_pr")]
    delete_branches: bool,

    /// Emit one JSON report on stdout.
    #[arg(long)]
    json: bool,

    /// Emit bounded progress information on stderr.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let mode = match (args.dry_run, args.no_pr) {
        (false, false) => RequestedMode::Full {
            delete_closed_heads: args.delete_branches,
        },
        (false, true) => RequestedMode::NoPr,
        (true, false) => RequestedMode::DryRunFull {
            delete_closed_heads: args.delete_branches,
        },
        (true, true) => RequestedMode::DryRunNoPr,
    };
    let invocation_cwd = match std::env::current_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("almighty-push: current directory is unavailable: {error}");
            return ExitCode::FAILURE;
        }
    };
    if args.verbose {
        eprintln!("almighty-push: resolving exact repository scope");
    }
    match app::run(
        RunOptions {
            remote: args.remote,
            repository: args.repository,
            base: args.base,
            tip_revset: args.tip,
            mode,
            limits: Limits::default(),
        },
        &invocation_cwd,
    ) {
        Ok(report) => {
            if args.json {
                match serde_json::to_string(&report) {
                    Ok(json) => println!("{json}"),
                    Err(error) => {
                        eprintln!("almighty-push: report serialization failed: {error}");
                        return ExitCode::FAILURE;
                    }
                }
            } else if mode.is_dry_run() {
                for stage in report.stages() {
                    for action in stage.actions() {
                        println!("{action}");
                    }
                }
            } else {
                for url in report.pr_urls() {
                    println!("{url}");
                }
            }
            if args.verbose {
                eprintln!(
                    "almighty-push: {} completed in {} stage(s)",
                    report.mode(),
                    report.stages().len()
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("almighty-push: {error}");
            ExitCode::FAILURE
        }
    }
}

fn parse_remote(value: &str) -> Result<RemoteName, String> {
    RemoteName::parse(value).map_err(|error| error.to_string())
}

fn parse_repository(value: &str) -> Result<RepositoryId, String> {
    RepositoryId::parse(value).map_err(|error| error.to_string())
}

fn parse_base(value: &str) -> Result<HeadRef, String> {
    HeadRef::parse(value).map_err(|error| error.to_string())
}

fn parse_tip(value: &str) -> Result<String, String> {
    Scope::validate_tip_revset(value).map_err(|error| error.to_string())?;
    Ok(value.to_owned())
}
