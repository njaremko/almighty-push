# Almighty Push - Complete Implementation Plan

## Executive Summary

This document provides a complete specification for implementing `almighty-push`, a tool that synchronizes jj (Jujutsu) commit stacks with GitHub pull requests. An LLM following this specification should be able to create a robust, feature-complete implementation handling all edge cases.

## Table of Contents

1. [Core Architecture](#core-architecture)
2. [Persistent State Management](#persistent-state-management)
3. [Command Specifications](#command-specifications)
4. [Core Algorithms](#core-algorithms)
5. [Edge Case Handling](#edge-case-handling)
6. [Error Recovery](#error-recovery)
7. [Implementation Details](#implementation-details)

## Core Architecture

### Module Structure

```rust
// main.rs - Entry point and CLI
struct CliArgs {
    dry_run: bool,
    verbose: bool,
    no_pr: bool,
    delete_branches: bool,
}

// almighty.rs - Main orchestrator
struct AlmightyPush {
    jj_client: JujutsuClient,
    github_client: GitHubClient,
    state_manager: StateManager,
    lock_manager: LockManager,
    command_executor: CommandExecutor,
}

// types.rs - Core data structures
struct Revision {
    change_id: String,      // Full jj change ID
    commit_id: String,      // Git commit hash
    description: String,    // First line of commit message
    bookmark: Option<String>,
    author: String,
    parent_change_ids: Vec<String>,
    has_conflicts: bool,
}

struct PrInfo {
    number: u32,
    url: String,
    state: PrState,         // Open, Closed, Merged
    branch: String,
    base_branch: String,
    change_id: String,
    commit_hash: String,
    last_sync: DateTime<Utc>,
}

enum PrState {
    Open,
    Closed,
    Merged,
}
```

### Process Locking

```rust
// lock.rs - File-based locking to prevent concurrent execution
struct LockManager {
    lock_file: PathBuf,  // ".almighty.lock"
    pid: u32,
    acquired_at: Instant,
    timeout: Duration,   // 5 minutes default
}

impl LockManager {
    fn acquire() -> Result<Self> {
        // 1. Check if lock file exists
        // 2. If exists, check if process still running
        // 3. If stale (process dead or timeout), remove
        // 4. Create lock with current PID
        // 5. Use atomic file operations (create with O_EXCL)
    }
    
    fn release(self) {
        // Remove lock file
    }
}
```

## Persistent State Management

### State File Format (.almighty)

```toml
# Version 2 format
version = 2
last_updated = "2024-01-01T00:00:00Z"
last_operation_id = "abc123"  # jj operation ID for recovery

# Active PR tracking
[[prs]]
change_id = "kntqzsqtvkwp"  # Full change ID
pr_number = 123
url = "https://github.com/owner/repo/pull/123"
branch = "push-kntqzsqtvkwp"
base_branch = "push-rlvkpnrzmxpy"  # Or "main" for first
commit_hash = "abc123def456"
state = "open"
last_sync = "2024-01-01T00:00:00Z"
stack_position = 2  # Position in stack
parent_change_id = "rlvkpnrzmxpy"

# Closed/merged PRs (kept for 30 days)
[[closed_prs]]
change_id = "xyzabc123456"
pr_number = 120
url = "https://github.com/owner/repo/pull/120"
closed_at = "2024-01-01T00:00:00Z"
reason = "merged"  # or "closed", "orphaned"

# Stack structure cache
[stack]
ordering = ["rlvkpnrzmxpy", "kntqzsqtvkwp", "mnopqr789012"]
last_rebuild = "2024-01-01T00:00:00Z"

# Operation history for recovery
[[operations]]
id = "op123"
type = "push_stack"
timestamp = "2024-01-01T00:00:00Z"
changes_affected = ["kntqzsqtvkwp"]
success = true
```

### State Migration

```rust
fn migrate_state(state: &mut AlmightyState) -> Result<()> {
    match state.version {
        1 => {
            // Migrate v1 to v2
            // - Add stack_position field
            // - Add parent_change_id field
            // - Convert closed PR format
            state.version = 2;
        }
        2 => {} // Current version
        v => bail!("Unknown state version: {}", v),
    }
    Ok(())
}
```

## Command Specifications

### Jujutsu Commands

#### 1. Get Current Stack

```bash
# Command
jj log --no-pager --color=never -r 'ancestors(@, 20) & descendants(main)' \
  --template 'change_id ++ "\t" ++ commit_id ++ "\t" ++ 
              if(description, description.first_line(), "") ++ "\t" ++ 
              bookmarks.join(",") ++ "\t" ++ 
              parents.map(|p| p.change_id()).join(",") ++ "\t" ++ 
              if(conflict, "true", "false")'

# Output format (TSV)
kntqzsqtvkwp	abc123def456	Add feature A	push-kntqzsqtvkwp	rlvkpnrzmxpy	false
rlvkpnrzmxpy	789ghi012jkl	Fix bug B		xyzabc123456	false

# Parse into
struct Revision {
    change_id: String,         // Column 1
    commit_id: String,         // Column 2
    description: String,       // Column 3
    bookmarks: Vec<String>,    // Column 4, split by comma
    parent_change_ids: Vec<String>, // Column 5, split by comma
    has_conflicts: bool,       // Column 6
}
```

#### 2. Check for Squashed Commits

```bash
# Command
jj op log --no-pager --limit 50 \
  --template 'id ++ "\t" ++ time.start ++ "\t" ++ description'

# Look for "squash" operations
# Parse operation IDs and check affected changes
```

#### 3. Push Branches

```bash
# Push single branch (with retry logic)
jj git push --bookmark "push-${CHANGE_ID}" 2>&1

# Parse output for:
# - "Creating bookmark" - new branch
# - "Move bookmark" - update
# - "force-push" - needs force
# - "Failed" - error handling
```

#### 4. Fetch Updates

```bash
# Fetch from remote
jj git fetch --all-remotes 2>&1

# Parse for:
# - "Fetching" - in progress
# - bookmark updates
# - Error messages
```

#### 5. Rebase Operations

```bash
# Rebase stack onto new base
jj rebase -s "${CHANGE_ID}" -d "${NEW_BASE}" 2>&1

# Parse for:
# - "Rebased N commits"
# - "Conflict" - handle conflicts
# - Error messages
```

### GitHub CLI Commands

#### 1. List PRs

```bash
# Command
gh pr list --json number,url,state,headRefName,baseRefName,title,body,mergeable,merged,mergedAt \
           --limit 1000 --state all

# Output (JSON)
[
  {
    "number": 123,
    "url": "https://github.com/owner/repo/pull/123",
    "state": "OPEN",
    "headRefName": "push-kntqzsqtvkwp",
    "baseRefName": "main",
    "title": "Add feature A",
    "body": "Description\n\n## Stack\n...",
    "mergeable": "MERGEABLE",
    "merged": false,
    "mergedAt": null
  }
]
```

#### 2. Create PR

```bash
# Command with HEREDOC for body
gh pr create \
  --title "${TITLE}" \
  --body "$(cat <<'EOF'
${DESCRIPTION}

## Stack
- [ ] #${PARENT_PR} ⬆️ ${PARENT_TITLE}
- [x] **#${THIS_PR} ⬅️ ${THIS_TITLE}** (this PR)
- [ ] #${CHILD_PR} ⬇️ ${CHILD_TITLE}

Created by [almighty-push](https://github.com/getcord/almighty-push)
EOF
)" \
  --base "${BASE_BRANCH}" \
  --head "${HEAD_BRANCH}" 2>&1

# Parse output for PR number and URL
```

#### 3. Update PR

```bash
# Update PR base branch
gh pr edit ${PR_NUMBER} --base "${NEW_BASE}" 2>&1

# Update PR description
gh pr edit ${PR_NUMBER} --body "${NEW_BODY}" 2>&1
```

#### 4. Close/Reopen PR

```bash
# Close PR
gh pr close ${PR_NUMBER} 2>&1

# Reopen PR
gh pr reopen ${PR_NUMBER} 2>&1
```

#### 5. Check PR Status

```bash
# Get single PR details
gh pr view ${PR_NUMBER} --json state,merged,mergedAt 2>&1
```

## Core Algorithms

### Main Workflow

```rust
fn execute_push_stack() -> Result<()> {
    // Phase 0: Setup
    let lock = LockManager::acquire()?;
    
    // Phase 1: Sync with remote
    jj_fetch_all()?;
    let mut state = StateManager::load()?;
    state.migrate_if_needed()?;
    
    // Phase 2: Detect changes
    let current_stack = get_current_stack()?;
    let github_prs = fetch_all_prs()?;
    let changes = detect_changes(&current_stack, &github_prs, &state)?;
    
    // Phase 3: Handle merged/closed PRs
    for pr in changes.merged_prs {
        handle_merged_pr(&pr, &mut state)?;
    }
    
    for pr in changes.closed_prs {
        handle_closed_pr(&pr, &mut state)?;
    }
    
    // Phase 4: Rebase stack if needed
    if !changes.merged_prs.is_empty() {
        rebase_stack_over_merged(&current_stack, &changes.merged_prs)?;
        // Re-fetch stack after rebase
        current_stack = get_current_stack()?;
    }
    
    // Phase 5: Push branches
    let push_results = push_all_branches(&current_stack)?;
    
    // Phase 6: Create/update PRs
    let pr_results = create_or_update_prs(&current_stack, &mut state)?;
    
    // Phase 7: Update PR descriptions with stack info
    update_pr_stack_descriptions(&pr_results)?;
    
    // Phase 8: Cleanup
    state.garbage_collect()?;
    state.save()?;
    lock.release();
    
    Ok(())
}
```

### Change Detection

```rust
struct DetectedChanges {
    new_commits: Vec<Revision>,        // Need PR creation
    updated_commits: Vec<Revision>,    // Need PR update
    deleted_commits: Vec<String>,      // Need PR closure
    merged_prs: Vec<PrInfo>,          // Merged on GitHub
    closed_prs: Vec<PrInfo>,          // Closed on GitHub
    reordered_stack: bool,            // Stack order changed
    conflicts_detected: Vec<String>,  // Commits with conflicts
}

fn detect_changes(
    current: &[Revision],
    github: &[GithubPr],
    state: &AlmightyState,
) -> DetectedChanges {
    let mut changes = DetectedChanges::default();
    
    // 1. Build lookup maps
    let current_by_id: HashMap<String, &Revision> = 
        current.iter().map(|r| (r.change_id.clone(), r)).collect();
    let github_by_branch: HashMap<String, &GithubPr> = 
        github.iter().map(|p| (p.head_ref.clone(), p)).collect();
    let state_by_id: HashMap<String, &PrInfo> = 
        state.prs.iter().map(|p| (p.change_id.clone(), p)).collect();
    
    // 2. Find new commits (in current, not in state)
    for rev in current {
        if !state_by_id.contains_key(&rev.change_id) {
            changes.new_commits.push(rev.clone());
        }
    }
    
    // 3. Find updated commits (different hash)
    for rev in current {
        if let Some(pr_info) = state_by_id.get(&rev.change_id) {
            if pr_info.commit_hash != rev.commit_id {
                changes.updated_commits.push(rev.clone());
            }
        }
    }
    
    // 4. Find deleted commits (in state, not in current)
    for pr_info in &state.prs {
        if !current_by_id.contains_key(&pr_info.change_id) {
            changes.deleted_commits.push(pr_info.change_id.clone());
        }
    }
    
    // 5. Find merged PRs
    for pr_info in &state.prs {
        if let Some(gh_pr) = github_by_branch.get(&pr_info.branch) {
            if gh_pr.merged {
                changes.merged_prs.push(pr_info.clone());
            } else if gh_pr.state == "CLOSED" {
                changes.closed_prs.push(pr_info.clone());
            }
        }
    }
    
    // 6. Detect reordering
    changes.reordered_stack = detect_stack_reorder(current, &state.stack.ordering);
    
    // 7. Find conflicts
    for rev in current {
        if rev.has_conflicts {
            changes.conflicts_detected.push(rev.change_id.clone());
        }
    }
    
    changes
}
```

### Squash Detection

```rust
fn detect_squashed_commits(
    current: &[Revision],
    state: &AlmightyState,
) -> HashMap<String, Vec<String>> {
    // Map of surviving change_id -> squashed change_ids
    let mut squash_map = HashMap::new();
    
    // 1. Get recent operations
    let ops = jj_get_operations(50)?;
    
    // 2. Find squash operations
    for op in ops {
        if op.description.contains("squash") {
            // Parse which commits were involved
            let (from, into) = parse_squash_operation(&op)?;
            squash_map.entry(into).or_insert(vec![]).push(from);
        }
    }
    
    // 3. Check for multiple bookmarks on same commit (another squash indicator)
    for rev in current {
        if rev.bookmarks.len() > 1 {
            // Multiple bookmarks suggest squashed commits
            for bookmark in &rev.bookmarks {
                if bookmark.starts_with("push-") {
                    let change_id = bookmark.strip_prefix("push-").unwrap();
                    if change_id != &rev.change_id[..12] {
                        // This bookmark belongs to a squashed commit
                        squash_map.entry(rev.change_id.clone())
                            .or_insert(vec![])
                            .push(change_id.to_string());
                    }
                }
            }
        }
    }
    
    squash_map
}
```

### PR Stack Description Management

```rust
struct StackSection {
    entries: Vec<StackEntry>,
}

struct StackEntry {
    pr_number: u32,
    title: String,
    is_current: bool,
    is_merged: bool,
    is_closed: bool,
    position: StackPosition,
}

enum StackPosition {
    Parent,
    Current,
    Child,
}

fn generate_stack_section(
    current_pr: &PrInfo,
    all_prs: &[PrInfo],
) -> String {
    let mut lines = vec!["## Stack".to_string()];
    
    // Find position in stack
    let stack_prs: Vec<_> = all_prs.iter()
        .filter(|p| p.state == PrState::Open)
        .collect();
    
    let current_idx = stack_prs.iter()
        .position(|p| p.number == current_pr.number)
        .unwrap_or(0);
    
    // Show up to 2 parents and 2 children
    let start = current_idx.saturating_sub(2);
    let end = (current_idx + 3).min(stack_prs.len());
    
    for (i, pr) in stack_prs[start..end].iter().enumerate() {
        let actual_idx = start + i;
        let checkbox = if pr.state == PrState::Merged { "[x]" } else { "[ ]" };
        let arrow = if actual_idx < current_idx {
            "⬆️"
        } else if actual_idx > current_idx {
            "⬇️"
        } else {
            "⬅️"
        };
        
        let bold = if actual_idx == current_idx { "**" } else { "" };
        let this_marker = if actual_idx == current_idx { " (this PR)" } else { "" };
        
        lines.push(format!(
            "- {} {bold}#{} {} {}{bold}{}",
            checkbox, pr.number, arrow, pr.title, this_marker
        ));
    }
    
    lines.push("".to_string());
    lines.push("Created by [almighty-push](https://github.com/getcord/almighty-push)".to_string());
    
    lines.join("\n")
}

fn preserve_user_content(old_body: &str, new_stack: &str) -> String {
    // Split at "## Stack" marker
    if let Some(idx) = old_body.find("## Stack") {
        let user_content = &old_body[..idx].trim_end();
        format!("{}\n\n{}", user_content, new_stack)
    } else {
        // No stack section found, append
        format!("{}\n\n{}", old_body, new_stack)
    }
}
```

### Conflict Handling

```rust
fn handle_conflicted_commit(
    rev: &Revision,
    options: &ConflictOptions,
) -> Result<ConflictResolution> {
    if options.block_on_conflict {
        return Err(anyhow!("Commit {} has conflicts", rev.change_id));
    }
    
    if options.skip_conflicted {
        return Ok(ConflictResolution::Skip);
    }
    
    if options.mark_pr_conflicted {
        return Ok(ConflictResolution::PushWithWarning(
            "⚠️ This PR contains unresolved conflicts".to_string()
        ));
    }
    
    Ok(ConflictResolution::Continue)
}

enum ConflictResolution {
    Continue,
    Skip,
    PushWithWarning(String),
    Block,
}
```

## Edge Case Handling

### 1. Race Condition Protection

```rust
fn ensure_single_instance() -> Result<()> {
    let lock = LockManager::acquire_with_timeout(Duration::from_secs(30))?;
    if let Err(e) = lock {
        // Check if other process is actually running
        if let Some(pid) = read_pid_from_lock() {
            if is_process_running(pid) {
                bail!("Another almighty-push instance is running (PID: {})", pid);
            } else {
                // Stale lock, remove and retry
                remove_lock_file()?;
                return ensure_single_instance();
            }
        }
    }
    Ok(())
}
```

### 2. Out-of-Order PR Merging

```rust
fn handle_out_of_order_merge(
    merged_pr: &PrInfo,
    stack: &[Revision],
    state: &mut AlmightyState,
) -> Result<()> {
    // Find children of merged PR
    let children: Vec<_> = state.prs.iter()
        .filter(|p| p.parent_change_id == merged_pr.change_id)
        .cloned()
        .collect();
    
    if children.is_empty() {
        return Ok(());
    }
    
    // Determine new base for children
    let new_base = if merged_pr.base_branch == "main" {
        "main".to_string()
    } else {
        // Find parent of merged PR
        state.prs.iter()
            .find(|p| p.change_id == merged_pr.parent_change_id)
            .map(|p| p.branch.clone())
            .unwrap_or_else(|| "main".to_string())
    };
    
    // Update children bases
    for child in children {
        // Update PR base on GitHub
        gh_update_pr_base(child.number, &new_base)?;
        
        // Update local state
        if let Some(pr) = state.prs.iter_mut().find(|p| p.number == child.number) {
            pr.base_branch = new_base.clone();
            pr.parent_change_id = merged_pr.parent_change_id.clone();
        }
    }
    
    Ok(())
}
```

### 3. Force Push Detection

```rust
fn detect_force_push(
    local_rev: &Revision,
    github_pr: &GithubPr,
) -> bool {
    // Check if GitHub commit is not ancestor of local
    let is_ancestor = jj_is_ancestor(&github_pr.head_sha, &local_rev.commit_id)
        .unwrap_or(false);
    
    !is_ancestor && github_pr.head_sha != local_rev.commit_id
}

fn handle_force_push(
    local_rev: &Revision,
    github_pr: &GithubPr,
    options: &ForceHandling,
) -> Result<()> {
    match options {
        ForceHandling::LocalWins => {
            // Force push local version
            jj_git_push_force(&local_rev.change_id)?;
        }
        ForceHandling::GitHubWins => {
            // Pull GitHub version
            jj_git_fetch_branch(&github_pr.head_ref)?;
            jj_rebase_onto(&local_rev.change_id, &github_pr.head_sha)?;
        }
        ForceHandling::Interactive => {
            // Ask user
            bail!("Force push detected on PR #{}. Manual intervention required.", 
                  github_pr.number);
        }
    }
    Ok(())
}
```

### 4. Split Commit Handling

```rust
fn detect_split_commits(
    current: &[Revision],
    state: &AlmightyState,
) -> Vec<SplitOperation> {
    let mut splits = vec![];
    
    // Look for commits with same description prefix
    for rev in current {
        // Check if description matches pattern like "(1/2) Original message"
        if let Some(captures) = SPLIT_PATTERN.captures(&rev.description) {
            let original_msg = captures.get(2).unwrap().as_str();
            
            // Find original PR with this message
            if let Some(original_pr) = state.prs.iter()
                .find(|p| p.title.contains(original_msg)) {
                
                splits.push(SplitOperation {
                    original_change_id: original_pr.change_id.clone(),
                    new_change_ids: vec![rev.change_id.clone()],
                    original_pr_number: original_pr.number,
                });
            }
        }
    }
    
    splits
}

fn handle_split_commit(
    split: &SplitOperation,
    state: &mut AlmightyState,
) -> Result<()> {
    // Create new PRs for split portions
    for new_change_id in &split.new_change_ids {
        // Create new PR
        let pr_number = create_pr_for_change(new_change_id)?;
        
        // Link to original in description
        let body = format!(
            "Split from #{}.\n\nOriginal change: {}",
            split.original_pr_number,
            split.original_change_id
        );
        gh_update_pr_body(pr_number, &body)?;
    }
    
    Ok(())
}
```

### 5. Merge Commit Support

```rust
fn handle_merge_commit(rev: &Revision) -> Result<BranchStrategy> {
    if rev.parent_change_ids.len() <= 1 {
        return Ok(BranchStrategy::Normal);
    }
    
    // Multiple parents = merge commit
    // Use first parent as primary base
    let primary_parent = &rev.parent_change_ids[0];
    let other_parents = &rev.parent_change_ids[1..];
    
    Ok(BranchStrategy::Merge {
        primary_base: format!("push-{}", &primary_parent[..12]),
        description_note: format!(
            "\n\n**Note**: This is a merge commit with additional parents: {}",
            other_parents.iter()
                .map(|p| format!("push-{}", &p[..12]))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}
```

### 6. Cyclic Dependency Detection

```rust
fn detect_cycles(prs: &[PrInfo]) -> Result<()> {
    let mut visited = HashSet::new();
    let mut recursion_stack = HashSet::new();
    
    fn has_cycle(
        pr: &PrInfo,
        prs_map: &HashMap<String, &PrInfo>,
        visited: &mut HashSet<String>,
        rec_stack: &mut HashSet<String>,
    ) -> bool {
        visited.insert(pr.change_id.clone());
        rec_stack.insert(pr.change_id.clone());
        
        // Check parent
        if !pr.parent_change_id.is_empty() {
            if let Some(parent_pr) = prs_map.get(&pr.parent_change_id) {
                if !visited.contains(&parent_pr.change_id) {
                    if has_cycle(parent_pr, prs_map, visited, rec_stack) {
                        return true;
                    }
                } else if rec_stack.contains(&parent_pr.change_id) {
                    return true; // Cycle detected
                }
            }
        }
        
        rec_stack.remove(&pr.change_id);
        false
    }
    
    let prs_map: HashMap<_, _> = prs.iter()
        .map(|p| (p.change_id.clone(), p))
        .collect();
    
    for pr in prs {
        if !visited.contains(&pr.change_id) {
            if has_cycle(pr, &prs_map, &mut visited, &mut recursion_stack) {
                bail!("Cycle detected in PR dependencies involving PR #{}", pr.number);
            }
        }
    }
    
    Ok(())
}
```

## Error Recovery

### Transaction Management

```rust
struct Transaction {
    operations: Vec<Operation>,
    rollback_operations: Vec<Operation>,
    savepoint: AlmightyState,
}

impl Transaction {
    fn new(state: &AlmightyState) -> Self {
        Self {
            operations: vec![],
            rollback_operations: vec![],
            savepoint: state.clone(),
        }
    }
    
    fn add_operation(&mut self, op: Operation, rollback: Operation) {
        self.operations.push(op);
        self.rollback_operations.push(rollback);
    }
    
    fn commit(self) -> Result<()> {
        for op in self.operations {
            op.execute()?;
        }
        Ok(())
    }
    
    fn rollback(self) -> Result<()> {
        for op in self.rollback_operations.into_iter().rev() {
            if let Err(e) = op.execute() {
                eprintln!("Warning: Rollback operation failed: {}", e);
            }
        }
        Ok(())
    }
}
```

### Partial Success Handling

```rust
struct PushResult {
    succeeded: Vec<String>,
    failed: Vec<(String, Error)>,
    skipped: Vec<String>,
}

fn push_with_partial_success(stack: &[Revision]) -> PushResult {
    let mut result = PushResult::default();
    
    for rev in stack {
        if rev.has_conflicts && SKIP_CONFLICTS {
            result.skipped.push(rev.change_id.clone());
            continue;
        }
        
        match push_single_branch(rev) {
            Ok(_) => result.succeeded.push(rev.change_id.clone()),
            Err(e) => {
                result.failed.push((rev.change_id.clone(), e));
                // Continue with next instead of failing entirely
            }
        }
    }
    
    result
}
```

### Network Retry Logic

```rust
struct RetryConfig {
    max_attempts: u32,
    initial_delay: Duration,
    max_delay: Duration,
    exponential_base: f64,
}

fn retry_with_backoff<T>(
    operation: impl Fn() -> Result<T>,
    config: &RetryConfig,
) -> Result<T> {
    let mut delay = config.initial_delay;
    
    for attempt in 1..=config.max_attempts {
        match operation() {
            Ok(result) => return Ok(result),
            Err(e) if attempt < config.max_attempts => {
                eprintln!("Attempt {} failed: {}. Retrying in {:?}...", 
                         attempt, e, delay);
                thread::sleep(delay);
                
                // Exponential backoff
                delay = Duration::from_secs_f64(
                    (delay.as_secs_f64() * config.exponential_base)
                        .min(config.max_delay.as_secs_f64())
                );
            }
            Err(e) => return Err(e),
        }
    }
    
    unreachable!()
}
```

## Implementation Details

### Garbage Collection

```rust
impl AlmightyState {
    fn garbage_collect(&mut self) -> Result<()> {
        let cutoff = Utc::now() - Duration::days(30);
        
        // Remove old closed PRs
        self.closed_prs.retain(|pr| {
            pr.closed_at > cutoff
        });
        
        // Remove orphaned entries
        let active_change_ids: HashSet<_> = self.prs.iter()
            .map(|p| p.change_id.clone())
            .collect();
        
        // Clean up operations history
        self.operations.retain(|op| {
            op.timestamp > cutoff && 
            op.changes_affected.iter().any(|c| active_change_ids.contains(c))
        });
        
        // Compact if file is too large
        if self.estimate_size() > 1_000_000 { // 1MB
            self.compact()?;
        }
        
        Ok(())
    }
    
    fn compact(&mut self) -> Result<()> {
        // Deduplicate and optimize internal structures
        self.prs.sort_by_key(|p| p.stack_position);
        self.prs.dedup_by_key(|p| p.change_id.clone());
        
        // Rebuild stack ordering
        self.stack.ordering = self.prs.iter()
            .map(|p| p.change_id.clone())
            .collect();
        
        Ok(())
    }
}
```

### Performance Optimizations

```rust
// Parallel branch pushing
fn push_branches_parallel(branches: &[String]) -> Result<Vec<Result<()>>> {
    use rayon::prelude::*;
    
    let results: Vec<_> = branches.par_iter()
        .map(|branch| {
            jj_git_push_branch(branch)
        })
        .collect();
    
    Ok(results)
}

// Batch GitHub API calls
fn batch_create_prs(revisions: &[Revision]) -> Result<Vec<u32>> {
    let mut pr_numbers = vec![];
    
    // Group by base branch to optimize
    let mut by_base: HashMap<String, Vec<&Revision>> = HashMap::new();
    for (i, rev) in revisions.iter().enumerate() {
        let base = if i == 0 {
            "main".to_string()
        } else {
            format!("push-{}", &revisions[i-1].change_id[..12])
        };
        by_base.entry(base).or_default().push(rev);
    }
    
    // Create PRs in batches
    for (base, revs) in by_base {
        for rev in revs {
            let pr_num = gh_create_pr(rev, &base)?;
            pr_numbers.push(pr_num);
        }
    }
    
    Ok(pr_numbers)
}

// Cache GitHub API responses
struct ApiCache {
    entries: HashMap<String, (Instant, serde_json::Value)>,
    ttl: Duration,
}

impl ApiCache {
    fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.entries.get(key)
            .filter(|(time, _)| time.elapsed() < self.ttl)
            .map(|(_, value)| value)
    }
    
    fn set(&mut self, key: String, value: serde_json::Value) {
        self.entries.insert(key, (Instant::now(), value));
    }
}
```

### CLI Output Management

```rust
struct OutputManager {
    verbose: bool,
    quiet: bool,
    use_color: bool,
}

impl OutputManager {
    fn progress(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{}→{} {}", 
                if self.use_color { "\x1b[34m" } else { "" },
                if self.use_color { "\x1b[0m" } else { "" },
                msg);
        }
    }
    
    fn success(&self, msg: &str) {
        if !self.quiet {
            eprintln!("{}✓{} {}",
                if self.use_color { "\x1b[32m" } else { "" },
                if self.use_color { "\x1b[0m" } else { "" },
                msg);
        }
    }
    
    fn warning(&self, msg: &str) {
        eprintln!("{}⚠{} {}",
            if self.use_color { "\x1b[33m" } else { "" },
            if self.use_color { "\x1b[0m" } else { "" },
            msg);
    }
    
    fn error(&self, msg: &str) {
        eprintln!("{}✗{} {}",
            if self.use_color { "\x1b[31m" } else { "" },
            if self.use_color { "\x1b[0m" } else { "" },
            msg);
    }
    
    fn pr_url(&self, url: &str) {
        // URLs go to stdout for scripting
        println!("{}", url);
    }
}
```

## Testing Strategy

### Unit Test Structure

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_detect_squashed_commits() {
        let current = vec![
            Revision {
                change_id: "abc123".to_string(),
                bookmarks: vec!["push-abc123".to_string(), "push-def456".to_string()],
                ..Default::default()
            },
        ];
        
        let squashed = detect_squashed_commits(&current, &Default::default());
        assert!(squashed.contains_key("abc123"));
        assert!(squashed["abc123"].contains(&"def456".to_string()));
    }
    
    #[test]
    fn test_cycle_detection() {
        let prs = vec![
            PrInfo {
                change_id: "a".to_string(),
                parent_change_id: "b".to_string(),
                ..Default::default()
            },
            PrInfo {
                change_id: "b".to_string(),
                parent_change_id: "a".to_string(),
                ..Default::default()
            },
        ];
        
        assert!(detect_cycles(&prs).is_err());
    }
}
```

### Integration Test Scenarios

```rust
// tests/integration.rs
#[test]
fn test_full_stack_push() {
    let temp_dir = TempDir::new().unwrap();
    
    // Setup test repository
    setup_test_repo(&temp_dir);
    
    // Create test stack
    create_test_commits(&[
        ("First commit", "file1.txt"),
        ("Second commit", "file2.txt"),
        ("Third commit", "file3.txt"),
    ]);
    
    // Run almighty-push
    let result = AlmightyPush::new()
        .with_dry_run(false)
        .execute();
    
    assert!(result.is_ok());
    
    // Verify PRs created
    let state = StateManager::load().unwrap();
    assert_eq!(state.prs.len(), 3);
    
    // Verify stack structure
    assert_eq!(state.prs[1].base_branch, format!("push-{}", &state.prs[0].change_id[..12]));
}
```

## Summary

This implementation plan provides:

1. **Robust concurrency control** through file-based locking
2. **Comprehensive state management** with migration support
3. **Detailed command specifications** for jj and gh with exact parsing rules
4. **Complete algorithms** for all core operations
5. **Edge case handling** for 15+ identified scenarios
6. **Error recovery** with transactions and partial success
7. **Performance optimizations** including parallel operations and caching
8. **Clear testing strategy** for validation

An LLM following this specification should be able to implement a production-ready version of `almighty-push` that correctly handles jj's unique model while providing reliable GitHub PR management.