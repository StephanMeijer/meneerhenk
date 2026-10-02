# Meneer Henk

The team's AI colleague: a strict, dry, senior software engineer and code
reviewer who works on GitHub pull requests, GitLab merge requests, issues,
Discord and email. He is advisory. He never approves, blocks, merges, pushes
or changes code.

What Henk does and promises is in [docs/SPEC.md](docs/SPEC.md). This
repository is how he is built.

## Layout

| Crate | Purpose |
|---|---|
| `crates/henk-domain` | The vocabulary and rules of the spec, free of I/O: identities and standing (§2), the allowlist (§2, §8.7), review outcomes and the check/status they produce (§3.3), Discord attention rules (§5.1, §5.4), mail rules (§6), hidden markers (§8.6), style rules (§7). |
| `crates/henk` | The `henk` binary. Loads and validates configuration. The platform surfaces are built on top of it. |

The domain crate holds no credentials and does no I/O (§8.4). Every surface
that talks to a platform, a mailbox or a model lives outside it and uses its
types.

## Build and test

Requires Rust 1.93 (pinned in `rust-toolchain.toml`).

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
cargo deny check
```

## Run

```sh
cargo run -- config example > henk.toml   # then edit the ids
cargo run -- config check --path henk.toml
```

`henk.toml` is git-ignored. Tokens never go in it.
