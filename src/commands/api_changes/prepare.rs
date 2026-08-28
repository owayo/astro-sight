//! diff の前処理。解析対象の絞り込みと、ファイル単位の事実 (exported / callees / export surface) の先取り抽出を行う。

use super::*;

/// API 差分検出から除外すべき diff ファイルかを判定する。
///
/// - 信頼境界外のトラバーサルパスを `dir.join()` で読まないよう、絶対パスや `..` を
///   含むパスは拒否する。
/// - `.gitattributes` の `linguist-generated` 指定ファイルは検出対象外。
/// - ファイル先頭の自動生成マーカーコメントが付くファイルも対象外。
pub(crate) fn should_skip_diff_file(
    df: &crate::models::impact::DiffFile,
    gitattrs: &crate::engine::gitattributes::GitAttributes,
    canonical_dir: Option<&std::path::Path>,
) -> bool {
    if df.new_path != "/dev/null" && !crate::engine::impact::is_safe_diff_path(&df.new_path) {
        return true;
    }
    if df.old_path != "/dev/null" && !crate::engine::impact::is_safe_diff_path(&df.old_path) {
        return true;
    }
    if gitattrs.is_generated(&df.new_path) || gitattrs.is_generated(&df.old_path) {
        return true;
    }
    if let Some(root) = canonical_dir
        && df.new_path != "/dev/null"
    {
        let full = root.join(&df.new_path);
        // symlink 経由で workspace 外を指すファイルを on-disk read しないよう、
        // canonicalize して root 配下にあることを fail-closed で確認する。
        // is_safe_diff_path は文字列レベルの check のみで、`evil.rs ->
        // /etc/passwd` のような symlink を検出できないため、実ファイルを読む直前に
        // canonical 境界判定を入れる (canonicalize 失敗時も skip 側へ倒す)。
        match std::fs::canonicalize(&full) {
            Ok(canonical) if canonical.starts_with(root) => {}
            _ => return true,
        }
        if crate::engine::generated::is_auto_generated(&full) {
            return true;
        }
    }
    false
}

/// `detect_api_changes` の各バケット。`reconcile_with_moves` /
/// `partition_removed_dead_candidates` で最終分類する前の中間状態。
#[derive(Default)]
pub(crate) struct ApiChangeBuckets {
    pub(crate) added: Vec<ApiSymbolCandidate>,
    pub(crate) removed: Vec<ApiSymbolCandidate>,
    pub(crate) modified: Vec<ApiSymbolChange>,
    pub(crate) modified_closed_in_diff: Vec<ApiSymbolChange>,
    pub(crate) const_value_changes: Vec<ApiSymbolChange>,
    pub(crate) compatible_modified: Vec<CompatibleApiModification>,
    pub(crate) all_new_candidates: Vec<ApiSymbolCandidate>,
    pub(crate) property_to_field: Vec<PropertyToFieldChange>,
}

/// diff 全体で不変な入力。ファイル単位の分類関数が共通で参照する。
///
/// 可変状態の `DetectionState` と型で分けることで、「読むだけの入力」と
/// 「書き換える先」を取り違えてもコンパイルが通らないようにする。
pub(crate) struct DetectionInputs<'a> {
    pub(crate) dir: &'a str,
    pub(crate) base: &'a str,
    pub(crate) diff_files: &'a [crate::models::impact::DiffFile],
    pub(crate) diff_new_paths: &'a HashSet<String>,
    pub(crate) ref_index: &'a ApiRefIndex,
}

/// 分類の過程で更新される可変状態 (結果バケットと、ファイル単位のメモ化キャッシュ)。
pub(crate) struct DetectionState<'a> {
    pub(crate) buckets: &'a mut ApiChangeBuckets,
    /// base/worktree を同じライフタイムへ閉じた Rust 公開面解析コンテキスト。
    pub(crate) rust_public: &'a mut RustPublicApiContext,
    pub(crate) closure_caches: &'a mut crate::commands::api_changes::ref_index::ApiClosureCaches,
}

/// 新規ファイル 1 件について Phase 0 で先取り済みの事実。
pub(crate) struct AddedFileFacts<'a> {
    pub(crate) new_syms: &'a [(String, String, String)],
    pub(crate) in_file_callees: &'a HashSet<String>,
}

/// 変更ファイル 1 件について Phase 0 で先取り済みの事実。
pub(crate) struct ModifiedFileFacts<'a> {
    pub(crate) old_syms: &'a [(String, String, String)],
    pub(crate) new_syms: &'a [(String, String, String)],
    pub(crate) in_file_callees: &'a HashSet<String>,
    pub(crate) new_export_surface_names: &'a HashSet<String>,
}

/// シグネチャ変更 1 件を分類するための現場情報。
pub(crate) struct SignatureChangeSite<'a> {
    pub(crate) change: ApiSymbolChange,
    pub(crate) kind: &'a str,
    pub(crate) old_sig: &'a str,
    pub(crate) new_sig: &'a str,
    pub(crate) lang_id: Option<crate::language::LangId>,
    /// 同一ファイル内でしか使われておらず、cross-file 参照が 1 件も無い。
    ///
    /// 通常はこの時点で api.mod から落とすが、Python の TypedDict `total=` 変更候補だけは
    /// 契約変更の 3 値判定に必ず通す必要があるため落とさずここまで運び、
    /// **種別を確定できたときだけ**残して他は従来どおり捨てる
    /// (Issue 2026-08-18-python-typeddict-contract-change-classification)。
    /// 証明できないケースまで残すと、`class X(Base, total=False)` のような無関係な
    /// Python class を新たに blocking にしてしまうため。
    pub(crate) internally_closed: bool,
}

/// Phase 0 で抽出した diff ファイルごとの exported シンボル。
/// 抽出は git show / parse を伴うため 1 回だけ行い、name 収集と process_* で共有する。
pub(crate) enum PreparedDiffFile {
    /// `should_skip_diff_file` で除外されたファイル。
    Skip,
    /// 新規ファイル (`old_path == "/dev/null"`)。
    Added {
        new_syms: Option<Vec<(String, String, String)>>,
        in_file_callees: std::collections::HashSet<String>,
    },
    /// 削除ファイル (`new_path == "/dev/null"`)。cross-file 参照判定を行わないため
    /// 抽出は従来どおり `process_deleted_file` 内で行う。
    Deleted,
    /// 通常の modified ファイル。
    Modified {
        old_syms: Option<Vec<(String, String, String)>>,
        new_syms: Option<Vec<(String, String, String)>>,
        in_file_callees: std::collections::HashSet<String>,
        /// new_path の export clause / `pub use` が公開する名前集合 (TS/JS/Rust)。
        /// Phase 0 の単一 parse で先取りし、process_modified_file が再 read+parse
        /// せず使う (perf #2)。
        new_export_surface_names: std::collections::HashSet<String>,
    },
}

/// modified ファイルで cross-file 参照判定の対象になりうる name を `index_names` へ集める。
/// - 新規追加 (new のみ): `is_used_in_diff_paths` が bare name で検索
/// - 削除 (old のみ): `is_removed_symbol_unreferenced` が bare name で検索
/// - シグネチャ変更: `has_cross_file_refs` が exact name、`is_modified_closed_in_diff` /
///   `has_blocking_value_usage` が bare name で検索
///
/// per-symbol 実装の短絡で検索に到達しえない name は収集しない (過剰収集は AC trie の
/// パターンと事前フィルタのヒット (= parse 対象ファイル) を増やし、小規模 diff で
/// per-symbol より遅くなる):
/// - 新規追加: `is_internally_connected` が true なら `is_used_in_diff_paths` 未到達
/// - シグネチャ変更: exact name (`has_cross_file_refs`) は
///   `is_internally_connected && !has_cross_file_refs` の短絡により internally connected
///   のときのみ評価される
/// - 削除: `(!new_symbols_in_current_file.is_empty() || bash_pure_removal_skip)` が
///   成立しなければ `is_removed_symbol_unreferenced` 未到達 (bash の git show 判定は
///   再現せず `is_bash_old_file` で過剰側に倒す)
///
/// bin-only crate / private module / 同名複数等の残りの篩いは再現せず過剰側に倒す。
/// 過剰収集は検索コストが増えるだけで判定結果には影響しない (逆に収集漏れは
/// `refs_for` が `None` を返して保守側に倒れ、判定が変わりうる)。
pub(crate) fn collect_modified_file_index_names(
    old_syms: &[(String, String, String)],
    new_syms: &[(String, String, String)],
    in_file_callees: &std::collections::HashSet<String>,
    is_bash_old_file: bool,
    index_names: &mut HashSet<String>,
) {
    let old_map: std::collections::HashMap<&str, &str> = old_syms
        .iter()
        .map(|(name, _kind, sig)| (name.as_str(), sig.as_str()))
        .collect();
    let new_names: HashSet<&str> = new_syms.iter().map(|(name, _, _)| name.as_str()).collect();
    let mut has_new_only_symbol = false;
    for (name, _kind, sig) in new_syms {
        match old_map.get(name.as_str()) {
            None => {
                has_new_only_symbol = true;
                if !is_internally_connected(in_file_callees, name) {
                    index_names.insert(bare_name(name).to_string());
                }
            }
            Some(old_sig) if old_sig != &sig.as_str() => {
                // has_cross_file_refs も bare 名で照合するため exact 名 (qualname) の
                // 収集は不要 (identifier ノードに一致せず常に 0 件で AC trie を太らせるだけ)。
                index_names.insert(bare_name(name).to_string());
            }
            Some(_) => {}
        }
    }
    if !(has_new_only_symbol || is_bash_old_file) {
        return;
    }
    for (name, _, _) in old_syms {
        if !new_names.contains(name.as_str()) {
            index_names.insert(bare_name(name).to_string());
        }
    }
}

/// src 相対パスを Rust モジュールセグメント列に変換する。
/// `meeting/macos.rs` → `[meeting, macos]`、`meeting/mod.rs` → `[meeting]`、
/// `lib.rs` / `main.rs` → `[]` (root モジュール)。
pub(crate) fn module_path_segments(rel: &std::path::Path) -> Vec<String> {
    let comps: Vec<_> = rel.components().collect();
    let mut segs: Vec<String> = Vec::new();
    let last = comps.len().saturating_sub(1);
    for (i, c) in comps.iter().enumerate() {
        let name = c.as_os_str().to_string_lossy();
        if i == last {
            let stem = std::path::Path::new(name.as_ref())
                .file_stem()
                .map(|s| s.to_string_lossy().to_string());
            match stem.as_deref() {
                // mod.rs / lib.rs / main.rs はそのディレクトリのモジュール自身を表す
                Some("mod") | Some("lib") | Some("main") => {}
                Some(s) => segs.push(s.to_string()),
                None => {}
            }
        } else {
            segs.push(name.to_string());
        }
    }
    segs
}

/// 親モジュールファイル直下の `mod <mod_name>` 宣言の可視性 (制限なし pub か) を返す。
///
/// source_file 直下の `mod_item` のみを見る。inline mod (`mod foo { mod bar; }`) 内の同名
/// 宣言は別モジュールスコープの宣言なので拾わない (codex 指摘: 再帰探索で別スコープの同名
/// mod を誤って拾うと可視性判定が壊れる)。
pub(crate) fn find_mod_decl_visibility(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    mod_name: &str,
) -> Option<bool> {
    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if child.kind() == "mod_item"
            && child
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
                == Some(mod_name)
        {
            // #[path = "..."] でファイル名と module 名がずれる場合、モジュール解決を諦めて
            // 「判定不能」(None) を返す。下流 (rust_private_module_info_at_base /
            // public_reachable_modules_at_base) は api.rm 抑制を諦め、削除を残す方向に倒す。
            if rust_mod_item_has_path_attribute(child, source) {
                return None;
            }
            let mut mc = child.walk();
            let is_pub = child.children(&mut mc).any(|c| {
                c.kind() == "visibility_modifier" && c.utf8_text(source).map(str::trim) == Ok("pub")
            });
            return Some(is_pub);
        }
    }
    None
}

/// new_path 1 ファイルから API 差分検出に必要な 3 種の facts をまとめて抽出する。
/// 旧 exported / in_file_callees / export surface の各抽出が同一 new_path をそれぞれ
/// read+parse していた (TS/JS/Rust で 3 回、他言語で 2 回) のを **1 回の read+parse に
/// 集約**する (perf #2)。
/// 各抽出のガードは元関数と完全に一致させ behavior-preserving とする:
/// - `exported`: test path は `Some(空)` (parse せず) / 非 test は `is_safe_diff_path` 必須で
///   None と空を区別 / lexer-only は lexer 経由 / それ以外 tree-sitter parse+filter
/// - `callees`: test/safe ガードなし、read+parse 失敗時は空 (lexer-only は parse_source が Err→空)
/// - `export_surface_names`: `is_safe_diff_path` かつ TS/TSX/JS/Rust のみ、それ以外は空。
///   TS/JS は named export clause (from 句の有無を問わず)、Rust は `pub use` が公開する名前
pub(crate) struct NewFileFacts {
    pub(crate) exported: Option<Vec<(String, String, String)>>,
    pub(crate) callees: std::collections::HashSet<String>,
    pub(crate) export_surface_names: std::collections::HashSet<String>,
}

pub(crate) fn extract_new_file_facts(dir: &str, file_path: &str) -> NewFileFacts {
    let mut facts = NewFileFacts {
        exported: None,
        callees: std::collections::HashSet::new(),
        export_surface_names: std::collections::HashSet::new(),
    };
    // exported の test path 短絡: parse せず Some(空) を返す (extract_exported_symbols_from_file と一致)。
    let is_test = is_test_path(std::path::Path::new(file_path));
    if is_test {
        facts.exported = Some(Vec::new());
    }
    // exported (非 test) / reexports は is_safe_diff_path を要求する。callees は要求しない。
    let safe = crate::engine::impact::is_safe_diff_path(file_path);

    let full_path = std::path::Path::new(dir).join(file_path);
    let Some(utf8) = full_path.to_str() else {
        return facts;
    };
    let utf8_path = camino::Utf8Path::new(utf8);
    let Ok(source) = parser::read_file(utf8_path) else {
        return facts;
    };
    let Ok(lang_id) = parser::detect_lang(utf8_path, &source) else {
        return facts;
    };

    // lexer-only (Xojo): tree-sitter parse は呼ばない (parse_source は Err を返すため callees/
    // reexports は元実装でも空)。exported のみ lexer 経由 (非 test かつ safe のとき)。
    if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
        if !is_test && safe {
            facts.exported = Some(crate::engine::lexer::extract_exported_symbols(
                &source, lexer_lang, false,
            ));
        }
        return facts;
    }

    let Ok(tree) = parser::parse_source(&source, lang_id) else {
        return facts;
    };
    let root = tree.root_node();

    // callees: test/safe ガードなし (extract_in_file_callees と一致)。
    facts.callees =
        crate::engine::calls::extract_all_callees(root, &source, lang_id).unwrap_or_default();

    // exported (非 test かつ safe): extract_symbols → filter_exported_symbols。
    // extract_symbols 失敗は None のまま (元 _inner の `?` と一致)。
    if !is_test
        && safe
        && let Ok(syms) = crate::engine::symbols::extract_symbols(root, &source, lang_id)
    {
        facts.exported = Some(filter_exported_symbols(
            &syms,
            root,
            &source,
            lang_id,
            true,
            false,
            Some(file_path),
        ));
    }

    // export surface: safe かつ TS/TSX/JS/Rust のみ。
    if safe
        && matches!(
            lang_id,
            crate::language::LangId::Typescript
                | crate::language::LangId::Tsx
                | crate::language::LangId::Javascript
                | crate::language::LangId::Rust
        )
    {
        facts.export_surface_names = match lang_id {
            crate::language::LangId::Rust => {
                crate::engine::symbols::collect_rust_reexported_names(root, &source)
            }
            _ => crate::engine::symbols::collect_js_ts_named_export_surface_names(root, &source),
        };
    }

    facts
}
