# Dependency evidence

This crate targets macOS and Linux with Rust 1.89, the exact standard-library API floor required by `File::try_lock`. `Cargo.lock` is the authoritative pinned transitive graph; release validation must run the complete suite on Rust 1.89, plus `cargo tree --depth 2` and `cargo metadata --locked --no-deps --format-version 1`, without network-dependent product behavior.

| Direct crate | Locked family | License | Why retained | Relevant failure/resource behavior |
|---|---:|---|---|---|
| `clap` | 4.5 | MIT OR Apache-2.0 | Typed CLI parser, generated help/version, and exit-code-2 boundary errors. | Parsing is bounded by the process argument vector and validated domain constructors; no runtime network or background work. |
| `serde` | 1.0 | MIT OR Apache-2.0 | Checked domain/state/plan serialization. | All external/state inputs have byte, collection, and depth bounds before or during decoding. |
| `serde_json` | 1.0 | MIT OR Apache-2.0 | Canonical plan/state and REST JSON. | Writers are bounded; malformed or unknown structures fail closed. |
| `sha2` | 0.10 | MIT OR Apache-2.0 | Stable plan, state-generation, body, migration, and checkpoint proof digests. | Streaming hashing is linear in already-bounded input and does not perform I/O. |
| `libc` | 0.2 | MIT OR Apache-2.0 | Narrow Unix APIs not exposed by the selected standard-library baseline: process groups/signals/reaping, subreaper control, no-follow `openat` traversal, `fchdir`, descriptor flags, and durable macOS sync. | Calls are wrapped in typed errors, retained-handle identity checks, RAII cleanup, explicit deadlines, and audited safety comments. Platform support is intentionally macOS/Linux. |

Removed direct crates:

- `anyhow`: the product boundary now reports typed configuration, state, adapter, planning, and execution failures.
- `chrono`: the new state/checkpoint model does not use wall-clock timestamps for authority, expiry, or locking.
- `regex`: structured JSON and exact typed identities replace prose, title, and prefix parsing.

No new dependency, generator, test framework, package manager, or runtime service was added for Tasks 9-10.
