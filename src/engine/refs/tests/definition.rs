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

/// JS/TS の object literal shorthand は「値の参照」、destructuring shorthand は「束縛」。
///
/// `{ handler }` (`shorthand_property_identifier`) は `handler` を読み出す参照なので
/// identifier として走査対象に入れる。これが無いと walk_refs の identifier ガードに
/// 弾かれ、レジストリ / DI / `module.exports = { a, b }` のような JS/TS で最も一般的な
/// 参照形が 1 件も数えられず、生きているシンボルが dead-code に出る。
///
/// 一方 `const { picked } = obj` (`shorthand_property_identifier_pattern`) は束縛。
/// 走査対象には入れるが、変数宣言の束縛位置だけを定義として出し、それ以外の文脈
/// (関数パラメータ・loop 変数・代入式) は `is_ignored_identifier_context` が除外する
/// (束縛を参照として数えると dead-code が fail-open する)。
#[test]
fn is_identifier_kind_accepts_shorthand_value_and_binding_pattern() {
    assert!(is_identifier_kind("shorthand_property_identifier"));
    assert!(is_identifier_kind("shorthand_property_identifier_pattern"));
}

/// shorthand の束縛パターンは変数宣言の束縛位置だけを残し、他の文脈は除外する。
#[test]
fn shorthand_binding_pattern_is_ignored_outside_variable_declarations() {
    let src = "const { kept } = obj;\n\
               function f({ param }) {}\n\
               for (const { loop } of xs) {}\n\
               ({ assigned } = obj);\n";
    let tree = crate::engine::parser::parse_source(src.as_bytes(), LangId::Typescript).unwrap();
    let mut ignored = Vec::new();
    let mut kept = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.kind() == "shorthand_property_identifier_pattern" {
            let name = node.utf8_text(src.as_bytes()).unwrap().to_string();
            if is_ignored_identifier_context(node, LangId::Typescript) {
                ignored.push(name);
            } else {
                kept.push(name);
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    ignored.sort();
    assert_eq!(kept, ["kept"]);
    assert_eq!(ignored, ["assigned", "loop", "param"]);
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

    let symbol_names = vec!["Base".to_string()];
    let counts = count_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        &symbol_names,
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

    let symbol_names = vec!["buffer_data".to_string()];
    let counts = count_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        &symbol_names,
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

/// 型注釈位置 (戻り値型・基底クラス・引数型) は Definition ではなく Reference。
///
/// Python と同型の誤分類が汎用パスに落ちる Go / Java / Kotlin / Swift / C# でも
/// 起きていた (`is_definition_context` の grandparent 走査が
/// `function_declaration > result: type_identifier` 等を def と判定していた)。
/// これにより基底クラス・戻り値型でしか使われない型が dead-code で dead と誤報され、
/// `api.add` の `refs_internal` も過小になっていた。
/// single refs と count-only (dead-code 経路) で分類が一致することも固定する。
#[test]
fn type_positions_are_refs_not_defs_in_every_language() {
    // (言語, ソース, 期待 def 数, 期待 ref 数)
    let cases: &[(LangId, &str, usize, usize)] = &[
        // 宣言 1 / 引数型・戻り値型・var 型・戻り式で 4 参照
        (
            LangId::Go,
            "package main\n\ntype Base struct{}\n\nfunc mk(param Base) Base {\n\tvar x Base\n\treturn Base{}\n}\n",
            1,
            4,
        ),
        // 宣言 1 / extends・戻り値型・引数型・new で 4 参照
        (
            LangId::Java,
            "class Base {}\nclass Impl extends Base {\n    Base make(Base param) { return new Base(); }\n}\n",
            1,
            4,
        ),
        // 宣言 1 / 継承・引数型・戻り値型・コンストラクタ呼び出しで 4 参照
        (
            LangId::Kotlin,
            "open class Base\nclass Impl : Base()\nfun make(param: Base): Base { return Base() }\n",
            1,
            4,
        ),
        // 宣言 1 / 継承・引数型・戻り値型・イニシャライザ呼び出しで 4 参照
        (
            LangId::Swift,
            "class Base {}\nclass Impl: Base {}\nfunc make(param: Base) -> Base { return Base() }\n",
            1,
            4,
        ),
        // 宣言 1 / 基底型・戻り値型・引数型・new で 4 参照
        (
            LangId::CSharp,
            "class Base { }\nclass Impl : Base {\n    Base Make(Base param) { return new Base(); }\n}\n",
            1,
            4,
        ),
    ];

    for (lang, source, want_def, want_ref) in cases {
        let tree = parser::parse_source(source.as_bytes(), *lang).expect("parse");
        let defs = definition_node_kinds(*lang);
        let refs = collect_single_refs_for_test(
            tree.root_node(),
            source.as_bytes(),
            "Base",
            "test.src",
            defs,
            *lang,
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
            def_cnt, *want_def,
            "{lang:?}: 型宣言だけが定義になること: {refs:?}"
        );
        assert_eq!(
            ref_cnt, *want_ref,
            "{lang:?}: 型注釈位置はすべて参照になること: {refs:?}"
        );

        let symbol_names = vec!["Base".to_string()];
        let counts = count_refs_for_test(
            tree.root_node(),
            source.as_bytes(),
            &symbol_names,
            defs,
            *lang,
            1,
        );
        assert_eq!(
            counts[0], *want_ref,
            "{lang:?}: count-only 経路も同じ分類になること"
        );
    }
}

/// 宣言名そのものは Definition のまま (name 一致判定が宣言名を落としていない対照)。
///
/// `name` フィールドを持たない Kotlin の各宣言と Go の `package_clause` は
/// 「最初の identifier 子」で名前位置を特定するため、ここが崩れると定義が
/// 1 件も見つからないという逆向きの誤りになる。
#[test]
fn declaration_names_stay_definitions_in_every_language() {
    // (言語, ソース, 探す名前, 期待 def 数, 期待 ref 数)
    let cases: &[(LangId, &str, &str, usize, usize)] = &[
        // package_clause は name フィールドを持たない
        (LangId::Go, "package main\n\nfunc mk() {}\n", "main", 1, 0),
        (
            LangId::Go,
            "package p\n\ntype Widget struct{}\n\nfunc build() Widget { return Widget{} }\n",
            "Widget",
            1,
            2,
        ),
        (
            LangId::Java,
            "class Widget {\n    void render() {}\n}\n",
            "render",
            1,
            0,
        ),
        // Kotlin の宣言はすべて name フィールドを持たない
        (
            LangId::Kotlin,
            "class Widget\nobject Single\n",
            "Widget",
            1,
            0,
        ),
        (
            LangId::Kotlin,
            "class Widget\nobject Single\n",
            "Single",
            1,
            0,
        ),
        (
            LangId::Kotlin,
            "fun build() {}\nfun run2() { build() }\n",
            "build",
            1,
            1,
        ),
        (
            LangId::Swift,
            "class Widget {}\nprotocol Proto {}\nfunc build() {}\n",
            "Widget",
            1,
            0,
        ),
        (
            LangId::Swift,
            "class Widget {}\nprotocol Proto {}\nfunc build() {}\n",
            "Proto",
            1,
            0,
        ),
        // Swift は関数名と戻り値型の双方が `name` フィールドに現れる。関数名側だけが
        // def になり、戻り値型は ref に落ちること (順序依存に頼らない判定の対照)。
        (
            LangId::Swift,
            "class Widget {}\nfunc build() -> Widget { return Widget() }\n",
            "build",
            1,
            0,
        ),
        (
            LangId::CSharp,
            "namespace App {\n    struct SVal { }\n    interface IFace { }\n    enum Color { Red }\n}\n",
            "SVal",
            1,
            0,
        ),
        (
            LangId::CSharp,
            "namespace App {\n    struct SVal { }\n    interface IFace { }\n}\n",
            "IFace",
            1,
            0,
        ),
    ];

    for (lang, source, name, want_def, want_ref) in cases {
        let tree = parser::parse_source(source.as_bytes(), *lang).expect("parse");
        let defs = definition_node_kinds(*lang);
        let refs = collect_single_refs_for_test(
            tree.root_node(),
            source.as_bytes(),
            name,
            "test.src",
            defs,
            *lang,
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
            def_cnt, *want_def,
            "{lang:?} {name}: 宣言名は定義: {refs:?}"
        );
        assert_eq!(ref_cnt, *want_ref, "{lang:?} {name}: 参照数: {refs:?}");
    }
}

/// Zig: 文レベルの代入 (`counter += 1;` / `counter = ..;` / `a, const b = ..;`) の代入先は
/// Reference。tree-sitter-zig は代入文も `variable_declaration` へ alias するため、旧実装
/// (「最初の identifier 子」を名前位置とみなす) では代入先が def に化け、書き込みでしか
/// 使われない `pub var` が dead-code に出ていた。
///
/// 対照: `var` / `pub var` / `threadlocal var` / 関数内 `const` / 分割代入中の `const b` の
/// 名前は Definition のまま、右辺の識別子は Reference のまま。`extern "c" var` の名前は
/// 旧実装では先頭の `"c"` (string) に名前位置を奪われて def にならなかったが、これも def。
/// `var` と名前の間に行コメントが挟まっても名前は Definition のまま。
#[test]
fn zig_assignment_target_is_reference_not_definition() {
    let source = "var counter: u32 = 0;\n\
pub var total: u32 = 0;\n\
threadlocal var tl: u32 = 0;\n\
extern \"c\" var ext: c_int;\n\
pub fn bump() void {\n\
    counter += 1;\n\
    counter = total + tl;\n\
    const local = counter;\n\
    var a: u32 = 0;\n\
    a, const b = .{ local, ext };\n\
    total = b;\n\
}\n\
pub var // note\n\
    noted: u32 = 0;\n\
pub fn touch() void {\n\
    noted = 1;\n\
}\n";
    let tree = parser::parse_source(source.as_bytes(), LangId::Zig).expect("parse");
    let defs = definition_node_kinds(LangId::Zig);

    // (名前, 定義行, 参照行)。行は 0-origin。
    let cases: &[(&str, &[usize], &[usize])] = &[
        ("counter", &[0], &[5, 6, 7]),
        ("total", &[1], &[6, 10]),
        ("tl", &[2], &[6]),
        ("ext", &[3], &[9]),
        ("local", &[7], &[9]),
        ("a", &[8], &[9]),
        ("b", &[9], &[10]),
        ("noted", &[13], &[15]),
    ];
    for (name, want_defs, want_refs) in cases {
        let refs = collect_single_refs_for_test(
            tree.root_node(),
            source.as_bytes(),
            name,
            "test.zig",
            defs,
            LangId::Zig,
        );
        let lines_of = |kind: RefKind| -> Vec<usize> {
            refs.iter()
                .filter(|r| r.kind == Some(kind))
                .map(|r| r.line)
                .collect()
        };
        assert_eq!(
            lines_of(RefKind::Definition),
            *want_defs,
            "{name}: 宣言名だけが定義: {refs:?}"
        );
        assert_eq!(
            lines_of(RefKind::Reference),
            *want_refs,
            "{name}: 代入先・右辺は参照: {refs:?}"
        );

        let counts = count_refs_for_test(
            tree.root_node(),
            source.as_bytes(),
            &[name.to_string()],
            defs,
            LangId::Zig,
            1,
        );
        assert_eq!(
            counts[0],
            want_refs.len(),
            "{name}: count-only 経路も同じ分類になること"
        );
    }
}
