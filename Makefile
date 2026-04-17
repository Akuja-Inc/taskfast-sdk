.PHONY: hooks fmt fmt-check clippy test doc ci

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

# Same gate the pre-push hook runs. Handy for manual verification.
ci: fmt-check clippy test doc
