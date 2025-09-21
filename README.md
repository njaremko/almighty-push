# almighty-push

Push jj stacks to GitHub as properly stacked pull requests.

## What it does

Takes your local jj commits above `main` and:
1. Pushes each as a GitHub branch (`push-{change-id}`)
2. Creates a PR for each commit
3. Stacks them correctly (each PR's base is the previous PR's branch)
4. Updates existing PRs on subsequent runs
5. Cleans up PRs for squashed/deleted commits

## Installation

```bash
cargo install --path .
```

Requires:
- `jj` (Jujutsu)
- `gh` CLI authenticated with GitHub

## Usage

```bash
# Push stack and create/update PRs
almighty-push

# Preview without making changes
almighty-push --dry-run

# Push branches only, skip PR operations
almighty-push --no-pr

# Also delete remote branches when closing orphaned PRs
almighty-push --delete-branches

# Debug output
almighty-push --verbose
```

## How it works

### Branch naming
Creates branches as `push-{change-id}` where change-id is the first 12 chars of jj's change ID. Reuses existing branches that match your change IDs.

### PR stacking
Each PR's base branch is set to the previous PR's branch in the stack, creating a proper dependency chain. The first PR uses `main` as base.

### State tracking
Stores PR associations in `.almighty` (gitignored). This enables:
- Detecting when commits were squashed/merged
- Closing orphaned PRs automatically
- Reopening previously closed PRs if the commit returns

### Commit requirements
All commits must have descriptions. Empty commits are skipped.

## Example workflow

```bash
# Make changes in jj
jj new main
jj describe -m "Add feature A"
echo "code" > feature_a.py

jj new
jj describe -m "Add feature B"
echo "more code" > feature_b.py

# Push stack to GitHub
almighty-push
# Created PR #1: Add feature A
# Created PR #2: Add feature B (based on #1)

# Make updates
jj edit {change-id-of-feature-a}
echo "fixes" >> feature_a.py

# Push updates (same PRs get updated)
almighty-push
# Updated PR #1: Add feature A
# Updated PR #2: Add feature B
```

## Stack management

The tool maintains your PR stack structure:
- Tracks PR URLs and branch associations
- Updates PR descriptions with stack visualization
- Handles rebases transparently (jj change IDs are stable)
- Cleans up after merged PRs

## Limitations

- Requires all commits to have descriptions
- Only works with GitHub (via `gh` CLI)
- Expects `origin` remote and `main` base branch
- Won't update closed/merged PRs

## Output

- Progress messages → stderr
- PR URLs → stdout (for scripting)
- Warnings/errors → stderr with clear prefixes

## Files

- `.almighty` - State file (PR associations, branch names)
- `CLAUDE.local.md` - Optional project-specific documentation