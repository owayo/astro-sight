//! astro-sight CLI の統合テスト。
//!
//! 実バイナリを起動して stdout / exit status / JSON スキーマを検証する。
//! テストはサブコマンドと対象言語で `tests/integration/` 配下に分割している。
//! (テストターゲットのルートファイルでは `mod x;` が `tests/x.rs` を指すため、
//!  `tests/integration/` を module ディレクトリにするインライン `mod integration` で包む)

mod integration {
    mod support;

    mod ast_symbols;
    mod cli_basics;
    mod cochange;
    mod context;
    mod dead_code;
    mod dead_code_conventions;
    mod dead_code_languages;
    mod git_non_ascii;
    mod hidden_and_angular_liveness;
    mod impact;
    mod impact_output;
    mod languages;
    mod mcp;
    mod php_member_liveness;
    mod refs;
    mod review;
    mod review_dead_scope;
    mod sandbox;
    mod session;
}
