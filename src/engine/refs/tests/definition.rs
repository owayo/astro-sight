use super::*;

/// 既知の identifier ノード種別が true を返すことを検証
#[test]
fn is_identifier_kind_matches() {
    assert!(is_identifier_kind("identifier"));
    assert!(is_identifier_kind("type_identifier"));
    assert!(is_identifier_kind("field_identifier"));
    assert!(is_identifier_kind("property_identifier"));
    assert!(is_identifier_kind("constant"));
    assert!(is_identifier_kind("name"));
    assert!(is_identifier_kind("word"));
}

/// 非 identifier ノード種別が false を返すことを検証
#[test]
fn is_identifier_kind_rejects_non_identifier() {
    assert!(!is_identifier_kind("function_definition"));
    assert!(!is_identifier_kind("block"));
    assert!(!is_identifier_kind("string"));
    assert!(!is_identifier_kind("comment"));
}

/// Rust の定義ノード種別に function_item と struct_item が含まれることを検証
#[test]
fn definition_node_kinds_rust() {
    let kinds = definition_node_kinds(LangId::Rust);
    assert!(kinds.contains(&"function_item"));
    assert!(kinds.contains(&"struct_item"));
    assert!(kinds.contains(&"enum_item"));
    assert!(kinds.contains(&"trait_item"));
}

/// Python の定義ノード種別に function_definition と class_definition が含まれることを検証
#[test]
fn definition_node_kinds_python() {
    let kinds = definition_node_kinds(LangId::Python);
    assert!(kinds.contains(&"function_definition"));
    assert!(kinds.contains(&"class_definition"));
}

/// C/C++ の tag 名は、本体付き `struct X {}` だけを Definition とし、
/// `struct X *p` / `sizeof(struct X)` / 引数型などの型使用は Reference として扱う。
/// 単独 forward declaration は ref/def いずれにも含めない。
#[test]
fn cpp_struct_tag_type_uses_are_refs_not_defs() {
    use std::borrow::Cow;
    use std::collections::HashMap;

    let source = "struct buffer_data { int x; };\n\
struct holder {\n\
  struct buffer_data* buffer;\n\
};\n\
void f(struct buffer_data header) {\n\
  struct buffer_data local;\n\
  (void)sizeof(struct buffer_data);\n\
}\n\
struct forward_declared;\n";
    let tree = parser::parse_source(source.as_bytes(), LangId::Cpp).expect("parse");
    let defs = definition_node_kinds(LangId::Cpp);
    let refs = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "buffer_data",
        "test.cpp",
        defs,
        LangId::Cpp,
    );

    let def_cnt = refs
        .iter()
        .filter(|r| matches!(r.kind, Some(RefKind::Definition)))
        .count();
    let ref_cnt = refs
        .iter()
        .filter(|r| matches!(r.kind, Some(RefKind::Reference)))
        .count();
    assert_eq!(
        def_cnt, 1,
        "tag body definition should be captured once: {refs:?}"
    );
    assert_eq!(
        ref_cnt, 4,
        "member type, parameter type, local type, and sizeof type should be refs: {refs:?}"
    );

    let mut name_to_ix: HashMap<Cow<'_, str>, Vec<usize>> = HashMap::new();
    name_to_ix.insert(Cow::Borrowed("buffer_data"), vec![0]);
    let counts = count_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        &name_to_ix,
        defs,
        LangId::Cpp,
        1,
    );
    assert_eq!(counts[0], 4, "count-only refs should match visible refs");

    let refs = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "forward_declared",
        "test.cpp",
        defs,
        LangId::Cpp,
    );
    assert!(
        refs.is_empty(),
        "standalone forward declaration is neither def nor ref: {refs:?}"
    );
}
