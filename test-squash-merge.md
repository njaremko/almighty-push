# Testing Squash-Merge Detection and Sync

## Scenario

You have a stack of commits in jj:
```
@  syyvlzov nathan@nobie.com 2025-09-21 17:01:00 07aed225
│  (empty) (no description set)
○  llztukst nathan@nobie.com 2025-09-21 17:01:00 push-llztukststnw 6aa8260b
│  More cleanup
○  uvmsxuuz nathan@nobie.com 2025-09-21 16:50:45 push-uvmsxuuzsymm a2bb8665
│  Sanding the edges
◆  kswvmkrs nathan@nobie.com 2025-09-21 16:26:19 main@origin 600ab59b
│  More fixes (#15)
```

## Steps to Test

1. Run `almighty-push` to create PRs for both commits
2. On GitHub, merge the "More cleanup" PR into "Sanding the edges" using squash-merge
3. Run `almighty-push` again

## Expected Behavior

When you run `almighty-push` after the squash-merge on GitHub:

1. The tool detects that:
   - "More cleanup" PR is closed
   - "Sanding the edges" PR is merged
   - These are consecutive commits in your stack

2. It automatically syncs your local jj state:
   ```
   Detected squash-merged PRs on GitHub. Syncing local jj state...
     Squashing 1 commit into 'Sanding the edges'...
       - Squashing 'More cleanup' (llztukst)
       Running: jj squash -r llztukst --into uvmsxuuz
       Cleaning up bookmarks for squashed commits...
   ```

3. Your local jj state now matches GitHub:
   ```
   @  syyvlzov nathan@nobie.com 2025-09-21 17:01:00 07aed225
   │  (empty) (no description set)
   ○  uvmsxuuz nathan@nobie.com 2025-09-21 16:50:45 push-uvmsxuuzsymm a2bb8665
   │  Sanding the edges (now contains "More cleanup" changes)
   ◆  kswvmkrs nathan@nobie.com 2025-09-21 16:26:19 main@origin 600ab59b
   │  More fixes (#15)
   ```

## Implementation Details

The feature works by:

1. **Detection Phase**:
   - Fetches PR states from GitHub
   - Identifies patterns of closed PRs followed by merged PRs
   - Verifies they are consecutive commits in the stack

2. **Sync Phase**:
   - Uses `jj squash -r <source> --into <target>` to combine commits
   - Cleans up bookmarks for squashed commits
   - Pushes bookmark deletions to remote

3. **Refresh Phase**:
   - Reloads the revision list after squashing
   - Re-populates PR states
   - Continues with normal push workflow

## Edge Cases Handled

- Multiple consecutive commits squashed together
- Non-consecutive PRs (won't be detected as squash-merge)
- Already merged PRs (skipped)
- PRs without numbers (ignored)

## Configuration

No additional configuration needed. The feature is automatic when:
- PRs exist for the commits
- GitHub shows one as merged and previous one(s) as closed
- Commits are consecutive in the stack