.PHONY: build release install clean test fmt check help

# Default target
.DEFAULT_GOAL := help

# Variables
BINARY_NAME := astro-sight
INSTALL_PATH := /usr/local/bin

# macOS: cc crate と rustc のデプロイメントターゲットを揃える
# 未指定だと tree-sitter-swift の parser.o がホスト SDK (例: 26.5) でビルドされ、
# rustc の aarch64-apple-darwin デフォルト (11.0) と齟齬になり linker が警告を出す。
export MACOSX_DEPLOYMENT_TARGET ?= 11.0

# macOS: GNU ar を使用（Xcode の ar は -D フラグ非対応で warning が出る）
AR_GNU := $(wildcard /opt/homebrew/opt/binutils/bin/ar)
ifdef AR_GNU
export AR := $(AR_GNU)
endif

## Build Commands

build: ## Build debug version
	cargo build

release: ## Build release version
	cargo build --release

## Installation

install: release ## Build release, install binary, and install skills (claude + codex)
	cp target/release/$(BINARY_NAME) $(INSTALL_PATH)/
	$(INSTALL_PATH)/$(BINARY_NAME) skill-install claude
	$(INSTALL_PATH)/$(BINARY_NAME) skill-install codex

## Development

test: ## Run tests
	cargo test

fmt: ## Format code
	cargo fmt

check: ## Run clippy, check, and fmt check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo check
	cargo fmt -- --check

clean: ## Clean build artifacts
	cargo clean

## Help

help: ## Show this help message
	@echo "$(BINARY_NAME) Build Commands"
	@echo ""
	@echo "Usage: make [target]"
	@echo ""
	@echo "Targets:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Release:"
	@echo "  Use GitHub Actions > Release > Run workflow"
