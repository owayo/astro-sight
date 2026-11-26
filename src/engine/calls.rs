use anyhow::Result;
use std::collections::HashSet;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, QueryCursor};

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

    let query = crate::engine::query_cache::cached_query(lang_id, query_src)?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

    // callee の capture を、match の順・capture の順のまま集める (出力の順序)。
    let mut callees = Vec::new();
    while let Some(m) = matches.next() {
        for capture in m.captures() {
            let capture_name = &query.capture_names()[capture.index as usize];
            if capture_name.ends_with("callee") {
                callees.push(capture.node);
            }
        }
    }
    let contexts = CallerContexts::collect(root, &callees, source, lang_id);

    let mut edges = Vec::new();
    for node in callees {
        let callee_name = node.utf8_text(source).unwrap_or("").to_string();
        if callee_name.is_empty() {
            continue;
        }

        // Find the enclosing function (caller) and the call_expression (the parent of the
        // callee identifier)
        let (caller, call_node) = contexts.caller_and_call_node(node, source, lang_id);
        let caller = match caller {
            Some(c) => c,
            None => continue, // top-level call, skip
        };

        if let Some(filter) = filter_function
            && caller.name != filter
        {
            continue;
        }

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

    Ok(edges)
}

/// 呼び出し (callee の capture) ごとの、呼び出し元 (最も近い名前を解決できる関数の祖先) と
/// 呼び出し式のノード (callee の親)。
///
/// `Node::parent` は根から降りて親を探すので、呼び出しごとに祖先を `parent` でたどると
/// 入れ子の呼び出し (`wrap(wrap(…(() => 1)…))`) では深さの 2 乗以上の時間がかかり、
/// 5 万段で終わらなかった。木を 1 回だけ前順にたどり、祖先の列と「その位置で最も近い
/// 名前付きの関数」を持ち回して求める。結果は `find_enclosing_function` で祖先をたどった
/// 場合と同じ。
struct CallerContexts<'tree> {
    /// 名前を解決できた関数 (出現順)。
    callers: Vec<CallEndpoint>,
    /// callee のノード id → (呼び出し元の `callers` の添字、呼び出し式のノード)。
    by_callee: std::collections::HashMap<usize, (Option<usize>, Node<'tree>)>,
}

/// 参照位置から最も内側の名前付き関数を引く。隣接する宣言は列でも区別する。
#[derive(Default)]
pub(crate) struct EnclosingCallers {
    boundaries: Vec<(tree_sitter::Point, Option<usize>)>,
    names: Vec<String>,
}

impl EnclosingCallers {
    pub(crate) fn collect(root: Node<'_>, source: &[u8], lang: LangId) -> Self {
        use crate::engine::tree_walk::{Children, Walk, walk_preorder};
        use std::ops::ControlFlow;

        let mut index = Self::default();
        let kinds = function_node_kinds(lang);
        let mut ancestors = Vec::new();
        let mut open = Vec::<(tree_sitter::Point, usize)>::new();
        let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
            ancestors.truncate(depth);
            if kinds.contains(&node.kind())
                && let Some(name) =
                    find_function_name(node, ancestors.last().copied(), source, lang)
            {
                let start = node.start_position();
                while open.last().is_some_and(|&(end, _)| end <= start) {
                    let (end, _) = open.pop().unwrap();
                    index.boundaries.push((end, open.last().map(|&(_, i)| i)));
                }
                let i = index.names.len();
                index.names.push(name);
                index.boundaries.push((start, Some(i)));
                open.push((node.end_position(), i));
            }
            ancestors.push(node);
            ControlFlow::Continue(Walk::Descend)
        });
        while let Some((end, _)) = open.pop() {
            index.boundaries.push((end, open.last().map(|&(_, i)| i)));
        }
        index
    }

    pub(crate) fn at(&self, line: usize, column: usize) -> Option<&str> {
        let point = tree_sitter::Point::new(line, column);
        let end = self
            .boundaries
            .partition_point(|&(start, _)| start <= point);
        let (_, name) = self.boundaries[..end].last()?;
        Some(&self.names[(*name)?])
    }
}

impl<'tree> CallerContexts<'tree> {
    fn collect(root: Node<'tree>, callees: &[Node<'tree>], source: &[u8], lang_id: LangId) -> Self {
        use std::ops::ControlFlow;

        use crate::engine::tree_walk::{Children, Walk, walk_preorder};

        let mut contexts = Self {
            callers: Vec::new(),
            by_callee: std::collections::HashMap::with_capacity(callees.len()),
        };
        if callees.is_empty() {
            return contexts;
        }
        let wanted: HashSet<usize> = callees.iter().map(|node| node.id()).collect();
        let func_kinds = function_node_kinds(lang_id);
        // 深さごとの祖先 (`path[d]` は深さ d の祖先)。
        let mut path: Vec<Node<'tree>> = Vec::new();
        // 祖先の関数ノードの (深さ, その関数の内側で最も近い名前付きの関数)。
        let mut frames: Vec<(usize, Option<usize>)> = Vec::new();
        let _: ControlFlow<()> = walk_preorder(root, Children::All, |node, depth| {
            path.truncate(depth);
            while frames
                .last()
                .is_some_and(|&(frame_depth, _)| frame_depth >= depth)
            {
                frames.pop();
            }
            let nearest = frames.last().and_then(|&(_, named)| named);
            let parent = depth.checked_sub(1).map(|d| path[d]);
            if wanted.contains(&node.id()) {
                contexts
                    .by_callee
                    .insert(node.id(), (nearest, parent.unwrap_or(node)));
            }
            if func_kinds.contains(&node.kind()) {
                // 名前を解決できない関数 (匿名 callback) は飛ばし、外側の名前付きの関数を使う。
                let named = match find_function_name(node, parent, source, lang_id) {
                    Some(name) => {
                        contexts.callers.push(CallEndpoint {
                            name,
                            range: Range::from(node.range()),
                        });
                        Some(contexts.callers.len() - 1)
                    }
                    None => nearest,
                };
                frames.push((depth, named));
            }
            path.push(node);
            ControlFlow::Continue(Walk::Descend)
        });
        contexts
    }

    /// `callee` の呼び出し元と呼び出し式のノード。走査で見つからなかった場合 (query の
    /// capture は root の部分木にあるので起きない) は祖先をたどって求める。
    fn caller_and_call_node(
        &self,
        callee: Node<'tree>,
        source: &[u8],
        lang_id: LangId,
    ) -> (Option<CallEndpoint>, Node<'tree>) {
        match self.by_callee.get(&callee.id()) {
            Some(&(caller, call_node)) => (caller.map(|ix| self.callers[ix].clone()), call_node),
            None => (
                find_enclosing_function(callee, source, lang_id),
                callee.parent().unwrap_or(callee),
            ),
        }
    }
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

    let query = crate::engine::query_cache::cached_query(lang_id, query_src)?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, root, source);

    let mut callees = HashSet::new();
    while let Some(m) = matches.next() {
        for capture in m.captures() {
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
            && let Some(name) = find_function_name(n, n.parent(), source, lang_id)
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
        // generator (`function* gen()` / `const g = function* () {}`) も呼び出し元になる。
        // 列挙しないと generator 内の呼び出しが外側の関数へ誤帰属するか、トップレベルの
        // generator では edge ごと消える。
        LangId::Javascript => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
            "function_expression",
            "generator_function_declaration",
            "generator_function",
        ],
        LangId::Typescript | LangId::Tsx => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
            "function_expression",
            "generator_function_declaration",
            "generator_function",
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

/// 関数ノードの名前を抽出する。`parent` は `node` の親 (JS/TS の束縛名の解決に使う。
/// 走査中の呼び出し元は祖先の列から渡し、`Node::parent` を呼ばずに済ませる)。
fn find_function_name(
    node: Node<'_>,
    parent: Option<Node<'_>>,
    source: &[u8],
    lang_id: LangId,
) -> Option<String> {
    // JS/TS の arrow/function expression は自身の "name" では外部から参照できない
    // (named function expression の name は内部スコープ専用)。親の binding 名を
    // caller 名とし、binding が無い匿名 callback では偽名 (最初の identifier 子 =
    // 引数名) を作らず None を返して呼び出し元の climb 継続に委ねる。
    if matches!(
        lang_id,
        LangId::Javascript | LangId::Typescript | LangId::Tsx
    ) && matches!(
        node.kind(),
        "arrow_function" | "function_expression" | "generator_function"
    ) {
        return js_ts_binding_name_for_function(node, parent, source);
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
/// `parent` は `node` の親。
fn js_ts_binding_name_for_function(
    node: Node<'_>,
    parent: Option<Node<'_>>,
    source: &[u8],
) -> Option<String> {
    let from_parent = parent.and_then(|parent| match parent.kind() {
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
/// parenthesized_declarator / array_declarator を辿る。
///
/// 宣言子は `int ****f(void)` のように任意に深く入れ子にできるので、1 段ごとに再帰せず
/// ループで辿る。末尾の名前が取れなかったときは、辿ってきた qualified_identifier のうち
/// 最も内側のものの全体 (`Foo::bar<T>` など) を返す (再帰で辿った場合と同じ)。
fn c_function_name_from_declarator(decl: Node<'_>, source: &[u8]) -> Option<String> {
    let mut qualified: Vec<Node<'_>> = Vec::new();
    let mut current = Some(decl);
    let name = loop {
        let Some(decl) = current else {
            break None;
        };
        current = match decl.kind() {
            "identifier" | "field_identifier" => {
                break decl.utf8_text(source).ok().map(|s| s.to_string());
            }
            // Foo::bar の末尾識別子を返す
            "qualified_identifier" => {
                qualified.push(decl);
                decl.child_by_field_name("name")
            }
            "function_declarator"
            | "pointer_declarator"
            | "reference_declarator"
            | "parenthesized_declarator"
            | "array_declarator" => decl.child_by_field_name("declarator").or_else(|| {
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
            }),
            _ => None,
        };
    };
    name.or_else(|| {
        qualified
            .iter()
            .rev()
            .find_map(|q| q.utf8_text(source).ok().map(|s| s.to_string()))
    })
}

/// 言語別の call expression 用 tree-sitter クエリを返す。
fn call_query(lang_id: LangId) -> &'static str {
    match lang_id {
        // turbofish 付き呼び出し (`parse::<u8>()` / `s.fetch::<u32>()` / `T::make::<i32>()`) は
        // function が `generic_function` に包まれる。列挙しないと calls / sequence から
        // 呼び出しごと消える (refs / dead-code 側は generic_function を透過済み)。
        LangId::Rust => {
            r#"
            (call_expression function: (identifier) @direct.callee)
            (call_expression function: (field_expression field: (field_identifier) @method.callee))
            (call_expression function: (scoped_identifier name: (identifier) @scoped.callee))
            (call_expression function: (generic_function function: (identifier) @direct.callee))
            (call_expression function: (generic_function function: (field_expression field: (field_identifier) @method.callee)))
            (call_expression function: (generic_function function: (scoped_identifier name: (identifier) @scoped.callee)))
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
        // nullsafe 呼び出し `$a?->fmt()` は member_call_expression とは別ノード。
        LangId::Php => {
            r#"
            (function_call_expression function: (name) @direct.callee)
            (member_call_expression name: (name) @method.callee)
            (nullsafe_member_call_expression name: (name) @method.callee)
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

    /// 修正前の再帰版 (浅い入力でだけ使う参照実装)。
    fn c_function_name_from_declarator_recursive(decl: Node<'_>, source: &[u8]) -> Option<String> {
        match decl.kind() {
            "identifier" | "field_identifier" => decl.utf8_text(source).ok().map(|s| s.to_string()),
            "qualified_identifier" => decl
                .child_by_field_name("name")
                .and_then(|n| c_function_name_from_declarator_recursive(n, source))
                .or_else(|| decl.utf8_text(source).ok().map(|s| s.to_string())),
            "function_declarator"
            | "pointer_declarator"
            | "reference_declarator"
            | "parenthesized_declarator"
            | "array_declarator" => decl
                .child_by_field_name("declarator")
                .or_else(|| {
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
                .and_then(|d| c_function_name_from_declarator_recursive(d, source)),
            _ => None,
        }
    }

    /// 関数の宣言子からの名前の取り出しは、ループ版と再帰版で一致する。ポインタ・参照の
    /// 戻り値、クラス外定義、入れ子の名前空間、テンプレート (末尾の名前が取れず
    /// qualified_identifier 全体へ戻る場合) を含む。
    #[test]
    fn c_function_name_from_declarator_matches_recursive_version() {
        let source = "int plain(void) { return 0; }\n\
                      int **ptr(void) { return 0; }\n\
                      int &ref_ret() { static int v; return v; }\n\
                      int (*paren(void))(int) { return 0; }\n\
                      void Outer::Inner::method() {}\n\
                      template <typename T> void Box<T>::put(T v) {}\n\
                      void ns::Box<int>::get() {}\n\
                      int Foo::operator+(int x) { return x; }\n\
                      Foo::~Foo() {}\n";
        let tree = parser::parse_source(source.as_bytes(), LangId::Cpp).unwrap();
        let mut stack = vec![tree.root_node()];
        let mut compared = 0;
        while let Some(n) = stack.pop() {
            if n.kind() == "function_definition"
                && let Some(decl) = n.child_by_field_name("declarator")
            {
                assert_eq!(
                    c_function_name_from_declarator(decl, source.as_bytes()),
                    c_function_name_from_declarator_recursive(decl, source.as_bytes()),
                    "{:?}",
                    decl.utf8_text(source.as_bytes())
                );
                compared += 1;
            }
            let mut cursor = n.walk();
            let children: Vec<_> = n.named_children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        assert!(compared >= 9, "{compared}");
    }

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

    /// Rust の turbofish 付き呼び出しと PHP の nullsafe 呼び出しも callee として捕捉する。
    /// どちらも通常の呼び出しとは別ノードに包まれ、旧クエリでは calls / sequence から消えていた。
    #[test]
    fn rust_turbofish_and_php_nullsafe_calls_are_captured() {
        let rust = b"fn run(s: &Store) -> u32 { s.fetch::<u32>() + parse::<u8>() + Wrapper::make::<i32>() + plain() }\n";
        let tree = parser::parse_source(rust, LangId::Rust).unwrap();
        let edges = extract_calls(tree.root_node(), rust, LangId::Rust, None).unwrap();
        let mut callees: Vec<&str> = edges.iter().map(|e| e.callee.name.as_str()).collect();
        callees.sort_unstable();
        assert_eq!(callees, ["fetch", "make", "parse", "plain"], "{edges:?}");

        let php = b"<?php\nfunction f($a) { return $a?->fmt() . $a->plain(); }\n";
        let tree = parser::parse_source(php, LangId::Php).unwrap();
        let edges = extract_calls(tree.root_node(), php, LangId::Php, None).unwrap();
        let mut callees: Vec<&str> = edges.iter().map(|e| e.callee.name.as_str()).collect();
        callees.sort_unstable();
        assert_eq!(callees, ["fmt", "plain"], "{edges:?}");
    }

    /// generator (`function* gen()` / `const g = function* () {}`) も caller になる。
    /// 列挙していないと、トップレベルの generator 内の呼び出しは edge ごと消え、
    /// 関数内の generator 式の呼び出しは外側の関数へ誤帰属していた。
    #[test]
    fn ts_generator_functions_are_callers() {
        let source = b"function target() {}\n\
                       export function* gen() { target(); yield 1; }\n\
                       function outer() {\n\
                           const inner = function* () { target(); };\n\
                           return inner;\n\
                       }\n";
        for lang in [LangId::Typescript, LangId::Javascript] {
            let tree = parser::parse_source(source, lang).unwrap();
            let edges = extract_calls(tree.root_node(), source, lang, None).unwrap();
            let callers: Vec<&str> = edges
                .iter()
                .filter(|e| e.callee.name == "target")
                .map(|e| e.caller.name.as_str())
                .collect();
            assert!(callers.contains(&"gen"), "{lang:?} callers: {callers:?}");
            assert!(
                callers.contains(&"inner") && !callers.contains(&"outer"),
                "{lang:?} generator 式は binding 名に帰属する: {callers:?}"
            );
        }
    }

    /// 修正前の実装 (呼び出しごとに `Node::parent` で祖先をたどる)。浅い入力でだけ使う。
    fn extract_calls_climbing(
        root: Node<'_>,
        source: &[u8],
        lang_id: LangId,
        filter_function: Option<&str>,
    ) -> Vec<CallEdge> {
        let query = crate::engine::query_cache::cached_query(lang_id, call_query(lang_id)).unwrap();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root, source);
        let mut edges = Vec::new();
        while let Some(m) = matches.next() {
            for capture in m.captures() {
                let node = capture.node;
                let capture_name = &query.capture_names()[capture.index as usize];
                if !capture_name.ends_with("callee") {
                    continue;
                }
                let callee_name = node.utf8_text(source).unwrap_or("").to_string();
                if callee_name.is_empty() {
                    continue;
                }
                let Some(caller) = find_enclosing_function(node, source, lang_id) else {
                    continue;
                };
                if let Some(filter) = filter_function
                    && caller.name != filter
                {
                    continue;
                }
                let call_node = node.parent().unwrap_or(node);
                edges.push(CallEdge {
                    caller,
                    callee: CallEndpoint {
                        name: callee_name,
                        range: Range::from(call_node.range()),
                    },
                    call_site: CallSite {
                        line: node.start_position().row,
                        column: node.start_position().column,
                    },
                });
            }
        }
        edges
    }

    /// 1 回の走査で呼び出し元を求める版は、祖先をたどっていた修正前の実装と同じ edge を
    /// 同じ順に返す。tests/fixtures の全言語のサンプルに、JS/TS の束縛名の解決
    /// (変数・object の key・class field・代入先・IIFE・generator・匿名 callback の飛ばし)、
    /// 入れ子の関数、トップレベルの呼び出しを足し、caller での絞り込みも比べる。
    #[test]
    fn extract_calls_matches_climbing_version() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let mut cases: Vec<(LangId, String)> = Vec::new();
        for entry in std::fs::read_dir(&fixtures).unwrap() {
            let path = entry.unwrap().path();
            if !path.is_file() {
                continue;
            }
            let utf8 = camino::Utf8Path::new(path.to_str().unwrap());
            let source = std::fs::read_to_string(&path).unwrap();
            if let Ok(lang) = parser::detect_lang(utf8, source.as_bytes())
                && !lang.is_lexer_only()
            {
                cases.push((lang, source));
            }
        }
        let js = "const a = () => { b(); };\n\
                  const obj = { handler: () => { c(); }, method() { d(); } };\n\
                  class K { field = () => { e(); }; m() { [1].map(x => f(x)); } }\n\
                  module.exports.run = function () { g(); };\n\
                  this.h = () => { i(); };\n\
                  (function iife() { j(); })();\n\
                  function* gen() { yield k(); }\n\
                  const nested = () => () => () => l();\n\
                  setTimeout(() => m(), 0);\n\
                  top();\n";
        // 参照実装は深さの 3 乗の時間がかかるので浅くする (深い入力は統合テストで確かめる)。
        let (open, close) = ("wrap(".repeat(40), ")".repeat(40));
        let deep = format!(
            "function outer() {{ return {open}() => inner(){close}; }}\nconst top = {open}1{close};\n"
        );
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            cases.push((lang, js.to_string()));
            cases.push((lang, deep.clone()));
        }
        cases.push((
            LangId::Cpp,
            "int *ptr(void) { return g(); }\nint &ref() { static int v; h(); return v; }\nvoid Outer::Inner::method() { k(); }\nnamespace n { void free_fn() { l(); } }\n".to_string(),
        ));
        cases.push((
            LangId::Rust,
            "fn outer() { let f = |x| inner(x); f(1); helper::<u8>(); }\nimpl S { fn m(&self) { self.n(); } }\n".to_string(),
        ));

        let (mut compared, mut edges_total) = (0, 0);
        for (lang, source) in &cases {
            let tree = parser::parse_source(source.as_bytes(), *lang).unwrap();
            let root = tree.root_node();
            let expected = extract_calls_climbing(root, source.as_bytes(), *lang, None);
            let actual = extract_calls(root, source.as_bytes(), *lang, None).unwrap();
            assert_eq!(format!("{actual:?}"), format!("{expected:?}"), "{lang:?}");
            edges_total += expected.len();
            let mut callers: Vec<&str> = expected.iter().map(|e| e.caller.name.as_str()).collect();
            callers.sort_unstable();
            callers.dedup();
            callers.push("no_such_caller");
            for caller in callers {
                assert_eq!(
                    format!(
                        "{:?}",
                        extract_calls(root, source.as_bytes(), *lang, Some(caller)).unwrap()
                    ),
                    format!(
                        "{:?}",
                        extract_calls_climbing(root, source.as_bytes(), *lang, Some(caller))
                    ),
                    "{lang:?} / {caller}"
                );
            }
            compared += 1;
        }
        // 空振り防止: 全言語のサンプルと追加の入力を比べ、edge が実際にある
        assert!(
            compared >= 20 && edges_total >= 200,
            "compared={compared} edges={edges_total}"
        );
    }
}
