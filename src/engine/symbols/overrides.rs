use tree_sitter::Node;

use crate::language::LangId;
use crate::models::location::Range;

use super::node_for_symbol_range;

/// メソッドが親 interface/class のメンバーを override しているかを判定する。
///
/// Kotlin/Swift/TS/C# は `override` 修飾子、Java は `@Override` アノテーションで判別する。
/// override メソッドは親型のディスパッチ経由（Android framework の listener callback など）
/// で呼ばれるため cross-file refs では caller を追跡できず、dead-code / API 変更判定の両方で
/// 誤検出源になるため明示的に除外する。
pub fn is_override_method(
    root: Node,
    source: &[u8],
    lang_id: LangId,
    symbol_range: &Range,
) -> bool {
    let Some(node) = node_for_symbol_range(root, symbol_range) else {
        return false;
    };
    let Some(decl) = find_method_declaration_for_override(node) else {
        return false;
    };

    match lang_id {
        LangId::Java => declaration_has_java_override_annotation(decl, source),
        LangId::Kotlin | LangId::Swift | LangId::CSharp => {
            declaration_has_override_modifier(decl, source)
        }
        LangId::Typescript | LangId::Tsx => declaration_has_override_modifier(decl, source),
        _ => false,
    }
}

/// 関数/メソッド宣言ノード（override 判定対象）を探す。
/// `find_enclosing_declaration` と異なり TS の `method_definition` も対象に含める。
fn find_method_declaration_for_override(node: Node) -> Option<Node> {
    let decl_kinds = [
        "function_declaration",
        "method_declaration",
        "method_definition",
    ];
    let mut current = Some(node);
    while let Some(n) = current {
        if decl_kinds.contains(&n.kind()) {
            return Some(n);
        }
        current = n.parent();
    }
    None
}

/// `modifiers` 子ノードのテキストに `override` キーワードが含まれるかをチェックする。
/// 単純な部分文字列マッチだと `overrider` のような名前にも反応するため、トークン境界を確認する。
fn declaration_has_override_modifier(decl: Node, source: &[u8]) -> bool {
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        let Ok(text) = child.utf8_text(source) else {
            continue;
        };
        if contains_keyword(text, "override") {
            return true;
        }
    }
    // `modifiers` コンテナを経由しない言語は method 宣言の直接子キーワードとして現れる。
    // tree-sitter-typescript の `override` キーワードは kind = "override_modifier"
    // (`"override"` ではない) のため両方を照合する (GitLab #36: TS の
    // `public override formatAttributes()` が dead 誤検出されていた)。
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        if matches!(child.kind(), "override" | "override_modifier") {
            return true;
        }
    }
    false
}

/// Java の `@Override` マーカーアノテーションが宣言の modifiers に含まれるかをチェックする。
fn declaration_has_java_override_annotation(decl: Node, source: &[u8]) -> bool {
    let mut cursor = decl.walk();
    for child in decl.children(&mut cursor) {
        if child.kind() != "modifiers" {
            continue;
        }
        // marker_annotation / annotation 子ノードを走査して @Override を探す。
        let mut inner = child.walk();
        for annot in child.children(&mut inner) {
            if !matches!(annot.kind(), "marker_annotation" | "annotation") {
                continue;
            }
            let Ok(text) = annot.utf8_text(source) else {
                continue;
            };
            let stripped = text.trim_start_matches('@').trim();
            if stripped == "Override" || stripped.ends_with(".Override") {
                return true;
            }
        }
    }
    false
}

/// 指定キーワードがトークン境界（英数字・アンダースコア以外）で囲まれて現れるかを判定する。
pub(super) fn contains_keyword(text: &str, keyword: &str) -> bool {
    let bytes = text.as_bytes();
    let kw = keyword.as_bytes();
    if bytes.len() < kw.len() {
        return false;
    }
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i + kw.len() <= bytes.len() {
        if &bytes[i..i + kw.len()] == kw {
            let before_ok = i == 0 || !is_word(bytes[i - 1]);
            let after_ok = i + kw.len() == bytes.len() || !is_word(bytes[i + kw.len()]);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}
