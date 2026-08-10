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

/// Python の型注釈位置 (戻り値型・基底クラス・引数型) は Reference として数える。
///
/// 汎用の parent/grandparent 走査では `function_definition > type > identifier` と
/// `class_definition > argument_list > identifier` が祖父ノード経由で Definition と
/// 誤判定され、dead-code で基底クラスや戻り値型専用のクラスが dead と報告されていた。
/// single refs と count-only (dead-code 経路) で分類が一致することも固定する。
#[test]
fn python_type_positions_are_refs_not_defs() {
    use std::borrow::Cow;
    use std::collections::HashMap;

    let source = "class Base:\n\
    pass\n\
\n\
class Derived(Base):\n\
    pass\n\
\n\
def f() -> Base:\n\
    return None\n\
\n\
def g(p: Base) -> None:\n\
    return None\n\
\n\
def h() -> list[Base]:\n\
    return []\n";
    let tree = parser::parse_source(source.as_bytes(), LangId::Python).expect("parse");
    let defs = definition_node_kinds(LangId::Python);
    let refs = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "Base",
        "test.py",
        defs,
        LangId::Python,
    );

    let def_cnt = refs
        .iter()
        .filter(|r| matches!(r.kind, Some(RefKind::Definition)))
        .count();
    let ref_cnt = refs
        .iter()
        .filter(|r| matches!(r.kind, Some(RefKind::Reference)))
        .count();
    assert_eq!(def_cnt, 1, "class 宣言だけが定義: {refs:?}");
    assert_eq!(
        ref_cnt, 4,
        "基底クラス / 戻り値型 / 引数型 / ジェネリック引数はいずれも参照: {refs:?}"
    );

    let mut name_to_ix: HashMap<Cow<'_, str>, Vec<usize>> = HashMap::new();
    name_to_ix.insert(Cow::Borrowed("Base"), vec![0]);
    let counts = count_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        &name_to_ix,
        defs,
        LangId::Python,
        1,
    );
    assert_eq!(counts[0], 4, "count-only 経路も同じ分類になること");
}

/// `def f(x)` の bare パラメータ名は従来どおり Definition のまま
/// (型注釈位置の修正がパラメータ宣言まで巻き込んでいないことの対照)。
#[test]
fn python_bare_parameter_name_stays_definition() {
    let source = "def k(x):\n    return x\n";
    let tree = parser::parse_source(source.as_bytes(), LangId::Python).expect("parse");
    let defs = definition_node_kinds(LangId::Python);
    let refs = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "x",
        "test.py",
        defs,
        LangId::Python,
    );

    let def_cnt = refs
        .iter()
        .filter(|r| matches!(r.kind, Some(RefKind::Definition)))
        .count();
    let ref_cnt = refs
        .iter()
        .filter(|r| matches!(r.kind, Some(RefKind::Reference)))
        .count();
    assert_eq!(def_cnt, 1, "パラメータ宣言は定義: {refs:?}");
    assert_eq!(ref_cnt, 1, "本体での使用は参照: {refs:?}");
}

/// 関数名 / クラス名そのものは Definition のまま
/// (name フィールド一致の判定が宣言名を落としていないことの対照)。
#[test]
fn python_declaration_names_stay_definitions() {
    let source =
        "class Widget:\n    def render(self):\n        pass\n\ndef build():\n    return Widget()\n";
    let tree = parser::parse_source(source.as_bytes(), LangId::Python).expect("parse");
    let defs = definition_node_kinds(LangId::Python);

    for (name, want_def, want_ref) in [("Widget", 1, 1), ("build", 1, 0), ("render", 1, 0)] {
        let refs = collect_single_refs_for_test(
            tree.root_node(),
            source.as_bytes(),
            name,
            "test.py",
            defs,
            LangId::Python,
        );
        let def_cnt = refs
            .iter()
            .filter(|r| matches!(r.kind, Some(RefKind::Definition)))
            .count();
        let ref_cnt = refs
            .iter()
            .filter(|r| matches!(r.kind, Some(RefKind::Reference)))
            .count();
        assert_eq!(def_cnt, want_def, "{name} の定義数: {refs:?}");
        assert_eq!(ref_cnt, want_ref, "{name} の参照数: {refs:?}");
    }
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
