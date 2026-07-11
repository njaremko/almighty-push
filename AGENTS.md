# Almighty Push contributor instructions

## Purpose and current shape

`almighty-push` reconciles one explicitly scoped, validated linear jj change stack
with collision-free source head refs and stacked pull requests in an exact target
GitHub repository. The binary is a thin parser over the typed application control
plane; deterministic policy remains in the planner and physical effects remain in
bounded adapters.

A full invocation may fetch or rebase local history, move remote refs, mutate
GitHub pull requests, and atomically publish durable checkpoints. Treat every
invocation as reconciliation with partial, irreversible effects, not as a simple
push wrapper. Dry-run and no-PR are distinct capability modes with the guarantees
in this file and `README.md`.

Current source, tests, and directly observed command behavior describe the
program. `README.md` is the intended user contract and must change with public
behavior. The maintained implementation plan records design history and current
validation status; it is not authority over current code.

## Repository workflow

- Use `jj`, never raw `git`, for version-control operations.
- Inspect the working copy with `jj status` before editing.
- Inspect changes with `jj diff --git`.
- Do not inspect old commits unless the task explicitly requires history.
- Preserve unrelated working-copy changes and keep commits logically focused.
- There are no Buck targets in this repository; use Cargo directly.

## Safety boundaries

Assume a normal invocation can mutate local jj operation history, remote
bookmarks/GitHub head refs (including non-fast-forward moves), GitHub pull
requests, and namespaced state under the canonical `.jj` directory.

Do not run the binary against a real repository or GitHub project while testing
unless the user explicitly authorizes every mutation class. The capability modes
are safety boundaries:

- `--dry-run` performs bounded read-only discovery and observation, creates no
  lock/state/repository file, fetches or rewrites no local history, mutates no
  ref or PR, and renders canonical actions plus explicit symbolic continuation
  when post-mutation identities cannot be known.
- `--no-pr` requires an explicit base, executes no `gh` process or GitHub effect,
  and preserves PR ownership/lifecycle records.
- `--delete-branches` is incompatible with `--no-pr` and deletes only an exact
  owned source ref through checked GitHub preconditions/postconditions.

Exercise command behavior from a temporary working directory with deterministic
fake `jj` and `gh` executables first on `PATH`. Never point a fixture at or
hand-edit a user's live `.almighty`.

A failed command, malformed output, truncated result, timeout, or ambiguous
result is not absence or success. Validate reads that determine identity,
ownership, existence, ancestry, lifecycle, or bases; verify every mutation's
postcondition before recording success.

Before any mutation, resolve the canonical jj workspace root and validate the
source repository, selected stack, Git remote, target GitHub repository, base,
state schema, and state namespace. Anchor state and lock paths at that root;
reject symlinks and unexpected path types. Use ownership-safe locking and atomic
same-directory state replacement. Never mutate a bookmark or PR from persisted
state alone: prove target scope, ownership, and the expected observed value.

## Target domain invariants

These are required target behaviors, not descriptions of the current source.

1. Managed identity is `(source_repository_id, full_jj_change_id)`. A Git commit
   ID is one observed version of a change, not its identity. Reject divergent or
   duplicate active identities. Prefixes, titles, body prose, PR numbers scraped
   from text, and mutable head commits are not identity or ownership proof.
2. Each active managed change owns one collision-free target head ref and has at
   most one active managed PR association. Closed and merged PRs are explicit
   historical lifecycle records rather than competing active identities.
3. Stack configuration separately names the source workspace/revision selection,
   Git remote, target GitHub repository, and target base. Do not hard-code `@`,
   `origin`, `main`, or GitHub.com in new behavior.
4. The selected changes must induce one unique parent-child chain. Reject a
   selected merge node, fork, disconnected component, or ambiguous order until
   its semantics are modeled; unrelated nonlinear history outside the selection
   is not grounds for rejection.
5. For active chain `[c0, c1, ..., cn]`, PR `c0` targets the configured base and
   PR `ci` targets the exact owned head ref for `c(i-1)`. Derive and validate the
   complete relation after reorder, merge, split, squash, deletion, lifecycle
   change, and an empty selection.
6. Local history, remote bookmarks, GitHub state, and persisted data are separate
   bounded observations. Within an observation epoch derive a deterministic plan.
   When fetch, rebase, push, or PR mutation invalidates dependent facts, stop at
   an explicit bounded barrier, re-observe, and derive the next plan. Revalidate
   each external precondition immediately before its effect.
7. `--dry-run` uses the same observation/planning logic, may perform bounded
   read-only queries, and renders the complete plan, but creates no file or lock,
   fetches or rewrites no local repository state, moves no ref, mutates no PR,
   and writes no persisted state. `--no-pr` executes no `gh` command, emits no PR
   effect, and preserves all PR ownership and lifecycle records.
8. Distinguish rebuildable observation cache, durable ownership/intent records,
   and durable execution checkpoints. Missing data is not permission to create a
   duplicate. Rebuild only from complete exact ownership metadata; malformed,
   ambiguous, cross-repository, or future-version data fails closed.
9. Preserve user-authored PR title and body content. Managed metadata belongs in
   one bounded, delimited, versioned section; replacing user content requires
   explicit user intent.
10. Partial execution is expected. Persist plan identity and preconditions before
    effects. For each effect, record the expected old value and success
    postcondition; on retry, accept an already-satisfied postcondition, execute
    only while the precondition holds, and otherwise fail on drift. Never promise
    atomic rollback across irreversible systems.
11. Bounds are behavior: cap selected changes, subprocess duration/output, lock
    wait, GitHub pages/results, retries, state/checkpoint history and bytes,
    generated body bytes, and planned effects. Hitting a completeness bound is a
    precise error, never absence.

Use precise types for repository scope, change and commit IDs, local/remote
bookmarks, GitHub refs, PR number/lifecycle, command outcomes, observations,
plans, and effect checkpoints. Keep policy deterministic without subprocesses;
put `jj`, `gh`, filesystem, clock, and locking behind narrow effect boundaries.

## Testing

A bug fix starts with the smallest failing behavioral test. Do not use source
string assertions. Use fake adapters or executable fake `jj`/`gh` programs to
assert commands, exit handling, state transitions, and externally observable
results.

Choose tests from the transitions and boundaries the change touches. The
reconciliation suite as a whole must cover the complete applicable matrix below;
a narrow change need not duplicate unrelated scenarios:

- zero, one, many, and maximum-size selected chains;
- create, update, reorder, merge, close, reopen, split, squash, and deletion;
- malformed, future-version, cross-repository, and symlinked state;
- missing PRs, duplicate ownership, pagination caps, and concurrent remote drift;
- command failure, timeout, malformed output, and lost response before or after
  each mutation;
- retry/resume after partial execution;
- dry-run plan equivalence with zero side effects and strict `--no-pr` isolation;
- selected merge/fork/disconnected histories and conflicts introduced by rebase.

Run the narrowest relevant test first, then before committing Rust changes run:

```bash
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

A `cargo test` result that reports zero tests is not verification of behavior.
Do not commit Rust changes while Clippy or the test suite fails.

## Documentation and reporting

When implemented behavior replaces an aspirational claim, remove or rewrite the
conflicting claim. Replace flawed runtime models end to end rather than adding
behavioral shims; preserve persisted-state compatibility only through an
explicit, versioned, tested migration with recovery behavior. Update `README.md`
and the maintained Unreleased changelog whenever public behavior changes.

When reporting work, state the exact checks run, any intentionally skipped live
repository/GitHub checks, and the remaining operational risk.
