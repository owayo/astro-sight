//! JS/TS の同名ローカル関数 shadow 解決 (Issue 2026-07-12-api-mod-same-diff-informational)。
//!
//! `is_modified_closed_in_diff` は bare 名で参照を照合するため、変更対象の export 関数と
//! 同名のローカル関数が**別ファイル**に存在すると、そのローカル関数への参照を「対象 API の
//! 未更新 caller かもしれない」と誤読して closed 判定を諦めていた (def_count != 1 ガード)。
//!
//! 本モジュールは参照位置の identifier を AST で解決し、「lexical scope chain を内側から
//! 外側へ辿った最初の同名 value binding が、同一ファイルの function_declaration である」
//! 場合に限り `SameFileFunction` を返す。その参照は対象 API と無関係なローカル呼び出しと
//! みなせるため、closed 判定の対象から除外できる。判定に失敗した参照・property 位置・
//! function 以外の binding (変数 / parameter / import / class 等) はすべて
//! `OtherOrAmbiguous` に倒し、除外しない (fail-closed: 誤って closed に倒さない)。

use tree_sitter::Node;

use crate::language::LangId;

/// 参照 identifier の束縛解決の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalResolution {
    /// 同一ファイル内の `function_declaration` に束縛される (対象 API と無関係な
    /// ローカル関数への参照)。
    SameFileFunction,
    /// それ以外 (property 位置 / 変数・parameter・import 等の binding / 未解決 /
    /// parse 失敗)。除外せず従来の closed 判定に掛ける。
    OtherOrAmbiguous,
}

/// `source` / `tree` は参照ファイルの内容とパース結果 (呼び出し側でキャッシュ)。
/// `(line, column)` は参照の 0-indexed 位置。
pub(crate) fn resolve_reference_binding(
    tree: &tree_sitter::Tree,
    source: &[u8],
    lang: LangId,
    name: &str,
    line: usize,
    column: usize,
) -> LocalResolution {
    if !matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx) {
        return LocalResolution::OtherOrAmbiguous;
    }
    let point = tree_sitter::Point { row: line, column };
    let Some(node) = tree
        .root_node()
        .descendant_for_point_range(point, point)
        .filter(|n| n.kind() == "identifier" && n.utf8_text(source).ok() == Some(name))
    else {
        return LocalResolution::OtherOrAmbiguous;
    };
    // property 位置 (`obj.name` / `ns.name`) の参照はローカル binding と無関係。
    if let Some(parent) = node.parent()
        && parent.kind() == "member_expression"
        && parent
            .child_by_field_name("property")
            .is_some_and(|p| p.id() == node.id())
    {
        return LocalResolution::OtherOrAmbiguous;
    }

    // lexical scope chain を内側から外側へ辿り、最初に同名 binding を持つ scope で判定する。
    match resolve_scope_binding(node, source, name, Namespace::Value) {
        Some(ScopeBinding::FunctionDeclaration) => LocalResolution::SameFileFunction,
        _ => LocalResolution::OtherOrAmbiguous,
    }
}

/// 名前の名前空間。TS の型の位置 (`type_identifier`) は型の宣言だけに束縛される。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Namespace {
    Value,
    Type,
}

/// 字句スコープで見つかった同名の束縛。
#[derive(Debug, Clone, Copy)]
pub(crate) enum ScopeBinding<'tree> {
    /// `function f() {}` / `function* f() {}`
    FunctionDeclaration,
    /// `class C {}` / `abstract class C {}`
    ClassDeclaration,
    /// TS の `interface` / `type` と `enum`
    TypeDeclaration,
    /// `const` / `let` / `var` の単純な名前の束縛 (`variable_declarator`)
    Variable(Node<'tree>),
    /// import 文が導入する束縛 (`import_statement`)
    Import(Node<'tree>),
    /// 引数・catch・ループ変数・分割代入・名前付き関数式の自己参照名・型引数・
    /// `declare` だけの宣言・TS の namespace など、値の由来を確かめられない束縛
    Other,
}

/// `node` (参照の名前) から字句スコープを内側から外側へ辿り、最初に見つかった同名の束縛を返す。
/// ファイル内に束縛が無ければ None (大域・未定義)。
pub(crate) fn resolve_scope_binding<'tree>(
    node: Node<'tree>,
    source: &[u8],
    name: &str,
    namespace: Namespace,
) -> Option<ScopeBinding<'tree>> {
    resolve_binding_scope(node, source, name, namespace).map(|(_, binding)| binding)
}

/// 束縛の種類に加えて、その束縛を持つ scope を返す。
pub(crate) fn resolve_binding_scope<'tree>(
    node: Node<'tree>,
    source: &[u8],
    name: &str,
    namespace: Namespace,
) -> Option<(Node<'tree>, ScopeBinding<'tree>)> {
    let mut cur = node;
    while let Some(scope) = enclosing_scope(cur) {
        if let Some(found) = find_binding_in_scope(scope, source, name, namespace) {
            return Some((scope, found));
        }
        cur = scope;
    }
    None
}

/// scope を作るノードまで親方向へ辿る。
fn enclosing_scope(node: Node<'_>) -> Option<Node<'_>> {
    let mut cur = node.parent()?;
    loop {
        if is_scope_node(cur.kind()) {
            return Some(cur);
        }
        cur = cur.parent()?;
    }
}

fn is_scope_node(kind: &str) -> bool {
    matches!(
        kind,
        "program"
            | "statement_block"
            | "function_declaration"
            | "function_expression"
            | "generator_function_declaration"
            | "generator_function"
            | "arrow_function"
            | "method_definition"
            | "class_static_block"
            | "for_statement"
            | "for_in_statement"
            | "switch_body"
            | "catch_clause"
    )
}

/// `scope` 配下 (ネストした scope の内側は除く) に `name` の binding があるかを探す。
/// `var` の hoisting / block 内宣言を漏らさないため、ネスト scope の境界までは再帰で潜る
/// (function 系の内側の宣言はその scope のものなので潜らない。ただし function 自身の名前は
/// 現 scope の binding として拾う)。
fn find_binding_in_scope<'tree>(
    scope: Node<'tree>,
    source: &[u8],
    name: &str,
    namespace: Namespace,
) -> Option<ScopeBinding<'tree>> {
    if namespace == Namespace::Type {
        // 型の位置は型引数 (`function f<T>()` の T) に束縛される。
        if let Some(params) = scope.child_by_field_name("type_parameters") {
            let mut cursor = params.walk();
            if params.named_children(&mut cursor).any(|param| {
                param
                    .child_by_field_name("name")
                    .is_some_and(|n| n.utf8_text(source).ok() == Some(name))
            }) {
                return Some(ScopeBinding::Other);
            }
        }
    } else {
        // 関数系 scope は parameter も binding。
        if let Some(params) = scope.child_by_field_name("parameters")
            && pattern_binds_name(params, source, name)
        {
            return Some(ScopeBinding::Other);
        }
        // 単一 parameter の arrow (`x => ...`) は parameter フィールドが identifier。
        if let Some(param) = scope.child_by_field_name("parameter")
            && param.kind() == "identifier"
            && param.utf8_text(source).ok() == Some(name)
        {
            return Some(ScopeBinding::Other);
        }
        // catch (e) の e。
        if scope.kind() == "catch_clause"
            && let Some(param) = scope.child_by_field_name("parameter")
            && pattern_binds_name(param, source, name)
        {
            return Some(ScopeBinding::Other);
        }
        // 名前付き function expression の自己参照名は内部 scope の binding。
        if matches!(scope.kind(), "function_expression" | "generator_function")
            && let Some(n) = scope.child_by_field_name("name")
            && n.utf8_text(source).ok() == Some(name)
        {
            return Some(ScopeBinding::Other);
        }
        // for-of / for-in の loop 変数 (`for (const x of xs)`) は left field の bare pattern
        // (lexical_declaration に包まれない) のため body 探索では見えない。ここで拾わないと
        // loop 変数への参照を外側の同名 function_declaration へ誤解決する (fail-open)。
        if scope.kind() == "for_in_statement"
            && let Some(left) = scope.child_by_field_name("left")
            && pattern_binds_name(left, source, name)
        {
            return Some(ScopeBinding::Other);
        }
    }

    let body = scope.child_by_field_name("body").unwrap_or(scope);
    find_binding_in_subtree(body, source, name, false, namespace)
}

fn declares_name(node: Node<'_>, source: &[u8], name: &str) -> bool {
    node.child_by_field_name("name")
        .is_some_and(|n| n.utf8_text(source).ok() == Some(name))
}

/// `node` 配下の宣言 binding を探す。
///
/// `in_nested_block = false` は現 scope 直下 (lexical binding = function / class /
/// let / const / import / var 全部が有効)。`true` はネストした block 内 (module /
/// strict mode の block-level function・let・const は block scope なので現 scope の
/// binding にならない — `var` だけが関数 scope へ hoist されるため拾う)。
/// これを分けないと `if (flag) { function startRecording() {} }` の block 内宣言を
/// 外側の呼び出しの binding と誤認し、未更新 caller を誤って closed にする fail-open
/// になる (codex レビュー指摘)。
fn find_binding_in_subtree<'tree>(
    node: Node<'tree>,
    source: &[u8],
    name: &str,
    in_nested_block: bool,
    namespace: Namespace,
) -> Option<ScopeBinding<'tree>> {
    // ブロック・式の入れ子は入力次第でいくらでも深くなるため、再帰せず明示スタックで辿る。
    // 要素は (調べる子, ネストした block の中か)。子は逆順に積み、左の子の部分木を調べ
    // 終えてから右の子へ進む (再帰版と同じく、最初に見つかった束縛を返す)。
    fn push_named_children<'tree>(
        stack: &mut Vec<(Node<'tree>, bool)>,
        node: Node<'tree>,
        in_nested_block: bool,
    ) {
        let mut cursor = node.walk();
        let start = stack.len();
        stack.extend(
            node.named_children(&mut cursor)
                .map(|child| (child, in_nested_block)),
        );
        stack[start..].reverse();
    }

    let mut stack = Vec::new();
    push_named_children(&mut stack, node, in_nested_block);
    while let Some((child, in_nested_block)) = stack.pop() {
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => {
                if !in_nested_block
                    && namespace == Namespace::Value
                    && declares_name(child, source, name)
                {
                    return Some(ScopeBinding::FunctionDeclaration);
                }
                // 内側 (parameters / body) はネスト scope なので潜らない。
            }
            "class_declaration" | "abstract_class_declaration" => {
                if !in_nested_block && declares_name(child, source, name) {
                    return Some(ScopeBinding::ClassDeclaration);
                }
            }
            // TS の型宣言は型の名前空間だけに束縛する。enum は値と型の両方。
            "interface_declaration" | "type_alias_declaration" => {
                if !in_nested_block
                    && namespace == Namespace::Type
                    && declares_name(child, source, name)
                {
                    return Some(ScopeBinding::TypeDeclaration);
                }
            }
            "enum_declaration" => {
                if !in_nested_block && declares_name(child, source, name) {
                    return Some(ScopeBinding::TypeDeclaration);
                }
            }
            // `declare function f()` / `declare const x` は実体が別の場所にある宣言。
            // TS の namespace (`namespace N {}`) も中身を辿らず由来不明として扱う。
            "ambient_declaration" | "internal_module" | "module" => {
                if !in_nested_block && subtree_declares_name(child, source, name) {
                    return Some(ScopeBinding::Other);
                }
            }
            // let / const は block scope: ネスト block 内の宣言は現 scope の binding でない。
            "lexical_declaration" => {
                if !in_nested_block {
                    push_named_children(&mut stack, child, false);
                }
            }
            // var は関数 scope に hoist されるため、ネスト block 内でも拾う。
            "variable_declaration" => {
                push_named_children(&mut stack, child, in_nested_block);
            }
            "variable_declarator" => {
                if namespace == Namespace::Value
                    && let Some(pat) = child.child_by_field_name("name")
                    && pattern_binds_name(pat, source, name)
                {
                    return Some(if pat.kind() == "identifier" {
                        ScopeBinding::Variable(child)
                    } else {
                        ScopeBinding::Other
                    });
                }
                // 初期化子内はさらに探索 (ネスト scope は下で打ち切られる)。
                if let Some(value) = child.child_by_field_name("value")
                    && !is_scope_node(value.kind())
                {
                    push_named_children(&mut stack, value, in_nested_block);
                }
            }
            "import_statement" => {
                if !in_nested_block && import_binds_name(child, source, name) {
                    return Some(ScopeBinding::Import(child));
                }
            }
            _ => {
                // ネストした function 系 / class body の内側はその scope の宣言なので潜らない。
                if matches!(
                    child.kind(),
                    "function_expression"
                        | "arrow_function"
                        | "generator_function"
                        | "method_definition"
                        | "class_body"
                ) {
                    continue;
                }
                // `for (var x of xs)` の var ヘッダ binding は関数 scope へ hoist される。
                // left は variable_declaration に包まれない bare pattern のため専用処理
                // (let/const は for scope 限りなので拾わない)。
                if namespace == Namespace::Value
                    && child.kind() == "for_in_statement"
                    && child
                        .child_by_field_name("kind")
                        .is_some_and(|k| k.kind() == "var")
                    && let Some(left) = child.child_by_field_name("left")
                    && pattern_binds_name(left, source, name)
                {
                    return Some(ScopeBinding::Other);
                }
                // statement_block / switch_body (および制御構造の中身) へ潜る際は
                // nested block 扱いに切り替え、以降は var 由来の binding だけを拾う。
                let nested =
                    in_nested_block || matches!(child.kind(), "statement_block" | "switch_body");
                push_named_children(&mut stack, child, nested);
            }
        }
    }
    None
}

/// `declare` / TS の namespace 宣言が `name` を宣言するか (宣言名のどれかが一致すれば true)。
fn subtree_declares_name(node: Node<'_>, source: &[u8], name: &str) -> bool {
    if declares_name(node, source, name) {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| match child.kind() {
            "function_signature"
            | "class_declaration"
            | "abstract_class_declaration"
            | "enum_declaration"
            | "interface_declaration"
            | "type_alias_declaration"
            | "internal_module"
            | "module" => declares_name(child, source, name),
            "lexical_declaration" | "variable_declaration" => {
                let mut declarators = child.walk();
                child
                    .named_children(&mut declarators)
                    .filter(|d| d.kind() == "variable_declarator")
                    .any(|d| {
                        d.child_by_field_name("name")
                            .is_some_and(|pat| pattern_binds_name(pat, source, name))
                    })
            }
            _ => false,
        })
}

/// binding パターン (identifier / destructuring / rest / default) が `name` を束縛するか。
/// `ts_const_arg` の binding 列挙とロジックを共有する (検出漏れ = shadow 見逃し =
/// fail-open になるため、実装を 2 箇所に分けない)。
///
/// 束縛位置の規則 (pair_pattern は value 側のみ、default 値 `{ a = X }` / `(a = X)` の
/// 右辺は参照なので left 側のみ) は [`visit_pattern_bindings`] に集約している。右辺まで
/// 辿ると `function f(other = SHARED_DEPS)` の SHARED_DEPS を parameter binding と誤認し、
/// binding 一意性を見る `ts_const_arg` で降格できるケースを取り逃す。
///
/// [`visit_pattern_bindings`]: crate::engine::js_binding_pattern::visit_pattern_bindings
pub(crate) fn pattern_binds_name(pattern: Node<'_>, source: &[u8], name: &str) -> bool {
    crate::engine::js_binding_pattern::visit_pattern_bindings(pattern, &mut |binding| {
        if binding.utf8_text(source).ok() == Some(name) {
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    })
    .is_break()
}

/// import 文が `name` をローカル binding として導入するか (default / named / alias / namespace)。
pub(crate) fn import_binds_name(import_stmt: Node<'_>, source: &[u8], name: &str) -> bool {
    visit_import_bindings(import_stmt, source, |local| local == name)
}

/// import のローカル束縛を列挙する。true を返した時点で走査を終える。
pub(crate) fn visit_import_bindings(
    import_stmt: Node<'_>,
    source: &[u8],
    mut visit: impl FnMut(&str) -> bool,
) -> bool {
    // import 節の構造自体は浅いが、構文エラーから回復した部分木は任意の深さになり得るため、
    // 再帰せず明示スタックで前順に辿る (再帰版と同じ順で `visit` を呼ぶ)。
    fn walk(root: Node<'_>, source: &[u8], visit: &mut impl FnMut(&str) -> bool) -> bool {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            let found = match node.kind() {
                "import_specifier" => {
                    // alias があれば alias がローカル名、なければ name がローカル名。
                    let local = node
                        .child_by_field_name("alias")
                        .or_else(|| node.child_by_field_name("name"));
                    local
                        .and_then(|n| n.utf8_text(source).ok())
                        .is_some_and(&mut *visit)
                }
                "namespace_import" => {
                    let mut cursor = node.walk();
                    node.named_children(&mut cursor).any(|c| {
                        c.kind() == "identifier"
                            && c.utf8_text(source).ok().is_some_and(&mut *visit)
                    })
                }
                "identifier" => node.utf8_text(source).ok().is_some_and(&mut *visit),
                "string" => false,
                _ => {
                    let mut cursor = node.walk();
                    let start = stack.len();
                    stack.extend(node.named_children(&mut cursor));
                    stack[start..].reverse();
                    false
                }
            };
            if found {
                return true;
            }
        }
        false
    }
    import_stmt
        .child_by_field_name("import")
        .map(|clause| walk(clause, source, &mut visit))
        .unwrap_or_else(|| {
            let mut cursor = import_stmt.walk();
            import_stmt
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "import_clause")
                .any(|c| walk(c, source, &mut visit))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;

    fn resolve_at(src: &str, name: &str, line: usize, column: usize) -> LocalResolution {
        let tree = parser::parse_source(src.as_bytes(), LangId::Typescript).unwrap();
        resolve_reference_binding(
            &tree,
            src.as_bytes(),
            LangId::Typescript,
            name,
            line,
            column,
        )
    }

    /// ネストした callback 内の呼び出しでも、scope chain を外へ辿って同一ファイルの
    /// function_declaration に到達すれば SameFileFunction (canvas-pip-window 再現形)。
    #[test]
    fn nested_callback_resolves_to_same_file_function() {
        let src = "function startRecording(p: number): number {\n    return p;\n}\nwindow.addEventListener(\"msg\", () => {\n    const res = startRecording(1);\n    console.log(res);\n});\n";
        // line 4 の `startRecording(1)` (column 16)
        assert_eq!(
            resolve_at(src, "startRecording", 4, 16),
            LocalResolution::SameFileFunction
        );
    }

    /// import された名前への参照は Other (対象 API への参照かもしれない)。
    #[test]
    fn imported_name_is_ambiguous() {
        let src = "import { startRecording } from \"./capture\";\nstartRecording(1);\n";
        assert_eq!(
            resolve_at(src, "startRecording", 1, 0),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// import した対象関数を呼ぶファイルに同名 class method があっても、bare 呼び出しは
    /// method に束縛されない → import binding が勝ち Other (codex 設計相談の反例)。
    #[test]
    fn class_method_does_not_shadow_imported_bare_call() {
        let src = "import { startRecording } from \"./capture\";\nclass Recorder {\n    startRecording() {}\n}\nstartRecording(1);\n";
        assert_eq!(
            resolve_at(src, "startRecording", 4, 0),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// 変数 binding (`const startRecording = ...`) は function_declaration でないため Other。
    #[test]
    fn variable_binding_is_ambiguous() {
        let src = "const startRecording = (p: number) => p;\nstartRecording(1);\n";
        assert_eq!(
            resolve_at(src, "startRecording", 1, 0),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// parameter が shadow する場合は Other (中間 binding)。
    #[test]
    fn parameter_shadow_is_ambiguous() {
        let src = "function startRecording(p: number): number {\n    return p;\n}\nfunction outer(startRecording: () => void) {\n    startRecording();\n}\n";
        assert_eq!(
            resolve_at(src, "startRecording", 4, 4),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// `obj.startRecording()` の property 位置は Other。
    #[test]
    fn property_position_is_ambiguous() {
        let src = "function startRecording() {}\nconst obj = { startRecording };\nobj.startRecording();\n";
        assert_eq!(
            resolve_at(src, "startRecording", 2, 4),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// destructuring parameter の shadow も Other。
    #[test]
    fn destructuring_parameter_shadow_is_ambiguous() {
        let src = "function startRecording() {}\nfunction outer({ startRecording }: { startRecording: () => void }) {\n    startRecording();\n}\n";
        assert_eq!(
            resolve_at(src, "startRecording", 2, 4),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// block-level function はネスト block の scope に閉じる (module/strict mode)。
    /// 外側の呼び出しはそれに束縛されず import に束縛されるため Other (codex 指摘)。
    #[test]
    fn block_level_function_does_not_shadow_outer_call() {
        let src = "import { startRecording } from \"./capture\";\nfunction caller(flag: boolean) {\n    if (flag) {\n        function startRecording() {}\n    }\n    startRecording({ fps: 30 });\n}\n";
        assert_eq!(
            resolve_at(src, "startRecording", 5, 4),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// switch body 内の block-level function も外側の呼び出しを shadow しない (codex 指摘)。
    #[test]
    fn switch_body_function_does_not_shadow_outer_call() {
        let src = "import { startRecording } from \"./capture\";\nfunction caller(x: number) {\n    switch (x) {\n        case 1:\n            function startRecording() {}\n    }\n    startRecording({ fps: 30 });\n}\n";
        assert_eq!(
            resolve_at(src, "startRecording", 6, 4),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// for-of の loop 変数が同名の場合、その参照は loop 変数に束縛される。
    /// ヘッダ binding を見ずに外側の function_declaration へ誤解決しない (fail-open 防止)。
    #[test]
    fn for_of_loop_variable_shadows_same_file_function() {
        let src = "function startRecording() {}\nfunction caller(recorders: Array<() => void>) {\n    for (const startRecording of recorders) {\n        startRecording();\n    }\n}\n";
        assert_eq!(
            resolve_at(src, "startRecording", 3, 8),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// 単文 body の for-of (enclosing scope が for_in_statement 自体) でも
    /// loop 変数 binding を拾う。
    #[test]
    fn for_of_single_statement_body_loop_variable_is_ambiguous() {
        let src = "function startRecording() {}\nfunction caller(recorders: Array<() => void>) {\n    for (const startRecording of recorders) startRecording();\n}\n";
        assert_eq!(
            resolve_at(src, "startRecording", 2, 44),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// destructuring の loop 変数 (`for (const { name } of xs)`) も binding。
    #[test]
    fn for_of_destructured_loop_variable_is_ambiguous() {
        let src = "function startRecording() {}\nfunction caller(items: Array<{ startRecording: () => void }>) {\n    for (const { startRecording } of items) {\n        startRecording();\n    }\n}\n";
        assert_eq!(
            resolve_at(src, "startRecording", 3, 8),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// for 初期化子 (`for (let f = ...;;)`) の binding も for scope の binding。
    #[test]
    fn for_initializer_binding_shadows_same_file_function() {
        let src = "function startRecording() {}\nfunction caller(fns: Array<() => void>) {\n    for (let startRecording = fns[0], i = 0; i < 1; i++) {\n        startRecording();\n    }\n}\n";
        assert_eq!(
            resolve_at(src, "startRecording", 3, 8),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// `for (var x of xs)` の var ヘッダ binding は関数 scope へ hoist され、
    /// for の外側の参照も shadow する。
    #[test]
    fn hoisted_var_for_of_header_is_ambiguous() {
        let src = "function startRecording() {}\nfunction caller(recorders: Array<() => void>) {\n    for (var startRecording of recorders) {\n    }\n    startRecording();\n}\n";
        assert_eq!(
            resolve_at(src, "startRecording", 4, 4),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// var hoisting: ネスト block 内の var 宣言も関数 scope の binding として拾い Other。
    #[test]
    fn hoisted_var_in_nested_block_is_ambiguous() {
        let src = "function startRecording() {}\nfunction outer(flag: boolean) {\n    if (flag) {\n        var startRecording = 1;\n    }\n    startRecording();\n}\n";
        assert_eq!(
            resolve_at(src, "startRecording", 5, 4),
            LocalResolution::OtherOrAmbiguous
        );
    }

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn find_binding_in_subtree_recursive<'tree>(
        node: Node<'tree>,
        source: &[u8],
        name: &str,
        in_nested_block: bool,
        namespace: Namespace,
    ) -> Option<ScopeBinding<'tree>> {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "function_declaration" | "generator_function_declaration" => {
                    if !in_nested_block
                        && namespace == Namespace::Value
                        && declares_name(child, source, name)
                    {
                        return Some(ScopeBinding::FunctionDeclaration);
                    }
                }
                "class_declaration" | "abstract_class_declaration" => {
                    if !in_nested_block && declares_name(child, source, name) {
                        return Some(ScopeBinding::ClassDeclaration);
                    }
                }
                "interface_declaration" | "type_alias_declaration" => {
                    if !in_nested_block
                        && namespace == Namespace::Type
                        && declares_name(child, source, name)
                    {
                        return Some(ScopeBinding::TypeDeclaration);
                    }
                }
                "enum_declaration" => {
                    if !in_nested_block && declares_name(child, source, name) {
                        return Some(ScopeBinding::TypeDeclaration);
                    }
                }
                "ambient_declaration" | "internal_module" | "module" => {
                    if !in_nested_block && subtree_declares_name(child, source, name) {
                        return Some(ScopeBinding::Other);
                    }
                }
                "lexical_declaration" => {
                    if !in_nested_block
                        && let Some(found) =
                            find_binding_in_subtree_recursive(child, source, name, false, namespace)
                    {
                        return Some(found);
                    }
                }
                "variable_declaration" => {
                    if let Some(found) = find_binding_in_subtree_recursive(
                        child,
                        source,
                        name,
                        in_nested_block,
                        namespace,
                    ) {
                        return Some(found);
                    }
                }
                "variable_declarator" => {
                    if namespace == Namespace::Value
                        && let Some(pat) = child.child_by_field_name("name")
                        && pattern_binds_name(pat, source, name)
                    {
                        return Some(if pat.kind() == "identifier" {
                            ScopeBinding::Variable(child)
                        } else {
                            ScopeBinding::Other
                        });
                    }
                    if let Some(value) = child.child_by_field_name("value")
                        && !is_scope_node(value.kind())
                        && let Some(found) = find_binding_in_subtree_recursive(
                            value,
                            source,
                            name,
                            in_nested_block,
                            namespace,
                        )
                    {
                        return Some(found);
                    }
                }
                "import_statement" => {
                    if !in_nested_block && import_binds_name(child, source, name) {
                        return Some(ScopeBinding::Import(child));
                    }
                }
                _ => {
                    if matches!(
                        child.kind(),
                        "function_expression"
                            | "arrow_function"
                            | "generator_function"
                            | "method_definition"
                            | "class_body"
                    ) {
                        continue;
                    }
                    if namespace == Namespace::Value
                        && child.kind() == "for_in_statement"
                        && child
                            .child_by_field_name("kind")
                            .is_some_and(|k| k.kind() == "var")
                        && let Some(left) = child.child_by_field_name("left")
                        && pattern_binds_name(left, source, name)
                    {
                        return Some(ScopeBinding::Other);
                    }
                    let nested = in_nested_block
                        || matches!(child.kind(), "statement_block" | "switch_body");
                    if let Some(found) =
                        find_binding_in_subtree_recursive(child, source, name, nested, namespace)
                    {
                        return Some(found);
                    }
                }
            }
        }
        None
    }

    fn binding_key(binding: Option<ScopeBinding<'_>>) -> Option<(&'static str, Option<usize>)> {
        binding.map(|binding| match binding {
            ScopeBinding::FunctionDeclaration => ("function", None),
            ScopeBinding::ClassDeclaration => ("class", None),
            ScopeBinding::TypeDeclaration => ("type", None),
            ScopeBinding::Variable(node) => ("variable", Some(node.id())),
            ScopeBinding::Import(node) => ("import", Some(node.id())),
            ScopeBinding::Other => ("other", None),
        })
    }

    /// 明示スタック版が、浅い入力のすべての部分木・名前・名前空間・block の内外で、
    /// 再帰版と同じ束縛 (同じ宣言ノード) を返す。同名の宣言が複数あるので、
    /// 最初に見つかったものを返す順序まで比べることになる。
    #[test]
    fn find_binding_in_subtree_matches_recursive_version() {
        let src = r#"import def, { a as alias, b } from "./m";
import * as ns from "./ns";
import type { T } from "./t";
function f(p) { var hoisted = 1; if (p) { var b = 2; function inner() {} } }
function* gen() {}
class C { m() { const a = 1; } }
abstract class Abs {}
interface I { x: number }
type T2 = string;
enum E { A }
declare const d: number;
declare function decl(): void;
namespace N { export const a = 1; }
const { a: renamed, ...rest } = obj, [first, , third] = list;
let x = cond ? obj.b : (() => { var t = 1; })();
const w = { k: (function named() {})(), v: class {} };
var y = 1;
for (var a of items) { let b = a; }
for (const b in items) {}
switch (y) { case 1: var b = 2; let a = 3; }
{ var a = 1; const f = 2; }
if (y) { function f() {} } else { var C = 1; }
try { var b = 1; } catch (a) { var x = 2; }
"#;
        let tree = parser::parse_source(src.as_bytes(), LangId::Typescript).unwrap();
        let names = [
            "a", "b", "f", "C", "Abs", "E", "I", "T", "T2", "x", "y", "ns", "def", "alias",
            "renamed", "rest", "first", "third", "hoisted", "inner", "gen", "d", "decl", "N", "t",
            "named", "k", "w", "missing",
        ];
        let mut checked = 0usize;
        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
            tree.root_node(),
            crate::engine::tree_walk::Children::All,
            |node, _| {
                for name in names {
                    for namespace in [Namespace::Value, Namespace::Type] {
                        for nested in [false, true] {
                            assert_eq!(
                                binding_key(find_binding_in_subtree(
                                    node,
                                    src.as_bytes(),
                                    name,
                                    nested,
                                    namespace
                                )),
                                binding_key(find_binding_in_subtree_recursive(
                                    node,
                                    src.as_bytes(),
                                    name,
                                    nested,
                                    namespace
                                )),
                                "{name} {namespace:?} nested={nested}: {}",
                                node.to_sexp()
                            );
                            checked += 1;
                        }
                    }
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        assert!(checked > 1000);
        // 少なくとも一部は束縛を見つける (空振りの比較になっていない)。
        let root = tree.root_node();
        for (name, namespace, expected) in [
            ("f", Namespace::Value, "function"),
            ("C", Namespace::Value, "class"),
            ("I", Namespace::Type, "type"),
            ("alias", Namespace::Value, "import"),
            ("renamed", Namespace::Value, "other"),
            ("y", Namespace::Value, "variable"),
        ] {
            assert_eq!(
                binding_key(find_binding_in_subtree(
                    root,
                    src.as_bytes(),
                    name,
                    false,
                    namespace
                ))
                .map(|(kind, _)| kind),
                Some(expected),
                "{name}"
            );
        }
    }

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn visit_import_bindings_recursive(
        import_stmt: Node<'_>,
        source: &[u8],
        mut visit: impl FnMut(&str) -> bool,
    ) -> bool {
        fn walk(node: Node<'_>, source: &[u8], visit: &mut impl FnMut(&str) -> bool) -> bool {
            match node.kind() {
                "import_specifier" => {
                    let local = node
                        .child_by_field_name("alias")
                        .or_else(|| node.child_by_field_name("name"));
                    local
                        .and_then(|n| n.utf8_text(source).ok())
                        .is_some_and(&mut *visit)
                }
                "namespace_import" => {
                    let mut cursor = node.walk();
                    node.named_children(&mut cursor).any(|c| {
                        c.kind() == "identifier"
                            && c.utf8_text(source).ok().is_some_and(&mut *visit)
                    })
                }
                "identifier" => node.utf8_text(source).ok().is_some_and(&mut *visit),
                "string" => false,
                _ => {
                    let mut cursor = node.walk();
                    node.named_children(&mut cursor)
                        .any(|c| walk(c, source, visit))
                }
            }
        }
        import_stmt
            .child_by_field_name("import")
            .map(|clause| walk(clause, source, &mut visit))
            .unwrap_or_else(|| {
                let mut cursor = import_stmt.walk();
                import_stmt
                    .named_children(&mut cursor)
                    .filter(|c| c.kind() == "import_clause")
                    .any(|c| walk(c, source, &mut visit))
            })
    }

    /// 明示スタック版が、浅い入力の import 文で再帰版と同じ順に束縛を列挙し、
    /// 同じ位置で走査を打ち切る。
    #[test]
    fn visit_import_bindings_matches_recursive_version() {
        let src = r#"import def from "./a";
import def2, { x, y as z, "s" as q } from "./b";
import * as ns from "./c";
import def3, * as ns2 from "./d";
import type { T, U as V } from "./e";
import "./side-effect";
import { broken, from "./f";
"#;
        let tree = parser::parse_source(src.as_bytes(), LangId::Typescript).unwrap();
        let root = tree.root_node();
        let mut cursor = root.walk();
        let statements: Vec<_> = root
            .named_children(&mut cursor)
            .filter(|n| n.kind() == "import_statement")
            .collect();
        assert!(statements.len() >= 6);
        for statement in statements {
            let mut all = Vec::new();
            let mut all_recursive = Vec::new();
            assert_eq!(
                visit_import_bindings(statement, src.as_bytes(), |name| {
                    all.push(name.to_string());
                    false
                }),
                visit_import_bindings_recursive(statement, src.as_bytes(), |name| {
                    all_recursive.push(name.to_string());
                    false
                })
            );
            assert_eq!(all, all_recursive, "{}", statement.to_sexp());
            // k 件目で true を返したときに、同じ位置で打ち切る。
            for stop in 0..all.len() {
                let mut seen = Vec::new();
                let mut seen_recursive = Vec::new();
                assert!(visit_import_bindings(statement, src.as_bytes(), |name| {
                    seen.push(name.to_string());
                    seen.len() == stop + 1
                }));
                assert!(visit_import_bindings_recursive(
                    statement,
                    src.as_bytes(),
                    |name| {
                        seen_recursive.push(name.to_string());
                        seen_recursive.len() == stop + 1
                    }
                ));
                assert_eq!(seen, seen_recursive);
            }
        }
    }
}
