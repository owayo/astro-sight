use super::*;

fn check_exported(source: &str, lang_id: LangId, symbol_name: &str) -> bool {
    let language = lang_id.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, source.as_bytes(), lang_id).unwrap();
    let sym = syms
        .iter()
        .find(|s| s.name == symbol_name)
        .unwrap_or_else(|| {
            panic!(
                "symbol '{}' not found in {:?}",
                symbol_name,
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    is_symbol_exported(root, source.as_bytes(), lang_id, &sym.range)
}

#[test]
fn ts_export_function_is_exported() {
    assert!(check_exported(
        "export function foo() {}",
        LangId::Typescript,
        "foo"
    ));
}

#[test]
fn ts_non_export_function_is_not_exported() {
    assert!(!check_exported(
        "function foo() {}",
        LangId::Typescript,
        "foo"
    ));
}

#[test]
fn ts_named_export_is_exported() {
    assert!(check_exported(
        "function foo() {}\nexport { foo }",
        LangId::Typescript,
        "foo"
    ));
}

/// テスト用: ソースからシンボルを抽出する。
fn syms_of(source: &str, lang_id: LangId) -> Vec<Symbol> {
    let language = lang_id.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    extract_symbols(tree.root_node(), source.as_bytes(), lang_id).unwrap()
}

/// テスト用: 指定シンボルの循環的複雑度を取得する。
fn cx_of(source: &str, lang_id: LangId, name: &str) -> usize {
    let syms = syms_of(source, lang_id);
    syms.iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| {
            panic!(
                "symbol '{name}' not found in {:?}",
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        })
        .complexity
        .unwrap_or_else(|| panic!("symbol '{name}' has no complexity"))
}

// --- C/C++: ポインタ返り関数・クラスメソッド抽出 (回帰) ---

#[test]
fn c_pointer_returning_function_is_extracted() {
    // `Type *foo()` は declarator が pointer_declarator に包まれ、旧クエリ
    // (function_definition > function_declarator 直結) ではマッチしなかった。
    let syms = syms_of("int *make(int n) { return 0; }", LangId::C);
    assert!(
        syms.iter()
            .any(|s| s.name == "make" && matches!(s.kind, SymbolKind::Function)),
        "ポインタ返り関数 make が抽出される: {:?}",
        syms.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[test]
fn c_function_prototype_is_not_extracted() {
    // 本体のない宣言 (プロトタイプ) はシンボルにしない。
    let syms = syms_of("int foo(void);\nint foo(void) { return 0; }", LangId::C);
    let count = syms.iter().filter(|s| s.name == "foo").count();
    assert_eq!(
        count,
        1,
        "定義のみ採用しプロトタイプは除外: {:?}",
        syms.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[test]
fn cpp_class_method_is_extracted_with_container() {
    let src = "class P {\npublic:\n  void add(int x) {}\n  int get() const { return 0; }\n};";
    let syms = syms_of(src, LangId::Cpp);
    let add = syms
        .iter()
        .find(|s| s.name == "add")
        .expect("メソッド add が抽出される");
    assert!(matches!(add.kind, SymbolKind::Method));
    assert_eq!(add.container.as_deref(), Some("P"));
    assert!(syms.iter().any(|s| s.name == "get"), "get も抽出される");
}

#[test]
fn cpp_reference_returning_method_is_extracted() {
    let src = "class P {\npublic:\n  const int& at(int i) const { static int z=0; return z; }\n};";
    let syms = syms_of(src, LangId::Cpp);
    assert!(
        syms.iter().any(|s| s.name == "at"),
        "参照返りメソッド at が抽出される: {:?}",
        syms.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

// --- C++: メソッド可視性 (is_exported_cpp) ---

#[test]
fn cpp_public_method_is_exported() {
    assert!(check_exported(
        "class P {\npublic:\n  void pub_m() {}\n};",
        LangId::Cpp,
        "pub_m"
    ));
}

#[test]
fn cpp_private_method_is_not_exported() {
    // class のデフォルトは private。private: 配下も非公開。
    let src = "class P {\n  void hidden() {}\nprivate:\n  void also() {}\n};";
    assert!(
        !check_exported(src, LangId::Cpp, "hidden"),
        "デフォルト private"
    );
    assert!(!check_exported(src, LangId::Cpp, "also"), "private: 配下");
}

#[test]
fn cpp_protected_method_is_exported() {
    assert!(
        check_exported(
            "class P {\nprotected:\n  void prot_m() {}\n};",
            LangId::Cpp,
            "prot_m"
        ),
        "protected は継承 API として公開扱い"
    );
}

#[test]
fn cpp_struct_method_default_is_exported() {
    // struct のデフォルトは public。
    assert!(check_exported(
        "struct S {\n  void m() {}\n};",
        LangId::Cpp,
        "m"
    ));
}

#[test]
fn cpp_free_function_is_exported() {
    assert!(check_exported(
        "int freefn() { return 0; }",
        LangId::Cpp,
        "freefn"
    ));
}

#[test]
fn ruby_simple_case_fold_unicode_identifiers_are_extracted() {
    let symbols = syms_of("def claſs\n  1\nend\n\ndef breaK\n  2\nend\n", LangId::Ruby);
    let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();

    assert!(names.contains(&"claſs"));
    assert!(names.contains(&"breaK"));
}

// --- Ruby: 循環的複雑度の二重計上回帰 ---

#[test]
fn ruby_if_else_complexity_no_double_count() {
    // tree-sitter-ruby は `if` 文ノードとキーワードトークン `if` が同名 kind。
    // named ガードが無いと二重計上され cx=3。正しくは 2 (ベース1 + if 1)。
    assert_eq!(
        cx_of("def f(x)\n  if x then 1 else 2 end\nend", LangId::Ruby, "f"),
        2,
        "if/else は分岐 1 つ"
    );
}

#[test]
fn ruby_case_when_complexity_no_double_count() {
    // when×2 = 判定点 2 → cx 3。`case` 本体は判定点ではないので数えない
    // (else も暗黙経路として数えない)。named ガードが無いと二重計上で 7 になる。
    let src = "def f(x)\n  case x\n  when 1 then 1\n  when 2 then 2\n  else 3\n  end\nend";
    assert_eq!(cx_of(src, LangId::Ruby, "f"), 3);
}

#[test]
fn ruby_while_complexity_no_double_count() {
    assert_eq!(
        cx_of(
            "def f(x)\n  while x > 0\n    x -= 1\n  end\nend",
            LangId::Ruby,
            "f"
        ),
        2,
        "while は分岐 1 つ"
    );
}

/// ネスト関数 / クロージャ内の分岐は外側関数の cx に混入しない。
///
/// `function_boundary_kinds` に言語アームが無い (`_ => &[]` に落ちる) か、
/// ノード名が grammar の改称に追随していないと外側の cx が膨らむ。
/// 実測で C++ ラムダ / PHP closure / Ruby `do...end` / bash ネスト関数 /
/// Zig ネスト fn の 5 言語が誤っていた回帰テスト。
#[test]
fn nested_function_branches_excluded_from_outer_complexity() {
    // C++: ラムダ本体の分岐 2 つは outer に入らない
    assert_eq!(
        cx_of(
            "int outer() {\n    auto f = [](int x) {\n        if (x > 0) { return 1; }\n        if (x < 0) { return 2; }\n        return 0;\n    };\n    return f(1);\n}\n",
            LangId::Cpp,
            "outer"
        ),
        1,
        "C++ ラムダ内の分岐は outer の cx に含めない"
    );
    // PHP: tree-sitter-php 0.24 の `anonymous_function` (旧 `..._creation_expression`)
    assert_eq!(
        cx_of(
            "<?php\nfunction outer() {\n    $f = function ($x) {\n        if ($x > 0) { return 1; }\n        if ($x < 0) { return 2; }\n        return 0;\n    };\n    return 42;\n}\n",
            LangId::Php,
            "outer"
        ),
        1,
        "PHP closure 内の分岐は outer の cx に含めない"
    );
    // Ruby: `do ... end` は `block` ではなく `do_block`
    assert_eq!(
        cx_of(
            "def outer\n  [1, 2].each do |x|\n    if x > 0\n      puts x\n    end\n  end\n  42\nend\n",
            LangId::Ruby,
            "outer"
        ),
        1,
        "Ruby do...end ブロック内の分岐は outer の cx に含めない"
    );
    // bash: 関数内に関数を定義できる
    assert_eq!(
        cx_of(
            "outer() {\n  inner() {\n    if [ \"$1\" -gt 0 ]; then echo pos; fi\n    if [ \"$1\" -lt 0 ]; then echo neg; fi\n  }\n  echo done\n}\n",
            LangId::Bash,
            "outer"
        ),
        1,
        "bash ネスト関数内の分岐は outer の cx に含めない"
    );
    // Zig: struct 内のネスト fn
    assert_eq!(
        cx_of(
            "pub fn outer() u32 {\n    const f = struct {\n        fn call(x: u32) u32 {\n            if (x > 0) return 1;\n            if (x < 2) return 2;\n            return 0;\n        }\n    }.call;\n    return f(1);\n}\n",
            LangId::Zig,
            "outer"
        ),
        1,
        "Zig ネスト fn 内の分岐は outer の cx に含めない"
    );
}

/// ネスト側 (内側) の関数自身の cx は従来どおり自分の分岐を数える。
#[test]
fn nested_function_keeps_own_complexity() {
    assert_eq!(
        cx_of(
            "outer() {\n  inner() {\n    if [ \"$1\" -gt 0 ]; then echo pos; fi\n    if [ \"$1\" -lt 0 ]; then echo neg; fi\n  }\n  echo done\n}\n",
            LangId::Bash,
            "inner"
        ),
        3,
        "inner 自身は分岐 2 つ + ベース 1"
    );
}

// --- Kotlin: 循環的複雑度 (旧実装は Java の分岐ノード名を共用しており常に 1 だった) ---

#[test]
fn kotlin_when_expression_complexity_counts_entries() {
    // `when` は tree-sitter-kotlin で `when_expression`、ブランチは `when_entry`。
    // 旧実装は Java の `switch_expression` を流用しており when を取りこぼし cx=1 だった。
    let src = "fun classify(x: Int): String {\n  return when (x) {\n    0 -> \"zero\"\n    1 -> \"one\"\n    2 -> \"two\"\n    else -> \"other\"\n  }\n}";
    // ベース 1 + when_entry 4 (0/1/2/else) = 5。`when_expression` 本体は
    // 判定点ではないので数えない (他言語の switch と揃える)。
    assert_eq!(cx_of(src, LangId::Kotlin, "classify"), 5);
}

#[test]
fn kotlin_if_else_complexity_is_counted() {
    // `if` は tree-sitter-kotlin で `if_expression`。旧実装では cx=1 のままだった。
    let src = "fun pickIf(a: Int, b: Int): Int {\n  if (a > 0) {\n    if (b > 0) {\n      return a + b\n    } else {\n      return a - b\n    }\n  }\n  return 0\n}";
    // ベース 1 + if_expression 2 (外側 + 内側) = 3
    assert_eq!(cx_of(src, LangId::Kotlin, "pickIf"), 3);
}

#[test]
fn kotlin_loop_complexity_is_counted() {
    let src = "fun loopSum(items: List<Int>): Int {\n  var sum = 0\n  for (it in items) {\n    while (sum < 100) {\n      sum += it\n    }\n  }\n  return sum\n}";
    // ベース 1 + for_statement 1 + while_statement 1 = 3
    assert_eq!(cx_of(src, LangId::Kotlin, "loopSum"), 3);
}

#[test]
fn kotlin_try_catch_complexity_is_counted() {
    // `catch` は tree-sitter-kotlin で `catch_block`。
    let src = "fun tryIt(): Int {\n  try {\n    return 1\n  } catch (e: Exception) {\n    return -1\n  }\n}";
    // ベース 1 + catch_block 1 = 2
    assert_eq!(cx_of(src, LangId::Kotlin, "tryIt"), 2);
}

/// named export surface 名抽出テスト用ヘルパー。
fn collect_export_surface(source: &str, lang_id: LangId) -> std::collections::HashSet<String> {
    let language = lang_id.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    super::collect_js_ts_named_export_surface_names(tree.root_node(), source.as_bytes())
}

#[test]
fn ts_reexport_from_clause_is_not_local_export() {
    // `export { foo } from "./b"` は re-export (別モジュールの forwarding) であり、
    // ローカル定義 foo があっても from 句付き export はローカル export 判定に使わない。
    assert!(!check_exported(
        "function foo() {}\nexport { foo } from \"./b\"",
        LangId::Typescript,
        "foo"
    ));
}

#[test]
fn collect_export_surface_named_and_alias() {
    let names = collect_export_surface(
        "export { foo, bar as baz } from \"./b\";",
        LangId::Typescript,
    );
    assert!(names.contains("foo"), "foo: {names:?}");
    assert!(names.contains("baz"), "alias 後の baz: {names:?}");
    assert!(
        !names.contains("bar"),
        "alias 前の bar は含まない: {names:?}"
    );
}

#[test]
fn collect_export_surface_includes_from_less_export_clause() {
    // from 句のない `export { foo }` も公開 export 名として収集する。
    // ローカル定義付きなら foo は exported シンボルとして別途抽出されるため
    // api.rm 候補に上がらず、収集しても無害 (doc 参照)。
    let names = collect_export_surface("function foo() {}\nexport { foo };", LangId::Typescript);
    assert!(
        names.contains("foo"),
        "from 句なし export clause も公開面: {names:?}"
    );
}

#[test]
fn collect_export_surface_from_less_reexport_of_import() {
    // Issue 2026-06-30-teamspirit-message-map-api-triage の再現:
    // `import type { X } from "..."; export type { X };` は from 句付き
    // `export { X } from "..."` と等価な公開面の維持であり、収集対象。
    let names = collect_export_surface(
        "import type { MessageName } from \"./messages\";\nexport type { MessageName };",
        LangId::Typescript,
    );
    assert!(
        names.contains("MessageName"),
        "from 句なし re-export も公開面: {names:?}"
    );
}

#[test]
fn collect_export_surface_from_less_alias_uses_public_name() {
    // `export { Local as Public };` の公開名は Public (Local は含まない)。
    let names = collect_export_surface(
        "import { Local } from \"./lib\";\nexport { Local as Public };",
        LangId::Typescript,
    );
    assert!(names.contains("Public"), "alias 後の公開名: {names:?}");
    assert!(
        !names.contains("Local"),
        "alias 前のローカル名は含まない: {names:?}"
    );
}

#[test]
fn collect_export_surface_js_from_less_export_clause() {
    // JS でも from 句なし export clause を収集する (grammar は TS と同系)。
    let names = collect_export_surface(
        "import { helper } from \"./lib\";\nexport { helper };",
        LangId::Javascript,
    );
    assert!(names.contains("helper"), "JS の from 句なし: {names:?}");
}

/// Rust の `pub use` 再エクスポート抽出ヘルパー。
fn collect_rust_reexports(source: &str) -> std::collections::HashSet<String> {
    let language = LangId::Rust.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    super::collect_rust_reexported_names(tree.root_node(), source.as_bytes())
}

#[test]
fn collect_rust_reexports_single_scoped_identifier() {
    // `pub use sub::name;` → name が公開される
    let names = collect_rust_reexports("pub use sub::name;");
    assert!(names.contains("name"), "got: {names:?}");
    assert_eq!(names.len(), 1, "他の名前は含まない: {names:?}");
}

#[test]
fn collect_rust_reexports_group_use_list() {
    // `pub use sub::{A, B};` → A, B が公開される
    let names = collect_rust_reexports("pub use sub::{A, B};");
    assert!(names.contains("A"), "got: {names:?}");
    assert!(names.contains("B"), "got: {names:?}");
    assert_eq!(names.len(), 2, "他の名前は含まない: {names:?}");
}

#[test]
fn collect_rust_reexports_use_as_alias() {
    // `pub use sub::name as alias;` → alias が公開される (name は含まれない)
    let names = collect_rust_reexports("pub use sub::name as alias;");
    assert!(names.contains("alias"), "alias を公開: {names:?}");
    assert!(!names.contains("name"), "元の name は対象外: {names:?}");
}

#[test]
fn collect_rust_reexports_group_with_alias() {
    // `pub use sub::{A, B as C};` → A, C が公開される
    let names = collect_rust_reexports("pub use sub::{A, B as C};");
    assert!(names.contains("A"), "got: {names:?}");
    assert!(names.contains("C"), "alias 後の C: {names:?}");
    assert!(!names.contains("B"), "alias 前の B は対象外: {names:?}");
}

#[test]
fn collect_rust_reexports_wildcard_is_ignored() {
    // `pub use sub::*;` は名前が静的に解決できないため対象外 (fail-open 回避)
    let names = collect_rust_reexports("pub use sub::*;");
    assert!(names.is_empty(), "wildcard は対象外: {names:?}");
}

#[test]
fn collect_rust_reexports_pub_crate_is_not_public() {
    // `pub(crate) use` は内部公開で外部利用者から見えないため対象外
    let names = collect_rust_reexports("pub(crate) use sub::name;");
    assert!(names.is_empty(), "pub(crate) は対象外: {names:?}");
}

#[test]
fn collect_rust_reexports_private_use_is_ignored() {
    // `use sub::name;` (private) は import であり re-export ではない
    let names = collect_rust_reexports("use sub::name;");
    assert!(names.is_empty(), "private use は対象外: {names:?}");
}

#[test]
fn collect_rust_reexports_multiple_declarations() {
    // 複数の pub use 宣言をまとめて収集する
    let source = "mod common;\n\
                      pub use common::{MAX_INPUT_SIZE, classify_error};\n\
                      pub use common::serialize_output;\n\
                      pub(crate) use common::internal;\n";
    let names = collect_rust_reexports(source);
    assert!(names.contains("MAX_INPUT_SIZE"), "got: {names:?}");
    assert!(names.contains("classify_error"), "got: {names:?}");
    assert!(names.contains("serialize_output"), "got: {names:?}");
    assert!(
        !names.contains("internal"),
        "pub(crate) は対象外: {names:?}"
    );
    assert_eq!(names.len(), 3, "got: {names:?}");
}

#[test]
fn rust_pub_fn_is_exported() {
    assert!(check_exported("pub fn foo() {}", LangId::Rust, "foo"));
}

#[test]
fn rust_private_fn_is_not_exported() {
    assert!(!check_exported("fn foo() {}", LangId::Rust, "foo"));
}

#[test]
fn swift_public_struct_is_exported() {
    assert!(check_exported(
        "public struct Detector {}",
        LangId::Swift,
        "Detector"
    ));
}

#[test]
fn swift_internal_enum_is_not_exported() {
    assert!(!check_exported(
        "enum DetectionError { case failed }",
        LangId::Swift,
        "DetectionError"
    ));
}

#[test]
fn swift_open_class_is_exported() {
    assert!(check_exported(
        "open class Service {}",
        LangId::Swift,
        "Service"
    ));
}

#[test]
fn swift_private_func_is_not_exported() {
    assert!(!check_exported(
        "private func helper() {}",
        LangId::Swift,
        "helper"
    ));
}

#[test]
fn swift_internal_method_in_public_struct_is_not_exported() {
    assert!(!check_exported(
        "public struct S {\n    func internalMethod() {}\n}\n",
        LangId::Swift,
        "internalMethod"
    ));
}

#[test]
fn swift_public_method_is_exported() {
    assert!(check_exported(
        "public struct S {\n    public func run() {}\n}\n",
        LangId::Swift,
        "run"
    ));
}

#[test]
fn swift_public_extension_method_is_exported() {
    // public extension のメンバは明示修飾子なしでもデフォルト public
    assert!(check_exported(
        "public extension Foo {\n    func bar() -> Int { 0 }\n}\n",
        LangId::Swift,
        "bar"
    ));
}

#[test]
fn swift_public_protocol_requirement_is_exported() {
    // public protocol の requirement (func handle) は外部公開 API なので exported。
    assert!(check_exported(
        "public protocol Service {\n    func handle() -> Int\n}\n",
        LangId::Swift,
        "handle"
    ));
}

#[test]
fn swift_internal_protocol_requirement_is_not_exported() {
    // internal (修飾子なし) protocol の requirement は外部公開 API ではない。
    assert!(!check_exported(
        "protocol Service {\n    func handle() -> Int\n}\n",
        LangId::Swift,
        "handle"
    ));
}

#[test]
fn swift_plain_extension_method_is_not_exported() {
    // 修飾子なし extension のメンバは internal (外部公開 API ではない)
    assert!(!check_exported(
        "extension Foo {\n    func bar() -> Int { 0 }\n}\n",
        LangId::Swift,
        "bar"
    ));
}

#[test]
fn go_uppercase_is_exported() {
    assert!(check_exported(
        "package main\nfunc Foo() {}",
        LangId::Go,
        "Foo"
    ));
}

#[test]
fn go_lowercase_is_not_exported() {
    assert!(!check_exported(
        "package main\nfunc foo() {}",
        LangId::Go,
        "foo"
    ));
}

#[test]
fn ts_local_var_inside_exported_fn_is_not_exported() {
    assert!(!check_exported(
        "export function foo() { const result = 1; }",
        LangId::Typescript,
        "result"
    ));
}

#[test]
fn ts_top_level_exported_const_is_exported() {
    assert!(check_exported(
        "export const bar = 42;",
        LangId::Typescript,
        "bar"
    ));
}

/// TypeScript class の `private` メソッドは公開 API ではないことを検証
/// (Issue: 2026-05-22-temperature-api-triage の private isLegacyGpt5Model 誤検出)
#[test]
fn ts_private_class_method_is_not_exported() {
    let src = r#"export class C {
    private internal(): boolean { return false; }
    public callable(): void { this.internal(); }
}
"#;
    assert!(!check_exported(src, LangId::Typescript, "internal"));
}

/// TypeScript class の `public` メソッドは引き続き公開 API として扱われることを検証
#[test]
fn ts_public_class_method_is_exported() {
    let src = r#"export class C {
    public callable(): void {}
}
"#;
    assert!(check_exported(src, LangId::Typescript, "callable"));
}

/// TypeScript class の accessibility なし (=public 相当) メソッドも exported のまま
#[test]
fn ts_default_class_method_is_exported() {
    let src = r#"export class C {
    callable(): void {}
}
"#;
    assert!(check_exported(src, LangId::Typescript, "callable"));
}

/// ECMAScript `#private` メンバを private_class_member 判定で捕捉できることを検証。
/// (extract_symbols は現状 `#`-prefixed メンバを symbols として返さないが、
/// 仮に抽出された場合に exported 判定を fail-closed にするための保険)
#[test]
fn ts_hash_private_member_helper_returns_true() {
    let src = r#"export class C {
    #secret: number = 0;
    #helper(): void {}
}
"#;
    let language = LangId::Typescript.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let root = tree.root_node();

    // private_property_identifier ノードを再帰探索し、is_private_class_member_js_ts が true を返すこと
    fn find_private_property_identifier<'t>(
        n: tree_sitter::Node<'t>,
    ) -> Option<tree_sitter::Node<'t>> {
        if n.kind() == "private_property_identifier" {
            return Some(n);
        }
        let mut c = n.walk();
        for child in n.children(&mut c) {
            if let Some(found) = find_private_property_identifier(child) {
                return Some(found);
            }
        }
        None
    }
    let priv_node =
        find_private_property_identifier(root).expect("private_property_identifier exists");
    assert!(
        is_private_class_member_js_ts(priv_node, src.as_bytes()),
        "#private メンバ ({:?}) は private_class_member として判定されるべき",
        priv_node.utf8_text(src.as_bytes())
    );
}

/// `protected` は当面保守的に exported のまま (TODO 扱い)
#[test]
fn ts_protected_class_method_is_exported() {
    let src = r#"export class C {
    protected helper(): void {}
}
"#;
    assert!(check_exported(src, LangId::Typescript, "helper"));
}

#[test]
fn python_public_function_is_exported() {
    assert!(check_exported(
        "def foo():\n    pass\n",
        LangId::Python,
        "foo"
    ));
}

#[test]
fn python_underscore_function_is_not_exported() {
    assert!(!check_exported(
        "def _helper():\n    pass\n",
        LangId::Python,
        "_helper"
    ));
}

#[test]
fn python_dunder_function_is_not_exported() {
    // `__dunder__` も `_` プレフィックスなので private 扱い
    assert!(!check_exported(
        "def __internal__():\n    pass\n",
        LangId::Python,
        "__internal__"
    ));
}

#[test]
fn python_underscore_method_is_not_exported() {
    assert!(!check_exported(
        "class C:\n    def _helper(self):\n        pass\n",
        LangId::Python,
        "_helper"
    ));
}

#[test]
fn python_underscore_class_is_not_exported() {
    assert!(!check_exported(
        "class _Internal:\n    pass\n",
        LangId::Python,
        "_Internal"
    ));
}

/// class 直下の method は従来どおり公開シンボル候補に残る (下のネスト系テストの対照)。
#[test]
fn python_class_method_is_exported() {
    assert!(check_exported(
        "class C:\n    def handle(self):\n        pass\n",
        LangId::Python,
        "handle"
    ));
}

/// 関数内のネスト定義は module 属性にならず、外から名前で参照できないため
/// 公開 API 面 (dead-code / API 差分) に出さない。
///
/// 字句スコープを見ずに `_` 規約だけで公開判定していた頃は、クロージャや
/// デコレータで登録されるハンドラが repo-wide の bare-name 参照検索にかけられ、
/// 「呼び出し式が無い」という理由だけで dead と誤報されていた。
#[test]
fn python_nested_function_is_not_exported() {
    let src = "def outer():\n    def inner():\n        pass\n    return inner\n";
    // 外側は従来どおり公開候補 (対照)
    assert!(check_exported(src, LangId::Python, "outer"));
    assert!(!check_exported(src, LangId::Python, "inner"));
}

#[test]
fn python_nested_class_is_not_exported() {
    let src = "def factory():\n    class Inner:\n        pass\n    return Inner\n";
    assert!(check_exported(src, LangId::Python, "factory"));
    assert!(!check_exported(src, LangId::Python, "Inner"));
}

#[test]
fn python_nested_function_in_method_is_not_exported() {
    let src = "class C:\n    def handle(self):\n        def helper():\n            pass\n        return helper\n";
    assert!(check_exported(src, LangId::Python, "handle"));
    assert!(!check_exported(src, LangId::Python, "helper"));
}

/// デコレータでフレームワークへ登録されるネスト関数も、ネストである時点で
/// 公開シンボル候補から外れる (登録デコレータの網羅に依存しない)。
#[test]
fn python_decorated_nested_function_is_not_exported() {
    let src = r#"
def make_app(registry):
    @registry.handler(name="ping")
    async def ping() -> str:
        return "pong"

    return registry
"#;
    assert!(check_exported(src, LangId::Python, "make_app"));
    assert!(!check_exported(src, LangId::Python, "ping"));
}

#[test]
fn python_dunder_all_limits_exports_to_list() {
    let src = r#"
__all__ = ["public_api"]

def public_api():
    pass

def also_public_without_underscore():
    pass
"#;
    assert!(check_exported(src, LangId::Python, "public_api"));
    // `also_public_without_underscore` は `_` プレフィックスを持たないが
    // `__all__` に含まれていないため非 public と判定される
    assert!(!check_exported(
        src,
        LangId::Python,
        "also_public_without_underscore"
    ));
}

#[test]
fn python_dunder_all_tuple_form_supported() {
    let src = r#"
__all__ = ("foo", 'bar')

def foo():
    pass

def bar():
    pass

def baz():
    pass
"#;
    assert!(check_exported(src, LangId::Python, "foo"));
    assert!(check_exported(src, LangId::Python, "bar"));
    assert!(!check_exported(src, LangId::Python, "baz"));
}

#[test]
fn python_typed_dunder_all_supported() {
    let src = r#"
__all__: list[str] = ["typed_api"]

def typed_api():
    pass

def other():
    pass
"#;
    assert!(check_exported(src, LangId::Python, "typed_api"));
    assert!(!check_exported(src, LangId::Python, "other"));
}

// --- is_local_scope_symbol テスト ---

fn check_local_scope(source: &str, lang_id: LangId, symbol_name: &str) -> bool {
    let language = lang_id.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, source.as_bytes(), lang_id).unwrap();
    let sym = syms
        .iter()
        .find(|s| s.name == symbol_name)
        .unwrap_or_else(|| {
            panic!(
                "symbol '{}' not found in {:?}",
                symbol_name,
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    is_local_scope_symbol(root, source.as_bytes(), lang_id, &sym.range)
}

#[test]
fn ts_local_var_is_local_scope() {
    assert!(check_local_scope(
        "export function foo() { const result = 1; }",
        LangId::Typescript,
        "result"
    ));
}

#[test]
fn ts_top_level_var_is_not_local_scope() {
    assert!(!check_local_scope(
        "export const bar = 42;",
        LangId::Typescript,
        "bar"
    ));
}

#[test]
fn ts_arrow_fn_local_is_local_scope() {
    assert!(check_local_scope(
        "export const foo = () => { const x = 1; }",
        LangId::Typescript,
        "x"
    ));
}

#[test]
fn rust_fn_def_is_not_local_scope() {
    // Rust のクエリは関数内ローカル変数をキャプチャしないが、関数定義自体はローカルスコープではない
    assert!(!check_local_scope(
        "pub fn foo() { let x = 1; }",
        LangId::Rust,
        "foo"
    ));
}

#[test]
fn ts_non_export_top_level_var_is_not_local_scope() {
    assert!(!check_local_scope(
        "const bar = 42;",
        LangId::Typescript,
        "bar"
    ));
}

#[test]
fn rust_function_local_const_is_local_scope() {
    assert!(check_local_scope(
        "pub fn outer() { const LOCAL_RUST: usize = 1; }",
        LangId::Rust,
        "LOCAL_RUST"
    ));
}

#[test]
fn python_nested_function_is_local_scope() {
    assert!(check_local_scope(
        "def outer():\n    def inner():\n        return 1\n    return inner()\n",
        LangId::Python,
        "inner"
    ));
}

#[test]
fn go_function_local_type_is_local_scope() {
    assert!(check_local_scope(
        "package demo\nfunc outer() {\n    type LocalGo struct{}\n    _ = LocalGo{}\n}\n",
        LangId::Go,
        "LocalGo"
    ));
}

#[test]
fn java_method_local_class_is_local_scope() {
    assert!(check_local_scope(
        "class Outer { void outer() { class LocalJava {} new LocalJava(); } }",
        LangId::Java,
        "LocalJava"
    ));
}

#[test]
fn kotlin_nested_function_is_local_scope() {
    assert!(check_local_scope(
        "fun outer() { fun localKotlin() = 1; localKotlin() }",
        LangId::Kotlin,
        "localKotlin"
    ));
}

#[test]
fn kotlin_top_level_function_is_not_local_scope() {
    assert!(!check_local_scope(
        "fun topLevel() = 1",
        LangId::Kotlin,
        "topLevel"
    ));
}

// --- calculate_complexity テスト ---

fn get_complexity(source: &str, lang_id: LangId, symbol_name: &str) -> Option<usize> {
    let language = lang_id.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, source.as_bytes(), lang_id).unwrap();
    let sym = syms
        .iter()
        .find(|s| s.name == symbol_name)
        .unwrap_or_else(|| {
            panic!(
                "symbol '{}' not found in {:?}",
                symbol_name,
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    sym.complexity
}

#[test]
fn rust_empty_fn_complexity_1() {
    // 空の関数はベース複雑度 1
    assert_eq!(get_complexity("fn foo() {}", LangId::Rust, "foo"), Some(1));
}

#[test]
fn rust_if_else_match_complexity() {
    // if(+1) + else(+1) + match(+1) + 2 match_arm(+2) = base 1 + 5 = 6
    // ただし match_arm は各アーム全てカウント
    let src = r#"
fn foo() {
    if x {
    } else {
        match y {
            1 => {},
            _ => {},
        }
    }
}
"#;
    // if_expression=1, match_arm=2 → 1+3=4。plain `else_clause` と
    // `match_expression` 本体は判定点ではないので数えない。
    assert_eq!(get_complexity(src, LangId::Rust, "foo"), Some(4));
}

#[test]
fn rust_for_while_loop_complexity() {
    let src = r#"
fn bar() {
    for i in 0..10 {
        while x > 0 {
            loop {
                break;
            }
        }
    }
}
"#;
    // for_expression=1, while_expression=1, loop_expression=1 → 1+3=4
    assert_eq!(get_complexity(src, LangId::Rust, "bar"), Some(4));
}

#[test]
fn python_complexity() {
    let src = r#"
def foo():
    if x:
        pass
    elif y:
        pass
    for i in range(10):
        pass
"#;
    // if_statement=1, elif_clause=1, for_statement=1 → 1+3=4
    assert_eq!(get_complexity(src, LangId::Python, "foo"), Some(4));
}

#[test]
fn ts_complexity() {
    let src = r#"
function foo() {
    if (x) {
        for (let i = 0; i < 10; i++) {}
    }
    const y = x ? 1 : 2;
}
"#;
    // if_statement=1, for_statement=1, ternary_expression=1 → 1+3=4
    assert_eq!(get_complexity(src, LangId::Typescript, "foo"), Some(4));
}

#[test]
fn struct_has_no_complexity() {
    // struct にはcomplexity が付かない
    assert_eq!(get_complexity("struct Foo {}", LangId::Rust, "Foo"), None);
}

#[test]
fn python_match_statement_complexity_counts_cases() {
    // `match` (PEP 634) の arm は case_clause=3 → 1+3=4。本体 `match_statement` は
    // 判定点ではないので数えない。旧テーブルは双方欠けており cx=1 を返していた。
    let src = r#"
def only_match(x):
    match x:
        case 1: return "a"
        case 2: return "b"
        case _: return "c"
"#;
    assert_eq!(get_complexity(src, LangId::Python, "only_match"), Some(4));
}

#[test]
fn go_expression_switch_complexity_counts_cases() {
    // tree-sitter-go の plain switch は `expression_switch_statement` + `expression_case`。
    // 旧テーブルは switch 自体を持たず、arm 名も Python の `case_clause` を誤って
    // 書いていたため、switch だけの関数がベース 1 を返していた。
    // case 2 件=2 (switch 本体と default_case は数えない) → 1+2=3
    let src = r#"
package main
func beta(x int) int {
	switch x {
	case 1:
		return 1
	case 2:
		return 2
	default:
		return 3
	}
}
"#;
    assert_eq!(get_complexity(src, LangId::Go, "beta"), Some(3));
}

#[test]
fn go_type_switch_and_select_complexity_counts_arms() {
    // type switch: type_case=2 (本体は数えない) → 1+2=3
    let type_switch = r#"
package main
func ts(i interface{}) {
	switch i.(type) {
	case int:
		_ = i
	case string:
		_ = i
	}
}
"#;
    assert_eq!(get_complexity(type_switch, LangId::Go, "ts"), Some(3));

    // select: communication_case=2 (本体は数えない) → 1+2=3
    let select = r#"
package main
func sel(a chan int, b chan int) {
	select {
	case <-a:
	case <-b:
	default:
	}
}
"#;
    assert_eq!(get_complexity(select, LangId::Go, "sel"), Some(3));
}

#[test]
fn c_do_while_and_ternary_complexity_is_counted() {
    // 旧汎用スライスは `do_statement` / `conditional_expression` を持たず、
    // これらだけを含む関数がベース 1 を返していた。
    let do_while = "int only_do(int x) { do { x--; } while (x > 0); return x; }";
    assert_eq!(get_complexity(do_while, LangId::C, "only_do"), Some(2));

    let ternary = "int only_ternary(int x) { return x > 5 ? 1 : 2; }";
    assert_eq!(get_complexity(ternary, LangId::C, "only_ternary"), Some(2));
}

#[test]
fn cpp_do_while_and_ternary_complexity_is_counted() {
    let src = "int f(int x) { do { x--; } while (x > 0); return x > 5 ? 1 : 2; }";
    // do_statement=1 + conditional_expression=1 → 1+2=3
    assert_eq!(get_complexity(src, LangId::Cpp, "f"), Some(3));
}

#[test]
fn bash_elif_and_case_item_complexity_is_counted() {
    // 旧汎用スライスは `elif_clause` / `case_item` を持たず過小計上だった。
    // if=1 + elif=1 + case=1 + case_item 2 件=2 + while=1 + for=1 + until(=while_statement)=1
    // → 1+8=9 (else_clause は Python 同様に数えない)
    let src = r#"
f() {
  if [ "$1" = a ]; then echo a
  elif [ "$1" = b ]; then echo b
  else echo c
  fi
  case "$2" in
    x) echo x ;;
    y) echo y ;;
  esac
  while true; do break; done
  for i in 1 2; do echo $i; done
  until false; do break; done
}
"#;
    // if 1 + elif 1 + case_item 2 + while 1 + for 1 + until 1 = 7 → cx 8。
    // `case_statement` 本体と `else_clause` は判定点ではないので数えない。
    assert_eq!(get_complexity(src, LangId::Bash, "f"), Some(8));
}

// --- is_override_method テスト ---

fn check_override(source: &str, lang_id: LangId, symbol_name: &str) -> bool {
    let language = lang_id.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, source.as_bytes(), lang_id).unwrap();
    let sym = syms
        .iter()
        .find(|s| s.name == symbol_name)
        .unwrap_or_else(|| {
            panic!(
                "symbol '{}' not found in {:?}",
                symbol_name,
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    is_override_method(root, source.as_bytes(), lang_id, &sym.range)
}

#[test]
fn kotlin_override_is_detected() {
    let src = "class A : B() { override fun foo() {} }";
    assert!(check_override(src, LangId::Kotlin, "foo"));
}

#[test]
fn kotlin_plain_function_is_not_override() {
    let src = "class A { fun foo() {} }";
    assert!(!check_override(src, LangId::Kotlin, "foo"));
}

#[test]
fn kotlin_override_in_object_expression_is_detected() {
    // 匿名 object 内の override も検出できること
    let src = r#"class A {
    fun setup() {
        val w = object : TextWatcher {
            override fun afterTextChanged(s: Editable?) {}
        }
    }
}"#;
    assert!(check_override(src, LangId::Kotlin, "afterTextChanged"));
}

#[test]
fn java_override_annotation_is_detected() {
    let src = r#"class A extends B {
    @Override
    public void foo() {}
}"#;
    assert!(check_override(src, LangId::Java, "foo"));
}

#[test]
fn java_plain_method_is_not_override() {
    let src = "class A { public void foo() {} }";
    assert!(!check_override(src, LangId::Java, "foo"));
}

/// GitLab #36: tree-sitter-typescript の override キーワードは kind =
/// "override_modifier" のため、旧実装 (`kind == "override"` のみ照合) では
/// 検出できず dead-code / API 差分の両方で誤検出源になっていた。
#[test]
fn ts_override_method_is_detected() {
    let src = r#"export class MyFormatter extends LogFormatter {
    public override formatAttributes(attrs: Attrs, additional: Attrs): LogItem {
        return new LogItem(attrs);
    }
}"#;
    assert!(check_override(src, LangId::Typescript, "formatAttributes"));
}

#[test]
fn ts_plain_method_is_not_override() {
    let src = r#"export class MyFormatter extends LogFormatter {
    public formatAttributes(attrs: Attrs): LogItem {
        return new LogItem(attrs);
    }
}"#;
    assert!(!check_override(src, LangId::Typescript, "formatAttributes"));
}

#[test]
fn tsx_override_method_is_detected() {
    let src = r#"export class Panel extends BasePanel {
    override render() {
        return <div />;
    }
}"#;
    assert!(check_override(src, LangId::Tsx, "render"));
}

#[test]
fn contains_keyword_respects_word_boundaries() {
    // `overrider` は `override` キーワードに誤マッチしないこと
    assert!(!super::contains_keyword("public overrider", "override"));
    assert!(super::contains_keyword("public override fun", "override"));
    assert!(super::contains_keyword("override", "override"));
}

// --- has_framework_entrypoint_decorator_python テスト ---

fn check_python_framework_decorator(source: &str, symbol_name: &str) -> bool {
    let language = LangId::Python.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, source.as_bytes(), LangId::Python).unwrap();
    let sym = syms
        .iter()
        .find(|s| s.name == symbol_name)
        .unwrap_or_else(|| {
            panic!(
                "symbol '{}' not found in {:?}",
                symbol_name,
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    has_framework_entrypoint_decorator_python(root, source.as_bytes(), &sym.range)
}

#[test]
fn python_typer_command_decorator_detected() {
    // Issue 報告ケース: Typer の @app.command(...) で装飾された関数
    let src = r#"
import typer
app = typer.Typer()

@app.command("list")
def list_tokens():
    pass
"#;
    assert!(check_python_framework_decorator(src, "list_tokens"));
}

#[test]
fn python_typer_callback_decorator_detected() {
    let src = r#"
@app.callback()
def main():
    pass
"#;
    assert!(check_python_framework_decorator(src, "main"));
}

#[test]
fn python_click_command_decorator_detected() {
    let src = r#"
import click

@click.command()
def hello():
    pass
"#;
    assert!(check_python_framework_decorator(src, "hello"));
}

#[test]
fn python_click_group_subcommand_detected() {
    let src = r#"
@cli.command()
def sync():
    pass
"#;
    assert!(check_python_framework_decorator(src, "sync"));
}

#[test]
fn python_fastapi_route_detected() {
    let src = r#"
@app.get("/items/{item_id}")
def read_item(item_id: int):
    return {"item_id": item_id}
"#;
    assert!(check_python_framework_decorator(src, "read_item"));
}

#[test]
fn python_fastapi_router_post_detected() {
    let src = r#"
@router.post("/users/")
def create_user(user: User):
    return user
"#;
    assert!(check_python_framework_decorator(src, "create_user"));
}

#[test]
fn python_flask_route_detected() {
    let src = r#"
@app.route("/")
def index():
    return "Hello"
"#;
    assert!(check_python_framework_decorator(src, "index"));
}

#[test]
fn python_flask_blueprint_route_detected() {
    let src = r#"
@bp.route("/foo")
def foo():
    return "foo"
"#;
    assert!(check_python_framework_decorator(src, "foo"));
}

#[test]
fn python_pytest_fixture_detected() {
    let src = r#"
import pytest

@pytest.fixture
def db_session():
    return None
"#;
    assert!(check_python_framework_decorator(src, "db_session"));
}

#[test]
fn python_pytest_mark_parametrize_detected() {
    let src = r#"
import pytest

@pytest.mark.parametrize("x", [1, 2])
def test_x(x):
    assert x > 0
"#;
    assert!(check_python_framework_decorator(src, "test_x"));
}

#[test]
fn python_celery_task_detected() {
    let src = r#"
@app.task
def send_email(to):
    pass
"#;
    assert!(check_python_framework_decorator(src, "send_email"));
}

#[test]
fn python_django_receiver_detected() {
    let src = r#"
from django.dispatch import receiver

@receiver(post_save, sender=User)
def on_user_save(sender, instance, **kwargs):
    pass
"#;
    assert!(check_python_framework_decorator(src, "on_user_save"));
}

#[test]
fn python_django_login_required_detected() {
    let src = r#"
from django.contrib.auth.decorators import login_required

@login_required
def my_view(request):
    return None
"#;
    assert!(check_python_framework_decorator(src, "my_view"));
}

/// 汎用の登録デコレータ (MCP の `@server.tool(...)`、イベントバスの
/// `@bus.subscribe(...)` 等) はランタイムがハンドラを呼ぶ入口なので runtime entrypoint。
#[test]
fn python_generic_registration_decorators_detected() {
    for (src, name) in [
        (
            "@server.tool(name=\"ping\")\ndef ping():\n    return \"pong\"\n",
            "ping",
        ),
        (
            "@registry.handler(\"evt\")\ndef on_evt():\n    pass\n",
            "on_evt",
        ),
        (
            "@bus.subscribe(\"topic\")\ndef consume():\n    pass\n",
            "consume",
        ),
        ("@emitter.listener()\ndef on_tick():\n    pass\n", "on_tick"),
    ] {
        assert!(
            check_python_framework_decorator(src, name),
            "登録デコレータとして扱うべき: {src}"
        );
    }
}

/// 「関数を変換するだけ」のデコレータは登録入口ではない。これらまで live に倒すと
/// 真の dead を恒久的に見逃す (対照ケース)。
#[test]
fn python_wrapping_decorators_not_detected() {
    for (src, name) in [
        (
            "from functools import lru_cache\n\n@lru_cache(maxsize=None)\ndef compute():\n    return 1\n",
            "compute",
        ),
        (
            "import functools\n\n@functools.wraps(inner)\ndef wrapper():\n    pass\n",
            "wrapper",
        ),
        ("@retry(times=3)\ndef flaky():\n    pass\n", "flaky"),
    ] {
        assert!(
            !check_python_framework_decorator(src, name),
            "登録入口として扱ってはならない: {src}"
        );
    }
}

#[test]
fn python_dataclass_decorator_not_detected() {
    // @dataclass はフレームワーク登録ではないので除外しない
    let src = r#"
from dataclasses import dataclass

@dataclass
class Point:
    x: int
    y: int
"#;
    assert!(!check_python_framework_decorator(src, "Point"));
}

#[test]
fn python_property_decorator_not_detected() {
    let src = r#"
class Foo:
    @property
    def name(self):
        return self._name
"#;
    assert!(!check_python_framework_decorator(src, "name"));
}

#[test]
fn python_classmethod_decorator_not_detected() {
    let src = r#"
class Foo:
    @classmethod
    def create(cls):
        return cls()
"#;
    assert!(!check_python_framework_decorator(src, "create"));
}

#[test]
fn python_undecorated_function_not_detected() {
    let src = r#"
def helper():
    return None
"#;
    assert!(!check_python_framework_decorator(src, "helper"));
}

#[test]
fn python_decorated_method_in_class_detected() {
    // クラス内のメソッドでも @app.command が認識できること
    let src = r#"
class Cli:
    @app.command("show")
    def show(self):
        pass
"#;
    assert!(check_python_framework_decorator(src, "show"));
}

/// `python_class_base_names` がクラスの直接 base を identifier / attribute 形式で
/// 抽出できることを検証する (`unittest.TestCase` 等)。
#[test]
fn python_class_base_names_returns_identifier_and_attribute() {
    let src = "import unittest\nclass Foo(unittest.TestCase, BaseMixin):\n    pass\n";
    let language = LangId::Python.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, src.as_bytes(), LangId::Python).unwrap();
    let foo = syms.iter().find(|s| s.name == "Foo").expect("class Foo");
    let bases = python_class_base_names(root, src.as_bytes(), &foo.range);
    assert_eq!(
        bases,
        vec!["unittest.TestCase".to_string(), "BaseMixin".to_string()]
    );
}

/// クラスが base を持たない場合は空リストを返すことを検証する。
#[test]
fn python_class_base_names_empty_when_no_base() {
    let src = "class Foo:\n    pass\n";
    let language = LangId::Python.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, src.as_bytes(), LangId::Python).unwrap();
    let foo = syms.iter().find(|s| s.name == "Foo").expect("class Foo");
    let bases = python_class_base_names(root, src.as_bytes(), &foo.range);
    assert!(bases.is_empty(), "no base => empty: {bases:?}");
}

/// `metaclass=...` のようなキーワード引数は base に含まれないことを検証する。
#[test]
fn python_class_base_names_skips_keyword_arguments() {
    let src = "class Foo(Bar, metaclass=ABCMeta):\n    pass\n";
    let language = LangId::Python.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, src.as_bytes(), LangId::Python).unwrap();
    let foo = syms.iter().find(|s| s.name == "Foo").expect("class Foo");
    let bases = python_class_base_names(root, src.as_bytes(), &foo.range);
    assert_eq!(bases, vec!["Bar".to_string()]);
}

/// Python の動的プロトコルメソッド判定を、抽出済みシンボルの range で呼び出す。
fn check_python_dynamic_protocol(src: &str, method_name: &str) -> bool {
    let language = LangId::Python.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let root = tree.root_node();
    let syms = extract_symbols(root, src.as_bytes(), LangId::Python).unwrap();
    let method = syms
        .iter()
        .find(|symbol| symbol.name == method_name)
        .expect("対象メソッドを抽出できること");
    is_python_dynamic_protocol_method(root, src.as_bytes(), &method.range, method_name)
}

#[test]
fn python_url_handler_protocol_methods_are_detected() {
    let src = concat!(
        "class Guard(urllib.request.HTTPHandler):\n",
        "    def http_open(self, request):\n",
        "        return request\n",
        "    def helper(self):\n",
        "        return None\n",
    );

    assert!(check_python_dynamic_protocol(src, "http_open"));
    assert!(!check_python_dynamic_protocol(src, "helper"));
}

#[test]
fn python_url_auth_and_error_processor_callbacks_are_detected() {
    let auth = concat!(
        "class Auth(urllib.request.HTTPBasicAuthHandler):\n",
        "    def http_error_401(self, request, response, code, message, headers):\n",
        "        return response\n",
    );
    let processor = concat!(
        "class Processor(urllib.request.HTTPErrorProcessor):\n",
        "    def http_response(self, request, response):\n",
        "        return response\n",
        "    def https_response(self, request, response):\n",
        "        return response\n",
    );

    assert!(check_python_dynamic_protocol(auth, "http_error_401"));
    assert!(check_python_dynamic_protocol(processor, "http_response"));
    assert!(check_python_dynamic_protocol(processor, "https_response"));
}

#[test]
fn python_watchdog_callbacks_are_detected() {
    let src = concat!(
        "class Handler(watchdog.events.FileSystemEventHandler):\n",
        "    def on_any_event(self, event):\n",
        "        return event\n",
    );

    assert!(check_python_dynamic_protocol(src, "on_any_event"));
}

#[test]
fn python_protocol_method_names_require_known_base_class() {
    let src = concat!(
        "class OrdinaryHandler:\n",
        "    def http_open(self, request):\n",
        "        return request\n",
        "    def on_any_event(self, event):\n",
        "        return event\n",
    );

    assert!(!check_python_dynamic_protocol(src, "http_open"));
    assert!(!check_python_dynamic_protocol(src, "on_any_event"));
}

/// PHP 擬似 enum パターンの判定 (Laravel/DDD 系の AbstractValueObject 派生)
#[test]
fn is_php_pseudo_enum_method_detects_value_object_factory() {
    let src = r#"<?php
class MenuName {
    public static function MENU_HOME(): self {
        return new self('MENU_HOME');
    }
    public static function MENU_NEW_FEATURE(): static {
        return new static('MENU_NEW_FEATURE');
    }
    public static function notPseudo(): self {
        return new self('different_name');
    }
    public function instanceMethod(): self {
        return new self('instanceMethod');
    }
}
"#;
    let language = LangId::Php.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, src.as_bytes(), LangId::Php).unwrap();

    let menu_home = syms
        .iter()
        .find(|s| s.name == "MENU_HOME")
        .expect("MENU_HOME");
    assert!(
        is_php_pseudo_enum_method(root, src.as_bytes(), &menu_home.range, "MENU_HOME"),
        "self ベースの擬似 enum を検出すべき"
    );

    let menu_new = syms
        .iter()
        .find(|s| s.name == "MENU_NEW_FEATURE")
        .expect("MENU_NEW_FEATURE");
    assert!(
        is_php_pseudo_enum_method(root, src.as_bytes(), &menu_new.range, "MENU_NEW_FEATURE"),
        "static ベースの擬似 enum も検出すべき"
    );

    let not_pseudo = syms
        .iter()
        .find(|s| s.name == "notPseudo")
        .expect("notPseudo");
    assert!(
        !is_php_pseudo_enum_method(root, src.as_bytes(), &not_pseudo.range, "notPseudo"),
        "メソッド名と new self('...') の文字列が不一致なら擬似 enum ではない"
    );

    let instance_method = syms
        .iter()
        .find(|s| s.name == "instanceMethod")
        .expect("instanceMethod");
    assert!(
        !is_php_pseudo_enum_method(
            root,
            src.as_bytes(),
            &instance_method.range,
            "instanceMethod"
        ),
        "static でないメソッドは擬似 enum ではない"
    );
}

/// 戻り値型が self/static でないと擬似 enum とみなさない
#[test]
fn is_php_pseudo_enum_method_requires_self_return_type() {
    let src = r#"<?php
class Foo {
    public static function BAR(): Foo {
        return new self('BAR');
    }
}
"#;
    let language = LangId::Php.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, src.as_bytes(), LangId::Php).unwrap();
    let bar = syms.iter().find(|s| s.name == "BAR").expect("BAR");
    assert!(
        !is_php_pseudo_enum_method(root, src.as_bytes(), &bar.range, "BAR"),
        "戻り値型が self/static でないと擬似 enum とは判定しない"
    );
}

/// PHP runtime annotation の検出 (`@TypeItem`, `@Route`, `@dataProvider` 等)
#[test]
fn php_doc_has_runtime_annotation_detects_uppercase_annotations() {
    // @TypeItem fully-qualified
    assert!(php_doc_has_runtime_annotation(
        "/**\n * @\\App\\Annotations\\TypeItem(id=1, name=\"X\")\n */"
    ));
    // @TypeItem short
    assert!(php_doc_has_runtime_annotation("/** @TypeItem(...) */"));
    // @Route
    assert!(php_doc_has_runtime_annotation("/** @Route(...) */"));
    // @dataProvider (lowercase 慣用)
    assert!(php_doc_has_runtime_annotation(
        "/** @dataProvider voEvent */"
    ));
    // @DataProvider (PHPUnit 11 形式)
    assert!(php_doc_has_runtime_annotation(
        "/** @DataProvider('foo') */"
    ));
}

#[test]
fn php_doc_has_runtime_annotation_skips_plain_docstring() {
    // 通常コメント
    assert!(!php_doc_has_runtime_annotation("/** メニュー: ホーム */"));
    // @param / @return / @var (低リスクの常識的タグ)
    assert!(!php_doc_has_runtime_annotation("/** @param int $x */"));
    assert!(!php_doc_has_runtime_annotation("/** @return self */"));
    assert!(!php_doc_has_runtime_annotation("/** @var string */"));
}

// --- is_js_ts_framework_dsl_callback テスト ---

fn check_js_ts_dsl_callback(source: &str, lang: LangId, symbol_name: &str) -> bool {
    let language = lang.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, source.as_bytes(), lang).unwrap();
    let sym = syms
        .iter()
        .find(|s| s.name == symbol_name)
        .unwrap_or_else(|| {
            panic!(
                "symbol '{}' not found in {:?}",
                symbol_name,
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    is_js_ts_framework_dsl_callback(root, source.as_bytes(), &sym.range)
}

#[test]
fn js_ts_wxt_define_content_script_main_method_detected() {
    // Issue 報告ケース: WXT defineContentScript の引数 object に method shorthand
    let src = r#"
import { defineContentScript } from 'wxt/sandbox';

export default defineContentScript({
    matches: ['*://example.com/*'],
    main() {
        console.log('hello');
    },
});
"#;
    assert!(check_js_ts_dsl_callback(src, LangId::Typescript, "main"));
}

#[test]
fn js_ts_wxt_define_background_main_method_detected() {
    let src = r#"
export default defineBackground({
    main() {
        console.log('background');
    },
});
"#;
    assert!(check_js_ts_dsl_callback(src, LangId::Typescript, "main"));
}

#[test]
fn js_ts_vue_define_component_setup_method_detected() {
    // Vue 推奨の method shorthand 形式
    let src = r#"
export default defineComponent({
    setup() {
        return {};
    },
});
"#;
    assert!(check_js_ts_dsl_callback(src, LangId::Typescript, "setup"));
}

#[test]
fn js_ts_vite_define_config_method_detected() {
    let src = r#"
export default defineConfig({
    name: "x",
});
"#;
    // この source には method/function symbol が無いため別 case で確認:
    let src2 = r#"
export default defineConfig({
    onPluginInit() {
        return null;
    },
});
"#;
    let _ = src;
    assert!(check_js_ts_dsl_callback(
        src2,
        LangId::Typescript,
        "onPluginInit"
    ));
}

#[test]
fn js_ts_plain_object_method_not_excluded() {
    // 通常のオブジェクトリテラルの method shorthand は dead 候補のまま (false)
    let src = r#"
export const obj = {
    main() {
        return 1;
    },
};
"#;
    assert!(!check_js_ts_dsl_callback(src, LangId::Typescript, "main"));
}

#[test]
fn js_ts_unknown_dsl_caller_not_excluded() {
    // allowlist に含まれない関数の引数 object メソッドは dead 候補のまま
    let src = r#"
export default someUnknownFn({
    main() {
        return 1;
    },
});
"#;
    assert!(!check_js_ts_dsl_callback(src, LangId::Typescript, "main"));
}

#[test]
fn js_ts_inner_helper_inside_dsl_callback_not_excluded() {
    // DSL callback (main) 内部で定義された helper は dead 候補のまま
    let src = r#"
export default defineContentScript({
    main() {
        function helper() {
            return 1;
        }
        return helper();
    },
});
"#;
    // main は除外対象
    assert!(check_js_ts_dsl_callback(src, LangId::Typescript, "main"));
    // helper は対象外 (内部 function は dead 判定の余地を残す)
    assert!(!check_js_ts_dsl_callback(src, LangId::Typescript, "helper"));
}

// --- is_js_ts_angular_lifecycle_hook テスト ---

fn check_angular_lifecycle_hook(source: &str, symbol_name: &str) -> bool {
    let language = LangId::Typescript.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();

    let syms = extract_symbols(root, source.as_bytes(), LangId::Typescript).unwrap();
    let sym = syms
        .iter()
        .find(|s| s.name == symbol_name)
        .unwrap_or_else(|| {
            panic!(
                "symbol '{}' not found in {:?}",
                symbol_name,
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    is_js_ts_angular_lifecycle_hook(root, source.as_bytes(), &sym.range)
}

/// GitLab issue #8 再現: `@Component` 装飾クラスの `ngAfterViewChecked` は除外対象。
#[test]
fn angular_component_lifecycle_hook_is_detected() {
    let src = r#"
import { Component } from '@angular/core';

@Component({
    template: '<div>example</div>',
})
export class MinimalComponent {
    public ngAfterViewChecked(): void {
    }
}
"#;
    assert!(check_angular_lifecycle_hook(src, "ngAfterViewChecked"));
}

/// `@Directive` 装飾クラスの lifecycle hook も除外対象。
#[test]
fn angular_directive_lifecycle_hook_is_detected() {
    let src = r#"
import { Directive } from '@angular/core';

@Directive({ selector: '[appFoo]' })
export class FooDirective {
    public ngOnInit(): void {}
    public ngOnDestroy(): void {}
}
"#;
    assert!(check_angular_lifecycle_hook(src, "ngOnInit"));
    assert!(check_angular_lifecycle_hook(src, "ngOnDestroy"));
}

/// `implements AfterViewChecked` 等の interface 実装が省略されていても、Angular の
/// 呼出規約は decorator + メソッド名で成立する。
#[test]
fn angular_lifecycle_hook_detected_without_interface_implementation() {
    let src = r#"
@Component({ template: '' })
class WithoutInterface {
    ngAfterViewChecked() {}
}
"#;
    assert!(check_angular_lifecycle_hook(src, "ngAfterViewChecked"));
}

/// `@Component` / `@Directive` のいずれも持たないクラスのメソッドは Angular hook 扱いしない。
#[test]
fn non_angular_class_with_same_method_name_not_detected() {
    let src = r#"
class PlainClass {
    ngAfterViewChecked(): void {}
}
"#;
    assert!(!check_angular_lifecycle_hook(src, "ngAfterViewChecked"));
}

/// `@Injectable` 等の他の Angular decorator は対象外 (lifecycle hook を持たないため)。
#[test]
fn angular_injectable_class_method_not_detected() {
    let src = r#"
@Injectable({ providedIn: 'root' })
class FooService {
    ngOnInit(): void {}
}
"#;
    assert!(!check_angular_lifecycle_hook(src, "ngOnInit"));
}

/// Angular lifecycle hook 名以外のメソッド (custom method) は除外対象外。
#[test]
fn angular_component_non_lifecycle_method_not_detected() {
    let src = r#"
@Component({ template: '' })
class Foo {
    public regularMethod(): void {}
}
"#;
    assert!(!check_angular_lifecycle_hook(src, "regularMethod"));
}

// --- is_js_ts_angular_runtime_entrypoint テスト ---

fn check_angular_runtime_entrypoint(source: &str, symbol_name: &str) -> bool {
    let language = LangId::Typescript.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();
    let syms = extract_symbols(root, source.as_bytes(), LangId::Typescript).unwrap();
    let sym = syms
        .iter()
        .find(|s| s.name == symbol_name)
        .unwrap_or_else(|| {
            panic!(
                "symbol '{}' not found in {:?}",
                symbol_name,
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    is_js_ts_angular_runtime_entrypoint(root, source.as_bytes(), &sym.range)
}

/// GitLab issue #20: `implements ControlValueAccessor` のクラスの 4 規約メソッドは除外。
#[test]
fn angular_cva_methods_via_implements_are_detected() {
    let src = r#"
@Directive()
export abstract class AbstractBaseControl implements ControlValueAccessor {
    writeValue(obj: any) {}
    registerOnChange(fn: any) {}
    registerOnTouched(fn: any) {}
    setDisabledState(isDisabled: boolean) {}
}
"#;
    assert!(check_angular_runtime_entrypoint(src, "writeValue"));
    assert!(check_angular_runtime_entrypoint(src, "registerOnChange"));
    assert!(check_angular_runtime_entrypoint(src, "registerOnTouched"));
    assert!(check_angular_runtime_entrypoint(src, "setDisabledState"));
}

/// GitLab issue #20: `NG_VALUE_ACCESSOR` provider 登録のあるクラス (implements 句なし) でも
/// CVA 規約メソッドは除外する。
#[test]
fn angular_cva_methods_via_ng_value_accessor_provider_are_detected() {
    let src = r#"
@Component({
    selector: 'bz-input',
    providers: [{ provide: NG_VALUE_ACCESSOR, useExisting: InputControlComponent, multi: true }],
})
export class InputControlComponent {
    writeValue(obj: any) {}
    registerOnChange(fn: any) {}
    registerOnTouched(fn: any) {}
}
"#;
    assert!(check_angular_runtime_entrypoint(src, "writeValue"));
    assert!(check_angular_runtime_entrypoint(src, "registerOnChange"));
    assert!(check_angular_runtime_entrypoint(src, "registerOnTouched"));
}

/// CVA 規約メソッド名でも `implements ControlValueAccessor` も NG_VALUE_ACCESSOR provider
/// もない通常クラスでは除外対象外 (Angular 装飾があるだけでは不十分。CVA 契約の証拠が必要)。
#[test]
fn angular_cva_method_name_in_non_cva_class_not_detected() {
    let src = r#"
@Component({ template: '' })
export class PlainComponent {
    writeValue(obj: any) {}
}
"#;
    assert!(!check_angular_runtime_entrypoint(src, "writeValue"));
}

/// GitLab issue #25: `@Directive()` / `@Component` 装飾が無くても、`implements
/// ControlValueAccessor` の abstract 基底クラスの CVA 規約メソッドは除外する。
/// 具象子クラスが別ファイルで `@Component({...NG_VALUE_ACCESSOR provider...})` を宣言し
/// `extends` する Angular の慣用パターンに対応する (codex 設計判断 — `ControlValueAccessor`
/// は @angular/forms の専用契約名なので同名衝突は実用上ゼロと評価)。
#[test]
fn angular_cva_in_undecorated_abstract_base_is_detected() {
    let src = r#"
export abstract class AbstractBaseControl implements ControlValueAccessor {
    writeValue(obj: any) {}
    registerOnChange(fn: any) {}
    registerOnTouched(fn: any) {}
    setDisabledState(isDisabled: boolean) {}
}
"#;
    assert!(check_angular_runtime_entrypoint(src, "writeValue"));
    assert!(check_angular_runtime_entrypoint(src, "registerOnChange"));
    assert!(check_angular_runtime_entrypoint(src, "registerOnTouched"));
    assert!(check_angular_runtime_entrypoint(src, "setDisabledState"));
}

/// `@Injectable` 装飾クラスでも `implements ControlValueAccessor` を伴う場合は除外。
/// 装飾なし基底を許す方針 (#25) との一貫性のため、Angular の他装飾でも CVA 契約の証拠が
/// あれば抑止する。
#[test]
fn angular_cva_in_injectable_class_with_implements_is_detected() {
    let src = r#"
@Injectable({ providedIn: 'root' })
export class InjectableCva implements ControlValueAccessor {
    writeValue(obj: any) {}
}
"#;
    assert!(check_angular_runtime_entrypoint(src, "writeValue"));
}

/// `ControlValueAccessor` 以外の interface を implements する class で同名 CVA メソッドが
/// あっても除外対象外。誤抑止防止のため interface 名は厳密一致する必要がある。
#[test]
fn angular_cva_method_in_unrelated_interface_implementation_not_detected() {
    let src = r#"
interface SomeOtherContract { writeValue(obj: any): void; }
export class Other implements SomeOtherContract {
    writeValue(obj: any) {}
}
"#;
    assert!(!check_angular_runtime_entrypoint(src, "writeValue"));
}

// --- is_php_laravel_runtime_entrypoint テスト ---

fn check_php_laravel_runtime_entrypoint(source: &str, symbol_name: &str) -> bool {
    let language = LangId::Php.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();
    let syms = extract_symbols(root, source.as_bytes(), LangId::Php).unwrap();
    let sym = syms
        .iter()
        .find(|s| s.name == symbol_name)
        .unwrap_or_else(|| {
            panic!(
                "symbol '{}' not found in {:?}",
                symbol_name,
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    is_php_laravel_runtime_entrypoint(root, source.as_bytes(), &sym.range)
}

/// GitLab issue #21: 戻り型 `BelongsTo` の public method は Eloquent リレーション
/// 定義として dead 除外。
#[test]
fn php_eloquent_relation_belongs_to_method_is_detected() {
    let src = "<?php
class QueueEloquent extends Model {
    public function omataseGuidance(): BelongsTo {
        return $this->belongsTo(GuidanceEloquent::class);
    }
}
";
    assert!(check_php_laravel_runtime_entrypoint(src, "omataseGuidance"));
}

/// 他の主要 Eloquent リレーション戻り型 (HasMany / HasOneThrough / MorphedByMany 等) も
/// 検出される。
#[test]
fn php_eloquent_relation_other_return_types_are_detected() {
    let src = "<?php
class M extends Model {
    public function items(): HasMany { return $this->hasMany(Item::class); }
    public function profile(): HasOneThrough { return $this->hasOneThrough(P::class, Q::class); }
    public function tags(): BelongsToMany { return $this->belongsToMany(Tag::class); }
    public function reverse(): MorphedByMany { return $this->morphedByMany(R::class, 'taggable'); }
}
";
    assert!(check_php_laravel_runtime_entrypoint(src, "items"));
    assert!(check_php_laravel_runtime_entrypoint(src, "profile"));
    assert!(check_php_laravel_runtime_entrypoint(src, "tags"));
    assert!(check_php_laravel_runtime_entrypoint(src, "reverse"));
}

/// FQN 戻り型 (`Illuminate\Database\Eloquent\Relations\BelongsTo`) でも末尾名で検出する。
#[test]
fn php_eloquent_relation_fqcn_return_type_is_detected() {
    let src = "<?php
class M extends Model {
    public function owner(): \\Illuminate\\Database\\Eloquent\\Relations\\BelongsTo {
        return $this->belongsTo(Owner::class);
    }
}
";
    assert!(check_php_laravel_runtime_entrypoint(src, "owner"));
}

/// 戻り型が無い / 戻り型が Relation 系でない public method は除外対象外。
#[test]
fn php_method_without_relation_return_type_not_detected() {
    let src = "<?php
class M extends Model {
    public function helper(): string { return ''; }
    public function noReturnType() { return 1; }
}
";
    assert!(!check_php_laravel_runtime_entrypoint(src, "helper"));
    assert!(!check_php_laravel_runtime_entrypoint(src, "noReturnType"));
}

/// `private` / `protected` visibility のメソッドは除外対象外 (Eloquent は public
/// relation method のみ呼ぶ)。
#[test]
fn php_non_public_relation_method_not_detected() {
    let src = "<?php
class M extends Model {
    protected function hidden(): BelongsTo { return $this->belongsTo(X::class); }
}
";
    assert!(!check_php_laravel_runtime_entrypoint(src, "hidden"));
}

/// GitLab issue #22: `implements CanResetPasswordContract` クラスの
/// `getEmailForPasswordReset` / `sendPasswordResetNotification` は dead 除外。
#[test]
fn php_laravel_can_reset_password_contract_methods_are_detected() {
    let src = "<?php
class AccountEloquent extends Model implements AuthenticatableContract, CanResetPasswordContract {
    public function getEmailForPasswordReset(): string { return $this->email; }
    public function sendPasswordResetNotification($token): void {}
}
";
    assert!(check_php_laravel_runtime_entrypoint(
        src,
        "getEmailForPasswordReset"
    ));
    assert!(check_php_laravel_runtime_entrypoint(
        src,
        "sendPasswordResetNotification"
    ));
}

/// `CanResetPassword` (alias なし) でも同名メソッドは検出する。
#[test]
fn php_laravel_can_reset_password_simple_name_methods_are_detected() {
    let src = "<?php
class A implements CanResetPassword {
    public function getEmailForPasswordReset(): string { return ''; }
}
";
    assert!(check_php_laravel_runtime_entrypoint(
        src,
        "getEmailForPasswordReset"
    ));
}

/// 既知 contract を implements しない class で同名メソッドがあっても dead 抑止しない。
/// `getEmailForPasswordReset` を自前で生やしただけのクラスは contract と関係ないため。
#[test]
fn php_laravel_contract_method_in_non_implementing_class_not_detected() {
    let src = "<?php
class StandaloneHelper {
    public function getEmailForPasswordReset(): string { return ''; }
}
";
    assert!(!check_php_laravel_runtime_entrypoint(
        src,
        "getEmailForPasswordReset"
    ));
}

/// GitLab issue #23: `@HostListener` 付きメソッドは除外対象。
#[test]
fn angular_host_listener_method_is_detected() {
    let src = r#"
@Component({ template: '' })
export class ChatComponent {
    @HostListener('window:beforeunload', ['$event'])
    beforeUnloadHandler() {}
}
"#;
    assert!(check_angular_runtime_entrypoint(src, "beforeUnloadHandler"));
}

/// `@HostListener` が付かないメソッドは除外対象外。
#[test]
fn angular_method_without_host_listener_not_detected() {
    let src = r#"
@Component({ template: '' })
export class ChatComponent {
    notAHandler() {}
}
"#;
    assert!(!check_angular_runtime_entrypoint(src, "notAHandler"));
}

/// `@HostBinding` 付き method も Angular 経路扱いで除外する (member 単位 decorator
/// allowlist の網羅確認)。
#[test]
fn angular_host_binding_method_is_detected() {
    let src = r#"
@Directive({ selector: '[appFoo]' })
export class FooDirective {
    @HostBinding('class.active')
    isActive() { return true; }
}
"#;
    assert!(check_angular_runtime_entrypoint(src, "isActive"));
}

/// `@Component` / `@Directive` を持たないクラスのメンバーは Angular 経路扱いしない
/// (member decorator が付いていても class 装飾が無ければ Angular とみなさない)。
#[test]
fn angular_member_decorator_in_non_angular_class_not_detected() {
    let src = r#"
class Plain {
    @HostListener('click')
    onClick() {}
}
"#;
    assert!(!check_angular_runtime_entrypoint(src, "onClick"));
}

// --- is_java_flyway_migration_class テスト ---

fn check_java_flyway_class(source: &str, symbol_name: &str) -> bool {
    let language = LangId::Java.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(source, None).unwrap();
    let root = tree.root_node();
    let syms = extract_symbols(root, source.as_bytes(), LangId::Java).unwrap();
    let sym = syms
        .iter()
        .find(|s| s.name == symbol_name)
        .unwrap_or_else(|| {
            panic!(
                "symbol '{}' not found in {:?}",
                symbol_name,
                syms.iter().map(|s| &s.name).collect::<Vec<_>>()
            )
        });
    is_java_flyway_migration_class(root, source.as_bytes(), &sym.range)
}

/// GitLab issue #24 再現: `extends BaseJavaMigration` の class は除外対象。
#[test]
fn java_flyway_migration_extends_base_class_is_detected() {
    let src = r#"
package db.migration;

import org.flywaydb.core.api.migration.BaseJavaMigration;
import org.flywaydb.core.api.migration.Context;

public class V2021_01_02__Zipcode extends BaseJavaMigration {
    public void migrate(Context context) throws Exception {
    }
}
"#;
    assert!(check_java_flyway_class(src, "V2021_01_02__Zipcode"));
}

/// `implements JavaMigration` のクラスも除外対象。
#[test]
fn java_flyway_migration_implements_interface_is_detected() {
    let src = r#"
package db.migration;

import org.flywaydb.core.api.migration.JavaMigration;
import org.flywaydb.core.api.migration.MigrationVersion;

public class V1__Manual implements JavaMigration {
    public MigrationVersion getVersion() { return null; }
    public String getDescription() { return ""; }
    public Integer getChecksum() { return null; }
    public boolean isUndo() { return false; }
    public boolean canExecuteInTransaction() { return true; }
    public void migrate(org.flywaydb.core.api.migration.Context context) throws Exception {}
}
"#;
    assert!(check_java_flyway_class(src, "V1__Manual"));
}

/// 完全修飾名 (`extends org.flywaydb.core.api.migration.BaseJavaMigration`) でも検出する。
#[test]
fn java_flyway_migration_fully_qualified_super_is_detected() {
    let src = r#"
package db.migration;

public class V2__Fqcn extends org.flywaydb.core.api.migration.BaseJavaMigration {
    public void migrate(org.flywaydb.core.api.migration.Context context) throws Exception {}
}
"#;
    assert!(check_java_flyway_class(src, "V2__Fqcn"));
}

/// Flyway を継承していない通常のクラスは除外対象外。
#[test]
fn java_non_flyway_class_not_detected() {
    let src = r#"
package app.example;

public class RegularService {
    public void doWork() {}
}
"#;
    assert!(!check_java_flyway_class(src, "RegularService"));
}

/// 同名でも別 framework (`extends BaseTask` 等) の継承は Flyway と誤判定しない。
#[test]
fn java_unrelated_super_class_not_detected() {
    let src = r#"
package app.batch;

public class V2021_01_02__BatchJob extends app.batch.BaseTask {
    public void run() {}
}
"#;
    assert!(!check_java_flyway_class(src, "V2021_01_02__BatchJob"));
}

/// Flyway migration class 配下の method (例: `migrate(Context)`) も同じ Flyway 判定で
/// true を返す。symbol_range から class_declaration 祖先まで遡る設計のため、Class と
/// Method を同じヘルパーで処理できる (dead-code 経路は class 単体 + 配下メソッドの
/// 両方を Flyway runtime が反射経由で呼ぶため除外したい)。
#[test]
fn java_flyway_migration_member_method_is_detected_via_class() {
    let src = r#"
package db.migration;
import org.flywaydb.core.api.migration.BaseJavaMigration;
import org.flywaydb.core.api.migration.Context;
public class V1__X extends BaseJavaMigration {
    public void migrate(Context context) throws Exception {}
}
"#;
    assert!(check_java_flyway_class(src, "migrate"));
}

/// `implements MigrationContainer<JavaMigration>` のような generic 型引数として現れる
/// `JavaMigration` は「直接の親型」ではないため除外対象にしない (codex 指摘の再帰走査
/// 過剰マッチ修正)。
#[test]
fn java_generic_type_argument_with_flyway_name_not_detected() {
    let src = r#"
package app;
interface MigrationContainer<T> {}
class JavaMigration {}
public class Wrapper implements MigrationContainer<JavaMigration> {
    public void run() {}
}
"#;
    assert!(!check_java_flyway_class(src, "Wrapper"));
}

/// 別 package の `com.example.BaseJavaMigration` は完全修飾名が Flyway のものと
/// 一致しないため除外対象にしない (codex 指摘の FQN 末尾名マッチ修正)。
#[test]
fn java_unrelated_fqcn_base_with_flyway_simple_name_not_detected() {
    let src = r#"
package app;
public class Custom extends com.example.BaseJavaMigration {
    public void run() {}
}
"#;
    assert!(!check_java_flyway_class(src, "Custom"));
}

/// FQN が改行 (valid Java) で分割されていても、AST 識別子セグメントの連結で比較する
/// ため検出漏れしない (codex 指摘の raw text 比較弱点に対する対処)。
#[test]
fn java_flyway_migration_split_fqn_is_detected() {
    let src = "package db.migration;\n\
                   public class V2__SplitFqn extends org.flywaydb.core.api.migration.\n\
                       BaseJavaMigration {\n\
                       public void migrate(org.flywaydb.core.api.migration.Context context) throws Exception {}\n\
                   }\n";
    assert!(check_java_flyway_class(src, "V2__SplitFqn"));
}

/// Swift の cx は tree-sitter-swift 固有の分岐ノード
/// (guard_statement / switch_entry / repeat_while_statement / ternary_expression) も
/// 計上する (Issue 2026-07-10-swift-complexity-guard-switch-entry)。
/// base1 + if1 + guard1 + switch1 + entry3 + for1 + while1 = 9。
#[test]
fn swift_complexity_counts_guard_and_switch_entry() {
    let src = r#"
func complexFn(x: Int?) -> Int {
    var total = 0
    if x == nil { total += 1 }
    guard let v = x else { return -1 }
    switch v {
    case 1: total += 1
    case 2: total += 2
    default: total += 0
    }
    for i in 0..<v { total += i }
    while total > 100 { total -= 1 }
    return total
}
"#;
    // if 1 + guard 1 + switch_entry 3 (case1/case2/default) + for 1 + while 1 = 7 → cx 8。
    // `switch_statement` 本体は判定点ではないので数えない。
    assert_eq!(cx_of(src, LangId::Swift, "complexFn"), 8);
}

/// Swift の ternary / catch / repeat-while も分岐として数える。
/// base1 + ternary1 + catch1 + repeat1 = 4。
#[test]
fn swift_complexity_counts_ternary_catch_repeat() {
    let src = r#"
func risky() throws {}
func extraFn(x: Int?) -> Int {
    let v = x != nil ? 1 : 0
    do { try risky() } catch { return -1 }
    repeat { print("hi") } while v > 2
    return v
}
"#;
    assert_eq!(cx_of(src, LangId::Swift, "extraFn"), 4);
}

/// C の関数直前の block comment を doc として抽出する
/// (Issue 2026-07-10-c-cpp-doc-comment-extraction)。name capture の親
/// (function_declarator) の prev sibling は戻り型で comment に届かないため、
/// 昇格した function_definition 基準で拾う。
#[test]
fn c_function_doc_comment_extracted_from_definition() {
    let src = "/* adds two numbers */\nint add(int a, int b) {\n    return a + b;\n}\n";
    let syms = syms_of(src, LangId::C);
    let add = syms.iter().find(|s| s.name == "add").expect("add symbol");
    assert_eq!(add.doc.as_deref(), Some("/* adds two numbers */"));
}

/// C++ メソッド (ポインタ返り含む) でも定義直前の連続コメントを doc として拾う。
#[test]
fn cpp_method_doc_comment_extracted() {
    let src = "class W {\npublic:\n    // line one\n    // line two\n    void m() {}\n};\n";
    let syms = syms_of(src, LangId::Cpp);
    let m = syms.iter().find(|s| s.name == "m").expect("m symbol");
    assert_eq!(m.doc.as_deref(), Some("// line one\n// line two"));
}

/// C++ の template メンバ関数は `field_declaration_list > template_declaration >
/// function_definition` 構造で access_specifier が template_declaration の兄弟になる。
/// member_anchor から遡ることで public template メンバを公開と判定する
/// (Issue 2026-07-10-cpp-template-member-visibility)。
#[test]
fn cpp_template_member_visibility_respects_access_specifier() {
    let src = "class Widget {\npublic:\n    template<typename T> void render(T v) { (void)v; }\n    void plain() {}\nprivate:\n    template<typename T> void hidden(T v) { (void)v; }\n};\n";
    let language = LangId::Cpp.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let root = tree.root_node();
    let syms = syms_of(src, LangId::Cpp);
    let vis: std::collections::HashMap<&str, bool> = syms
        .iter()
        .map(|s| {
            (
                s.name.as_str(),
                is_symbol_exported(root, src.as_bytes(), LangId::Cpp, &s.range),
            )
        })
        .collect();
    assert_eq!(vis.get("render"), Some(&true), "public template member");
    assert_eq!(vis.get("plain"), Some(&true), "public plain member");
    assert_eq!(vis.get("hidden"), Some(&false), "private template member");
}

/// Ruby の `class A::B` は name capture が scope_resolution 内で range が名前に潰れる。
/// class ノードまで昇格して本体全体を range にし、配下メソッドの container 帰属を
/// 成立させる (Issue 2026-07-10-ruby-scoped-class-range)。
#[test]
fn ruby_scoped_class_range_spans_body_and_assigns_container() {
    let src = "class Admin::UsersController\n  def index\n    render\n  end\nend\n";
    let syms = syms_of(src, LangId::Ruby);
    let class_sym = syms
        .iter()
        .find(|s| s.name == "UsersController")
        .expect("class symbol");
    assert_eq!(class_sym.range.start.line, 0);
    assert_eq!(class_sym.range.end.line, 4, "range spans to `end`");
    let index = syms.iter().find(|s| s.name == "index").expect("index");
    assert_eq!(index.container.as_deref(), Some("UsersController"));
}

/// カスタムクエリ (`symbols --query`) は built-in を置換し、未知 capture は
/// INVALID_REQUEST を返す (Issue 2026-07-10-symbols-query-flag-silent-noop)。
#[test]
fn custom_query_replaces_builtin_and_validates_captures() {
    let src = "function target() {}\nexport const single = (x: number) => x;\n";
    let language = LangId::Typescript.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let root = tree.root_node();

    // 有効クエリ: function_declaration のみ抽出される
    let syms = extract_symbols_with_custom_query(
        root,
        src.as_bytes(),
        LangId::Typescript,
        "(function_declaration name: (identifier) @function.name)",
    )
    .expect("valid custom query");
    assert_eq!(syms.len(), 1);
    assert_eq!(syms[0].name, "target");

    // 不正クエリ / 未知 capture は INVALID_REQUEST
    for bad in ["(invalid_query", "(function_declaration) @bogus.capture"] {
        let err = extract_symbols_with_custom_query(root, src.as_bytes(), LangId::Typescript, bad)
            .expect_err("must be rejected");
        let ae = err
            .downcast_ref::<crate::error::AstroError>()
            .expect("AstroError");
        assert_eq!(ae.code, crate::error::ErrorCode::InvalidRequest);
    }
}

// --- 循環的複雑度: 言語横断の等価性契約 ---
//
// 同じロジックが言語によって違う cx になると、しきい値 (`cx > 10` 等) での判断も
// polyglot リポジトリでの比較も成立しない。branch_node_kinds は言語ごとに手書きの
// ノード名テーブルで、汎用スライスの流用や grammar の改称に追随できていないと
// 「1 つもマッチせず cx=1 のまま」という**エラーにならない壊れ方**をする。
// 実測で 9 系統の不整合 (同じ 3 分岐 switch が 2〜5 にばらつく等) が見つかったため、
// 論理的に等価なコードを全 tree-sitter 言語で並べて期待値に固定する。
//
// 期待値は McCabe (1 + 判定点数)。詳細な計上規約は `complexity.rs` の
// `branch_node_kinds` の doc コメントを参照。

/// 各言語の (LangId, 関数名, ソース) と McCabe 期待値のケース。
struct CxCase {
    lang: LangId,
    label: &'static str,
    src: &'static str,
}

fn assert_cx_cases(expected: usize, cases: &[CxCase]) {
    let mut mismatches: Vec<String> = Vec::new();
    for c in cases {
        let got = cx_of(c.src, c.lang, "f");
        if got != expected {
            mismatches.push(format!(
                "{:?}/{}: got {got}, want {expected}",
                c.lang, c.label
            ));
        }
    }
    assert!(
        mismatches.is_empty(),
        "言語間で cx が一致していない:\n  {}",
        mismatches.join("\n  ")
    );
}

/// `if` 1 個 = 判定点 1 → cx 2。
#[test]
fn cx_if_only_is_two_in_every_language() {
    assert_cx_cases(
        2,
        &[
            CxCase {
                lang: LangId::Rust,
                label: "if",
                src: "fn f(a: i32) -> i32 { if a > 0 { return 1; } 0 }",
            },
            CxCase {
                lang: LangId::C,
                label: "if",
                src: "int f(int a) { if (a > 0) { return 1; } return 0; }",
            },
            CxCase {
                lang: LangId::Cpp,
                label: "if",
                src: "int f(int a) { if (a > 0) { return 1; } return 0; }",
            },
            CxCase {
                lang: LangId::Python,
                label: "if",
                src: "def f(a):\n    if a > 0:\n        return 1\n    return 0\n",
            },
            CxCase {
                lang: LangId::Javascript,
                label: "if",
                src: "function f(a) { if (a > 0) { return 1; } return 0; }",
            },
            CxCase {
                lang: LangId::Typescript,
                label: "if",
                src: "function f(a: number): number { if (a > 0) { return 1; } return 0; }",
            },
            CxCase {
                lang: LangId::Go,
                label: "if",
                src: "package p\nfunc f(a int) int { if a > 0 { return 1 }\n return 0 }",
            },
            CxCase {
                lang: LangId::Php,
                label: "if",
                src: "<?php\nfunction f($a) { if ($a > 0) { return 1; } return 0; }",
            },
            CxCase {
                lang: LangId::Java,
                label: "if",
                src: "class C { int f(int a) { if (a > 0) { return 1; } return 0; } }",
            },
            CxCase {
                lang: LangId::Kotlin,
                label: "if",
                src: "fun f(a: Int): Int { if (a > 0) { return 1 }\n return 0 }",
            },
            CxCase {
                lang: LangId::Swift,
                label: "if",
                src: "func f(a: Int) -> Int {\n    if a > 0 {\n        return 1\n    }\n    return 0\n}",
            },
            CxCase {
                lang: LangId::CSharp,
                label: "if",
                src: "class C { int f(int a) { if (a > 0) { return 1; } return 0; } }",
            },
            CxCase {
                lang: LangId::Bash,
                label: "if",
                src: "f() {\n  if [ \"$1\" -gt 0 ]; then echo 1; fi\n}",
            },
            CxCase {
                lang: LangId::Ruby,
                label: "if",
                src: "def f(a)\n  if a > 0 then return 1 end\n  0\nend",
            },
            CxCase {
                lang: LangId::Zig,
                label: "if",
                src: "pub fn f(a: i32) i32 {\n    if (a > 0) { return 1; }\n    return 0;\n}",
            },
        ],
    );
}

/// plain `else` は判定点ではない → `if/else` も cx 2。
///
/// Rust / Zig だけ `else_clause` を分岐計上しており cx 3 を返していた。
#[test]
fn cx_plain_else_is_not_a_decision_point() {
    assert_cx_cases(
        2,
        &[
            CxCase {
                lang: LangId::Rust,
                label: "if_else",
                src: "fn f(a: i32) -> i32 { if a > 0 { 1 } else { 0 } }",
            },
            CxCase {
                lang: LangId::C,
                label: "if_else",
                src: "int f(int a) { if (a > 0) { return 1; } else { return 0; } }",
            },
            CxCase {
                lang: LangId::Python,
                label: "if_else",
                src: "def f(a):\n    if a > 0:\n        return 1\n    else:\n        return 0\n",
            },
            CxCase {
                lang: LangId::Javascript,
                label: "if_else",
                src: "function f(a) { if (a > 0) { return 1; } else { return 0; } }",
            },
            CxCase {
                lang: LangId::Go,
                label: "if_else",
                src: "package p\nfunc f(a int) int { if a > 0 { return 1 } else { return 0 } }",
            },
            CxCase {
                lang: LangId::Php,
                label: "if_else",
                src: "<?php\nfunction f($a) { if ($a > 0) { return 1; } else { return 0; } }",
            },
            CxCase {
                lang: LangId::Java,
                label: "if_else",
                src: "class C { int f(int a) { if (a > 0) { return 1; } else { return 0; } } }",
            },
            CxCase {
                lang: LangId::Kotlin,
                label: "if_else",
                src: "fun f(a: Int): Int { if (a > 0) { return 1 } else { return 0 } }",
            },
            CxCase {
                lang: LangId::Swift,
                label: "if_else",
                src: "func f(a: Int) -> Int {\n    if a > 0 {\n        return 1\n    } else {\n        return 0\n    }\n}",
            },
            CxCase {
                lang: LangId::CSharp,
                label: "if_else",
                src: "class C { int f(int a) { if (a > 0) { return 1; } else { return 0; } } }",
            },
            CxCase {
                lang: LangId::Bash,
                label: "if_else",
                src: "f() {\n  if [ \"$1\" -gt 0 ]; then echo 1; else echo 0; fi\n}",
            },
            CxCase {
                lang: LangId::Ruby,
                label: "if_else",
                src: "def f(a)\n  if a > 0 then 1 else 0 end\nend",
            },
            CxCase {
                lang: LangId::Zig,
                label: "if_else",
                src: "pub fn f(a: i32) i32 {\n    if (a > 0) { return 1; } else { return 0; }\n}",
            },
        ],
    );
}

/// `if / else if / else` = 判定点 2 → cx 3。
///
/// PHP は `else_if_clause` が未登録で cx 2 (過小)、Rust / Zig は `else_clause` の
/// 二重計上で cx 5 (過大) だった。
#[test]
fn cx_else_if_chain_counts_only_the_nested_conditions() {
    assert_cx_cases(
        3,
        &[
            CxCase {
                lang: LangId::Rust,
                label: "elif",
                src: "fn f(a: i32) -> i32 { if a > 0 { 1 } else if a < 0 { 2 } else { 0 } }",
            },
            CxCase {
                lang: LangId::C,
                label: "elif",
                src: "int f(int a) { if (a>0) { return 1; } else if (a<0) { return 2; } else { return 0; } }",
            },
            CxCase {
                lang: LangId::Python,
                label: "elif",
                src: "def f(a):\n    if a > 0:\n        return 1\n    elif a < 0:\n        return 2\n    else:\n        return 0\n",
            },
            CxCase {
                lang: LangId::Javascript,
                label: "elif",
                src: "function f(a) { if (a>0) { return 1; } else if (a<0) { return 2; } else { return 0; } }",
            },
            CxCase {
                lang: LangId::Go,
                label: "elif",
                src: "package p\nfunc f(a int) int { if a > 0 { return 1 } else if a < 0 { return 2 } else { return 0 } }",
            },
            CxCase {
                lang: LangId::Php,
                label: "elseif",
                src: "<?php\nfunction f($a) { if ($a>0) { return 1; } elseif ($a<0) { return 2; } else { return 0; } }",
            },
            CxCase {
                lang: LangId::Java,
                label: "elif",
                src: "class C { int f(int a) { if (a>0) { return 1; } else if (a<0) { return 2; } else { return 0; } } }",
            },
            CxCase {
                lang: LangId::Kotlin,
                label: "elif",
                src: "fun f(a: Int): Int { if (a>0) { return 1 } else if (a<0) { return 2 } else { return 0 } }",
            },
            CxCase {
                lang: LangId::CSharp,
                label: "elif",
                src: "class C { int f(int a) { if (a>0) { return 1; } else if (a<0) { return 2; } else { return 0; } } }",
            },
            CxCase {
                lang: LangId::Bash,
                label: "elif",
                src: "f() {\n  if [ \"$1\" -gt 0 ]; then echo 1; elif [ \"$1\" -lt 0 ]; then echo 2; else echo 0; fi\n}",
            },
            CxCase {
                lang: LangId::Ruby,
                label: "elsif",
                src: "def f(a)\n  if a > 0 then 1 elsif a < 0 then 2 else 0 end\nend",
            },
            CxCase {
                lang: LangId::Zig,
                label: "elif",
                src: "pub fn f(a: i32) i32 {\n    if (a > 0) { return 1; } else if (a < 0) { return 2; } else { return 0; }\n}",
            },
        ],
    );
}

/// switch/match の arm 3 個 = 判定点 3 → cx 4。**構文本体は数えない**。
///
/// 旧実装では本体 + arm の二重計上で 5 になる言語が 10、逆に Java は arm を
/// 1 つも数えず 2 だった (3 分岐が cx 2 という実態と逆の値)。
#[test]
fn cx_switch_counts_arms_not_the_construct() {
    assert_cx_cases(
        4,
        &[
            CxCase {
                lang: LangId::Rust,
                label: "match",
                src: "fn f(a: i32) -> i32 { match a { 1 => 1, 2 => 2, _ => 3 } }",
            },
            CxCase {
                lang: LangId::C,
                label: "switch",
                src: "int f(int a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; } return 0; }",
            },
            CxCase {
                lang: LangId::Cpp,
                label: "switch",
                src: "int f(int a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; } return 0; }",
            },
            CxCase {
                lang: LangId::Python,
                label: "match",
                src: "def f(a):\n    match a:\n        case 1: return 1\n        case 2: return 2\n        case _: return 3\n",
            },
            CxCase {
                lang: LangId::Javascript,
                label: "switch",
                src: "function f(a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; } return 0; }",
            },
            CxCase {
                lang: LangId::Typescript,
                label: "switch",
                src: "function f(a: number): number { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; } return 0; }",
            },
            CxCase {
                lang: LangId::Go,
                label: "switch",
                src: "package p\nfunc f(a int) int { switch a { case 1: return 1; case 2: return 2; case 3: return 3 }\n return 0 }",
            },
            CxCase {
                lang: LangId::Php,
                label: "switch",
                src: "<?php\nfunction f($a) { switch ($a) { case 1: return 1; case 2: return 2; case 3: return 3; } return 0; }",
            },
            CxCase {
                lang: LangId::Java,
                label: "switch",
                src: "class C { int f(int a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; } return 0; } }",
            },
            CxCase {
                lang: LangId::Kotlin,
                label: "when",
                src: "fun f(a: Int): Int { return when (a) { 1 -> 1; 2 -> 2; else -> 3 } }",
            },
            CxCase {
                lang: LangId::Swift,
                label: "switch",
                src: "func f(a: Int) -> Int {\n    switch a {\n    case 1:\n        return 1\n    case 2:\n        return 2\n    default:\n        return 3\n    }\n}",
            },
            CxCase {
                lang: LangId::CSharp,
                label: "switch",
                src: "class C { int f(int a) { switch (a) { case 1: return 1; case 2: return 2; case 3: return 3; } return 0; } }",
            },
            CxCase {
                lang: LangId::Bash,
                label: "case",
                src: "f() {\n  case \"$1\" in 1) echo 1;; 2) echo 2;; 3) echo 3;; esac\n}",
            },
            CxCase {
                lang: LangId::Ruby,
                label: "case",
                src: "def f(a)\n  case a when 1 then 1 when 2 then 2 when 3 then 3 end\nend",
            },
            CxCase {
                lang: LangId::Zig,
                label: "switch",
                src: "pub fn f(a: i32) i32 {\n    return switch (a) {\n        1 => 1,\n        2 => 2,\n        else => 3,\n    };\n}",
            },
        ],
    );
}

/// Java の arrow 構文 switch (Java 14+) も colon 構文と同じ arm 数で数える。
/// `switch_label` は両構文に共通して 1 arm = 1 個現れる。
#[test]
fn cx_java_arrow_switch_matches_colon_switch() {
    let arrow = "class C { int f(int a) { return switch (a) { case 1 -> 1; case 2 -> 2; default -> 0; }; } }";
    assert_eq!(cx_of(arrow, LangId::Java, "f"), 4);
}

/// Java の fall-through は label 数 = 進入経路数で数える。
#[test]
fn cx_java_fallthrough_counts_each_label() {
    let src =
        "class C { int f(int a) { switch (a) { case 1: case 2: return 1; default: return 0; } } }";
    // label 3 個 (case 1 / case 2 / default) → cx 4
    assert_eq!(cx_of(src, LangId::Java, "f"), 4);
}

/// 三項演算子は判定点 → cx 2。PHP / Java / C# / Ruby で欠落していた。
#[test]
fn cx_ternary_is_counted_in_every_language_that_has_one() {
    assert_cx_cases(
        2,
        &[
            CxCase {
                lang: LangId::C,
                label: "ternary",
                src: "int f(int a) { return a > 0 ? 1 : 0; }",
            },
            CxCase {
                lang: LangId::Cpp,
                label: "ternary",
                src: "int f(int a) { return a > 0 ? 1 : 0; }",
            },
            CxCase {
                lang: LangId::Python,
                label: "ternary",
                src: "def f(a):\n    return 1 if a > 0 else 0\n",
            },
            CxCase {
                lang: LangId::Javascript,
                label: "ternary",
                src: "function f(a) { return a > 0 ? 1 : 0; }",
            },
            CxCase {
                lang: LangId::Typescript,
                label: "ternary",
                src: "function f(a: number): number { return a > 0 ? 1 : 0; }",
            },
            CxCase {
                lang: LangId::Php,
                label: "ternary",
                src: "<?php\nfunction f($a) { return $a > 0 ? 1 : 0; }",
            },
            CxCase {
                lang: LangId::Java,
                label: "ternary",
                src: "class C { int f(int a) { return a > 0 ? 1 : 0; } }",
            },
            CxCase {
                lang: LangId::CSharp,
                label: "ternary",
                src: "class C { int f(int a) { return a > 0 ? 1 : 0; } }",
            },
            CxCase {
                lang: LangId::Swift,
                label: "ternary",
                src: "func f(a: Int) -> Int {\n    return a > 0 ? 1 : 0\n}",
            },
            CxCase {
                lang: LangId::Ruby,
                label: "ternary",
                src: "def f(a)\n  a > 0 ? 1 : 0\nend",
            },
        ],
    );
}

/// C# の `foreach` は `foreach_statement` (旧テーブルの `for_each_statement` は
/// 存在しないノード名で 1 つもマッチせず cx=1 のままだった)。
#[test]
fn cx_csharp_foreach_is_counted() {
    let src = "class C { int f(int a) { foreach (var z in new int[]{1}) { a += z; } return a; } }";
    assert_eq!(cx_of(src, LangId::CSharp, "f"), 2);
}

/// ループは全言語で判定点 1 → cx 2。
#[test]
fn cx_loop_is_two_in_every_language() {
    assert_cx_cases(
        2,
        &[
            CxCase {
                lang: LangId::Rust,
                label: "for",
                src: "fn f(a: i32) -> i32 { for _ in 0..3 {} a }",
            },
            CxCase {
                lang: LangId::C,
                label: "for",
                src: "int f(int a) { for (int i=0;i<3;i++) {} return a; }",
            },
            CxCase {
                lang: LangId::Python,
                label: "for",
                src: "def f(a):\n    for i in range(3):\n        pass\n    return a\n",
            },
            CxCase {
                lang: LangId::Javascript,
                label: "for",
                src: "function f(a) { for (let i=0;i<3;i++) {} return a; }",
            },
            CxCase {
                lang: LangId::Go,
                label: "for",
                src: "package p\nfunc f(a int) int { for i:=0;i<3;i++ {}\n return a }",
            },
            CxCase {
                lang: LangId::Php,
                label: "foreach",
                src: "<?php\nfunction f($a) { foreach ([1] as $z) {} return $a; }",
            },
            CxCase {
                lang: LangId::Java,
                label: "for",
                src: "class C { int f(int a) { for (int i=0;i<3;i++) {} return a; } }",
            },
            CxCase {
                lang: LangId::Kotlin,
                label: "for",
                src: "fun f(a: Int): Int { for (i in 0..2) {}\n return a }",
            },
            CxCase {
                lang: LangId::Swift,
                label: "for",
                src: "func f(a: Int) -> Int {\n    for _ in 0..<3 {}\n    return a\n}",
            },
            CxCase {
                lang: LangId::CSharp,
                label: "for",
                src: "class C { int f(int a) { for (int i=0;i<3;i++) {} return a; } }",
            },
            CxCase {
                lang: LangId::Bash,
                label: "for",
                src: "f() {\n  for i in 1 2 3; do echo \"$i\"; done\n}",
            },
            CxCase {
                lang: LangId::Ruby,
                label: "while",
                src: "def f(a)\n  while a > 0 do break end\n  a\nend",
            },
            CxCase {
                lang: LangId::Zig,
                label: "while",
                src: "pub fn f(a: i32) i32 {\n    var i: usize = 0;\n    while (i < 3) : (i += 1) {}\n    return a;\n}",
            },
        ],
    );
}

// --- Zig: 宣言と代入の区別 / ローカルスコープ ---

/// tree-sitter-zig は代入式 `x += y;` も `variable_declaration` として parse する。
/// `const` / `var` キーワードを先頭アンカーで要求しないと、代入先が宣言として
/// 二重に出るうえ、直下 identifier を全て拾うので右辺まで宣言扱いになる。
#[test]
fn zig_assignment_is_not_extracted_as_declaration() {
    let src = "pub fn add(a: i32, b: i32) i32 {\n    const total = a + b;\n    var scratch: i32 = 0;\n    scratch += total;\n    return scratch;\n}\n";
    let syms = syms_of(src, LangId::Zig);
    let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["add", "total", "scratch"],
        "代入行 `scratch += total;` から宣言シンボルを作らない: {names:?}"
    );
}

/// Zig の関数内 `const` / `var` はローカルスコープと判定する
/// (impact のクロスファイル起点から除外するため)。
#[test]
fn zig_function_local_variable_is_local_scope() {
    let src = "const TOP: i32 = 1;\npub fn add(a: i32) i32 {\n    const inner = a + 1;\n    return inner;\n}\n";
    let language = LangId::Zig.ts_language();
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).unwrap();
    let tree = parser.parse(src, None).unwrap();
    let root = tree.root_node();
    let syms = extract_symbols(root, src.as_bytes(), LangId::Zig).unwrap();

    let inner = syms.iter().find(|s| s.name == "inner").expect("inner");
    assert!(
        is_local_scope_symbol(root, src.as_bytes(), LangId::Zig, &inner.range),
        "関数内の const はローカル"
    );
    let top = syms.iter().find(|s| s.name == "TOP").expect("TOP");
    assert!(
        !is_local_scope_symbol(root, src.as_bytes(), LangId::Zig, &top.range),
        "ファイルトップレベルの const はローカルではない"
    );
}

// --- Go: メソッドのレシーバ型を container にする ---

/// Go のメソッドはレシーバ型が container だが、宣言はトップレベルに並ぶため
/// range 内包ベースの `assign_enclosing_containers` では引き当てられない。
/// container が空だと同名メソッドが全て bare 名に潰れ、api 差分の突き合わせや
/// dead-code の帰属が壊れる。
#[test]
fn go_method_receiver_becomes_container() {
    let src = "package p\n\ntype Alpha struct{ v int }\ntype Beta struct{ v int }\n\nfunc (a Alpha) Value() int { return a.v }\nfunc (b *Beta) Value() int { return b.v }\nfunc Free() int { return 0 }\n";
    let syms = syms_of(src, LangId::Go);
    let containers: Vec<(&str, Option<&str>)> = syms
        .iter()
        .filter(|s| matches!(s.kind, SymbolKind::Method | SymbolKind::Function))
        .map(|s| (s.name.as_str(), s.container.as_deref()))
        .collect();
    assert!(
        containers.contains(&("Value", Some("Alpha"))),
        "値レシーバの container は Alpha: {containers:?}"
    );
    assert!(
        containers.contains(&("Value", Some("Beta"))),
        "ポインタレシーバでも container は Beta (`*` を剥がす): {containers:?}"
    );
    assert!(
        containers.contains(&("Free", None)),
        "レシーバの無い関数は container なし: {containers:?}"
    );
}

/// ジェネリックなレシーバ (`*Stack[T]`) でも型名だけを container にする。
#[test]
fn go_generic_receiver_container_strips_type_params() {
    let src = "package p\n\ntype Stack[T any] struct{ items []T }\n\nfunc (s *Stack[T]) Push(x T) { s.items = append(s.items, x) }\n";
    let syms = syms_of(src, LangId::Go);
    let push = syms.iter().find(|s| s.name == "Push").expect("Push");
    assert_eq!(push.container.as_deref(), Some("Stack"));
}
