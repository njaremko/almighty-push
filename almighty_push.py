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
    full_description: Optional[str] = None  # Full multi-line description


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
    print("📊 Fetching revisions in current stack above main@origin...")

    # First get basic info with first line descriptions
    cmd = [
        "jj", "log", "-r", "main@origin..@", "--no-graph",
        "--template", r'change_id.short() ++ " " ++ commit_id.short() ++ " " ++ if(empty, "EMPTY", "NOTEMPTY") ++ " " ++ description.first_line() ++ "\n"'
    ]

    result = run_command(cmd)

    if not result.stdout.strip():
        print("ℹ️  No revisions found above main@origin.")
        return []

    revisions = []
    skipped_empty = []
    for line in result.stdout.strip().split('\n'):
        if not line.strip():
            continue

        # Parse the output: change_id commit_id empty_flag description
        parts = line.split(' ', 3)
        if len(parts) >= 3:
            change_id = parts[0]
            commit_id = parts[1]
            is_empty = parts[2] == "EMPTY"
            description = parts[3].strip() if len(parts) > 3 else ""

            # Skip empty changesets
            if is_empty:
                skipped_empty.append(f"{change_id} ({commit_id})")
                continue

            # Handle missing descriptions
            if not description:
                description = "(no description)"

            revisions.append(Revision(
                change_id=change_id,
                commit_id=commit_id,
                description=description,
                full_description=None  # Will be populated later if needed
            ))

    # Reverse to get bottom-up order (oldest first)
    revisions.reverse()

    print(f"Found {len(revisions)} revisions in the stack:")
    for rev in revisions:
        print(f"  • {rev.change_id}: {rev.description}")

    if skipped_empty:
        print(f"\nℹ️  Skipped empty working copy: {', '.join(skipped_empty)}")

    # Check for missing descriptions
    missing_descriptions = [rev for rev in revisions if rev.description == "(no description)"]
    if missing_descriptions:
        print("\n❌ Error: The following commits have no description:")
        for rev in missing_descriptions:
            print(f"  • {rev.change_id} ({rev.commit_id})")
        print("\nPlease add descriptions to all commits before pushing.")
        print("Use: jj describe -r <change_id> -m \"Your description\"")
        sys.exit(1)

    # Get full descriptions for all revisions
    print("\n📝 Fetching full commit descriptions...")
    for rev in revisions:
        # Get the full description for this revision
        cmd = [
            "jj", "log", "-r", rev.change_id, "--no-graph",
            "--template", "description"
        ]
        result = run_command(cmd, check=False)
        if result.returncode == 0 and result.stdout:
            rev.full_description = result.stdout.strip()
        else:
            # Fall back to first line if we can't get full description
            rev.full_description = rev.description

    return revisions


def get_existing_github_branches() -> Dict[str, str]:
    """Get existing branches from GitHub that match our change IDs."""
    print("\n🔍 Checking for existing branches on GitHub...")

    # Try to get the repo info first
    try:
        owner, repo = get_github_repo_info()
        # Get list of branches from GitHub
        cmd = ["gh", "api", f"repos/{owner}/{repo}/branches", "--paginate", "-q", ".[].name"]
    except:
        # Fallback to using gh repo view
        cmd = ["gh", "repo", "view", "--json", "defaultBranchRef,refs", "-q", ".refs.nodes[].name"]

    result = run_command(cmd, check=False)

    if result.returncode != 0:
        print("  ⚠️  Could not fetch existing branches from GitHub")
        print(f"     (This is OK if the repo is private or you're not authenticated)")
        return {}

    branches = {}
    if result.stdout:
        branch_names = result.stdout.strip().split('\n')
        # Filter for branches that look like push-* or changes/*
        for branch in branch_names:
            if branch.startswith('push-') or branch.startswith('changes/'):
                # Store the full branch name with the change ID part as key
                branches[branch] = branch

        if branches:
            print(f"  Found {len(branches)} existing branches that may match changes")

    return branches


def push_revisions(revisions: List[Revision]) -> List[Revision]:
    """Push all revisions to GitHub using jj git push."""
    if not revisions:
        return []

    print("\n🚀 Pushing revisions to GitHub...")

    # Check for existing branches
    existing_branches = get_existing_github_branches()

    # Separate revisions into new and existing
    to_create = []  # Need to create new branches
    to_update = []  # Need to update existing branches

    for rev in revisions:
        # Check if a branch already exists for this change
        branch_found = None
        for branch_name in existing_branches.keys():
            # Check if branch name contains the change ID
            if rev.change_id[:12] in branch_name or rev.change_id[:8] in branch_name:
                branch_found = branch_name
                break

        if branch_found:
            rev.branch_name = branch_found
            to_update.append(rev)
            print(f"  ✓ Found existing branch for {rev.change_id}: {branch_found}")
        else:
            to_create.append(rev)
            print(f"  • Will create branch for {rev.change_id}")

    # Push new branches using --change
    if to_create:
        # Build the jj git push command with changes that need pushing
        cmd = ["jj", "git", "push"]
        for rev in to_create:
            cmd.extend(["--change", rev.change_id])

        print(f"\n  Creating new branches: {' '.join(cmd)}")
        result = run_command(cmd)

        # Show the output from jj git push
        if result.stdout:
            print("\n📤 Push output:")
            for line in result.stdout.strip().split('\n'):
                print(f"  {line}")
        elif result.stderr:
            print("\n📤 Push output (stderr):")
            for line in result.stderr.strip().split('\n'):
                print(f"  {line}")
        else:
            print("\n📤 Push completed (no output)")

        # Parse the output to get the created branch names
        # jj git push output includes lines like:
        # "Branch changes/xxxxxx to xxxxxxxxxxx"
        # or "Creating branch push-xxxxxx for revision xxxxxx"
        # or "Created branch push-xxxxxx for revision xxxxxx"
        # Check both stdout and stderr

        output_to_parse = result.stdout + (result.stderr if result.stderr else "")

        # Multiple patterns to catch different output formats
        patterns = [
            r'(?:Creating branch|Created branch|Branch) (push-\w+|changes/\w+)',
            r'(push-\w+|changes/\w+) for revision',
            r'branch[:\s]+(push-\w+|changes/\w+)',
        ]

        branches_found = []
        for pattern in patterns:
            branches_found.extend(re.findall(pattern, output_to_parse, re.IGNORECASE))

        # Map newly pushed branches to revisions
        for i, rev in enumerate(to_create):
            # Try to find a branch that contains the change_id
            for branch in branches_found:
                if rev.change_id[:6] in branch or rev.change_id[:8] in branch or rev.change_id[:12] in branch:
                    rev.branch_name = branch
                    print(f"  ✅ Pushed {rev.change_id} as branch: {branch}")
                    break

            # If no specific branch found, jj might have used a default pattern
            if not rev.branch_name and i < len(branches_found):
                rev.branch_name = branches_found[i]
                print(f"  ✅ Pushed {rev.change_id} as branch: {rev.branch_name}")

            # If still no branch name, assume the standard jj pattern
            if not rev.branch_name:
                # jj typically creates branches as push-<change_id>
                assumed_branch = f"push-{rev.change_id[:12]}"
                rev.branch_name = assumed_branch
                print(f"  ⚠️  Assuming branch name: {assumed_branch} (couldn't parse from output)")

    # Update existing branches
    if to_update:
        print("\n  Updating existing branches...")
        for rev in to_update:
            # Use jj git push with bookmark to update the branch
            cmd = ["jj", "git", "push", "-b", rev.branch_name]
            print(f"  Updating {rev.branch_name} for {rev.change_id}...")
            result = run_command(cmd, check=False)

            if result.returncode == 0:
                print(f"  ✅ Updated branch {rev.branch_name}")
            else:
                # Branch might not exist locally, try with --change instead
                cmd = ["jj", "git", "push", "--change", rev.change_id]
                result = run_command(cmd, check=False)
                if result.returncode == 0:
                    print(f"  ✅ Updated {rev.change_id} as branch: {rev.branch_name}")
                else:
                    print(f"  ⚠️  Failed to update branch {rev.branch_name}")
                    if result.stderr:
                        print(f"     Error: {result.stderr}")

    # Combine the results
    return revisions


def get_github_repo_info() -> Tuple[str, str]:
    """Get the GitHub repository owner and name from the remote."""
    # First try jj's git remote command
    result = run_command(["jj", "git", "remote", "get-url", "origin"], check=False)

    if result.returncode != 0:
        # Try listing remotes if get-url doesn't work
        result = run_command(["jj", "git", "remote", "list"], check=False)
        if result.returncode == 0 and result.stdout:
            # Parse output like "origin https://github.com/user/repo.git"
            for line in result.stdout.strip().split('\n'):
                if line.startswith('origin'):
                    parts = line.split(None, 1)
                    if len(parts) > 1:
                        url = parts[1]
                        break
            else:
                raise ValueError("Could not find origin remote")
        else:
            raise ValueError("Could not determine GitHub repository from remote")
    else:
        url = result.stdout.strip()

    # Parse GitHub URL (ssh or https)
    # ssh: git@github.com:owner/repo.git
    # https://github.com/owner/repo.git

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

    print("\n📝 Creating/updating pull requests...")

    repo_spec = None
    try:
        owner, repo = get_github_repo_info()
        repo_spec = f"{owner}/{repo}"
        print(f"  Repository: {repo_spec}")
    except ValueError as e:
        print(f"  ⚠️  {e}")
        print("  Cannot create PRs without repository information")
        return revisions

    for i, rev in enumerate(revisions):
        if not rev.branch_name:
            print(f"  ⚠️  Skipping {rev.change_id}: no branch name")
            continue

        # Determine the expected base branch
        if i == 0:
            # First PR targets main
            expected_base = "main"
        else:
            # Subsequent PRs target the previous PR's branch
            expected_base = revisions[i-1].branch_name
            if not expected_base:
                print(f"  ⚠️  Cannot create PR for {rev.change_id}: previous revision has no branch")
                continue

        # Check if PR already exists and get its current base
        check_cmd = ["gh", "pr", "view", rev.branch_name, "--repo", repo_spec, "--json", "url,baseRefName"]
        check_result = run_command(check_cmd, check=False)

        if check_result.returncode == 0:
            # PR exists
            try:
                pr_data = json.loads(check_result.stdout)
                rev.pr_url = pr_data.get("url")
                current_base = pr_data.get("baseRefName")

                print(f"  ℹ️  PR already exists for {rev.change_id}: {rev.pr_url}")

                # Check if base needs updating
                if current_base != expected_base:
                    print(f"    ⚠️  Base mismatch: current={current_base}, expected={expected_base}")
                    # Update the PR base
                    update_cmd = [
                        "gh", "pr", "edit", rev.branch_name,
                        "--repo", repo_spec,
                        "--base", expected_base
                    ]
                    update_result = run_command(update_cmd, check=False)

                    if update_result.returncode == 0:
                        print(f"    ✅ Updated PR base to {expected_base}")
                    else:
                        print(f"    ❌ Failed to update PR base")
                        if update_result.stderr:
                            print(f"       Error: {update_result.stderr}")
                else:
                    print(f"    ✓ PR base is already correct: {current_base}")

            except json.JSONDecodeError:
                print(f"  ⚠️  Could not parse PR data for {rev.change_id}")
        else:
            # PR doesn't exist, create it
            title = rev.description
            body = f"**Stack PR #{i+1}**\n\n"
            body += f"Part of stack:\n"
            for j, r in enumerate(revisions):
                prefix = "→ " if j == i else "  "
                body += f"{prefix} {j+1}. {r.description}\n"
            body += f"\nChange ID: `{rev.change_id}`\n"
            body += f"Commit ID: `{rev.commit_id}`\n"

            # Create the PR using gh with explicit repo
            cmd = [
                "gh", "pr", "create",
                "--repo", repo_spec,
                "--head", rev.branch_name,
                "--base", expected_base,
                "--title", title,
                "--body", body
            ]

            result = run_command(cmd, check=False)

            if result.returncode == 0:
                pr_url = result.stdout.strip()
                rev.pr_url = pr_url
                print(f"  ✅ Created PR for {rev.change_id}: {pr_url}")
            else:
                print(f"  ❌ Failed to create PR for {rev.change_id}")
                if result.stderr:
                    print(f"     Error: {result.stderr}")

    return revisions


def update_pr_titles_and_bodies(revisions: List[Revision]):
    """Update PR titles and bodies with stack information and full descriptions."""
    if not revisions or not any(rev.pr_url for rev in revisions):
        return

    print("\n✏️ Updating PR titles and bodies with stack information...")

    # Get repo spec for gh commands
    try:
        owner, repo = get_github_repo_info()
        repo_spec = f"{owner}/{repo}"
    except ValueError:
        print("  ⚠️  Cannot update PRs without repository information")
        return

    # Build the stack section that will be in all PR bodies
    stack_section = "## Stack\n\n"
    for i, rev in enumerate(revisions):
        if rev.pr_url:
            # Use emoji to mark current PR in the list
            marker = "👉" if i == 0 else "  "
            pr_number = rev.pr_url.split('/')[-1]  # Extract PR number from URL
            stack_section += f"{marker} **#{pr_number}**: {rev.description}\n"

    # Update each PR
    for i, rev in enumerate(revisions):
        if not rev.pr_url or not rev.branch_name:
            continue

        # Build the PR body
        # Start with stack section, marking the current PR
        body = "## Stack\n\n"
        for j, r in enumerate(revisions):
            if r.pr_url:
                marker = "👉" if j == i else "  "  # Mark current PR with arrow
                pr_number = r.pr_url.split('/')[-1]
                body += f"{marker} **#{pr_number}**: {r.description}\n"

        # Add description section if there are additional lines
        if rev.full_description:
            lines = rev.full_description.split('\n')
            if len(lines) > 1:
                # Skip the first line (it's the title) and add the rest
                additional_lines = '\n'.join(lines[1:]).strip()
                if additional_lines:
                    body += "\n## Description\n\n"
                    body += additional_lines + "\n"

        # Add metadata
        body += "\n---\n"
        body += f"Change ID: `{rev.change_id}`\n"
        body += f"Commit ID: `{rev.commit_id}`\n"

        # Update the PR
        # Note: --title is optional, only update if different from current
        title = rev.description  # First line of description

        cmd = [
            "gh", "pr", "edit", rev.branch_name,
            "--repo", repo_spec,
            "--title", title,
            "--body", body
        ]

        result = run_command(cmd, check=False)

        if result.returncode == 0:
            print(f"  ✅ Updated PR #{rev.pr_url.split('/')[-1]} for {rev.change_id}")
        else:
            print(f"  ⚠️  Failed to update PR for {rev.change_id}")
            if result.stderr:
                print(f"     Error: {result.stderr}")


def update_pr_bases(revisions: List[Revision]):
    """Update the base branches of PRs to create a proper stack.

    Note: This function is now mostly redundant since create_pull_requests
    handles base updates, but we keep it for backwards compatibility and
    as a final verification step.
    """
    print("\n🔗 Verifying PR stack structure...")

    # Get repo spec for gh commands
    try:
        owner, repo = get_github_repo_info()
        repo_spec = f"{owner}/{repo}"
    except ValueError:
        print("  ⚠️  Cannot verify PR bases without repository information")
        return

    all_correct = True
    for i, rev in enumerate(revisions):
        if not rev.pr_url or not rev.branch_name:
            continue

        # Determine expected base
        if i == 0:
            expected_base = "main"
        else:
            prev_rev = revisions[i-1]
            if not prev_rev.branch_name:
                print(f"  ⚠️  Cannot verify base for {rev.change_id}: previous revision has no branch")
                all_correct = False
                continue
            expected_base = prev_rev.branch_name

        # Check current base
        check_cmd = ["gh", "pr", "view", rev.branch_name, "--repo", repo_spec, "--json", "baseRefName"]
        check_result = run_command(check_cmd, check=False)

        if check_result.returncode == 0:
            try:
                pr_data = json.loads(check_result.stdout)
                current_base = pr_data.get("baseRefName")

                if current_base == expected_base:
                    print(f"  ✓ {rev.change_id} correctly based on {expected_base}")
                else:
                    print(f"  ⚠️  {rev.change_id} has incorrect base: {current_base} (expected {expected_base})")
                    all_correct = False
            except json.JSONDecodeError:
                print(f"  ⚠️  Could not verify base for {rev.change_id}")
                all_correct = False
        else:
            print(f"  ⚠️  Could not check PR for {rev.change_id}")
            all_correct = False

    if all_correct:
        print("  ✅ All PRs have correct base branches")


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
            print("\n✨ No revisions to push. You're all caught up with main@origin!")
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

            # Update PR titles and bodies with stack information
            update_pr_titles_and_bodies(revisions)

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