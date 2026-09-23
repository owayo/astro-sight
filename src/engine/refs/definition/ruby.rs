//! Ruby の定義コンテキスト判定と、シンボルリテラル由来の参照源。

use tree_sitter::Node;

use crate::language::LangId;

pub(crate) fn is_ruby_definition_context(node: Node<'_>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };

    match parent.kind() {
        "method" | "singleton_method" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "assignment" => parent
            .child_by_field_name("left")
            .is_some_and(|left| left.id() == node.id()),
        "class" | "module" => parent
            .child_by_field_name("name")
            .is_some_and(|name| name.id() == node.id()),
        "scope_resolution" => {
            let is_name = parent
                .child_by_field_name("name")
                .is_some_and(|name| name.id() == node.id());
            if !is_name {
                return false;
            }

            if let Some(grandparent) = parent.parent() {
                match grandparent.kind() {
                    "assignment" => grandparent
                        .child_by_field_name("left")
                        .is_some_and(|left| left.id() == parent.id()),
                    "class" | "module" => grandparent
                        .child_by_field_name("name")
                        .is_some_and(|name| name.id() == parent.id()),
                    _ => false,
                }
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Ruby のシンボルリテラルを、名前部分の参照セグメント `(name, row, col)` として返す。
///
/// Ruby はメソッドをシンボルで名指しする (`before_action :set_user` / `after_action
/// :log_access` / `validate :check_name` / `send(:name)` / `alias_method :a, :b`)。
/// シンボルは identifier ノードではないため通常の走査では数えられず、シンボル経由で
/// しか呼ばれないメソッドが dead に出ていた (`refs` も定義行しか返さなかった)。
///
/// - `simple_symbol` (`:name`): 先頭の `:` を除いた部分
/// - `bare_symbol` (`%i[name]` の要素) / `delimited_symbol` (`:"name"` / `%s(name)`):
///   子が `string_content` 1 つだけのとき、その中身。補間・エスケープを含むものは名前が
///   静的に決まらないので対象外
/// - 値を省略したハッシュキー / キーワード引数 (`{ token: }` / `deliver(token:)`、Ruby 3.1+):
///   `{ token: token }` の略記で、キー名と同名のローカル変数かメソッドを**読む式**になる
///   (JS/TS の object shorthand `{ handler }` と同じ形)
///
/// `{ status: :ok }` の `:ok` のような無関係なシンボルも参照として数えられるが、
/// 「参照を過大に数える = dead と断定しない」保守側として許容する。値を伴うハッシュキー
/// (`{ key: value }` の `key:`) はキー名であってメソッドの名指しではないので数えない。
pub(crate) fn ruby_symbol_ref_segment<'a>(
    node: Node<'_>,
    source: &'a [u8],
    lang_id: LangId,
) -> Option<(&'a str, usize, usize)> {
    if lang_id != LangId::Ruby {
        return None;
    }
    match node.kind() {
        "simple_symbol" => {
            let name = node.utf8_text(source).ok()?.strip_prefix(':')?;
            if name.is_empty() {
                return None;
            }
            let pos = node.start_position();
            Some((name, pos.row, pos.column + 1))
        }
        "bare_symbol" | "delimited_symbol" => {
            if node.named_child_count() != 1 {
                return None;
            }
            let content = node.named_child(0)?;
            if content.kind() != "string_content" {
                return None;
            }
            let name = content.utf8_text(source).ok()?;
            if name.is_empty() {
                return None;
            }
            let pos = content.start_position();
            Some((name, pos.row, pos.column))
        }
        // `hash_key_symbol` のノード範囲は末尾の `:` を含まない。
        "hash_key_symbol" => {
            let pair = node.parent()?;
            let is_omitted_value_key = pair.kind() == "pair"
                && pair.child_by_field_name("value").is_none()
                && pair
                    .child_by_field_name("key")
                    .is_some_and(|key| key.id() == node.id());
            if !is_omitted_value_key {
                return None;
            }
            let pos = node.start_position();
            Some((node.utf8_text(source).ok()?, pos.row, pos.column))
        }
        _ => None,
    }
}
