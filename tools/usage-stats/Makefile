# usage-stats (astro-sight の採用率の集計) の開発用タスク。引数なしの `make` で一覧を表示する。
#
# ツールの版はリポジトリ直下の mise.toml が正 (mise は親ディレクトリの設定を読む)。
# mise を使わず PATH 上のツールで動かすなら SYSTEM_TOOLS=1 を付ける。
# macOS 標準の GNU Make 3.81 で動く書き方に限っている。

.DEFAULT_GOAL := help

BINARY_NAME := usage-stats
INSTALL_PATH ?= /usr/local/bin
# Cargo.lock をコミットしているので、依存の解決結果を固定する
CARGO_FLAGS ?= --locked

# ---- ツールチェーン -----------------------------------------------------------
# mise は PATH、よくある導入先の順に探す。make MISE=/path/to/mise で明示もできる。
MISE_CANDIDATES ?= $(HOME)/.local/bin/mise /opt/homebrew/bin/mise /usr/local/bin/mise
ifeq ($(SYSTEM_TOOLS),1)
RUN :=
else
ifndef MISE
MISE := $(firstword $(shell command -v mise 2>/dev/null) $(wildcard $(MISE_CANDIDATES)))
endif
ifeq ($(MISE),)
ifneq ($(filter-out help,$(or $(MAKECMDGOALS),help)),)
$(error mise が見つかりません。https://mise.jdx.dev で導入するか、PATH 上のツールで実行するなら SYSTEM_TOOLS=1 を付けてください)
endif
endif
RUN := $(if $(MISE),$(MISE) exec --,)
endif

.PHONY: build release install uninstall clean check help

## Build Commands

build: ## Build debug version
	$(RUN) cargo build $(CARGO_FLAGS)

release: ## Build release version
	$(RUN) cargo build --release $(CARGO_FLAGS)

## Installation

# 上書きコピーではなく一時ファイル + rename で置き換える。macOS はコード署名の検証結果を
# inode 単位でキャッシュするため、cp で上書きすると新しいバイナリが起動直後に SIGKILL
# される (exit 137)。一時ファイルは rename が inode の差し替えになるよう同じディレクトリに置く
install: release ## Build release and install the binary to INSTALL_PATH
	@mkdir -p "$(INSTALL_PATH)"
	cp "target/release/$(BINARY_NAME)" "$(INSTALL_PATH)/$(BINARY_NAME).new"
	mv -f "$(INSTALL_PATH)/$(BINARY_NAME).new" "$(INSTALL_PATH)/$(BINARY_NAME)"

uninstall: ## Remove the binary from INSTALL_PATH
	rm -f "$(INSTALL_PATH)/$(BINARY_NAME)"

## Development

check: ## Run clippy
	$(RUN) cargo clippy $(CARGO_FLAGS) --all-targets --all-features -- -D warnings

clean: ## Clean build artifacts
	$(RUN) cargo clean

## Help

help: ## Show this help message
	@echo "$(BINARY_NAME) Build Commands"
	@echo ""
	@echo "Usage: make [target]"
	@echo ""
	@echo "Targets:"
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Toolchain: versions are pinned in ../../mise.toml"
