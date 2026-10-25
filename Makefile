# Telegram Agent — one entry point for everything.
# Each target maps to the role level that owns it (see AUTO_ROUTE.md).

SHELL := /usr/bin/env bash
VERSION := $(shell sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)

.DEFAULT_GOAL := help

.PHONY: help
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk 'BEGIN{FS=":.*?## "}{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

## --- run & configure (L8) -------------------------------------------------
.PHONY: setup
setup: ## Interactive first-run configuration (writes .env, 0600)
	./setup.sh

.PHONY: run
run: ## Build (debug) and run locally
	cargo run

.PHONY: run-release
run-release: ## Build (release) and run locally
	cargo build --release && ./target/release/telegram-agent

## --- quality gates (L3) ----------------------------------------------------
.PHONY: build
build: ## cargo build
	cargo build

.PHONY: test
test: ## Full gate suite: fmt, clippy, test, audit, deny, routing contract
	cargo fmt --check
	cargo clippy -- -D warnings
	cargo test
	cargo audit
	cargo deny check
	python3 scripts/verify-agent-tiers.py

.PHONY: fmt
fmt: ## Format all Rust code
	cargo fmt

.PHONY: doctor
doctor: ## Environment diagnostics (read-only)
	bash scripts/doctor.sh --fix-hints

.PHONY: verify-agents
verify-agents: ## Check the role-level routing contract
	python3 scripts/verify-agent-tiers.py

## --- fuzzing (L3) -----------------------------------------------------------
## Requires: rustup nightly + `cargo install cargo-fuzz`. Property tests run
## as part of `make test`; this runs a bounded libFuzzer campaign over the
## security-critical pure functions (esc, hmac+ct_eq, invoice parsing,
## gen_token).
.PHONY: fuzz
fuzz: ## Bounded fuzz campaign (FUZZ_SECS=45, sanitizer-free)
	cargo +nightly fuzz run -s none fuzz_core -- -max_total_time=$(FUZZ_SECS) -rss_limit_mb=4096

## --- distribution (L9) -----------------------------------------------------
.PHONY: icons
icons: ## Regenerate app icons from DESIGN.md tokens
	python3 scripts/gen_icons.py

.PHONY: package
package: ## Build host-platform bundle into dist/ (.exe/.app/tar.gz)
	bash scripts/package.sh

.PHONY: install
install: ## Install onto this (existing) Linux server, systemd service
	bash scripts/install.sh

.PHONY: status
status: ## Show install status on this machine
	bash scripts/install.sh --status

.PHONY: uninstall
uninstall: ## Remove the installed service (keeps data)
	bash scripts/install.sh --uninstall

.PHONY: backup
backup: ## Back up the database safely (WAL-aware)
	bash scripts/backup.sh

.PHONY: dist-clean
dist-clean: ## Remove build bundles in dist/
	rm -rf dist
