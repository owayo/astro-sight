//! Rust `pub use` の AST 展開と path 解決。

use super::*;

/// `pub use` の edge 1 本と、先頭が識別子の path (`UsePathOrigin::Bare`) の先頭の名前が
/// use と同じスコープで何に束縛されるか、および use 自身が条件なしか。
///
/// 束縛は、ルート以外の module (ファイルの module か inline mod の body) に書かれた Bare の
/// edge だけに付ける。ルートの module では 2015 (クレートルート起点) と 2018 以降 (現在の
/// module 起点) の読み方が一致し、関数の本体などのブロックに書かれた use は外から辿れない
/// ので、どちらも従来どおりの edge のまま扱う。edition による読み直しは呼び出し側が行う。
#[derive(Clone, Debug)]
pub(crate) struct CollectedPubUseEdge {
    pub(crate) edge: RustPubUseEdge,
    pub(crate) bare_leading: Option<BareLeadingBinding>,
    /// use 自身に条件や束縛を書き換え得る属性がない場合だけ glob を隠す根拠に使う。
    pub(crate) unconditional: bool,
}

/// Bare な path の先頭の名前の、use と同じスコープでの束縛 (2018 以降の解決規則で読む)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BareLeadingBinding {
    /// 条件なし・一意・マクロの生成でない子 `mod` の宣言。2018 以降はこの子 module に解決する
    /// (宣言した名前は同名の extern crate や glob import より優先される)。
    ChildModule,
    /// 名前を束縛し得るもの (named import・別名・`extern crate`・同名の項目・条件付きか重複した
    /// `mod`・glob import・item 位置のマクロ) があり、どこを指すか決められない。
    Unresolved,
    /// スコープに束縛が無い。2018 以降は extern prelude の名前 (依存の crate) を指す。
    Unbound,
}

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
    edges: &mut Vec<CollectedPubUseEdge>,
) -> Option<()> {
    use std::rc::Rc;

    // #[path = "..."] でファイル名と module 名がずれる場合、source_module の解決を保守的に
    // 諦めて index 全体を None にする (codex Warning #3 対応、fail-closed)。ブロックの中の
    // `mod` も別のファイルを読み込み得るので、module のスコープに限らずファイル全体で探す。
    if crate::engine::tree_walk::any_node(node, crate::engine::tree_walk::Children::All, |node| {
        node.kind() == "mod_item" && rust_mod_item_has_path_attribute(node, source)
    }) {
        return None;
    }
    let mut scopes = ScopeBindingsCache::default();
    // module のスコープ (ファイル・inline mod の body) だけを前順に辿る。関数の本体などの
    // ブロックの中の `use` と `mod` は外の module から辿れず公開経路を作らないので、edge にしない
    // (混ぜると glob の shadow 判定が、glob で入る module を誤って隠す)。ERROR の中は辿らない
    // (構文エラーのあるファイルでは、呼び出し側の `collect_rust_pub_use_index` が index 自体を
    // 作らない)。要素は (ノード, 所属モジュール)。
    let mut stack = vec![(node, Rc::new(current_module.to_vec()))];
    while let Some((node, current_module)) = stack.pop() {
        match node.kind() {
            "use_declaration" => {
                if !rust_node_has_unrestricted_pub_visibility(node, source) {
                    continue;
                }
                let argument = node.child_by_field_name("argument")?;
                let mut expanded = Vec::new();
                expand_rust_use_tree_edges_ast(
                    argument,
                    source,
                    &[],
                    &current_module,
                    &current_module,
                    None,
                    &mut expanded,
                )?;
                for edge in expanded {
                    let bare_leading =
                        bare_leading_binding(node, source, &current_module, &edge, &mut scopes);
                    edges.push(CollectedPubUseEdge {
                        edge,
                        bare_leading,
                        unconditional: item_attributes_are_plain(node, source),
                    });
                }
            }
            "mod_item" => {
                let Some(body) = node.child_by_field_name("body") else {
                    continue;
                };
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(str::to_string);
                let mut next_module = current_module.as_ref().clone();
                if let Some(seg) = name {
                    next_module.push(seg);
                }
                stack.push((body, Rc::new(next_module)));
            }
            "source_file" | "declaration_list" => {
                // 左の子から順に処理するため、子を逆順に積む。所属モジュールは子どうしで共有する。
                let mut cursor = node.walk();
                let start = stack.len();
                stack.extend(
                    node.named_children(&mut cursor)
                        .map(|child| (child, Rc::clone(&current_module))),
                );
                stack[start..].reverse();
            }
            _ => {}
        }
    }
    Some(())
}

/// スコープ (ノードの id) ごとの束縛。そのスコープに Bare な `pub use` があるときだけ作る。
#[derive(Default)]
pub(crate) struct ScopeBindingsCache {
    scopes: std::collections::HashMap<usize, ScopeBindings>,
}

/// `use_node` (pub な `use_declaration`) から展開した `edge` の、先頭の名前の束縛。
/// Bare でない edge と、ルートの module に書かれた use の edge は `None` (従来どおりの edge の
/// まま扱う)。module のスコープの直下にあると確かめられない use は Unresolved。
pub(crate) fn bare_leading_binding(
    use_node: tree_sitter::Node<'_>,
    source: &[u8],
    current_module: &[String],
    edge: &RustPubUseEdge,
    cache: &mut ScopeBindingsCache,
) -> Option<BareLeadingBinding> {
    if edge.origin() != UsePathOrigin::Bare || current_module.is_empty() {
        return None;
    }
    let Some(scope) = use_node.parent().filter(|scope| is_module_scope(*scope)) else {
        return Some(BareLeadingBinding::Unresolved);
    };
    let bindings = cache
        .scopes
        .entry(scope.id())
        .or_insert_with(|| ScopeBindings::collect(scope, source, current_module));
    Some(
        bare_leading_name(edge).map_or(BareLeadingBinding::Unresolved, |name| {
            bindings.classify(name)
        }),
    )
}

/// module のスコープ (ファイルの module か inline mod の body) か。
fn is_module_scope(node: tree_sitter::Node<'_>) -> bool {
    match node.kind() {
        "source_file" => true,
        "declaration_list" => node
            .parent()
            .is_some_and(|parent| parent.kind() == "mod_item"),
        _ => false,
    }
}

/// Bare な edge の target の path に書かれた先頭の名前 (`target_module` はクレートルート起点に
/// 書かれた path を並べたもの。単独の名前 `pub use name;` は `target_item`)。
pub(crate) fn bare_leading_name(edge: &RustPubUseEdge) -> Option<&str> {
    match edge {
        RustPubUseEdge::Named {
            target_module,
            target_item,
            ..
        } => Some(target_module.first().unwrap_or(target_item)),
        RustPubUseEdge::Wildcard { target_module, .. } => target_module.first(),
    }
    .map(String::as_str)
}

/// raw identifier (`r#name`) を名前の比較用に外す。
fn normalize_raw_identifier(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

/// スコープの直下の宣言が束縛する名前 (raw identifier は外して持つ)。
#[derive(Default)]
struct ScopeBindings {
    /// 直下の `mod` 宣言の名前 → 宣言ごとに「条件なしで、宣言を書き換え得る属性も無い」か。
    child_mods: std::collections::HashMap<String, Vec<bool>>,
    /// `mod` 以外で名前を束縛する宣言 (named import・別名・`extern crate`・項目)。
    other_names: std::collections::HashSet<String>,
    /// `other_names` のうち、同名の子 `mod` と競合し得るもの。型の名前空間に入り得ない項目
    /// (fn・const・static・macro_rules) と、その子 mod の中を指す自分の再公開
    /// (`pub use value::value;`) は子 mod と共存できるので含めない。
    child_competitors: std::collections::HashSet<String>,
    /// glob import・item 位置のマクロ・展開できない use・構文エラーがあり、どの名前も束縛し得る。
    open: bool,
}

impl ScopeBindings {
    fn collect(scope: tree_sitter::Node<'_>, source: &[u8], current_module: &[String]) -> Self {
        let mut bindings = Self::default();
        let mut cursor = scope.walk();
        for item in scope.named_children(&mut cursor) {
            if item.is_error() || item.is_missing() {
                bindings.open = true;
                continue;
            }
            match item.kind() {
                "attribute_item" | "inner_attribute_item" | "line_comment" | "block_comment" => {}
                "mod_item" => match item_name(item, source) {
                    Some(name) => bindings
                        .child_mods
                        .entry(name)
                        .or_default()
                        .push(mod_declaration_is_unconditional(item, source)),
                    None => bindings.open = true,
                },
                "use_declaration" => bindings.bind_use(item, source, current_module),
                "extern_crate_declaration" => match item
                    .child_by_field_name("alias")
                    .or_else(|| item.child_by_field_name("name"))
                    .and_then(|name| name.utf8_text(source).ok())
                {
                    Some(name) => {
                        bindings.bind(normalize_raw_identifier(name.trim()).to_string(), true);
                    }
                    None => bindings.open = true,
                },
                // item 位置のマクロは任意の項目を作り得る (`foo! { .. }` と `foo!(..);`)。
                "macro_invocation" | "expression_statement" => bindings.open = true,
                // `extern { fn name(); }` の項目は外側の module の名前になる。
                "foreign_mod_item" => {
                    if let Some(body) = item.child_by_field_name("body") {
                        let mut inner = body.walk();
                        for foreign in body.named_children(&mut inner) {
                            if let Some(name) = item_name(foreign, source) {
                                let competes = !is_value_namespace_item(foreign.kind());
                                bindings.bind(name, competes);
                            }
                        }
                    }
                }
                _ => {
                    if let Some(name) = item_name(item, source) {
                        bindings.bind(name, !is_value_namespace_item(item.kind()));
                    }
                }
            }
        }
        bindings
    }

    fn bind(&mut self, name: String, competes_with_child_mod: bool) {
        if competes_with_child_mod {
            self.child_competitors.insert(name.clone());
        }
        self.other_names.insert(name);
    }

    /// `use` が束縛する名前を足す (pub かどうかは問わない)。glob は任意の名前を束縛し得る。
    fn bind_use(&mut self, item: tree_sitter::Node<'_>, source: &[u8], current_module: &[String]) {
        let Some(argument) = item.child_by_field_name("argument") else {
            self.open = true;
            return;
        };
        // path の無い `{*}` は展開で edge にならないので、glob は構文で見つける。
        if crate::engine::tree_walk::any_node(
            argument,
            crate::engine::tree_walk::Children::All,
            |node| node.kind() == "use_wildcard",
        ) {
            self.open = true;
        }
        let mut edges = Vec::new();
        if expand_rust_use_tree_edges_ast(
            argument,
            source,
            &[],
            current_module,
            current_module,
            None,
            &mut edges,
        )
        .is_none()
        {
            self.open = true;
            return;
        }
        for edge in edges {
            match edge {
                RustPubUseEdge::Named {
                    exported_name,
                    target_module,
                    target_item,
                    origin,
                    ..
                } => {
                    let name = normalize_raw_identifier(&exported_name).to_string();
                    // 束縛する名前と同名の子 mod の中を指す再公開 (`pub use value::value;`・
                    // `use self::value::value;`) は、その子 mod が在ることを前提にしており、
                    // 子 mod の代わりに `value` を束縛することはない。
                    let path_head = match origin {
                        UsePathOrigin::Bare => target_module.first().or(Some(&target_item)),
                        UsePathOrigin::Anchored => target_module
                            .strip_prefix(current_module)
                            .and_then(<[String]>::first),
                        UsePathOrigin::Global => None,
                    };
                    let through_child = path_head
                        .is_some_and(|head| normalize_raw_identifier(head) == name.as_str());
                    self.bind(name, !through_child);
                }
                RustPubUseEdge::Wildcard { .. } => self.open = true,
            }
        }
    }

    fn classify(&self, name: &str) -> BareLeadingBinding {
        let name = normalize_raw_identifier(name);
        match self.child_mods.get(name).map(Vec::as_slice) {
            // 子 mod と競合し得るものが同じスコープにあれば、コンパイルが通るのはどちらかが
            // cfg で消える場合で、どちらが残るかは決められない。
            Some([true]) if !self.child_competitors.contains(name) => {
                BareLeadingBinding::ChildModule
            }
            Some(_) => BareLeadingBinding::Unresolved,
            None if self.open || self.other_names.contains(name) => BareLeadingBinding::Unresolved,
            None => BareLeadingBinding::Unbound,
        }
    }
}

/// 型の名前空間に入らず、同名の `mod` と共存できる項目か (`use value::X` の先頭の `value` は
/// 型の名前空間で解決するので、これらは子 mod の解決に影響しない)。
fn is_value_namespace_item(kind: &str) -> bool {
    matches!(
        kind,
        "function_item"
            | "function_signature_item"
            | "const_item"
            | "static_item"
            | "macro_definition"
    )
}

/// 項目の `name` field の名前 (raw identifier を外す)。
fn item_name(item: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let name = item.child_by_field_name("name")?.utf8_text(source).ok()?;
    Some(normalize_raw_identifier(name.trim()).to_string())
}

/// 属性が条件 (`cfg` / `test`) でも、項目を書き換え得るもの (`cfg_attr` や proc macro かも
/// しれない未知の属性) でもないか。
fn attribute_is_plain(attribute: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let name = attribute
        .named_child(0)
        .and_then(|attribute| attribute.named_child(0))
        .and_then(|name| name.utf8_text(source).ok());
    crate::engine::impact::rust_std_origin::safe_attribute(attribute, source).is_some()
        && !matches!(name, Some("cfg" | "test"))
}

/// `mod` 宣言に条件 (`#[cfg]` / `#[test]`) も、宣言を書き換え得る属性 (`#[cfg_attr]` や
/// proc macro かもしれない未知の属性) も付いていないか。inline mod は body の内部属性も見る
/// (外部ファイルの module の内部属性は module tree の走査がそのファイルを読むときに見る)。
pub(crate) fn mod_declaration_is_unconditional(item: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    item_attributes_are_plain(item, source)
        && item
            .child_by_field_name("body")
            .is_none_or(|body| module_inner_attributes_are_plain(body, source))
}

/// 項目の直前の属性が条件や未知の属性でないか。
pub(crate) fn item_attributes_are_plain(item: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut previous = item.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "line_comment" | "block_comment" => {}
            "attribute_item" if attribute_is_plain(sibling, source) => {}
            "attribute_item" => return false,
            _ => break,
        }
        previous = sibling.prev_named_sibling();
    }
    true
}

/// module の中身 (外部ファイルの `source_file` か inline mod の body) の内部属性
/// (`#![cfg(..)]` など、その module 全体に掛かる) がどれも条件や未知の属性でないか。
/// 直下に構文エラーがあると内部属性を確かめられないので false。
pub(crate) fn module_inner_attributes_are_plain(
    contents: tree_sitter::Node<'_>,
    source: &[u8],
) -> bool {
    let mut cursor = contents.walk();
    contents.named_children(&mut cursor).all(|child| {
        !(child.is_error() || child.is_missing())
            && (child.kind() != "inner_attribute_item" || attribute_is_plain(child, source))
    })
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
