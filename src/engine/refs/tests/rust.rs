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
    use std::borrow::Cow;
    use std::collections::HashMap;

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
    let mut name_to_ix: HashMap<Cow<'_, str>, Vec<usize>> = HashMap::new();
    name_to_ix.insert(Cow::Borrowed("serialize_jst"), vec![0]);
    let counts = count_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        &name_to_ix,
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
