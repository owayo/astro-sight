use anyhow::Result;
use std::collections::HashSet;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, Query, QueryCursor};

use crate::language::LangId;
use crate::models::call::{CallEdge, CallEndpoint, CallSite};
use crate::models::location::Range;

/// パース済み AST からコールエッジを抽出する。
/// `filter_function` 指定時は caller が一致するもののみ返す。
pub fn extract_calls(
    root: Node<'_>,
    source: &[u8],
    lang_id: LangId,
    filter_function: Option<&str>,
) -> Result<Vec<CallEdge>> {
    let query_src = call_query(lang_id);
    if query_src.is_empty() {
        return Ok(Vec::new());
    }

    let language = lang_id.ts_language();
    let query = Query::new(&language, query_src)?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

    let mut edges = Vec::new();
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let node = capture.node;
            let capture_name = &query.capture_names()[capture.index as usize];
            if !capture_name.ends_with("callee") {
                continue;
            }

            let callee_name = node.utf8_text(source).unwrap_or("").to_string();
            if callee_name.is_empty() {
                continue;
            }

            // Find the enclosing function (caller)
            let caller = find_enclosing_function(node, source, lang_id);
            let caller = match caller {
                Some(c) => c,
                None => continue, // top-level call, skip
            };

            if let Some(filter) = filter_function
                && caller.name != filter
            {
                continue;
            }

            // call_expression node is the parent of the callee identifier
            let call_node = node.parent().unwrap_or(node);
            let callee_range = Range::from(call_node.range());

            edges.push(CallEdge {
                caller,
                callee: CallEndpoint {
                    name: callee_name,
                    range: callee_range,
                },
                call_site: CallSite {
                    line: node.start_position().row,
                    column: node.start_position().column,
                },
            });
        }
    }

    Ok(edges)
}

/// ファイル全体の呼び出し先名 (callee) の集合を抽出する。
/// `extract_calls` はトップレベル呼び出し (caller が関数本体に属さないもの) を除外するため、
/// CLI スクリプトの `main()` 呼び出しや bash エントリポイントのコマンド呼び出しを
/// 拾えない。本関数は caller の有無を問わず純粋に callee 名を収集する。
pub fn extract_all_callees(
    root: Node<'_>,
    source: &[u8],
    lang_id: LangId,
) -> Result<HashSet<String>> {
    let query_src = call_query(lang_id);
    if query_src.is_empty() {
        return Ok(HashSet::new());
    }

    let language = lang_id.ts_language();
    let query = Query::new(&language, query_src)?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

    let mut callees = HashSet::new();
    while let Some(m) = matches.next() {
        for capture in m.captures {
            let capture_name = &query.capture_names()[capture.index as usize];
            if !capture_name.ends_with("callee") {
                continue;
            }
            let node = capture.node;
            let callee_name = node.utf8_text(source).unwrap_or("").to_string();
            if !callee_name.is_empty() {
                callees.insert(callee_name);
            }
        }
    }

    Ok(callees)
}

/// AST を上方走査し、最も近い「名前を解決できる」関数定義を見つける。
///
/// 匿名 callback (binding の無い arrow/function expression) は名前解決に失敗するが、
/// そこで打ち切らず外側の named function へ climb を続ける。即 None にすると
/// 匿名 callback 内の呼び出し edge が丸ごと消失する。
fn find_enclosing_function(node: Node<'_>, source: &[u8], lang_id: LangId) -> Option<CallEndpoint> {
    let func_kinds = function_node_kinds(lang_id);
    let mut current = node.parent();

    while let Some(n) = current {
        if func_kinds.contains(&n.kind())
            && let Some(name) = find_function_name(n, source, lang_id)
        {
            return Some(CallEndpoint {
                name,
                range: Range::from(n.range()),
            });
        }
        current = n.parent();
    }

    None
}

/// 言語ごとの関数ノード種別を返す。
fn function_node_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Rust => &["function_item"],
        LangId::C | LangId::Cpp => &["function_definition"],
        LangId::Python => &["function_definition"],
        LangId::Javascript => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
            "function_expression",
        ],
        LangId::Typescript | LangId::Tsx => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
            "function_expression",
        ],
        LangId::Go => &["function_declaration", "method_declaration"],
        LangId::Php => &["function_definition", "method_declaration"],
        LangId::Java => &["method_declaration", "constructor_declaration"],
        LangId::Kotlin => &["function_declaration"],
        LangId::Swift => &["function_declaration"],
        LangId::CSharp => &["method_declaration", "constructor_declaration"],
        LangId::Bash => &["function_definition"],
        LangId::Ruby => &["method", "singleton_method"],
        LangId::Zig => &["function_declaration", "test_declaration"],
        LangId::Xojo => &[
            "sub_declaration",
            "function_declaration",
            "constructor_declaration",
            "destructor_declaration",
            "event_declaration",
        ],
    }
}

/// 関数ノードの名前を抽出する。
fn find_function_name(node: Node<'_>, source: &[u8], lang_id: LangId) -> Option<String> {
    // JS/TS の arrow/function expression は自身の "name" では外部から参照できない
    // (named function expression の name は内部スコープ専用)。親の binding 名を
    // caller 名とし、binding が無い匿名 callback では偽名 (最初の identifier 子 =
    // 引数名) を作らず None を返して呼び出し元の climb 継続に委ねる。
    if matches!(
        lang_id,
        LangId::Javascript | LangId::Typescript | LangId::Tsx
    ) && matches!(node.kind(), "arrow_function" | "function_expression")
    {
        return js_ts_binding_name_for_function(node, source);
    }

    // まず "name" フィールドを試行（多くの言語で共通）
    if let Some(name_node) = node.child_by_field_name("name") {
        return name_node.utf8_text(source).ok().map(|s| s.to_string());
    }

    // Kotlin: function_declaration > simple_identifier
    if lang_id == LangId::Kotlin {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "simple_identifier" {
                return child.utf8_text(source).ok().map(|s| s.to_string());
            }
        }
    }

    // C/C++ の function_definition: declarator を辿って末尾の名前ノードを取る。
    // ポインタ返り (pointer_declarator) や参照返り (reference_declarator) では
    // function_declarator が宣言子に包まれるため、単純な 2 段では届かず
    // `foo(args)` のような宣言子テキスト全体を誤って名前にしてしまう。
    if let Some(decl) = node.child_by_field_name("declarator") {
        if let Some(name) = c_function_name_from_declarator(decl, source) {
            return Some(name);
        }
        return decl.utf8_text(source).ok().map(|s| s.to_string());
    }

    // フォールバック: 最初の identifier 子ノード
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let k = child.kind();
        if k == "identifier" || k == "type_identifier" || k == "field_identifier" || k == "word" {
            return child.utf8_text(source).ok().map(|s| s.to_string());
        }
    }

    None
}

/// JS/TS の arrow/function expression の binding 名を親ノードから解決する。
/// `const f = () => {}` → f / `{ handler: () => {} }` → handler /
/// `class C { f = () => {} }` → f / `obj.f = () => {}` → f。
/// binding が無い場合のみ named function expression 自身の name (IIFE 等) に fallback。
fn js_ts_binding_name_for_function(node: Node<'_>, source: &[u8]) -> Option<String> {
    let from_parent = node.parent().and_then(|parent| match parent.kind() {
        "variable_declarator" => parent.child_by_field_name("name"),
        "pair" => parent.child_by_field_name("key"),
        "public_field_definition" | "field_definition" => parent
            .child_by_field_name("name")
            .or_else(|| parent.child_by_field_name("property")),
        "assignment_expression" => {
            let left = parent.child_by_field_name("left")?;
            match left.kind() {
                // `module.exports.foo = () => {}` / `this.foo = () => {}` は末尾 property を採る
                "member_expression" => left.child_by_field_name("property"),
                _ => Some(left),
            }
        }
        _ => None,
    });
    if let Some(name_node) = from_parent {
        return name_node.utf8_text(source).ok().map(str::to_string);
    }
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
        .map(str::to_string)
}

/// C/C++ の宣言子ノードから末尾の関数名 (identifier / field_identifier /
/// qualified_identifier) を取り出す。pointer_declarator / reference_declarator /
/// parenthesized_declarator / array_declarator を再帰的に辿る。
fn c_function_name_from_declarator(decl: Node<'_>, source: &[u8]) -> Option<String> {
    match decl.kind() {
        "identifier" | "field_identifier" => decl.utf8_text(source).ok().map(|s| s.to_string()),
        // Foo::bar の末尾識別子を返す
        "qualified_identifier" => decl
            .child_by_field_name("name")
            .and_then(|n| c_function_name_from_declarator(n, source))
            .or_else(|| decl.utf8_text(source).ok().map(|s| s.to_string())),
        "function_declarator"
        | "pointer_declarator"
        | "reference_declarator"
        | "parenthesized_declarator"
        | "array_declarator" => decl
            .child_by_field_name("declarator")
            .or_else(|| {
                // reference_declarator は declarator フィールドを持たないため子から探す
                let mut c = decl.walk();
                decl.children(&mut c).find(|ch| {
                    matches!(
                        ch.kind(),
                        "function_declarator"
                            | "pointer_declarator"
                            | "reference_declarator"
                            | "parenthesized_declarator"
                            | "identifier"
                            | "field_identifier"
                            | "qualified_identifier"
                    )
                })
            })
            .and_then(|d| c_function_name_from_declarator(d, source)),
        _ => None,
    }
}

/// 言語別の call expression 用 tree-sitter クエリを返す。
fn call_query(lang_id: LangId) -> &'static str {
    match lang_id {
        LangId::Rust => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (field_expression field: (field_identifier) @method.callee))
            (call_expression function: (scoped_identifier name: (identifier) @scoped.callee))
            "#
        }
        LangId::C => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            "#
        }
        LangId::Cpp => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (field_expression field: (field_identifier) @method.callee))
            (call_expression function: (qualified_identifier name: (identifier) @scoped.callee))
            "#
        }
        LangId::Python => {
            r#"
            (call function: (identifier) @direct.callee)
            (call function: (attribute attribute: (identifier) @method.callee))
            "#
        }
        LangId::Javascript => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (member_expression property: (property_identifier) @method.callee))
            "#
        }
        LangId::Typescript | LangId::Tsx => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (member_expression property: (property_identifier) @method.callee))
            "#
        }
        LangId::Go => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (selector_expression field: (field_identifier) @method.callee))
            "#
        }
        LangId::Php => {
            r#"
            (function_call_expression function: (name) @direct.callee)
            (member_call_expression name: (name) @method.callee)
            (scoped_call_expression name: (name) @scoped.callee)
            "#
        }
        LangId::Java => {
            // name: は object の有無に依らず全 method_invocation を捕捉するため 1 本で足りる
            // (object 付きの別パターンを併記すると同一呼び出しが二重計上される)。
            r#"
            (method_invocation name: (identifier) @direct.callee)
            "#
        }
        LangId::Kotlin => {
            // メソッド名は navigation_suffix 配下にある (直接子の simple_identifier は
            // receiver 側で、`repo.save()` の callee が "repo" になってしまう)。
            r#"
            (call_expression (simple_identifier) @direct.callee)
            (call_expression (navigation_expression (navigation_suffix (simple_identifier) @method.callee)))
            "#
        }
        LangId::Swift => {
            r#"
            (call_expression (simple_identifier) @direct.callee)
            (call_expression
              (navigation_expression
                (navigation_suffix suffix: (simple_identifier) @method.callee)))
            "#
        }
        LangId::CSharp => {
            r#"
            (invocation_expression function: (identifier) @direct.callee)
            (invocation_expression function: (member_access_expression name: (identifier) @method.callee))
            "#
        }
        LangId::Bash => {
            // 第1行: 通常のコマンド呼び出し `foo arg1 arg2` → foo が callee
            // 第2パターン: trap ハンドラ `trap <fn> SIGNAL...` → 第1引数 <fn> のみ callee
            //               (cleanup ハンドラ等として参照される関数を internal 扱いするため)
            //               `.` アンカーで command_name 直後の最初の argument のみを捕捉し、
            //               シグナル名 (EXIT/INT/TERM 等) がキャプチャされるのを防ぐ。
            r#"
            (command name: (command_name (word) @direct.callee))
            (command
              name: (command_name (word) @_cmd) .
              argument: (word) @direct.callee
              (#eq? @_cmd "trap"))
            "#
        }
        LangId::Ruby => {
            r#"
            (call method: (identifier) @direct.callee)
            "#
        }
        LangId::Zig => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (field_expression member: (identifier) @method.callee))
            "#
        }
        LangId::Xojo => {
            r#"
            (array_or_call_expression function: (identifier) @direct.callee)
            (array_or_call_expression function: (member_expression property: (identifier) @method.callee))
            "#
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;

    /// Rust の関数呼び出しを正しく抽出する
    #[test]
    fn extract_calls_rust() {
        let source = b"fn main() { helper(); }\nfn helper() {}";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Rust, None).unwrap();
        assert!(!edges.is_empty());
        assert!(edges.iter().any(|e| e.callee.name == "helper"));
        assert!(edges.iter().any(|e| e.caller.name == "main"));
    }

    /// filter_function で特定の caller のみ抽出できる
    #[test]
    fn extract_calls_with_filter() {
        let source = b"fn a() { b(); }\nfn b() { c(); }\nfn c() {}";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Rust, Some("a")).unwrap();
        assert!(edges.iter().all(|e| e.caller.name == "a"));
    }

    /// Python の関数呼び出しを抽出する
    #[test]
    fn extract_calls_python() {
        let source = b"def main():\n    helper()\ndef helper():\n    pass\n";
        let tree = parser::parse_source(source, LangId::Python).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Python, None).unwrap();
        assert!(edges.iter().any(|e| e.callee.name == "helper"));
    }

    /// JavaScript の関数呼び出しを抽出する
    #[test]
    fn extract_calls_javascript() {
        let source = b"function main() { helper(); }\nfunction helper() {}";
        let tree = parser::parse_source(source, LangId::Javascript).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Javascript, None).unwrap();
        assert!(edges.iter().any(|e| e.callee.name == "helper"));
    }

    /// C のポインタ返り関数 `Type *foo()` の caller 名が宣言子テキスト全体
    /// (`foo(args)`) に汚染されず識別子 `foo` になる (回帰)。
    #[test]
    fn extract_calls_c_pointer_returning_caller_name() {
        let source = b"void helper(int n) {}\nint *make(int n) { helper(n); return 0; }";
        let tree = parser::parse_source(source, LangId::C).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::C, None).unwrap();
        assert!(
            edges.iter().any(|e| e.caller.name == "make"),
            "caller 名が make: {:?}",
            edges.iter().map(|e| &e.caller.name).collect::<Vec<_>>()
        );
        assert!(
            !edges.iter().any(|e| e.caller.name.contains('(')),
            "caller 名に宣言子テキストが混入していない"
        );
    }

    /// C++ の参照返りメソッド `T& m()` の caller 名も識別子になる (回帰)。
    #[test]
    fn extract_calls_cpp_reference_returning_caller_name() {
        let source =
            b"int g(int x) { return x; }\nstruct P { int v; int& at() { g(v); return v; } };";
        let tree = parser::parse_source(source, LangId::Cpp).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Cpp, None).unwrap();
        assert!(
            !edges.iter().any(|e| e.caller.name.contains('(')),
            "caller 名に宣言子テキストが混入していない: {:?}",
            edges.iter().map(|e| &e.caller.name).collect::<Vec<_>>()
        );
    }

    /// 空クエリの言語でも空配列を返しパニックしない
    #[test]
    fn extract_calls_empty_source() {
        let source = b"";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Rust, None).unwrap();
        assert!(edges.is_empty());
    }

    /// 全言語の call_query が空文字でないことを確認（Bash 含む）
    #[test]
    fn call_query_all_languages_non_empty() {
        let languages = [
            LangId::Rust,
            LangId::C,
            LangId::Cpp,
            LangId::Python,
            LangId::Javascript,
            LangId::Typescript,
            LangId::Tsx,
            LangId::Go,
            LangId::Php,
            LangId::Java,
            LangId::Kotlin,
            LangId::Swift,
            LangId::CSharp,
            LangId::Bash,
            LangId::Ruby,
            LangId::Zig,
            LangId::Xojo,
        ];
        for lang in &languages {
            let q = call_query(*lang);
            assert!(!q.is_empty(), "{:?} should have a call query", lang);
        }
    }

    /// bash の `trap <fn> SIGNAL` で指定される関数名を callee として抽出できる。
    /// extract_calls はトップレベル呼び出しを除外するので extract_all_callees で確認する。
    #[test]
    fn extract_all_callees_bash_trap_handler() {
        let source = b"#!/usr/bin/env bash\n\
            stop_memory_sampler() { echo stop; }\n\
            trap stop_memory_sampler EXIT\n\
            trap 'echo literal' INT\n\
            trap \"echo double\" TERM\n";
        let tree = parser::parse_source(source, LangId::Bash).unwrap();
        let callees = extract_all_callees(tree.root_node(), source, LangId::Bash).unwrap();
        assert!(
            callees.contains("stop_memory_sampler"),
            "trap <fn> の関数名は callee として抽出されるべき。got: {callees:?}"
        );
        // 文字列引数はコマンド名ではないので拾わない
        assert!(!callees.contains("echo literal"));
        assert!(!callees.contains("echo double"));
        // シグナル名 (EXIT/INT/TERM) は handler ではないため callee に含めてはならない
        assert!(
            !callees.contains("EXIT"),
            "シグナル名 EXIT は callee に含めてはならない。got: {callees:?}"
        );
        assert!(!callees.contains("INT"));
        assert!(!callees.contains("TERM"));
    }

    /// `trap <fn> EXIT INT TERM` のように複数シグナルを指定した場合でも、
    /// ハンドラ関数名は 1 回だけ callee として扱われる (HashSet なので重複はない)。
    #[test]
    fn extract_all_callees_bash_trap_multiple_signals() {
        let source = b"#!/usr/bin/env bash\n\
            cleanup() { echo clean; }\n\
            trap cleanup EXIT INT TERM HUP\n";
        let tree = parser::parse_source(source, LangId::Bash).unwrap();
        let callees = extract_all_callees(tree.root_node(), source, LangId::Bash).unwrap();
        assert!(
            callees.contains("cleanup"),
            "複数シグナル trap でも handler は callee に含まれる。got: {callees:?}"
        );
        // `trap - EXIT` のようなハンドラ解除も誤認されないこと
        // ("-" は argument[0] として現れるが word ノードでもマッチする可能性があるため確認)
        assert!(!callees.contains("EXIT"));
        assert!(!callees.contains("HUP"));
    }

    /// bash トップレベル呼び出しも extract_all_callees で拾える
    /// (extract_calls は caller なしで除外するが extract_all_callees は拾う)
    #[test]
    fn extract_all_callees_bash_top_level_command() {
        let source = b"#!/usr/bin/env bash\n\
            timed() { \"$@\"; }\n\
            timed echo hi\n";
        let tree = parser::parse_source(source, LangId::Bash).unwrap();
        let callees = extract_all_callees(tree.root_node(), source, LangId::Bash).unwrap();
        assert!(
            callees.contains("timed"),
            "トップレベルコマンドも callee として扱われるべき。got: {callees:?}"
        );
    }

    /// Kotlin のメソッド呼び出し `repo.save()` は receiver ではなくメソッド名を callee に
    /// 抽出する (navigation_suffix 配下の simple_identifier を捕捉する回帰防止)。
    #[test]
    fn kotlin_method_call_captures_method_name_not_receiver() {
        let source = b"fun run(repo: Repo) {\n    repo.save(\"x\")\n    a.b.c()\n}\n";
        let tree = parser::parse_source(source, LangId::Kotlin).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Kotlin, None).unwrap();
        let callees: Vec<&str> = edges.iter().map(|e| e.callee.name.as_str()).collect();
        assert!(
            callees.contains(&"save"),
            "メソッド名 save が callee になるべき。got: {callees:?}"
        );
        assert!(
            callees.contains(&"c"),
            "チェーン末尾のメソッド名 c が callee になるべき。got: {callees:?}"
        );
        assert!(
            !callees.contains(&"repo"),
            "receiver は callee に含めてはならない。got: {callees:?}"
        );
    }

    /// Java のメソッド呼び出し `b.work()` は 1 呼び出しにつき edge ちょうど 1 件
    /// (object 付きパターン併記による同一呼び出しの二重計上の回帰防止)。
    #[test]
    fn java_method_call_edge_is_not_duplicated() {
        let source = b"class A { void run(B b) { b.work(); } }";
        let tree = parser::parse_source(source, LangId::Java).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Java, None).unwrap();
        let work_edges = edges.iter().filter(|e| e.callee.name == "work").count();
        assert_eq!(
            work_edges,
            1,
            "work の edge はちょうど 1 件であるべき。edges: {:?}",
            edges.iter().map(|e| &e.callee.name).collect::<Vec<_>>()
        );
    }

    /// function_node_kinds が全言語で空でないことを確認
    #[test]
    fn function_node_kinds_all_languages() {
        let languages = [
            LangId::Rust,
            LangId::C,
            LangId::Cpp,
            LangId::Python,
            LangId::Javascript,
            LangId::Typescript,
            LangId::Tsx,
            LangId::Go,
            LangId::Php,
            LangId::Java,
            LangId::Kotlin,
            LangId::Swift,
            LangId::CSharp,
            LangId::Bash,
            LangId::Ruby,
            LangId::Zig,
            LangId::Xojo,
        ];
        for lang in &languages {
            let kinds = function_node_kinds(*lang);
            assert!(
                !kinds.is_empty(),
                "{:?} should have function node kinds",
                lang
            );
        }
    }

    /// TS の arrow function / function expression の caller は親の binding 名で解決する。
    /// 単一引数 arrow (`x => ...`) が引数名 "x" に誤帰属したり、`(req) =>` 形や
    /// function expression の edge が消失したりしない
    /// (Issue 2026-07-10-ts-arrow-function-caller-attribution)。
    #[test]
    fn ts_arrow_and_function_expression_caller_binding_names() {
        let source = b"function target() {}\n\
                       export const single = x => { target(); return x; };\n\
                       export const handler = (req) => { target(); return req; };\n\
                       function outer() {\n\
                           const inner = (a, b) => { target(); return a + b; };\n\
                           return inner;\n\
                       }\n\
                       const f = function named() { target(); };\n";
        let tree = parser::parse_source(source, LangId::Typescript).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Typescript, None).unwrap();
        let callers: Vec<&str> = edges
            .iter()
            .filter(|e| e.callee.name == "target")
            .map(|e| e.caller.name.as_str())
            .collect();
        assert!(callers.contains(&"single"), "callers: {callers:?}");
        assert!(callers.contains(&"handler"), "callers: {callers:?}");
        assert!(callers.contains(&"inner"), "callers: {callers:?}");
        assert!(callers.contains(&"f"), "callers: {callers:?}");
        assert!(
            !callers.contains(&"x"),
            "arrow の引数名を caller にしない: {callers:?}"
        );
    }

    /// object literal の pair (`{ handler: () => {} }`) は key を caller 名にし、
    /// 匿名 callback (binding 無し) 内の呼び出しは外側の named function へ climb する。
    #[test]
    fn ts_pair_key_and_anonymous_callback_climb() {
        let source = b"function target() {}\n\
                       export const obj = { onDone: () => { target(); } };\n\
                       function wrapper() {\n\
                           [1].map(function () { target(); });\n\
                       }\n";
        let tree = parser::parse_source(source, LangId::Typescript).unwrap();
        let edges = extract_calls(tree.root_node(), source, LangId::Typescript, None).unwrap();
        let callers: Vec<&str> = edges
            .iter()
            .filter(|e| e.callee.name == "target")
            .map(|e| e.caller.name.as_str())
            .collect();
        assert!(callers.contains(&"onDone"), "callers: {callers:?}");
        assert!(
            callers.contains(&"wrapper"),
            "匿名 callback は外側 named function へ帰属: {callers:?}"
        );
    }
}
