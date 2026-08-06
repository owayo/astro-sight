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

# macOS: ar は Apple 純正 (/usr/bin/ar) を使う。以前は -D フラグ warning 回避のため
# GNU binutils の ar を export していたが、GNU ar 2.46 が生成する静的アーカイブを
# 新しい Apple ld (ld-1267 以降) が「member not 8-byte aligned」で拒否し、
# make 経由のリンクが全滅する。また AR は cc crate のビルド指紋に入るため、
# make (GNU ar) と素の cargo (Apple ar) で build script の再実行がピンポンする
# 副作用もあった。warning 回避より実害が大きいため override を撤去。

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
