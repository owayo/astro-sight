# astro-sight の開発用タスク。引数なしの `make` でターゲット一覧を表示する。
#
# ツールの版は mise.toml が正。mise があればコマンドを `mise exec --` 経由で呼ぶので、
# シェルで mise を activate していなくても (IDE や GUI から make を呼んでも)
# mise.toml の版で動く。mise を使わず PATH 上のツールで動かすなら SYSTEM_TOOLS=1 を
# 付ける (その場合、版の再現性は保証しない)。
#
# CI の quality ジョブは `make setup` と `make ci` だけを呼ぶ。検査を足すときは ci から
# たどれるターゲットに足す (workflow にコマンドを重ねて書かない)。
#
# macOS 標準の GNU Make 3.81 で動く書き方に限っている
# (.ONESHELL / .SHELLFLAGS / $(file ...) / != は使わない)。

.DEFAULT_GOAL := help

# Variables
BINARY_NAME := astro-sight
INSTALL_PATH ?= /usr/local/bin
# Cargo.lock をコミットしているので、依存の解決結果を CI とそろえる。
# .cargo/config.toml の [patch] でローカルの tree-sitter 系を差し込んでいて Cargo.lock が
# 手元でだけ変わるときは、make ci CARGO_FLAGS= のように外す
CARGO_FLAGS ?= --locked
# install の後にスキルを入れる AI エージェント。make install SKILL_TARGETS= で入れない
SKILL_TARGETS ?= claude codex

# macOS: cc crate と rustc のデプロイメントターゲットを揃える
# 未指定だと tree-sitter-swift の parser.o がホスト SDK (例: 26.5) でビルドされ、
# rustc の aarch64-apple-darwin デフォルト (11.0) と齟齬になり linker が警告を出す。
# CI の build ジョブと release.yml の build ジョブにも同じ値を書いている。
export MACOSX_DEPLOYMENT_TARGET ?= 11.0

# macOS: ar は Apple 純正 (/usr/bin/ar) を使う。以前は -D フラグ warning 回避のため
# GNU binutils の ar を export していたが、GNU ar 2.46 が生成する静的アーカイブを
# 新しい Apple ld (ld-1267 以降) が「member not 8-byte aligned」で拒否し、
# make 経由のリンクが全滅する。また AR は cc crate のビルド指紋に入るため、
# make (GNU ar) と素の cargo (Apple ar) で build script の再実行がピンポンする
# 副作用もあった。warning 回避より実害が大きいため override を撤去。

# ---- ツールチェーン -----------------------------------------------------------
# mise は PATH、よくある導入先の順に探す。GUI から起動した make はシェルの PATH を
# 引き継がないことがあるため。make MISE=/path/to/mise で明示もできる。
# mise が無い環境の振る舞いを試すときは MISE_CANDIDATES= で探す先を空にする。
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

.PHONY: help setup build release run test lint fmt fmt-check check ci install uninstall clean

## Setup

setup: ## Install the toolchain (mise.toml) and fetch dependencies
	@if [ -n "$(MISE)" ]; then "$(MISE)" install; fi
	$(RUN) cargo fetch $(CARGO_FLAGS)

## Build Commands

build: ## Build debug version
	$(RUN) cargo build $(CARGO_FLAGS)

release: ## Build release version
	$(RUN) cargo build --release $(CARGO_FLAGS)

run: ## Run the debug build (pass arguments with ARGS="...")
	$(RUN) cargo run $(CARGO_FLAGS) -- $(ARGS)

## Development

# テストは既定の feature で回す (配布物と同じ mimalloc の構成を検証するため)。
# --all-features にすると dhat-heap のアロケータで動き、各プロセスが dhat-heap.json を書き出す
test: ## Run tests
	$(RUN) cargo test $(CARGO_FLAGS)

# dhat-heap 側のコードも検査するため --all-features で回す。既定の feature の側
# (#[cfg(not(feature = "dhat-heap"))]) は check の cargo check と test がコンパイルする
lint: ## Run clippy with warnings as errors
	$(RUN) cargo clippy $(CARGO_FLAGS) --all-targets --all-features -- -D warnings

fmt: ## Format code
	$(RUN) cargo fmt

fmt-check: ## Check formatting (no rewrite)
	$(RUN) cargo fmt -- --check

check: fmt-check lint ## Run fmt check, clippy, and cargo check (no rewrite)
	$(RUN) cargo check $(CARGO_FLAGS)

ci: check test ## Run the same checks as the CI quality job (no rewrite)

## Installation

# 上書きコピーではなく一時ファイル + rename で置き換える。macOS はコード署名の
# 検証結果を inode 単位でキャッシュするため、実行中や直前に実行したバイナリへ cp で
# 上書きすると、新しいバイナリが起動直後に SIGKILL される (exit 137)。
# 一時ファイルは rename が inode の差し替えになるよう、同じディレクトリに置く。
# スキルは入れたばかりのバイナリで書き出す (バイナリとスキルの版をそろえるため)。
install: release ## Build release, install the binary to INSTALL_PATH, and install skills (claude + codex)
	@mkdir -p "$(INSTALL_PATH)"
	cp "target/release/$(BINARY_NAME)" "$(INSTALL_PATH)/$(BINARY_NAME).new"
	mv -f "$(INSTALL_PATH)/$(BINARY_NAME).new" "$(INSTALL_PATH)/$(BINARY_NAME)"
	@for target in $(SKILL_TARGETS); do \
		"$(INSTALL_PATH)/$(BINARY_NAME)" skill-install "$$target" || exit 1; \
	done

# バイナリだけを消し、スキルは消さない。スキルの置き場所はエージェントごとに違い、
# 別の版で入れたものまで消してしまうため
uninstall: ## Remove the binary from INSTALL_PATH (installed skills are kept)
	rm -f "$(INSTALL_PATH)/$(BINARY_NAME)"

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
	@echo "Toolchain:"
	@echo "  Versions are pinned in mise.toml. Run 'make setup' first."
	@echo "  Without mise, add SYSTEM_TOOLS=1 to use the tools on PATH."
	@echo ""
	@echo "Release:"
	@echo "  Use GitHub Actions > Release > Run workflow"
