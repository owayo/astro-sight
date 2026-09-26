# Development tasks for astro-sight. Run `make` with no arguments to list the targets.
#
# Tool versions are pinned in mise.toml. When mise is available, every tool runs through
# `mise exec --`, so the pinned versions are used even when mise is not activated in the shell
# (for example when make is started from an IDE or a GUI). SYSTEM_TOOLS=1 uses the tools on PATH
# instead (the versions are then not guaranteed).
#
# The CI quality job only runs `make setup` and `make ci`. Add new checks to a target that ci
# depends on instead of repeating commands in the workflow.
#
# Only GNU Make 3.81 features are used (the make that ships with macOS):
# no .ONESHELL, .SHELLFLAGS, $(file ...) or !=.

.DEFAULT_GOAL := help

BINARY_NAME := astro-sight
INSTALL_PATH ?= /usr/local/bin
# Cargo.lock is committed, so resolve dependencies exactly as CI does. When a [patch] in
# .cargo/config.toml swaps in local tree-sitter crates and Cargo.lock changes only on your machine,
# drop the flag with `make ci CARGO_FLAGS=`.
CARGO_FLAGS ?= --locked
# AI agents that receive the skill after install. `make install SKILL_TARGETS=` installs none.
SKILL_TARGETS ?= claude codex

# macOS: the deployment target shared by the cc crate (the tree-sitter C parsers) and rustc
# (MACOSX_DEPLOYMENT_TARGET) is set in the [env] table of mise.toml. $(RUN) (= mise exec --) passes
# it on, so it is not exported here: if make and a bare cargo saw different values, the C parsers
# would be rebuilt back and forth.

# macOS: use Apple's ar (/usr/bin/ar). Exporting GNU binutils' ar (to silence the -D flag warning)
# broke linking: newer Apple ld (ld-1267 and later) rejects static archives made by GNU ar 2.46
# with "member not 8-byte aligned". AR is also part of the cc crate's build fingerprint, so make
# (GNU ar) and a bare cargo (Apple ar) kept re-running the build scripts. The override was removed.

# ---- Toolchain ------------------------------------------------------------------
# Look for mise on PATH, then in the usual install locations (make started from a GUI may not
# inherit the shell's PATH). Override with make MISE=/path/to/mise.
# To try the behavior without mise, empty the candidates with MISE_CANDIDATES=.
MISE_CANDIDATES ?= $(HOME)/.local/bin/mise /opt/homebrew/bin/mise /usr/local/bin/mise
ifeq ($(SYSTEM_TOOLS),1)
RUN :=
else
ifndef MISE
MISE := $(firstword $(shell command -v mise 2>/dev/null) $(wildcard $(MISE_CANDIDATES)))
endif
ifeq ($(MISE),)
ifneq ($(filter-out help,$(or $(MAKECMDGOALS),help)),)
$(error mise was not found. Install it from https://mise.jdx.dev, or add SYSTEM_TOOLS=1 to use the tools on PATH)
endif
endif
RUN := $(if $(MISE),$(MISE) exec --,)
endif

.PHONY: help setup build release run test lint fmt fmt-check check ci install uninstall clean

## Setup

setup: ## Install the toolchain (mise) and dependencies
	@if [ -n "$(MISE)" ]; then "$(MISE)" install; fi
	$(RUN) cargo fetch $(CARGO_FLAGS)

## Build

build: ## Build a debug binary
	$(RUN) cargo build $(CARGO_FLAGS)

release: ## Build a release binary
	$(RUN) cargo build --release $(CARGO_FLAGS)

run: ## Run the debug binary (arguments via ARGS="...")
	$(RUN) cargo run $(CARGO_FLAGS) -- $(ARGS)

## Checks

# Tests run with the default features (the same mimalloc setup as the released binary).
# With --all-features the dhat-heap allocator is used and every process writes dhat-heap.json.
test: ## Run the tests
	$(RUN) cargo test $(CARGO_FLAGS)

# clippy runs twice: with the default features (the released binary, #[cfg(not(feature = "dhat-heap"))])
# and with --all-features so that the dhat-heap code is checked too.
lint: ## Run clippy with warnings as errors
	$(RUN) cargo clippy $(CARGO_FLAGS) --all-targets -- -D warnings
	$(RUN) cargo clippy $(CARGO_FLAGS) --all-targets --all-features -- -D warnings

fmt: ## Format the code (rewrites files)
	$(RUN) cargo fmt --all

fmt-check: ## Check the formatting (no changes)
	$(RUN) cargo fmt --all -- --check

check: fmt-check lint ## Run fmt-check and lint (no changes)

ci: check test ## Run the same checks as CI (no changes)

## Install

# Replace the binary through a temporary file and a rename instead of copying over it. macOS
# caches the code signature check per inode, so a binary copied over one that is running (or ran
# a moment ago) is killed with SIGKILL right after it starts (exit 137). The temporary file sits
# in the same directory so that the rename swaps the inode.
# The skills are written by the binary that was just installed, so that the binary and the skills
# come from the same version.
install: release ## Install the release binary to INSTALL_PATH (default /usr/local/bin) and the agent skills (SKILL_TARGETS)
	@mkdir -p "$(INSTALL_PATH)"
	cp "target/release/$(BINARY_NAME)" "$(INSTALL_PATH)/$(BINARY_NAME).new"
	mv -f "$(INSTALL_PATH)/$(BINARY_NAME).new" "$(INSTALL_PATH)/$(BINARY_NAME)"
	@for target in $(SKILL_TARGETS); do \
		"$(INSTALL_PATH)/$(BINARY_NAME)" skill-install "$$target" || exit 1; \
	done

# Only the binary is removed. The skills are kept: each agent keeps them in its own place, and
# removing them could delete skills installed by another version.
uninstall: ## Remove the binary from INSTALL_PATH (the installed skills are kept)
	rm -f "$(INSTALL_PATH)/$(BINARY_NAME)"

clean: ## Remove build artifacts
	$(RUN) cargo clean

## Help

help: ## Show this help
	@echo "Development tasks for $(BINARY_NAME)"
	@echo ""
	@echo "Usage: make <target>"
	@echo ""
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Tool versions are pinned in mise.toml. Run make setup first."
	@echo "Without mise, add SYSTEM_TOOLS=1 to use the tools on PATH."
	@echo "Release: GitHub Actions > Release > Run workflow"
