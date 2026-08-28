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
    let mut rust_public = RustPublicApiContext::default();

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
        rust_public: &mut rust_public,
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
    let (added, removed, moved, ambiguous_relation_keys) =
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
    //
    // `ambiguous_relation_keys` の候補は区分を揃える。N→1 集約のように対応付けが一意に
    // ならなかった削除は、`old_path` 依存の帰属判定で片方だけ `removed_dead` へ落ちうる
    // ため、blocking と informational に割れたときだけ保守側 (blocking) へ寄せる
    // (揃っているグループは触らない。Issue 2026-08-21-api-consolidation-many-to-one-move)。
    let (removed_kept, removed_dead) =
        partition_removed_dead_candidates(dir, removed, &ambiguous_relation_keys);

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

/// `(name, kind, signature)` — 削除側と追加側を突き合わせる対応付けキー。
///
/// `reconcile_with_moves` の move 認定と `partition_removed_dead_candidates` の
/// 区分揃えで**同じキー定義を共有する**必要があるため、モジュールレベルに置く
/// (片方だけ定義がずれると、対応付け不能と判定した削除に区分揃えが掛からない)。
pub(crate) type ApiRelationKey = (String, String, String);

/// 候補から `ApiRelationKey` を作る。
pub(crate) fn api_relation_key(sym: &ApiSymbolCandidate) -> ApiRelationKey {
    (sym.name.clone(), sym.kind.clone(), sym.signature.clone())
}

/// 同名・同種別・同シグネチャの api.add / api.rm ペアを `moved` として相殺する。
///
/// `all_new_candidates` は `added` フィルタ適用前の新規側候補一覧（`added` の上位集合）。
/// `is_used_in_diff_paths` などで `added` から落ちた候補も `removed` との突き合わせに
/// 利用するため、別系統で渡す。
///
/// **move 認定は `(name, kind, signature)` キーの両側が「削除 1 件・追加先 1 ファイル」の
/// ときだけ成立する** (対応付けが一意な場合に限る)。2→1 / 1→2 / 2→2 のような複数候補は
/// 対応付け不能として扱い、削除側は 1 件も相殺せず全件 `kept_removed` に残す。
///
/// 旧実装は removed バケットを FIFO で 1 件ずつ消費していたため、重複していた同名クラスを
/// 共有パッケージへ集約する N→1 リファクタで**同質な削除のうち先頭 1 件だけが `moved` に
/// 化け、残りが `api.rm` に残る**という順序依存の非対称が起きていた (どちらが `moved` に
/// なるかは diff の並び順という無関係な要因で決まる)。利用者からは「片方だけ壊れた」ように
/// 見え、実測でも 4 シンボルが誤って削除として報告された
/// (Issue 2026-08-21-api-consolidation-many-to-one-move)。
///
/// 残った削除を「全て同じ `to` への複数 `moved`」にする案は採らない。`moved` は hook で
/// informational (非 blocking) のため、移動と証明できていない削除まで `moved` に倒すと
/// 「旧 import パスが失われた」事実を隠し、破壊的変更の false negative になる。集約を
/// 独立カテゴリとして表現するなら、実体の関係 (moved / renamed / consolidated) と
/// 互換性 (旧公開面が維持されたか) の 2 軸を持つ API relation として設計し直す必要があり、
/// ラベルだけ足しても blocking 判定が決まらない。
///
/// 戻り値:
/// - `kept_added`: `moved` で相殺されなかった追加シンボル
/// - `kept_removed`: `moved` で相殺されなかった削除シンボル
/// - `moved`: `from`/`to` のペアにまとめた移動シンボル
/// - `ambiguous_relation_keys`: 追加側に同一キーの候補があるのに対応付けが一意にならず
///   move 認定できなかったキー。`partition_removed_dead_candidates` がこのキーの削除候補の
///   区分を揃えるために使う (blocking と informational に割れたときだけ保守側へ寄せる。下記参照)。
///
/// **`ambiguous_relation_keys` が必要な理由**: 対応付け不能に落ちた削除同士は bare 名が
/// 同じでも、`removed_dead` への降格判定 (`proves_survivor_origin`) が候補ごとの `old_path` /
/// 言語 / kind に依存するため、片方だけ informational へ落ちうる。実測では
/// `pkg/alpha.py` と `pkg/beta.py` の同名クラスを共有パッケージへ集約し HEAD に
/// `alpha.Store()` だけが残るケースで、Python 属性帰属が alpha 側を `removed` (blocking)、
/// beta 側を `removed_dead` (informational) に振り分けた。これでは本 Issue が解消したかった
/// 「同質な削除が別区分になる非対称」がそのまま残る。
pub(crate) fn reconcile_with_moves(
    added: Vec<ApiSymbolCandidate>,
    removed: Vec<ApiSymbolCandidate>,
    all_new_candidates: Vec<ApiSymbolCandidate>,
) -> (
    Vec<ApiSymbolCandidate>,
    Vec<ApiSymbolCandidate>,
    Vec<MovedSymbol>,
    std::collections::HashSet<ApiRelationKey>,
) {
    use std::collections::HashMap;
    use std::collections::HashSet;

    // 1) removed を対応付けキーでバケット化。バケットには候補そのものではなく
    //    `removed` 内の index を積む。値を持たせて最後に `into_values()` で回収すると
    //    HashMap (RandomState) の反復順がそのまま出力順になり、api.rm / removed_dead の
    //    並びが実行ごとに変わってしまう (決定論的出力の前提が崩れ、snapshot 比較と
    //    トリアージ結果の再現性が壊れる)。
    let mut removed_bucket: HashMap<ApiRelationKey, Vec<usize>> = HashMap::new();
    for (i, sym) in removed.iter().enumerate() {
        removed_bucket
            .entry(api_relation_key(sym))
            .or_default()
            .push(i);
    }

    // 2) 追加側も同じキーで「相異なる移動先ファイル数」を数える。同じ
    //    (name, kind, signature, file) の重複候補は 1 件として扱う (同一ファイル内の
    //    重複検出で移動先が 2 つあると誤判定しないため)。
    let mut new_files: HashMap<ApiRelationKey, HashSet<String>> = HashMap::new();
    let mut seen_new_keys: HashSet<(String, String, String, String)> = HashSet::new();
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
        new_files
            .entry(api_relation_key(new))
            .or_default()
            .insert(new.file.clone());
    }

    // 3) 削除 1 件・追加先 1 ファイルのキーだけを move 認定する。
    //    走査順は `all_new_candidates` の Vec 順なので、`moved` の並びは決定的。
    //    バケットの中身 (削除件数・移動先ファイル数) だけで可否が決まるため、
    //    `removed` / `all_new_candidates` の並び順を入れ替えても結果は変わらない。
    let mut moved: Vec<MovedSymbol> = Vec::new();
    let mut removed_matched = vec![false; removed.len()];
    let mut matched_new_files: HashMap<ApiRelationKey, String> = HashMap::new();
    for new in &all_new_candidates {
        let bucket_key = api_relation_key(new);
        if matched_new_files.contains_key(&bucket_key) {
            continue;
        }
        let Some(rm_ixs) = removed_bucket.get(&bucket_key) else {
            continue;
        };
        // 同一キーの削除が複数 = どの削除がこの追加に対応するか決められない。
        if rm_ixs.len() != 1 {
            continue;
        }
        // 同一キーの移動先が複数 = どのファイルへ移動したか決められない。
        if new_files
            .get(&bucket_key)
            .map(HashSet::len)
            .unwrap_or_default()
            != 1
        {
            continue;
        }
        let rm_ix = rm_ixs[0];
        let rm = &removed[rm_ix];
        removed_matched[rm_ix] = true;
        matched_new_files.insert(bucket_key, new.file.clone());
        moved.push(MovedSymbol {
            name: rm.name.clone(),
            kind: rm.kind.clone(),
            from: rm.file.clone(),
            to: new.file.clone(),
        });
    }

    // 4) 追加側に同一キーの候補があるのに move 認定できなかったキー = 対応付け不能。
    //    このキーの削除候補は `partition_removed_dead_candidates` で区分を揃える
    //    (blocking と informational に割れたときだけ保守側へ寄せる。上記 doc 参照)。
    //    追加側に同一キーが 1 件も無い削除は「対応先の無い純粋な削除」なので対象外＝
    //    従来どおり参照検索の結果で removed / removed_dead に分かれる。
    let ambiguous_relation_keys: HashSet<ApiRelationKey> = removed_bucket
        .keys()
        .filter(|key| new_files.contains_key(*key) && !matched_new_files.contains_key(*key))
        .cloned()
        .collect();

    // 5) `moved` で相殺された候補は `added` からも除外する。
    let kept_added: Vec<ApiSymbolCandidate> = added
        .into_iter()
        .filter(|a| matched_new_files.get(&api_relation_key(a)) != Some(&a.file))
        .collect();

    // 6) ペア化されなかった `removed` を、入力順のまま集める。
    let kept_removed: Vec<ApiSymbolCandidate> = removed
        .into_iter()
        .zip(removed_matched)
        .filter_map(|(sym, matched)| (!matched).then_some(sym))
        .collect();

    (kept_added, kept_removed, moved, ambiguous_relation_keys)
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
