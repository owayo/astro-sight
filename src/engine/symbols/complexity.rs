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
        // tree-sitter-php 0.24 で `anonymous_function_creation_expression` →
        // `anonymous_function` に改称された。旧名のままだと closure 内の分岐が
        // 外側関数の cx に混入する。
        LangId::Php => &[
            "function_definition",
            "method_declaration",
            "anonymous_function",
            "arrow_function",
        ],
        // `do ... end` は `do_block`、`{ ... }` は `block`。前者が漏れていた。
        LangId::Ruby => &["method", "singleton_method", "lambda", "block", "do_block"],
        LangId::C => &["function_definition"],
        // C++ のラムダ本体は `lambda_expression` 配下に入る。
        LangId::Cpp => &["function_definition", "lambda_expression"],
        // bash は関数内に関数を定義できる。
        LangId::Bash => &["function_definition"],
        LangId::Zig => &["function_declaration", "test_declaration"],
        LangId::Xojo => &[],
    }
}

/// 言語別の分岐ノード種別を返す。
/// 静的スライスを返すことで毎回の Vec アロケーションを回避する。
///
/// # 計上規約 (McCabe に揃える。言語をまたいで同じ値になることが要件)
///
/// 同じロジックが言語によって違う cx になると、しきい値 (例 `cx > 10`) での判断も
/// polyglot リポジトリでの比較も成立しない。次の 3 点を全言語で守る:
///
/// 1. **switch/match は arm だけを数え、構文本体は数えない**。`switch (a) { case 1..3 }`
///    は判定点 3 個なので cx=4。本体ノードも数えると 5 になり 1 つ過大。
///    (旧実装は C/C++/Go/PHP/Python/Kotlin/Swift/Ruby/bash/Zig/Rust が二重計上、
///    Java は逆に arm を 1 つも数えず 3 分岐が cx=2 になっていた)
/// 2. **plain `else` は数えない**。分岐の「もう一方の経路」であって判定点ではない。
///    `else if` はネストした `if` として自然に +1 される。
///    (旧実装は Rust/Zig だけ `else_clause` を数え if/else が cx=3 になっていた)
/// 3. **三項演算子は数える**。判定点そのもの。
///    (旧実装は PHP/Java/C#/Ruby で欠落していた)
///
/// catch-all arm (`default:` / `_ =>` / `else ->`) を数えるかは文法依存で残る:
/// 別ノードを持つ文法 (Go の `default_case`、JS の `switch_default`、PHP の
/// `default_statement`、Ruby の `else`) では数えず、arm と同一ノードになる文法
/// (Rust/Kotlin/Python/Swift/Java/C#/C) では数える。ノード種別だけで判別できないため、
/// ここは ±1 のぶれとして許容する (実在の複雑度ツール間でも解釈が分かれる)。
///
/// テーブルを触るときは必ず `astro-sight ast` で実ノード名をダンプして照合すること。
/// 汎用スライスの流用は「1 つもマッチせず cx=1 のまま」という無害に見える壊れ方をする。
fn branch_node_kinds(lang_id: LangId) -> &'static [&'static str] {
    match lang_id {
        // `match_expression` 本体と plain `else_clause` は判定点ではないため数えない
        // (`match_arm` / ネストした `if_expression` 側で計上される)。
        LangId::Rust => &[
            "if_expression",
            "for_expression",
            "while_expression",
            "loop_expression",
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
        // `match` 文 (PEP 634) の arm は `case_clause`。本体 `match_statement` は
        // 判定点ではないので数えない (旧実装は本体も数えて 1 つ過大だった)。
        LangId::Python => &[
            "if_statement",
            "elif_clause",
            "for_statement",
            "while_statement",
            "except_clause",
            "conditional_expression",
            "case_clause",
        ],
        // tree-sitter-go の switch は種別ごとに arm 名が分かれる
        // (`expression_case` / `type_case` / `communication_case`)。旧テーブルの
        // `case_clause` は Go に存在せず (Python 側のノード名) plain switch が 0 計上だった。
        // 本体 (`expression_switch_statement` 等) は判定点ではないので数えない。
        // `default_case` は暗黙の fall-through 経路なので数えない。
        LangId::Go => &[
            "if_statement",
            "for_statement",
            "expression_case",
            "type_case",
            "communication_case",
        ],
        // tree-sitter-java は switch 文も式も `switch_expression` 1 種で、arm は
        // colon 構文が `switch_block_statement_group`、arrow 構文 (Java 14+) が
        // `switch_rule` と分かれる。両構文に共通して 1 arm = 1 個現れるのは
        // `switch_label` なのでこれを arm カウンタに使う (fall-through の
        // `case 1: case 2:` も label 数 = 進入経路数で正しく 2 になる)。
        // 旧テーブルは本体 `switch_expression` だけを数えており、3 分岐 switch が
        // cx=2 という実態と逆の値を返していた。三項演算子も欠落していた。
        LangId::Java => &[
            "if_statement",
            "switch_label",
            "for_statement",
            "enhanced_for_statement",
            "while_statement",
            "do_statement",
            "catch_clause",
            "ternary_expression",
        ],
        // Kotlin の分岐ノードは tree-sitter-kotlin 固有名 (`if_expression` / `when_expression` /
        // `when_entry` / `do_while_statement` / `catch_block` / `elvis_expression`)。
        // Java と同じスライスを共用すると一切マッチせず複雑度がベース 1 のまま返る。
        LangId::Kotlin => &[
            "if_expression",
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
            "switch_entry",
            "for_statement",
            "while_statement",
            "repeat_while_statement",
            "catch_block",
            "ternary_expression",
        ],
        // 三項演算子は tree-sitter-ruby では `conditional` (他言語の
        // `conditional_expression` / `ternary_expression` ではない)。欠落していた。
        // `case` 本体は判定点ではないので数えない (`when` 側で計上)。
        LangId::Ruby => &[
            "if",
            "elsif",
            "unless",
            "when",
            "for",
            "while",
            "until",
            "rescue",
            "conditional",
        ],
        // `elseif` は `else_if_clause`、三項演算子は `conditional_expression`。
        // どちらも欠落しており、`if/elseif/else` が cx=2、三項のみの関数が cx=1 だった。
        // `switch_statement` 本体は判定点ではないので数えない (`case_statement` 側で計上)。
        LangId::Php => &[
            "if_statement",
            "else_if_clause",
            "case_statement",
            "for_statement",
            "foreach_statement",
            "while_statement",
            "do_statement",
            "catch_clause",
            "conditional_expression",
        ],
        // tree-sitter-c-sharp の foreach は `foreach_statement` (アンダースコア無し)。
        // 旧テーブルの `for_each_statement` は存在しないノード名で 1 つもマッチせず、
        // foreach だけの関数が cx=1 のまま返っていた。三項も欠落していた。
        LangId::CSharp => &[
            "if_statement",
            "switch_section",
            "for_statement",
            "foreach_statement",
            "while_statement",
            "do_statement",
            "catch_clause",
            "conditional_expression",
        ],
        // `switch_expression` 本体と plain `else_clause` は判定点ではないので数えない
        // (`switch_case` / ネストした `if` 側で計上される)。
        LangId::Zig => &[
            "if_expression",
            "if_statement",
            "for_expression",
            "for_statement",
            "while_expression",
            "while_statement",
            "switch_case",
            "catch_expression",
        ],
        // C/C++ は `do ... while` が `do_statement`、三項演算子が `conditional_expression`。
        // 旧汎用スライスはどちらも持たず、これらだけを持つ関数がベース 1 のまま返っていた。
        // `switch_statement` 本体は判定点ではないので数えない (`case_statement` 側で計上。
        // tree-sitter-c では `default:` も `case_statement` なので catch-all も 1 計上される)。
        LangId::C | LangId::Cpp => &[
            "if_statement",
            "for_statement",
            "while_statement",
            "do_statement",
            "case_statement",
            "conditional_expression",
            "catch_clause",
        ],
        // bash の `elif` は `elif_clause`、`case` の arm は `case_item`。
        // 旧汎用スライスはどちらも持たず過小計上だった (`until` は `while_statement`
        // として既に計上される)。`else_clause` は Python 同様に数えない。
        LangId::Bash => &[
            "if_statement",
            "elif_clause",
            "for_statement",
            "while_statement",
            "case_item",
        ],
        // lexer-only 言語は AST を持たず calculate_complexity へ到達しない。
        LangId::Xojo => &[],
    }
}
