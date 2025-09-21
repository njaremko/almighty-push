# Changelog

## [Unreleased] - 2025-09-21

### Added

#### Core Features
- **File-based locking** (`lock.rs`): Prevents concurrent execution with stale lock detection
- **Transaction support** (`transaction.rs`): Atomic operations with rollback capability
- **PR state caching** (`pr_cache.rs`): TTL-based cache to reduce GitHub API calls
- **Enhanced PR descriptions** (`pr_description.rs`): Separate user content from auto-generated stack sections
- **State garbage collection**: Automatic cleanup of stale entries older than 30 days

#### Edge Case Handling
- **Force push detection**: Detects when GitHub branches diverge from local
- **Cyclic dependency detection**: Identifies and breaks circular PR dependencies
- **Split/merge commit detection**: Handles commits that are split or merged
- **Out-of-order PR merging**: Handles PRs merged in different order than stack
- **Partial rebase success**: Continues rebasing non-conflicting commits when conflicts occur
- **Merge commit support**: Warns about multi-parent commits and documents limitations

#### Integrated Edge Case Handling
- Edge case detection is now integrated naturally throughout operations
- Force push detection integrated into `push_revisions()` with inline handling
- Squash detection enhanced in `sync_squash_merged_prs()` with operation log analysis
- Cycle detection integrated into `verify_pr_bases()` with automatic resolution
- Split/merge detection integrated into `get_revisions_above_base()` in jj operations
- Reorder detection integrated into `populate_pr_states()` with automatic PR base updates
- No longer requires a separate edge case detection phase

### Changed

#### Refactored Main Flow (`main.rs`)
- Added 7-step optimized operation order with progress indicators
- Lock acquisition before any operations
- Periodic state garbage collection (weekly)
- Better error handling and recovery
- Transaction-based atomic operations (prepared for future use)
- Edge case handling integrated inline within normal operations

#### Enhanced State Management (`state.rs`)
- Added `garbage_collect()` and `garbage_collect_verbose()` methods
- Compacts merged/closed PR IDs (keeps last 100)
- Removes orphaned bookmarks and duplicate entries
- Improved conflict recovery from corrupted state files

#### Improved JJ Operations (`jj.rs`)
- Added `push_branch_force()` for force pushing
- Added `create_bookmark()` and `rename_bookmark()` methods
- Enhanced merge commit detection and handling
- Better conflict detection with `get_conflicted_files()`

#### GitHub Integration (`github.rs`)
- Added `close_pr()` method with reason
- Made executor field public for pr_cache access
- Enhanced PR body management with stack sections
- Better PR state tracking and caching

#### Almighty Orchestrator (`almighty.rs`)
- Edge case handling integrated into relevant operations
- Force push detection and handling in `push_revisions()`
- Enhanced squash-merge detection in `sync_squash_merged_prs()`
- Cycle detection and resolution in `verify_pr_bases()`
- Improved `rebase_stack_over_merged()` with partial success handling

### Architecture Improvements

1. **Deterministic Reconciliation**: Clear priority rules for resolving conflicts
   - GitHub wins for PR metadata
   - Local jj wins for code changes
   - Merged PRs trigger local rebasing

2. **Operation Ordering**: Prevents merge conflicts through proper sequencing
   - Fetch → Load State → Analyze → Build Plan → Execute → Push → Save
   - Edge cases handled inline during each phase

3. **Robustness**: Multiple layers of error recovery
   - File locking prevents corruption
   - Transaction support enables rollback
   - State validation catches inconsistencies
   - Partial success allows progress despite conflicts

### Dependencies Added
- `hostname = "0.4"` - For lock file hostname tracking

### Technical Debt Addressed
- Added proper error types and handling
- Improved code organization with new modules
- Better separation of concerns
- More testable architecture with dependency injection

## Migration Notes

The new version maintains backward compatibility with existing `.almighty` state files through automatic migration. The lock file (`.almighty.lock`) is automatically managed and cleaned up.

## Known Limitations

- Merge commits (multiple parents) are detected but GitHub PRs only support single base branches
- Transaction support is implemented but not fully utilized (prepared for future enhancements)
- Some edge case handlers are conservative and may require manual intervention
- Edge case detection is integrated throughout operations rather than as a separate phase

## Future Enhancements

The architecture now supports:
- Full transaction rollback on failures
- Parallel PR operations with dependency ordering
- Advanced conflict resolution strategies
- Real-time GitHub webhook integration
- Multi-workspace support