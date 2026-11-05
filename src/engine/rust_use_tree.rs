//! Shared Rust use-tree expansion for API exposure and impact ownership.

use std::rc::Rc;

/// use path の起点 (書かれた path の先頭)。
///
/// `target_module` はどの起点でもクレートルート起点の表現で持つ。`Bare` の path をどの
/// module から読むかは edition とスコープの束縛で決まる (2015 はクレートルート、2018 以降は
/// 現在の module のスコープか extern prelude) ため、ここでは決めずに呼び出し側へ渡す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsePathOrigin {
    /// `crate` / `self` / `super` で始まる (現在の module かクレートルートから解決済み)。
    Anchored,
    /// 識別子で始まる。`target_module` は書かれた path をクレートルート起点に並べたもの。
    Bare,
    /// `::` で始まる。2015 はクレートルート、2018 以降は extern crate だけを指す。
    Global,
}

/// `pub use` から生成される re-export edge。Named は `source_module::exported_name` が
/// `target_module::target_item` を指す。alias の場合 `exported_name = alias`、`target_item = 元名`。
/// Wildcard は `source_module::* = target_module::*` で名前ごとに伝播する。
/// `origin` は target の path の起点 (group の要素は group の path の起点を引き継ぐ)。
#[derive(Clone, Debug)]
pub(crate) enum RustPubUseEdge {
    Named {
        source_module: Vec<String>,
        exported_name: String,
        target_module: Vec<String>,
        target_item: String,
        origin: UsePathOrigin,
    },
    Wildcard {
        source_module: Vec<String>,
        target_module: Vec<String>,
        origin: UsePathOrigin,
    },
}

impl RustPubUseEdge {
    /// target の path の起点。
    pub(crate) fn origin(&self) -> UsePathOrigin {
        match self {
            Self::Named { origin, .. } | Self::Wildcard { origin, .. } => *origin,
        }
    }
}

/// 構造的に AST を walk して `pub use` re-export ターゲットを抽出する (whitespace / コメント非依存)。
///
/// `argument` ノードは tree-sitter-rust の以下のいずれかになる:
/// - `identifier`: 単一名 `pub use Foo;` (この crate root の Foo を再エクスポート)
/// - `scoped_identifier`: `path::name` 形式。`path` は field=path、`name` は field=name
/// - `scoped_use_list`: `path::{...}` 形式。`path` は field=path、`list` は field=list (use_list)
/// - `use_list`: `{...}` 形式 (path なし、トップでは稀)
/// - `use_as_clause`: `path as alias` 形式。`path` は field=path、`alias` は field=alias
/// - `use_wildcard`: `path::*` 形式。`path` は field=path (省略あり)
/// - `crate` / `self` / `super`: アンカーキーワード (再帰中に処理)
///
/// 戻り値 `None` で「判定不能」(root を超える super::、解決不能な anchor) — 呼出元は index を `None` にする。
///
/// `path_prefix` は解決済みの prefix で、起点はそれぞれの path の先頭から決める (呼び出し側は
/// 通常は空を渡す)。
///
/// `a::{b::{c::{…}}}` の入れ子の深さだけ再帰しないよう、明示スタックで前順に展開する。
/// 子は逆順に積むので、edge を出す順序と、失敗で打ち切る位置は子を順に再帰した場合と同じ。
pub(crate) fn expand_rust_use_tree_edges_ast(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path_prefix: &[String],
    current_module: &[String],
    source_module: &[String],
    alias_override: Option<&str>,
    out: &mut Vec<RustPubUseEdge>,
) -> Option<()> {
    let mut pending = vec![UseTreeFrame {
        node,
        path_prefix: Rc::from(path_prefix),
        prefix_origin: None,
        current_module,
        alias_override,
    }];
    while let Some(frame) = pending.pop() {
        let UseTreeFrame {
            node,
            path_prefix,
            prefix_origin,
            current_module,
            alias_override,
        } = frame;
        match node.kind() {
            "scoped_use_list" => {
                let mut path_node: Option<tree_sitter::Node<'_>> = None;
                let mut list_node: Option<tree_sitter::Node<'_>> = None;
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        "use_list" => {
                            list_node = Some(child);
                            break;
                        }
                        _ => path_node = Some(child),
                    }
                }
                let list = list_node?;
                let (resolved_prefix, origin) = match path_node {
                    Some(pn) => {
                        let (prefix, leaf, own_origin) =
                            resolve_use_path_node(pn, source, &path_prefix, current_module)?;
                        let mut p = prefix;
                        if let Some(name) = leaf {
                            p.push(name);
                        }
                        (Rc::from(p), prefix_origin.unwrap_or(own_origin))
                    }
                    // path の無い scoped_use_list は `::{a, b}` (先頭が `::`)。
                    None => (path_prefix, prefix_origin.unwrap_or(UsePathOrigin::Global)),
                };
                // prefix 付き group の要素の `self` は prefix 自身を指すので、要素には
                // current_module を渡さない (`self` を module anchor として読ませない)。
                push_use_list_items(&mut pending, list, &resolved_prefix, Some(origin), &[]);
            }
            "use_list" => {
                // prefix の無い group (`{a::b, self::c}`) の要素は、それぞれが use の引数と
                // 同じく字句的な module で解決する (`{self::x}` の `self` は現在の module)。
                // prefix 付き group の中の入れ子 (`a::{{b}}`) では current_module は既に空。
                push_use_list_items(
                    &mut pending,
                    node,
                    &path_prefix,
                    prefix_origin,
                    current_module,
                );
            }
            "use_as_clause" => {
                // [path, alias] 順。alias=`_` は外部非公開なので edge を作らない。
                let mut named: Vec<tree_sitter::Node<'_>> = Vec::new();
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    named.push(child);
                }
                if named.len() != 2 {
                    continue;
                }
                let alias_text = named[1].utf8_text(source).ok()?.trim();
                if alias_text == "_" {
                    continue;
                }
                // use_as_clause の内側 path には alias がさらに適用されるケースは無いので alias_override
                // をここで指定して下流の `scoped_identifier` 経路で edge 化する。
                pending.push(UseTreeFrame {
                    node: named[0],
                    path_prefix,
                    prefix_origin,
                    current_module,
                    alias_override: Some(alias_text),
                });
            }
            "use_wildcard" => {
                // named child = [path]
                let mut cursor = node.walk();
                let path_node = node.named_children(&mut cursor).next();
                let (target_module, origin) = if let Some(path_node) = path_node {
                    let (resolved_prefix, leaf_name, own_origin) =
                        resolve_use_path_node(path_node, source, &path_prefix, current_module)?;
                    let mut target_module = resolved_prefix;
                    if let Some(name) = leaf_name {
                        target_module.push(name);
                    }
                    (target_module, prefix_origin.unwrap_or(own_origin))
                } else {
                    if path_prefix.is_empty() && prefix_origin.is_none() {
                        return None;
                    }
                    (
                        path_prefix.to_vec(),
                        prefix_origin.unwrap_or(UsePathOrigin::Bare),
                    )
                };
                // 空の経路も crate root への有効な glob なので落とさない。
                out.push(RustPubUseEdge::Wildcard {
                    source_module: source_module.to_vec(),
                    target_module,
                    origin,
                });
            }
            "scoped_identifier" | "identifier" | "crate" | "self" | "super" => {
                // path::name 形式の単純 re-export、または anchor 単体。
                let (resolved_prefix, leaf_name, own_origin) =
                    resolve_use_path_node(node, source, &path_prefix, current_module)?;
                if let Some(item) = leaf_name {
                    // resolved_prefix が空 (crate root 直下の item / module) でも edge を
                    // 生成する。旧実装は silent drop しており、`pub use self::wifi as api;`
                    // のような root 直下モジュールの再エクスポートが API 面判定から漏れて
                    // pub fn の削除・変更が無音になっていた (モジュール到達性は
                    // compute_reexport_reachable_modules 側で固定点計算する)。
                    let exported_name = alias_override
                        .map(str::to_string)
                        .unwrap_or_else(|| item.clone());
                    out.push(RustPubUseEdge::Named {
                        source_module: source_module.to_vec(),
                        exported_name,
                        target_module: resolved_prefix,
                        target_item: item,
                        origin: prefix_origin.unwrap_or(own_origin),
                    });
                }
            }
            _ => {
                // 知らない kind は子供を walk (将来の grammar 変更に保守的に対応)。
                let mut cursor = node.walk();
                let children: Vec<_> = node
                    .children(&mut cursor)
                    .filter(|child| child.is_named())
                    .collect();
                for child in children.into_iter().rev() {
                    pending.push(UseTreeFrame {
                        node: child,
                        path_prefix: Rc::clone(&path_prefix),
                        prefix_origin,
                        current_module,
                        alias_override,
                    });
                }
            }
        }
    }
    Some(())
}

/// `expand_rust_use_tree_edges_ast` の展開待ちの 1 要素。
struct UseTreeFrame<'tree, 'a> {
    node: tree_sitter::Node<'tree>,
    path_prefix: Rc<[String]>,
    /// `path_prefix` を作った group の path の起点。`None` はまだ path が無い (use の引数
    /// そのもの、または prefix の無い group の要素)。`path_prefix` が空でも `crate::{..}` の
    /// 要素は `Anchored` なので、prefix の有無とは別に持つ。
    prefix_origin: Option<UsePathOrigin>,
    current_module: &'a [String],
    alias_override: Option<&'a str>,
}

/// `use_list` ノード (`{ ... }`) の各要素 (`,` 区切り) を展開待ちに積む。
/// 要素は group の path の起点 (`prefix_origin`) を引き継ぎ、`current_module` で解決する。
fn push_use_list_items<'tree, 'a>(
    pending: &mut Vec<UseTreeFrame<'tree, 'a>>,
    list: tree_sitter::Node<'tree>,
    path_prefix: &Rc<[String]>,
    prefix_origin: Option<UsePathOrigin>,
    current_module: &'a [String],
) {
    let mut cursor = list.walk();
    let items: Vec<_> = list
        .children(&mut cursor)
        .filter(|child| child.is_named())
        .collect();
    for item in items.into_iter().rev() {
        pending.push(UseTreeFrame {
            node: item,
            path_prefix: Rc::clone(path_prefix),
            prefix_origin,
            current_module,
            alias_override: None,
        });
    }
}

/// use path の 1 セグメント。先頭から順に平坦化して保持する。
#[derive(Debug, Clone, PartialEq, Eq)]
enum UsePathSegment {
    /// `crate` (クレートルート起点)
    Crate,
    /// `self` (現 module 起点)
    SelfMod,
    /// `super` (1 階層上)
    Super,
    /// 先頭の `::` (`::a::b`)。起点を表すだけで、module パスは積まない。
    Global,
    /// 通常の識別子
    Name(String),
}

/// `scoped_identifier` の入れ子を先頭から順の平坦なセグメント列へ畳む。
///
/// **path 位置だけを anchor として扱ってはいけない**: tree-sitter-rust は
/// `super::super::x` の 2 つ目の `super` を `scoped_identifier` の **name 位置**へ置く。
/// name 位置のテキストを無条件に識別子として扱う実装だと、`"super"` が文字列として
/// モジュールパスに積まれ、2 段以上の `super` を含む再エクスポートの解決が壊れる。
/// ノード kind で分類してから解決すれば、path / name のどちらに来ても同じ結果になる。
///
/// `a::b::…` の段数だけ再帰しないよう、明示スタックで前順に辿る。知らない kind は子を
/// 順に試し、最初に成功した子を採る (試した子が失敗したら `out` をその子の開始時点へ戻す)。
fn flatten_use_path(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    out: &mut Vec<UsePathSegment>,
) -> Option<()> {
    enum Task<'tree> {
        Visit(tree_sitter::Node<'tree>),
        /// 知らない kind の子を順に試している途中。`next` は次に試す子、`checkpoint` は
        /// 試す前の `out` の長さ。この要素まで戻ってきたら、試していた子が成功している。
        Attempt {
            children: Vec<tree_sitter::Node<'tree>>,
            next: usize,
            checkpoint: usize,
        },
    }

    let mut tasks = vec![Task::Visit(node)];
    while let Some(task) = tasks.pop() {
        let Task::Visit(node) = task else {
            // 試していた子が最後まで成功した。残りの子は試さない。
            continue;
        };
        let succeeded = match node.kind() {
            "crate" => {
                out.push(UsePathSegment::Crate);
                true
            }
            "self" => {
                out.push(UsePathSegment::SelfMod);
                true
            }
            "super" => {
                out.push(UsePathSegment::Super);
                true
            }
            "identifier" => match node.utf8_text(source) {
                Ok(text) => {
                    out.push(UsePathSegment::Name(text.trim().to_string()));
                    true
                }
                Err(_) => false,
            },
            "scoped_identifier" => {
                // tree-sitter-rust grammar は scoped_identifier で path/name の field 名を出さない。
                // named children は最大 2 つ: [path, name] または [name] のみ。
                // [name] だけなのは path を省いた `::name` (先頭が `::`) の形。
                let mut named: Vec<tree_sitter::Node<'_>> = Vec::new();
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    named.push(child);
                }
                match named.as_slice() {
                    [only] => {
                        out.push(UsePathSegment::Global);
                        tasks.push(Task::Visit(*only));
                        true
                    }
                    [path_node, name_node] => {
                        tasks.push(Task::Visit(*name_node));
                        tasks.push(Task::Visit(*path_node));
                        true
                    }
                    _ => false, // 想定外の named children 数
                }
            }
            _ => {
                // 知らない kind は子供を walk して可能な解決を試みる (従来の保守姿勢を維持)。
                let mut cursor = node.walk();
                let children: Vec<_> = node
                    .children(&mut cursor)
                    .filter(|child| child.is_named())
                    .collect();
                match children.first() {
                    Some(&first) => {
                        tasks.push(Task::Attempt {
                            children,
                            next: 1,
                            checkpoint: out.len(),
                        });
                        tasks.push(Task::Visit(first));
                        true
                    }
                    None => false,
                }
            }
        };
        if succeeded {
            continue;
        }
        // 失敗した。直近の試行まで戻り、その子の残りの作業と出力を捨てて次の子を試す。
        // 試行が無ければ全体の失敗、子を試し尽くした試行はさらに外側へ失敗を伝える。
        loop {
            match tasks.pop() {
                None => return None,
                Some(Task::Visit(_)) => {}
                Some(Task::Attempt {
                    children,
                    next,
                    checkpoint,
                }) => {
                    out.truncate(checkpoint);
                    if let Some(&child) = children.get(next) {
                        tasks.push(Task::Attempt {
                            children,
                            next: next + 1,
                            checkpoint,
                        });
                        tasks.push(Task::Visit(child));
                        break;
                    }
                }
            }
        }
    }
    Some(())
}

/// use path のノードを (モジュール prefix, 末尾の item 名, path の起点) へ解決する。
///
/// 先頭から続く `crate` / `self` / `super` を module anchor として消費し、残りの
/// 識別子列のうち末尾を item、それ以外を module パスとして扱う。
/// クレートルートを越える `super`、anchor が先頭以外に現れる不正な path、
/// 解決できないノードはすべて `None` (fail-closed)。
///
/// 起点は書かれた path の先頭だけから決める (group の要素なら、呼び出し側が group の path の
/// 起点を優先する)。anchor の無い path (`Bare` / `Global`) は従来どおり `path_prefix`
/// (トップレベルでは空 = クレートルート) から積む。
pub(crate) fn resolve_use_path_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path_prefix: &[String],
    current_module: &[String],
) -> Option<(Vec<String>, Option<String>, UsePathOrigin)> {
    let mut segments = Vec::new();
    flatten_use_path(node, source, &mut segments)?;
    let origin = match segments.first()? {
        UsePathSegment::Crate | UsePathSegment::SelfMod | UsePathSegment::Super => {
            UsePathOrigin::Anchored
        }
        UsePathSegment::Global => UsePathOrigin::Global,
        UsePathSegment::Name(_) => UsePathOrigin::Bare,
    };

    let mut prefix: Vec<String> = path_prefix.to_vec();
    let mut names: Vec<String> = Vec::new();
    for (idx, segment) in segments.iter().enumerate() {
        match segment {
            UsePathSegment::Global => {
                // `::` は path の先頭にしか書けない。
                if idx != 0 {
                    return None;
                }
            }
            UsePathSegment::Crate => {
                // 先頭以外の `crate` は Rust として不正。
                if idx != 0 {
                    return None;
                }
                prefix = Vec::new();
            }
            UsePathSegment::SelfMod => {
                if idx != 0 {
                    return None;
                }
                if prefix.is_empty() {
                    prefix = current_module.to_vec();
                }
            }
            UsePathSegment::Super => {
                // `super` は先頭から連続する場合のみ有効 (`a::super::b` は不正)。
                if !names.is_empty() {
                    return None;
                }
                if idx == 0 && prefix.is_empty() {
                    prefix = current_module.to_vec();
                }
                // クレートルートを越える `super` は解決不能。
                prefix.pop()?;
            }
            UsePathSegment::Name(name) => names.push(name.clone()),
        }
    }

    // grouped 形式 (`use a::b::{self};`) では、list 要素として**単独の `self`** が
    // path_prefix = ["a", "b"] を積んだ状態で届く (`expand_use_list_edges` が list の各要素へ
    // 解決済み prefix を渡すため)。直接形 `use a::b::self;` と同値なので、prefix の末尾を
    // item へ畳んで同じ edge を作る。畳まないと `leaf_name` が None になり
    // `expand_rust_use_tree_edges_ast` が Named edge を生成せず、private module 配下の
    // 公開 module をこの形で再エクスポートした場合に配下の API 削除を見逃す。
    //
    // path_prefix が空のとき (`use self::{a, b};` の path 位置に来る `self` など) は
    // module anchor としての `self` なので畳まない。
    if names.is_empty()
        && segments.as_slice() == [UsePathSegment::SelfMod]
        && !path_prefix.is_empty()
    {
        let item = prefix.pop()?;
        return Some((prefix, Some(item), origin));
    }

    // 末尾の `self` (`use a::b::self;` = `use a::b::{self};`) は「その module 自身」の
    // 再エクスポート。`self` は予約語なので item 名にはなり得ず、この位置に現れたら
    // 必ず trailing self 形式。直前の名前を item にする。
    //
    // tree-sitter は末尾の `self` を kind `self` ではなく `identifier` として返すため
    // (path 位置の `self` だけが kind `self` になる)、ここは `Name("self")` として届く。
    // 畳まないと「`self` という名前の item」という実在しない edge を作る。
    if names.last().is_some_and(|n| n == "self") {
        names.pop();
        if names.is_empty() {
            // `crate::self` のような、直前の名前が無い形は解決不能 (fail-closed)。
            return None;
        }
    }
    // 末尾以外に現れる `self` は Rust として不正なので解決しない。
    if names.iter().any(|n| n == "self") {
        return None;
    }

    let leaf = names.pop();
    prefix.extend(names);
    Some((prefix, leaf, origin))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::parser;
    use crate::language::LangId;

    /// 修正前の再帰版 (浅い入力で反復版と比べるためだけに残す)。path の起点と、prefix の無い
    /// group の要素へ current_module を渡す扱いは、反復版と同じ変更を入れてある。
    #[allow(clippy::too_many_arguments)]
    fn expand_recursive(
        node: tree_sitter::Node<'_>,
        source: &[u8],
        path_prefix: &[String],
        prefix_origin: Option<UsePathOrigin>,
        current_module: &[String],
        source_module: &[String],
        alias_override: Option<&str>,
        out: &mut Vec<RustPubUseEdge>,
    ) -> Option<()> {
        match node.kind() {
            "scoped_use_list" => {
                let mut path_node = None;
                let mut list_node = None;
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    match child.kind() {
                        "use_list" => {
                            list_node = Some(child);
                            break;
                        }
                        _ => path_node = Some(child),
                    }
                }
                let list = list_node?;
                let (resolved_prefix, origin) = match path_node {
                    Some(pn) => {
                        let (mut prefix, leaf, own_origin) =
                            resolve_use_path_node(pn, source, path_prefix, current_module)?;
                        if let Some(name) = leaf {
                            prefix.push(name);
                        }
                        (prefix, prefix_origin.unwrap_or(own_origin))
                    }
                    None => (
                        path_prefix.to_vec(),
                        prefix_origin.unwrap_or(UsePathOrigin::Global),
                    ),
                };
                expand_list_recursive(
                    list,
                    source,
                    &resolved_prefix,
                    Some(origin),
                    &[],
                    source_module,
                    out,
                )?;
            }
            "use_list" => expand_list_recursive(
                node,
                source,
                path_prefix,
                prefix_origin,
                current_module,
                source_module,
                out,
            )?,
            "use_as_clause" => {
                let mut cursor = node.walk();
                let named: Vec<_> = node.named_children(&mut cursor).collect();
                if named.len() != 2 {
                    return Some(());
                }
                let alias_text = named[1].utf8_text(source).ok()?.trim();
                if alias_text == "_" {
                    return Some(());
                }
                expand_recursive(
                    named[0],
                    source,
                    path_prefix,
                    prefix_origin,
                    current_module,
                    source_module,
                    Some(alias_text),
                    out,
                )?;
            }
            "use_wildcard" => {
                let mut cursor = node.walk();
                let (target_module, origin) =
                    if let Some(path_node) = node.named_children(&mut cursor).next() {
                        let (mut target_module, leaf_name, own_origin) =
                            resolve_use_path_node(path_node, source, path_prefix, current_module)?;
                        if let Some(name) = leaf_name {
                            target_module.push(name);
                        }
                        (target_module, prefix_origin.unwrap_or(own_origin))
                    } else {
                        if path_prefix.is_empty() && prefix_origin.is_none() {
                            return None;
                        }
                        (
                            path_prefix.to_vec(),
                            prefix_origin.unwrap_or(UsePathOrigin::Bare),
                        )
                    };
                out.push(RustPubUseEdge::Wildcard {
                    source_module: source_module.to_vec(),
                    target_module,
                    origin,
                });
            }
            "scoped_identifier" | "identifier" | "crate" | "self" | "super" => {
                let (resolved_prefix, leaf_name, own_origin) =
                    resolve_use_path_node(node, source, path_prefix, current_module)?;
                if let Some(item) = leaf_name {
                    out.push(RustPubUseEdge::Named {
                        source_module: source_module.to_vec(),
                        exported_name: alias_override
                            .map(str::to_string)
                            .unwrap_or_else(|| item.clone()),
                        target_module: resolved_prefix,
                        target_item: item,
                        origin: prefix_origin.unwrap_or(own_origin),
                    });
                }
            }
            _ => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.is_named() {
                        expand_recursive(
                            child,
                            source,
                            path_prefix,
                            prefix_origin,
                            current_module,
                            source_module,
                            alias_override,
                            out,
                        )?;
                    }
                }
            }
        }
        Some(())
    }

    fn expand_list_recursive(
        list: tree_sitter::Node<'_>,
        source: &[u8],
        path_prefix: &[String],
        prefix_origin: Option<UsePathOrigin>,
        current_module: &[String],
        source_module: &[String],
        out: &mut Vec<RustPubUseEdge>,
    ) -> Option<()> {
        let mut cursor = list.walk();
        for child in list.children(&mut cursor) {
            if child.is_named() {
                expand_recursive(
                    child,
                    source,
                    path_prefix,
                    prefix_origin,
                    current_module,
                    source_module,
                    None,
                    out,
                )?;
            }
        }
        Some(())
    }

    fn flatten_recursive(
        node: tree_sitter::Node<'_>,
        source: &[u8],
        out: &mut Vec<UsePathSegment>,
    ) -> Option<()> {
        match node.kind() {
            "crate" => out.push(UsePathSegment::Crate),
            "self" => out.push(UsePathSegment::SelfMod),
            "super" => out.push(UsePathSegment::Super),
            "identifier" => out.push(UsePathSegment::Name(
                node.utf8_text(source).ok()?.trim().to_string(),
            )),
            "scoped_identifier" => {
                let mut cursor = node.walk();
                let named: Vec<_> = node.named_children(&mut cursor).collect();
                return match named.as_slice() {
                    [only] => {
                        out.push(UsePathSegment::Global);
                        flatten_recursive(*only, source, out)
                    }
                    [path_node, name_node] => {
                        flatten_recursive(*path_node, source, out)?;
                        flatten_recursive(*name_node, source, out)
                    }
                    _ => None,
                };
            }
            _ => {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.is_named() {
                        let mut sub = Vec::new();
                        if flatten_recursive(child, source, &mut sub).is_some() {
                            out.extend(sub);
                            return Some(());
                        }
                    }
                }
                return None;
            }
        }
        Some(())
    }

    fn all_nodes(root: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
        let mut cursor = root.walk();
        let mut nodes = vec![root];
        let mut next = 0;
        while next < nodes.len() {
            let node = nodes[next];
            nodes.extend(node.children(&mut cursor));
            next += 1;
        }
        nodes
    }

    /// 反復に書き換えた use ツリーの展開と path の平坦化が、浅い入力で修正前の再帰版と
    /// 同じ結果 (edge の内容と順序、失敗の有無、失敗した時点までの出力) を返す。
    /// 平坦化は木の全ノードに当て、知らない kind の子を順に試す分岐 (先の子が失敗して
    /// 後の子で成功する、すべて失敗する) も通す。
    #[test]
    fn iterative_use_tree_walk_matches_recursive_version() {
        let source = b"pub use crate::a::b;\npub use self::c as d;\npub use super::super::e::{self, f as _, g::{h, i::*}};\npub use j::{k::{l, m}, n as o, self};\npub use crate::self;\npub use ::std::fmt;\npub use a::b::c::{self};\npub use super::{x, y::z};\npub use super::super::super::too::far;\npub use a::super::b;\npub use {p, q::r};\npub use crate::*;\npub use ::{s, t::u};\npub use {self::v::w, super::x2, crate::y2::{z2}, a2::b2, ::c2::*};\npub use crate::{d2::e2, self};\npub use f2::{{g2, h2::i2}, self as j2};\nfn t() { let _ = [1, a]; let _ = (1, 2); let _ = a::b::c; }\n";
        let tree = parser::parse_source(source, LangId::Rust).unwrap();
        let mut checked_edges = 0;
        for node in all_nodes(tree.root_node()) {
            let (mut iterative, mut recursive) = (Vec::new(), Vec::new());
            assert_eq!(
                (flatten_use_path(node, source, &mut iterative), &iterative),
                (flatten_recursive(node, source, &mut recursive), &recursive),
                "flatten {}: {}",
                node.kind(),
                node.utf8_text(source).unwrap()
            );
            if node.kind() != "use_declaration" {
                continue;
            }
            let argument = node.child_by_field_name("argument").unwrap();
            let module = |names: &[&str]| {
                names
                    .iter()
                    .map(|name| name.to_string())
                    .collect::<Vec<_>>()
            };
            for (prefix, current) in [
                (module(&[]), module(&["m", "n"])),
                (module(&["p"]), module(&["q"])),
                (module(&[]), module(&[])),
            ] {
                for alias in [None, Some("renamed")] {
                    let (mut iterative, mut recursive) = (Vec::new(), Vec::new());
                    let left = expand_rust_use_tree_edges_ast(
                        argument,
                        source,
                        &prefix,
                        &current,
                        &module(&["src"]),
                        alias,
                        &mut iterative,
                    );
                    let right = expand_recursive(
                        argument,
                        source,
                        &prefix,
                        None,
                        &current,
                        &module(&["src"]),
                        alias,
                        &mut recursive,
                    );
                    assert_eq!(
                        (left, format!("{iterative:?}")),
                        (right, format!("{recursive:?}")),
                        "expand {}",
                        node.utf8_text(source).unwrap()
                    );
                    checked_edges += iterative.len();
                }
            }
        }
        assert!(checked_edges > 20, "edge を出す入力を含む: {checked_edges}");
    }

    /// edge の起点は書かれた path の先頭から決まり、group の要素は group の path の起点を
    /// 引き継ぐ。prefix が空かどうかでは決めない (`crate::{..}` は prefix が空でも Anchored)。
    /// prefix の無い group の要素は字句的な module で解決する (`{self::x}` は現在の module)。
    #[test]
    fn edge_origin_follows_the_written_path_not_the_resolved_prefix() {
        use UsePathOrigin::{Anchored, Bare, Global};
        let module =
            |names: &[&str]| -> Vec<String> { names.iter().map(|name| name.to_string()).collect() };
        let current = module(&["reader"]);
        for (src, want) in [
            ("pub use crate::*;", vec![(vec![], None, Anchored)]),
            ("pub use crate::{*};", vec![(vec![], None, Anchored)]),
            ("pub use a::{*};", vec![(vec!["a"], None, Bare)]),
            (
                "pub use value::NewValue;",
                vec![(vec!["value"], Some("NewValue"), Bare)],
            ),
            (
                "pub use self::value::NewValue;",
                vec![(vec!["reader", "value"], Some("NewValue"), Anchored)],
            ),
            (
                "pub use crate::{value::X};",
                vec![(vec!["value"], Some("X"), Anchored)],
            ),
            (
                "pub use {value::X};",
                vec![(vec!["value"], Some("X"), Bare)],
            ),
            (
                "pub use {self::value::X};",
                vec![(vec!["reader", "value"], Some("X"), Anchored)],
            ),
            ("pub use {super::x};", vec![(vec![], Some("x"), Anchored)]),
            (
                "pub use value::{self, X};",
                vec![
                    (vec![], Some("value"), Bare),
                    (vec!["value"], Some("X"), Bare),
                ],
            ),
            (
                "pub use value::X as Y;",
                vec![(vec!["value"], Some("X"), Bare)],
            ),
            ("pub use value::*;", vec![(vec!["value"], None, Bare)]),
            (
                "pub use self::value::*;",
                vec![(vec!["reader", "value"], None, Anchored)],
            ),
            (
                "pub use ::value::X;",
                vec![(vec!["value"], Some("X"), Global)],
            ),
            (
                "pub use ::{a, b::c};",
                vec![(vec![], Some("a"), Global), (vec!["b"], Some("c"), Global)],
            ),
            (
                "pub use {a::b, crate::c::d, ::e::f};",
                vec![
                    (vec!["a"], Some("b"), Bare),
                    (vec!["c"], Some("d"), Anchored),
                    (vec!["e"], Some("f"), Global),
                ],
            ),
        ] {
            let tree = parser::parse_source(src.as_bytes(), LangId::Rust).unwrap();
            let argument = tree
                .root_node()
                .named_child(0)
                .and_then(|decl| decl.child_by_field_name("argument"))
                .unwrap();
            let mut edges = Vec::new();
            expand_rust_use_tree_edges_ast(
                argument,
                src.as_bytes(),
                &[],
                &current,
                &current,
                None,
                &mut edges,
            )
            .unwrap();
            let got: Vec<_> = edges
                .iter()
                .map(|edge| match edge {
                    RustPubUseEdge::Named {
                        target_module,
                        target_item,
                        origin,
                        ..
                    } => (target_module.clone(), Some(target_item.clone()), *origin),
                    RustPubUseEdge::Wildcard {
                        target_module,
                        origin,
                        ..
                    } => (target_module.clone(), None, *origin),
                })
                .collect();
            let want: Vec<_> = want
                .into_iter()
                .map(|(target, item, origin)| (module(&target), item.map(str::to_string), origin))
                .collect();
            assert_eq!(got, want, "{src}");
        }
    }
}
