#!/usr/bin/env python3
"""
Almighty Push - Automated jj stack pusher and PR creator for GitHub.
Pushes all changes in current stack above main and creates properly stacked PRs.
"""

from __future__ import annotations

import argparse
import json
import logging
import re
import subprocess
import sys
from dataclasses import dataclass, field
from datetime import datetime
from enum import Enum
from pathlib import Path
from typing import Any, Dict, List, Optional, Set, Tuple

# Configure logging
logging.basicConfig(
    level=logging.INFO,
    format="%(message)s",
    handlers=[logging.StreamHandler()]
)
logger = logging.getLogger(__name__)

# Constants
STATE_FILE = Path(".almighty")
DEFAULT_BASE_BRANCH = "main"
DEFAULT_REMOTE = "origin"
MAX_OPS_TO_CHECK = 50
PUSH_BRANCH_PREFIX = "push-"
CHANGES_BRANCH_PREFIX = "changes/"

# Emoji constants for consistent output
class Emoji:
    ROCKET = "🚀"
    CHECK = "✅"
    CROSS = "❌"
    WARNING = "⚠️"
    INFO = "ℹ️"
    CHART = "📊"
    MEMO = "📝"
    LINK = "🔗"
    SPARKLE = "✨"
    RECYCLE = "🔄"
    TRASH = "🗑️"
    ARROW = "👉"
    SEARCH = "🔍"


class PRState(Enum):
    """Pull request states."""
    OPEN = "open"
    CLOSED = "closed"
    MERGED = "merged"


@dataclass
class Revision:
    """Represents a jj revision with associated PR information."""
    change_id: str
    commit_id: str
    description: str
    branch_name: Optional[str] = None
    pr_url: Optional[str] = None
    full_description: Optional[str] = None
    pr_number: Optional[int] = None

    @property
    def short_change_id(self) -> str:
        """Return abbreviated change ID."""
        return self.change_id[:8]

    @property
    def has_pr(self) -> bool:
        """Check if revision has an associated PR."""
        return self.pr_url is not None

    def extract_pr_number(self) -> Optional[int]:
        """Extract PR number from URL."""
        if not self.pr_url:
            return None
        try:
            return int(self.pr_url.split('/')[-1])
        except (ValueError, IndexError):
            return None


class CommandExecutor:
    """Handles command execution with consistent error handling."""

    @staticmethod
    def run(cmd: List[str], check: bool = True, capture_output: bool = True) -> subprocess.CompletedProcess:
        """Execute a command and return the result.

        Args:
            cmd: Command and arguments to execute
            check: Whether to raise on non-zero exit
            capture_output: Whether to capture stdout/stderr

        Returns:
            CompletedProcess instance with execution results
        """
        try:
            result = subprocess.run(
                cmd,
                capture_output=capture_output,
                text=True,
                check=check
            )
            return result
        except subprocess.CalledProcessError as e:
            logger.error(f"Command failed: {' '.join(cmd)}")
            logger.error(f"Exit code: {e.returncode}")
            if e.stdout:
                logger.error(f"Stdout: {e.stdout}")
            if e.stderr:
                logger.error(f"Stderr: {e.stderr}")
            if check:
                raise
            return e


class JujutsuClient:
    """Handles all Jujutsu (jj) operations."""

    def __init__(self, executor: CommandExecutor):
        self.executor = executor

    def get_bookmarks_on_same_commit(self) -> Dict[str, List[str]]:
        """Get bookmarks that point to the same commit.

        Returns:
            Dict mapping commit_id to list of bookmark names
        """
        cmd = [
            "jj", "log", "-r", "bookmarks()", "--no-graph", "--template",
            r'commit_id.short() ++ " " ++ bookmarks.join(" ") ++ "\n"'
        ]
        result = self.executor.run(cmd, check=False)

        if result.returncode != 0:
            return {}

        commit_to_bookmarks = {}
        for line in result.stdout.strip().split('\n'):
            if not line.strip():
                continue

            parts = line.strip().split(' ', 1)
            if len(parts) != 2:
                continue

            commit_id, bookmarks_str = parts
            bookmarks = self._parse_bookmarks(bookmarks_str)

            if len(bookmarks) > 1:
                commit_to_bookmarks[commit_id] = bookmarks

        return commit_to_bookmarks

    def _parse_bookmarks(self, bookmarks_str: str) -> List[str]:
        """Parse bookmark string into list of relevant bookmarks."""
        bookmarks = []
        for bookmark in bookmarks_str.split():
            if self._is_managed_bookmark(bookmark):
                if bookmark not in bookmarks:
                    bookmarks.append(bookmark)
            elif '@' in bookmark:
                base_name = bookmark.split('@')[0]
                if self._is_managed_bookmark(base_name):
                    remote_bookmark = base_name + '*'
                    if remote_bookmark not in bookmarks:
                        bookmarks.append(remote_bookmark)
        return bookmarks

    @staticmethod
    def _is_managed_bookmark(name: str) -> bool:
        """Check if bookmark is managed by almighty-push."""
        return name.startswith(PUSH_BRANCH_PREFIX) or name.startswith(CHANGES_BRANCH_PREFIX)

    def get_revisions_above_base(self, base_branch: str = DEFAULT_BASE_BRANCH) -> List[Revision]:
        """Get all revisions in the current stack above the base bookmark."""
        cmd = [
            "jj", "log", "-r", f"{base_branch}@{DEFAULT_REMOTE}..@", "--no-graph",
            "--template", r'change_id.short() ++ " " ++ commit_id.short() ++ " " ++ if(empty, "EMPTY", "NOTEMPTY") ++ " " ++ description.first_line() ++ "\n"'
        ]

        result = self.executor.run(cmd)

        if not result.stdout.strip():
            return []

        revisions = []
        skipped_empty = []

        for line in result.stdout.strip().split('\n'):
            if not line.strip():
                continue

            revision = self._parse_revision_line(line)
            if revision:
                if revision.description == "EMPTY":
                    skipped_empty.append(f"{revision.change_id} ({revision.commit_id})")
                    continue
                revisions.append(revision)

        # Reverse to get bottom-up order (oldest first)
        revisions.reverse()

        logger.info(f"{Emoji.CHART} Found {len(revisions)} revision(s) to push")

        if skipped_empty:
            logger.info(f"  (Skipped empty working copy: {skipped_empty[0]})")

        # Validate revisions
        self._validate_revisions(revisions)

        # Fetch full descriptions
        self._fetch_full_descriptions(revisions)

        return revisions

    def _parse_revision_line(self, line: str) -> Optional[Revision]:
        """Parse a single revision line from jj log output."""
        parts = line.split(' ', 3)
        if len(parts) < 3:
            return None

        change_id = parts[0]
        commit_id = parts[1]
        is_empty = parts[2]
        description = parts[3].strip() if len(parts) > 3 else "(no description)"

        if is_empty == "EMPTY":
            # Return a marker revision for empty commits
            return Revision(
                change_id=change_id,
                commit_id=commit_id,
                description="EMPTY"
            )

        return Revision(
            change_id=change_id,
            commit_id=commit_id,
            description=description
        )

    def _validate_revisions(self, revisions: List[Revision]) -> None:
        """Validate that all revisions have descriptions."""
        missing_descriptions = [rev for rev in revisions if rev.description == "(no description)"]
        if missing_descriptions:
            logger.error(f"\n{Emoji.CROSS} Error: The following commits have no description:")
            for rev in missing_descriptions:
                logger.error(f"  • {rev.change_id} ({rev.commit_id})")
            logger.error("\nPlease add descriptions to all commits before pushing.")
            logger.error('Use: jj describe -r <change_id> -m "Your description"')
            sys.exit(1)

    def _fetch_full_descriptions(self, revisions: List[Revision]) -> None:
        """Fetch full multi-line descriptions for all revisions."""
        for rev in revisions:
            cmd = [
                "jj", "log", "-r", rev.change_id, "--no-graph",
                "--template", "description"
            ]
            result = self.executor.run(cmd, check=False)
            if result.returncode == 0 and result.stdout:
                rev.full_description = result.stdout.strip()
            else:
                rev.full_description = rev.description

    def get_local_bookmarks(self) -> Set[str]:
        """Get all local bookmarks from jj."""
        cmd = ["jj", "bookmark", "list", "--template", r'name ++ "\n"']
        result = self.executor.run(cmd, check=False)

        if result.returncode != 0:
            return set()

        bookmarks = set()
        for line in result.stdout.strip().split('\n'):
            line = line.strip()
            if line and self._is_managed_bookmark(line):
                bookmarks.add(line)

        return bookmarks

    def push_revisions(self, revisions: List[Revision]) -> None:
        """Push revisions to remote using jj git push."""
        if not revisions:
            return

        to_create = [rev for rev in revisions if not rev.branch_name]
        to_update = [rev for rev in revisions if rev.branch_name]

        if to_create:
            self._push_new_branches(to_create)

        if to_update:
            self._update_existing_branches(to_update)

    def _push_new_branches(self, revisions: List[Revision]) -> None:
        """Push revisions that don't have branches yet."""
        cmd = ["jj", "git", "push"]
        for rev in revisions:
            cmd.extend(["--change", rev.change_id])

        result = self.executor.run(cmd)
        self._parse_push_output(result, revisions)

    def _update_existing_branches(self, revisions: List[Revision]) -> None:
        """Update existing branches."""
        for rev in revisions:
            cmd = ["jj", "git", "push", "-b", rev.branch_name]
            result = self.executor.run(cmd, check=False)

            if result.returncode != 0:
                # Try with --change as fallback
                cmd = ["jj", "git", "push", "--change", rev.change_id]
                self.executor.run(cmd, check=False)

    def _parse_push_output(self, result: subprocess.CompletedProcess, revisions: List[Revision]) -> None:
        """Parse jj git push output to extract branch names."""
        output = result.stdout + (result.stderr or "")

        patterns = [
            r'(?:Creating branch|Created branch|Branch) (push-\w+|changes/\w+)',
            r'(push-\w+|changes/\w+) for revision',
            r'branch[:\s]+(push-\w+|changes/\w+)',
        ]

        branches_found = []
        for pattern in patterns:
            branches_found.extend(re.findall(pattern, output, re.IGNORECASE))

        for rev in revisions:
            for branch in branches_found:
                if any(rev.change_id[:n] in branch for n in [6, 8, 12]):
                    rev.branch_name = branch
                    logger.info(f"  {Emoji.CHECK} Pushed {rev.change_id} as branch: {branch}")
                    break

            if not rev.branch_name:
                # Assume standard pattern
                rev.branch_name = f"{PUSH_BRANCH_PREFIX}{rev.change_id[:12]}"
                logger.warning(f"  {Emoji.WARNING} Assuming branch name: {rev.branch_name}")

    def get_recently_squashed_commits(self) -> Set[str]:
        """Use jj op log to find commits that were recently squashed or abandoned."""
        cmd = ["jj", "op", "log", "--limit", str(MAX_OPS_TO_CHECK), "--no-graph",
               "--template", r'id.short() ++ " " ++ description ++ "\n"']
        result = self.executor.run(cmd, check=False)

        if result.returncode != 0:
            return set()

        squashed_change_ids = set()

        for line in result.stdout.strip().split('\n'):
            line = line.strip()
            if not line:
                continue

            if 'squash' in line.lower() or 'abandon' in line.lower():
                squashed_change_ids.update(self._extract_change_ids(line))

        return squashed_change_ids

    def _extract_change_ids(self, text: str) -> Set[str]:
        """Extract potential change IDs from text."""
        change_ids = set()
        words = text.split()
        for word in words:
            # Check if word looks like a change ID (8-12 hex chars)
            if 8 <= len(word) <= 12 and all(c in '0123456789abcdefklmnopqrstuvwxyz' for c in word.lower()):
                change_ids.add(word.lower())
        return change_ids


class StateManager:
    """Manages persistent state for almighty-push."""

    def __init__(self, state_file: Path = STATE_FILE):
        self.state_file = state_file

    def load(self) -> Dict[str, Any]:
        """Load state from file."""
        if not self.state_file.exists():
            return {}

        try:
            with open(self.state_file, 'r') as f:
                return json.load(f)
        except (json.JSONDecodeError, IOError):
            return {}

    def save(self, revisions: List[Revision], closed_prs: List[Tuple[int, str]] = None,
             local_bookmarks: Set[str] = None) -> None:
        """Save current state to file."""
        state = self.load()

        state['last_run'] = datetime.now().isoformat()

        # Save PR state
        state['prs'] = {}
        for rev in revisions:
            if rev.pr_url:
                state['prs'][rev.change_id] = {
                    'pr_number': rev.extract_pr_number(),
                    'pr_url': rev.pr_url,
                    'branch_name': rev.branch_name,
                    'commit_id': rev.commit_id,
                    'description': rev.description,
                    'last_seen': datetime.now().isoformat()
                }

        # Track closed PRs
        if closed_prs:
            if 'closed_prs_map' not in state:
                state['closed_prs_map'] = {}
            for pr_num, branch_name in closed_prs:
                state['closed_prs_map'][branch_name] = {
                    'pr_number': pr_num,
                    'closed_at': datetime.now().isoformat(),
                    'reason': 'squashed'
                }

        # Save bookmarks
        if local_bookmarks is not None:
            state['bookmarks'] = list(local_bookmarks)

        try:
            with open(self.state_file, 'w') as f:
                json.dump(state, f, indent=2)
        except IOError as e:
            logger.error(f"  {Emoji.WARNING} Could not save state: {e}")

    def get_disappeared_bookmarks(self, current_bookmarks: Set[str]) -> Set[str]:
        """Get bookmarks that existed in the last run but don't exist now."""
        state = self.load()
        previous_bookmarks = set(state.get('bookmarks', []))

        disappeared = previous_bookmarks - current_bookmarks

        # Filter to managed bookmarks only
        return {
            b for b in disappeared
            if b.startswith(PUSH_BRANCH_PREFIX) or b.startswith(CHANGES_BRANCH_PREFIX)
        }


class GitHubClient:
    """Handles all GitHub operations via gh CLI."""

    def __init__(self, executor: CommandExecutor, state_manager: StateManager):
        self.executor = executor
        self.state_manager = state_manager
        self._repo_info: Optional[Tuple[str, str]] = None

    @property
    def repo_spec(self) -> str:
        """Get repository spec in owner/repo format."""
        owner, repo = self.get_repo_info()
        return f"{owner}/{repo}"

    def get_repo_info(self) -> Tuple[str, str]:
        """Get GitHub repository owner and name from remote."""
        if self._repo_info:
            return self._repo_info

        # Try jj git remote command
        result = self.executor.run(["jj", "git", "remote", "get-url", DEFAULT_REMOTE], check=False)

        if result.returncode != 0:
            # Try listing remotes as fallback
            result = self.executor.run(["jj", "git", "remote", "list"], check=False)
            if result.returncode == 0 and result.stdout:
                for line in result.stdout.strip().split('\n'):
                    if line.startswith(DEFAULT_REMOTE):
                        parts = line.split(None, 1)
                        if len(parts) > 1:
                            url = parts[1]
                            break
                else:
                    raise ValueError(f"Could not find {DEFAULT_REMOTE} remote")
            else:
                raise ValueError("Could not determine GitHub repository from remote")
        else:
            url = result.stdout.strip()

        # Parse GitHub URL
        match = re.search(r'github\.com[:/]([^/]+)/([^/\s]+?)(?:\.git)?$', url)
        if not match:
            raise ValueError(f"Could not parse GitHub repository from URL: {url}")

        self._repo_info = (match.group(1), match.group(2))
        return self._repo_info

    def get_existing_branches(self, verbose: bool = True) -> Dict[str, str]:
        """Get existing branches from GitHub that match our patterns."""
        try:
            owner, repo = self.get_repo_info()
            cmd = ["gh", "api", f"repos/{owner}/{repo}/branches", "--paginate", "-q", ".[].name"]
        except:
            # Fallback to repo view
            cmd = ["gh", "repo", "view", "--json", "defaultBranchRef,refs", "-q", ".refs.nodes[].name"]

        result = self.executor.run(cmd, check=False)

        if result.returncode != 0:
            if verbose:
                logger.warning(f"  {Emoji.WARNING} Could not fetch existing branches from GitHub")
                logger.info("     (This is OK if the repo is private or you're not authenticated)")
            return {}

        branches = {}
        if result.stdout:
            for branch in result.stdout.strip().split('\n'):
                if branch.startswith(PUSH_BRANCH_PREFIX) or branch.startswith(CHANGES_BRANCH_PREFIX):
                    branches[branch] = branch

        return branches

    def reopen_pr_if_needed(self, branch_name: str) -> bool:
        """Check if a PR was previously closed and reopen it if needed."""
        state = self.state_manager.load()
        closed_prs_map = state.get('closed_prs_map', {})

        if branch_name not in closed_prs_map:
            return False

        pr_info = closed_prs_map[branch_name]
        pr_number = pr_info['pr_number']

        # Check PR state
        cmd = ["gh", "pr", "view", str(pr_number), "--repo", self.repo_spec, "--json", "state"]
        result = self.executor.run(cmd, check=False)

        if result.returncode != 0:
            return False

        try:
            pr_data = json.loads(result.stdout)
            if pr_data.get('state') != 'CLOSED':
                return False

            logger.info(f"  {Emoji.RECYCLE} Reopening previously closed PR #{pr_number} for {branch_name}")

            # Reopen PR
            reopen_cmd = ["gh", "pr", "reopen", str(pr_number), "--repo", self.repo_spec]
            result = self.executor.run(reopen_cmd, check=False)

            if result.returncode == 0:
                logger.info(f"    {Emoji.CHECK} Reopened PR #{pr_number}")

                # Add comment
                comment = "This PR was automatically reopened because the commit has been separated back out in the stack."
                comment_cmd = ["gh", "pr", "comment", str(pr_number), "--repo", self.repo_spec, "--body", comment]
                self.executor.run(comment_cmd, check=False)

                # Update state
                del closed_prs_map[branch_name]
                state['closed_prs_map'] = closed_prs_map
                with open(self.state_manager.state_file, 'w') as f:
                    json.dump(state, f, indent=2)

                return True
        except json.JSONDecodeError:
            pass

        return False

    def close_orphaned_prs(
        self,
        current_revisions: List[Revision],
        jj_client: JujutsuClient,
        existing_branches: Optional[Dict[str, str]] = None,
        delete_branches: bool = False
    ) -> List[Tuple[int, str]]:
        """Close PRs whose branches no longer exist in jj (e.g., were squashed)."""
        try:
            _ = self.repo_spec
        except ValueError:
            return []

        if existing_branches is None:
            existing_branches = self.get_existing_branches(verbose=False)

        local_bookmarks = jj_client.get_local_bookmarks()
        disappeared_bookmarks = self.state_manager.get_disappeared_bookmarks(local_bookmarks)
        squashed_commits = jj_client.get_recently_squashed_commits()
        bookmarks_on_same_commit = jj_client.get_bookmarks_on_same_commit()

        state = self.state_manager.load()
        previous_prs = state.get('prs', {})

        # Get all open PRs from GitHub
        cmd = ["gh", "pr", "list", "--repo", self.repo_spec, "--state", "open",
               "--json", "number,headRefName,title", "--limit", "100"]
        result = self.executor.run(cmd, check=False)

        if result.returncode != 0:
            logger.warning(f"  {Emoji.WARNING} Could not fetch open PRs from GitHub")
            return []

        try:
            prs = json.loads(result.stdout)
        except json.JSONDecodeError:
            logger.warning(f"  {Emoji.WARNING} Could not parse PR list from GitHub")
            return []

        # Filter to managed PRs
        managed_prs = [
            pr for pr in prs
            if pr.get('headRefName', '').startswith(PUSH_BRANCH_PREFIX)
            or pr.get('headRefName', '').startswith(CHANGES_BRANCH_PREFIX)
        ]

        active_branches = {rev.branch_name for rev in current_revisions if rev.branch_name}
        active_change_ids = {rev.change_id for rev in current_revisions}

        orphaned_prs = []
        branches_to_delete = []

        # Handle bookmarks squashed into same commit
        squashed_into_same = self._handle_squashed_bookmarks(
            bookmarks_on_same_commit, prs, active_branches, existing_branches,
            orphaned_prs, branches_to_delete
        )

        # Check for other orphaned PRs
        for pr in managed_prs:
            branch_name = pr.get('headRefName', '')

            if branch_name in squashed_into_same:
                continue

            change_id = self._extract_change_id_from_branch(branch_name)

            should_close, close_reason = self._should_close_pr(
                branch_name, change_id, disappeared_bookmarks,
                squashed_commits, previous_prs, active_change_ids,
                local_bookmarks, active_branches
            )

            if should_close:
                orphaned_prs.append((pr, close_reason))
                branches_to_delete.append(branch_name)

        if not orphaned_prs:
            if branches_to_delete:
                logger.info(f"  {Emoji.INFO} No PRs to close, but found {len(branches_to_delete)} orphaned branch(es)")
                for branch in branches_to_delete:
                    logger.info(f"    - {branch}")
                if not delete_branches:
                    logger.info("      (Use --delete-branches to remove these branches from GitHub)")
            else:
                logger.info(f"  {Emoji.CHECK} No orphaned PRs or branches to clean up")
            return []

        logger.info(f"  Found {len(orphaned_prs)} orphaned PR(s) to close:")
        closed_pr_info = self._close_prs(orphaned_prs)

        if branches_to_delete and delete_branches:
            self._delete_remote_branches(branches_to_delete, jj_client)
        elif branches_to_delete:
            logger.info(f"\n  {Emoji.INFO} Not deleting remote branches (use --delete-branches)")
            for branch in branches_to_delete:
                logger.info(f"    Keeping branch: {branch}")

        return closed_pr_info

    def _handle_squashed_bookmarks(
        self, bookmarks_on_same_commit: Dict[str, List[str]],
        prs: List[Dict], active_branches: Set[str],
        existing_branches: Dict[str, str],
        orphaned_prs: List, branches_to_delete: List[str]
    ) -> Set[str]:
        """Handle bookmarks that were squashed into the same commit."""
        squashed_into_same = set()

        for commit_id, bookmarks in bookmarks_on_same_commit.items():
            if len(bookmarks) <= 1:
                continue

            pr_numbers_for_bookmarks = []
            for bookmark in bookmarks:
                clean_bookmark = bookmark.rstrip('*')
                for pr in prs:
                    if pr.get('headRefName') == clean_bookmark:
                        pr_numbers_for_bookmarks.append((pr.get('number'), clean_bookmark, pr))
                        break

            if len(pr_numbers_for_bookmarks) > 1:
                pr_numbers_for_bookmarks.sort(key=lambda x: x[0])

                logger.info(f"  Found {len(bookmarks)} bookmarks on commit {commit_id}")
                logger.info(f"    Keeping PR #{pr_numbers_for_bookmarks[0][0]}, closing duplicates")

                for pr_num, bookmark, pr in pr_numbers_for_bookmarks[1:]:
                    orphaned_prs.append((pr, "squashed into same commit as earlier PR"))
                    branches_to_delete.append(bookmark)
                    squashed_into_same.add(bookmark)

            # Check for orphaned branches without PRs
            for bookmark in bookmarks:
                clean_bookmark = bookmark.rstrip('*')
                if (clean_bookmark in existing_branches and
                    clean_bookmark not in active_branches and
                    clean_bookmark not in squashed_into_same):
                    branches_to_delete.append(clean_bookmark)

        return squashed_into_same

    def _extract_change_id_from_branch(self, branch_name: str) -> Optional[str]:
        """Extract change ID from branch name."""
        if branch_name.startswith(PUSH_BRANCH_PREFIX):
            return branch_name[len(PUSH_BRANCH_PREFIX):]
        elif branch_name.startswith(CHANGES_BRANCH_PREFIX):
            return branch_name[len(CHANGES_BRANCH_PREFIX):]
        return None

    def _should_close_pr(
        self, branch_name: str, change_id: Optional[str],
        disappeared_bookmarks: Set[str], squashed_commits: Set[str],
        previous_prs: Dict, active_change_ids: Set[str],
        local_bookmarks: Set[str], active_branches: Set[str]
    ) -> Tuple[bool, str]:
        """Determine if a PR should be closed and why."""
        if branch_name in disappeared_bookmarks:
            return True, "bookmark was deleted (likely squashed or abandoned)"

        if change_id:
            if change_id in squashed_commits:
                return True, "squashed or abandoned according to operation log"

            if change_id in previous_prs and change_id not in active_change_ids:
                return True, "no longer in the current stack"

            if (branch_name not in local_bookmarks and
                branch_name not in active_branches and
                change_id not in active_change_ids):
                return True, "removed from the stack"

        return False, ""

    def _close_prs(self, orphaned_prs: List[Tuple[Dict, str]]) -> List[Tuple[int, str]]:
        """Close the given PRs with explanatory comments."""
        closed_pr_info = []

        for pr, reason in orphaned_prs:
            pr_number = pr.get('number')
            branch_name = pr.get('headRefName')
            title = pr.get('title', 'Unknown')

            logger.info(f"    Closing PR #{pr_number} ({branch_name}): {title}")
            logger.info(f"      Reason: {reason}")

            # Add comment
            comment = f"This PR was automatically closed because the corresponding commits were {reason}."
            comment_cmd = ["gh", "pr", "comment", str(pr_number), "--repo", self.repo_spec, "--body", comment]
            self.executor.run(comment_cmd, check=False)

            # Close PR
            close_cmd = ["gh", "pr", "close", str(pr_number), "--repo", self.repo_spec]
            result = self.executor.run(close_cmd, check=False)

            if result.returncode == 0:
                logger.info(f"      {Emoji.CHECK} Closed PR #{pr_number}")
                closed_pr_info.append((pr_number, branch_name))
            else:
                logger.warning(f"      {Emoji.WARNING} Failed to close PR #{pr_number}")
                if result.stderr:
                    logger.error(f"         Error: {result.stderr}")

        return closed_pr_info

    def _delete_remote_branches(self, branches: List[str], jj_client: JujutsuClient) -> None:
        """Delete remote branches."""
        logger.info(f"\n  {Emoji.TRASH} Deleting remote branches for closed PRs...")

        for branch in branches:
            cmd = ["jj", "git", "push", "--branch", branch, "--delete"]
            result = self.executor.run(cmd, check=False)

            if result.returncode == 0:
                logger.info(f"    {Emoji.CHECK} Deleted remote branch: {branch}")
            else:
                logger.warning(f"    {Emoji.WARNING} Failed to delete remote branch: {branch}")
                if result.stderr:
                    logger.error(f"       Error: {result.stderr}")

    def create_pull_request(self, revision: Revision, base_branch: str, stack_position: int,
                           all_revisions: List[Revision]) -> bool:
        """Create or update a pull request for a revision."""
        if not revision.branch_name:
            logger.warning(f"  {Emoji.WARNING} Skipping {revision.change_id}: no branch name")
            return False

        # Check if PR already exists
        existing_pr = self._get_existing_pr(revision.branch_name)

        if existing_pr:
            # Update base if needed
            current_base = existing_pr.get('baseRefName')
            if current_base != base_branch:
                self._update_pr_base(revision.branch_name, base_branch)

            revision.pr_url = existing_pr.get('url')
            return True

        # Create new PR
        title = revision.description
        body = self._build_pr_body(revision, stack_position, all_revisions)

        cmd = [
            "gh", "pr", "create",
            "--repo", self.repo_spec,
            "--head", revision.branch_name,
            "--base", base_branch,
            "--title", title,
            "--body", body
        ]

        result = self.executor.run(cmd, check=False)

        if result.returncode == 0:
            pr_url = result.stdout.strip()
            revision.pr_url = pr_url
            logger.info(f"  {Emoji.CHECK} Created PR for {revision.change_id}: {pr_url}")
            return True
        else:
            logger.error(f"  {Emoji.CROSS} Failed to create PR for {revision.change_id}")
            if result.stderr:
                logger.error(f"     Error: {result.stderr}")
            return False

    def _get_existing_pr(self, branch_name: str) -> Optional[Dict]:
        """Check if a PR already exists for the given branch."""
        cmd = ["gh", "pr", "view", branch_name, "--repo", self.repo_spec,
               "--json", "url,baseRefName"]
        result = self.executor.run(cmd, check=False)

        if result.returncode == 0:
            try:
                return json.loads(result.stdout)
            except json.JSONDecodeError:
                pass
        return None

    def _update_pr_base(self, branch_name: str, new_base: str) -> None:
        """Update the base branch of a PR."""
        cmd = ["gh", "pr", "edit", branch_name, "--repo", self.repo_spec,
               "--base", new_base]
        result = self.executor.run(cmd, check=False)

        if result.returncode != 0:
            logger.warning(f"    {Emoji.WARNING} Failed to update PR base to {new_base}")
            if result.stderr:
                logger.error(f"       Error: {result.stderr}")

    def _build_pr_body(self, revision: Revision, position: int, all_revisions: List[Revision]) -> str:
        """Build the PR body with stack information."""
        body = f"**Stack PR #{position + 1}**\n\n"
        body += "Part of stack:\n"

        for i, rev in enumerate(all_revisions):
            prefix = "→ " if i == position else "  "
            body += f"{prefix} {i + 1}. {rev.description}\n"

        body += f"\nChange ID: `{revision.change_id}`\n"
        body += f"Commit ID: `{revision.commit_id}`\n"

        return body

    def update_pr_details(self, revisions: List[Revision]) -> None:
        """Update PR titles and bodies with stack information."""
        if not revisions or not any(rev.pr_url for rev in revisions):
            return

        for i, rev in enumerate(revisions):
            if not rev.pr_url or not rev.branch_name:
                continue

            body = self._build_full_pr_body(rev, i, revisions)
            title = rev.description

            cmd = [
                "gh", "pr", "edit", rev.branch_name,
                "--repo", self.repo_spec,
                "--title", title,
                "--body", body
            ]

            result = self.executor.run(cmd, check=False)

            if result.returncode == 0:
                pr_number = rev.extract_pr_number()
                logger.info(f"  {Emoji.CHECK} Updated PR #{pr_number} for {rev.change_id}")
            else:
                logger.warning(f"  {Emoji.WARNING} Failed to update PR for {rev.change_id}")
                if result.stderr:
                    logger.error(f"     Error: {result.stderr}")

    def _build_full_pr_body(self, revision: Revision, position: int, all_revisions: List[Revision]) -> str:
        """Build complete PR body with stack info and full description."""
        # Stack section
        body = "## Stack\n\n"
        for j, r in enumerate(all_revisions):
            if r.pr_url:
                marker = Emoji.ARROW if j == position else "  "
                pr_number = r.extract_pr_number()
                body += f"{marker} **#{pr_number}**: {r.description}\n"

        # Description section
        if revision.full_description:
            lines = revision.full_description.split('\n')
            if len(lines) > 1:
                additional_lines = '\n'.join(lines[1:]).strip()
                if additional_lines:
                    body += "\n## Description\n\n"
                    body += additional_lines + "\n"

        # Metadata
        body += "\n---\n"
        body += f"Change ID: `{revision.change_id}`\n"
        body += f"Commit ID: `{revision.commit_id}`\n"

        return body


class AlmightyPush:
    """Main orchestrator for almighty-push operations."""

    def __init__(self, executor: CommandExecutor, jj_client: JujutsuClient,
                 github_client: GitHubClient, state_manager: StateManager):
        self.executor = executor
        self.jj = jj_client
        self.github = github_client
        self.state = state_manager

    def push_revisions(self, revisions: List[Revision]) -> Dict[str, str]:
        """Push all revisions to GitHub and return existing branches."""
        if not revisions:
            return {}

        logger.info(f"\n{Emoji.ROCKET} Pushing revisions to GitHub...")

        existing_branches = self.github.get_existing_branches(verbose=False)

        # Categorize revisions
        to_create, to_update = self._categorize_revisions(revisions, existing_branches)

        # Check for PRs to reopen
        self._check_pr_reopening(revisions, existing_branches, to_update)

        # Push branches
        self.jj.push_revisions(to_create + to_update)

        # Print summary
        self._print_push_summary(to_create, to_update)

        return existing_branches

    def _categorize_revisions(self, revisions: List[Revision],
                            existing_branches: Dict[str, str]) -> Tuple[List[Revision], List[Revision]]:
        """Separate revisions into those needing new branches vs updates."""
        to_create = []
        to_update = []

        for rev in revisions:
            branch_found = self._find_existing_branch(rev, existing_branches)

            if branch_found:
                rev.branch_name = branch_found
                to_update.append(rev)
                logger.info(f"  {Emoji.CHECK} Found existing branch for {rev.change_id}: {branch_found}")
            else:
                to_create.append(rev)
                logger.info(f"  • Will create branch for {rev.change_id}")

        return to_create, to_update

    def _find_existing_branch(self, revision: Revision, existing_branches: Dict[str, str]) -> Optional[str]:
        """Find an existing branch for a revision."""
        for branch_name in existing_branches.keys():
            if any(revision.change_id[:n] in branch_name for n in [8, 12]):
                return branch_name
        return None

    def _check_pr_reopening(self, revisions: List[Revision], existing_branches: Dict[str, str],
                          to_update: List[Revision]) -> None:
        """Check if any PRs need to be reopened."""
        try:
            for rev in revisions:
                for branch_name in existing_branches.keys():
                    if any(rev.change_id[:n] in branch_name for n in [8, 12]):
                        if self.github.reopen_pr_if_needed(branch_name):
                            if rev not in to_update:
                                to_update.append(rev)
                        break
        except ValueError:
            pass

    def _print_push_summary(self, to_create: List[Revision], to_update: List[Revision]) -> None:
        """Print summary of push operations."""
        total = len(to_create) + len(to_update)
        if total > 0:
            if to_create and to_update:
                logger.info(f"  {Emoji.CHECK} Created {len(to_create)} branch(es), updated {len(to_update)}")
            elif to_create:
                logger.info(f"  {Emoji.CHECK} Created {len(to_create)} new branch(es)")
            else:
                logger.info(f"  {Emoji.CHECK} Updated {len(to_update)} existing branch(es)")

    def create_pull_requests(self, revisions: List[Revision]) -> None:
        """Create or update GitHub pull requests for all revisions."""
        if not revisions:
            return

        logger.info(f"\n{Emoji.MEMO} Creating/updating pull requests...")

        try:
            logger.info(f"  Repository: {self.github.repo_spec}")
        except ValueError as e:
            logger.warning(f"  {Emoji.WARNING} {e}")
            logger.warning("  Cannot create PRs without repository information")
            return

        # Check for PRs to reopen
        for rev in revisions:
            if rev.branch_name:
                self.github.reopen_pr_if_needed(rev.branch_name)

        # Create/update PRs
        for i, rev in enumerate(revisions):
            base_branch = DEFAULT_BASE_BRANCH if i == 0 else revisions[i-1].branch_name

            if not base_branch:
                logger.warning(f"  {Emoji.WARNING} Cannot create PR for {rev.change_id}: no base branch")
                continue

            self.github.create_pull_request(rev, base_branch, i, revisions)

        # Print summary
        created_count = sum(1 for r in revisions if r.pr_url)
        if created_count > 0:
            logger.info(f"  {Emoji.CHECK} Created/updated {created_count} PR(s)")

    def close_orphaned_prs(self, revisions: List[Revision], existing_branches: Optional[Dict[str, str]],
                          delete_branches: bool) -> List[Tuple[int, str]]:
        """Close PRs for commits that were squashed or removed."""
        return self.github.close_orphaned_prs(
            revisions, self.jj, existing_branches, delete_branches
        )

    def update_pr_details(self, revisions: List[Revision]) -> None:
        """Update PR titles and bodies with stack information."""
        self.github.update_pr_details(revisions)

    def verify_pr_bases(self, revisions: List[Revision]) -> None:
        """Verify that PR base branches are correct."""
        issues = []

        for i, rev in enumerate(revisions):
            if not rev.pr_url or not rev.branch_name:
                continue

            expected_base = DEFAULT_BASE_BRANCH if i == 0 else revisions[i-1].branch_name

            if not expected_base:
                issues.append(f"Cannot verify base for {rev.change_id}: no expected base")
                continue

            existing_pr = self.github._get_existing_pr(rev.branch_name)
            if existing_pr:
                current_base = existing_pr.get('baseRefName')
                if current_base != expected_base:
                    issues.append(f"{rev.change_id} has incorrect base: {current_base} (expected {expected_base})")

        if issues:
            logger.warning(f"\n{Emoji.WARNING} PR stack verification found issues:")
            for issue in issues:
                logger.warning(f"  - {issue}")


def parse_args() -> argparse.Namespace:
    """Parse command-line arguments."""
    parser = argparse.ArgumentParser(
        description="Push jj stack to GitHub and create stacked PRs",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  almighty-push                    # Push stack and create/update PRs
  almighty-push --dry-run          # Show what would be done
  almighty-push --no-pr            # Only push branches
  almighty-push --delete-branches  # Also delete orphaned branches
        """
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Show what would be done without actually doing it"
    )
    parser.add_argument(
        "--no-pr",
        action="store_true",
        help="Only push branches, don't create or update PRs"
    )
    parser.add_argument(
        "--no-close-orphaned",
        action="store_true",
        help="Don't close PRs for squashed or removed commits"
    )
    parser.add_argument(
        "--delete-branches",
        action="store_true",
        help="Delete remote branches when closing orphaned PRs (default: keep branches)"
    )
    parser.add_argument(
        "--verbose", "-v",
        action="store_true",
        help="Enable verbose output"
    )

    return parser.parse_args()


def main() -> int:
    """Main entry point for the CLI."""
    args = parse_args()

    # Set up logging
    if args.verbose:
        logging.getLogger().setLevel(logging.DEBUG)

    logger.info(f"{Emoji.ROCKET} Almighty Push")

    # Initialize components
    executor = CommandExecutor()
    jj_client = JujutsuClient(executor)
    state_manager = StateManager()
    github_client = GitHubClient(executor, state_manager)
    almighty = AlmightyPush(executor, jj_client, github_client, state_manager)

    try:
        # Get revisions in the stack
        revisions = jj_client.get_revisions_above_base()

        if not revisions:
            logger.info(f"{Emoji.SPARKLE} No revisions to push")
            # Still check for orphaned PRs even if no new revisions
            if not args.no_pr and not args.no_close_orphaned:
                closed_prs = almighty.close_orphaned_prs([], None, args.delete_branches)
                # Save state even when no revisions
                local_bookmarks = jj_client.get_local_bookmarks()
                state_manager.save([], closed_prs, local_bookmarks)
            return 0

        if args.dry_run:
            logger.info(f"\n{Emoji.SEARCH} DRY RUN MODE - No actual changes will be made")
            logger.info(f"Would push {len(revisions)} revision(s)")
            return 0

        # Push all revisions
        existing_branches = almighty.push_revisions(revisions)

        if not args.no_pr:
            # Close orphaned PRs first (before creating new ones)
            closed_prs = []
            if not args.no_close_orphaned:
                closed_prs = almighty.close_orphaned_prs(revisions, existing_branches, args.delete_branches)

            # Create PRs
            almighty.create_pull_requests(revisions)

            # Update PR bases to create proper stack
            almighty.verify_pr_bases(revisions)

            # Update PR titles and bodies with stack information
            almighty.update_pr_details(revisions)

            # Save state for next run
            local_bookmarks = jj_client.get_local_bookmarks()
            state_manager.save(revisions, closed_prs, local_bookmarks)

        # Summary
        if any(r.pr_url for r in revisions):
            logger.info(f"\n{Emoji.LINK} Pull Request URLs:")
            for rev in revisions:
                if rev.pr_url:
                    pr_number = rev.extract_pr_number()
                    logger.info(f"  PR #{pr_number}: {rev.description}")
                    logger.info(f"  {rev.pr_url}")

        logger.info(f"\n{Emoji.SPARKLE} Done!")
        return 0

    except KeyboardInterrupt:
        logger.warning(f"\n\n{Emoji.WARNING} Interrupted by user")
        return 1
    except Exception as e:
        logger.error(f"\n{Emoji.CROSS} Error: {e}")
        if args.verbose:
            import traceback
            traceback.print_exc()
        return 1


if __name__ == "__main__":
    sys.exit(main())