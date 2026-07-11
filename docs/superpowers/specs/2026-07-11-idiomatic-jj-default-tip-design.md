# Idiomatic jj Default Tip Design

## Problem

A normal jj workflow treats the working copy as a commit. After `jj commit`, jj
records the completed change and creates a fresh empty, undescribed child at `@`.
`almighty-push` currently defaults `--tip` to `@`, so that ordinary workflow can
select the fresh working-copy commit instead of ending the stack at the completed
change in `@-`.

The tool must preserve exact explicit revset behavior while making its omitted
`--tip` default match the ordinary jj commit loop.

## User-visible behavior

The CLI distinguishes an omitted tip from an explicit revset.

- When `--tip` is omitted, the tool observes exact `@`. If `@` is both empty and
  undescribed, the effective tip is `@-`. Otherwise, the effective tip is `@`.
- When `--tip <REVSET>` is supplied, the tool uses that revset exactly. Explicit
  `--tip @` is never normalized.
- Reports, persisted scope, plans, and checkpoints contain the effective revset,
  not an ambiguous implicit-default marker.

This changes only tip selection. Existing chain validation, change identity,
remote selection, base selection, and reconciliation behavior remain unchanged.

## Representation and ownership

The CLI passes a typed tip-selection intent rather than collapsing omission and
an explicit `@` into the same string:

- `ImplicitWorkingCopy`
- `ExplicitRevset(String)`

Configuration resolution owns conversion from intent to an effective revset.
It already owns the facts bound into `Scope`, so resolving the default there
keeps persisted scope equal to the selection actually observed by the planner.
`Scope` continues to contain a validated concrete revset string.

No compatibility path or fallback is introduced. Internal callers and fixtures
must choose their intent explicitly.

## Observation flow

For `ImplicitWorkingCopy`, configuration performs one bounded, read-only jj
query for exact `@` using structured template output. The row must identify one
current commit and expose the two facts needed by policy:

- whether the commit is empty;
- its complete description.

The resolver selects `@-` only when the row reports an empty commit and the
complete description is empty. A described empty change and a non-empty
undescribed change both remain selected as `@`.

For `ExplicitRevset`, configuration performs no default-tip qualification query.
The existing bounded stack observation validates the revset and selected graph.

The qualification command uses `--ignore-working-copy`, the resolved workspace,
the restricted jj environment, the existing subprocess timeout, and the existing
combined-output bound. It does not fetch, update the working copy, create state,
or acquire the application state lock. Mutating modes continue to hold the
configuration lock before this observation; dry-run remains lock-free and
read-only.

## Failure model and bounds

Default-tip qualification fails closed when jj exits unsuccessfully, times out,
returns malformed structured output, exceeds output bounds, or returns anything
other than exactly one `@` row. No heuristic based on titles, commit summaries,
or textual command diagnostics is allowed.

The query has a row limit of one and reuses the compiled subprocess byte and time
bounds. It introduces no loop, retry, or new persistent data.

## Reporting and documentation

JSON and human-visible scope reporting show `@-` when the implicit fresh working
copy was normalized and `@` otherwise. README examples explain that omitting
`--tip` follows the usual `jj commit` loop, while explicit `--tip` always means
exactly what was supplied. CLI help describes the distinction. The Unreleased
changelog records the public behavior change.

## Verification

Behavioral tests must prove:

1. omitted `--tip` resolves to `@-` for an empty, undescribed `@`;
2. omitted `--tip` remains `@` for a described empty `@`;
3. omitted `--tip` remains `@` for a non-empty undescribed `@`;
4. explicit `--tip @` remains exact and skips normalization;
5. malformed, duplicate, absent, failed, timed-out, and oversized qualification
   results fail before reconciliation effects;
6. dry-run qualification creates no lock, state, repository file, fetch, or
   mutation;
7. persisted scope, reports, and subsequent stack queries use the same effective
   revset.

Tests use deterministic fake jj executables or adapters. No test targets a live
repository or GitHub project. Before committing Rust implementation changes, run:

```console
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

## Non-goals

- Changing explicit revset semantics.
- Removing intentional empty changes elsewhere in a selected stack.
- Inferring stack intent from bookmarks, descriptions, or commit history.
- Supporting nonlinear selected histories.
- Fetching or rewriting local history during default-tip qualification.
