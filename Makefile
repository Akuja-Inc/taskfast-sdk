.PHONY: hooks fmt fmt-check clippy test doc ci audit deny machete semver coverage supply-chain ci-full bump-patch bump-minor bump-major

hooks:
	./.githooks/install.sh

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

test:
	cargo test --workspace --locked

doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --locked

# Vulnerability scan. Matches the rustsec/audit-check job in CI.
# Requires: cargo install cargo-audit
audit:
	cargo audit --deny warnings=false

# Advisory + license + bans + sources. Matches the cargo-deny job in CI.
# Requires: cargo install cargo-deny
deny:
	cargo deny check

# Detect unused workspace dependencies. Requires: cargo install cargo-machete
machete:
	cargo machete

# Detect breaking public-API changes vs. last crates.io release.
# Requires: cargo install cargo-semver-checks
semver:
	cargo semver-checks --package taskfast-cli

# Line coverage via cargo-llvm-cov. Produces lcov.info.
# Requires: cargo install cargo-llvm-cov; rustup component add llvm-tools-preview
coverage:
	cargo llvm-cov --workspace --locked --lcov --output-path lcov.info

supply-chain: audit deny machete

# Same gate the pre-push hook runs. Handy for manual verification.
ci: fmt-check clippy test doc

# Full gate including supply-chain. Use before pushing risky dep bumps.
ci-full: ci supply-chain

# Bump workspace version (taskfast-cli + taskfast-agent). See CONTRIBUTING.md.
bump-patch:
	cargo xtask bump patch

bump-minor:
	cargo xtask bump minor

bump-major:
	cargo xtask bump major
