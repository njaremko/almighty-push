# Almighty Push - Development Memory

## Project Overview
`almighty-push` is a Rust CLI tool that automates pushing jj (Jujutsu) commit stacks to GitHub as properly stacked pull requests. It manages PR creation, updating, and cleanup while maintaining stack dependencies.

## Key Features Implemented

### 1. Squash-Merge Detection and Sync
- **Problem**: When PRs are squash-merged on GitHub, the local jj state becomes out of sync
- **Solution**: Implemented `sync_squash_merged_prs()` that:
  - Detects closed PRs followed by merged PRs (typical squash-merge pattern)
  - Automatically runs `jj squash -r <source> --into <target>` to match GitHub's state
  - Cleans up bookmarks for squashed commits
  - Handles both single and multiple commit squashes

### 2. Conflict Detection and Handling
- **Problem**: When a PR in the middle of the stack is merged, rebasing commits above it can create conflicts
- **Error**: `Won't push commit b03194ca4270 since it has conflicts`
- **Solution Started**:
  - Added `has_conflicts()` method to check if a commit has conflicts
  - Added `get_conflicted_files()` to list conflicted files
  - Added `rebase_with_conflict_check()` to detect conflicts after rebase
  - Added `fetch_from_remote()` to get latest changes from GitHub
  - Started implementing `handle_merged_prs_in_stack()` but NOT COMPLETED

### 3. GitHub Operations Refactoring
- Refactored `github.rs` for better organization:
  - Added type aliases (`OrphanedPr`, `ClosedPrInfo`)
  - Improved error handling and method organization
  - Better separation of concerns
  - Added context structs to reduce parameter counts

## Current Issues

### 1. Merge Conflicts When PR in Middle of Stack is Merged
**Scenario**:
```
○  commit3 (PR #3)
○  commit2 (PR #2) <- Gets merged on GitHub
○  commit1 (PR #1)
◆  main
```

**Problem**: When PR #2 is merged and we try to rebase commit3, it creates conflicts because:
- GitHub squash-merged commit2 into main
- commit3 was based on the original commit2
- Rebasing commit3 directly onto main creates conflicts

**Attempted Solution**:
- Fetch latest changes from GitHub first
- Detect conflicts during rebase
- Stop and provide user guidance for resolution

**Status**: INCOMPLETE - The `handle_merged_prs_in_stack()` method was started but not added to the codebase

### 2. Jujutsu Conflict Philosophy
- jj allows conflicts to exist in commits locally
- Conflicts can be resolved later (deferred resolution)
- BUT: Cannot push conflicted commits to GitHub
- Need to resolve before pushing

## Code Structure

### Key Modules
- `main.rs`: Entry point, orchestrates workflow
- `almighty.rs`: Core orchestrator
- `jj.rs`: Jujutsu operations
- `github.rs`: GitHub API interactions
- `state.rs`: State persistence
- `types.rs`: Shared data structures
- `command.rs`: Command execution abstraction
- `constants.rs`: Configuration constants

### Important Workflows

1. **Normal Push Flow**:
   - Get revisions from jj
   - Populate PR states from GitHub
   - Sync squash-merged PRs (if any)
   - Handle merged PRs (rebase/skip)
   - Push branches to GitHub
   - Create/update PRs
   - Update PR dependencies

2. **Squash-Merge Sync Flow**:
   - Detect closed PR followed by merged PR
   - Run `jj squash` to combine commits locally
   - Clean up bookmarks
   - Refresh revision list

3. **Conflict Resolution Flow** (INCOMPLETE):
   - Detect merged PRs in stack
   - Fetch latest from GitHub
   - Attempt rebase
   - If conflicts, stop and guide user

## User Instructions

### For Squash-Merged PRs
The tool automatically detects and syncs squash-merged PRs. No action needed.

### For Conflicts After Merge
When a PR in the middle of the stack is merged:
1. The tool will detect conflicts
2. User must manually resolve:
   ```bash
   jj new <conflicted-change-id>
   jj resolve
   jj squash
   almighty-push  # Re-run
   ```

## Testing Scenarios

### 1. Squash-Merge Detection
- Create stack with 2+ commits
- Push all as PRs
- On GitHub: squash-merge higher PR into lower PR
- Run `almighty-push` - should auto-sync

### 2. Middle PR Merge (Conflict Case)
- Create stack with 3+ commits
- Push all as PRs
- On GitHub: merge middle PR to main
- Run `almighty-push` - will likely hit conflicts
- Need to resolve manually

## Known Limitations

1. Cannot automatically resolve conflicts (jj limitation)
2. Manual intervention required for complex merge scenarios
3. Conflict resolution workflow could be smoother

## Future Improvements

1. Better conflict resolution:
   - Try to auto-resolve simple conflicts
   - Better guidance for complex conflicts
   - Integration with jj's merge tools

2. Smarter rebase strategies:
   - Detect which commits will conflict before rebasing
   - Suggest optimal rebase order
   - Handle partial stack merges better

3. Enhanced squash-merge detection:
   - Parse GitHub merge commit messages
   - Handle non-consecutive squash-merges
   - Detect squashes across multiple PRs

## Configuration

No special configuration needed. The tool uses:
- `jj` for version control
- `gh` CLI for GitHub operations
- `.almighty` state file for tracking

## Dependencies
- Rust toolchain
- `jj` (Jujutsu) installed
- `gh` CLI authenticated
- Git repository with GitHub remote

## Important Commands

### Jujutsu Commands Used
- `jj log`: Get revision history
- `jj git push --change`: Push specific commits
- `jj squash -r X --into Y`: Squash commits
- `jj rebase -s X -d Y`: Rebase commits
- `jj resolve`: Resolve conflicts
- `jj git fetch`: Fetch from remote

### GitHub CLI Commands Used
- `gh pr create`: Create PRs
- `gh pr edit`: Update PRs
- `gh pr view`: Check PR status
- `gh pr list`: List PRs
- `gh pr close/reopen`: Manage PR state