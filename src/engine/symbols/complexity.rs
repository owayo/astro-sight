use tree_sitter::Node;

use crate::language::LangId;

/// 関数/メソッドノードの循環的複雑度を算出する（ベース1 + 分岐ノード数）。
/// ネストした関数/クロージャの分岐は含めない。
pub fn calculate_complexity(node: Node, lang_id: LangId) -> usize {
    let branch_kinds = branch_node_kinds(lang_id);
    let func_kinds = function_boundary_kinds(lang_id);
    let mut count = 1; // ベース複雑度
    count_branch_nodes(node, branch_kinds, func_kinds, true, &mut count);
    count
}

/// 再帰的に分岐ノードをカウントする。
/// ネストした関数境界（クロージャ・内部関数）で走査を停止する。
fn count_branch_nodes(
    node: Node,
    branch_kinds: &'static [&'static str],
    func_kinds: &[&str],
    is_root: bool,
    count: &mut usize,
) {
    let kind = node.kind();
    // ルート以外の関数境界で停止（ネスト関数の分岐を除外）
    if !is_root && func_kinds.contains(&kind) {
        return;
    }
    // named ノードのみ計上する。tree-sitter-ruby では `if` 文ノードと
    // キーワードトークン `if` が同じ kind 名を持つため、named 制約が無いと
    // 分岐が二重計上される（他言語の分岐ノードは全て named なので無影響）。
    if node.is_named() && branch_kinds.contains(&kind) {
        *count += 1;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        count_branch_nodes(child, branch_kinds, func_kinds, false, count);
    }
}

/// 関数境界を示すノード種別を返す（ネスト関数検出用）。
/// 言語別の関数境界ノード種別を返す。
/// 静的スライスを返すことで毎回の Vec アロケーションを回避する。
fn function_boundary_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Rust => &["function_item", "closure_expression"],
        LangId::Javascript | LangId::Typescript | LangId::Tsx => &[
            "function_declaration",
            "function_expression",
            "arrow_function",
            "method_definition",
            "generator_function_declaration",
        ],
        LangId::Python => &["function_definition", "lambda"],
        LangId::Go => &["function_declaration", "method_declaration", "func_literal"],
        LangId::Java => &["method_declaration", "lambda_expression"],
        LangId::Kotlin => &[
            "function_declaration",
            "lambda_literal",
            "anonymous_function",
        ],
        LangId::Swift => &["function_declaration", "lambda_literal"],
        LangId::CSharp => &["method_declaration", "lambda_expression"],
        LangId::Php => &[
            "function_definition",
            "method_declaration",
            "anonymous_function_creation_expression",
        ],
        LangId::Ruby => &["method", "singleton_method", "lambda", "block"],
        _ => &[],
    }
}

/// 言語別の分岐ノード種別を返す。
/// 静的スライスを返すことで毎回の Vec アロケーションを回避する。
fn branch_node_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        LangId::Rust => &[
            "if_expression",
            "match_expression",
            "for_expression",
            "while_expression",
            "loop_expression",
            "else_clause",
            "match_arm",
        ],
        LangId::Javascript | LangId::Typescript | LangId::Tsx => &[
            "if_statement",
            "switch_case",
            "for_statement",
            "for_in_statement",
            "while_statement",
            "do_statement",
            "ternary_expression",
            "catch_clause",
        ],
        LangId::Python => &[
            "if_statement",
            "elif_clause",
            "for_statement",
            "while_statement",
            "except_clause",
            "conditional_expression",
        ],
        LangId::Go => &[
            "if_statement",
            "for_statement",
            "select_statement",
            "type_switch_statement",
            "case_clause",
        ],
        LangId::Java => &[
            "if_statement",
            "switch_expression",
            "for_statement",
            "enhanced_for_statement",
            "while_statement",
            "do_statement",
            "catch_clause",
        ],
        // Kotlin の分岐ノードは tree-sitter-kotlin 固有名 (`if_expression` / `when_expression` /
        // `when_entry` / `do_while_statement` / `catch_block` / `elvis_expression`)。
        // Java と同じスライスを共用すると一切マッチせず複雑度がベース 1 のまま返る。
        LangId::Kotlin => &[
            "if_expression",
            "when_expression",
            "when_entry",
            "for_statement",
            "while_statement",
            "do_while_statement",
            "catch_block",
            "elvis_expression",
        ],
        // Swift の分岐ノードも tree-sitter-swift 固有名を含む。汎用スライスには
        // `guard_statement` / `switch_entry` / `repeat_while_statement` / `catch_block` が
        // 無く、guard や case arm が計上されない (Kotlin 専用スライス化 v26.6.110 と同型)。
        LangId::Swift => &[
            "if_statement",
            "guard_statement",
            "switch_statement",
            "switch_entry",
            "for_statement",
            "while_statement",
            "repeat_while_statement",
            "catch_block",
            "ternary_expression",
        ],
        LangId::Ruby => &[
            "if", "elsif", "unless", "case", "when", "for", "while", "until", "rescue",
        ],
        LangId::Php => &[
            "if_statement",
            "switch_statement",
            "case_statement",
            "for_statement",
            "foreach_statement",
            "while_statement",
            "do_statement",
            "catch_clause",
        ],
        LangId::CSharp => &[
            "if_statement",
            "switch_section",
            "for_statement",
            "for_each_statement",
            "while_statement",
            "do_statement",
            "catch_clause",
        ],
        LangId::Zig => &[
            "if_expression",
            "if_statement",
            "for_expression",
            "for_statement",
            "while_expression",
            "while_statement",
            "switch_expression",
            "switch_case",
            "catch_expression",
            "else_clause",
        ],
        // 汎用パターン（C, C++, Swift, Bash 等）
        _ => &[
            "if_statement",
            "if_expression",
            "for_statement",
            "for_expression",
            "while_statement",
            "while_expression",
            "switch_statement",
            "case_statement",
            "catch_clause",
        ],
    }
}
