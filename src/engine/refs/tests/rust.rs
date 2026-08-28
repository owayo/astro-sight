use super::*;

/// Rust の `pub fn` と struct field が同名のとき、フィールドアクセスや
/// struct 宣言・初期化を関数参照として誤マッチしないことを検証
/// (Issue: 2026-05-21-redact-impact-triage)
#[test]
fn find_references_rust_function_excludes_same_name_struct_fields() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.rs");
    std::fs::write(
        &a,
        r#"pub struct Cfg {
pub redact: bool,
}

pub fn redact(input: &str) -> String {
input.to_string()
}

fn build(flag: bool) -> Cfg {
Cfg { redact: flag }
}

fn build_short() -> Cfg {
let redact = true;
Cfg { redact }
}

fn caller(cfg: &Cfg, data: &str) {
if cfg.redact {
    let _ = redact(data);
}
}
"#,
    )
    .unwrap();

    let refs = find_references("redact", dir.path(), Some("**/*.rs")).unwrap();
    let kinds: Vec<_> = refs.iter().map(|r| (r.line, r.kind)).collect();

    // 期待:
    // - L4 (`pub fn redact`) — Definition
    // - L18 (`let _ = redact(data)`) — Reference (関数呼び出し)
    // それ以外のフィールド系 (L1=struct field 宣言, L9=field_initializer,
    // L13=`let redact = true;` の binding ではなく、`Cfg { redact }` の shorthand,
    // L16=`cfg.redact` の field_expression) は含まれないこと
    assert!(
        kinds.iter().any(|(_, k)| *k == Some(RefKind::Definition)),
        "関数定義が含まれること: kinds={kinds:?}"
    );
    let refs_text: Vec<&str> = refs.iter().filter_map(|r| r.context.as_deref()).collect();
    // 関数呼び出しの行は含まれる
    assert!(
        refs_text.iter().any(|c| c.contains("redact(data)")),
        "関数呼び出し redact(data) は含まれるべき: {refs_text:?}"
    );
    // 純粋なフィールドアクセス / 宣言 / 初期化系は含まれない
    assert!(
        !refs_text.iter().any(|c| c.contains("pub redact: bool")),
        "struct field 宣言 'pub redact: bool' は除外されるべき: {refs_text:?}"
    );
    assert!(
        !refs_text.iter().any(|c| c.trim() == "redact: flag,"),
        "field_initializer 'redact: flag' は除外されるべき: {refs_text:?}"
    );
    assert!(
        !refs_text.iter().any(|c| c.contains("Cfg { redact }")),
        "shorthand 'Cfg {{ redact }}' は除外されるべき: {refs_text:?}"
    );
    assert!(
        !refs_text.iter().any(|c| c.contains("cfg.redact")),
        "field_expression 'cfg.redact' は除外されるべき: {refs_text:?}"
    );
}

/// destructuring pattern (`let Cfg { redact: v } = ...`) の field name も
/// 関数参照として誤マッチしないことを検証
/// (codex コミット前レビューでの追加指摘)
#[test]
fn find_references_rust_function_excludes_field_pattern() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.rs");
    std::fs::write(
        &a,
        r#"pub struct Cfg { pub redact: bool }
pub fn redact(input: &str) -> String { input.to_string() }
fn caller(cfg: Cfg, data: &str) {
let Cfg { redact: value } = cfg;
if value {
    let _ = redact(data);
}
}
"#,
    )
    .unwrap();

    let refs = find_references("redact", dir.path(), Some("**/*.rs")).unwrap();
    let texts: Vec<&str> = refs.iter().filter_map(|r| r.context.as_deref()).collect();
    assert!(
        !texts
            .iter()
            .any(|c| c.contains("let Cfg { redact: value }")),
        "field_pattern の name 部は除外されるべき: {texts:?}"
    );
    assert!(
        texts.iter().any(|c| c.contains("redact(data)")),
        "関数呼び出しは残るべき: {texts:?}"
    );
}

/// メソッド呼び出し `obj.method()` の `method` 部は関数参照として残ることを検証
#[test]
fn find_references_rust_method_call_field_identifier_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.rs");
    std::fs::write(
        &a,
        r#"struct S;
impl S {
fn run(&self) {}
}
fn caller(s: &S) {
s.run();
}
"#,
    )
    .unwrap();

    let refs = find_references("run", dir.path(), Some("**/*.rs")).unwrap();
    let texts: Vec<&str> = refs.iter().filter_map(|r| r.context.as_deref()).collect();
    // 定義 (`fn run(&self) {}`) + メソッド呼び出し (`s.run();`) の 2 件
    assert!(
        texts.iter().any(|c| c.contains("s.run()")),
        "method call s.run() は関数参照として残るべき: {texts:?}"
    );
    assert!(
        texts.iter().any(|c| c.contains("fn run(&self)")),
        "定義 fn run は残るべき: {texts:?}"
    );
}

/// 単一 refs 検索が複数ファイルを横断し、定義を先頭に返すことを検証
#[test]
fn find_references_single_search_sorts_definition_first() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.rs");
    let b = dir.path().join("b.rs");
    std::fs::write(&a, "pub fn greet() {}\nfn main() { greet(); }\n").unwrap();
    std::fs::write(&b, "fn other() { crate::greet(); }\n").unwrap();

    let refs = find_references("greet", dir.path(), Some("**/*.rs")).unwrap();

    assert_eq!(refs.len(), 3);
    assert_eq!(refs[0].kind, Some(RefKind::Definition));
    assert_eq!(refs[0].line, 0);
    assert!(
        refs[1..]
            .iter()
            .all(|r| r.kind != Some(RefKind::Definition))
    );
}

/// `split_path_segments` が "::" 区切りの各セグメントとバイトオフセットを返すことを検証
#[test]
fn split_path_segments_basic() {
    assert_eq!(split_path_segments("foo"), vec![("foo", 0)]);
    assert_eq!(
        split_path_segments("Option::is_none"),
        vec![("Option", 0), ("is_none", 8)]
    );
    assert_eq!(
        split_path_segments("a::b::c"),
        vec![("a", 0), ("b", 3), ("c", 6)]
    );
    assert!(split_path_segments("").is_empty());
}

/// ヘルパー: Rust ソースを tree-sitter でパースしてツリーを返す
fn parse_rust(source: &str) -> tree_sitter::Tree {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .expect("load rust language");
    parser.parse(source, None).expect("parse rust source")
}

/// serde の serialize_with = "..." 内の関数名が参照として収集されることを検証
#[test]
fn rust_attr_string_ref_detected_for_serialize_with() {
    let source = r#"
fn serialize_jst() {}
struct Foo;
impl Foo {
fn placeholder() {}
}
#[derive(Serialize)]
struct Bar {
#[serde(serialize_with = "serialize_jst")]
time: i64,
}
"#;
    let tree = parse_rust(source);
    let defs = definition_node_kinds(LangId::Rust);
    let refs = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "serialize_jst",
        "test.rs",
        defs,
        LangId::Rust,
    );

    // 定義 1 件 + 属性文字列内参照 1 件
    let def_cnt = refs
        .iter()
        .filter(|r| matches!(r.kind, Some(RefKind::Definition)))
        .count();
    let ref_cnt = refs
        .iter()
        .filter(|r| matches!(r.kind, Some(RefKind::Reference)))
        .count();
    assert_eq!(def_cnt, 1, "definition should be captured");
    assert_eq!(ref_cnt, 1, "serde attribute string ref should be captured");
}

/// 属性文字列参照が非 Definition としてカウントされ、dead-code 判定に反映されることを検証
#[test]
fn rust_attr_string_ref_counted_as_non_definition() {
    let source = r#"
fn serialize_jst() {}
#[derive(Serialize)]
struct Bar {
#[serde(serialize_with = "serialize_jst")]
time: i64,
}
"#;
    let tree = parse_rust(source);
    let defs = definition_node_kinds(LangId::Rust);
    let symbol_names = vec!["serialize_jst".to_string()];
    let counts = count_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        &symbol_names,
        defs,
        LangId::Rust,
        1,
    );
    assert_eq!(counts[0], 1, "attribute string ref must lift dead-code");
}

/// `Option::is_none` のようなパス文字列では最終セグメントもカウントされることを検証
#[test]
fn rust_attr_string_ref_path_segments() {
    let source = r#"
#[derive(Serialize)]
struct Bar {
#[serde(skip_serializing_if = "Option::is_none")]
inner: Option<i64>,
}
"#;
    let tree = parse_rust(source);
    let defs = definition_node_kinds(LangId::Rust);
    let refs = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "is_none",
        "test.rs",
        defs,
        LangId::Rust,
    );
    assert_eq!(
        refs.len(),
        1,
        "path tail segment should be matched as reference"
    );
}

/// 対象外キー (例: rename) の文字列値は参照として扱わないことを検証
#[test]
fn rust_attr_string_ref_ignores_non_ref_keys() {
    let source = r#"
#[derive(Serialize)]
struct Bar {
#[serde(rename = "created_at")]
time: i64,
}
"#;
    let tree = parse_rust(source);
    let defs = definition_node_kinds(LangId::Rust);
    let refs = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "created_at",
        "test.rs",
        defs,
        LangId::Rust,
    );
    assert!(
        refs.is_empty(),
        "rename is not a reference key and must not match"
    );
}

/// 非 Rust 言語では属性文字列ヒューリスティックが動作しないことを検証
#[test]
fn rust_attr_helper_is_noop_for_other_languages() {
    // Python AST 上に string_content が登場しても反応しない
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .expect("load python language");
    let source = "x = \"serialize_jst\"\n";
    let tree = parser.parse(source, None).unwrap();
    let segs = collect_all_attr_segments(tree.root_node(), source.as_bytes(), LangId::Python);
    assert!(segs.is_empty());
}

/// ヘルパー: 木全体で rust_attr_string_ref_segments が拾うセグメントを再帰収集
fn collect_all_attr_segments<'a>(
    node: Node<'a>,
    source: &'a [u8],
    lang_id: LangId,
) -> Vec<(String, usize, usize)> {
    let mut out: Vec<(String, usize, usize)> = rust_attr_string_ref_segments(node, source, lang_id)
        .into_iter()
        .map(|(s, r, c)| (s.to_string(), r, c))
        .collect();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        out.extend(collect_all_attr_segments(child, source, lang_id));
    }
    out
}

/// closure パラメータが束縛する名前は、束縛位置も closure 本体の使用も参照ではない。
///
/// `refs` は名前一致だけで数えるため、`|(_, tail)| tail` のようなローカル束縛が
/// 同名の関数をシャドーイングしていると本番参照ゼロの関数が「参照あり」に見え、
/// dead-code が死蔵コードを live と誤認していた (fail-open)。
/// single refs と count-only (dead-code 経路) の分類一致まで固定する。
#[test]
fn rust_closure_bound_identifiers_are_not_references() {
    // (説明, ソース, 期待 def 数, 期待 ref 数)
    let cases: &[(&str, &str, usize, usize)] = &[
        (
            "tuple pattern",
            "pub fn tail(path: &str) -> &str {\n    path.rsplit_once('/').map_or(path, |(_, tail)| tail)\n}\n",
            1,
            0,
        ),
        (
            "plain pattern",
            "pub fn tail() -> u8 { 0 }\npub fn run(xs: &[u8]) -> u8 { xs.iter().map(|tail| *tail).max().unwrap_or(0) }\n",
            1,
            0,
        ),
        (
            "reference pattern",
            "pub fn tail() -> u8 { 0 }\npub fn run(xs: &[u8]) -> u8 { xs.iter().map(|&tail| tail).max().unwrap_or(0) }\n",
            1,
            0,
        ),
        (
            "typed parameter",
            "pub fn tail() -> u8 { 0 }\npub fn run() -> u8 { let f = |tail: u8| tail; f(1) }\n",
            1,
            0,
        ),
        (
            "struct shorthand pattern",
            "pub fn tail() -> u8 { 0 }\npub struct P { tail: u8 }\npub fn run(p: P) -> u8 { Some(p).map(|P { tail }| tail).unwrap_or(0) }\n",
            1,
            0,
        ),
        (
            "struct renamed pattern binds the value side only",
            "pub fn tail() -> u8 { 0 }\npub struct P { key: u8 }\npub fn run(p: P) -> u8 { Some(p).map(|P { key: tail }| tail).unwrap_or(0) }\n",
            1,
            0,
        ),
        (
            "nested closure inherits the outer binding",
            "pub fn tail() -> u8 { 0 }\npub fn run() -> u8 { let f = |tail: u8| (move || tail)(); f(1) }\n",
            1,
            0,
        ),
        // 対照: closure の外にある同名参照は従来どおり数える
        (
            "call outside the closure stays a reference",
            "pub fn tail() -> u8 { 0 }\npub fn run() -> u8 { let f = |x: u8| x; f(tail()) }\n",
            1,
            1,
        ),
        // 対照: 束縛していない名前は closure 内でも参照のまま
        (
            "unbound name inside a closure stays a reference",
            "pub fn tail() -> u8 { 0 }\npub fn run() -> u8 { let f = |x: u8| x + tail(); f(1) }\n",
            1,
            1,
        ),
        // 対照: パターンの型名は束縛ではないので参照のまま (引数型 + パターン型名の 2 件)
        (
            "tuple struct pattern type name stays a reference",
            "pub struct Tail(u8);\npub fn run(t: Tail) -> u8 { Some(t).map(|Tail(v)| v).unwrap_or(0) }\n",
            1,
            2,
        ),
        // 対照: 修飾パス経由の呼び出しはローカル束縛にシャドーイングされない。
        // ここを消すと live symbol を dead と誤判定する (逆向きの事故)。
        (
            "crate-qualified call inside the closure stays a reference",
            "pub fn tail() -> u8 { 0 }\npub fn run() -> u8 { let f = |tail: u8| tail + crate::tail(); f(1) }\n",
            1,
            1,
        ),
        // 対照: メソッド呼び出しの名前 (field_identifier) も束縛の対象外
        (
            "method call inside the closure stays a reference",
            "pub struct H;\nimpl H { pub fn tail(&self) -> u8 { 0 } }\npub fn run(h: H) -> u8 { let f = |tail: u8| tail + h.tail(); f(1) }\n",
            1,
            1,
        ),
        // 対照: 型注釈位置の識別子も束縛の対象外 (型と値で名前空間が別)。
        // フィールドを持つ struct にするのは、単位構造体だとパターン位置で定数パターンに
        // なり得て束縛判定自体が諦められ、型注釈の検証にならないため。
        (
            "type annotation with the same name stays a reference",
            "pub struct tail { v: u8 }\npub fn run() -> u8 { let f = |tail: tail| { let _ = tail; 0u8 }; f(tail { v: 0 }) }\n",
            1,
            2,
        ),
        // 対照: 単位構造体パターンは束縛ではないので参照のまま
        (
            "unit struct pattern stays a reference",
            "pub struct Unit;\npub fn run() { let _f = |Unit| (); }\n",
            1,
            1,
        ),
        // 対照: leading underscore 付きの単位構造体も束縛ではない。
        // 先頭 1 文字だけを見ると `_` が小文字扱いになり、参照を消してしまう。
        (
            "underscore-prefixed unit struct pattern stays a reference",
            "pub struct _Unit;\npub fn run() { let _f = |_Unit| (); }\n",
            1,
            1,
        ),
        // 対照: 小文字名の定数パターン。命名 lint は強制ではないので大小文字だけでは
        // 束縛と証明できない。同一ファイルに同名 const があれば束縛判定を諦める。
        (
            "lowercase const pattern stays a reference",
            "#![allow(non_upper_case_globals)]\npub const tail: () = ();\npub fn run() { let f = |tail: ()| (); f(()); }\n",
            1,
            1,
        ),
        // 対照: 小文字名の単位構造体も同様
        (
            "lowercase unit struct pattern stays a reference",
            "pub struct tail;\npub fn run() { let f = |tail| { let _ = tail; }; f(tail); }\n",
            1,
            3,
        ),
        // 対照: `use` で持ち込まれた名前は外部の const / unit struct かもしれない
        (
            "imported name in a pattern stays a reference",
            "use crate::other::tail;\npub fn run() { let f = |tail| { let _ = tail; }; f(tail); }\n",
            0,
            4,
        ),
        // 対照: glob import は持ち込む名前が AST に現れないため、対象名が定数パターン
        // でないことを確定できない。ファイル内に宣言も名前付き use も無い形で再現する。
        (
            "glob import makes pattern names unresolvable",
            "use crate::other::*;\npub fn run() { let f = |tail| { let _ = tail; }; f(tail); }\n",
            0,
            3,
        ),
        // 対照: マクロ名は値とは別の名前空間なので値束縛にシャドーイングされない。
        // `macro_rules!` は Rust の definition_node_kinds に無いため定義は 0 件で、
        // 宣言名と `tail!()` 呼び出しの 2 件が参照として残る (closure 束縛の
        // `|tail: u8|` とその使用だけが除外される)。
        (
            "macro invocation inside the closure stays a reference",
            "#[macro_export]\nmacro_rules! tail { () => { 0u8 } }\npub fn run() -> u8 { let f = |tail: u8| tail + tail!(); f(1) }\n",
            0,
            2,
        ),
    ];

    for (label, source, want_def, want_ref) in cases {
        let name = match *label {
            l if l.starts_with("tuple struct pattern type") => "Tail",
            l if l.starts_with("unit struct pattern") => "Unit",
            l if l.starts_with("underscore-prefixed unit struct") => "_Unit",
            _ => "tail",
        };
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).expect("parse");
        let defs = definition_node_kinds(LangId::Rust);
        let refs = collect_single_refs_for_test(
            tree.root_node(),
            source.as_bytes(),
            name,
            "test.rs",
            defs,
            LangId::Rust,
        );

        let def_cnt = refs
            .iter()
            .filter(|r| matches!(r.kind, Some(RefKind::Definition)))
            .count();
        let ref_cnt = refs
            .iter()
            .filter(|r| matches!(r.kind, Some(RefKind::Reference)))
            .count();
        assert_eq!(def_cnt, *want_def, "{label}: 定義数: {refs:?}");
        assert_eq!(ref_cnt, *want_ref, "{label}: 参照数: {refs:?}");

        let symbol_names = vec![name.to_string()];
        let counts = count_refs_for_test(
            tree.root_node(),
            source.as_bytes(),
            &symbol_names,
            defs,
            LangId::Rust,
            1,
        );
        assert_eq!(
            counts[0], *want_ref,
            "{label}: count-only 経路も同じ分類になること"
        );
    }
}

/// 束縛判定のメモが walk を跨いで再利用されない (前ファイルの判定を誤用しない)。
///
/// 判定結果を `source` のポインタ+長さでキャッシュすると、allocator が同サイズの
/// 次ファイルへ同じアドレスを再利用したときに前ファイルの結果を引いてしまい、
/// 「glob import が無い」と記録された判定を glob import のあるファイルに適用して
/// 本物の参照を消す。メモは walk 1 回の寿命に閉じる必要がある。
///
/// 同一の `Vec<u8>` バッファを同じ長さの 2 ソースで上書きして別々に walk し、
/// glob なし → glob あり / glob あり → glob なし の両順で検証する。
#[test]
fn rust_pattern_binding_memo_does_not_leak_across_walks() {
    // 同じ長さ・同じ名前 `tail` を含み、glob import の有無だけが違う 2 ソース。
    let with_glob =
        "use crate::o::*;\npub fn run() { let f = |tail| { let _ = tail; }; f(tail); }\n";
    let no_glob = "use crate::o::t;\npub fn run() { let f = |tail| { let _ = tail; }; f(tail); }\n";
    assert_eq!(
        with_glob.len(),
        no_glob.len(),
        "同じバッファ長で上書きするテスト前提"
    );

    // glob あり: 束縛判定を諦めるので closure 内の 2 件も参照に残る (計 3)
    // glob なし: `tail` の宣言も名前付き use も無いので束縛と判定し、closure 外の 1 件のみ
    let expected = |src: &str| if src == with_glob { 3 } else { 1 };

    for order in [[no_glob, with_glob], [with_glob, no_glob]] {
        // 同一バッファを使い回してアドレス再利用の状況を作る
        let mut buf: Vec<u8> = vec![0; with_glob.len()];
        for src in order {
            buf.copy_from_slice(src.as_bytes());
            let tree = parser::parse_source(&buf, LangId::Rust).expect("parse");
            let defs = definition_node_kinds(LangId::Rust);
            let refs = collect_single_refs_for_test(
                tree.root_node(),
                &buf,
                "tail",
                "test.rs",
                defs,
                LangId::Rust,
            );
            let ref_cnt = refs
                .iter()
                .filter(|r| matches!(r.kind, Some(RefKind::Reference)))
                .count();
            assert_eq!(
                ref_cnt,
                expected(src),
                "同一バッファを使い回しても前の walk の判定を引き継がない (src={src:?}): {refs:?}"
            );
        }
    }
}
