# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Almighty Push is a Python CLI tool that automates pushing Jujutsu (jj) stacks to GitHub and creating properly stacked pull requests. It bridges the gap between jj's powerful local version control and GitHub's PR workflow.

## Commands

### Running the Tool
```bash
# Main usage - push stack and create/update PRs
./almighty_push.py

# Preview what would be done without executing
./almighty_push.py --dry-run

# Only push branches without creating PRs
./almighty_push.py --no-pr

# Clean up orphaned PRs and delete their branches
./almighty_push.py --delete-branches

# Verbose output for debugging
./almighty_push.py --verbose
```

### Development
```bash
# The script is self-contained with no external dependencies
python3 almighty_push.py

# Check Python syntax
python3 -m py_compile almighty_push.py
```

## Architecture

### Core Components

1. **CommandExecutor**: Central command execution handler with consistent error handling for subprocess calls

2. **JujutsuClient**: Interface to jj commands
   - `get_revisions_above_base()`: Fetches stack of changes above main branch
   - `push_revisions()`: Handles branch creation and pushing to GitHub
   - `get_recently_squashed_commits()`: Detects commits that were squashed/merged

3. **GitHubClient**: GitHub API and gh CLI wrapper
   - Creates and updates pull requests with proper stacking relationships
   - Manages PR state (open/closed/merged)
   - Handles orphaned PR cleanup

4. **StateManager**: Persists state between runs in `.almighty` file
   - Tracks branch names, PR URLs, and closed PRs
   - Enables detection of disappeared branches and squashed commits

5. **AlmightyPush**: Main orchestrator
   - Coordinates the full workflow: fetch revisions → push branches → create/update PRs
   - Handles PR stacking with proper base branch management
   - Manages orphaned PR detection and cleanup

### Workflow

1. **Revision Discovery**: Queries jj for all revisions above main branch
2. **Branch Management**: Creates `push-<change-id>` branches for each revision
3. **PR Creation**: Creates stacked PRs with proper base branches (each PR based on the previous one in the stack)
4. **State Persistence**: Saves PR URLs and branch associations for future runs
5. **Cleanup**: Detects and optionally closes PRs for squashed/removed commits

### Key Design Decisions

- **Branch Naming**: Uses `push-<change-id>` format to avoid conflicts with user branches
- **State File**: `.almighty` JSON file tracks PR associations across runs
- **Stacking**: Each PR's base is the previous PR's branch, creating a proper dependency chain
- **Idempotency**: Safe to run repeatedly; updates existing PRs rather than creating duplicates
- **Jujutsu Integration**: Leverages jj's change IDs for stable references across rebases

## Important Notes

- This repository uses Jujutsu (jj) exclusively - never use git commands directly
- The tool requires `gh` CLI to be installed and authenticated for GitHub operations
- State is stored locally in `.almighty` (gitignored) to track PR associations
- The script is a standalone Python file with no external dependencies beyond standard library

**IMPORTANT DIRECTIVE**: When working in this repository, you MUST use Jujutsu (jj) EXCLUSIVELY for all version control operations. NEVER use git commands directly. This repository uses jj as its primary VCS, and you should embrace jj's idiomatic workflows and philosophy throughout your work.

This section provides comprehensive guidance for using Jujutsu (jj), a Git-compatible version control system that is both simpler and more powerful than Git.

## What is Jujutsu?

Jujutsu (jj) is a powerful version control system designed from the ground up to be easy to use. Key features include:
- **Git-compatible backend**: Works with any Git repository and remote
- **Automatic commits**: Your working copy is always a commit (no staging area)
- **Automatic rebasing**: Changes to a commit automatically rebase all descendants
- **First-class conflicts**: Conflicts can be committed and resolved later
- **Anonymous branches**: No need to name branches unless pushing to remotes
- **Powerful query language**: Revsets allow complex selections of commits
- **Comprehensive undo**: Every operation can be undone
- **Immutable history tracking**: All operations are recorded and can be inspected

## Installation

```bash
# macOS (via Homebrew)
brew install jj

# Linux/Windows (via Cargo)
cargo install jujutsu

# From source
git clone https://github.com/jj-vcs/jj.git
cd jj
cargo install --path cli
```

## Core Concepts

### Working Copy as Commit
Unlike Git, your working directory is always a commit with an ID. Changes are automatically amended into this commit by any jj command. The working copy commit is marked with `@` in logs.

### Changes vs Commits
- **Change**: A logical unit of work with a stable change ID (e.g., `rlvkpnrz`)
- **Commit**: A specific version of a change with a commit hash
- When you modify a change, you get a new commit hash but the same change ID

### No Staging Area
There's no index or staging area. All tracked changes are part of the working copy commit. Use `jj split` to selectively commit changes.

### Automatic Rebasing
When you modify any commit, all descendant commits are automatically rebased on top, maintaining a clean history without manual intervention.

## Essential Commands

### Repository Setup
```bash
# Initialize a new Jujutsu repo
jj init

# Initialize in an existing directory
jj git init

# Clone a Git repository
jj git clone https://github.com/user/repo

# Create a colocated repo (hybrid jj/git)
jj git init --colocate
jj git clone --colocate https://github.com/user/repo

# Import changes from Git repo
jj git import

# Export changes to Git repo
jj git export
```

### Basic Operations
```bash
# Show current status
jj status        # or jj st

# Show commit graph (smartlog)
jj log           # or just jj

# Show current commit details
jj show          # or jj show -r @

# Show changes in working copy
jj diff          # or jj diff -r @

# Show specific revision
jj show CHANGE_ID
```

### Working with Changes
```bash
# Create a new change (commit)
jj new           # New change on top of current
jj new REVISION  # New change on top of REVISION
jj new A B       # New merge commit with parents A and B

# Describe (set message for) current change
jj describe      # Opens editor
jj describe -m "Commit message"

# Edit a specific change
jj edit REVISION

# Move to next/previous change
jj next
jj prev

# Split the current change into multiple
jj split         # Interactive split
jj split -i      # Interactive mode (like git add -p)

# Squash changes together
jj squash        # Squash current into parent
jj squash -r A --into B  # Squash A into B

# Abandon a change (hide it)
jj abandon       # Abandon current change
jj abandon REVISION
```

### File Operations
```bash
# Track new files (automatic for new files)
# Files are automatically tracked when created

# Restore files to state at revision
jj restore       # Restore from parent
jj restore --from REVISION
jj restore FILE --from REVISION

# Show file at specific revision
jj file show FILE -r REVISION

# Untrack files
jj file untrack FILE
```

### History Editing
```bash
# Rebase changes
jj rebase -r REVISION -d DESTINATION  # Rebase single revision
jj rebase -s SOURCE -d DESTINATION    # Rebase source and descendants
jj rebase -b BRANCH -d DESTINATION    # Rebase branch onto destination

# Move changes between commits
jj move --from SOURCE --to DESTINATION

# Insert change into history
jj new A --before B  # Insert new change between A and B

# Duplicate changes (cherry-pick)
jj duplicate REVISION
jj duplicate -r A -d B  # Duplicate A onto B

# Undo/Redo operations
jj undo          # Undo last operation
jj op undo       # Same as above
jj op log        # Show operation history
jj op restore OPERATION_ID  # Restore to specific operation
```

### Bookmarks (Branches)
```bash
# Create bookmark
jj bookmark create my-feature  # At current revision
jj bookmark create my-feature -r REVISION

# List bookmarks
jj bookmark list  # or jj b

# Move bookmark
jj bookmark move my-feature -r REVISION
jj bookmark set my-feature -r REVISION  # Alternative

# Delete bookmark
jj bookmark delete my-feature

# Track remote bookmarks
jj bookmark track main@origin
jj bookmark track my-feature@upstream
```

### Remote Operations
```bash
# Fetch from remote
jj git fetch     # Fetch from all remotes
jj git fetch --remote origin

# Push to remote
jj git push      # Push current bookmark
jj git push --bookmark my-feature
jj git push --change CHANGE_ID  # Creates temporary bookmark

# Push all bookmarks
jj git push --all-bookmarks

# Delete remote bookmark
jj git push --bookmark my-feature --delete
```

### Conflict Resolution
```bash
# List conflicts
jj resolve --list

# Resolve conflicts (opens merge tool)
jj resolve

# Use specific resolution
jj resolve --tool vimdiff
jj restore --from REVISION  # Take version from specific revision
```

## Advanced Features

### Revsets - Powerful Commit Selection

Revsets are expressions for selecting sets of revisions:

```bash
# Basic revsets
jj log -r @           # Current commit
jj log -r @-          # Parent of current
jj log -r @--         # Grandparent
jj log -r root()      # Root commit
jj log -r heads()     # All heads (commits with no children)

# Operators
jj log -r "A & B"     # Intersection
jj log -r "A | B"     # Union
jj log -r "A ~ B"     # Difference (A but not B)

# Ancestors and descendants
jj log -r ::@         # Current and all ancestors
jj log -r @::         # Current and all descendants
jj log -r A::B        # Commits reachable from B but not A's ancestors

# Functions
jj log -r 'author(alice)'           # Commits by alice
jj log -r 'description("fix")'      # Commits with "fix" in message
jj log -r 'empty()'                  # Empty commits
jj log -r 'conflict()'               # Commits with conflicts
jj log -r 'bookmarks()'              # All bookmarked commits
jj log -r 'remote_bookmarks()'       # All remote bookmarks
jj log -r 'mine()'                   # Your commits

# Complex queries
jj log -r 'author(alice) & description("bug")'
jj log -r '::@ & conflict()'        # Conflicts in current history
jj log -r 'ancestors(@, 5)'         # Last 5 ancestors
jj log -r 'present(@-)'              # Parent if it exists
```

### Templates - Customizing Output

Configure how jj displays information:

```toml
# In ~/.config/jj/config.toml
[templates]
# Custom log format
log = """
commit_id.short() ++ " " ++
if(description, description.first_line(), "(no description)") ++
" (" ++ author.email() ++ ")"
"""

[template-aliases]
# Define reusable template functions
'format_timestamp(timestamp)' = 'timestamp.ago()'
```

### Configuration

```bash
# Edit configuration
jj config edit --user    # Edit user config
jj config edit --repo    # Edit repo config

# View configuration
jj config list
jj config get user.email
jj config path --user    # Show config file location
```

Key configuration options in `~/.config/jj/config.toml`:

```toml
[user]
name = "Your Name"
email = "you@example.com"

[ui]
default-command = ["log"]  # Command to run when just typing 'jj'
diff-editor = "vimdiff"
merge-editor = "vimdiff"
editor = "vim"
pager = "less -FRX"

[ui.diff]
tool = ["difft", "--color=always", "$left", "$right"]

[merge-tools.vimdiff]
program = "vim"
merge-args = ["-f", "-d", "$output", "-M", "$left", "$base", "$right"]

# Customize default revisions shown in log
[revsets]
log = "@ | ancestors(@-, 10) | trunk()"

[git]
# Automatically track specified remote bookmarks
auto-track = "glob:main@origin"
# Commits to keep private (not push)
private-commits = "description('WIP')"
```

## Common Workflows

### Feature Development
```bash
# Start new feature from main
jj new main
jj describe -m "Implement feature X"

# Make changes iteratively
# (changes auto-commit to current revision)
vim file.py
jj st  # See changes

# Split into logical commits
jj split -i  # Interactive split

# Continue with next part
jj new
jj describe -m "Add tests for feature X"
vim tests.py

# Review your changes
jj log
jj diff -r @--::@  # See all changes in feature
```

### Stacked Changes (Patch Series)
```bash
# Create a stack of changes
jj new main
jj describe -m "Refactor module A"
# Make changes...

jj new
jj describe -m "Add feature B"
# Make changes...

jj new
jj describe -m "Add tests"
# Make changes...

# Edit middle commit in stack
jj edit @--  # Go to parent's parent
# Make changes - descendants auto-rebase

# Squash fixup into right commit
# Make fixes in working copy, then:
jj squash --into @--  # Squash into grandparent
```

### Cleaning Up History
```bash
# Interactive rebase equivalent
jj rebase -i  # Not available, but alternatives:

# Squash commits
jj squash -r A --into B

# Reorder commits
jj rebase -r C -d A  # Move C after A
jj rebase -s C -d A  # Move C and descendants after A

# Combine multiple commits
jj new A B C  # Create merge of A, B, C
jj describe -m "Combined changes"

# Split a commit
jj edit COMMIT
jj split
```

### Working with Remotes
```bash
# Typical GitHub workflow
jj git clone --colocate https://github.com/user/repo
cd repo

# Create feature
jj new main
jj describe -m "Fix issue #123"
# Make changes...

# Push to GitHub (creates bookmark automatically)
jj git push --change @

# After PR feedback, make fixes
# Changes auto-amend to current commit
vim file.py
jj git push  # Updates the same PR

# Alternative: explicit bookmark
jj bookmark create fix-123
jj git push --bookmark fix-123
```

### Conflict Resolution Workflow
```bash
# Merge with conflicts (doesn't fail!)
jj new main feature -m "Merge feature"
# Conflicts are recorded in the commit

# View conflicts
jj st
jj resolve --list

# Resolve
jj resolve  # Opens merge tool
# Or manually edit and:
jj squash  # Squash resolution into conflict commit

# Continue working (conflicts don't block)
jj new  # Can create new commits on top
```

## Git to Jujutsu Command Reference

| Git Command | Jujutsu Equivalent | Notes |
|------------|-------------------|--------|
| `git init` | `jj init` or `jj git init` | Use `--colocate` for hybrid |
| `git clone URL` | `jj git clone URL` | |
| `git status` | `jj st` | |
| `git log` | `jj log` | Much richer with revsets |
| `git log --graph` | `jj log` | Always shows graph |
| `git show COMMIT` | `jj show -r COMMIT` | |
| `git diff` | `jj diff` | |
| `git diff --staged` | N/A | No staging in jj |
| `git add FILE` | Automatic | Files auto-tracked |
| `git add -p` | `jj split -i` | Split changes interactively |
| `git commit` | `jj new && jj describe` | Or just `jj commit` |
| `git commit -a` | Automatic | Changes auto-commit |
| `git commit --amend` | Automatic or `jj squash` | Working copy auto-amends |
| `git checkout BRANCH` | `jj edit BOOKMARK` | |
| `git checkout -b BRANCH` | `jj new && jj b create NAME` | |
| `git switch BRANCH` | `jj edit BOOKMARK` | |
| `git restore FILE` | `jj restore FILE` | |
| `git reset --hard` | `jj restore` | |
| `git reset --soft` | N/A | Use `jj squash` |
| `git rebase main` | `jj rebase -b @ -d main` | |
| `git rebase -i` | Use `jj squash`, `jj split` | |
| `git cherry-pick` | `jj duplicate -r COMMIT` | |
| `git merge BRANCH` | `jj new CURRENT BRANCH` | |
| `git stash` | `jj new @-` | Just create new commit |
| `git stash pop` | `jj edit STASH && jj abandon @` | |
| `git branch` | `jj bookmark list` | |
| `git branch NAME` | `jj bookmark create NAME` | |
| `git branch -d NAME` | `jj bookmark delete NAME` | |
| `git push` | `jj git push` | |
| `git pull` | `jj git fetch && jj rebase` | |
| `git fetch` | `jj git fetch` | |
| `git remote add` | `jj git remote add` | |
| `git reflog` | `jj op log` | |

## Tips and Best Practices

### For Daily Use
1. **Embrace auto-commit**: Don't fight it - your working copy is always committed
2. **Use change IDs**: Reference commits by change ID for stability across rebases
3. **Think in changes, not commits**: A change can evolve; commits are immutable snapshots
4. **Split liberally**: Use `jj split` to create logical commits from messy work
5. **Squash fixups immediately**: Use `jj squash --into` to put fixes in the right place

### For Git Users
1. **No staging area**: Use `jj split -i` instead of `git add -p`
2. **No detached HEAD**: You're always on a commit that can be evolved
3. **No stash needed**: Just create a new commit with `jj new @-`
4. **Branches are optional**: Use anonymous branches and only name when pushing
5. **Conflicts don't block**: Conflicts are recorded and can be resolved async

### For Collaboration
1. **Colocated repos**: Use `--colocate` for Git compatibility with existing tools
2. **Auto-tracking**: Configure `git.auto-track` for remote bookmarks
3. **Private commits**: Mark WIP with `git.private-commits` revset
4. **Push changes, not bookmarks**: Use `jj git push --change` for PRs

### Advanced Usage
1. **Master revsets**: Learn revset language for powerful selections
2. **Custom templates**: Configure output format for your workflow
3. **Aliases**: Create aliases for common command combinations
4. **Operation log**: Use `jj op log` to understand and undo anything
5. **Workspaces**: Use multiple working copies for parallel work

## Common Pitfalls and Solutions

### "I accidentally edited the wrong commit"
```bash
jj undo  # Undo the operation
# Or just move the changes:
jj squash --from @ --into CORRECT_COMMIT
```

### "I want to see what I just did"
```bash
jj op log  # See operation history
jj diff --from @- --to @  # See changes
```

### "How do I resolve conflicts?"
```bash
jj st  # See conflict status
jj resolve  # Opens merge tool
# Or manually fix and squash
```

### "I need to work on two things at once"
```bash
# Option 1: Use workspaces
jj workspace add ../feature-2

# Option 2: Just switch
jj new main  # Start second feature
# Switch back later:
jj edit FIRST_CHANGE_ID
```

## Learning Resources

- **Official Tutorial**: https://steveklabnik.github.io/jujutsu-tutorial/
- **Official Docs**: https://jj-vcs.github.io/jj/latest/
- **Git Comparison**: https://jj-vcs.github.io/jj/latest/git-comparison/
- **GitHub Workflow**: https://jj-vcs.github.io/jj/latest/github/
- **Community Cheat Sheets**: Search for "jj cheat sheet" for quick references

When helping users with Jujutsu, emphasize:
- The simplicity of always having a commit (no staging complexity)
- The power of automatic rebasing and conflict handling
- The flexibility of revsets for querying history
- The safety of comprehensive undo
- The compatibility with Git infrastructure

## CRITICAL INSTRUCTIONS FOR CLAUDE

### Version Control Usage Rules

1. **EXCLUSIVE USE OF JJ**: You MUST use `jj` commands EXCLUSIVELY. Never fall back to `git` commands, even if you're more familiar with them. This repository is managed with jj, and using git directly can cause confusion and conflicts.

2. **IDIOMATIC JJ WORKFLOWS**: Always use jj idiomatically:
   - Embrace the working-copy-as-commit model - don't try to recreate git's staging area
   - Use `jj new` to start new work, not trying to simulate git branches
   - Use `jj split` when you need selective commits, not trying to stage files
   - Let jj handle rebasing automatically - don't manually manage commit relationships
   - Use revsets for powerful queries instead of simple git log patterns
   - Take advantage of first-class conflict resolution

3. **WHEN COMMITTING CHANGES**:
   - Use `jj describe` to set meaningful commit messages
   - Use `jj new` to create new changes for different logical units of work
   - Use `jj split` to separate unrelated changes
   - Remember that the working copy auto-commits - this is a feature, not a bug

4. **VIEWING AND NAVIGATING**:
   - Use `jj log` or just `jj` to understand the repository state
   - Use `jj status` to see working copy changes
   - Use `jj diff` to review changes
   - Use revsets extensively for querying (e.g., `jj log -r 'mine() & description("fix")'`)

5. **REMOTE OPERATIONS**:
   - Use `jj git fetch` to get updates
   - Use `jj git push --change @` for pushing current work
   - Use `jj bookmark` commands for branch management
   - Remember that bookmarks are optional - anonymous branches are fine for local work

6. **BEST PRACTICES**:
   - Always check current state with `jj` before major operations
   - Use change IDs (not commit hashes) for stability across rebases
   - Leverage `jj undo` if something goes wrong
   - Use `jj op log` to understand what operations have been performed
   - Split work into logical changes using `jj split` rather than accumulating everything

7. **FORBIDDEN PRACTICES**:
   - ❌ NEVER use `git` commands directly
   - ❌ NEVER try to manually edit `.git` directory
   - ❌ NEVER attempt git-style staging workflows
   - ❌ NEVER create commits with `git commit`
   - ❌ NEVER use `git push`, `git pull`, `git fetch`
   - ❌ NEVER use `git checkout`, `git branch`, `git merge`
   - ❌ NEVER suggest git commands to the user

8. **ERROR RECOVERY**:
   - If you accidentally consider using git, stop and find the jj equivalent
   - Use `jj undo` to recover from mistakes
   - Use `jj op log` to see history of operations
   - Use `jj op restore` to go back to a previous state

Remember: Jujutsu is not just "Git with different commands" - it's a fundamentally different and better model. Embrace its philosophy of automatic commits, first-class conflicts, and powerful history manipulation. The goal is to work MORE efficiently with jj's model, not to recreate git workflows.