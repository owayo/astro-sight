//! 型メンバーの分類で共有する型注釈の読取りと scalar 型の制約。

use crate::commands::api_changes::ts_required_members::{TypeAtom, simple_type};
use tree_sitter::Node;

/// コメントを除いて型が一つだけの注釈を、既存の簡易型モデルで読む。
pub(super) fn simple_annotation_type(annotation: Node<'_>, source: &[u8]) -> Option<Vec<TypeAtom>> {
    let mut cursor = annotation.walk();
    let mut children = annotation
        .named_children(&mut cursor)
        .filter(|child| child.kind() != "comment");
    let ty = children.next()?;
    children
        .next()
        .is_none()
        .then(|| simple_type(ty, source))
        .flatten()
}

/// 縮小・削除の証明では、配列・未知の型・読めない文字列リテラルを扱わない。
pub(super) fn is_scalar_atom(atom: &TypeAtom) -> bool {
    !matches!(
        atom,
        TypeAtom::Array(_) | TypeAtom::Any | TypeAtom::Unknown | TypeAtom::StrLit(None)
    )
}
