# Changelog

## [Unreleased]

### Changed

- Replaced the CWD-relative prototype with one typed staged control plane over strict configuration discovery, retained workspace handles, state v3, bounded jj/GitHub adapters, the pure planner, and the resumable executor.
- Managed identity is now the exact `(source repository, full jj change ID)` pair. Generated heads use `almighty-push/<full-change-id>`; prefix, title, operation-prose, and mutable-commit heuristics were removed.
- Source remote, source repository, target repository, target base, and tip selection are independently resolved and bound into every observation, plan, checkpoint, and effect.
- Selected history must be one bounded parent-child chain. Merge nodes, forks, cycles, disconnected selections, duplicate identities, and conflicts fail closed.
- PR bases now derive from complete active-chain adjacency after reorder, merge, split, squash, deletion, and empty selection.
- User PR content is byte-preserved outside one bounded versioned managed section.
- Partial execution now uses durable exact-plan checkpoints and postcondition recovery rather than fictional cross-system transactions or rollback.

### Safety

- `--dry-run` creates no lock, state namespace, state file, or repository file and executes no fetch, rebase, push, ref deletion, PR mutation, or state write. It renders bounded canonical actions, renders any exact durable pending checkpoint before deriving new work, and explicitly labels continuations whose post-mutation identities cannot be known.
- `--no-pr` requires `--base`, starts no `gh` process, emits no GitHub effect, and preserves PR ownership and lifecycle rows.
- After the single minimal `jj workspace root` discovery needed to identify the repository, full execution holds an OS-released `.jj` configuration lock before remote/GitHub configuration observation and retains it across state namespace creation, the state writer lock, fetch, migration, reconciliation observation, checkpoint publication, and mutation. Re-observation is bounded to four stages.
- Every subprocess has explicit argument/environment/input/output/time bounds and a retained no-follow working-directory identity. Timeout and cleanup own the complete process group. GitHub token variables are passed only to `gh`, never to `jj`.
- GitHub pagination, identity, ownership, lifecycle, bases, bodies, and exact refs are validated; incomplete or ambiguous observations are errors.
- Every external mutation is bracketed by an exact precondition and postcondition observation. Lost successful responses are recovered only from the postcondition.
- State is private, size-bounded, same-directory atomically replaced, directory-synced, and namespaced under the canonical `.jj` metadata directory.
- Legacy `.almighty` v2 records migrate once through exact GitHub evidence; they never grant authority by themselves and are deleted only after durable resolution.

### CLI

- Omitting `--tip` now follows the ordinary `jj commit` workflow: a fresh empty,
  undescribed working-copy commit resolves to `@-`, while other working-copy
  commits remain `@`. Explicit revsets, including `--tip @`, remain exact.
- Added `--remote`, `--repo`, `--base`, `--tip`, `--dry-run`, `--no-pr`, `--delete-branches`, `--json`, and `--verbose` with typed validation and explicit conflicts.
- JSON reports include the exact scope and ordered stage identities, action counts, outcomes, canonical dry-run actions, and resulting PR URLs.
- Runtime failures exit 1; Clap syntax and value failures exit 2.

### Packaging and dependencies

- Declared the MIT license, Rust 1.89 minimum (the exact `File::try_lock` API floor), repository/homepage/readme metadata, keywords, and categories.
- Removed direct `anyhow`, `chrono`, and `regex` dependencies with the obsolete prototype.
- Retained only `clap`, `serde`, `serde_json`, `sha2`, and `libc` as direct crates. `libc` remains for audited Unix process-group signalling/reaping, no-follow handle-relative filesystem operations, child `fchdir`, and nonblocking descriptors; no dependency offers a smaller portable standard-library replacement for those contracts.
- The complete resolved dependency graph is pinned in `Cargo.lock`. License and direct-dependency rationale are recorded in `docs/dependencies.md`; the local release gate validated `cargo metadata --locked` and `cargo tree --locked --depth 2`.

### Testing and delivery

- Added deterministic fake-binary CLI coverage for dry-run, strict no-PR, full staged reconciliation, lock ordering, scope/flag errors, state effects, and help/version behavior without live GitHub.
- Added read-only state and caller-retained locked-session behavioral tests.
- Added Linux and macOS CI for formatting, all-target checking, Clippy with warnings denied, and the complete test suite using default test parallelism.

### Migration notes

The old root `.almighty` file is no longer a runtime state writer. A full GitHub-enabled run validates and seals its candidates into v3. No-PR refuses pending legacy evidence because it cannot validate ownership without violating its zero-`gh` contract.

The old `.almighty.lock`, short `push-*` refs, title/PR-number scraping, tolerant command failures, legacy split/squash prose inference, CWD state writes, and claimed transaction rollback are not supported.
