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
    let mut cur = node;
    while let Some(scope) = enclosing_scope(cur) {
        match find_binding_in_scope(scope, source, name) {
            Some(BindingKind::FunctionDeclaration) => return LocalResolution::SameFileFunction,
            Some(_) => return LocalResolution::OtherOrAmbiguous,
            None => cur = scope,
        }
    }
    LocalResolution::OtherOrAmbiguous
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

/// scope 内で見つかった同名 binding の種類。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingKind {
    FunctionDeclaration,
    Other,
}

/// `scope` 配下 (ネストした scope の内側は除く) に `name` の value binding があるかを探す。
/// `var` の hoisting / block 内宣言を漏らさないため、ネスト scope の境界までは再帰で潜る
/// (function 系の内側の宣言はその scope のものなので潜らない。ただし function 自身の名前は
/// 現 scope の binding として拾う)。
fn find_binding_in_scope(scope: Node<'_>, source: &[u8], name: &str) -> Option<BindingKind> {
    // 関数系 scope は parameter も binding。
    if let Some(params) = scope.child_by_field_name("parameters")
        && pattern_binds_name(params, source, name)
    {
        return Some(BindingKind::Other);
    }
    // 単一 parameter の arrow (`x => ...`) は parameter フィールドが identifier。
    if let Some(param) = scope.child_by_field_name("parameter")
        && param.kind() == "identifier"
        && param.utf8_text(source).ok() == Some(name)
    {
        return Some(BindingKind::Other);
    }
    // catch (e) の e。
    if scope.kind() == "catch_clause"
        && let Some(param) = scope.child_by_field_name("parameter")
        && pattern_binds_name(param, source, name)
    {
        return Some(BindingKind::Other);
    }
    // 名前付き function expression の自己参照名は内部 scope の binding。
    if matches!(scope.kind(), "function_expression" | "generator_function")
        && let Some(n) = scope.child_by_field_name("name")
        && n.utf8_text(source).ok() == Some(name)
    {
        return Some(BindingKind::Other);
    }
    // for-of / for-in の loop 変数 (`for (const x of xs)`) は left field の bare pattern
    // (lexical_declaration に包まれない) のため body 探索では見えない。ここで拾わないと
    // loop 変数への参照を外側の同名 function_declaration へ誤解決する (fail-open)。
    if scope.kind() == "for_in_statement"
        && let Some(left) = scope.child_by_field_name("left")
        && pattern_binds_name(left, source, name)
    {
        return Some(BindingKind::Other);
    }

    let body = scope.child_by_field_name("body").unwrap_or(scope);
    find_binding_in_subtree(body, source, name, false)
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
fn find_binding_in_subtree(
    node: Node<'_>,
    source: &[u8],
    name: &str,
    in_nested_block: bool,
) -> Option<BindingKind> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" | "generator_function_declaration" => {
                if !in_nested_block
                    && child
                        .child_by_field_name("name")
                        .is_some_and(|n| n.utf8_text(source).ok() == Some(name))
                {
                    return Some(BindingKind::FunctionDeclaration);
                }
                // 内側 (parameters / body) はネスト scope なので潜らない。
            }
            "class_declaration" => {
                if !in_nested_block
                    && child
                        .child_by_field_name("name")
                        .is_some_and(|n| n.utf8_text(source).ok() == Some(name))
                {
                    return Some(BindingKind::Other);
                }
            }
            // let / const は block scope: ネスト block 内の宣言は現 scope の binding でない。
            "lexical_declaration" => {
                if !in_nested_block
                    && let Some(found) = find_binding_in_subtree(child, source, name, false)
                {
                    return Some(found);
                }
            }
            // var は関数 scope に hoist されるため、ネスト block 内でも拾う。
            "variable_declaration" => {
                if let Some(found) = find_binding_in_subtree(child, source, name, in_nested_block) {
                    return Some(found);
                }
            }
            "variable_declarator" => {
                if let Some(pat) = child.child_by_field_name("name")
                    && pattern_binds_name(pat, source, name)
                {
                    return Some(BindingKind::Other);
                }
                // 初期化子内はさらに探索 (ネスト scope は下で打ち切られる)。
                if let Some(value) = child.child_by_field_name("value")
                    && !is_scope_node(value.kind())
                    && let Some(found) =
                        find_binding_in_subtree(value, source, name, in_nested_block)
                {
                    return Some(found);
                }
            }
            "import_statement" => {
                if !in_nested_block && import_binds_name(child, source, name) {
                    return Some(BindingKind::Other);
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
                if child.kind() == "for_in_statement"
                    && child
                        .child_by_field_name("kind")
                        .is_some_and(|k| k.kind() == "var")
                    && let Some(left) = child.child_by_field_name("left")
                    && pattern_binds_name(left, source, name)
                {
                    return Some(BindingKind::Other);
                }
                // statement_block / switch_body (および制御構造の中身) へ潜る際は
                // nested block 扱いに切り替え、以降は var 由来の binding だけを拾う。
                let nested =
                    in_nested_block || matches!(child.kind(), "statement_block" | "switch_body");
                if let Some(found) = find_binding_in_subtree(child, source, name, nested) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// binding パターン (identifier / destructuring / rest / default) が `name` を束縛するか。
fn pattern_binds_name(pattern: Node<'_>, source: &[u8], name: &str) -> bool {
    match pattern.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => {
            pattern.utf8_text(source).ok() == Some(name)
        }
        // property key 側 (`{ key: alias }` の key) は binding ではないので value 側のみ辿る。
        "pair_pattern" => pattern
            .child_by_field_name("value")
            .is_some_and(|v| pattern_binds_name(v, source, name)),
        _ => {
            let mut cursor = pattern.walk();
            pattern
                .named_children(&mut cursor)
                .any(|c| pattern_binds_name(c, source, name))
        }
    }
}

/// import 文が `name` をローカル binding として導入するか (default / named / alias / namespace)。
fn import_binds_name(import_stmt: Node<'_>, source: &[u8], name: &str) -> bool {
    fn walk(node: Node<'_>, source: &[u8], name: &str) -> bool {
        match node.kind() {
            "import_specifier" => {
                // alias があれば alias がローカル名、なければ name がローカル名。
                let local = node
                    .child_by_field_name("alias")
                    .or_else(|| node.child_by_field_name("name"));
                local.is_some_and(|n| n.utf8_text(source).ok() == Some(name))
            }
            "namespace_import" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .any(|c| c.kind() == "identifier" && c.utf8_text(source).ok() == Some(name))
            }
            "identifier" => node.utf8_text(source).ok() == Some(name),
            "string" => false,
            _ => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .any(|c| walk(c, source, name))
            }
        }
    }
    import_stmt
        .child_by_field_name("import")
        .map(|clause| walk(clause, source, name))
        .unwrap_or_else(|| {
            let mut cursor = import_stmt.walk();
            import_stmt
                .named_children(&mut cursor)
                .filter(|c| c.kind() == "import_clause")
                .any(|c| walk(c, source, name))
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
}
