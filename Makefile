# Verify + build for the fleet binary.
#
# Most day-to-day work is `cargo fmt`, `cargo clippy --all-targets --
# -D warnings`, and `cargo test`. These targets exist for CI and for a
# single-command "did I break anything?" check before commit.

.PHONY: verify build test clean

verify:
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings
	cargo test

build:
	cargo build --release

test:
	cargo test

clean:
	cargo clean
