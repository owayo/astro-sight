//! Rust module visibility と `pub use` re-export graph。

use super::*;

/// base 側 crate の public-reachable な module 群から集めた `pub use` re-export ターゲット。
/// re-export edge graph + public-reachable module 集合 + 逆引き map。
/// `collect_rust_pub_use_index_at_base` で base 側 crate 全体を 1 度走査して構築し、
/// `exposes_symbol` で削除シンボルから固定点伝播して公開到達性を判定する。
pub(crate) struct RustPubUseIndex {
    edges: Vec<RustPubUseEdge>,
    /// 外部から到達可能な module 集合。`pub mod` 経路 (root = `[]`) を seed に、
    /// module 再エクスポート (`pub use internal::wifi;` / `pub use self::wifi as api;`)
    /// で到達可能になる module とその pub 子孫を固定点で加えたもの。
    reachable_modules: std::collections::HashSet<Vec<String>>,
    /// `(target_module, target_item)` → Named edge index。Named 伝播の逆引き。
    named_by_target: std::collections::HashMap<RustExportKey, Vec<usize>>,
    /// `target_module` → Wildcard edge index。Wildcard 伝播の逆引き。
    wildcard_by_target_module: std::collections::HashMap<Vec<String>, Vec<usize>>,
    signature_types: std::collections::HashSet<super::signature_types::TypeKey>,
    signature_unknown: bool,
}

/// 「ある module でこの名前がエクスポートされている」を表す key。固定点計算の単位。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RustExportKey {
    module: Vec<String>,
    name: String,
}

impl RustPubUseIndex {
    /// `info` の private module 配下の `symbol_name` が外部公開 API として到達可能かを返す。
    /// 削除シンボルを seed として live export 集合を固定点伝播し、
    /// live ∩ reachable_modules ≠ ∅ なら true。reachable_modules は `pub mod` 経路に
    /// module 再エクスポート由来の到達 module を加えた集合のため、seed の module 自体が
    /// `pub use self::wifi as api;` で公開されているケースも item 伝播なしで検出できる。
    pub(super) fn exposes_symbol(&self, info: &RustPrivateModuleInfo, symbol_name: &str) -> bool {
        let item = rust_reexport_item_name(symbol_name).to_string();
        self.signature_unknown
            || self
                .signature_types
                .contains(&super::signature_types::TypeKey {
                    module: info.module_segments.clone(),
                    name: item.clone(),
                })
            || self.exposes_key(&info.module_segments, &item)
    }

    fn exposes_key(&self, module: &[String], name: &str) -> bool {
        let seed = RustExportKey {
            module: module.to_vec(),
            name: name.to_owned(),
        };
        self.propagate_live_exports(seed)
            .into_iter()
            .any(|key| self.reachable_modules.contains(&key.module))
    }

    /// 削除 seed から逆向きに live export を BFS で伝播。HashSet で重複を防いで循環で停止する。
    fn propagate_live_exports(
        &self,
        seed: RustExportKey,
    ) -> std::collections::HashSet<RustExportKey> {
        use std::collections::{HashSet, VecDeque};
        let mut live: HashSet<RustExportKey> = HashSet::new();
        let mut queue: VecDeque<RustExportKey> = VecDeque::new();
        live.insert(seed.clone());
        queue.push_back(seed);
        while let Some(key) = queue.pop_front() {
            if let Some(edge_ids) = self.named_by_target.get(&key) {
                for &idx in edge_ids {
                    if let RustPubUseEdge::Named {
                        source_module,
                        exported_name,
                        ..
                    } = &self.edges[idx]
                    {
                        let next = RustExportKey {
                            module: source_module.clone(),
                            name: exported_name.clone(),
                        };
                        if live.insert(next.clone()) {
                            queue.push_back(next);
                        }
                    }
                }
            }
            if let Some(edge_ids) = self.wildcard_by_target_module.get(&key.module) {
                for &idx in edge_ids {
                    if let RustPubUseEdge::Wildcard { source_module, .. } = &self.edges[idx] {
                        let next = RustExportKey {
                            module: source_module.clone(),
                            name: key.name.clone(),
                        };
                        if live.insert(next.clone()) {
                            queue.push_back(next);
                        }
                    }
                }
            }
        }
        live
    }
}

#[cfg(test)]
impl RustPubUseIndex {
    /// 読み直した後の edge (テストで正規化先を確かめる)。
    pub(crate) fn edges(&self) -> &[RustPubUseEdge] {
        &self.edges
    }
}

/// re-export item 名。Rust の method は `Container.method` qualname で出るが re-export 対象 item は
/// container の `Container`。free function / struct 等は bare name。
pub(crate) fn rust_reexport_item_name(name: &str) -> &str {
    if let Some((container, _method)) = name.split_once('.') {
        container
    } else {
        bare_name(name)
    }
}

/// base 側 crate の src/ 配下を全 .rs 走査して `pub use` を edge として集め、public-reachable module
/// 集合と逆引き map を構築する。public-reachable filter は collect 段階では外し (private module 内の
/// pub use も root から `pub use private::x` されれば公開になり得るため)、最終判定は `exposes_symbol`
/// の固定点伝播で行う。`git ls-tree` / `git show` / parse / path 解決のいずれかで判定不能になったら
/// `None` を返して `api.rm` を残す (false negative 回避)。
/// Rust crate の src/ 配下を `source` 経由で全走査し、`pub use` re-export edge graph と
/// public-reachable module 集合を構築する (リファクタ Step 3: `_at_base` / `_at_worktree` の
/// 本体統合)。public-reachable filter は collect 段階では外し、最終判定は
/// `exposes_symbol` の固定点伝播で行う。`ls-tree` / `read` / parse / path 解決のいずれかが
/// 失敗したら `None` を返す (`api.rm` / `api.add` を残す方向、false negative 回避)。
pub(crate) fn collect_rust_pub_use_index(
    source: RustSourceTree<'_>,
    dir: &str,
    info: &RustPrivateModuleInfo,
) -> Option<RustPubUseIndex> {
    let files = collect_rust_rs_files(source, dir, &info.src_root_rel)?;
    let module_tree = collect_all_modules(source, dir, info)?;
    let mut collected: Vec<CollectedPubUseEdge> = Vec::new();
    let mut signatures = super::signature_types::SignatureTypes::default();
    for file in files {
        let Ok(rel_to_src) = file.strip_prefix(&info.src_root_rel) else {
            continue;
        };
        let module_path = module_path_segments(rel_to_src);
        let file_source = read_rs_blob(source, dir, &file)?;
        let tree = parser::parse_source(&file_source, crate::language::LangId::Rust).ok()?;
        // 構文エラー (ERROR / MISSING) があると、module の直下の公開宣言がエラーのノードに
        // 取り込まれて edge が黙って欠け得る。公開経路を確かめられないので index を作らない。
        if tree.root_node().has_error() {
            return None;
        }
        collect_pub_use_edges(tree.root_node(), &file_source, &module_path, &mut collected)?;
        if module_tree.pub_children.contains_key(&module_path)
            && (!module_path.is_empty() || rel_to_src == std::path::Path::new("lib.rs"))
        {
            signatures.collect(tree.root_node(), &file_source, &module_path)?;
        }
    }
    // edition と self 別名は、どちらも判定する側の snapshot から読む。
    let edition = crate::engine::rust_edition::library_edition(&info.crate_root_rel, |manifest| {
        read_rust_manifest(source, dir, manifest)
    });
    let collected = resolve_global_pub_use_edges(collected, edition, || {
        read_rust_module_source(
            source,
            dir,
            &info.crate_root_rel,
            std::path::Path::new("lib.rs"),
        )
    })?;
    let collected = resolve_bare_pub_use_edges(collected, &module_tree, || edition)?;
    let unconditional_named_edges: std::collections::HashSet<_> = collected
        .iter()
        .enumerate()
        .filter(|(_, collected)| {
            collected.unconditional && matches!(collected.edge, RustPubUseEdge::Named { .. })
        })
        .map(|(index, _)| index)
        .collect();
    let edges: Vec<_> = collected
        .into_iter()
        .map(|collected| collected.edge)
        .collect();
    let all_modules = &module_tree.pub_children;
    let mut named_by_target: std::collections::HashMap<RustExportKey, Vec<usize>> =
        std::collections::HashMap::new();
    let mut wildcard_by_target_module: std::collections::HashMap<Vec<String>, Vec<usize>> =
        std::collections::HashMap::new();
    for (idx, edge) in edges.iter().enumerate() {
        match edge {
            RustPubUseEdge::Named {
                target_module,
                target_item,
                ..
            } => {
                let key = RustExportKey {
                    module: target_module.clone(),
                    name: target_item.clone(),
                };
                named_by_target.entry(key).or_default().push(idx);
            }
            RustPubUseEdge::Wildcard { target_module, .. } => {
                wildcard_by_target_module
                    .entry(target_module.clone())
                    .or_default()
                    .push(idx);
            }
        }
    }
    let public_modules = pub_reachable_modules(all_modules);
    let reachable_modules = compute_reexport_reachable_modules(
        &edges,
        &public_modules,
        &module_tree,
        &unconditional_named_edges,
    );
    let mut index = RustPubUseIndex {
        edges,
        reachable_modules,
        named_by_target,
        wildcard_by_target_module,
        signature_types: std::collections::HashSet::new(),
        signature_unknown: false,
    };
    let aliases = read_rust_module_source(
        source,
        dir,
        &info.crate_root_rel,
        std::path::Path::new("lib.rs"),
    )
    .and_then(|source| global_self_aliases(&source));
    let (types, unknown) = signatures.reachable(
        all_modules,
        &module_tree.unconditional,
        edition,
        aliases,
        |module, name| index.exposes_key(module, name),
    );
    index.signature_types = types;
    index.signature_unknown = unknown;
    Some(index)
}

/// Global の経路をローカルのグラフへ混ぜない。2015 と self 別名だけを root 起点へ直す。
fn resolve_global_pub_use_edges(
    collected: Vec<CollectedPubUseEdge>,
    edition: Option<crate::engine::rust_edition::RustEdition>,
    root_source: impl FnOnce() -> Option<Vec<u8>>,
) -> Option<Vec<CollectedPubUseEdge>> {
    use crate::engine::rust_edition::RustEdition;
    if !collected
        .iter()
        .any(|collected| collected.edge.origin() == UsePathOrigin::Global)
    {
        return Some(collected);
    }
    let edition = edition?;
    let aliases = match edition {
        RustEdition::E2015 => std::collections::HashSet::new(),
        RustEdition::E2018OrLater => global_self_aliases(&root_source()?)?,
    };
    let mut result = Vec::new();
    for mut collected in collected {
        if collected.edge.origin() == UsePathOrigin::Global {
            let (target_module, target_item, origin) = match &mut collected.edge {
                RustPubUseEdge::Named {
                    target_module,
                    target_item,
                    origin,
                    ..
                } => (target_module, Some(target_item), origin),
                RustPubUseEdge::Wildcard {
                    target_module,
                    origin,
                    ..
                } => (target_module, None, origin),
            };
            if edition == RustEdition::E2018OrLater {
                let first = target_module.first().or(target_item.as_deref())?;
                if !aliases.contains(first.strip_prefix("r#").unwrap_or(first)) {
                    // extern prelude の経路は、この crate 内の宣言や module を運ばない。
                    continue;
                }
                if target_module.is_empty() {
                    // crate 自体の名前付き再公開は、この item グラフでは表現できない。
                    return None;
                }
                target_module.remove(0);
            }
            *origin = UsePathOrigin::Anchored;
        }
        result.push(collected);
    }
    Some(result)
}

/// root で確実に extern prelude へ入る self 別名を集める。生成され得る別名は推測しない。
fn global_self_aliases(source: &[u8]) -> Option<std::collections::HashSet<String>> {
    let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut aliases = std::collections::HashSet::new();
    let mut cursor = root.walk();
    for item in root.named_children(&mut cursor) {
        match item.kind() {
            "attribute_item" | "inner_attribute_item" => {
                crate::engine::impact::rust_std_origin::safe_attribute(item, source)?;
                if item
                    .named_child(0)?
                    .named_child(0)?
                    .utf8_text(source)
                    .ok()?
                    == "derive"
                {
                    // derive 名の置換を証明していないため、生成される root の項目を決めない。
                    return None;
                }
            }
            "macro_invocation" => return None,
            "expression_statement"
                if item
                    .named_child(0)
                    .is_some_and(|child| child.kind() == "macro_invocation") =>
            {
                return None;
            }
            "extern_crate_declaration" => {
                let name = item.child_by_field_name("name")?.utf8_text(source).ok()?;
                if name == "self" {
                    if !item_attributes_are_plain(item, source) {
                        return None;
                    }
                    let alias = item.child_by_field_name("alias")?.utf8_text(source).ok()?;
                    if !aliases.insert(alias.strip_prefix("r#").unwrap_or(alias).to_owned()) {
                        return None;
                    }
                }
            }
            _ => {}
        }
    }
    Some(aliases)
}

/// 先頭が識別子の edge (Bare) を、edition と use と同じスコープの束縛で読み直す。
///
/// - 束縛を見ない edge (ルートの module の use)、module tree に無い module (どの `mod` 宣言からも
///   辿れないファイル) の use、edition 2015、スコープに束縛の無い名前 (2018 以降は extern
///   prelude の名前): 従来どおりクレートルート起点の edge のまま
/// - 2018 以降で先頭の名前が子 `mod` に束縛され、その子 module が条件なしと確かめられた
///   (宣言・inline の body・外部ファイルの内部属性に `cfg` や未知の属性が無い): 現在の module
///   起点に付け替える
/// - それ以外 (2018 以降で束縛を決められない・子 module の条件を確かめられない、edition が
///   不明で子 `mod` か未解決の束縛を持つ): `None`。index を不完全として、公開面による抑制を
///   諦めさせる (公開面の外と断定しない)。
///
/// edition が不明なときに両方の読み方の edge を作らないのは、推測の edge が glob の shadow
/// 判定 (`compute_reexport_reachable_modules`) に混ざると、glob で入る module を誤って隠し得る
/// ため (クレートルート起点では module、子 module 起点では関数になる名前の再公開など)。
/// `edition` は Bare な edge の束縛を読み直すときだけ呼ぶ (manifest を読む回数を増やさない)。
fn resolve_bare_pub_use_edges(
    collected: Vec<CollectedPubUseEdge>,
    module_tree: &ModuleTree,
    edition: impl FnOnce() -> Option<crate::engine::rust_edition::RustEdition>,
) -> Option<Vec<CollectedPubUseEdge>> {
    use crate::engine::rust_edition::RustEdition;
    let edition = if collected.iter().any(|edge| edge.bare_leading.is_some()) {
        edition()
    } else {
        None
    };
    collected
        .into_iter()
        .map(|mut collected| {
            let Some(binding) = collected.bare_leading else {
                return Some(collected);
            };
            let edge = &mut collected.edge;
            let (RustPubUseEdge::Named { source_module, .. }
            | RustPubUseEdge::Wildcard { source_module, .. }) = &edge;
            // module tree に無い module の use は、外から辿れる公開経路にならない。
            if !module_tree.pub_children.contains_key(source_module) {
                return Some(collected);
            }
            match (binding, edition) {
                (_, Some(RustEdition::E2015)) | (BareLeadingBinding::Unbound, _) => {}
                (BareLeadingBinding::ChildModule, Some(RustEdition::E2018OrLater)) => {
                    let mut child = source_module.clone();
                    child.push(bare_leading_name(edge)?.to_string());
                    if !module_tree.is_unconditional(&child) {
                        return None;
                    }
                    let (RustPubUseEdge::Named {
                        source_module,
                        target_module,
                        ..
                    }
                    | RustPubUseEdge::Wildcard {
                        source_module,
                        target_module,
                        ..
                    }) = edge;
                    target_module.splice(0..0, source_module.iter().cloned());
                }
                (BareLeadingBinding::ChildModule | BareLeadingBinding::Unresolved, _) => {
                    return None;
                }
            }
            Some(collected)
        })
        .collect()
}

/// module 再エクスポート (`pub use path::to::module;`) による外部到達可能 module 集合を
/// 固定点計算する。seed は `pub mod` 経路の `public_modules`。
///
/// - **Named edge** (`pub use path::to::module;`): source module が到達可能なら、target path が
///   実在する **pub 宣言された** module の場合に限りその module も到達可能になり、pub 子孫 module
///   も連鎖する。private module 自体の再エクスポート (`mod wifi;` + `pub use self::wifi as api;`)
///   は rustc では E0365 で無効なため、親の pub_children に含まれない target は reachable 化しない。
/// - **Wildcard edge** (`pub use internal::*;`): source module が到達可能なら、target の
///   **pub 子 module とその pub 子孫**を到達可能にする (target 自体は入れない — 直下の item は
///   `propagate_live_exports` の exact-match wildcard 伝播が拾う)。glob は module ではなく
///   **中身**を再エクスポートするので target が private module でも有効で (E0365 は module 自体を
///   名前として再エクスポートする場合の制限)、target の `declared_pub` は要求しない。
///   ただし **glob import は source namespace の同名 item に shadow される**ため、型名前空間を
///   占める同名 item (子 module / module の named re-export) が条件なしに存在すると証明できた
///   場合だけ到達可能化しない。
///   旧実装は Named 以外を `else { continue; }` で捨てていたため、
///   `mod internal; pub use internal::*;` 構成で **pub 子 module 配下**の公開 API 変更が無音に
///   なっていた (落ちるのは seed の module が wildcard の target と一致しない子 module 配下)。
///
/// どちらの経路でも private child module は親が公開されても外部から辿れないため含めない
/// (単純な prefix 判定だと private 子まで誤って公開扱いになる)。
fn compute_reexport_reachable_modules(
    edges: &[RustPubUseEdge],
    public_modules: &std::collections::HashSet<Vec<String>>,
    module_tree: &ModuleTree,
    unconditional_named_edges: &std::collections::HashSet<usize>,
) -> std::collections::HashSet<Vec<String>> {
    let all_modules = &module_tree.pub_children;
    let mut reachable = public_modules.clone();
    // glob shadow 判定用: Named re-export が source namespace へ持ち込む **module 名**。
    //
    // **名前空間を区別する必要がある**。Rust では `mod api` は型名前空間、`fn api` は
    // 値名前空間なので共存でき、`pub use other::thing as api;` (thing は関数) は
    // glob 由来の module `api` を shadow しない。名前だけで shadow 扱いにすると、
    // 実際に公開されている `api::found` の削除を見逃す (false negative)。
    // use と target module、その祖先が条件なしに存在すると証明できた場合だけ隠す。
    let named_module_reexports: std::collections::HashSet<(&Vec<String>, &String)> = edges
        .iter()
        .enumerate()
        .filter(|(index, _)| unconditional_named_edges.contains(index))
        .filter_map(|(_, e)| match e {
            RustPubUseEdge::Named {
                source_module,
                target_module,
                target_item,
                exported_name,
                ..
            } => {
                let mut target_path = target_module.clone();
                target_path.push(target_item.clone());
                (module_tree.is_unconditional_path(source_module)
                    && module_tree.is_unconditional_path(&target_path))
                .then_some((source_module, exported_name))
            }
            RustPubUseEdge::Wildcard { .. } => None,
        })
        .collect();
    loop {
        let mut changed = false;
        for edge in edges {
            let (source_module, target_module, target_item) = match edge {
                RustPubUseEdge::Named {
                    source_module,
                    target_module,
                    target_item,
                    ..
                } => (source_module, target_module, Some(target_item)),
                RustPubUseEdge::Wildcard {
                    source_module,
                    target_module,
                    ..
                } => (source_module, target_module, None),
            };
            if !reachable.contains(source_module) {
                continue;
            }
            let Some(target_item) = target_item else {
                // glob 再エクスポート (`pub use internal::*;`) は target module の pub item を
                // source namespace へ持ち込む。到達可能化するのは **target の pub 子 module** で、
                // それらの配下 item が `S::child::x` として外から見えるようになる
                // (target 直下の item は `propagate_live_exports` の exact-match wildcard 伝播が
                // 拾うので、ここで target 自体を reachable にする必要はない)。
                // private 子 module は `pub mod` 宣言されていないので `all_modules` の
                // pub_children に載らず、はじめから対象外。
                //
                // Named と違い target module 自体の `declared_pub` は要求しない。glob は module
                // 自体ではなく**中身**を再エクスポートするので、`mod internal; pub use internal::*;`
                // のように target が private module でも有効 (E0365 は module 自体を名前として
                // 再エクスポートする場合の制限)。
                let Some(pub_children) = all_modules.get(target_module) else {
                    continue;
                };
                for child in pub_children {
                    // **glob import は source namespace の明示 item / named import に shadow
                    // される**。glob が持ち込むのは module なので、shadow するのも
                    // **型名前空間を占める同名 item** に限る:
                    // (a) 条件なしの同名の子 module が source にある (private でも占める)、
                    // (b) 同名を条件なしの **module の** named re-export で持ち込んでいる。
                    // 関数などの値名前空間の同名 re-export は共存するので shadow しない。
                    //
                    // 検出できるのはこの 2 つの signal まで。型名前空間の非 module item
                    // (`pub struct api`) や `pub use` でない private named import
                    // (`use other::module as api;`) による shadow は、item 単位の情報や
                    // 非公開 import の収集が要るため見ておらず、その場合は従来どおり
                    // (= 修正前と同じ) 到達可能扱いに倒れる。
                    let mut shadow_probe = source_module.clone();
                    shadow_probe.push(child.clone());
                    if module_tree.is_unconditional_path(&shadow_probe)
                        || named_module_reexports.contains(&(source_module, child))
                    {
                        continue;
                    }
                    let mut candidate = target_module.clone();
                    candidate.push(child.clone());
                    if !all_modules.contains_key(&candidate) || reachable.contains(&candidate) {
                        continue;
                    }
                    reachable.insert(candidate.clone());
                    changed = true;
                    insert_pub_descendants(&mut reachable, all_modules, candidate);
                }
                continue;
            };
            let mut candidate = target_module.clone();
            candidate.push(target_item.clone());
            if !all_modules.contains_key(&candidate) || reachable.contains(&candidate) {
                continue;
            }
            // E0365: private module は公開再エクスポートできない。candidate 自体が
            // 親 module で `pub mod` と宣言されている場合のみ到達可能にする。
            let declared_pub = all_modules
                .get(target_module)
                .is_some_and(|pub_children| pub_children.contains(target_item));
            if !declared_pub {
                continue;
            }
            reachable.insert(candidate.clone());
            changed = true;
            // 到達可能になった module の pub 子孫 module を連鎖登録する。
            insert_pub_descendants(&mut reachable, all_modules, candidate);
        }
        if !changed {
            break;
        }
    }
    reachable
}

/// `all_modules` の pub 子リストを辿り、`start` の pub 子孫 module を `reachable` に登録する。
/// `start` 自身は登録済みである前提。
fn insert_pub_descendants(
    reachable: &mut std::collections::HashSet<Vec<String>>,
    all_modules: &std::collections::HashMap<Vec<String>, Vec<String>>,
    start: Vec<String>,
) {
    let mut stack = vec![start];
    while let Some(m) = stack.pop() {
        if let Some(pub_children) = all_modules.get(&m) {
            for c in pub_children {
                let mut child = m.clone();
                child.push(c.clone());
                if reachable.insert(child.clone()) {
                    stack.push(child);
                }
            }
        }
    }
}

/// crate root から `pub mod` 経路のみで到達できる module 集合を `all_modules`
/// (全 module → pub 宣言された子 module 名) から導出する。root `[]` は常に含む。
///
/// 旧実装は同じ module ツリー walk を「pub 経路のみ版」(`public_reachable_modules` +
/// `collect_public_pub_mods`) と「全 module 版」(`collect_all_modules` +
/// `collect_modules_with_visibility`) の 2 本持ちで、module ファイル解決の修正を
/// 両方へ入れる必要があった (片方だけ直して階層 module を取りこぼした経緯あり)。
/// 全 module 版は各 module の pub 子リストを持つため pub 経路の到達性はここで導出でき、
/// module ツリーの読み込み + parse も 1 回で済む。
fn pub_reachable_modules(
    all_modules: &std::collections::HashMap<Vec<String>, Vec<String>>,
) -> std::collections::HashSet<Vec<String>> {
    let mut reachable = std::collections::HashSet::new();
    reachable.insert(Vec::new());
    insert_pub_descendants(&mut reachable, all_modules, Vec::new());
    reachable
}

/// module ファイル内の子 `mod name;` 宣言が指すファイルの基準ディレクトリ。
/// `lib.rs` / `main.rs` / `mod.rs` は自身と同じディレクトリ、file-style module
/// (`internal.rs`) は `internal/` 配下に子を持つ (Rust 2018+ のモジュール解決)。
/// 旧実装は常に parent dir を使い、`internal.rs` 内の `pub mod api;` を
/// `api.rs` と誤解決して階層 module の到達性を取りこぼしていた。
fn child_module_base_dir(current_file_rel: &std::path::Path) -> std::path::PathBuf {
    use std::path::Path;
    let parent = current_file_rel
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let file_name = current_file_rel
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if matches!(file_name, "lib.rs" | "main.rs" | "mod.rs") {
        parent
    } else {
        match current_file_rel.file_stem().and_then(|s| s.to_str()) {
            Some(stem) => parent.join(stem),
            None => parent,
        }
    }
}

/// crate の module tree (`mod` 宣言から辿れる module だけ。可視性は問わない)。
pub(crate) struct ModuleTree {
    /// module 絶対 path → `pub` と宣言された子 module 名のリスト。
    pub(crate) pub_children: std::collections::HashMap<Vec<String>, Vec<String>>,
    /// module 絶対 path → 条件なしと確かめられたか。宣言のすべてに条件 (`cfg` 等) も宣言を
    /// 書き換え得る属性も無く、中身 (inline の body か外部ファイル) の内部属性も同様で、
    /// 外部ファイルを読めた場合だけ true。
    unconditional: std::collections::HashMap<Vec<String>, bool>,
}

impl ModuleTree {
    fn is_unconditional(&self, module: &[String]) -> bool {
        self.unconditional.get(module) == Some(&true)
    }

    /// 祖先の条件で消える module も、必ずある束縛とはみなさない。
    fn is_unconditional_path(&self, module: &[String]) -> bool {
        (0..=module.len()).all(|depth| self.is_unconditional(&module[..depth]))
    }
}

/// `module` が条件なしかを記録する。一度でも条件付きと分かった module は条件付きのまま。
fn record_unconditional(
    map: &mut std::collections::HashMap<Vec<String>, bool>,
    module: &[String],
    unconditional: bool,
) {
    map.entry(module.to_vec())
        .and_modify(|known| *known &= unconditional)
        .or_insert(unconditional);
}

/// crate 内の**全 module** (可視性問わず) を walk し、module 絶対 path →
/// 「`pub` と宣言された子 module 名のリスト」と、各 module が条件なしかを返す。
/// module 再エクスポートの到達性固定点で「target が module か」
/// 「reachable module の pub 子孫」の判定に使う。判定不能 (`#[path]` 等) は
/// `None` (呼出元で index 全体を諦めて api.rm を残す fail-closed)。
/// 外部ファイルの module の内部属性は、判定する側の source tree (base は base の snapshot)
/// から読んだファイルで確かめる。
pub(crate) fn collect_all_modules(
    source: RustSourceTree<'_>,
    dir: &str,
    info: &RustPrivateModuleInfo,
) -> Option<ModuleTree> {
    use std::collections::HashMap;
    use std::path::PathBuf;
    let mut result: HashMap<Vec<String>, Vec<String>> = HashMap::new();
    result.insert(Vec::new(), Vec::new());
    let mut unconditional: HashMap<Vec<String>, bool> = HashMap::new();
    let mut frontier: Vec<(Vec<String>, PathBuf)> = vec![(Vec::new(), PathBuf::from("lib.rs"))];
    while let Some((segments, current_rel)) = frontier.pop() {
        let module_source =
            read_rust_module_source(source, dir, &info.crate_root_rel, &current_rel)?;
        let tree = parser::parse_source(&module_source, crate::language::LangId::Rust).ok()?;
        // 構文エラーのあるファイルは `mod` 宣言を取りこぼし得る (edge の収集と同じく諦める。
        // ignore された module ファイルは edge の収集の走査に入らないので、ここでも確かめる)。
        if tree.root_node().has_error() {
            return None;
        }
        // ファイル先頭の内部属性 (`#![cfg(..)]` など) はこの module 全体に掛かる。
        record_unconditional(
            &mut unconditional,
            &segments,
            module_inner_attributes_are_plain(tree.root_node(), &module_source),
        );
        let base_dir = child_module_base_dir(&current_rel);
        let mut collector = ModuleVisibilityCollector {
            source,
            dir,
            info,
            result: &mut result,
            unconditional: &mut unconditional,
            frontier: &mut frontier,
        };
        collector.collect(tree.root_node(), &module_source, &segments, &base_dir)?;
    }
    Some(ModuleTree {
        pub_children: result,
        unconditional,
    })
}

/// `collect_public_pub_mods` の全 module 版。private mod にも潜って module 集合を作り、
/// pub な子 module 名だけを親エントリに記録する。
/// `base_dir` は子 `mod name;` 宣言のファイル解決基準 dir で、inline module
/// (`mod internal { pub mod api; }`) へ潜るたびに module 名を積む — inline 階層内の
/// 外部 mod 宣言は `internal/api.rs` を指すため。
struct ModuleVisibilityCollector<'a, 'b> {
    source: RustSourceTree<'a>,
    dir: &'a str,
    info: &'a RustPrivateModuleInfo,
    result: &'b mut std::collections::HashMap<Vec<String>, Vec<String>>,
    /// module → 条件なしと確かめられたか (宣言の属性・inline の body の内部属性・外部ファイルの
    /// 有無。外部ファイルの内部属性は `collect_all_modules` がファイルを読むときに足す)。
    unconditional: &'b mut std::collections::HashMap<Vec<String>, bool>,
    frontier: &'b mut Vec<(Vec<String>, std::path::PathBuf)>,
}

impl ModuleVisibilityCollector<'_, '_> {
    /// `node` 配下の `mod` 宣言を前順に辿り、module 集合と pub な子の一覧を作る。
    /// 辿るのは module のスコープ (ファイル・inline mod の body) だけで、関数の本体などの
    /// ブロックの中の `mod` は外の module tree に入れない (外から辿れない)。
    /// inline module の入れ子は入力次第で深くなるため、再帰せず明示スタックで辿る。
    /// 要素は (ノード, 所属モジュール, 子 `mod name;` の解決基準 dir)。
    fn collect(
        &mut self,
        node: tree_sitter::Node<'_>,
        source_bytes: &[u8],
        current_segments: &[String],
        base_dir: &std::path::Path,
    ) -> Option<()> {
        use std::path::PathBuf;
        use std::rc::Rc;

        let mut stack: Vec<(tree_sitter::Node<'_>, Rc<Vec<String>>, Rc<PathBuf>)> = vec![(
            node,
            Rc::new(current_segments.to_vec()),
            Rc::new(base_dir.to_path_buf()),
        )];
        while let Some((node, current_segments, base_dir)) = stack.pop() {
            let start = stack.len();
            match node.kind() {
                "mod_item" => {
                    if rust_mod_item_has_path_attribute(node, source_bytes) {
                        return None;
                    }
                    let name = node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source_bytes).ok())
                        .map(str::to_string)?;
                    let is_pub = rust_node_has_unrestricted_pub_visibility(node, source_bytes);
                    let mut child_segments = current_segments.as_ref().clone();
                    child_segments.push(name.clone());
                    if is_pub {
                        self.result
                            .entry(current_segments.as_ref().clone())
                            .or_default()
                            .push(name.clone());
                    }
                    self.result.entry(child_segments.clone()).or_default();
                    // 宣言の属性と inline の body の内部属性。外部ファイルの内部属性は
                    // `collect_all_modules` がそのファイルを読むときに足す。
                    record_unconditional(
                        self.unconditional,
                        &child_segments,
                        mod_declaration_is_unconditional(node, source_bytes),
                    );
                    let child_segments = Rc::new(child_segments);
                    let mut has_inline_body = false;
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        if child.kind() == "declaration_list" {
                            has_inline_body = true;
                            let inline_base = Rc::new(base_dir.join(&name));
                            let mut inner_cursor = child.walk();
                            stack.extend(child.named_children(&mut inner_cursor).map(|inner| {
                                (inner, Rc::clone(&child_segments), Rc::clone(&inline_base))
                            }));
                        }
                    }
                    if !has_inline_body {
                        let as_mod = base_dir.join(&name).join("mod.rs");
                        let as_file = base_dir.join(format!("{name}.rs"));
                        let child_segments = child_segments.as_ref().clone();
                        if read_rust_module_source(
                            self.source,
                            self.dir,
                            &self.info.crate_root_rel,
                            &as_mod,
                        )
                        .is_some()
                        {
                            self.frontier.push((child_segments, as_mod));
                        } else if read_rust_module_source(
                            self.source,
                            self.dir,
                            &self.info.crate_root_rel,
                            &as_file,
                        )
                        .is_some()
                        {
                            self.frontier.push((child_segments, as_file));
                        } else {
                            // ファイルが見つからない module は中身の条件を確かめられない。
                            record_unconditional(self.unconditional, &child_segments, false);
                        }
                    }
                }
                // module のスコープの中身 (ファイルの直下) だけを辿る。inline mod の body の中身は
                // 上の `mod_item` で直接積んでいる。ERROR の中は辿らない (構文エラーのある
                // ファイルでは `collect_all_modules` が module tree 自体を作らない)。
                "source_file" => {
                    let mut cursor = node.walk();
                    stack.extend(
                        node.named_children(&mut cursor).map(|child| {
                            (child, Rc::clone(&current_segments), Rc::clone(&base_dir))
                        }),
                    );
                }
                _ => {}
            }
            // 左の子から順に処理するため、今回積んだ分を逆順にする。
            stack[start..].reverse();
        }
        Some(())
    }
}

/// `src/` 配下の `.rs` ファイル (file は dir 相対) を `source` 経由で読む。
/// Worktree なら `std::fs::read(<canonical_dir>/<file>)`、Base なら `<rev>:<file>` の blob。
pub(crate) fn read_rs_blob(
    source: RustSourceTree<'_>,
    dir: &str,
    file: &std::path::Path,
) -> Option<Vec<u8>> {
    match source {
        RustSourceTree::Worktree => {
            let canonical_dir = std::fs::canonicalize(dir).ok()?;
            let abs = canonical_dir.join(file);
            std::fs::read(abs).ok()
        }
        RustSourceTree::Base { blobs } => blobs.read(file.to_str()?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_use_paths_need_edition_and_proven_self_aliases() {
        use crate::engine::rust_edition::RustEdition::{E2015, E2018OrLater};
        let resolve = |edition, root: &str| {
            let source = b"pub use ::dep::api; pub use ::dep::*;";
            let tree = parser::parse_source(source, crate::language::LangId::Rust).unwrap();
            let mut edges = Vec::new();
            collect_pub_use_edges(tree.root_node(), source, &[], &mut edges).unwrap();
            resolve_global_pub_use_edges(edges, edition, || Some(root.as_bytes().to_vec()))
        };
        assert!(resolve(Some(E2018OrLater), "").unwrap().is_empty());
        assert!(resolve(None, "").is_none());
        for root in [
            "#[cfg(any())] extern crate self as dep;",
            "generate_alias!();",
            "#[custom] struct Item;",
            "#[derive(Debug)] struct Item;",
            "extern crate self as dep; extern crate self as dep;",
            "extern crate self as",
        ] {
            assert!(resolve(Some(E2018OrLater), root).is_none(), "{root}");
        }
        for (edition, root, target) in [
            (E2015, "", vec!["dep"]),
            (E2018OrLater, "extern crate self as dep;", vec![]),
            (E2018OrLater, "extern crate self as r#dep;", vec![]),
        ] {
            let edges = resolve(Some(edition), root).unwrap();
            assert_eq!(edges.len(), 2);
            for collected in edges {
                assert_eq!(collected.edge.origin(), UsePathOrigin::Anchored);
                let module = match collected.edge {
                    RustPubUseEdge::Named { target_module, .. }
                    | RustPubUseEdge::Wildcard { target_module, .. } => target_module,
                };
                assert_eq!(module, target, "{root}");
            }
        }
    }

    impl ModuleVisibilityCollector<'_, '_> {
        /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。辿る範囲 (module のスコープ
        /// だけ) と条件なしの記録は、反復版と同じ変更を入れてある。
        fn collect_recursive(
            &mut self,
            node: tree_sitter::Node<'_>,
            source_bytes: &[u8],
            current_segments: &[String],
            base_dir: &std::path::Path,
        ) -> Option<()> {
            match node.kind() {
                "mod_item" => {
                    if rust_mod_item_has_path_attribute(node, source_bytes) {
                        return None;
                    }
                    let name = node
                        .child_by_field_name("name")
                        .and_then(|n| n.utf8_text(source_bytes).ok())
                        .map(str::to_string)?;
                    let is_pub = rust_node_has_unrestricted_pub_visibility(node, source_bytes);
                    let mut child_segments = current_segments.to_vec();
                    child_segments.push(name.clone());
                    if is_pub {
                        self.result
                            .entry(current_segments.to_vec())
                            .or_default()
                            .push(name.clone());
                    }
                    self.result.entry(child_segments.clone()).or_default();
                    record_unconditional(
                        self.unconditional,
                        &child_segments,
                        mod_declaration_is_unconditional(node, source_bytes),
                    );
                    let mut has_inline_body = false;
                    let mut cursor = node.walk();
                    for child in node.children(&mut cursor) {
                        if child.kind() == "declaration_list" {
                            has_inline_body = true;
                            let inline_base = base_dir.join(&name);
                            let mut inner_cursor = child.walk();
                            for inner in child.named_children(&mut inner_cursor) {
                                self.collect_recursive(
                                    inner,
                                    source_bytes,
                                    &child_segments,
                                    &inline_base,
                                )?;
                            }
                        }
                    }
                    if !has_inline_body {
                        let as_mod = base_dir.join(&name).join("mod.rs");
                        let as_file = base_dir.join(format!("{name}.rs"));
                        if read_rust_module_source(
                            self.source,
                            self.dir,
                            &self.info.crate_root_rel,
                            &as_mod,
                        )
                        .is_some()
                        {
                            self.frontier.push((child_segments, as_mod));
                        } else if read_rust_module_source(
                            self.source,
                            self.dir,
                            &self.info.crate_root_rel,
                            &as_file,
                        )
                        .is_some()
                        {
                            self.frontier.push((child_segments, as_file));
                        } else {
                            record_unconditional(self.unconditional, &child_segments, false);
                        }
                    }
                }
                "source_file" => {
                    let mut cursor = node.walk();
                    for child in node.named_children(&mut cursor) {
                        self.collect_recursive(child, source_bytes, current_segments, base_dir)?;
                    }
                }
                _ => {}
            }
            Some(())
        }
    }

    type Collected = (
        Option<()>,
        Vec<(Vec<String>, Vec<String>)>,
        Vec<(Vec<String>, std::path::PathBuf)>,
        Vec<(Vec<String>, bool)>,
    );

    fn run_collector(dir: &str, source: &str, recursive: bool) -> Collected {
        let info = RustPrivateModuleInfo {
            crate_root_rel: std::path::PathBuf::new(),
            src_root_rel: std::path::PathBuf::from("src"),
            module_segments: Vec::new(),
        };
        let tree = parser::parse_source(source.as_bytes(), crate::language::LangId::Rust).unwrap();
        let mut result = std::collections::HashMap::new();
        let mut unconditional = std::collections::HashMap::new();
        let mut frontier = Vec::new();
        let mut collector = ModuleVisibilityCollector {
            source: RustSourceTree::Worktree,
            dir,
            info: &info,
            result: &mut result,
            unconditional: &mut unconditional,
            frontier: &mut frontier,
        };
        let base = std::path::Path::new("");
        let outcome = if recursive {
            collector.collect_recursive(tree.root_node(), source.as_bytes(), &[], base)
        } else {
            collector.collect(tree.root_node(), source.as_bytes(), &[], base)
        };
        // HashMap の反復順は実行ごとに変わるので、キーで並べて比べる (値の Vec の順は保つ)。
        let mut result: Vec<_> = result.into_iter().collect();
        result.sort();
        let mut unconditional: Vec<_> = unconditional.into_iter().collect();
        unconditional.sort();
        (outcome, result, frontier, unconditional)
    }

    /// 明示スタック版が、浅い入力で再帰版と同じ module 集合・pub な子の並び・
    /// 未解決の子ファイルの積み順を返す。`#[path]` で判定を諦める場合も一致する。
    #[test]
    fn module_visibility_collector_matches_recursive_version() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for file in [
            "src/a.rs",
            "src/b/mod.rs",
            "src/inline/deeper/leaf.rs",
            "src/inline/file_child.rs",
        ] {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let dir = root.to_str().unwrap();
        let lib = "pub mod a;\nmod b;\npub mod inline {\n    pub mod deeper { pub mod leaf; mod hidden {} }\n\
                   mod private_inline { pub mod x {} pub mod y {} }\n    pub mod file_child;\n}\n\
                   mod missing;\nfn f() { mod in_fn { pub mod nested {} } }\npub(crate) mod restricted {}\n\
                   pub mod z {}\n#[cfg(test)]\nmod conditional {}\nmod gated {\n    #![cfg(any())]\n}\n";
        let iterative = run_collector(dir, lib, false);
        assert_eq!(iterative, run_collector(dir, lib, true));
        assert_eq!(iterative.0, Some(()));
        assert!(
            iterative.2.len() >= 4,
            "子ファイルの解決も比べる: {:?}",
            iterative.2
        );
        // 関数の本体の中の `mod` は外の module tree に入れない
        assert!(
            iterative
                .1
                .iter()
                .all(|(module, _)| module.first().map(String::as_str) != Some("in_fn")),
            "{:?}",
            iterative.1
        );
        // 条件なしの記録: 宣言の cfg・inline の body の内部属性・見つからないファイルは false
        let unconditional = |name: &str| {
            iterative
                .3
                .iter()
                .find(|(module, _)| module == &[name.to_string()])
                .map(|(_, known)| *known)
        };
        assert_eq!(unconditional("z"), Some(true));
        assert_eq!(unconditional("conditional"), Some(false));
        assert_eq!(unconditional("gated"), Some(false));
        assert_eq!(unconditional("missing"), Some(false));
        assert_eq!(unconditional("in_fn"), None);

        let with_path = "pub mod a;\npub mod inline {\n    #[path = \"x.rs\"]\n    mod p;\n}\n";
        let iterative = run_collector(dir, with_path, false);
        assert_eq!(iterative.0, None);
        assert_eq!(iterative.0, run_collector(dir, with_path, true).0);
    }

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。
    fn walk_for_inline_mod_containing_recursive(
        node: tree_sitter::Node<'_>,
        source: &[u8],
        symbol_name: &str,
        inside_inline_mod: bool,
    ) -> bool {
        let kind = node.kind();
        if inside_inline_mod
            && matches!(
                kind,
                "function_item"
                    | "struct_item"
                    | "enum_item"
                    | "type_item"
                    | "const_item"
                    | "static_item"
                    | "trait_item"
                    | "mod_item"
                    | "union_item"
            )
            && let Some(name_node) = node.child_by_field_name("name")
            && name_node.utf8_text(source).map(str::trim) == Ok(symbol_name)
        {
            return true;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            let next_inside = if kind == "mod_item" && child.kind() == "declaration_list" {
                true
            } else {
                inside_inline_mod
            };
            if walk_for_inline_mod_containing_recursive(child, source, symbol_name, next_inside) {
                return true;
            }
        }
        false
    }

    /// 明示スタック版が、浅い入力のすべての部分木・名前で再帰版と同じ判定を返す。
    #[test]
    fn walk_for_inline_mod_containing_matches_recursive_version() {
        let src = "pub fn top() {}\nmod outer {\n    pub fn inner() {}\n    mod nested { struct Deep; }\n}\n\
                   mod file_only;\nimpl X { fn method() {} }\nconst C: u8 = 0;\n";
        let tree = parser::parse_source(src.as_bytes(), crate::language::LangId::Rust).unwrap();
        let names = [
            "top",
            "inner",
            "Deep",
            "nested",
            "outer",
            "file_only",
            "method",
            "C",
        ];
        let mut found = 0;
        let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
            tree.root_node(),
            crate::engine::tree_walk::Children::All,
            |node, _| {
                for name in names {
                    for inside in [false, true] {
                        let expected = walk_for_inline_mod_containing_recursive(
                            node,
                            src.as_bytes(),
                            name,
                            inside,
                        );
                        assert_eq!(
                            walk_for_inline_mod_containing(node, src.as_bytes(), name, inside),
                            expected,
                            "{name} inside={inside}: {}",
                            node.to_sexp()
                        );
                        found += usize::from(expected);
                    }
                }
                std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
            },
        );
        assert!(found > 0);
        assert!(walk_for_inline_mod_containing(
            tree.root_node(),
            src.as_bytes(),
            "Deep",
            false
        ));
        assert!(!walk_for_inline_mod_containing(
            tree.root_node(),
            src.as_bytes(),
            "top",
            false
        ));
    }

    /// 修正前の再帰と同じ形の参照実装 (浅い入力でだけ使う)。Bare な edge の束縛は、反復版と
    /// 同じ `bare_leading_binding` で付ける (比べたいのは走査の順序と所属モジュール)。辿る範囲
    /// (module のスコープだけ) と `#[path]` の扱いは反復版と同じ変更を入れてある。
    fn collect_pub_use_edges_recursive(
        node: tree_sitter::Node<'_>,
        source: &[u8],
        current_module: &[String],
        scopes: &mut ScopeBindingsCache,
        edges: &mut Vec<CollectedPubUseEdge>,
    ) -> Option<()> {
        if node.kind() == "source_file"
            && crate::engine::tree_walk::any_node(
                node,
                crate::engine::tree_walk::Children::All,
                |node| node.kind() == "mod_item" && rust_mod_item_has_path_attribute(node, source),
            )
        {
            return None;
        }
        match node.kind() {
            "use_declaration" => {
                if !rust_node_has_unrestricted_pub_visibility(node, source) {
                    return Some(());
                }
                let argument = node.child_by_field_name("argument")?;
                let mut expanded = Vec::new();
                expand_rust_use_tree_edges_ast(
                    argument,
                    source,
                    &[],
                    current_module,
                    current_module,
                    None,
                    &mut expanded,
                )?;
                for edge in expanded {
                    let bare_leading =
                        bare_leading_binding(node, source, current_module, &edge, scopes);
                    edges.push(CollectedPubUseEdge {
                        edge,
                        bare_leading,
                        unconditional: item_attributes_are_plain(node, source),
                    });
                }
            }
            "mod_item" => {
                let Some(body) = node.child_by_field_name("body") else {
                    return Some(());
                };
                let name = node
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .map(str::to_string);
                let mut next_module = current_module.to_vec();
                if let Some(seg) = name {
                    next_module.push(seg);
                }
                collect_pub_use_edges_recursive(body, source, &next_module, scopes, edges)?;
            }
            "source_file" | "declaration_list" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    collect_pub_use_edges_recursive(child, source, current_module, scopes, edges)?;
                }
            }
            _ => {}
        }
        Some(())
    }

    /// 明示スタック版が、浅い入力で再帰版と同じ edge を同じ順に集め、判定不能
    /// (`#[path]` / 解決できない `super::`) も同じく None を返す。関数の本体などのブロックの中の
    /// `pub use` は module の edge にしない (`#[path]` の mod はブロックの中でも判定不能)。
    #[test]
    fn collect_pub_use_edges_matches_recursive_version() {
        let cases = [
            "pub use a::b;\npub use crate::c::{d, e as f};\nuse private::g;\n\
             pub mod m {\n    pub use super::h::*;\n    mod n { pub use self::i as j; pub(crate) use k::l; }\n}\n\
             pub use m::n;\nfn body() { pub use inner::x; }\n",
            "pub mod m {\n    #[path = \"x.rs\"]\n    mod p;\n    pub use q::r;\n}\n",
            "pub use super::super::too_far;\n",
            // Bare な edge の束縛 (子 mod・extern crate 名・同名の import・glob・ブロックの use)
            "mod value;\npub use value::{A, b::C};\npub use serde::Ser;\nuse other::alias;\n\
             pub use alias::D;\npub mod g { use x::*; pub use y::E; mod z {} pub use z::F; }\n\
             fn f() { pub use value::G; mod in_fn { pub use self::H; } }\n",
            "fn f() {\n    #[path = \"x.rs\"]\n    mod p;\n}\n",
        ];
        for src in cases {
            let tree = parser::parse_source(src.as_bytes(), crate::language::LangId::Rust).unwrap();
            for module in [Vec::new(), vec!["base".to_string()]] {
                let mut edges = Vec::new();
                let mut edges_recursive = Vec::new();
                let result =
                    collect_pub_use_edges(tree.root_node(), src.as_bytes(), &module, &mut edges);
                assert_eq!(
                    result,
                    collect_pub_use_edges_recursive(
                        tree.root_node(),
                        src.as_bytes(),
                        &module,
                        &mut ScopeBindingsCache::default(),
                        &mut edges_recursive
                    ),
                    "{src}"
                );
                if result.is_some() {
                    assert_eq!(
                        format!("{edges:?}"),
                        format!("{edges_recursive:?}"),
                        "{src}"
                    );
                }
            }
        }
        let tree =
            parser::parse_source(cases[0].as_bytes(), crate::language::LangId::Rust).unwrap();
        let mut edges = Vec::new();
        assert!(
            collect_pub_use_edges(tree.root_node(), cases[0].as_bytes(), &[], &mut edges).is_some()
        );
        assert!(edges.len() >= 5, "{edges:?}");
        // ブロックの中の `pub use` (関数の本体の `inner::x`) は edge にしない
        assert!(
            edges.iter().all(|collected| !matches!(
                &collected.edge,
                RustPubUseEdge::Named { exported_name, .. } if exported_name == "x"
            )),
            "{edges:?}"
        );
        // `#[path]` の mod はブロックの中でも判定不能
        let tree =
            parser::parse_source(cases[4].as_bytes(), crate::language::LangId::Rust).unwrap();
        assert!(
            collect_pub_use_edges(tree.root_node(), cases[4].as_bytes(), &[], &mut Vec::new())
                .is_none()
        );

        // 束縛の分類そのもの: 非ルートの module に置いた入力で、先頭の名前ごとに確かめる。
        let src = cases[3];
        let tree = parser::parse_source(src.as_bytes(), crate::language::LangId::Rust).unwrap();
        let mut edges = Vec::new();
        collect_pub_use_edges(
            tree.root_node(),
            src.as_bytes(),
            &["reader".to_string()],
            &mut edges,
        )
        .unwrap();
        let got: Vec<(String, Option<BareLeadingBinding>)> = edges
            .iter()
            .map(|collected| {
                let RustPubUseEdge::Named { exported_name, .. } = &collected.edge else {
                    panic!("Named のみ: {collected:?}");
                };
                (exported_name.clone(), collected.bare_leading)
            })
            .collect();
        use BareLeadingBinding::{ChildModule, Unbound, Unresolved};
        assert_eq!(
            got,
            [
                ("A", Some(ChildModule)),
                ("C", Some(ChildModule)),
                ("Ser", Some(Unbound)),
                ("D", Some(Unresolved)),
                // glob のある inline mod では子 mod 以外は決められない
                ("E", Some(Unresolved)),
                ("F", Some(ChildModule)),
                // 関数の本体の `pub use value::G;` と、その中の mod の `pub use` は edge にしない
            ]
            .map(|(name, binding)| (name.to_string(), binding))
            .to_vec(),
        );
    }
}
