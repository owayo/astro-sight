//! diff から公開 API の追加 / 削除 / シグネチャ変更を検出する。
//!
//! `detect_api_changes` がオーケストレーションを担い、実処理はサブモジュールに分ける:
//! `prepare` (前処理) → `diff_processing` (ファイル単位の分類) → `removed` (削除候補の最終分類)。
//! `exported` / `signature` は公開面判定とシグネチャ抽出を提供する横断モジュール。

use std::collections::HashSet;

use crate::engine::parser;

use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, MovedSymbol,
    PropertyToFieldChange,
};

use crate::models::symbol::{Symbol, SymbolKind};

use crate::service::AppService;

use super::dead_code::{
    collect_python_unittest_classes, enclosing_container, is_phpunit_test_symbol,
    is_python_test_symbol, is_test_path,
};

use super::git_input::git_show_blob;

pub(crate) type ExportedSymbols = Vec<(String, String, String)>;

type ExportedSymbolsWithLang = (crate::language::LangId, ExportedSymbols);

pub(crate) fn detect_api_changes(
    dir: &str,
    base: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> ApiChanges {
    let mut buckets = ApiChangeBuckets::default();
    // api.rm の Rust private module 抑制で base 側 re-export index を base+crate 単位に再利用する。
    let mut rust_reexport_cache = RustBaseReexportCache::default();
    // api.add 経路では new (working tree) 側 crate を 1 度走査して edge graph を構築する。
    let mut rust_new_reexport_cache = RustWorktreeReexportCache::default();

    // .gitattributes の linguist-generated 指定ファイルは API 変更検出から除外する
    let gitattrs = std::fs::canonicalize(dir)
        .map(|d| crate::engine::gitattributes::GitAttributes::load(&d))
        .unwrap_or_default();

    // 同一 diff 内で追加/変更されたファイルパスの集合。新規 pub シンボルが diff 内の
    // 別ファイルから参照されていれば「同一 diff 内で完結して使用されている」と判断し、
    // api.add から除外する（binary crate の pub struct が同 diff 内で use されるケース等）。
    let diff_new_paths: HashSet<String> = diff_files
        .iter()
        .filter(|f| f.new_path != "/dev/null")
        .map(|f| f.new_path.clone())
        .collect();

    let canonical_dir = std::fs::canonicalize(dir).ok();

    // Phase 0: added / modified ファイルの exported シンボルを抽出し、cross-file 参照
    // 判定の対象になりうる name を集めて ApiRefIndex を構築する。候補シンボルごとの
    // 全リポジトリ走査 (O(候補数 × 全ファイル)) を chunk 単位の batch 検索に集約する。
    let mut prepared: Vec<PreparedDiffFile> = Vec::with_capacity(diff_files.len());
    let mut index_names: HashSet<String> = HashSet::new();
    for df in diff_files {
        if should_skip_diff_file(df, &gitattrs, canonical_dir.as_deref()) {
            prepared.push(PreparedDiffFile::Skip);
            continue;
        }
        if df.old_path == "/dev/null" {
            // new_path を 1 回 read+parse して exported / callees を導出 (perf #2)。
            let facts = extract_new_file_facts(dir, &df.new_path);
            let new_syms = facts.exported;
            let in_file_callees = facts.callees;
            if let Some(syms) = &new_syms {
                for (name, _, _) in syms {
                    // per-symbol 実装と同じ短絡: ファイル内から呼ばれるシンボルは
                    // `is_used_in_diff_paths` に到達しないため検索対象に入れない。
                    if !is_internally_connected(&in_file_callees, name) {
                        index_names.insert(bare_name(name).to_string());
                    }
                }
            }
            prepared.push(PreparedDiffFile::Added {
                new_syms,
                in_file_callees,
            });
            continue;
        }
        if df.new_path == "/dev/null" {
            prepared.push(PreparedDiffFile::Deleted);
            continue;
        }
        // rename 差分では base 側に新パスが存在しないため、旧版は old_path から読む。
        let old_syms = extract_exported_symbols_from_git(dir, base, &df.old_path);
        // new_path を 1 回 read+parse して exported / callees / export surface を導出 (perf #2)。
        // export surface は process_modified_file が再 parse せず使えるよう
        // PreparedDiffFile に持たせる。
        let facts = extract_new_file_facts(dir, &df.new_path);
        let new_syms = facts.exported;
        let in_file_callees = facts.callees;
        let new_export_surface_names = facts.export_surface_names;
        if let (Some(old), Some(new)) = (&old_syms, &new_syms) {
            collect_modified_file_index_names(
                old,
                new,
                &in_file_callees,
                is_bash_script_path(&df.old_path),
                &mut index_names,
            );
        }
        prepared.push(PreparedDiffFile::Modified {
            old_syms,
            new_syms,
            in_file_callees,
            new_export_surface_names,
        });
    }
    let ref_index = ApiRefIndex::build(dir, &index_names);

    // process_modified_file → classify_signature_change → is_modified_closed_in_diff の per-file
    // キャッシュ (import 行集合 / 変更行集合) を detect_api_changes スコープで 1 度確保し、
    // 全 modified シンボル横断で共有する。per-symbol の git diff 起動 + tree-sitter parse を
    // unique file 単位に削減 (#perf N+1 改善)。
    let mut closure_caches = crate::commands::api_changes::ref_index::ApiClosureCaches::default();

    let inputs = DetectionInputs {
        dir,
        base,
        diff_files,
        diff_new_paths: &diff_new_paths,
        ref_index: &ref_index,
    };
    let mut state = DetectionState {
        buckets: &mut buckets,
        base_reexports: &mut rust_reexport_cache,
        worktree_reexports: &mut rust_new_reexport_cache,
        closure_caches: &mut closure_caches,
    };

    for (df, prep) in diff_files.iter().zip(&prepared) {
        match prep {
            PreparedDiffFile::Skip => {}
            PreparedDiffFile::Added {
                new_syms,
                in_file_callees,
            } => {
                if let Some(new_syms) = new_syms {
                    process_added_file(
                        &inputs,
                        &mut state,
                        df,
                        &AddedFileFacts {
                            new_syms,
                            in_file_callees,
                        },
                    );
                }
            }
            PreparedDiffFile::Deleted => {
                process_deleted_file(&inputs, &mut state, df);
            }
            PreparedDiffFile::Modified {
                old_syms,
                new_syms,
                in_file_callees,
                new_export_surface_names,
            } => {
                if let (Some(old_syms), Some(new_syms)) = (old_syms, new_syms) {
                    process_modified_file(
                        &inputs,
                        &mut state,
                        df,
                        &ModifiedFileFacts {
                            old_syms,
                            new_syms,
                            in_file_callees,
                            new_export_surface_names,
                        },
                    );
                }
            }
        }
    }

    // git の rename detection が効かない diff (外部供給 / 非 git 入力 / 設定で無効化された
    // 環境など) に対するフォールバックとして、同一 (name, kind, signature) の add/rm ペアを
    // rename または move として相殺し、`moved` カテゴリに移す。`all_new_candidates` には
    // `is_used_in_diff_paths` 等で `added` から外れた候補も含まれるため、module → package
    // 化のように新規ファイル側のシンボルが同 diff 内の `__init__.py` 等から参照されて
    // `added` に乗らないケースでも `removed` を相殺できる。
    let (added, removed, moved) =
        reconcile_with_moves(buckets.added, buckets.removed, buckets.all_new_candidates);

    // removed のうち HEAD ツリーで他ファイル参照 0 件のものを `removed_dead` に振り分け。
    // 「base 時点で dead だった symbol の整理」だけでなく「base alive → HEAD で関連
    // caller も削除」も同 diff 内で repo 内到達性 0 になるため同一カテゴリに含む。
    // 順序は moved > removed_dead (rename/move 相殺を先に行わないと移動が dead 誤分類
    // される)。codex 設計合意 (Issue
    // 2026-05-28-meet-virtual-you-gemini-multi-select 対応)。
    //
    // qualname (`Container.method`) は refs 検索が identifier ノードでマッチするため
    // 常に 0 件返却となり誤分類するため、bare name で検索する。同名 def が複数残って
    // いる場合は「部分的削除」or「同名複数定義」の可能性があるため保守的に removed
    // に残す (codex 指摘 1 対応)。
    //
    // 複数候補がある場合、`find_references_batch` で 1 度のリポジトリ走査に集約する
    // (codex 指摘 3 対応: 候補数 × リポ全体走査の回避)。
    let (removed_kept, removed_dead) = partition_removed_dead_candidates(dir, removed);

    // api.add に確定した候補にだけ同一ファイル内参照数を添える。move 相殺 (`moved`) で
    // 抜けた候補には算出しないよう、reconcile 後のこの位置で 1 度だけ数える
    // (Issue 2026-08-04-review-add-scope-naming)。参照集合は既に構築済みの ref_index から
    // 引くため追加のディレクトリ走査は発生しない。
    let added: Vec<ApiSymbol> = added
        .into_iter()
        .map(|c| {
            let refs_internal =
                count_internal_refs(&ref_index, dir, &c.file, &c.name, &mut closure_caches);
            c.into_added_api_symbol(refs_internal)
        })
        .collect();

    ApiChanges {
        added,
        removed: removed_kept
            .into_iter()
            .map(|c| c.into_api_symbol())
            .collect(),
        modified: buckets.modified,
        moved,
        property_to_field: buckets.property_to_field,
        removed_dead: removed_dead
            .into_iter()
            .map(|c| c.into_api_symbol())
            .collect(),
        modified_closed_in_diff: buckets.modified_closed_in_diff,
        const_value_changes: buckets.const_value_changes,
        compatible_modified: buckets.compatible_modified,
    }
}

/// 同名・同種別・同シグネチャの api.add / api.rm ペアを `moved` として相殺する。
///
/// `all_new_candidates` は `added` フィルタ適用前の新規側候補一覧（`added` の上位集合）。
/// `is_used_in_diff_paths` などで `added` から落ちた候補も `removed` との突き合わせに
/// 利用するため、別系統で渡す。
///
/// 戻り値:
/// - `kept_added`: `moved` で相殺されなかった追加シンボル
/// - `kept_removed`: `moved` で相殺されなかった削除シンボル
/// - `moved`: `from`/`to` のペアにまとめた移動シンボル
pub(crate) fn reconcile_with_moves(
    added: Vec<ApiSymbolCandidate>,
    removed: Vec<ApiSymbolCandidate>,
    all_new_candidates: Vec<ApiSymbolCandidate>,
) -> (
    Vec<ApiSymbolCandidate>,
    Vec<ApiSymbolCandidate>,
    Vec<MovedSymbol>,
) {
    use std::collections::HashMap;
    use std::collections::VecDeque;

    // 1) removed を (name, kind, signature) でバケット化。
    //    バケットには候補そのものではなく `removed` 内の index を積む。値を持たせて
    //    最後に `into_values()` で回収すると HashMap (RandomState) の反復順がそのまま
    //    出力順になり、api.rm / removed_dead の並びが実行ごとに変わってしまう
    //    (決定論的出力の前提が崩れ、snapshot 比較とトリアージ結果の再現性が壊れる)。
    let mut removed_bucket: HashMap<(String, String, String), VecDeque<usize>> = HashMap::new();
    for (i, sym) in removed.iter().enumerate() {
        removed_bucket
            .entry((sym.name.clone(), sym.kind.clone(), sym.signature.clone()))
            .or_default()
            .push_back(i);
    }
    let mut removed_matched = vec![false; removed.len()];

    // 2) 新規候補を順に走査して removed と突き合わせ、`moved` を組み立てる。
    //    同じ (name, kind, signature, file) の重複候補は最初の 1 件だけ扱う。
    //    (name, kind, signature) を共有する複数 add が同じ removed と組まないように、
    //    一度マッチした new 側は `matched_new_files` に記録しておき、後で `added` から
    //    除外する。
    let mut moved: Vec<MovedSymbol> = Vec::new();
    let mut seen_new_keys: std::collections::HashSet<(String, String, String, String)> =
        std::collections::HashSet::new();
    let mut matched_new_files: HashMap<
        (String, String, String),
        std::collections::HashSet<String>,
    > = HashMap::new();
    for new in &all_new_candidates {
        let dedup_key = (
            new.name.clone(),
            new.kind.clone(),
            new.signature.clone(),
            new.file.clone(),
        );
        if !seen_new_keys.insert(dedup_key) {
            continue;
        }
        let bucket_key = (new.name.clone(), new.kind.clone(), new.signature.clone());
        if let Some(bucket) = removed_bucket.get_mut(&bucket_key)
            && let Some(rm_ix) = bucket.pop_front()
        {
            let rm = &removed[rm_ix];
            removed_matched[rm_ix] = true;
            matched_new_files
                .entry(bucket_key)
                .or_default()
                .insert(new.file.clone());
            moved.push(MovedSymbol {
                name: rm.name.clone(),
                kind: rm.kind.clone(),
                from: rm.file.clone(),
                to: new.file.clone(),
            });
        }
    }

    // 3) `moved` で相殺された候補は `added` からも除外する。
    let kept_added: Vec<ApiSymbolCandidate> = added
        .into_iter()
        .filter(|a| {
            let key = (a.name.clone(), a.kind.clone(), a.signature.clone());
            !matched_new_files
                .get(&key)
                .map(|files| files.contains(&a.file))
                .unwrap_or(false)
        })
        .collect();

    // 4) ペア化されなかった `removed` を、入力順のまま集める。
    let kept_removed: Vec<ApiSymbolCandidate> = removed
        .into_iter()
        .zip(removed_matched)
        .filter_map(|(sym, matched)| (!matched).then_some(sym))
        .collect();

    (kept_added, kept_removed, moved)
}

pub(crate) mod diff_processing;
pub(crate) mod exported;
mod js_ts_shadow;
pub(crate) mod prepare;
pub(crate) mod removed;
pub(crate) mod signature;

mod python_contract;

mod python_signature;

mod ref_index;

mod removed_attribution;

mod rust_public;

mod source_pair;

mod ts_const_arg;

mod ts_signature;

pub(crate) use diff_processing::*;
pub(crate) use exported::*;
pub(crate) use prepare::*;
pub(crate) use python_contract::*;
pub(crate) use python_signature::*;
pub(crate) use removed::*;
pub(crate) use signature::*;

pub(crate) use ref_index::*;

pub(crate) use removed_attribution::*;

pub(crate) use rust_public::*;

pub(crate) use source_pair::{CompatibleModSite, SignatureSourceCache};

pub(crate) use ts_signature::*;
