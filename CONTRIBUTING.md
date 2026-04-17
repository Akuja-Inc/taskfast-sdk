# Contributing to `taskfast-cli`

Thanks for your interest. This document describes how to get a local dev loop
going, the style we enforce, and how to ship changes.

## Development loop

```bash
# Build all crates
cargo build --workspace --locked

# Run the CLI
cargo run -p taskfast-cli -- --help

# Full test suite
cargo test --workspace --locked

# Format + lints (must be clean before PR)
cargo fmt --all --check
cargo clippy --all-targets --all-features --workspace --locked -- -D warnings

# Regenerate the OpenAPI-derived client when spec changes
cargo xtask sync-spec
```

MSRV is **Rust 1.91** (pinned in `rust-toolchain.toml`). CI tests on stable and
`1.91`; do not use features newer than 1.91 without bumping `rust-version` in
`Cargo.toml`.

### Git hooks (opt-in)

Run once after cloning to enable the same gates CI uses:

```bash
./.githooks/install.sh
```

- `pre-commit` runs `cargo fmt --check` (+ `typos` if installed).
- `pre-push` runs `cargo clippy -D warnings`, `cargo test`, and `cargo doc -D warnings`.

Bypass a hook with `--no-verify` (CI will still block).
`make ci` runs the same gate manually.

## Commit style — Conventional Commits

Every commit on `main` must follow [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <summary>

[optional body]

[optional footer(s)]
```

Allowed types: `feat`, `fix`, `docs`, `chore`, `refactor`, `perf`, `test`, `ci`,
`build`, `revert`. Scope is usually a crate name (`cli`, `agent`, `client`,
`chains`).

`release-plz` parses these commits to bump semver and generate `CHANGELOG.md` —
non-conforming subjects will be skipped from release notes.

Breaking changes: append `!` after the type/scope (e.g. `feat(cli)!: ...`) or
include a `BREAKING CHANGE:` footer.

## PR checklist

- [ ] `cargo fmt --all --check` clean
- [ ] `cargo clippy --all-targets --all-features --workspace --locked -- -D warnings` clean
- [ ] `cargo test --workspace --locked` green
- [ ] New public items have rustdoc (workspace warns on `missing_docs`)
- [ ] Commits follow Conventional Commits
- [ ] `CHANGELOG.md` update **not** required — release-plz handles it

## Release flow (maintainers only)

1. Merge changes to `main` with Conventional Commits.
2. `release-plz` opens a Release PR with version bump + CHANGELOG diff.
3. Merge the Release PR → git tag is created → `cargo-dist` builds artifacts +
   Homebrew formula + GHCR image → crates.io publish.

## Reporting bugs / requesting features

Use the issue templates in `.github/ISSUE_TEMPLATE/`. Security issues: see
[SECURITY.md](SECURITY.md) — **do not** open a public issue.
