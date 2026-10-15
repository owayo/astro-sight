//! astro-sight CLI の統合テスト。
//!
//! 実バイナリを起動して stdout / exit status / JSON スキーマを検証する。
//! テストはサブコマンドと対象言語で `tests/integration/` 配下に分割している。
//! (テストターゲットのルートファイルでは `mod x;` が `tests/x.rs` を指すため、
//!  `tests/integration/` を module ディレクトリにするインライン `mod integration` で包む)

mod integration {
    mod support;

    mod ast_symbols;
    mod bash_grammar;
    mod cli_basics;
    mod cochange;
    mod context;
    mod dead_code;
    mod dead_code_conventions;
    mod dead_code_languages;
    mod dead_code_locations;
    mod dead_code_uncounted;
    mod deep_nesting;
    mod git_non_ascii;
    mod hidden_and_angular_liveness;
    mod identifier_spellings;
    mod impact;
    mod impact_diff_input;
    mod impact_jsx_props;
    mod impact_object_members;
    mod impact_output;
    mod impact_rust_reexport;
    mod impact_rust_scope;
    mod impact_ts_barrels;
    mod js_signature_sites;
    mod languages;
    mod mcp;
    mod optional_param_removal;
    mod output_format;
    mod php_member_liveness;
    mod python_member_removal;
    mod python_private_owner_removal;
    mod refs;
    mod review;
    mod review_dead_scope;
    mod review_retry;
    mod runtime_invoked_symbols;
    mod sandbox;
    mod session;
    mod static_const_values;
    mod swift_grammar;
    mod type_annotations;
    mod typescript_grammar;
}
