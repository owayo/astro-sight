//! Python の単純な lambda 束縛を、抽出・公開面・参照分類で共有する。

use tree_sitter::Node;

/// 単一の名前へ直接 lambda を代入する文だけを callable の宣言として扱う。
/// 連鎖代入・分割代入・属性代入は、名前ごとの契約を確定できないため含めない。
pub(crate) fn lambda_binding_name(node: Node<'_>) -> Option<Node<'_>> {
    if node.kind() != "assignment"
        || node.parent()?.kind() != "expression_statement"
        || node.child_by_field_name("right")?.kind() != "lambda"
    {
        return None;
    }
    let name = node.child_by_field_name("left")?;
    (name.kind() == "identifier").then_some(name)
}
