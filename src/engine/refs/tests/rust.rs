use super::*;

#[test]
fn rust_nfc_references_keep_spelling_and_byte_columns_in_every_scan() {
    let source = "pub fn caf\u{e9}() {}\nfn call() { cafe\u{301}(); }\n";
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("lib.rs");
    std::fs::write(&file, source).unwrap();
    let path = camino::Utf8Path::from_path(&file).unwrap();
    let names = vec![
        "caf\u{e9}".to_owned(),
        "cafe\u{301}".to_owned(),
        "cafe".to_owned(),
    ];
    let acs = build_batch_acs(&names).unwrap();
    let batch = find_refs_batch_in_file_indexed::<true>(&names, &acs, path).unwrap();
    let counts = count_refs_in_file(&names, &acs, path).unwrap();
    let old = SourceReferenceBatch::new(&names)
        .unwrap()
        .scan(path, source.as_bytes())
        .unwrap();
    for (index, name) in names.iter().enumerate() {
        let prefilter = SingleNamePrefilter::new(name).unwrap();
        let single = find_refs_in_file(name, path, &prefilter).unwrap();
        let expected = if index < 2 { 2 } else { 0 };
        assert_eq!(single.len(), expected, "{name}: {single:?}");
        assert_eq!(batch[index].len(), expected, "{name}: {:?}", batch[index]);
        assert_eq!(old.references[index].len(), expected, "{name}");
        assert_eq!(counts[index], usize::from(index < 2), "{name}");
        if index < 2 {
            let call = single.iter().find(|reference| reference.line == 1).unwrap();
            assert_eq!(call.column, 12);
            assert_eq!(
                call.context.as_deref(),
                Some("fn call() { cafe\u{301}(); }")
            );
        }
    }
    std::fs::write(&file, "fn K() {}\nfn call() { K(); }\n").unwrap();
    let prefilter = SingleNamePrefilter::new("\u{212a}").unwrap();
    assert_eq!(
        find_refs_in_file("\u{212a}", path, &prefilter)
            .unwrap()
            .len(),
        2
    );
    std::fs::write(&file, "fn call() { println!(\"{cafe\\u{301}}\"); }\n").unwrap();
    let names = vec!["caf\u{e9}".to_owned()];
    let acs = build_batch_acs(&names).unwrap();
    let prefilter = SingleNamePrefilter::new(&names[0]).unwrap();
    assert_eq!(
        find_refs_in_file(&names[0], path, &prefilter)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        find_refs_batch_in_file_indexed::<true>(&names, &acs, path).unwrap()[0].len(),
        1
    );
    assert_eq!(count_refs_in_file(&names, &acs, path).unwrap(), [1]);
}

/// cfg 条件のキーは同名の Rust シンボルを参照しない。
/// cfg_attr の条件以外や通常のマクロ引数、関数呼び出しは残す。
#[test]
fn rust_cfg_condition_keys_are_not_symbol_references() {
    let source = r#"pub fn feature() {}
#[cfg(feature = "extra")]
fn optional() {}
#[cfg_attr(all(feature = "extra", not(test)), derive(Feature))]
struct Optional;
#[cfg_attr(feature = "extra", cfg(feature = "nested"), derive(Feature))]
#[cfg_attr(feature = "extra", cfg_attr(feature = "inner", cfg(feature = "deep")))]
struct Nested;
fn call() {
    if cfg!(not(feature = "extra")) { feature(); }
    assert!(cfg!(feature = "inside_macro"));
    ordinary!(feature);
    cfg_attr!(feature, 0);
}
"#;
    let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
    let kinds = definition_node_kinds(LangId::Rust);
    let refs = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "feature",
        "test.rs",
        kinds,
        LangId::Rust,
    );
    let refs_in_batch = collect_batch_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        &["feature".to_string()],
        "test.rs",
        kinds,
        LangId::Rust,
    );
    assert_eq!(refs.len(), 4, "定義と実際の参照 3 件だけ: {refs:?}");
    assert_eq!(
        ref_fingerprints(&refs_in_batch[0]),
        ref_fingerprints(&refs),
        "単一名とバッチの結果が一致すること"
    );
    assert_eq!(
        count_refs_for_test(
            tree.root_node(),
            source.as_bytes(),
            &["feature".to_string()],
            kinds,
            LangId::Rust,
            1,
        )[0],
        3,
        "件数だけの経路も cfg 条件を除外すること"
    );
    let combinator = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "test",
        "test.rs",
        kinds,
        LangId::Rust,
    );
    assert!(combinator.is_empty(), "bare の条件キーも除外すること");
    let derive_trait = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "Feature",
        "test.rs",
        kinds,
        LangId::Rust,
    );
    assert_eq!(derive_trait.len(), 2, "cfg_attr の後続属性は保持すること");
}

/// Rust の `pub fn` と struct field が同名のとき、フィールド名位置の扱いが
/// **出力面 (`refs`) と判定面 (dead-code の参照カウント) で分かれる**ことを検証する。
///
/// - `refs` は「識別子の出現」を返す契約なので、フィールド宣言・初期化・アクセスも返す。
///   旧実装はこれらを共有 walker で落としており、`refs --name <フィールド名>` が
///   実出現に対して常に 0 件を返していた (CLAUDE.md がエージェントに
///   「0 件の AST クエリも解析結果なので Grep で追試するな」と指示している以上、
///   最悪方向の契約違反)。
/// - dead-code の参照カウントは、同名関数への参照ではないと構造的に確定するので数えない
///   (Issue: 2026-05-21-redact-impact-triage の impact ノイズと同じ根拠)。
///
/// 対照として、shorthand (`Cfg { redact }`) は**どちらの面でも参照**として残す
/// (`shorthand_is_a_reference_so_fn_pointer_table_entry_is_not_dead` が本来の理由を固定する)。
#[test]
fn rust_field_name_positions_are_refs_but_not_counted_for_dead_code() {
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

fn caller(cfg: &Cfg, data: &str) {
if cfg.redact {
    let _ = redact(data);
}
}
"#,
    )
    .unwrap();

    let refs = find_references("redact", dir.path(), Some("**/*.rs")).unwrap();
    let refs_text: Vec<&str> = refs.iter().filter_map(|r| r.context.as_deref()).collect();

    // 出力面: 定義・関数呼び出しに加え、フィールド名位置もすべて返る
    assert!(
        refs.iter().any(|r| r.kind == Some(RefKind::Definition)),
        "関数定義が含まれること: {refs_text:?}"
    );
    assert!(
        refs_text.iter().any(|c| c.contains("redact(data)")),
        "関数呼び出し redact(data) は含まれるべき: {refs_text:?}"
    );
    assert!(
        refs_text.iter().any(|c| c.contains("pub redact: bool")),
        "struct field 宣言も識別子の出現として返るべき: {refs_text:?}"
    );
    assert!(
        refs_text.iter().any(|c| c.contains("Cfg { redact: flag }")),
        "field_initializer も識別子の出現として返るべき: {refs_text:?}"
    );
    assert!(
        refs_text.iter().any(|c| c.contains("cfg.redact")),
        "field_expression も識別子の出現として返るべき: {refs_text:?}"
    );

    // 判定面: dead-code の参照カウントはフィールド名位置を数えない。
    // 数えるのは `redact(data)` の 1 件だけ (定義行は Definition なので対象外)。
    let counts = count_non_definition_refs_split_with_extra_files(
        &["redact".to_string()],
        dir.path(),
        Some("**/*.rs"),
        &[],
        |_| false,
    )
    .unwrap()
    .counts;
    let (prod, _test) = counts["redact"];
    assert_eq!(
        prod, 1,
        "dead-code のカウントは関数呼び出し 1 件のみ (フィールド名位置は数えない)"
    );
}

/// shorthand (`Table { handler }`) は `Table { handler: handler }` の**値側**、
/// つまりスコープ内の名前を読む式なので、fn ポインタ表へ登録された公開関数への
/// 正真正銘の参照になる。旧実装はこれをフィールド名位置と一緒に落としていたため、
/// **生きている公開関数が dead-code に出る**という最悪方向の誤りが起きていた
/// (JS/TS の `shorthand_property_identifier` を参照として数える判断と同じ形)。
#[test]
fn shorthand_is_a_reference_so_fn_pointer_table_entry_is_not_dead() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        r#"pub fn handler() {}

pub struct Table {
pub handler: fn(),
}

pub fn make() -> Table {
Table { handler }
}
"#,
    )
    .unwrap();

    let refs = find_references("handler", dir.path(), Some("**/*.rs")).unwrap();
    let texts: Vec<&str> = refs.iter().filter_map(|r| r.context.as_deref()).collect();
    assert!(
        texts.iter().any(|c| c.contains("Table { handler }")),
        "shorthand は参照として返るべき: {texts:?}"
    );

    // dead-code のカウント経路でも参照として残る (= dead と誤判定しない)。
    let counts = count_non_definition_refs_split_with_extra_files(
        &["handler".to_string()],
        dir.path(),
        Some("**/*.rs"),
        &[],
        |_| false,
    )
    .unwrap()
    .counts;
    let (prod, _test) = counts["handler"];
    assert!(
        prod >= 1,
        "shorthand を数えないと fn ポインタ表の公開関数が dead と誤判定される: prod={prod}"
    );
}

/// destructuring pattern (`let Cfg { redact: v } = ...`) の field name は
/// **判定面では**関数参照として数えないことを検証
/// (出力面の `refs` には識別子の出現として残る)。
#[test]
fn rust_field_pattern_name_is_not_counted_for_dead_code() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
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

    let counts = count_non_definition_refs_split_with_extra_files(
        &["redact".to_string()],
        dir.path(),
        Some("**/*.rs"),
        &[],
        |_| false,
    )
    .unwrap()
    .counts;
    let (prod, _test) = counts["redact"];
    assert_eq!(
        prod, 1,
        "field_pattern の name 部と field 宣言は数えず、関数呼び出し 1 件のみ"
    );

    // 対照: 出力面では field_pattern も識別子の出現として返る
    let refs = find_references("redact", dir.path(), Some("**/*.rs")).unwrap();
    let texts: Vec<&str> = refs.iter().filter_map(|r| r.context.as_deref()).collect();
    assert!(
        texts
            .iter()
            .any(|c| c.contains("let Cfg { redact: value }")),
        "refs には field_pattern の出現も返るべき: {texts:?}"
    );
}

/// 構造体パターンの省略形 (`let Cfg { zeta, .. } = c` / `Cfg { zeta, other } =>`) の
/// フィールド名も `refs` の出現として返す。
///
/// 旧実装は `shorthand_field_identifier` を識別子として扱わず、非省略形
/// (`let Cfg { zeta: z, .. }`) のフィールド名は返すのに省略形だけを落としていた。
/// フィールド名を変えるときは省略形も書き換えが要るので、出現一覧から落ちると修正漏れになる。
///
/// **対照ケースを内蔵する**: 判定面 (dead-code の件数) は省略形のパターンを数えない
/// (フィールド名でありローカルへの束縛で、同名の関数への参照ではない)。単一検索と
/// `--names` のバッチ検索は同じ位置を返す。
#[test]
fn rust_shorthand_field_pattern_is_returned_by_refs_but_not_counted() {
    let dir = tempfile::tempdir().unwrap();
    let source = "pub struct Cfg { pub zeta: u8, pub other: u8 }\n\
                  pub fn make() -> Cfg { let zeta = 1; Cfg { zeta, other: 2 } }\n\
                  pub fn read(c: &Cfg) -> u8 { let Cfg { zeta, .. } = c; *zeta }\n\
                  pub fn read2(c: &Cfg) -> u8 { match c { Cfg { zeta, other } => *zeta + *other } }\n\
                  pub fn read3(c: &Cfg) -> u8 { let Cfg { zeta: z, .. } = c; *z + c.zeta }\n\
                  pub fn zeta() -> u8 { 0 }\n";
    std::fs::write(dir.path().join("lib.rs"), source).unwrap();

    let positions = |refs: &[SymbolReference]| -> Vec<(usize, usize)> {
        let mut out: Vec<(usize, usize)> = refs.iter().map(|r| (r.line, r.column)).collect();
        out.sort();
        out
    };
    let zeta = find_references("zeta", dir.path(), Some("**/*.rs")).unwrap();
    let zeta_positions = positions(&zeta);
    for shorthand in [(2, 39), (3, 46)] {
        assert!(
            zeta_positions.contains(&shorthand),
            "省略形のパターンのフィールド名 {shorthand:?} を返す: {zeta_positions:?}"
        );
    }
    // 対照: 非省略形のフィールド名 (read3) は従来どおり返す
    assert!(zeta_positions.contains(&(4, 40)), "{zeta_positions:?}");

    let batch = find_references_batch(
        &["zeta".to_string(), "other".to_string()],
        dir.path(),
        Some("**/*.rs"),
    )
    .unwrap();
    assert_eq!(
        positions(&batch["zeta"]),
        zeta_positions,
        "単一とバッチが一致"
    );
    assert!(
        positions(&batch["other"]).contains(&(3, 52)),
        "`Cfg {{ zeta, other }}` の other も返す: {:?}",
        positions(&batch["other"])
    );

    // 判定面: 省略形のパターンは関数 `zeta` への参照として数えない。数えるのは
    // パターンが束縛したローカルの読み出し `*zeta` の 1 件だけ (識別子の種別に加えても、
    // `is_rust_struct_field_non_callable` で判定面から外す)。
    let count_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        count_dir.path().join("lib.rs"),
        "pub struct Cfg { pub zeta: u8 }\npub fn zeta() -> u8 { 0 }\npub fn read(c: &Cfg) -> u8 { let Cfg { zeta, .. } = c; *zeta }\n",
    )
    .unwrap();
    let counts = count_non_definition_refs_split_with_extra_files(
        &["zeta".to_string()],
        count_dir.path(),
        Some("**/*.rs"),
        &[],
        |_| false,
    )
    .unwrap()
    .counts;
    assert_eq!(counts["zeta"].0, 1, "`*zeta` の 1 件だけを数える");
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

/// turbofish 付きメソッド呼び出し (`s.fetch::<u32>()`) はメソッド参照として数える。
///
/// `call_expression > generic_function > field_expression` と祖父が `generic_function` に
/// なるため、旧実装は「ただのフィールドアクセス」と誤判定して dead-code / impact の
/// 判定面から落としていた (turbofish でしか呼ばれない `Store.fetch` が dead に出た)。
/// 対照: turbofish なしの呼び出しは従来どおり数え、同名フィールドへのアクセス
/// (`s.fetch` / `s.plain`) とフィールド宣言は引き続き数えない。
#[test]
fn rust_turbofish_method_call_is_counted_but_field_access_is_not() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        r#"pub struct Store {
    pub fetch: u32,
    pub plain: u32,
}
impl Store {
    pub fn fetch<T: Default>(&self) -> T { T::default() }
    pub fn plain(&self) -> u32 { 0 }
}
pub fn run(s: &Store) -> u32 {
    s.fetch::<u32>() + s.plain() + s.fetch + s.plain
}
"#,
    )
    .unwrap();

    let counts = count_non_definition_refs_split_with_extra_files(
        &["fetch".to_string(), "plain".to_string()],
        dir.path(),
        Some("**/*.rs"),
        &[],
        |_| false,
    )
    .unwrap()
    .counts;
    assert_eq!(
        counts["fetch"].0, 1,
        "turbofish 付き呼び出し 1 件だけを数える (フィールド宣言・`s.fetch` は数えない)"
    );
    assert_eq!(
        counts["plain"].0, 1,
        "対照: turbofish なしの呼び出し 1 件だけを数える (フィールド宣言・`s.plain` は数えない)"
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

/// serde の `default = "fn"` の関数名も参照として数える (単一検索と件数の両方)。
///
/// 旧実装はキーに `default` が無く、`#[serde(default = "default_port")]` からしか使われない
/// `pub fn default_port()` を refs が 0 件と返し、dead-code が dead と報告していた
/// (serde の derive が呼ぶので、消すとコンパイルできない)。
/// 対照: 値の無い `#[serde(default)]` は文字列を持たず、何も数えない。
#[test]
fn rust_attr_string_ref_detected_for_serde_default() {
    let source = r#"
#[derive(Deserialize)]
pub struct Config {
#[serde(default = "default_port")]
pub port: u16,
#[serde(default)]
pub host: String,
}
pub fn default_port() -> u16 { 8080 }
"#;
    let tree = parse_rust(source);
    let defs = definition_node_kinds(LangId::Rust);
    let refs = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "default_port",
        "test.rs",
        defs,
        LangId::Rust,
    );
    let count_kind = |kind: RefKind| refs.iter().filter(|r| r.kind == Some(kind)).count();
    assert_eq!(
        (
            count_kind(RefKind::Definition),
            count_kind(RefKind::Reference)
        ),
        (1, 1),
        "定義と属性の文字列の参照: {refs:?}"
    );
    let counts = count_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        &["default_port".to_string()],
        defs,
        LangId::Rust,
        1,
    );
    assert_eq!(counts[0], 1, "dead-code の件数にも数える");
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
    // (説明, ソース, 期待 def 数, `refs` の期待 ref 数, dead-code カウントの期待 ref 数)
    // 2 つの ref 数が分かれるのは Rust のフィールド名位置だけ。`refs` は識別子の出現を
    // 返す契約なので struct のフィールド宣言も返し、dead-code のカウントは同名関数への
    // 参照ではないと構造的に確定するので数えない。
    let cases: &[(&str, &str, usize, usize, usize)] = &[
        (
            "tuple pattern",
            "pub fn tail(path: &str) -> &str {\n    path.rsplit_once('/').map_or(path, |(_, tail)| tail)\n}\n",
            1,
            0,
            0,
        ),
        (
            "plain pattern",
            "pub fn tail() -> u8 { 0 }\npub fn run(xs: &[u8]) -> u8 { xs.iter().map(|tail| *tail).max().unwrap_or(0) }\n",
            1,
            0,
            0,
        ),
        (
            "reference pattern",
            "pub fn tail() -> u8 { 0 }\npub fn run(xs: &[u8]) -> u8 { xs.iter().map(|&tail| tail).max().unwrap_or(0) }\n",
            1,
            0,
            0,
        ),
        (
            "typed parameter",
            "pub fn tail() -> u8 { 0 }\npub fn run() -> u8 { let f = |tail: u8| tail; f(1) }\n",
            1,
            0,
            0,
        ),
        (
            "struct shorthand pattern",
            "pub fn tail() -> u8 { 0 }\npub struct P { tail: u8 }\npub fn run(p: P) -> u8 { Some(p).map(|P { tail }| tail).unwrap_or(0) }\n",
            1,
            // `pub struct P { tail: u8 }` のフィールド宣言と、パターンの省略形 `P { tail }` の
            // フィールド名は識別子の出現として `refs` に出る (closure 本体の `tail` は出ない)
            2,
            0,
        ),
        (
            "struct renamed pattern binds the value side only",
            "pub fn tail() -> u8 { 0 }\npub struct P { key: u8 }\npub fn run(p: P) -> u8 { Some(p).map(|P { key: tail }| tail).unwrap_or(0) }\n",
            1,
            0,
            0,
        ),
        (
            "nested closure inherits the outer binding",
            "pub fn tail() -> u8 { 0 }\npub fn run() -> u8 { let f = |tail: u8| (move || tail)(); f(1) }\n",
            1,
            0,
            0,
        ),
        // 対照: closure の外にある同名参照は従来どおり数える
        (
            "call outside the closure stays a reference",
            "pub fn tail() -> u8 { 0 }\npub fn run() -> u8 { let f = |x: u8| x; f(tail()) }\n",
            1,
            1,
            1,
        ),
        // 対照: 束縛していない名前は closure 内でも参照のまま
        (
            "unbound name inside a closure stays a reference",
            "pub fn tail() -> u8 { 0 }\npub fn run() -> u8 { let f = |x: u8| x + tail(); f(1) }\n",
            1,
            1,
            1,
        ),
        // 対照: パターンの型名は束縛ではないので参照のまま (引数型 + パターン型名の 2 件)
        (
            "tuple struct pattern type name stays a reference",
            "pub struct Tail(u8);\npub fn run(t: Tail) -> u8 { Some(t).map(|Tail(v)| v).unwrap_or(0) }\n",
            1,
            2,
            2,
        ),
        // 対照: 修飾パス経由の呼び出しはローカル束縛にシャドーイングされない。
        // ここを消すと live symbol を dead と誤判定する (逆向きの事故)。
        (
            "crate-qualified call inside the closure stays a reference",
            "pub fn tail() -> u8 { 0 }\npub fn run() -> u8 { let f = |tail: u8| tail + crate::tail(); f(1) }\n",
            1,
            1,
            1,
        ),
        // 対照: メソッド呼び出しの名前 (field_identifier) も束縛の対象外
        (
            "method call inside the closure stays a reference",
            "pub struct H;\nimpl H { pub fn tail(&self) -> u8 { 0 } }\npub fn run(h: H) -> u8 { let f = |tail: u8| tail + h.tail(); f(1) }\n",
            1,
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
            2,
        ),
        // 対照: 単位構造体パターンは束縛ではないので参照のまま
        (
            "unit struct pattern stays a reference",
            "pub struct Unit;\npub fn run() { let _f = |Unit| (); }\n",
            1,
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
            1,
        ),
        // 対照: 小文字名の定数パターン。命名 lint は強制ではないので大小文字だけでは
        // 束縛と証明できない。同一ファイルに同名 const があれば束縛判定を諦める。
        (
            "lowercase const pattern stays a reference",
            "#![allow(non_upper_case_globals)]\npub const tail: () = ();\npub fn run() { let f = |tail: ()| (); f(()); }\n",
            1,
            1,
            1,
        ),
        // 対照: 小文字名の単位構造体も同様
        (
            "lowercase unit struct pattern stays a reference",
            "pub struct tail;\npub fn run() { let f = |tail| { let _ = tail; }; f(tail); }\n",
            1,
            3,
            3,
        ),
        // 対照: `use` で持ち込まれた名前は外部の const / unit struct かもしれない
        (
            "imported name in a pattern stays a reference",
            "use crate::other::tail;\npub fn run() { let f = |tail| { let _ = tail; }; f(tail); }\n",
            0,
            4,
            4,
        ),
        // 対照: glob import は持ち込む名前が AST に現れないため、対象名が定数パターン
        // でないことを確定できない。ファイル内に宣言も名前付き use も無い形で再現する。
        (
            "glob import makes pattern names unresolvable",
            "use crate::other::*;\npub fn run() { let f = |tail| { let _ = tail; }; f(tail); }\n",
            0,
            3,
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
            2,
        ),
    ];

    for (label, source, want_def, want_ref, want_count) in cases {
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
            counts[0], *want_count,
            "{label}: count-only 経路 (dead-code) の分類"
        );
    }
}

/// 修正前の再帰版。反復版と判定が一致することを確かめる参照実装 (浅い入力でだけ使う)。
fn rust_file_declares_non_binding_name_recursive(
    node: Node<'_>,
    name: &str,
    source: &[u8],
) -> bool {
    match node.kind() {
        "use_wildcard" => return true,
        "const_item" | "static_item" => {
            if node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                == Some(name)
            {
                return true;
            }
        }
        "struct_item" => {
            let is_unit = node.child_by_field_name("body").is_none();
            if is_unit
                && node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    == Some(name)
            {
                return true;
            }
        }
        "use_declaration"
            if node.utf8_text(source).is_ok_and(|text| {
                text.split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|seg| seg == name)
            }) =>
        {
            return true;
        }
        _ => {}
    }
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .any(|child| rust_file_declares_non_binding_name_recursive(child, name, source))
}

/// ファイル内の「束縛ではなくする宣言」の判定は、反復版と再帰版で一致する。
/// 入れ子の item (関数内の const、ブロック内の use) と glob import も含める。
#[test]
fn rust_file_declares_non_binding_name_matches_recursive_version() {
    let sources = [
        "pub fn tail() -> u8 { 0 }\npub fn run(v: &[u8]) -> usize { v.iter().map(|tail| *tail).count() }\n",
        "const tail: u8 = 1;\npub fn run() -> u8 { let f = |tail: u8| tail; f(1) }\n",
        "pub fn run() -> u8 { const head: u8 = 2; static tail: u8 = 3; head + tail }\n",
        "struct Unit;\nstruct Wide { a: u8 }\nstruct Pair(u8, u8);\npub fn run() {}\n",
        "mod m { use crate::o::{tail, head as h}; }\npub fn run() { { use crate::x::*; } }\n",
        "use crate::o::tailor;\npub fn run() {}\n",
    ];
    let names = [
        "tail", "head", "h", "Unit", "Wide", "Pair", "tailor", "run", "x",
    ];
    for source in sources {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        for name in names {
            assert_eq!(
                rust_file_declares_non_binding_name(tree.root_node(), name, source.as_bytes()),
                rust_file_declares_non_binding_name_recursive(
                    tree.root_node(),
                    name,
                    source.as_bytes()
                ),
                "{name} in {source}"
            );
        }
    }
    // 対照: 該当する宣言の有無で結果が分かれる
    let tree = parser::parse_source(sources[1].as_bytes(), LangId::Rust).unwrap();
    assert!(rust_file_declares_non_binding_name(
        tree.root_node(),
        "tail",
        sources[1].as_bytes()
    ));
    let tree = parser::parse_source(sources[0].as_bytes(), LangId::Rust).unwrap();
    assert!(!rust_file_declares_non_binding_name(
        tree.root_node(),
        "tail",
        sources[0].as_bytes()
    ));
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

/// Rust の def/ref 分類が「宣言の `name` フィールド一致」で行われることを固定する。
///
/// 旧実装は汎用の parent/grandparent 走査で、親が定義ノードでありさえすれば無条件に
/// def としていた。tree-sitter-rust では戻り値型・型注釈・型エイリアス右辺・impl の
/// trait 名/型名がいずれも定義ノードの**直接の子**なので、これらが軒並み def に化け、
/// 実測では定義 1 箇所の型に対し `refs` が def を 5 件返していた。
///
/// **対照ケース内蔵**: 宣言名そのもの (struct / trait / type alias / const / static /
/// fn / mod の name) は Definition のままであることを同一テストで固定する。これが無いと
/// 「全部 Reference になっただけ」の退行を検出できない。
#[test]
fn rust_definition_is_only_the_name_field_not_type_positions() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        r#"pub struct Outcome;

pub type Alias = Outcome;

pub type Wrapped = Option<Outcome>;

pub const DEFAULT: Option<Outcome> = None;

pub static CURRENT: Option<Outcome> = None;

pub trait Runner {
fn run(&self) -> Outcome;
}

pub struct Engine;

impl Runner for Engine {
fn run(&self) -> Outcome {
    Outcome
}
}

pub fn make() -> Outcome {
Outcome
}

pub mod registry {
pub fn lookup() {}
}
"#,
    )
    .unwrap();

    let defs_of = |name: &str| -> Vec<String> {
        find_references(name, dir.path(), Some("**/*.rs"))
            .unwrap()
            .iter()
            .filter(|r| r.kind == Some(RefKind::Definition))
            .map(|r| r.context.clone().unwrap_or_default().trim().to_string())
            .collect()
    };
    let refs_of = |name: &str| -> Vec<String> {
        find_references(name, dir.path(), Some("**/*.rs"))
            .unwrap()
            .iter()
            .filter(|r| r.kind != Some(RefKind::Definition))
            .map(|r| r.context.clone().unwrap_or_default().trim().to_string())
            .collect()
    };

    // 型名: 定義は struct 宣言の 1 件だけ。戻り値型・alias 右辺・型引数・struct 式は Reference。
    let outcome_defs = defs_of("Outcome");
    assert_eq!(
        outcome_defs,
        vec!["pub struct Outcome;".to_string()],
        "Outcome の Definition は struct 宣言 1 件のみであること"
    );
    let outcome_refs = refs_of("Outcome");
    for expected in [
        "pub type Alias = Outcome;",
        "pub type Wrapped = Option<Outcome>;",
        "pub const DEFAULT: Option<Outcome> = None;",
        "pub static CURRENT: Option<Outcome> = None;",
        "fn run(&self) -> Outcome;",
        "pub fn make() -> Outcome {",
    ] {
        assert!(
            outcome_refs.iter().any(|c| c == expected),
            "{expected:?} は Reference であること: {outcome_refs:?}"
        );
    }

    // trait 名: 宣言が Definition、impl 行は Reference。
    assert_eq!(
        defs_of("Runner"),
        vec!["pub trait Runner {".to_string()],
        "Runner の Definition は trait 宣言 1 件のみであること"
    );
    assert!(
        refs_of("Runner")
            .iter()
            .any(|c| c == "impl Runner for Engine {"),
        "impl の trait 名は Reference であること: {:?}",
        refs_of("Runner")
    );

    // impl 対象の型名も Reference (impl_item は name フィールドを持たない)。
    assert_eq!(
        defs_of("Engine"),
        vec!["pub struct Engine;".to_string()],
        "Engine の Definition は struct 宣言 1 件のみであること"
    );
    assert!(
        refs_of("Engine")
            .iter()
            .any(|c| c == "impl Runner for Engine {"),
        "impl の型名は Reference であること: {:?}",
        refs_of("Engine")
    );

    // 対照: 宣言名そのものは Definition のまま (「全部 ref になった」退行の検出)。
    for (name, decl) in [
        ("Alias", "pub type Alias = Outcome;"),
        ("DEFAULT", "pub const DEFAULT: Option<Outcome> = None;"),
        ("CURRENT", "pub static CURRENT: Option<Outcome> = None;"),
        ("make", "pub fn make() -> Outcome {"),
        ("registry", "pub mod registry {"),
    ] {
        assert_eq!(
            defs_of(name),
            vec![decl.to_string()],
            "{name} の宣言名は Definition のままであること"
        );
    }
}

/// supertrait (`trait Child: Parent`) が Reference として数えられることを固定する。
///
/// 旧実装では `trait_bounds` を挟んだ grandparent 走査で `Parent` が def に化け、
/// 「継承されているだけの trait」が参照 0 件になっていた。新実装が grandparent 経由の
/// 救済を持たないことをここで保証する (持たせると本ケースが再び def へ戻る)。
#[test]
fn rust_supertrait_position_is_a_reference() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        r#"pub trait Parent {}

pub trait Child: Parent {}
"#,
    )
    .unwrap();

    let refs = find_references("Parent", dir.path(), Some("**/*.rs")).unwrap();
    let defs: Vec<_> = refs
        .iter()
        .filter(|r| r.kind == Some(RefKind::Definition))
        .collect();
    assert_eq!(
        defs.len(),
        1,
        "Parent の Definition は宣言 1 件のみ: {refs:?}"
    );

    let counts = count_non_definition_refs_split_with_extra_files(
        &["Parent".to_string()],
        dir.path(),
        Some("**/*.rs"),
        &[],
        |_| false,
    )
    .unwrap()
    .counts;
    let (prod, _test) = counts["Parent"];
    assert_eq!(prod, 1, "supertrait 位置が参照として数えられること");
}

/// 修正前の再帰版 (浅い入力でだけ使う参照実装)。
fn rust_pattern_binds_name_recursive(
    node: tree_sitter::Node<'_>,
    name: &str,
    source: &[u8],
    cache: &crate::engine::refs::definition::rust::RustPatternBindingCache,
) -> bool {
    let matches_name =
        |n: tree_sitter::Node<'_>| n.utf8_text(source).is_ok_and(|text| text == name);
    match node.kind() {
        "identifier" => {
            matches_name(node)
                && crate::engine::refs::definition::rust::rust_pattern_binds_name(
                    node, name, source, cache,
                )
        }
        "shorthand_field_identifier" => matches_name(node),
        "tuple_struct_pattern" | "struct_pattern" => {
            let type_node = node.child_by_field_name("type");
            let mut cursor = node.walk();
            node.children(&mut cursor).any(|child| {
                type_node.is_none_or(|t| t.id() != child.id())
                    && rust_pattern_binds_name_recursive(child, name, source, cache)
            })
        }
        "field_pattern" => match node.child_by_field_name("pattern") {
            Some(pattern) => rust_pattern_binds_name_recursive(pattern, name, source, cache),
            None => node
                .child_by_field_name("name")
                .is_some_and(|n| n.kind() == "shorthand_field_identifier" && matches_name(n)),
        },
        "parameter" => node
            .child_by_field_name("pattern")
            .is_some_and(|pattern| rust_pattern_binds_name_recursive(pattern, name, source, cache)),
        "closure_parameters" | "tuple_pattern" | "slice_pattern" | "reference_pattern"
        | "ref_pattern" | "mut_pattern" | "captured_pattern" | "or_pattern" => {
            let mut cursor = node.walk();
            node.children(&mut cursor)
                .any(|child| rust_pattern_binds_name_recursive(child, name, source, cache))
        }
        _ => false,
    }
}

/// closure 引数のパターンが名前を束縛するかの判定は、明示スタックの版と再帰版で一致する。
/// identifier の葉の判定 (命名規約と同名の const / 単位構造体) は共通の処理なので、
/// 参照実装からは本体を identifier 1 個に対して呼ぶ。
#[test]
fn rust_pattern_binds_name_matches_recursive_version() {
    let source = "const MAX: u32 = 1;\n\
                  struct Unit;\n\
                  struct P(u32, u32);\n\
                  struct S { a: u32, b: u32 }\n\
                  fn f() {\n\
                  \x20   let c1 = |x| x;\n\
                  \x20   let c2 = |(a, (b, [c, ..]))| a;\n\
                  \x20   let c3 = |P(m, n), S { a, b: bb }| m;\n\
                  \x20   let c4 = |&mut (ref r, mut q @ 1..=3)| r;\n\
                  \x20   let c5 = |Unit, MAX, k: u32| k;\n\
                  \x20   let c6 = |S { a: (aa | aa), .. }, [first, .., last]| aa;\n\
                  \x20   let c7 = |((((deep,),),),)| deep;\n\
                  }\n";
    let tree = crate::engine::parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
    let mut params = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(n) = stack.pop() {
        if n.kind() == "closure_parameters" {
            params.push(n);
        }
        let mut cursor = n.walk();
        let children: Vec<_> = n.children(&mut cursor).collect();
        stack.extend(children.into_iter().rev());
    }
    assert_eq!(params.len(), 7);
    let names = [
        "x", "a", "b", "c", "bb", "m", "n", "r", "q", "k", "Unit", "MAX", "aa", "first", "last",
        "deep", "zz",
    ];
    let (mut bound, mut unbound) = (0, 0);
    for param in params {
        for name in names {
            let iterative = crate::engine::refs::definition::rust::rust_pattern_binds_name(
                param,
                name,
                source.as_bytes(),
                &Default::default(),
            );
            let recursive = rust_pattern_binds_name_recursive(
                param,
                name,
                source.as_bytes(),
                &Default::default(),
            );
            assert_eq!(
                iterative,
                recursive,
                "{name} in {:?}",
                param.utf8_text(source.as_bytes())
            );
            if iterative {
                bound += 1;
            } else {
                unbound += 1;
            }
        }
    }
    // 空振り防止: 束縛になる組とならない組がどちらもある (Unit / MAX / zz は束縛にならない)
    assert!(
        bound >= 15 && unbound >= 50,
        "bound={bound} unbound={unbound}"
    );
}

/// union の宣言の名前は Definition で、型・式・パターンの位置は Reference (Issue #277)。宣言を
/// 参照に数えると、使われない union が dead-code で生きて見える。
#[test]
fn rust_union_declaration_is_a_definition() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.rs"),
        "pub union Value {\n    pub number: u32,\n}\n\npub fn make() -> Value {\n    Value { number: 1 }\n}\n",
    )
    .unwrap();
    let refs = find_references("Value", dir.path(), Some("**/*.rs")).unwrap();
    let of = |kind_is_definition: bool| -> Vec<String> {
        refs.iter()
            .filter(|r| (r.kind == Some(RefKind::Definition)) == kind_is_definition)
            .map(|r| r.context.clone().unwrap_or_default().trim().to_string())
            .collect()
    };
    assert_eq!(of(true), ["pub union Value {"]);
    assert_eq!(
        of(false),
        ["pub fn make() -> Value {", "Value { number: 1 }"]
    );
}
