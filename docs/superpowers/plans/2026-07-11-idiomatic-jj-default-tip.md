# Idiomatic jj Default Tip Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make an omitted `--tip` reconcile through `@-` when `@` is a fresh empty, undescribed jj working-copy commit while preserving every explicit revset exactly.

**Architecture:** Carry omission as a typed `TipSelection` from the CLI into configuration resolution. Resolve only the implicit selection with one bounded structured jj query, then store the concrete effective revset in the existing `Scope`, so observation, reports, plans, checkpoints, and persisted state all agree.

**Tech Stack:** Rust 2021, Rust 1.89+, Clap 4.5, Serde/serde_json, deterministic fake `jj` executables, Cargo, Jujutsu.

## Global Constraints

- Use `jj`, never raw `git`; inspect changes with `jj diff --git`.
- Begin implementation with `jj status` and preserve unrelated working-copy changes.
- Load and apply the `hickey` and `algebra-driven-design` skills before production changes because this changes CLI intent, configuration, and persisted scope boundaries.
- Start each behavior change with the smallest failing behavioral test.
- Do not add dependencies, fallbacks, shims, compatibility paths, or source-text assertions.
- The implicit-tip query is read-only, uses `--ignore-working-copy`, returns at most one bounded structured row, and introduces no retry or unbounded resource.
- Explicit `--tip <REVSET>` must remain exact, including explicit `--tip @`.
- Dry-run must create no lock, state namespace, state file, repository file, fetch, local rewrite, remote mutation, or GitHub mutation.
- Never run the binary against a real repository or GitHub project; use deterministic fake `jj`/`gh` programs.
- Before each non-trivial Rust commit, run the complete crate verification gate.

## File Structure

- Modify `src/config.rs`: define tip-selection intent, qualify implicit `@`, and bind the effective revset into `Scope`.
- Modify `src/app.rs`: carry typed selection through `RunOptions` and report the effective revset already held by `ResolvedConfig`.
- Modify `src/main.rs`: preserve whether Clap observed `--tip` rather than assigning a string default.
- Modify `tests/config_resolution.rs`: test policy, exact command shape, malformed/failed observations, and effective scope.
- Modify `tests/cli_workflow.rs`: prove omitted and explicit CLI behavior end to end, including dry-run safety.
- Modify test constructors using `ConfigInput` or `RunOptions`: choose `TipSelection::ExplicitRevset` where fixtures require their current exact scope.
- Modify `README.md`, `CHANGELOG.md`, and CLI help in `src/main.rs`: document the public workflow.

---

### Task 1: Typed Tip Intent and Bounded Qualification

**Files:**
- Modify: `src/config.rs`
- Test: `tests/config_resolution.rs`
- Modify mechanically: every test/module constructing `ConfigInput`

**Interfaces:**
- Produces: `pub enum TipSelection { ImplicitWorkingCopy, ExplicitRevset(String) }`
- Changes: `ConfigInput::tip_selection: TipSelection` replaces `tip_revset: String`
- Produces privately: `ConfigResolver::resolve_tip_revset(&self, selection: &TipSelection, workspace_root: &Path, limits: Limits) -> Result<String, ConfigError>`
- Preserves: `ResolvedConfig::tip_revset() -> &str` and `Scope::tip_revset() -> &str` return the concrete effective revset.

- [ ] **Step 1: Load design skills and inspect the clean working copy**

Run:

```console
jj status
```

Read completely:

```text
/Users/njaremko/.agents/skills/hickey/SKILL.md
/Users/njaremko/.agents/skills/algebra-driven-design/SKILL.md
```

Record the governing law before editing: explicit revsets denote themselves; implicit selection denotes `@-` exactly when exact `@` is empty and has an empty full description, and denotes `@` otherwise.

- [ ] **Step 2: Write failing configuration-policy tests**

In `tests/config_resolution.rs`, add a recording fake executor that returns workspace root, remote rows, and the implicit-tip JSON row in sequence. Add behavioral cases equivalent to:

```rust
#[test]
fn implicit_tip_skips_only_a_fresh_working_copy() {
    for (empty, description, expected) in [
        (true, "", "@-"),
        (true, "named empty change", "@"),
        (false, "", "@"),
    ] {
        let input = ConfigInput {
            remote: Some(RemoteName::parse("origin").unwrap()),
            repository: Some(RepositoryId::parse("github.com/source/project").unwrap()),
            base: Some(HeadRef::parse("main").unwrap()),
            tip_selection: TipSelection::ImplicitWorkingCopy,
            limits: limits(),
            github_enabled: false,
        };
        let config = resolver_with_tip_row(empty, description)
            .resolve(&input, workspace())
            .unwrap();
        assert_eq!(config.tip_revset(), expected);
        assert_eq!(config.scope().tip_revset(), expected);
    }
}

#[test]
fn explicit_tip_is_not_qualified_or_rewritten() {
    let input = ConfigInput {
        remote: Some(RemoteName::parse("origin").unwrap()),
        repository: Some(RepositoryId::parse("github.com/source/project").unwrap()),
        base: Some(HeadRef::parse("main").unwrap()),
        tip_selection: TipSelection::ExplicitRevset("@".to_owned()),
        limits: limits(),
        github_enabled: false,
    };
    let (resolver, calls) = resolver_without_tip_reply();
    let config = resolver.resolve(&input, workspace()).unwrap();
    assert_eq!(config.tip_revset(), "@");
    assert!(calls.iter().all(|call| !call.args().iter().any(|arg| arg == "@")));
}
```

Assert observable command arguments structurally: `--ignore-working-copy`, `log`, `--no-graph`, `--revision`, `@`, `--limit`, `2`, and the exact structured template. Do not assert source strings.

- [ ] **Step 3: Run the narrow tests and confirm the expected failure**

Run:

```console
cargo test --test config_resolution implicit_tip -- --nocapture
cargo test --test config_resolution explicit_tip -- --nocapture
```

Expected: compilation fails because `TipSelection` and `ConfigInput::tip_selection` do not exist.

- [ ] **Step 4: Add the typed intent and qualify implicit `@`**

In `src/config.rs`, add:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TipSelection {
    ImplicitWorkingCopy,
    ExplicitRevset(String),
}
```

Replace `ConfigInput::tip_revset` with `tip_selection`. Add a private denied-unknown-fields row:

```rust
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkingCopyRow {
    empty: bool,
    description: String,
}
```

Add precise configuration errors for malformed JSON and row cardinality, retaining the serde source where applicable. Implement `resolve_tip_revset` so it:

1. validates and returns an `ExplicitRevset` unchanged without running a qualification query;
2. runs an implicit query for exact `@` with `--limit 2` and a structured JSON-lines template exposing `empty` and full `description`;
3. parses lines with a hard maximum of one row;
4. returns `@-` only for `WorkingCopyRow { empty: true, description }` when `description.is_empty()`;
5. returns `@` for the other valid rows;
6. fails for zero rows, two rows, malformed JSON, command failure, timeout, or output overflow.

Call this after canonical workspace/metadata validation and configuration-lock acquisition, and before constructing `Scope`. Pass its concrete result to `Scope::new`.

Update existing `ConfigInput` constructors mechanically to use:

```rust
tip_selection: TipSelection::ExplicitRevset("@".to_owned()),
```

This preserves fixture behavior and prevents tests unrelated to the new policy from acquiring another fake command.

- [ ] **Step 5: Add negative cardinality and malformed-result tests**

Add table-driven tests for empty output, two valid rows, malformed JSON, and nonzero command exit. Assert the precise `ConfigError` variant and assert no later GitHub/reconciliation command was recorded. Reuse existing command timeout/output-bound tests if they exercise this exact query; otherwise add cases using the existing bounded executor facilities.

- [ ] **Step 6: Run focused and crate-wide verification**

Run:

```console
cargo test --test config_resolution
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Expected: all commands pass and the test suite reports nonzero tests.

- [ ] **Step 7: Request triggered design review and address every comment**

Use one bounded review workflow covering Hickey and algebra-driven-design lenses. Give it the approved spec, `src/config.rs`, affected tests, and `jj diff --git`. Record the returned review artifact in markdown, then either change the code or record a specific evidence-based reason each comment does not apply. Rerun the complete verification gate after any code change.

- [ ] **Step 8: Commit the typed configuration behavior**

Run:

```console
jj diff --git
jj commit -m $'Resolve the implicit jj working-copy tip\n\nPreserve explicit revsets exactly and bind an omitted tip to @- only for a fresh empty, undescribed working-copy commit.'
```

Expected: the completed change is `@-` and `@` is a fresh empty child.

---

### Task 2: Preserve Omission at the CLI Boundary

**Files:**
- Modify: `src/main.rs`
- Modify: `src/app.rs`
- Test: `tests/cli_workflow.rs`
- Modify mechanically: every test/module constructing `RunOptions`

**Interfaces:**
- Consumes: `config::TipSelection`
- Changes: `RunOptions::tip_selection: TipSelection` replaces `tip_revset: String`
- Preserves: reports expose `ResolvedConfig::tip_revset()`, the effective concrete value.

- [ ] **Step 1: Write failing end-to-end CLI tests**

Extend the fake `jj` support so a fixture can describe `@` as empty/non-empty and described/undescribed and can record the qualification query. Add tests equivalent to:

```rust
#[test]
fn omitted_tip_reports_parent_for_a_fresh_jj_working_copy() {
    let fixture = CliFixture::new_with_working_copy(true, "");
    let output = fixture.run(&["--dry-run", "--no-pr", "--base", "main", "--json"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report = CliFixture::json_stdout(&output);
    assert_eq!(report["tip"], "@-");
    assert!(fixture.stack_query_used_tip("@-"));
}

#[test]
fn explicit_at_remains_exact_for_a_fresh_jj_working_copy() {
    let fixture = CliFixture::new_with_working_copy(true, "");
    let output = fixture.run(&[
        "--dry-run", "--no-pr", "--base", "main", "--tip", "@", "--json",
    ]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let report = CliFixture::json_stdout(&output);
    assert_eq!(report["tip"], "@");
    assert!(fixture.stack_query_used_tip("@"));
    assert!(!fixture.default_tip_was_qualified());
}
```

Also assert the omitted-tip dry run has no lock, state namespace, fetch, `jj` mutation, or `gh` process.

- [ ] **Step 2: Run the two narrow CLI tests and confirm failure**

Run:

```console
cargo test --test cli_workflow omitted_tip_reports_parent -- --nocapture
cargo test --test cli_workflow explicit_at_remains_exact -- --nocapture
```

Expected: tests fail because Clap still collapses omission into `"@"`.

- [ ] **Step 3: Preserve omission through Clap and application options**

In `src/main.rs`, replace the defaulted string with:

```rust
#[arg(long, value_parser = parse_tip)]
tip: Option<String>,
```

Map it at the boundary:

```rust
let tip_selection = match args.tip {
    Some(revset) => TipSelection::ExplicitRevset(revset),
    None => TipSelection::ImplicitWorkingCopy,
};
```

In `src/app.rs`, replace `RunOptions::tip_revset` with `tip_selection: TipSelection`, then pass it unchanged into `ConfigInput`. Update direct `RunOptions` constructors to use `ExplicitRevset` unless the test intentionally exercises omission.

Do not add a second normalization point in `app`, `JjClient`, or the planner. The resolved `Scope` remains the single concrete source of truth.

- [ ] **Step 4: Run focused and complete verification**

Run:

```console
cargo test --test cli_workflow
cargo test --test cli_safety
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
```

Expected: all commands pass.

- [ ] **Step 5: Extend the triggered review and address every comment**

Use one bounded review workflow with the final Task 1–2 diff and the same Hickey/algebra lenses. Focus the review on whether omission survives exactly one boundary, whether persisted scope equals observed scope, and whether dry-run remains read-only. Address all comments and rerun the complete gate after changes.

- [ ] **Step 6: Commit the CLI behavior**

Run:

```console
jj diff --git
jj commit -m $'Preserve omitted tip intent at the CLI boundary\n\nLet configuration distinguish the idiomatic jj default from an explicit @ revset.'
```

---

### Task 3: Document and Validate the Daily jj Workflow

**Files:**
- Modify: `README.md`
- Modify: `CHANGELOG.md`
- Modify: `src/main.rs` (help text only)
- Test: `tests/cli_safety.rs`

**Interfaces:**
- Consumes: the implemented implicit/explicit tip behavior.
- Produces: user documentation and help output describing that behavior without changing runtime interfaces.

- [ ] **Step 1: Add a behavioral help test**

Extend `help_and_version_are_successful_without_discovery` or add a focused test that runs `--help` and asserts user-visible behavior, not source text:

```rust
let output = fixture.run(&["--help"]);
let stdout = String::from_utf8(output.stdout).unwrap();
assert!(stdout.contains("fresh empty working-copy"));
assert!(stdout.contains("explicit --tip"));
assert!(fixture.records().is_empty());
```

- [ ] **Step 2: Run the help test and confirm failure**

Run:

```console
cargo test --test cli_safety help -- --nocapture
```

Expected: failure because help does not yet describe normalization.

- [ ] **Step 3: Update help, README, and changelog**

Change the `--tip` doc comment in `src/main.rs` to state that omission uses `@-` for a fresh empty, undescribed working copy and explicit values remain exact.

In `README.md`:

- replace “tip defaults to `@`” with the implicit policy;
- add an ordinary sequence showing `jj commit` followed by `almighty-push`, explaining that the fresh `@` is skipped;
- state that `almighty-push --tip @` explicitly includes `@`;
- keep all dry-run and safety claims unchanged.

In `CHANGELOG.md` under Unreleased/CLI, record that omitted tip selection now follows the ordinary jj commit loop while explicit revsets remain exact.

- [ ] **Step 4: Run final verification from a clean understanding of the diff**

Run:

```console
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
jj diff --git
jj status
```

Expected: all Cargo commands pass; the diff contains only this whole feature and its documentation; unrelated changes are absent.

Live repository/GitHub validation must remain skipped because the safety contract forbids mutation without explicit authorization. The real-jj adapter test may run only in its isolated temporary fixture.

- [ ] **Step 5: Commit documentation and final validation evidence**

Run:

```console
jj commit -m $'Document the idiomatic jj push workflow\n\nExplain implicit fresh-working-copy selection, exact explicit tips, and the preserved dry-run guarantees.'
```

Record the exact four verification commands and outcomes in the tracker or final work report, along with the skipped live-repository/GitHub check and its safety reason.
