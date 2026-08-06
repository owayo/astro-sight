use super::*;

/// bash の `trap '<handler>' SIG` 内の関数参照が count 経路 (CountSink) で
/// 非 Definition としてカウントされ、dead-code 判定で生存扱いになることを検証する。
/// 旧実装では trap handler 内は文字列扱いで参照ゼロとなり、`cleanup_signal` のような
/// シグナルハンドラが false-positive で dead として列挙される回帰があった。
#[test]
fn bash_trap_handler_counts_as_non_definition_ref() {
    use std::borrow::Cow;
    use std::collections::HashMap;

    let source = "cleanup_signal() {\n    exit 1\n}\ntrap 'cleanup_signal 130' INT\ntrap \"cleanup_signal 143\" TERM\n";
    let tree = parser::parse_source(source.as_bytes(), LangId::Bash).expect("parse");
    let defs = definition_node_kinds(LangId::Bash);
    let mut name_to_ix: HashMap<Cow<'_, str>, Vec<usize>> = HashMap::new();
    name_to_ix.insert(Cow::Borrowed("cleanup_signal"), vec![0]);
    let counts = count_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        &name_to_ix,
        defs,
        LangId::Bash,
        1,
    );
    assert_eq!(
        counts[0], 2,
        "bash trap handler 内の関数参照は 2 件カウントされるべき (single+double quoted)"
    );
}

/// `find_references` (CLI の `astro-sight refs --name`) が bash trap handler
/// 内の関数参照を返すこと。Issue #5 で報告された再現を回帰テスト化したもの。
#[test]
fn bash_trap_handler_resolved_in_find_references() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("update_server.sh");
    std::fs::write(
        &script,
        "#!/bin/bash\ncleanup_signal() {\n    local sig_exit=$1\n    exit \"${sig_exit}\"\n}\ntrap 'cleanup_signal 130' INT\ntrap 'cleanup_signal 143' TERM\n",
    )
    .unwrap();

    let refs = find_references("cleanup_signal", dir.path(), None).unwrap();
    // 期待: 定義 1 件 + trap 経由参照 2 件
    let defs: Vec<_> = refs
        .iter()
        .filter(|r| r.kind == Some(RefKind::Definition))
        .collect();
    let non_defs: Vec<_> = refs
        .iter()
        .filter(|r| r.kind != Some(RefKind::Definition))
        .collect();
    assert_eq!(defs.len(), 1, "definition should be 1, got refs={refs:?}");
    assert_eq!(
        non_defs.len(),
        2,
        "trap handler refs should be 2, got refs={refs:?}"
    );
}

/// 引用なし `trap func SIG` (`word` ノード) は通常の identifier 走査で拾われるため、
/// bash_trap_handler_ref_segments 側では二重カウントしないことを検証する。
#[test]
fn bash_trap_unquoted_word_not_double_counted() {
    use std::borrow::Cow;
    use std::collections::HashMap;

    let source = "cleanup() { exit 1; }\ntrap cleanup INT\n";
    let tree = parser::parse_source(source.as_bytes(), LangId::Bash).expect("parse");
    let defs = definition_node_kinds(LangId::Bash);
    let mut name_to_ix: HashMap<Cow<'_, str>, Vec<usize>> = HashMap::new();
    name_to_ix.insert(Cow::Borrowed("cleanup"), vec![0]);
    let counts = count_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        &name_to_ix,
        defs,
        LangId::Bash,
        1,
    );
    // 引用なし `trap cleanup INT` は通常の word 走査で 1 件として拾われる。
    // bash_trap_handler_ref_segments は raw_string/string のみ対象なので加算しない。
    assert_eq!(counts[0], 1, "unquoted word must not be double-counted");
}

/// visitor 経由の参照イベントは `column` にファイル絶対列 (tree-sitter Point 座標系)、
/// `context_column` に trim 済み context 行内の相対列を運ぶ (インデント行で両者がずれる)。
#[test]
fn ref_visit_event_carries_absolute_and_context_columns() {
    struct Recorder {
        // (line, column, context_column, is_def)
        events: Vec<(usize, usize, usize, bool)>,
    }
    impl RefVisitor for Recorder {
        fn on_ref(&mut self, event: RefVisitEvent<'_>) {
            self.events
                .push((event.line, event.column, event.context_column, event.is_def));
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample.ts");
    // 参照行は行頭スペース 4 のインデント付き
    std::fs::write(
        &path,
        "function targetFn(): void {}\nfunction run(): void {\n    targetFn();\n}\n",
    )
    .unwrap();

    let names = vec!["targetFn".to_string()];
    let ac = build_ac_case_insensitive(&names).unwrap();
    let utf8_path = camino::Utf8Path::from_path(&path).expect("utf-8 path");
    let mut recorder = Recorder { events: Vec::new() };
    visit_refs_and_defs_in_file_cb(&names, &ac, utf8_path, &mut recorder).unwrap();

    let (line, column, context_column, _) = *recorder
        .events
        .iter()
        .find(|(_, _, _, is_def)| !is_def)
        .expect("targetFn の参照イベントが 1 件はあるはず");
    assert_eq!(line, 2, "参照は 3 行目 (0-indexed 2)");
    assert_eq!(column, 4, "column はファイル絶対列 (インデント 4 込み)");
    assert_eq!(context_column, 0, "context_column は trim 済み行内の相対列");
}

/// Rust の参照 usage role 分類 (Issue 2026-07-12-bevy-systemparam-optional-res-impact-fp)。
/// callee / タプル値渡し / 直接引数値渡し / fn 型固定 let / 型付きタプル /
/// 冗長括弧付き method receiver (B-1) を区別する。
#[test]
fn classify_rust_ref_usage_roles() {
    let source = b"fn my_system(x: u32) { let _ = x; }\n\
                   fn register<F>(_f: F) {}\n\
                   fn setup() {\n\
                       my_system(1);\n\
                       register((1, my_system, 2));\n\
                       register(my_system);\n\
                       let pinned: fn(u32) = my_system;\n\
                       let _ = pinned;\n\
                       let typed_tuple: (fn(u32),) = (my_system,);\n\
                       let _ = typed_tuple;\n\
                       let typed_arr: [fn(u32); 1] = [my_system];\n\
                       let _ = typed_arr;\n\
                       let untyped = (my_system,);\n\
                       let _ = untyped;\n\
                       let parens: (fn(u32),) = ((my_system,));\n\
                       let _ = parens;\n\
                       (my_system).after(1);\n\
                       my_system.after(1);\n\
                       let paren_pinned: fn(u32) = (my_system);\n\
                       let _ = paren_pinned;\n\
                   }\n";
    let tree = crate::engine::parser::parse_source(source, LangId::Rust).unwrap();

    // (行, 期待 role) — my_system の参照出現行 (0-indexed)
    let mut roles: Vec<(usize, RefUsageRole)> = Vec::new();
    fn walk(node: Node<'_>, source: &[u8], roles: &mut Vec<(usize, RefUsageRole)>) {
        if node.kind() == "identifier"
            && node.utf8_text(source).ok() == Some("my_system")
            && node.start_position().row >= 3
        {
            roles.push((
                node.start_position().row,
                classify_ref_usage_role(node, LangId::Rust),
            ));
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(child, source, roles);
        }
    }
    walk(tree.root_node(), source, &mut roles);
    roles.sort_by_key(|(row, _)| *row);

    assert_eq!(
        roles,
        vec![
            (3, RefUsageRole::CallCallee),    // my_system(1)
            (4, RefUsageRole::FunctionValue), // (1, my_system, 2) タプル要素
            // register(my_system) の直接引数は、渡し先が fn ポインタ引数
            // (`fn accept(_: fn(u32))`) の場合に型変更で壊れるため blocking 維持
            (5, RefUsageRole::Other),
            (6, RefUsageRole::TypeConstrainedValue), // let pinned: fn(u32) = my_system
            // 明示型付きタプル/配列の要素は fn ポインタ型固定なので blocking 維持
            (8, RefUsageRole::Other), // let typed_tuple: (fn(u32),) = (my_system,)
            (10, RefUsageRole::Other), // let typed_arr: [fn(u32); 1] = [my_system]
            // 型注釈なしは fn item 型に推論され変更へ追随するため格下げ可
            (12, RefUsageRole::FunctionValue), // let untyped = (my_system,)
            // 冗長括弧付きでも外側の明示型を透過検出して blocking 維持
            (14, RefUsageRole::Other), // let parens: (fn(u32),) = ((my_system,))
            // 冗長括弧付き method receiver も透過して FunctionValue に格下げ (B-1)
            (16, RefUsageRole::FunctionValue), // (my_system).after(1)
            (17, RefUsageRole::FunctionValue), // my_system.after(1)
            // 冗長括弧付き fn 型固定 let も透過して blocking (TypeConstrainedValue)
            (18, RefUsageRole::TypeConstrainedValue), // let paren_pinned: fn(u32) = (my_system)
        ],
        "roles: {roles:?}"
    );
}

/// リファクタ後も single / batch Vec / callback / count の 4 経路が一致し続けることを
/// 担保する同値性テスト。5 種 synthetic ref 源 (rust_attr / bash_trap /
/// phpunit_metadata / php_callable_array / php_string_callable) + 通常 identifier を
/// 言語別 fixture で網羅し、実際に synthetic 参照が発火していることも確認する。
#[test]
fn ref_walkers_agree_across_all_paths() {
    let rust_src = r#"fn serialize_jst() {}
fn helper() {}
#[derive(Serialize)]
struct Bar {
#[serde(serialize_with = "serialize_jst")]
time: i64,
#[serde(skip_serializing_if = "Option::is_none")]
opt: Option<i64>,
}
fn run() { helper(); let _ = serialize_jst; }
"#;
    let bash_src = r#"cleanup_signal() {
exit 1
}
cleanup_exit() {
echo bye
}
trap 'cleanup_signal 130' INT
trap "cleanup_signal 143" TERM
trap 'cleanup_exit' EXIT
cleanup_signal 0
"#;
    let php_src = r#"<?php
class Ctrl {
/**
 * @dataProvider provideData
 */
public function testThing(): void {
    $this->handle();
}

#[DataProvider('attrData')]
public function testAttr(): void {}

public function provideData(): array { return []; }
public function attrData(): array { return []; }
public function handle(): void {}

public function routes(): void {
    $r = [Ctrl::class, 'handle'];
    $s = 'Ctrl@handle';
}
}
"#;

    let cases: &[(&str, &str, &[&str])] = &[
        (
            "equiv.rs",
            rust_src,
            &["serialize_jst", "helper", "is_none"],
        ),
        ("equiv.sh", bash_src, &["cleanup_signal", "cleanup_exit"]),
        (
            "equiv.php",
            php_src,
            &["provideData", "attrData", "handle", "testThing"],
        ),
    ];

    for (fname, source, names) in cases {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(fname);
        std::fs::write(&path, source).unwrap();
        let utf8_path = camino::Utf8Path::from_path(&path).expect("utf-8 path");
        let name_vec: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        let ac = build_ac_case_insensitive(&name_vec).unwrap();

        // 経路 B: batch Vec
        let batch = find_refs_batch_in_file_indexed(&name_vec, &ac, utf8_path).unwrap();

        // 経路 C: callback (index 別に (line, column, is_def) を収集)
        struct Rec {
            per_ix: Vec<Vec<(usize, usize, bool)>>,
        }
        impl RefVisitor for Rec {
            fn on_ref(&mut self, e: RefVisitEvent<'_>) {
                self.per_ix[e.sym_ix as usize].push((e.line, e.column, e.is_def));
            }
        }
        let mut rec = Rec {
            per_ix: vec![Vec::new(); name_vec.len()],
        };
        visit_refs_and_defs_in_file_cb(&name_vec, &ac, utf8_path, &mut rec).unwrap();

        // 経路 D: count (非 Definition のみ)
        let counts = count_refs_in_file(&name_vec, &ac, utf8_path).unwrap();

        for (ix, name) in name_vec.iter().enumerate() {
            // 経路 A: single
            let single = find_refs_in_file(name, utf8_path).unwrap();

            // A == B: (行, 列, 種別, context) 列が完全一致
            let a_key: Vec<_> = single
                .iter()
                .map(|r| (r.line, r.column, r.kind, r.context.clone()))
                .collect();
            let b_key: Vec<_> = batch[ix]
                .iter()
                .map(|r| (r.line, r.column, r.kind, r.context.clone()))
                .collect();
            assert_eq!(a_key, b_key, "single vs batch: {name} in {fname}");

            // B == C: (行, 列, is_def) 列が一致
            let b_events: Vec<_> = batch[ix]
                .iter()
                .map(|r| (r.line, r.column, r.kind == Some(RefKind::Definition)))
                .collect();
            assert_eq!(
                rec.per_ix[ix], b_events,
                "callback vs batch: {name} in {fname}"
            );

            // D == 非 Definition 件数
            let non_def = batch[ix]
                .iter()
                .filter(|r| r.kind != Some(RefKind::Definition))
                .count();
            assert_eq!(counts[ix], non_def, "count vs batch: {name} in {fname}");
        }

        // synthetic 源が実際に発火していること (テストが空振り一致でないこと) を担保。
        let count_of = |n: &str| -> usize {
            name_vec
                .iter()
                .position(|x| x == n)
                .map(|i| counts[i])
                .unwrap_or(0)
        };
        match *fname {
            // is_none は属性文字列内にのみ出現するので、非定義参照 = rust_attr 発火。
            "equiv.rs" => assert!(count_of("is_none") >= 1, "rust_attr synthetic must fire"),
            // cleanup_exit は trap 内にのみ出現するので、非定義参照 = bash_trap 発火。
            "equiv.sh" => {
                assert!(
                    count_of("cleanup_exit") >= 1,
                    "bash_trap synthetic must fire"
                )
            }
            // provideData/attrData は phpunit metadata 経由でのみ参照される。
            // handle は member_call(1) + callable_array(1) + string_callable(1) = 3。
            "equiv.php" => {
                assert!(
                    count_of("provideData") >= 1,
                    "phpunit docblock synthetic must fire"
                );
                assert!(
                    count_of("attrData") >= 1,
                    "phpunit attribute synthetic must fire"
                );
                assert!(
                    count_of("handle") >= 3,
                    "php callable_array + string_callable synthetics must fire"
                );
            }
            _ => {}
        }
    }
}
