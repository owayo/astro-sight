//! Rust `pub use` の AST 展開と path 解決。

use super::*;

/// AST を走査し `use_declaration` ノードから `pub use` re-export edge を集める。
///
/// - `current_module`: lib.rs 起点の current source 所属モジュール (super:: 解決 + source_module に使う)
/// - 戻り値 `None` = 「判定不能」 (解決不能な super:: や不正な use tree)。呼出元は index 全体を
///   `None` にして api.rm を残す (false negative より false positive を優先する fail-closed 方針)
///
/// **注**: Step A の `collect_pub_use_targets` から「inline_private_depth による pub use 除外」を外した。
/// 非 pub inline mod 配下の `pub use` でも、root から `pub use private_mod::x` されれば外部公開
/// 経路になり得るため。最終判定は `RustPubUseIndex::exposes_symbol` の固定点伝播で行う。
pub(crate) fn collect_pub_use_edges(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    current_module: &[String],
    edges: &mut Vec<RustPubUseEdge>,
) -> Option<()> {
    match node.kind() {
        "use_declaration" => {
            if !rust_node_has_unrestricted_pub_visibility(node, source) {
                return Some(());
            }
            let argument = node.child_by_field_name("argument")?;
            expand_rust_use_tree_edges_ast(
                argument,
                source,
                &[],
                current_module,
                current_module,
                None,
                edges,
            )?;
        }
        "mod_item" => {
            // #[path = "..."] でファイル名と module 名がずれる場合、source_module の解決を保守的に
            // 諦めて index 全体を None にする (codex Warning #3 対応、fail-closed)。
            if rust_mod_item_has_path_attribute(node, source) {
                return None;
            }
            let name = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                .map(str::to_string);
            let mut next_module = current_module.to_vec();
            if let Some(seg) = name {
                next_module.push(seg);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_pub_use_edges(child, source, &next_module, edges)?;
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                collect_pub_use_edges(child, source, current_module, edges)?;
            }
        }
    }
    Some(())
}

/// `mod_item` の直前の同一スコープ sibling に `#[path = "..."]` attribute があるかを返す。
/// tree-sitter-rust では attribute_item と mod_item は親 (source_file / declaration_list) の
/// 子として **隣接 sibling** に並ぶため、prev_sibling を逆方向に辿って attribute_item を集める。
/// `#[path]` が見つかったら true。
pub(crate) fn rust_mod_item_has_path_attribute(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut prev = node.prev_named_sibling();
    while let Some(sib) = prev {
        if sib.kind() != "attribute_item" {
            break; // 連続する attribute_item は積み上がるが、他の宣言が出たら終了
        }
        if attribute_item_is_path(sib, source) {
            return true;
        }
        prev = sib.prev_named_sibling();
    }
    false
}

/// `attribute_item` の中身が `#[path = ...]` か判定する。
pub(crate) fn attribute_item_is_path(attr_item: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut cursor = attr_item.walk();
    for child in attr_item.named_children(&mut cursor) {
        if child.kind() == "attribute" {
            // attribute の最初の identifier 子 (= attribute path) の text を見る。
            let mut inner = child.walk();
            for c in child.named_children(&mut inner) {
                if c.kind() == "identifier" || c.kind() == "scoped_identifier" {
                    return c.utf8_text(source).map(str::trim) == Ok("path");
                }
            }
        }
    }
    false
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
pub(crate) fn expand_rust_use_tree_edges_ast(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path_prefix: &[String],
    current_module: &[String],
    source_module: &[String],
    alias_override: Option<&str>,
    out: &mut Vec<RustPubUseEdge>,
) -> Option<()> {
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
            let resolved_prefix = match path_node {
                Some(pn) => {
                    let (prefix, leaf) =
                        resolve_use_path_node(pn, source, path_prefix, current_module)?;
                    let mut p = prefix;
                    if let Some(name) = leaf {
                        p.push(name);
                    }
                    p
                }
                None => path_prefix.to_vec(),
            };
            expand_use_list_edges(list, source, &resolved_prefix, source_module, out)?;
        }
        "use_list" => {
            expand_use_list_edges(node, source, path_prefix, source_module, out)?;
        }
        "use_as_clause" => {
            // [path, alias] 順。alias=`_` は外部非公開なので edge を作らない。
            let mut named: Vec<tree_sitter::Node<'_>> = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                named.push(child);
            }
            if named.len() != 2 {
                return Some(());
            }
            let alias_text = named[1].utf8_text(source).ok()?.trim();
            if alias_text == "_" {
                return Some(());
            }
            let path_node = named[0];
            // use_as_clause の内側 path には alias がさらに適用されるケースは無いので alias_override
            // をここで指定して下流の `scoped_identifier` 経路で edge 化する。
            expand_rust_use_tree_edges_ast(
                path_node,
                source,
                path_prefix,
                current_module,
                source_module,
                Some(alias_text),
                out,
            )?;
        }
        "use_wildcard" => {
            // named child = [path]
            let mut cursor = node.walk();
            let path_node = node.named_children(&mut cursor).next();
            if let Some(path_node) = path_node {
                let (resolved_prefix, leaf_name) =
                    resolve_use_path_node(path_node, source, path_prefix, current_module)?;
                let mut target_module = resolved_prefix;
                if let Some(name) = leaf_name {
                    target_module.push(name);
                }
                if !target_module.is_empty() {
                    out.push(RustPubUseEdge::Wildcard {
                        source_module: source_module.to_vec(),
                        target_module,
                    });
                }
            }
        }
        "scoped_identifier" | "identifier" | "crate" | "self" | "super" => {
            // path::name 形式の単純 re-export、または anchor 単体。
            let (resolved_prefix, leaf_name) =
                resolve_use_path_node(node, source, path_prefix, current_module)?;
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
                });
            }
        }
        _ => {
            // 知らない kind は子供を再帰 walk (将来の grammar 変更に保守的に対応)。
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named() {
                    expand_rust_use_tree_edges_ast(
                        child,
                        source,
                        path_prefix,
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

/// `use_list` ノード (`{ ... }`) の各要素 (`,` 区切り) を再帰展開して edge を出力する。
/// group 内では `current_module` は継承しない (空)。
pub(crate) fn expand_use_list_edges(
    list: tree_sitter::Node<'_>,
    source: &[u8],
    path_prefix: &[String],
    source_module: &[String],
    out: &mut Vec<RustPubUseEdge>,
) -> Option<()> {
    let mut cursor = list.walk();
    for child in list.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        expand_rust_use_tree_edges_ast(child, source, path_prefix, &[], source_module, None, out)?;
    }
    Some(())
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
fn flatten_use_path(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    out: &mut Vec<UsePathSegment>,
) -> Option<()> {
    match node.kind() {
        "crate" => {
            out.push(UsePathSegment::Crate);
            Some(())
        }
        "self" => {
            out.push(UsePathSegment::SelfMod);
            Some(())
        }
        "super" => {
            out.push(UsePathSegment::Super);
            Some(())
        }
        "identifier" => {
            out.push(UsePathSegment::Name(
                node.utf8_text(source).ok()?.trim().to_string(),
            ));
            Some(())
        }
        "scoped_identifier" => {
            // tree-sitter-rust grammar は scoped_identifier で path/name の field 名を出さない。
            // named children は最大 2 つ: [path, name] または [name] のみ
            // (path が省略された場合は crate root レベルの単一 identifier 扱い)。
            let mut named: Vec<tree_sitter::Node<'_>> = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                named.push(child);
            }
            match named.as_slice() {
                [only] => flatten_use_path(*only, source, out),
                [path_node, name_node] => {
                    flatten_use_path(*path_node, source, out)?;
                    flatten_use_path(*name_node, source, out)
                }
                _ => None, // 想定外の named children 数
            }
        }
        _ => {
            // 知らない kind は子供を再帰 walk して可能な解決を試みる (従来の保守姿勢を維持)。
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named() {
                    let mut sub = Vec::new();
                    if flatten_use_path(child, source, &mut sub).is_some() {
                        out.extend(sub);
                        return Some(());
                    }
                }
            }
            None
        }
    }
}

/// use path のノードを (モジュール prefix, 末尾の item 名) へ解決する。
///
/// 先頭から続く `crate` / `self` / `super` を module anchor として消費し、残りの
/// 識別子列のうち末尾を item、それ以外を module パスとして扱う。
/// クレートルートを越える `super`、anchor が先頭以外に現れる不正な path、
/// 解決できないノードはすべて `None` (fail-closed)。
pub(crate) fn resolve_use_path_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path_prefix: &[String],
    current_module: &[String],
) -> Option<(Vec<String>, Option<String>)> {
    let mut segments = Vec::new();
    flatten_use_path(node, source, &mut segments)?;
    if segments.is_empty() {
        return None;
    }

    let mut prefix: Vec<String> = path_prefix.to_vec();
    let mut names: Vec<String> = Vec::new();
    for (idx, segment) in segments.iter().enumerate() {
        match segment {
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
        return Some((prefix, Some(item)));
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
    Some((prefix, leaf))
}
