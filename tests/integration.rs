//! astro-sight CLI の統合テスト。
//!
//! 実バイナリを起動して stdout / exit status / JSON スキーマを検証する。
//! テストはサブコマンドと対象言語で `tests/integration/` 配下に分割している。
//! (テストターゲットのルートファイルでは `mod x;` が `tests/x.rs` を指すため、
//!  `tests/integration/` を module ディレクトリにするインライン `mod integration` で包む)

mod integration {
    mod added_reference_summary;
    mod metrics;
    mod support;

    mod ast_symbols;
    mod bash_grammar;
    mod c_headers;
    mod change_evidence;
    mod cli_basics;
    mod cochange;
    mod constructor_optional_params;
    mod context;
    mod dead_code;
    mod dead_code_conventions;
    mod dead_code_languages;
    mod dead_code_locations;
    mod dead_code_parse_budget;
    mod dead_code_uncounted;
    mod deep_nesting;
    mod destructured_optional_defaults;
    mod fixed_callback_contract;
    mod function_value_handoff;
    mod git_invocations;
    mod git_non_ascii;
    mod hidden_and_angular_liveness;
    mod identifier_spellings;
    mod impact;
    mod impact_diff_input;
    mod impact_import_context;
    mod impact_jsx_props;
    mod impact_object_members;
    mod impact_output;
    mod impact_python_bindings;
    mod impact_rust_anonymous_const;
    mod impact_rust_cargo_reachability;
    mod impact_rust_line_endings;
    mod impact_rust_macro_arguments;
    mod impact_rust_private_helpers;
    mod impact_rust_reexport;
    mod impact_rust_replaced_types;
    mod impact_rust_same_named_constants;
    mod impact_rust_scope;
    mod impact_rust_visibility_widening;
    mod impact_ts_barrels;
    mod js_signature_sites;
    mod kotlin_grammar;
    mod languages;
    mod linguist_generated_attributes;
    mod literal_const_values;
    mod literal_union_removal;
    mod local_callable_shadow;
    mod mcp;
    mod moved_compatible_change;
    mod moved_member_signature;
    mod multiline_declaration_signature;
    mod nullable_input;
    mod optional_param_removal;
    mod optional_props_removal;
    mod output_format;
    mod php_member_liveness;
    mod property_removal_arrays;
    mod python_import_rebinding;
    mod python_member_removal;
    mod python_optional_removal;
    mod python_private_owner_removal;
    mod reference_evidence;
    mod refs;
    mod refs_declaration_budget;
    mod rename_candidates;
    mod required_type_members;
    mod review;
    mod review_dead_scope;
    mod review_retry;
    mod ruby_constructor_liveness;
    mod runtime_invoked_symbols;
    mod rust_cargo_removal;
    mod rust_const_fn;
    mod rust_format_captures;
    mod rust_import_path;
    mod rust_parameter_patterns;
    mod rust_private_inline_test;
    mod rust_reference_sites;
    mod rust_relative_reexport;
    mod rust_unicode_escapes;
    mod sandbox;
    mod session;
    mod signature_type_dependencies;
    mod static_const_values;
    mod swift_grammar;
    mod trailing_type_parameters;
    mod tree_sitter_generated;
    mod ts_extension_changes;
    mod ts_import_alias;
    mod ts_namespace_collisions;
    mod type_annotations;
    mod typescript_grammar;
}
