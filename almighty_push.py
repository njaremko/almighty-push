#!/usr/bin/env python3
"""
Almighty Push - Automated jj stack pusher and PR creator for GitHub.
Pushes all changes in current stack above main and creates properly stacked PRs.
"""

import subprocess
import json
import re
import sys
from typing import List, Dict, Optional, Tuple
from dataclasses import dataclass
import argparse


@dataclass
class Revision:
    """Represents a jj revision."""
    change_id: str
    commit_id: str
    description: str
    branch_name: Optional[str] = None
    pr_url: Optional[str] = None


def run_command(cmd: List[str], check: bool = True, capture_output: bool = True) -> subprocess.CompletedProcess:
    """Run a shell command and return the result."""
    try:
        result = subprocess.run(
            cmd,
            capture_output=capture_output,
            text=True,
            check=check
        )
        return result
    except subprocess.CalledProcessError as e:
        print(f"Error running command: {' '.join(cmd)}")
        print(f"Exit code: {e.returncode}")
        if e.stdout:
            print(f"Stdout: {e.stdout}")
        if e.stderr:
            print(f"Stderr: {e.stderr}")
        if check:
            raise
        return e


def get_revisions_above_main() -> List[Revision]:
    """Get all revisions in the current stack above the main bookmark."""
    print("📊 Fetching revisions in current stack above main...")

    # Use jj log with a revset to get commits between main and current
    # Format: change_id commit_id description
    cmd = [
        "jj", "log", "-r", "main..@", "--no-graph",
        "--template", r'change_id.short() ++ " " ++ commit_id.short() ++ " " ++ description.first_line()'
    ]

    result = run_command(cmd)

    if not result.stdout.strip():
        print("ℹ️  No revisions found above main.")
        return []

    revisions = []
    for line in result.stdout.strip().split('\n'):
        if not line.strip():
            continue

        # Parse the output: change_id commit_id description
        parts = line.split(' ', 2)
        if len(parts) >= 2:
            change_id = parts[0]
            commit_id = parts[1]
            description = parts[2] if len(parts) > 2 else "No description"

            revisions.append(Revision(
                change_id=change_id,
                commit_id=commit_id,
                description=description
            ))

    # Reverse to get bottom-up order (oldest first)
    revisions.reverse()

    print(f"Found {len(revisions)} revisions in the stack:")
    for rev in revisions:
        print(f"  • {rev.change_id}: {rev.description}")

    return revisions


def push_revisions(revisions: List[Revision]) -> List[Revision]:
    """Push all revisions to GitHub using jj git push."""
    if not revisions:
        return []

    print("\n🚀 Pushing revisions to GitHub...")

    # Build the jj git push command with all changes
    cmd = ["jj", "git", "push"]
    for rev in revisions:
        cmd.extend(["--change", rev.change_id])

    result = run_command(cmd)

    # Parse the output to get the created branch names
    # jj git push output includes lines like:
    # "Branch changes/xxxxxx to xxxxxxxxxxx"
    # or "Creating branch push-xxxxxx for revision xxxxxx"

    branch_pattern = re.compile(r'(?:Creating branch|Branch) (push-\w+|changes/\w+)')

    branches_found = branch_pattern.findall(result.stdout)

    # Map branches to revisions
    for i, rev in enumerate(revisions):
        # Try to find a branch that contains the change_id
        for branch in branches_found:
            if rev.change_id[:6] in branch or rev.change_id[:8] in branch:
                rev.branch_name = branch
                print(f"  ✅ Pushed {rev.change_id} as branch: {branch}")
                break

        # If no specific branch found, jj might have used a default pattern
        if not rev.branch_name and i < len(branches_found):
            rev.branch_name = branches_found[i]
            print(f"  ✅ Pushed {rev.change_id} as branch: {rev.branch_name}")

    return revisions


def get_github_repo_info() -> Tuple[str, str]:
    """Get the GitHub repository owner and name from the remote."""
    # Get the remote URL
    result = run_command(["jj", "git", "remote", "get-url", "origin"], check=False)

    if result.returncode != 0:
        # Try with git command as fallback
        result = run_command(["git", "remote", "get-url", "origin"], check=False)

    if result.returncode != 0:
        raise ValueError("Could not determine GitHub repository from remote")

    url = result.stdout.strip()

    # Parse GitHub URL (ssh or https)
    # ssh: git@github.com:owner/repo.git
    # https: https://github.com/owner/repo.git

    match = re.search(r'github\.com[:/]([^/]+)/([^/\s]+?)(?:\.git)?$', url)
    if not match:
        raise ValueError(f"Could not parse GitHub repository from URL: {url}")

    owner = match.group(1)
    repo = match.group(2)

    return owner, repo


def create_pull_requests(revisions: List[Revision]) -> List[Revision]:
    """Create GitHub pull requests for each pushed branch."""
    if not revisions:
        return []

    print("\n📝 Creating pull requests...")

    try:
        owner, repo = get_github_repo_info()
        print(f"  Repository: {owner}/{repo}")
    except ValueError as e:
        print(f"  ⚠️  {e}")
        print("  Attempting to create PRs anyway...")

    for i, rev in enumerate(revisions):
        if not rev.branch_name:
            print(f"  ⚠️  Skipping {rev.change_id}: no branch name")
            continue

        # Determine the base branch
        if i == 0:
            # First PR targets main
            base = "main"
        else:
            # Subsequent PRs target the previous PR's branch
            base = revisions[i-1].branch_name
            if not base:
                print(f"  ⚠️  Cannot create PR for {rev.change_id}: previous revision has no branch")
                continue

        # Create the PR title and body
        title = rev.description
        body = f"**Stack PR #{i+1}**\n\n"
        body += f"Part of stack:\n"
        for j, r in enumerate(revisions):
            prefix = "→ " if j == i else "  "
            body += f"{prefix} {j+1}. {r.description}\n"
        body += f"\nChange ID: `{rev.change_id}`\n"
        body += f"Commit ID: `{rev.commit_id}`\n"

        # Create the PR using gh
        cmd = [
            "gh", "pr", "create",
            "--head", rev.branch_name,
            "--base", base,
            "--title", title,
            "--body", body
        ]

        result = run_command(cmd, check=False)

        if result.returncode == 0:
            pr_url = result.stdout.strip()
            rev.pr_url = pr_url
            print(f"  ✅ Created PR for {rev.change_id}: {pr_url}")
        else:
            # Check if PR already exists
            check_cmd = ["gh", "pr", "view", rev.branch_name, "--json", "url"]
            check_result = run_command(check_cmd, check=False)

            if check_result.returncode == 0:
                try:
                    pr_data = json.loads(check_result.stdout)
                    rev.pr_url = pr_data.get("url")
                    print(f"  ℹ️  PR already exists for {rev.change_id}: {rev.pr_url}")
                except json.JSONDecodeError:
                    print(f"  ⚠️  Could not create or find PR for {rev.change_id}")
            else:
                print(f"  ❌ Failed to create PR for {rev.change_id}")
                if result.stderr:
                    print(f"     Error: {result.stderr}")

    return revisions


def update_pr_bases(revisions: List[Revision]):
    """Update the base branches of PRs to create a proper stack."""
    print("\n🔗 Setting up PR stack bases...")

    for i, rev in enumerate(revisions):
        if not rev.pr_url or not rev.branch_name:
            continue

        # Skip the first PR as it should already target main
        if i == 0:
            print(f"  ✓ First PR targets main: {rev.change_id}")
            continue

        # Get the base branch from the previous revision
        prev_rev = revisions[i-1]
        if not prev_rev.branch_name:
            print(f"  ⚠️  Cannot update base for {rev.change_id}: previous revision has no branch")
            continue

        # Update the PR base using gh
        cmd = [
            "gh", "pr", "edit", rev.branch_name,
            "--base", prev_rev.branch_name
        ]

        result = run_command(cmd, check=False)

        if result.returncode == 0:
            print(f"  ✅ Updated {rev.change_id} to base on {prev_rev.branch_name}")
        else:
            print(f"  ⚠️  Failed to update base for {rev.change_id}")
            if result.stderr and "already has" not in result.stderr:
                print(f"     Error: {result.stderr}")


def main():
    """Main entry point for the CLI."""
    parser = argparse.ArgumentParser(
        description="Push jj stack to GitHub and create stacked PRs"
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Show what would be done without actually doing it"
    )
    parser.add_argument(
        "--no-pr",
        action="store_true",
        help="Only push branches, don't create PRs"
    )

    args = parser.parse_args()

    print("🚀 Almighty Push - Automated jj stack pusher")
    print("=" * 50)

    try:
        # Get revisions in the stack
        revisions = get_revisions_above_main()

        if not revisions:
            print("\n✨ No revisions to push. You're all caught up with main!")
            return 0

        if args.dry_run:
            print("\n🔍 DRY RUN MODE - No actual changes will be made")
            print("\nWould push the following revisions:")
            for rev in revisions:
                print(f"  • {rev.change_id}: {rev.description}")
            return 0

        # Push all revisions
        revisions = push_revisions(revisions)

        if not args.no_pr:
            # Create PRs
            revisions = create_pull_requests(revisions)

            # Update PR bases to create proper stack
            update_pr_bases(revisions)

        # Summary
        print("\n" + "=" * 50)
        print("📊 Summary:")
        print(f"  • Pushed {len(revisions)} revision(s)")

        if not args.no_pr:
            pr_count = sum(1 for r in revisions if r.pr_url)
            print(f"  • Created/found {pr_count} PR(s)")

            if any(r.pr_url for r in revisions):
                print("\n🔗 Pull Request URLs:")
                for i, rev in enumerate(revisions):
                    if rev.pr_url:
                        print(f"  {i+1}. {rev.description}")
                        print(f"     {rev.pr_url}")

        print("\n✨ Done! Your stack has been pushed to GitHub.")
        return 0

    except KeyboardInterrupt:
        print("\n\n⚠️  Interrupted by user")
        return 1
    except Exception as e:
        print(f"\n❌ Error: {e}")
        return 1


if __name__ == "__main__":
    sys.exit(main())