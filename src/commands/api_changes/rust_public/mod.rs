use crate::engine::parser;
use crate::engine::symbols::rust_node_has_unrestricted_pub_visibility;

use super::super::git_input::{git_show_blob, validate_git_revision};
use super::{bare_name, find_mod_decl_visibility, module_path_segments};

mod reexport_graph;
mod source_tree;
mod use_tree;
pub(crate) use reexport_graph::*;
pub(crate) use source_tree::*;
pub(crate) use use_tree::*;

/// `file_path` が属する Rust crate が binary-only (`src/lib.rs` を持たず外部から
/// `pub` シンボルへ到達できない構成) かを判定する。binary-only crate では `pub` は
/// クレート内モジュール境界の役割しか持たないため api.add の対象から除外する。
///
/// 判定方針: `file_path` (dir 相対) から祖先方向に遡って最も近い `Cargo.toml` を
/// 見つけ、そのディレクトリで `src/lib.rs` が存在せず、かつ `Cargo.toml` に `[lib]`
/// セクションも書かれていなければ binary-only とみなす。`[lib] path = "..."` のような
/// custom path で lib crate を構成しているケースを誤って binary-only と判定しないよう、
/// TOML の `[lib]` セクション存在も判定に含める。`Cargo.toml` のパースに失敗した場合は
/// 保守的に false (binary-only ではない) を返す。Rust ファイル以外や `Cargo.toml` が
/// 見つからない場合も false を返す。
pub(crate) fn is_binary_only_rust_crate(dir: &str, file_path: &str) -> bool {
    let path = std::path::Path::new(file_path);
    if path.extension().and_then(|s| s.to_str()) != Some("rs") {
        return false;
    }
    let full = std::path::Path::new(dir).join(file_path);
    let dir_canonical = std::fs::canonicalize(dir).ok();
    let mut current = full.parent();
    while let Some(d) = current {
        let cargo_toml = d.join("Cargo.toml");
        if cargo_toml.is_file() {
            if d.join("src").join("lib.rs").is_file() {
                return false;
            }
            // Cargo.toml に `[lib]` セクションがあれば custom path の lib crate。
            // パース失敗時は保守的に lib crate 扱い (false = binary-only ではない)。
            let Ok(text) = std::fs::read_to_string(&cargo_toml) else {
                return false;
            };
            return !cargo_toml_text_declares_lib(&text);
        }
        // dir より上には探索しない
        if let (Some(root), Ok(canon)) = (dir_canonical.as_ref(), std::fs::canonicalize(d))
            && canon == *root
        {
            return false;
        }
        current = d.parent();
    }
    false
}

/// `api.rm` 側専用: `base` リビジョン時点での crate type を判定する。
///
/// 新ツリーで `src/lib.rs` を削除した、または `Cargo.toml` の `[lib]` セクションを
/// 同一 diff で消したケースで、旧公開 API の削除まで誤って `api.rm` から除外しないため、
/// `git show` で旧側の `Cargo.toml` / `src/lib.rs` を取得して判定する。
///
/// 判定方針:
/// - `file_path` (dir 相対) の祖先方向に向けて、`base` リビジョンに存在する最も近い
///   `Cargo.toml` を探す
/// - その `Cargo.toml` ディレクトリで base 側に `src/lib.rs` があれば library crate
/// - `Cargo.toml` を TOML パースし `[lib]` セクションがあれば library crate
/// - いずれの判定にも失敗 / 該当しない場合 = binary-only
///
/// 失敗時は保守的に `false` (library crate 扱い) を返し、`api.rm` を抑制しない方向に倒す。
pub(crate) fn is_binary_only_rust_crate_at_base(dir: &str, base: &str, file_path: &str) -> bool {
    let path = std::path::Path::new(file_path);
    if path.extension().and_then(|s| s.to_str()) != Some("rs") {
        return false;
    }
    // 祖先から導出する `<dir>/Cargo.toml` も同じ検証対象になるため、起点の file_path を
    // ここで弾いておく (先頭 `-` の dir を含む場合は導出パスも必ず不正になる)。
    if validate_git_revision(base, "--base").is_err()
        || validate_git_revision(file_path, "diff file path").is_err()
    {
        return false;
    }
    // dir 相対パスの祖先を順に辿り、最初に base 時点で存在した Cargo.toml を採用する。
    let mut ancestor: Option<&std::path::Path> = path.parent();
    while let Some(rel_dir) = ancestor {
        let cargo_rel = rel_dir.join("Cargo.toml");
        if let Some(cargo_src) = git_show_blob(dir, base, &cargo_rel.to_string_lossy()) {
            // 同 crate root の base 側 src/lib.rs 存在を git show で判定
            let lib_rel = rel_dir.join("src/lib.rs");
            if git_show_blob(dir, base, &lib_rel.to_string_lossy()).is_some() {
                return false;
            }
            let Ok(text) = std::str::from_utf8(&cargo_src) else {
                return false;
            };
            return !cargo_toml_text_declares_lib(text);
        }
        // ancestor を一つ上に。リポジトリルート (空パス) もループ本体が
        // `Cargo.toml` / `src/lib.rs` として扱えるため特別扱いせず、
        // `Path::new("").parent() == None` で自然に終端する。
        ancestor = rel_dir.parent();
    }
    false
}

/// `api.rm` 判定用: 旧 (base) 側で削除されたシンボル `symbol_name` が「外部公開 API 面の外」に
/// あるかを返す。bin-only crate の `pub`、または crate-private module (`mod foo`、`pub mod` 経路で
/// 到達不能) 配下の `pub` は crate 外から構造的に到達できないため、削除されても破壊的変更ではない。
///
/// ただし private module 配下でも、別の public-reachable module から `pub use` で re-export 公開
/// されている (`pub mod prelude;` + prelude.rs に `pub use crate::wifi::found;` 等) 場合は外部公開
/// API 面に含まれるため抑制しない。`reexport_cache` で base+crate 単位の re-export index を一度だけ
/// 構築する。`api.add` (new 側) / `api.mod` (old/new 両側) の private module 抑制と対称に base 側で判定する。
pub(crate) fn is_rust_old_symbol_outside_public_api_surface(
    dir: &str,
    base: &str,
    old_path: &str,
    symbol_name: &str,
    context: &mut RustPublicApiContext,
) -> bool {
    if context.is_binary_only_at_base(dir, base, old_path) {
        return true;
    }
    // symbol が inline `mod_item` 内 (`mod foo { pub fn symbol() }` 形式) で定義されている
    // 場合、ファイルパス由来の module_segments とずれて edge graph seed が誤合致する。
    // 範囲限定 fail-closed: false negative を防ぐため `api.rm` 抑制を諦め symbol を残す
    // (Issue 2026-06-05-rust-api-add-private-module-reexport-edge-graph の codex 指摘)。
    // inline_mod は symbol 依存のためメモ化対象外 (今回見送り)。
    if rust_symbol_is_inside_inline_mod(
        RustSourceTree::Base { rev: base },
        dir,
        old_path,
        symbol_name,
    ) {
        return false;
    }
    // re-export を考慮しない raw private 判定。public-reachable / 判定不能なら api.rm を残す。
    // old_path 単位でメモ化済み (symbol 非依存)。
    let Some(private) = context.private_module_info_at_base(dir, base, old_path) else {
        return false;
    };
    // index 構築に失敗したら api.rm を残す (false negative 回避優先)。
    let Some(index) = context.index_for(RustSourceTree::Base { rev: base }, dir, &private) else {
        return false;
    };
    !index.exposes_symbol(&private, symbol_name)
}

/// base 側 crate の private module 情報 (re-export は考慮しない raw 判定の結果)。
#[derive(Clone)]
pub(crate) struct RustPrivateModuleInfo {
    crate_root_rel: std::path::PathBuf,
    src_root_rel: std::path::PathBuf,
    /// `file_path` の src 相対モジュールパス (例: `[wifi]` / `[wifi, detector]`)。
    module_segments: Vec<String>,
}

/// base 側で `file_path` (dir 相対) の private module 情報を構築する。re-export は考慮しない
/// (index 側で扱う)。public-reachable (全 `pub mod`) なら `None`、判定不能 (`#[path]` / inline mod /
/// 宣言未検出 / モジュールファイル解決不能) も `None` を返し、呼び出し側で api.rm を残す方向に倒す。
/// `file_path` (dir 相対) の Rust source が属する private module の情報を返す。
/// `RustSourceTree::Base { rev }` なら base リビジョン、`RustSourceTree::Worktree` なら working
/// tree のソースを読む (リファクタ Step 3: `_at_base` / `_at_worktree` の本体統合)。
///
/// lib.rs から mod 宣言チェーンを辿り、最初に private (`mod` 修飾なし) だった prefix を含む
/// `RustPrivateModuleInfo` を返す。`#[path]` 属性 / inline mod / 宣言未検出は `None` を返して
/// 上流で fail-closed する。全 `pub mod` で到達可能なら `None` (public-reachable)。
pub(crate) fn rust_private_module_info(
    source: RustSourceTree<'_>,
    dir: &str,
    file_path: &str,
) -> Option<RustPrivateModuleInfo> {
    use std::path::{Path, PathBuf};
    let rel = Path::new(file_path);
    if rel.extension().and_then(|s| s.to_str()) != Some("rs") {
        return None;
    }
    let canonical_dir = std::fs::canonicalize(dir).ok()?;
    let abs = canonical_dir.join(rel);
    let mut crate_root: Option<PathBuf> = None;
    let mut anc = abs.parent();
    while let Some(d) = anc {
        if d.join("Cargo.toml").is_file() {
            crate_root = Some(d.to_path_buf());
            break;
        }
        if d == canonical_dir {
            break;
        }
        anc = d.parent();
    }
    let crate_root = crate_root?;
    let src_dir = crate_root.join("src");
    if !src_dir.join("lib.rs").is_file() {
        return None;
    }
    let rel_to_src = abs.strip_prefix(&src_dir).ok()?;
    let segments = module_path_segments(rel_to_src);
    if segments.is_empty() {
        return None;
    }
    let crate_root_rel = crate_root.strip_prefix(&canonical_dir).ok()?.to_path_buf();
    let src_root_rel = crate_root_rel.join("src");
    let mut current_rel = PathBuf::from("lib.rs");
    for (idx, seg) in segments.iter().enumerate() {
        let module_source = read_rust_module_source(source, dir, &crate_root_rel, &current_rel)?;
        let tree = parser::parse_source(&module_source, crate::language::LangId::Rust).ok()?;
        match find_mod_decl_visibility(tree.root_node(), &module_source, seg) {
            Some(true) => {
                let parent = current_rel
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                let as_mod = parent.join(seg).join("mod.rs");
                let as_file = parent.join(format!("{seg}.rs"));
                if src_dir.join(&as_mod).is_file() {
                    current_rel = as_mod;
                } else if src_dir.join(&as_file).is_file() {
                    current_rel = as_file;
                } else {
                    return None;
                }
            }
            Some(false) => {
                let _ = idx;
                return Some(RustPrivateModuleInfo {
                    crate_root_rel,
                    src_root_rel,
                    module_segments: segments,
                });
            }
            None => return None,
        }
    }
    None
}

/// `api.add` 判定用: 新 (working tree) 側で新規追加されたシンボル `symbol_name` が
/// 「外部公開 API 面の外」にあるかを返す。bin-only crate / crate-private module (`mod foo`、
/// `pub mod` 経路で到達不能) 配下の `pub` は外部到達できないため、追加されても外部 API 面で
/// はない。ただし private module でも別の public-reachable module から `pub use` で re-export
/// 公開されている場合は外部 API 面に含めるため、edge graph + 固定点伝播で判定する。
///
/// `api.rm` 側 (`is_rust_old_symbol_outside_public_api_surface`) と対称の処理を、base 側でなく
/// working tree 側に行う。`reexport_cache` は new 側 crate 単位で再利用する。
pub(crate) fn is_rust_new_symbol_outside_public_api_surface(
    dir: &str,
    new_path: &str,
    symbol_name: &str,
    context: &mut RustPublicApiContext,
) -> bool {
    if is_binary_only_rust_crate(dir, new_path) {
        return true;
    }
    // symbol が inline `mod_item` 内で定義されている場合、ファイルパス由来の module_segments
    // とずれて edge graph seed が誤合致するため、fail-closed で `api.add` 抑制を諦める。
    if rust_symbol_is_inside_inline_mod(RustSourceTree::Worktree, dir, new_path, symbol_name) {
        return false;
    }
    // raw private 判定 (re-export 考慮なし)。public-reachable / 判定不能なら api.add を残す。
    let Some(private) = rust_private_module_info(RustSourceTree::Worktree, dir, new_path) else {
        return false;
    };
    let Some(index) = context.index_for(RustSourceTree::Worktree, dir, &private) else {
        return false; // index 構築失敗 → fail-closed (api.add を残す)
    };
    !index.exposes_symbol(&private, symbol_name)
}

/// ファイル AST を walk して `symbol_name` の定義が inline `mod_item` (`mod foo { ... }`)
/// の中にあるかを判定する。working tree 側。`mod_item` を見つけたら、その body 内に
/// 同名 identifier の定義 (function_item / struct_item / enum_item / type_alias 等の
/// name field) があるかを確認する。複数経路に同名がある場合は保守的に true (=fail-closed
/// 側に倒し抑制しない方向)。検出失敗・parse 失敗・ファイル読み込み失敗時は false。
/// ファイルソース (`source` 経由) を Rust として parse し、`symbol_name` の定義が inline
/// `mod_item` body 内にあるかを判定する (リファクタ Step 3: `_at_base` / `_at_worktree` の
/// 本体統合)。読み込み / parse 失敗時は false (= 抑制しない / shadow なし扱い)。
pub(crate) fn rust_symbol_is_inside_inline_mod(
    source: RustSourceTree<'_>,
    dir: &str,
    file_path: &str,
    symbol_name: &str,
) -> bool {
    let source_bytes = match source {
        RustSourceTree::Worktree => {
            let Ok(canonical_dir) = std::fs::canonicalize(dir) else {
                return false;
            };
            let full = canonical_dir.join(file_path);
            match std::fs::read(&full) {
                Ok(s) => s,
                Err(_) => return false,
            }
        }
        RustSourceTree::Base { rev } => match git_show_blob(dir, rev, file_path) {
            Some(blob) => blob,
            None => return false,
        },
    };
    rust_source_has_symbol_in_inline_mod(&source_bytes, symbol_name)
}

/// 共通ロジック: source を Rust として parse し、inline `mod_item` の body 内に
/// `symbol_name` の定義 (name field が一致する `function_item` / `struct_item` /
/// `enum_item` / `type_item` / `const_item` / `static_item` / `trait_item` / `mod_item`) が
/// あるか再帰探索する。
pub(crate) fn rust_source_has_symbol_in_inline_mod(source: &[u8], symbol_name: &str) -> bool {
    let bare = bare_name(symbol_name);
    let tree = match parser::parse_source(source, crate::language::LangId::Rust) {
        Ok(t) => t,
        Err(_) => return false,
    };
    walk_for_inline_mod_containing(tree.root_node(), source, bare, false)
}

/// 再帰 walk: `inside_inline_mod=true` のスコープに symbol 定義があれば true。
/// `mod_item` の body (declaration_list) に入ったら `inside_inline_mod=true` で再帰する。
pub(crate) fn walk_for_inline_mod_containing(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    symbol_name: &str,
    inside_inline_mod: bool,
) -> bool {
    let kind = node.kind();
    // 対象シンボル定義かを判定 (name field を持つ各種 item)
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
    // 子 node を再帰 walk。`mod_item` の declaration_list (body) に入ったら
    // inside_inline_mod=true で潜る。`mod foo;` (宣言のみ) は body が無いので追加判定なし。
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        let next_inside = if kind == "mod_item" && child.kind() == "declaration_list" {
            true
        } else {
            inside_inline_mod
        };
        if walk_for_inline_mod_containing(child, source, symbol_name, next_inside) {
            return true;
        }
    }
    false
}

/// working tree 側で `file_path` (dir 相対) の private module 情報を構築する。`rust_private_module_info_at_base`
/// working tree から `<crate_root_rel>/src/<module_rel>` のソースを読み取る。`read_rust_module_source_at_base`
/// の worktree 版。failures は `None` を返し、呼び出し側で `api.add` 抑制を諦める。
/// Rust 公開面解析 1 回分のキャッシュ。
///
/// base/worktree の re-export graph と、base 側の crate/module 判定メモを同じライフタイムへ
/// 閉じる。旧 `RustBaseReexportCache` / `RustWorktreeReexportCache` の薄い wrapper を廃止し、
/// source tree の違いは `RustSourceTree` で明示する。
#[derive(Default)]
pub(crate) struct RustPublicApiContext {
    by_key: std::collections::HashMap<RustSourceTreeKey, Option<RustPubUseIndex>>,
    /// `old_path` → `is_binary_only_rust_crate_at_base` の結果。dir/base は
    /// `detect_api_changes` 呼び出し内で固定なので old_path 単独 key で十分。per-symbol の
    /// 多重 `git show base:Cargo.toml`/`src/lib.rs` を排除する (cache は呼び出し単位で閉じる)。
    binary_crate_memo: std::collections::HashMap<String, bool>,
    /// `old_path` → base 側 `rust_private_module_info` の結果。`None` (public-reachable /
    /// 判定不能) もキャッシュして再 `git show` + 再 parse を防ぐ。
    private_module_memo: std::collections::HashMap<String, Option<RustPrivateModuleInfo>>,
}

impl RustPublicApiContext {
    fn index_for(
        &mut self,
        source: RustSourceTree<'_>,
        dir: &str,
        info: &RustPrivateModuleInfo,
    ) -> Option<&RustPubUseIndex> {
        let key = RustSourceTreeKey::from_source(source, info.crate_root_rel.clone());
        self.by_key
            .entry(key)
            .or_insert_with(|| collect_rust_pub_use_index(source, dir, info))
            .as_ref()
    }

    /// `is_binary_only_rust_crate_at_base` を old_path 単位でメモ化する。
    pub(crate) fn is_binary_only_at_base(
        &mut self,
        dir: &str,
        base: &str,
        file_path: &str,
    ) -> bool {
        if let Some(&cached) = self.binary_crate_memo.get(file_path) {
            return cached;
        }
        let computed = is_binary_only_rust_crate_at_base(dir, base, file_path);
        self.binary_crate_memo
            .insert(file_path.to_string(), computed);
        computed
    }

    /// base 側 `rust_private_module_info` を old_path 単位でメモ化し、結果を clone で返す。
    /// `None` も「計算済み」としてキャッシュする (`entry` で未計算と区別)。
    fn private_module_info_at_base(
        &mut self,
        dir: &str,
        base: &str,
        file_path: &str,
    ) -> Option<RustPrivateModuleInfo> {
        if let Some(cached) = self.private_module_memo.get(file_path) {
            return cached.clone();
        }
        let computed = rust_private_module_info(RustSourceTree::Base { rev: base }, dir, file_path);
        self.private_module_memo
            .insert(file_path.to_string(), computed.clone());
        computed
    }
}

/// Cargo.toml のテキストから `[lib]` セクションが宣言されているかを判定する。
///
/// パース失敗時は **保守的に true (= library 宣言ありとみなす)** を返す。`api.rm` 側で
/// false negative (公開 API 削除の見逃し) を起こさない方向に倒すための既定値。
pub(crate) fn cargo_toml_text_declares_lib(text: &str) -> bool {
    match toml::from_str::<toml::Table>(text) {
        Ok(parsed) => parsed.contains_key("lib"),
        Err(_) => true,
    }
}
