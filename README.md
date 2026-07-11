# almighty-push

`almighty-push` reconciles one explicitly scoped jj change stack with collision-free GitHub head refs and stacked pull requests.

The tool is intentionally fail-closed. A normal run can rewrite local jj history, move remote refs, mutate pull requests, and persist execution checkpoints. It does not promise rollback across those systems.

## Requirements

- macOS or Linux
- Rust 1.89 or newer to build
- jj 0.41 or newer
- GitHub CLI (`gh`) authenticated for the selected repositories, except with `--no-pr`

```console
cargo install --path .
almighty-push --help
```

## Scope

Every invocation resolves and binds these independent facts:

- **source repository**: derived from the selected jj Git remote;
- **target repository**: `--repo HOST/OWNER/NAME`, or the source repository;
- **remote**: `--remote NAME`, or the only configured remote;
- **base**: `--base REF`, or the target repository's GitHub default branch;
- **tip**: `--tip REVSET`, or an implicit jj working-copy selection. When the
  exact `@` commit is both empty and fully undescribed, the implicit selection
  resolves to `@-`; otherwise it resolves to `@`. An explicit revset is always
  used exactly as supplied, including `--tip @`.

The effective `@` or `@-` revset is bound into the scope used by observation,
reports, plans, checkpoints, and persisted state. Resolving an omitted tip adds
one bounded, structured, read-only query for exact `@`; qualification failure or
an incomplete result stops the invocation before reconciliation effects.

Source and target may differ for a same-host fork pull request. Cross-host targets, ambiguous remotes, missing bases, unsafe workspace metadata, and non-linear selected graphs are rejected.

```console
# Reconcile the only remote with the target repository's default base.
almighty-push

# Pin every scope dimension.
almighty-push \
  --remote origin \
  --repo github.com/example/project \
  --base main \
  --tip '@'
```

The selected changes must induce one unique base-to-tip parent/child chain. Selected merges, forks, cycles, disconnected components, duplicate full change IDs, conflicts, and more than 64 changes fail before reconciliation effects.

### Daily jj workflow

A normal `jj commit` records the completed change and creates a fresh empty,
undescribed child at `@`. Omitting `--tip` skips that fresh working-copy commit:

```console
jj commit -m 'Complete the change'
almighty-push --base main
```

Here the effective tip is `@-`. If `@` has content or a description, omission
keeps `@` as the effective tip. To intentionally include the exact working-copy
commit regardless of those properties, supply it explicitly:

```console
almighty-push --base main --tip @
```

## Owned refs and PR bases

Managed identity is `(source repository, full 32-letter jj change ID)`. Each generated source head is:

```text
almighty-push/<full-jj-change-id>
```

Commit IDs, prefixes, titles, PR body prose, and mutable head commits are observations, not identity. For active chain `[c0, c1, ..., cn]`, PR `c0` targets the configured base and PR `ci` targets the exact owned head for `c(i-1)`.

Only the delimited `almighty-push:stack:v1` section of a PR body is managed. User-authored bytes outside it are preserved.

## Modes

### Full reconciliation

```console
almighty-push --base main
almighty-push --base main --delete-branches
```

After read-only scope discovery, full mode acquires the workspace lock before fetch, migration, reconciliation observation, checkpointing, or mutation. It fetches the exact selected remote, resumes any durable checkpoint, and executes at most four checked stages. Every effect validates its external precondition and postcondition; drift stops the run.

`--delete-branches` deletes only exact owned heads after their PR ownership has become historical.

### Dry run

```console
almighty-push --dry-run --base main
almighty-push --dry-run --no-pr --base main
```

Dry-run performs bounded read-only discovery and observation. It creates no lock, state directory, state file, or repository file; performs no fetch, rebase, push, ref deletion, or PR mutation; and prints canonical JSON action rows.

Exact head publications are symbolically applied so the preview includes the following PR stage. If a rebase or another effect would create identities that cannot be known without executing it, the report labels the terminal re-observation continuation instead of inventing IDs or effects.

### No PR

```console
almighty-push --no-pr --base main
```

`--no-pr` requires an explicit base and never starts `gh`. Its planner and executor have only jj capability. Existing PR ownership and lifecycle rows are preserved. If an old `.almighty` file still needs GitHub ownership validation, first run one full reconciliation; no-PR will not guess or erase that evidence.

## State, locking, and recovery

Durable state is namespaced to the canonical workspace:

```text
<workspace>/.jj/almighty-push/state-v3.json
```

The adjacent `lock` is an OS-backed advisory lock retained for the invocation and released by the OS on process exit. State replacement is bounded, same-directory, atomic, and durability-checked.

A legacy root `.almighty` v2 file is parsed only when v3 is absent. Every candidate remains non-authoritative until its exact repository, PR number, head, commit, and body marker are validated through GitHub. The old file is deleted only after all resolutions are durably recorded. Malformed, future-version, cross-scope, changed, symlinked, or ambiguous evidence fails closed.

Partial execution is expected. The complete exact plan and effect cursor are persisted before effects. On restart, an already-satisfied postcondition advances without replay; otherwise execution proceeds only while the recorded precondition still holds.

## Bounds

Compiled defaults and maxima are product behavior:

- 64 selected or managed changes;
- 10 GitHub pages of 100 rows;
- 512 effects per stage;
- 4 MiB combined command output;
- 4 MiB state;
- 64 KiB PR body;
- 30 seconds per subprocess;
- 5 seconds waiting for the repository lock;
- 4 reconciliation stages per invocation.

Hitting a completeness bound is an error, never an empty result.

## Output and exit status

- Human diagnostics and `--verbose` progress go to stderr.
- PR URLs go to stdout after a successful full run.
- Dry-run canonical actions go to stdout.
- `--json` emits one bounded structured report to stdout.
- Success exits 0, runtime/scope/drift failures exit 1, and invalid CLI syntax or flag combinations exit 2.

No command output containing tokens, full user-authored bodies, or child environments is included in routine error formatting.

## Safety notes

Use a deterministic fake `jj`/`gh` environment before changing command behavior. Do not point tests at a live repository or GitHub project. Dry-run is read-only but still performs bounded remote observations in full capability mode.
