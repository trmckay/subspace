SHELL := /bin/sh
.DEFAULT_GOAL := check

.PHONY: check lint test build clean

check: lint test

lint: node_modules
	cargo fmt --all -- --check
	cargo clippy --locked -- -D warnings
	npm run lint

test: node_modules
	cargo test --locked
	cargo build --locked -p subspace-cli
	SUBSPACE_TEST_CLI="$(CURDIR)/target/debug/subspace" npm test

build: node_modules
	cargo build --locked
	npm run build

node_modules: package.json package-lock.json
	npm ci --no-audit --no-fund
	@touch node_modules

clean:
	cargo clean
	rm -rf dist
