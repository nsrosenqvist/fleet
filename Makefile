# Unified verify + build for the polyglot stacks living in this repo:
#   - Rust binary `fleet` (src/, Cargo.toml at root)
#   - TypeScript AO plugin (packages/tracker-git-bug/)
#
# Most day-to-day work is in one stack at a time, so feel free to just run
# `cargo …` or `pnpm …` directly. These targets exist for CI and for
# verifying both stacks before a commit.

TRACKER := packages/tracker-git-bug

.PHONY: verify verify-rust verify-tracker build build-rust build-tracker test clean

verify: verify-rust verify-tracker

verify-rust:
	cargo fmt --all --check
	cargo clippy --all-targets -- -D warnings
	cargo test

verify-tracker:
	cd $(TRACKER) && pnpm verify

build: build-rust build-tracker

build-rust:
	cargo build --release

build-tracker:
	cd $(TRACKER) && pnpm build

test:
	cargo test
	cd $(TRACKER) && pnpm test

clean:
	cargo clean
	cd $(TRACKER) && pnpm clean
