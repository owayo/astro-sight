//! `detect_api_changes` のテスト。対象言語と判定カテゴリで分割している。

mod compatibility_params;
mod compatibility_react;
mod declaration_coverage;
mod general;
mod moved_compatible_change;
mod moved_member_signature;
mod other_languages;
mod parse_errors;
mod python;
mod reference_positions;
mod removal;
mod rename_move;
mod required_type_members;
mod rust_const_fn;
mod rust_crate_kind;
mod rust_module_visibility;
mod rust_parameter_binding;
mod rust_reexport;
mod rust_reference_sites;
mod rust_type_alias;
mod typescript;
