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
}

/// 「ある module でこの名前がエクスポートされている」を表す key。固定点計算の単位。
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RustExportKey {
    module: Vec<String>,
    name: String,
}

/// `pub use` から生成される re-export edge。Named は `source_module::exported_name` が
/// `target_module::target_item` を指す。alias の場合 `exported_name = alias`、`target_item = 元名`。
/// Wildcard は `source_module::* = target_module::*` で名前ごとに伝播する。
#[derive(Clone, Debug)]
pub(crate) enum RustPubUseEdge {
    Named {
        source_module: Vec<String>,
        exported_name: String,
        target_module: Vec<String>,
        target_item: String,
    },
    Wildcard {
        source_module: Vec<String>,
        target_module: Vec<String>,
    },
}

impl RustPubUseIndex {
    /// `info` の private module 配下の `symbol_name` が外部公開 API として到達可能かを返す。
    /// 削除シンボルを seed として live export 集合を固定点伝播し、
    /// live ∩ reachable_modules ≠ ∅ なら true。reachable_modules は `pub mod` 経路に
    /// module 再エクスポート由来の到達 module を加えた集合のため、seed の module 自体が
    /// `pub use self::wifi as api;` で公開されているケースも item 伝播なしで検出できる。
    pub(super) fn exposes_symbol(&self, info: &RustPrivateModuleInfo, symbol_name: &str) -> bool {
        let item = rust_reexport_item_name(symbol_name).to_string();
        let seed = RustExportKey {
            module: info.module_segments.clone(),
            name: item,
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
    let mut edges: Vec<RustPubUseEdge> = Vec::new();
    for file in files {
        let Ok(rel_to_src) = file.strip_prefix(&info.src_root_rel) else {
            continue;
        };
        let module_path = module_path_segments(rel_to_src);
        let file_source = read_rs_blob(source, dir, &file)?;
        let tree = parser::parse_source(&file_source, crate::language::LangId::Rust).ok()?;
        collect_pub_use_edges(tree.root_node(), &file_source, &module_path, &mut edges)?;
    }
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
    let all_modules = collect_all_modules(source, dir, info)?;
    let public_modules = pub_reachable_modules(&all_modules);
    let reachable_modules =
        compute_reexport_reachable_modules(&edges, &public_modules, &all_modules);
    Some(RustPubUseIndex {
        edges,
        reachable_modules,
        named_by_target,
        wildcard_by_target_module,
    })
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
///   占める同名 item (子 module / module の named re-export) が source にあれば到達可能化しない。
///   旧実装は Named 以外を `else { continue; }` で捨てていたため、
///   `mod internal; pub use internal::*;` 構成で **pub 子 module 配下**の公開 API 変更が無音に
///   なっていた (落ちるのは seed の module が wildcard の target と一致しない子 module 配下)。
///
/// どちらの経路でも private child module は親が公開されても外部から辿れないため含めない
/// (単純な prefix 判定だと private 子まで誤って公開扱いになる)。
fn compute_reexport_reachable_modules(
    edges: &[RustPubUseEdge],
    public_modules: &std::collections::HashSet<Vec<String>>,
    all_modules: &std::collections::HashMap<Vec<String>, Vec<String>>,
) -> std::collections::HashSet<Vec<String>> {
    let mut reachable = public_modules.clone();
    // glob shadow 判定用: Named re-export が source namespace へ持ち込む **module 名**。
    //
    // **名前空間を区別する必要がある**。Rust では `mod api` は型名前空間、`fn api` は
    // 値名前空間なので共存でき、`pub use other::thing as api;` (thing は関数) は
    // glob 由来の module `api` を shadow しない。名前だけで shadow 扱いにすると、
    // 実際に公開されている `api::found` の削除を見逃す (false negative)。
    // target が module だと証明できる (= `target_module + target_item` が実在 module) 場合
    // だけ shadow とみなす。
    let named_module_reexports: std::collections::HashSet<(&Vec<String>, &String)> = edges
        .iter()
        .filter_map(|e| match e {
            RustPubUseEdge::Named {
                source_module,
                target_module,
                target_item,
                exported_name,
            } => {
                let mut target_path = target_module.clone();
                target_path.push(target_item.clone());
                all_modules
                    .contains_key(&target_path)
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
                    // (a) 同名の子 module が source にある (private でも namespace を占める)、
                    // (b) 同名を **module の** named re-export で持ち込んでいる。
                    // 関数などの値名前空間の同名 re-export は共存するので shadow しない。
                    //
                    // 検出できるのはこの 2 つの signal まで。型名前空間の非 module item
                    // (`pub struct api`) や `pub use` でない private named import
                    // (`use other::module as api;`) による shadow は、item 単位の情報や
                    // 非公開 import の収集が要るため見ておらず、その場合は従来どおり
                    // (= 修正前と同じ) 到達可能扱いに倒れる。
                    let mut shadow_probe = source_module.clone();
                    shadow_probe.push(child.clone());
                    if all_modules.contains_key(&shadow_probe)
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

/// crate 内の**全 module** (可視性問わず) を walk し、
/// module 絶対 path → 「`pub` と宣言された子 module 名のリスト」を返す。
/// module 再エクスポートの到達性固定点で「target が module か」
/// 「reachable module の pub 子孫」の判定に使う。判定不能 (`#[path]` 等) は
/// `None` (呼出元で index 全体を諦めて api.rm を残す fail-closed)。
pub(crate) fn collect_all_modules(
    source: RustSourceTree<'_>,
    dir: &str,
    info: &RustPrivateModuleInfo,
) -> Option<std::collections::HashMap<Vec<String>, Vec<String>>> {
    use std::collections::HashMap;
    use std::path::PathBuf;
    let mut result: HashMap<Vec<String>, Vec<String>> = HashMap::new();
    result.insert(Vec::new(), Vec::new());
    let mut frontier: Vec<(Vec<String>, PathBuf)> = vec![(Vec::new(), PathBuf::from("lib.rs"))];
    while let Some((segments, current_rel)) = frontier.pop() {
        let module_source =
            read_rust_module_source(source, dir, &info.crate_root_rel, &current_rel)?;
        let tree = parser::parse_source(&module_source, crate::language::LangId::Rust).ok()?;
        let base_dir = child_module_base_dir(&current_rel);
        let mut collector = ModuleVisibilityCollector {
            source,
            dir,
            info,
            result: &mut result,
            frontier: &mut frontier,
        };
        collector.collect(tree.root_node(), &module_source, &segments, &base_dir)?;
    }
    Some(result)
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
    frontier: &'b mut Vec<(Vec<String>, std::path::PathBuf)>,
}

impl ModuleVisibilityCollector<'_, '_> {
    fn collect(
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
                let mut has_inline_body = false;
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    if child.kind() == "declaration_list" {
                        has_inline_body = true;
                        let inline_base = base_dir.join(&name);
                        let mut inner_cursor = child.walk();
                        for inner in child.named_children(&mut inner_cursor) {
                            self.collect(inner, source_bytes, &child_segments, &inline_base)?;
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
                    }
                }
            }
            _ => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.collect(child, source_bytes, current_segments, base_dir)?;
                }
            }
        }
        Some(())
    }
}

/// `src/` 配下の `.rs` ファイル (file は dir 相対) を `source` 経由で読む。
/// Worktree なら `std::fs::read(<canonical_dir>/<file>)`、Base なら `git show <rev>:<file>`。
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
        RustSourceTree::Base { rev } => {
            let file_str = file.to_str()?;
            read_git_blob_at_base(dir, rev, file_str)
        }
    }
}

/// `git show <base>:<file>` で blob を取る (file は repo 相対)。
pub(crate) fn read_git_blob_at_base(dir: &str, base: &str, file: &str) -> Option<Vec<u8>> {
    git_show_blob(dir, base, file)
}
