//! JS/TS の束縛パターン (分割代入・default・rest) が導入する束縛識別子を列挙する。
//!
//! シンボル抽出 (`export const { a, b } = obj` の各名前をシンボル化)、参照検索の
//! def/ref 分類、API 差分の束縛ごとの signature、shadow 判定 (`pattern_binds_name`) が
//! 同じ規則を使う。規則を複数箇所に書くと「一方だけが束縛として拾う」ずれが起き、
//! shadow の見逃し (fail-open) や、束縛位置を参照と数えて dead を見逃す誤りになるため、
//! ここに 1 つだけ置く。

use std::ops::ControlFlow;

use tree_sitter::Node;

/// `pattern` が導入する束縛識別子ノードを source 順に `visit` へ渡す。
///
/// - `identifier` / `shorthand_property_identifier_pattern` は束縛そのもの
/// - `pair_pattern` (`{ key: value }`) は value 側だけを辿る (key はプロパティ名で束縛ではない。
///   computed key `{ [k]: v }` の `k` も参照)
/// - `assignment_pattern` / `object_assignment_pattern` (`x = fallback`) は left 側だけを辿る
///   (default 値の式は参照)
/// - それ以外 (object_pattern / array_pattern / rest_pattern / パラメータ列など) は
///   named child を再帰する
///
/// `visit` が `Break` を返すと走査を打ち切り、`Break` を返す。
///
/// パターンは `[[[x]]]` のように任意に深く入れ子にできるので、再帰ではなく明示スタックで
/// 辿る (入れ子 1 段ごとに再帰するとスタックを使い切ってプロセスごと落ちる)。子は逆順に
/// 積み、再帰で辿った場合と同じ source 順に訪れる。
pub(crate) fn visit_pattern_bindings<'a>(
    pattern: Node<'a>,
    visit: &mut impl FnMut(Node<'a>) -> ControlFlow<()>,
) -> ControlFlow<()> {
    let mut stack = vec![pattern];
    while let Some(node) = stack.pop() {
        match node.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => visit(node)?,
            // 型注釈・default 値は、その引数が宣言する束縛ではない。
            "required_parameter" | "optional_parameter" => stack.extend(
                node.child_by_field_name("pattern")
                    .or_else(|| node.child_by_field_name("name")),
            ),
            "pair_pattern" => stack.extend(node.child_by_field_name("value")),
            "assignment_pattern" | "object_assignment_pattern" => {
                stack.extend(node.child_by_field_name("left"));
            }
            _ => {
                let first = stack.len();
                let mut cursor = node.walk();
                stack.extend(node.named_children(&mut cursor));
                stack[first..].reverse();
            }
        }
    }
    ControlFlow::Continue(())
}

/// `node` (identifier) が変数宣言 (`const` / `let` / `var`) の分割代入パターン内の
/// **束縛位置**にあるかを判定する。
///
/// `const { a: { b }, c = fallback, ...rest } = obj;` なら `b` / `c` / `rest` が束縛位置、
/// `fallback` (default 値) と computed key 内の識別子は参照。宣言の `name` フィールドへ
/// 束縛位置だけを辿って到達できたときに限り true を返す。関数パラメータ・`for (const x of ..)`
/// の loop 変数・分割代入の代入式 (`({ a } = obj)`) は変数宣言ではないので false。
pub(crate) fn is_declarator_pattern_binding(node: Node<'_>) -> bool {
    let mut child = node;
    while let Some(parent) = child.parent() {
        match parent.kind() {
            "object_pattern" | "array_pattern" | "rest_pattern" => {}
            "pair_pattern" => {
                if parent.child_by_field_name("value").map(|v| v.id()) != Some(child.id()) {
                    return false;
                }
            }
            "assignment_pattern" | "object_assignment_pattern" => {
                if parent.child_by_field_name("left").map(|l| l.id()) != Some(child.id()) {
                    return false;
                }
            }
            "variable_declarator" => {
                // パターンを 1 段以上経由して宣言の name へ到達した場合だけ束縛。
                // 直接の `const x = ..` は各言語共通の name フィールド判定が扱う。
                return child.id() != node.id()
                    && parent.child_by_field_name("name").map(|n| n.id()) == Some(child.id());
            }
            _ => return false,
        }
        child = parent;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    fn binding_names(src: &str) -> Vec<String> {
        let tree = parser::parse_source(src.as_bytes(), LangId::Typescript).unwrap();
        let root = tree.root_node();
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if n.kind() == "variable_declarator"
                && let Some(name) = n.child_by_field_name("name")
                && matches!(name.kind(), "object_pattern" | "array_pattern")
            {
                let _ = visit_pattern_bindings(name, &mut |b| {
                    out.push(b.utf8_text(src.as_bytes()).unwrap().to_string());
                    ControlFlow::Continue(())
                });
            }
            let mut cursor = n.walk();
            let children: Vec<_> = n.named_children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        out
    }

    #[test]
    fn visits_only_binding_positions_in_source_order() {
        let names = binding_names(
            "const { a, b: renamed, [key]: computed, c = fallback, d: { e }, ...rest } = obj;\n\
             const [x, , y = dflt, [z], ...zs] = arr;",
        );
        assert_eq!(
            names,
            [
                "a", "renamed", "computed", "c", "e", "rest", "x", "y", "z", "zs"
            ]
        );
    }

    #[test]
    fn declarator_pattern_binding_excludes_defaults_keys_and_non_declarations() {
        let src = "const { a: { b: bb }, c = fallback, [key]: v, ...rest } = obj;\n\
                   const [x = dflt] = arr;\n\
                   const plain = value;\n\
                   function f({ p }, [q]) {}\n\
                   for (const [k] of entries) {}\n\
                   ({ w } = obj);";
        let tree = parser::parse_source(src.as_bytes(), LangId::Typescript).unwrap();
        let root = tree.root_node();
        let mut binding = Vec::new();
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            if n.kind() == "identifier" && is_declarator_pattern_binding(n) {
                binding.push(n.utf8_text(src.as_bytes()).unwrap().to_string());
            }
            let mut cursor = n.walk();
            let children: Vec<_> = n.named_children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
        // bb / v / rest / x は束縛。fallback / key / dflt (参照)、plain (パターン外の直接束縛)、
        // パラメータ・loop 変数・代入式の識別子は対象外。shorthand (`{ c }`) は
        // identifier ではない別ノードなので、そもそも refs の判定対象に来ない。
        assert_eq!(binding, ["bb", "v", "rest", "x"]);
    }

    /// 修正前の再帰版 (浅い入力でだけ使う参照実装)。
    fn visit_pattern_bindings_recursive<'a>(
        pattern: Node<'a>,
        visit: &mut impl FnMut(Node<'a>) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        match pattern.kind() {
            "identifier" | "shorthand_property_identifier_pattern" => visit(pattern),
            "pair_pattern" => match pattern.child_by_field_name("value") {
                Some(value) => visit_pattern_bindings_recursive(value, visit),
                None => ControlFlow::Continue(()),
            },
            "assignment_pattern" | "object_assignment_pattern" => {
                match pattern.child_by_field_name("left") {
                    Some(left) => visit_pattern_bindings_recursive(left, visit),
                    None => ControlFlow::Continue(()),
                }
            }
            _ => {
                let mut cursor = pattern.walk();
                for child in pattern.named_children(&mut cursor) {
                    visit_pattern_bindings_recursive(child, visit)?;
                }
                ControlFlow::Continue(())
            }
        }
    }

    /// 明示スタックの版は、再帰版と同じ束縛を同じ順に訪れ、`Break` で同じ位置で止まる。
    /// 宣言だけでなくパラメータ・catch・for-of・代入式のパターンも渡す。
    #[test]
    fn visit_pattern_bindings_matches_recursive_version() {
        let src = "const { a, b: renamed, [key]: computed, c = fallback, d: { e: [f, { g = h }] }, ...rest } = obj;\n\
                   const [x, , y = dflt, [z, [w = { q } = r]], ...zs] = arr;\n\
                   function fn1({ p, s: [t] }, [u = v], ...others) {}\n\
                   try {} catch ({ message, cause: { code } }) {}\n\
                   for (const [k, { val }] of entries) {}\n\
                   ({ m, n: [o] } = obj);";
        for lang in [LangId::Typescript, LangId::Javascript] {
            let tree = parser::parse_source(src.as_bytes(), lang).unwrap();
            let mut patterns = Vec::new();
            let mut stack = vec![tree.root_node()];
            while let Some(n) = stack.pop() {
                if matches!(
                    n.kind(),
                    "object_pattern" | "array_pattern" | "formal_parameters"
                ) && !matches!(
                    n.parent().map(|p| p.kind()),
                    Some("object_pattern" | "array_pattern" | "pair_pattern" | "rest_pattern")
                ) {
                    patterns.push(n);
                }
                let mut cursor = n.walk();
                let children: Vec<_> = n.named_children(&mut cursor).collect();
                stack.extend(children.into_iter().rev());
            }
            assert!(patterns.len() >= 6, "{lang:?}: {}", patterns.len());
            let mut total = 0;
            for pattern in patterns {
                let collect = |stop_after: Option<usize>, recursive: bool| {
                    let mut seen = Vec::new();
                    let mut visit = |b: Node<'_>| {
                        seen.push(b.utf8_text(src.as_bytes()).unwrap().to_string());
                        if Some(seen.len()) == stop_after {
                            ControlFlow::Break(())
                        } else {
                            ControlFlow::Continue(())
                        }
                    };
                    let flow = if recursive {
                        visit_pattern_bindings_recursive(pattern, &mut visit)
                    } else {
                        visit_pattern_bindings(pattern, &mut visit)
                    };
                    (seen, flow.is_break())
                };
                let (all, _) = collect(None, true);
                assert_eq!(collect(None, false), (all.clone(), false), "{lang:?}");
                for stop in 1..=all.len() {
                    assert_eq!(
                        collect(Some(stop), false),
                        collect(Some(stop), true),
                        "{lang:?}: stop={stop}"
                    );
                }
                total += all.len();
            }
            assert!(total >= 25, "{lang:?}: {total}");
        }
    }
}
