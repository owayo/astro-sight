use crate::engine::parser;

use super::super::git_input::validate_git_revision;
use super::{bare_name, find_mod_decl_visibility, module_path_segments};

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
    if validate_git_revision(base, "--base").is_err() {
        return false;
    }
    // dir 相対パスの祖先を順に辿り、最初に base 時点で存在した Cargo.toml を採用する。
    let mut ancestor: Option<&std::path::Path> = path.parent();
    while let Some(rel_dir) = ancestor {
        let cargo_rel = if rel_dir.as_os_str().is_empty() {
            std::path::PathBuf::from("Cargo.toml")
        } else {
            rel_dir.join("Cargo.toml")
        };
        let cargo_rel_str = cargo_rel.to_string_lossy().to_string();
        if validate_git_revision(&cargo_rel_str, "diff file path").is_err() {
            return false;
        }
        let cargo_output = std::process::Command::new("git")
            .args(["show", &format!("{base}:{cargo_rel_str}")])
            .current_dir(dir)
            .output();
        if let Ok(out) = cargo_output
            && out.status.success()
        {
            // 同 crate root の base 側 src/lib.rs 存在を git show で判定
            let lib_rel = if rel_dir.as_os_str().is_empty() {
                std::path::PathBuf::from("src/lib.rs")
            } else {
                rel_dir.join("src/lib.rs")
            };
            let lib_rel_str = lib_rel.to_string_lossy().to_string();
            if validate_git_revision(&lib_rel_str, "diff file path").is_err() {
                return false;
            }
            let lib_output = std::process::Command::new("git")
                .args(["show", &format!("{base}:{lib_rel_str}")])
                .current_dir(dir)
                .output();
            if matches!(lib_output, Ok(ref o) if o.status.success()) {
                return false;
            }
            let Ok(text) = std::str::from_utf8(&out.stdout) else {
                return false;
            };
            return !cargo_toml_text_declares_lib(text);
        }
        // ancestor を一つ上に
        match rel_dir.parent() {
            Some(parent) => ancestor = Some(parent),
            None => break,
        }
        if ancestor.is_some_and(|p| p.as_os_str().is_empty()) {
            // ルート直下まで来たので最後にもう一度だけ Cargo.toml チェックする
            let last = std::path::PathBuf::from("Cargo.toml");
            let cargo_rel_str = last.to_string_lossy().to_string();
            let cargo_output = std::process::Command::new("git")
                .args(["show", &format!("{base}:{cargo_rel_str}")])
                .current_dir(dir)
                .output();
            if let Ok(out) = cargo_output
                && out.status.success()
            {
                let lib_output = std::process::Command::new("git")
                    .args(["show", &format!("{base}:src/lib.rs")])
                    .current_dir(dir)
                    .output();
                if matches!(lib_output, Ok(ref o) if o.status.success()) {
                    return false;
                }
                let Ok(text) = std::str::from_utf8(&out.stdout) else {
                    return false;
                };
                return !cargo_toml_text_declares_lib(text);
            }
            break;
        }
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
    reexport_cache: &mut RustBaseReexportCache,
) -> bool {
    if reexport_cache.is_binary_only_at_base(dir, base, old_path) {
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
    let Some(private) = reexport_cache.private_module_info_at_base(dir, base, old_path) else {
        return false;
    };
    // index 構築に失敗したら api.rm を残す (false negative 回避優先)。
    let Some(index) = reexport_cache.index_for(dir, base, &private) else {
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
    reexport_cache: &mut RustWorktreeReexportCache,
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
    let Some(index) = reexport_cache.index_for(dir, &private) else {
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
        RustSourceTree::Base { rev } => {
            if validate_git_revision(rev, "--base").is_err()
                || validate_git_revision(file_path, "diff file path").is_err()
            {
                return false;
            }
            match std::process::Command::new("git")
                .args(["show", &format!("{rev}:{file_path}")])
                .current_dir(dir)
                .output()
            {
                Ok(o) if o.status.success() => o.stdout,
                _ => return false,
            }
        }
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
/// Rust crate のソースツリーをどこから読むかを表す抽象化。
///
/// `Worktree` は `std::fs` 経由で working tree を直接読み、`Base { rev }` は `git show <rev>:<path>` /
/// `git ls-tree <rev>` 経由で base リビジョンを読む。`read_rust_module_source` / `collect_rust_rs_files` /
/// `RustReexportCache` の API に渡して I/O 差分を吸収する (リファクタ Step 1: I/O 抽象化、
/// 別 Issue `2026-06-06-refactor-rust-private-module-helpers-with-source-tree-enum.md` 対応)。
#[derive(Clone, Copy, Debug)]
pub(crate) enum RustSourceTree<'a> {
    Worktree,
    Base { rev: &'a str },
}

/// `crate_root_rel`/src/`module_rel` を `source` 経由で読む。Worktree なら `std::fs::read`、
/// Base なら `git show <rev>:<crate_root_rel>/src/<module_rel>`。失敗時は `None`。
pub(crate) fn read_rust_module_source(
    source: RustSourceTree<'_>,
    dir: &str,
    crate_root_rel: &std::path::Path,
    module_rel: &std::path::Path,
) -> Option<Vec<u8>> {
    match source {
        RustSourceTree::Worktree => {
            let canonical_dir = std::fs::canonicalize(dir).ok()?;
            let full = canonical_dir
                .join(crate_root_rel)
                .join("src")
                .join(module_rel);
            std::fs::read(full).ok()
        }
        RustSourceTree::Base { rev } => {
            let full_rel = crate_root_rel.join("src").join(module_rel);
            let full_rel_str = full_rel.to_str()?;
            if validate_git_revision(rev, "--base").is_err()
                || validate_git_revision(full_rel_str, "diff file path").is_err()
            {
                return None;
            }
            let out = std::process::Command::new("git")
                .args(["show", &format!("{rev}:{full_rel_str}")])
                .current_dir(dir)
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            Some(out.stdout)
        }
    }
}

/// `src_root_rel` 配下の `.rs` ファイル列 (repo 相対) を `source` 経由で取得する。
/// Worktree なら `ignore::WalkBuilder`、Base なら `git ls-tree -r --name-only`。
pub(crate) fn collect_rust_rs_files(
    source: RustSourceTree<'_>,
    dir: &str,
    src_root_rel: &std::path::Path,
) -> Option<Vec<std::path::PathBuf>> {
    match source {
        RustSourceTree::Worktree => {
            use ignore::WalkBuilder;
            let canonical_dir = std::fs::canonicalize(dir).ok()?;
            let src_full = canonical_dir.join(src_root_rel);
            if !src_full.is_dir() {
                return None;
            }
            let mut files = Vec::new();
            for entry in WalkBuilder::new(&src_full).hidden(false).build().flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                    continue;
                }
                let rel = match path.strip_prefix(&canonical_dir) {
                    Ok(r) => r.to_path_buf(),
                    Err(_) => continue,
                };
                files.push(rel);
            }
            Some(files)
        }
        RustSourceTree::Base { rev } => {
            let src_str = src_root_rel.to_str()?;
            if validate_git_revision(rev, "--base").is_err()
                || validate_git_revision(src_str, "diff file path").is_err()
            {
                return None;
            }
            let out = std::process::Command::new("git")
                .args(["ls-tree", "-r", "--name-only", rev, "--", src_str])
                .current_dir(dir)
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let text = std::str::from_utf8(&out.stdout).ok()?;
            Some(
                text.lines()
                    .filter(|l| l.ends_with(".rs"))
                    .map(std::path::PathBuf::from)
                    .collect(),
            )
        }
    }
}

/// 統合 cache キー (リファクタ Step 2: cache 統合)。`rev = None` で working tree、
/// `rev = Some(<rev>)` で base リビジョンを表す。型で意図を明確化することで、
/// `(Option<String>, PathBuf)` の生 tuple よりも事故りにくい。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct RustSourceTreeKey {
    rev: Option<String>,
    crate_root_rel: std::path::PathBuf,
}

impl RustSourceTreeKey {
    fn from_source(source: RustSourceTree<'_>, crate_root_rel: std::path::PathBuf) -> Self {
        let rev = match source {
            RustSourceTree::Worktree => None,
            RustSourceTree::Base { rev } => Some(rev.to_string()),
        };
        Self {
            rev,
            crate_root_rel,
        }
    }
}

/// base / worktree 統合 re-export cache (リファクタ Step 2)。
/// `RustBaseReexportCache` / `RustWorktreeReexportCache` の本体としても使用される
/// (これら 2 cache は外部 API 維持のため薄い wrapper として残り、内部は本 cache に転送する)。
#[derive(Default)]
pub(crate) struct RustReexportCache {
    by_key: std::collections::HashMap<RustSourceTreeKey, Option<RustPubUseIndex>>,
}

impl RustReexportCache {
    fn index_for(
        &mut self,
        source: RustSourceTree<'_>,
        dir: &str,
        info: &RustPrivateModuleInfo,
    ) -> Option<&RustPubUseIndex> {
        let key = RustSourceTreeKey::from_source(source, info.crate_root_rel.clone());
        self.by_key
            .entry(key)
            .or_insert_with(|| match source {
                RustSourceTree::Worktree | RustSourceTree::Base { .. } => {
                    collect_rust_pub_use_index(source, dir, info)
                }
            })
            .as_ref()
    }
}

/// working tree 用 re-export cache。`api.add` 経路から呼ばれる外部 API を維持しつつ、
/// 内部は統合 `RustReexportCache` に転送する (リファクタ Step 2: cache 統合)。
#[derive(Default)]
pub(crate) struct RustWorktreeReexportCache {
    inner: RustReexportCache,
}

impl RustWorktreeReexportCache {
    fn index_for(&mut self, dir: &str, info: &RustPrivateModuleInfo) -> Option<&RustPubUseIndex> {
        self.inner.index_for(RustSourceTree::Worktree, dir, info)
    }
}

/// base+crate 単位で `pub use` re-export index を一度だけ構築するキャッシュ。
/// `api.rm` / `api.mod` 経路から呼ばれる外部 API を維持しつつ、内部は統合 `RustReexportCache`
/// に転送する (リファクタ Step 2: cache 統合)。
#[derive(Default)]
pub(crate) struct RustBaseReexportCache {
    inner: RustReexportCache,
    /// `old_path` → `is_binary_only_rust_crate_at_base` の結果。dir/base は
    /// `detect_api_changes` 呼び出し内で固定なので old_path 単独 key で十分。per-symbol の
    /// 多重 `git show base:Cargo.toml`/`src/lib.rs` を排除する (cache は呼び出し単位で閉じる)。
    binary_crate_memo: std::collections::HashMap<String, bool>,
    /// `old_path` → base 側 `rust_private_module_info` の結果。`None` (public-reachable /
    /// 判定不能) もキャッシュして再 `git show` + 再 parse を防ぐ。
    private_module_memo: std::collections::HashMap<String, Option<RustPrivateModuleInfo>>,
}

impl RustBaseReexportCache {
    fn index_for(
        &mut self,
        dir: &str,
        base: &str,
        info: &RustPrivateModuleInfo,
    ) -> Option<&RustPubUseIndex> {
        self.inner
            .index_for(RustSourceTree::Base { rev: base }, dir, info)
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
    fn exposes_symbol(&self, info: &RustPrivateModuleInfo, symbol_name: &str) -> bool {
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
    let public_modules = public_reachable_modules(source, dir, info)?;
    let all_modules = collect_all_modules(source, dir, info)?;
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
/// 固定点計算する。seed は `pub mod` 経路の `public_modules`。Named edge の source module
/// が到達可能なら、target path が実在する **pub 宣言された** module の場合に限り
/// その module も到達可能になり、pub 子孫 module も連鎖する。private child module は
/// 親が公開されても外部から辿れないため含めない (単純な prefix 判定だと private 子まで
/// 誤って公開扱いになる)。private module 自体の再エクスポート (`mod wifi;` +
/// `pub use self::wifi as api;`) は rustc では E0365 で無効なため、親の pub_children に
/// 含まれない target は reachable 化しない。
fn compute_reexport_reachable_modules(
    edges: &[RustPubUseEdge],
    public_modules: &std::collections::HashSet<Vec<String>>,
    all_modules: &std::collections::HashMap<Vec<String>, Vec<String>>,
) -> std::collections::HashSet<Vec<String>> {
    let mut reachable = public_modules.clone();
    loop {
        let mut changed = false;
        for edge in edges {
            let RustPubUseEdge::Named {
                source_module,
                target_module,
                target_item,
                ..
            } = edge
            else {
                continue;
            };
            if !reachable.contains(source_module) {
                continue;
            }
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
            let mut stack = vec![candidate];
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
        if !changed {
            break;
        }
    }
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
        collect_modules_with_visibility(
            source,
            tree.root_node(),
            &module_source,
            dir,
            info,
            &segments,
            &current_rel,
            &mut result,
            &mut frontier,
        )?;
    }
    Some(result)
}

/// `collect_public_pub_mods` の全 module 版。private mod にも潜って module 集合を作り、
/// pub な子 module 名だけを親エントリに記録する。
#[allow(clippy::too_many_arguments)]
fn collect_modules_with_visibility(
    source: RustSourceTree<'_>,
    node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    dir: &str,
    info: &RustPrivateModuleInfo,
    current_segments: &[String],
    current_file_rel: &std::path::Path,
    result: &mut std::collections::HashMap<Vec<String>, Vec<String>>,
    frontier: &mut Vec<(Vec<String>, std::path::PathBuf)>,
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
            let is_pub = rust_use_declaration_is_pub(node, source_bytes);
            let mut child_segments = current_segments.to_vec();
            child_segments.push(name.clone());
            if is_pub {
                result
                    .entry(current_segments.to_vec())
                    .or_default()
                    .push(name.clone());
            }
            result.entry(child_segments.clone()).or_default();
            let mut has_inline_body = false;
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "declaration_list" {
                    has_inline_body = true;
                    let mut inner_cursor = child.walk();
                    for inner in child.named_children(&mut inner_cursor) {
                        collect_modules_with_visibility(
                            source,
                            inner,
                            source_bytes,
                            dir,
                            info,
                            &child_segments,
                            current_file_rel,
                            result,
                            frontier,
                        )?;
                    }
                }
            }
            if !has_inline_body {
                let base_dir = child_module_base_dir(current_file_rel);
                let as_mod = base_dir.join(&name).join("mod.rs");
                let as_file = base_dir.join(format!("{name}.rs"));
                if read_rust_module_source(source, dir, &info.crate_root_rel, &as_mod).is_some() {
                    frontier.push((child_segments, as_mod));
                } else if read_rust_module_source(source, dir, &info.crate_root_rel, &as_file)
                    .is_some()
                {
                    frontier.push((child_segments, as_file));
                }
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_modules_with_visibility(
                    source,
                    child,
                    source_bytes,
                    dir,
                    info,
                    current_segments,
                    current_file_rel,
                    result,
                    frontier,
                )?;
            }
        }
    }
    Some(())
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

/// `source` 経由で lib.rs (crate root) から `pub mod` 経路 (制限なし pub) で到達できる
/// module 集合を構築する (リファクタ Step 3: `_at_base` / `_at_worktree` の本体統合)。root `[]`
/// は常に含む。inline `pub mod foo { ... }` も再帰的に拾う。`mod foo;` (制限なし pub なし) は
/// 除外する。判定不能 (`#[path]` / モジュールファイル解決失敗 / 解析失敗) は `None` を返し、
/// 呼出元で api.rm を残す (fail-closed)。
pub(crate) fn public_reachable_modules(
    source: RustSourceTree<'_>,
    dir: &str,
    info: &RustPrivateModuleInfo,
) -> Option<std::collections::HashSet<Vec<String>>> {
    use std::collections::HashSet;
    use std::path::PathBuf;
    let mut result: HashSet<Vec<String>> = HashSet::new();
    result.insert(Vec::new());
    let mut frontier: Vec<(Vec<String>, PathBuf)> = vec![(Vec::new(), PathBuf::from("lib.rs"))];
    while let Some((segments, current_rel)) = frontier.pop() {
        let module_source =
            read_rust_module_source(source, dir, &info.crate_root_rel, &current_rel)?;
        let tree = parser::parse_source(&module_source, crate::language::LangId::Rust).ok()?;
        collect_public_pub_mods(
            source,
            tree.root_node(),
            &module_source,
            dir,
            info,
            &segments,
            &current_rel,
            &mut result,
            &mut frontier,
        )?;
    }
    Some(result)
}

/// `lib.rs` / 親モジュールファイルから子の `pub mod` を `source` 経由で再帰的に集める。
/// inline body は同じファイル内で walk 続行、宣言のみは次のモジュールファイルを resolve して
/// frontier に積む (リファクタ Step 3: `_at_base` / `_at_worktree` の本体統合)。
#[allow(clippy::too_many_arguments)]
pub(crate) fn collect_public_pub_mods(
    source: RustSourceTree<'_>,
    node: tree_sitter::Node<'_>,
    source_bytes: &[u8],
    dir: &str,
    info: &RustPrivateModuleInfo,
    current_segments: &[String],
    current_file_rel: &std::path::Path,
    result: &mut std::collections::HashSet<Vec<String>>,
    frontier: &mut Vec<(Vec<String>, std::path::PathBuf)>,
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
            let is_pub = rust_use_declaration_is_pub(node, source_bytes);
            if !is_pub {
                return Some(());
            }
            let mut child_segments = current_segments.to_vec();
            child_segments.push(name.clone());
            result.insert(child_segments.clone());
            let mut has_inline_body = false;
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "declaration_list" {
                    has_inline_body = true;
                    let mut inner_cursor = child.walk();
                    for inner in child.named_children(&mut inner_cursor) {
                        collect_public_pub_mods(
                            source,
                            inner,
                            source_bytes,
                            dir,
                            info,
                            &child_segments,
                            current_file_rel,
                            result,
                            frontier,
                        )?;
                    }
                }
            }
            if !has_inline_body {
                let base_dir = child_module_base_dir(current_file_rel);
                let as_mod = base_dir.join(&name).join("mod.rs");
                let as_file = base_dir.join(format!("{name}.rs"));
                if read_rust_module_source(source, dir, &info.crate_root_rel, &as_mod).is_some() {
                    frontier.push((child_segments, as_mod));
                } else if read_rust_module_source(source, dir, &info.crate_root_rel, &as_file)
                    .is_some()
                {
                    frontier.push((child_segments, as_file));
                }
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                collect_public_pub_mods(
                    source,
                    child,
                    source_bytes,
                    dir,
                    info,
                    current_segments,
                    current_file_rel,
                    result,
                    frontier,
                )?;
            }
        }
    }
    Some(())
}

/// `git show <base>:<file>` で blob を取る (file は repo 相対)。
pub(crate) fn read_git_blob_at_base(dir: &str, base: &str, file: &str) -> Option<Vec<u8>> {
    if validate_git_revision(base, "--base").is_err()
        || validate_git_revision(file, "diff file path").is_err()
    {
        return None;
    }
    let out = std::process::Command::new("git")
        .args(["show", &format!("{base}:{file}")])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
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
    edges: &mut Vec<RustPubUseEdge>,
) -> Option<()> {
    match node.kind() {
        "use_declaration" => {
            if !rust_use_declaration_is_pub(node, source) {
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

/// `use_declaration` / `mod_item` ノードが「制限なし `pub`」 (`pub(crate)` / `pub(super)` 等の
/// 制限付きや非 pub を除く) かを返す。`visibility_modifier` 子ノードのテキストを厳密に `"pub"` で照合する。
pub(crate) fn rust_use_declaration_is_pub(node: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            return child.utf8_text(source).map(str::trim) == Ok("pub");
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

/// `path` ノード (scoped_identifier / identifier / crate / self / super) を解決して
/// `(resolved_prefix, leaf_name)` を返す。anchor (crate/self/super) を current_module で解決し、
/// scoped_identifier は再帰的に path → name を展開する。
///
/// 戻り値:
/// - `Some((prefix, Some(name)))`: scoped_identifier の終端で name 部分を抽出した
/// - `Some((prefix, None))`: anchor 単体 (super::, crate:: 等) のみで終わった
/// - `None`: 判定不能 (root を超える super::、不正な構造)
pub(crate) fn resolve_use_path_node(
    node: tree_sitter::Node<'_>,
    source: &[u8],
    path_prefix: &[String],
    current_module: &[String],
) -> Option<(Vec<String>, Option<String>)> {
    match node.kind() {
        "scoped_identifier" => {
            // tree-sitter-rust grammar は scoped_identifier で path/name の field 名を出さない。
            // named children は最大 2 つ: [path, name] または [name] のみ (path が省略された
            // 場合は crate root レベルの単一 identifier 扱い)。
            let mut named: Vec<tree_sitter::Node<'_>> = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                named.push(child);
            }
            match named.as_slice() {
                [name_node] => {
                    let name_text = name_node.utf8_text(source).ok()?.trim().to_string();
                    Some((path_prefix.to_vec(), Some(name_text)))
                }
                [path_node, name_node] => {
                    let name_text = name_node.utf8_text(source).ok()?.trim().to_string();
                    let (prefix, intermediate_leaf) =
                        resolve_use_path_node(*path_node, source, path_prefix, current_module)?;
                    let mut full_prefix = prefix;
                    if let Some(leaf) = intermediate_leaf {
                        full_prefix.push(leaf);
                    }
                    Some((full_prefix, Some(name_text)))
                }
                _ => None, // 想定外の named children 数
            }
        }
        "identifier" => {
            let text = node.utf8_text(source).ok()?.trim().to_string();
            Some((path_prefix.to_vec(), Some(text)))
        }
        "crate" => {
            // crate root 起点: 現 prefix を捨ててルート (空) 起点にする。
            Some((Vec::new(), None))
        }
        "self" => {
            // 現 module 起点。current_module を prefix に積む (まだ何も積んでいない時のみ)。
            if path_prefix.is_empty() {
                Some((current_module.to_vec(), None))
            } else {
                Some((path_prefix.to_vec(), None))
            }
        }
        "super" => {
            // 現 module から 1 階層上。
            let mut effective = if path_prefix.is_empty() {
                current_module.to_vec()
            } else {
                path_prefix.to_vec()
            };
            effective.pop()?;
            Some((effective, None))
        }
        _ => {
            // 知らない kind は子供を再帰 walk して可能な解決を試みる。
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.is_named()
                    && let Some(result) =
                        resolve_use_path_node(child, source, path_prefix, current_module)
                {
                    return Some(result);
                }
            }
            None
        }
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
