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
    enclosing_container, file_name_is_pytest_module, is_phpunit_test_symbol,
    is_python_conftest_function, is_test_path,
};

use super::git_input::git_show_blob;

pub(crate) type ExportedSymbols = Vec<(String, String, String)>;

#[cfg(test)]
type ExportedSymbolsWithLang = (crate::language::LangId, ExportedSymbols);

/// `detect_api_changes` に渡す diff ファイル一覧。`diff_files` (= `parse_unified_diff` の結果) に、
/// そこへ現れない内容同一の rename のうち、拡張子で言語を判定できるソースを判定できないパスへ
/// 移したもの (`git mv api.ts api.ts.bak`) を足す。
///
/// impact / cochange / dead-code は内容同一の rename を扱わない前提で揃えているため
/// `parse_unified_diff` 自体には足さず、API 差分でだけ見る。上の rename は旧ファイルの API が
/// すべて消える変更だが、hunk を持たないため、これを足さないと api.rm が出ない (同じファイルを
/// `git rm` すれば出る)。それ以外の内容同一 rename は API 面を変えないので足さない
/// (ディレクトリ移動で大量に来る画像や文書の rename ごとにファイルを読ませない)。
/// 新側が shebang で言語を判定できる場合の除外は `detect_api_changes` 側で行う。
pub(crate) fn api_diff_files<'a>(
    diff_files: &'a [crate::models::impact::DiffFile],
    diff_input: &str,
) -> std::borrow::Cow<'a, [crate::models::impact::DiffFile]> {
    let hunkless_renames: Vec<_> = crate::engine::diff::parse_hunkless_renames(diff_input)
        .into_iter()
        .filter(renames_out_of_known_extension)
        .collect();
    if hunkless_renames.is_empty() {
        return std::borrow::Cow::Borrowed(diff_files);
    }
    std::borrow::Cow::Owned(diff_files.iter().cloned().chain(hunkless_renames).collect())
}

/// rename の旧パスは拡張子で言語を判定できるが、新パスはできない (`api.ts` → `api.ts.bak`)。
fn renames_out_of_known_extension(df: &crate::models::impact::DiffFile) -> bool {
    let known =
        |path: &str| crate::language::LangId::from_path(camino::Utf8Path::new(path)).is_ok();
    known(&df.old_path) && !known(&df.new_path)
}

pub(crate) fn detect_api_changes(
    dir: &str,
    base: &str,
    diff_files: &[crate::models::impact::DiffFile],
) -> ApiChanges {
    detect_api_changes_with_renames(dir, base, diff_files, &[])
}

/// `renames` は diff の hunk の無い変更 (`parse_hunkless_changes`: 内容同一の rename と空のファイルの
/// 追加・削除)。API の分類には使わず、削除した Rust の宣言と同名の参照があるファイルの変更前の位置を
/// 引くのと、TS の `stripInternal` の比べ直しの引き金 (Issue #287・#288) にだけ使う。
pub(crate) fn detect_api_changes_with_renames(
    dir: &str,
    base: &str,
    diff_files: &[crate::models::impact::DiffFile],
    renames: &[crate::models::impact::DiffFile],
) -> ApiChanges {
    let base_blobs = crate::commands::git_input::GitBlobBatch::new(dir, base);
    detect_api_changes_with_renames_and_blobs(dir, base, diff_files, renames, &base_blobs)
}

/// 同じ review の base 読み手を、API 差分と改名候補の解析で共有する。
pub(crate) fn detect_api_changes_with_renames_and_blobs(
    dir: &str,
    base: &str,
    diff_files: &[crate::models::impact::DiffFile],
    renames: &[crate::models::impact::DiffFile],
    base_blobs: &crate::commands::git_input::GitBlobBatch,
) -> ApiChanges {
    let mut buckets = ApiChangeBuckets::default();
    let mut rust_public = RustPublicApiContext::default();

    // 同一 diff 内で追加/変更されたファイルパスの集合。新規 pub シンボルが diff 内の
    // 別ファイルから参照されていれば「同一 diff 内で完結して使用されている」と判断し、
    // api.add から除外する（binary crate の pub struct が同 diff 内で use されるケース等）。
    let diff_new_paths: HashSet<String> = diff_files
        .iter()
        .filter(|f| f.new_path != "/dev/null")
        .map(|f| f.new_path.clone())
        .collect();

    let canonical_dir = std::fs::canonicalize(dir).ok();

    // 差分の中の package の Rust の lib crate (Issue #300)。束縛が変わった package だけ、旧新の
    // module のファイルを必要になったときに開く。
    let rust_crates = RustCrates::new(dir, diff_files, renames, base_blobs);
    // Phase 0: diff ファイルごとの事実と、ApiRefIndex で検索する名前を先に集める。
    let PreparedDiff {
        files: prepared,
        mut index_names,
        unverified_deleted_paths,
        parse_truncations,
        strip_internal,
        rechecks,
    } = PreparedDiff::new(
        dir,
        diff_files,
        renames,
        canonical_dir.as_deref(),
        base_blobs,
        &rust_crates,
    );
    buckets.parse_truncations.extend(parse_truncations);
    // 再公開に効く `use` が変わった lib crate で、再公開した名前の有る設定を比べ直す (Issue #306・
    // #318)。module の比べ直しは、消した (足した) module の名前をこの比較が取り込みの束縛として比べたかを
    // 使うので先に作る (Issue #314)。
    let reexport_names =
        rust_reexport_names::ReexportNames::new(dir, &rust_crates, diff_files, renames);
    // module の宣言の条件 (`#[cfg] mod m;` など) が変わった lib crate で、配下の公開の項目の有る設定を
    // 比べ直す (Issue #305)。追加の候補の名前は索引で引けるよう先に足す。
    let module_availability = rust_module_availability::ModuleAvailability::new(
        dir,
        &rust_crates,
        base_blobs,
        &strip_internal,
        diff_files,
        renames,
        &reexport_names,
        &mut rust_public,
    );
    index_names.extend(module_availability.index_names().cloned());
    buckets
        .parse_truncations
        .extend(module_availability.truncations.iter().cloned());
    index_names.extend(reexport_names.index_names().cloned());
    buckets
        .parse_truncations
        .extend(reexport_names.truncations.iter().cloned());

    // 型の移動と同じ差分でシグネチャの変わったメンバー、移動と同じ差分でシグネチャの変わった
    // 自由関数・定数は、削除と追加に割れた後で変更として判定し直す
    // (`pair_moved_member_changes` / `pair_moved_symbol_changes`)。その名前も検索対象に入れておく。
    let member_files = MemberFileFacts::new(diff_files, &prepared);
    member_files.collect_index_names(&mut index_names);
    member_files.collect_moved_symbol_index_names(&mut index_names);
    index_names.extend(python_import_rebinding::index_names(
        dir, diff_files, &prepared,
    ));
    let ref_index = ApiRefIndex::build(dir, &index_names);
    let old_ref_index = std::cell::OnceCell::new();

    // process_modified_file → classify_signature_change → is_modified_closed_in_diff の per-file
    // キャッシュ (import 行集合 / 変更行集合) を detect_api_changes スコープで 1 度確保し、
    // 全 modified シンボル横断で共有する。per-symbol の git diff 起動 + tree-sitter parse を
    // unique file 単位に削減 (#perf N+1 改善)。
    let mut closure_caches = crate::commands::api_changes::ref_index::ApiClosureCaches::default();
    // 省略可能引数・props の削除の証明が在庫の外のファイルを 1 回で読めるように、照合する語
    // (変えた関数の名前と、変えた TS 系のファイルを指す名前) を先に渡す (Issue #208)。
    closure_caches.optional_removal =
        crate::commands::api_changes::ts_signature::OptionalRemovalCache::with_words(
            crate::commands::api_changes::ts_signature::optional_removal_words(
                &index_names,
                diff_files,
            ),
        );

    // 項目の有る設定の比較に、ファイルを置く module の置き場の条件を足す (Issue #316)。
    let placement_availability =
        rust_placement_availability::PlacementAvailability::new(&rust_crates);

    let inputs = DetectionInputs {
        dir,
        base,
        diff_files,
        diff_new_paths: &diff_new_paths,
        ref_index: &ref_index,
        old_ref_index: &old_ref_index,
        index_names: &index_names,
        prepared: &prepared,
        base_blobs,
        strip_internal: &strip_internal,
        rust_crates: &rust_crates,
        module_availability: &module_availability,
        reexport_names: &reexport_names,
        placement_availability: &placement_availability,
    };
    let mut state = DetectionState {
        buckets: &mut buckets,
        rust_public: &mut rust_public,
        closure_caches: &mut closure_caches,
    };

    // 設定だけの変更で比べ直す TS のソース (Issue #287) も、変更ファイルと同じく比べる。差分の
    // ファイル (`diff_files`) には入れないので、変えたファイルとしては扱わない。
    for (df, prep) in diff_files
        .iter()
        .zip(&prepared)
        .chain(rechecks.iter().map(|(df, prep)| (df, prep)))
    {
        match prep {
            PreparedDiffFile::Skip => {}
            PreparedDiffFile::Added {
                new_syms,
                in_file_callees,
                rust_declarations,
            } => {
                if let Some(new_syms) = new_syms {
                    process_added_file(
                        &inputs,
                        &mut state,
                        df,
                        &AddedFileFacts {
                            new_syms,
                            in_file_callees,
                            rust_declarations: rust_declarations.as_deref(),
                        },
                    );
                }
            }
            PreparedDiffFile::Deleted {
                old_syms,
                old_source_unverified,
                rust_declarations,
            } => {
                if let Some(old_syms) = old_syms {
                    process_deleted_file(
                        &inputs,
                        &mut state,
                        df,
                        old_syms,
                        *old_source_unverified,
                        rust_declarations.as_deref(),
                    );
                }
            }
            PreparedDiffFile::Modified {
                type_dependencies,
                unparsed_declarations,
                old_syms,
                new_syms,
                parameter_pattern_only_names,
                rust_const_additions,
                in_file_callees,
                new_export_surface_names,
                rust_declarations,
            } => {
                if let (Some(old_syms), Some(new_syms)) = (old_syms, new_syms) {
                    process_modified_file(
                        &inputs,
                        &mut state,
                        df,
                        &ModifiedFileFacts {
                            type_dependencies,
                            unparsed_declarations,
                            old_syms,
                            new_syms,
                            parameter_pattern_only_names,
                            rust_const_additions,
                            in_file_callees,
                            new_export_surface_names,
                            rust_declarations: rust_declarations.as_deref(),
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
    let all_new_candidates = std::mem::take(&mut state.buckets.all_new_candidates);
    let (verified_removed, unverified_removed): (Vec<_>, Vec<_>) =
        std::mem::take(&mut state.buckets.removed)
            .into_iter()
            .partition(|candidate| !unverified_deleted_paths.contains(&candidate.file));
    let (verified_removed, import_rebindings) =
        python_import_rebinding::partition(&inputs, state.buckets, verified_removed);
    let (added, removed, moved, ambiguous_relation_keys) = reconcile_with_moves(
        std::mem::take(&mut state.buckets.added),
        verified_removed,
        all_new_candidates.clone(),
    );

    // 型の移動と同じ差分でシグネチャの変わったメンバーは、厳密一致の move に乗らず削除と
    // 追加に割れる。git が rename として渡す差分と同じ判定にするため、移動元と移動先の組を
    // 変更ファイルと同じ規則で判定する (Issue #109)。
    let mut member_bindings: std::collections::HashMap<(Side, String, String, String), bool> =
        std::collections::HashMap::new();
    let (added, removed, moved_member_changes) = pair_moved_member_changes(
        &moved,
        added,
        removed,
        &all_new_candidates,
        &ambiguous_relation_keys,
        &member_files,
        &mut |side, path, owner, member| {
            *member_bindings
                .entry((side, path.to_owned(), owner.to_owned(), member.to_owned()))
                .or_insert_with(|| match side {
                    Side::Old => base_blobs
                        .read(path)
                        .is_some_and(|source| proves_member_binding(path, &source, owner, member)),
                    Side::New => load_new_source(dir, path)
                        .is_some_and(|source| proves_member_binding(path, &source, owner, member)),
                })
        },
    );
    process_moved_member_changes(&inputs, &mut state, &moved_member_changes);

    // 自由関数・定数を移すのと同時に、移さなければ互換と判定される変更 (定数の初期化の式、
    // 同じ型の別名への書き換え) をした宣言も、削除と追加に割れる。組を移さないときと同じ規則で
    // 判定し、互換と確定したものだけを moved にする (Issue #114)。
    let moved_symbol_changes = pair_moved_symbol_changes(
        &removed,
        &unverified_removed,
        &all_new_candidates,
        &moved,
        &ambiguous_relation_keys,
        &member_files,
    );
    let compatible_moves = process_moved_symbol_changes(&inputs, &mut state, &moved_symbol_changes);
    let (added, removed, moved) =
        settle_moved_symbol_changes(added, removed, moved, &compatible_moves);

    // 項目を別のファイルへ移して元のファイルから `pub use` で再公開した差分は、削除にしなかった
    // 旧い宣言を再公開の先の宣言と組にして、移さない場合と同じ規則で比べる (Issue #281)。
    let reexported_removals = std::mem::take(&mut state.buckets.reexported_removals);
    let (added, moved) = process_reexported_removals(
        &inputs,
        &mut state,
        &reexported_removals,
        renames,
        added,
        moved,
        &member_files,
    );

    // 別のファイルへ移した Rust の項目も、移動元と移動先の宣言 (同じ場所の `#[cfg]` の分岐を
    // まとめて) の有る設定を比べる (Issue #291)。移動は項目の対応を示すだけで、有る設定を保った
    // ことの証明にはしない。
    let moved = rust_impl_moves::verify(&inputs, moved, &mut state.buckets.modified);
    compare_moved_availability(&inputs, &mut state, &moved);
    // 内容の同じ rename をした Rust のファイルの項目も、旧い置き場と新しい置き場の条件で比べる (Issue #316)。
    compare_renamed_availability(&inputs, &mut state, renames);
    // module の宣言の条件が変わった module の項目の比べ直しの結果を、ファイルごとの比較と同じ区分に
    // 振り分ける (Issue #305)。
    record_module_availability(&inputs, &mut state);
    // 再公開した名前の比べ直しの結果も、同じ区分に振り分ける (Issue #306・#318)。
    record_reexport_names(&inputs, &mut state);
    rust_opaque_return::record(&inputs, &mut state, &moved);

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
    let reexport_removed = std::mem::take(&mut state.buckets.reexport_removed);
    let rust_packages =
        removed_cargo::RemovedRustPackages::new(dir, base_blobs, diff_files, renames);
    let (mut removed_kept, removed_dead, removed_unresolved) = partition_removed_dead_candidates(
        dir,
        base_blobs,
        &rust_packages,
        removed,
        &ambiguous_relation_keys,
    );
    removed_kept.extend(unverified_removed);
    // 有る設定の一部から消えた Rust の項目 (Issue #291) は、項目そのものは残るので移動の相殺に
    // かけず、名前の参照が repo に残れば removed、無ければ removed_dead にする。
    let cfg_removed = std::mem::take(&mut state.buckets.cfg_removed);
    // 旧い版の全ての設定に有り、新しい版のどの設定にも無くなった項目 (module の宣言を消した。Issue #314) は、
    // ふつうの削除と同じく再公開の名前の削除と重ねない (名前が同じなので、参照による区分も同じになる)。
    // 一部の設定にしか無かった項目は、再公開の名前が残りの設定で消えたことを表さないので重ねる。
    let lost_everywhere: Vec<ApiSymbolCandidate> = cfg_removed
        .iter()
        .filter(|(_, availability)| {
            availability.new.is_empty() && availability.old.iter().any(Vec::is_empty)
        })
        .map(|(candidate, _)| candidate.clone())
        .collect();
    let (cfg_removed_kept, cfg_removed_dead) = partition_cfg_removals(
        dir,
        cfg_removed
            .into_iter()
            .map(|(candidate, availability)| (candidate, Some(availability)))
            .collect(),
    );
    // 再公開した名前の削除 (Issue #306・#318) も同じく分け、同じ名前の宣言自身の削除が同じか強い区分に
    // 残るものは重ねない。
    let removed_kept_and_lost: Vec<ApiSymbolCandidate> = removed_kept
        .iter()
        .cloned()
        .chain(lost_everywhere)
        .collect();
    let (reexport_removed_kept, reexport_removed_dead) =
        partition_reexport_removals(dir, reexport_removed, &removed_kept_and_lost, &removed_dead);
    let cfg_added = std::mem::take(&mut state.buckets.cfg_added);
    let reexport_added = std::mem::take(&mut state.buckets.reexport_added);

    // cfg の相補的な 2 分岐は公開名として 1 件。同一ファイルの新規候補だけを
    // move 照合が終わった後に畳み、シグネチャ別の対応付け材料は保持する。
    let added = collapse_exclusive_rust_const_additions(dir, added);
    // 新しく再公開した名前は、名前の指す宣言自身の追加を報告するなら重ねない (Issue #306・#318)。
    let reexport_added = uncovered_reexports(reexport_added, &[&added]);

    // api.add に確定した候補にだけ同一ファイル内参照数を添える。move 相殺 (`moved`) で
    // 抜けた候補には算出しないよう、reconcile 後のこの位置で 1 度だけ数える
    // (Issue 2026-08-04-review-add-scope-naming)。参照集合は既に構築済みの ref_index から
    // 引くため追加のディレクトリ走査は発生しない。
    let added: Vec<ApiSymbol> = added
        .into_iter()
        .map(|c| (c, None))
        .chain(
            cfg_added
                .into_iter()
                .map(|(c, availability)| (c, Some(availability))),
        )
        .chain(reexport_added.into_iter().map(|c| (c, None)))
        .map(|(c, cfg_availability)| {
            let refs_internal =
                count_internal_refs(&ref_index, dir, &c.file, &c.name, &mut closure_caches);
            ApiSymbol {
                cfg_availability,
                ..c.into_added_api_symbol(refs_internal)
            }
        })
        .collect();

    let type_dependency_symbols = type_dependency_symbols::collect(&inputs, &buckets.modified);
    fill_modification_diagnostics(&mut buckets.modified);
    let added_reference_summary = added_reference_summary::collect(
        &buckets.added_reference_candidates,
        &added,
        &moved,
        &ref_index,
        &diff_new_paths,
    );
    let mut api = ApiChanges {
        added_reference_summary,
        type_dependency_symbols,
        unresolved_modified_callers: closure_caches.unresolved_modified_callers,
        removed_unresolved,
        uncertain_removals: buckets.uncertain_removals,
        parse_truncations: buckets.parse_truncations,
        added,
        removed: removed_kept
            .into_iter()
            .map(|c| c.into_api_symbol())
            .chain(cfg_removed_kept)
            .chain(reexport_removed_kept)
            .collect(),
        modified: buckets.modified,
        moved,
        property_to_field: buckets.property_to_field,
        removed_dead: removed_dead
            .into_iter()
            .map(|c| c.into_api_symbol())
            .chain(cfg_removed_dead)
            .chain(reexport_removed_dead)
            .collect(),
        modified_closed_in_diff: buckets.modified_closed_in_diff,
        const_value_changes: buckets.const_value_changes,
        unchanged_static_consts: buckets.unchanged_static_consts,
        type_annotation_changes: buckets.type_annotation_changes,
        unchanged_type_annotations: buckets.unchanged_type_annotations,
        callback_body_changes: buckets.callback_body_changes,
        compatible_modified: buckets.compatible_modified,
        import_rebindings,
        type_change_evidence: Vec::new(),
    };
    api.type_change_evidence =
        ts_signature::collect_nullable_type_evidence(dir, base, &api, base_blobs, &strip_internal);
    super::reference_evidence::attach_api(
        dir,
        &mut api.modified,
        |name| ref_index.refs_for(name),
        |name| {
            let scan = &ref_index.added_reference_scan;
            ref_index.complete_refs_for(name).is_some()
                && scan.failed_files == 0
                && scan.parse_error_files == 0
                && !scan.scan_failed
                && scan.unknown_references == 0
                && scan.truncations.is_empty()
                && scan.generated_files_skipped.is_none()
                && ref_index.refs_for(name).is_some_and(|references| {
                    references.iter().all(|reference| {
                        reference.kind != Some(crate::models::reference::RefKind::Unknown)
                    })
                })
        },
    );
    attach_syntax_evidence(&mut api, dir, base_blobs, diff_files);
    api
}

fn attach_syntax_evidence(
    api: &mut ApiChanges,
    dir: &str,
    blobs: &super::git_input::GitBlobBatch,
    diff_files: &[crate::models::impact::DiffFile],
) {
    use crate::engine::change_evidence::ChangeEvidenceIndex;
    let paths: std::collections::BTreeSet<_> = api
        .modified
        .iter()
        .chain(&api.modified_closed_in_diff)
        .chain(&api.const_value_changes)
        .chain(
            api.type_annotation_changes
                .iter()
                .map(|entry| &entry.change),
        )
        .chain(api.callback_body_changes.iter().map(|entry| &entry.change))
        .map(|change| change.file.clone())
        .chain(
            api.compatible_modified
                .iter()
                .map(|change| change.file.clone()),
        )
        .collect();
    for path in paths {
        let lang = crate::language::LangId::from_path(camino::Utf8Path::new(&path)).ok();
        let old_path = diff_files
            .iter()
            .find(|file| file.new_path == path)
            .map_or(path.as_str(), |file| file.old_path.as_str());
        let index = lang
            .and_then(|lang| {
                if !crate::engine::impact::is_safe_diff_path(old_path) {
                    return None;
                }
                let root = std::fs::canonicalize(dir).ok()?;
                let current = std::fs::canonicalize(root.join(&path)).ok()?;
                if !current.starts_with(&root) {
                    return None;
                }
                let old = blobs.read(old_path)?;
                let new = source_pair::load_new_source(dir, &path)?;
                Some(ChangeEvidenceIndex::from_sources(&old, &new, lang))
            })
            .unwrap_or_else(ChangeEvidenceIndex::unavailable);
        for change in api
            .modified
            .iter_mut()
            .chain(&mut api.modified_closed_in_diff)
            .chain(&mut api.const_value_changes)
            .chain(
                api.type_annotation_changes
                    .iter_mut()
                    .map(|entry| &mut entry.change),
            )
            .chain(
                api.callback_body_changes
                    .iter_mut()
                    .map(|entry| &mut entry.change),
            )
            .filter(|change| change.file == path)
        {
            change.change_evidence = Some(lang.map_or_else(
                || {
                    crate::models::change_evidence::ChangeEvidence::unknown(
                        crate::models::change_evidence::ChangeUnknownReason::UnsupportedLanguage,
                    )
                },
                |lang| index.get(&change.name, &change.kind, lang),
            ));
        }
        for change in api
            .compatible_modified
            .iter_mut()
            .filter(|change| change.file == path)
        {
            change.change_evidence = Some(lang.map_or_else(
                || {
                    crate::models::change_evidence::ChangeEvidence::unknown(
                        crate::models::change_evidence::ChangeUnknownReason::UnsupportedLanguage,
                    )
                },
                |lang| index.get(&change.name, &change.kind, lang),
            ));
        }
    }
}

/// 新しく再公開した名前 (Issue #306・#318) のうち、名前の指す宣言のどれもが `reported` (宣言自身の追加の
/// 候補) にあるものを除く (同じ項目を 2 度報告しない)。
fn uncovered_reexports(
    names: Vec<(ApiSymbolCandidate, Vec<rust_reexport_names::DeclarationId>)>,
    reported: &[&[ApiSymbolCandidate]],
) -> Vec<ApiSymbolCandidate> {
    names
        .into_iter()
        .filter(|(_, targets)| {
            targets.is_empty()
                || !targets.iter().all(|(file, inline_module, name, kind)| {
                    reported
                        .iter()
                        .flat_map(|list| list.iter())
                        .any(|candidate| {
                            candidate.file == *file
                                && candidate.inline_module == *inline_module
                                && candidate.name == *name
                                && candidate.kind == *kind
                        })
                })
        })
        .map(|(candidate, _)| candidate)
        .collect()
}

/// 名前 `candidates` のうち、参照 (定義を除く) が repo に残るもの。探せなければ None。
fn referenced_names<'a>(
    dir: &str,
    candidates: impl Iterator<Item = &'a ApiSymbolCandidate>,
) -> Option<HashSet<String>> {
    let mut names: Vec<String> = Vec::new();
    for candidate in candidates {
        let name = bare_name(&candidate.name).to_owned();
        if !names.contains(&name) {
            names.push(name);
        }
    }
    AppService::new()
        .find_symbol_references_batch(&names, dir, None)
        .ok()
        .map(|results| {
            results
                .into_iter()
                .filter(|result| {
                    result.references.iter().any(|reference| {
                        reference.kind != Some(crate::models::reference::RefKind::Definition)
                    })
                })
                .map(|result| result.symbol)
                .collect()
        })
}

/// 再公開した名前の削除 (Issue #306・#318) を、[`partition_cfg_removals`] と同じく参照の有無で removed と
/// removed_dead に分ける。名前と同じ名前の宣言を指していて、その宣言自身の削除が同じか強い区分 (removed は
/// removed_dead より強い) に残るものは、同じ項目を 2 度報告しないよう除く (別名の削除は、別名の参照で
/// 区分するので残す)。
fn partition_reexport_removals(
    dir: &str,
    removals: Vec<(ApiSymbolCandidate, Vec<rust_reexport_names::DeclarationId>)>,
    kept: &[ApiSymbolCandidate],
    dead: &[ApiSymbolCandidate],
) -> (Vec<ApiSymbol>, Vec<ApiSymbol>) {
    if removals.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let referenced = referenced_names(dir, removals.iter().map(|(candidate, _)| candidate));
    let covered = |lists: &[&[ApiSymbolCandidate]],
                   candidate: &ApiSymbolCandidate,
                   targets: &[rust_reexport_names::DeclarationId]| {
        targets.iter().any(|(file, inline_module, name, kind)| {
            *name == candidate.name
                && lists
                    .iter()
                    .flat_map(|list| list.iter())
                    .any(|declaration| {
                        declaration.file == *file
                            && declaration.inline_module == *inline_module
                            && declaration.name == *name
                            && declaration.kind == *kind
                    })
        })
    };
    let mut result_kept = Vec::new();
    let mut result_dead = Vec::new();
    for (candidate, targets) in removals {
        let live = referenced
            .as_ref()
            .is_none_or(|names| names.contains(bare_name(&candidate.name)));
        if live {
            if !covered(&[kept], &candidate, &targets) {
                result_kept.push(candidate.into_api_symbol());
            }
        } else if !covered(&[kept, dead], &candidate, &targets) {
            result_dead.push(candidate.into_api_symbol());
        }
    }
    (result_kept, result_dead)
}

/// 有る設定の一部から消えた Rust の項目 (Issue #291) を、名前の参照 (定義を除く) が repo に残るもの
/// (removed) と残らないもの (removed_dead) に分ける。項目は残るので、削除の区分の残った定義への
/// 帰属 (`partition_removed_dead_candidates`) は使わない (残った定義は消えた設定の項目そのもの)。
/// 同じ名前の別のものの参照も数える (保守側)。参照を探せなければ removed に残す。
fn partition_cfg_removals(
    dir: &str,
    candidates: Vec<(
        ApiSymbolCandidate,
        Option<crate::models::review::CfgAvailability>,
    )>,
) -> (Vec<ApiSymbol>, Vec<ApiSymbol>) {
    if candidates.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let referenced = referenced_names(dir, candidates.iter().map(|(candidate, _)| candidate));
    let mut kept = Vec::new();
    let mut dead = Vec::new();
    for (candidate, availability) in candidates {
        let live = referenced
            .as_ref()
            .is_none_or(|names| names.contains(bare_name(&candidate.name)));
        let symbol = ApiSymbol {
            cfg_availability: availability,
            ..candidate.into_api_symbol()
        };
        if live {
            kept.push(symbol);
        } else {
            dead.push(symbol);
        }
    }
    (kept, dead)
}

fn collapse_exclusive_rust_const_additions(
    dir: &str,
    added: Vec<ApiSymbolCandidate>,
) -> Vec<ApiSymbolCandidate> {
    use std::collections::HashMap;

    let mut counts: HashMap<(String, String), usize> = HashMap::new();
    for candidate in &added {
        if candidate.kind == "constant" && candidate.file.ends_with(".rs") {
            *counts
                .entry((candidate.file.clone(), candidate.name.clone()))
                .or_default() += 1;
        }
    }

    let mut cache: HashMap<String, HashSet<String>> = HashMap::new();
    let mut seen = HashSet::new();
    added
        .into_iter()
        .filter(|candidate| {
            let key = (candidate.file.clone(), candidate.name.clone());
            if counts.get(&key).copied().unwrap_or(0) != 2 {
                return true;
            }
            let equivalent_names = cache.entry(candidate.file.clone()).or_insert_with(|| {
                let full_path = std::path::Path::new(dir).join(&candidate.file);
                let Some(path) = full_path.to_str() else {
                    return HashSet::new();
                };
                let path = camino::Utf8Path::new(path);
                let Ok(source) = parser::read_file(path) else {
                    return HashSet::new();
                };
                let Ok((tree, crate::language::LangId::Rust)) = parser::parse_file(path, &source)
                else {
                    return HashSet::new();
                };
                crate::engine::rust_cfg::exclusive_const_names(tree.root_node(), &source)
            });
            !equivalent_names.contains(&candidate.name) || seen.insert(key)
        })
        .collect()
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

    // 2) 追加側も同じキーで「相異なる移動先」(ファイルと、Rust の inline module の経路) の数を
    //    数える。同じ (name, kind, signature, file, inline module) の重複候補は 1 件として扱う
    //    (同一ファイル内の重複検出で移動先が 2 つあると誤判定しないため)。同じファイルの別々の
    //    inline module は別の移動先 (Issue #282)。
    let destination = |new: &ApiSymbolCandidate| (new.file.clone(), new.inline_module.clone());
    let mut new_files: HashMap<ApiRelationKey, HashSet<(String, Option<String>)>> = HashMap::new();
    let mut seen_new_keys: HashSet<(String, String, String, String, Option<String>)> =
        HashSet::new();
    for new in &all_new_candidates {
        let dedup_key = (
            new.name.clone(),
            new.kind.clone(),
            new.signature.clone(),
            new.file.clone(),
            new.inline_module.clone(),
        );
        if !seen_new_keys.insert(dedup_key) {
            continue;
        }
        new_files
            .entry(api_relation_key(new))
            .or_default()
            .insert(destination(new));
    }

    // 3) 削除 1 件・追加先 1 ファイルのキーだけを move 認定する。
    //    走査順は `all_new_candidates` の Vec 順なので、`moved` の並びは決定的。
    //    バケットの中身 (削除件数・移動先ファイル数) だけで可否が決まるため、
    //    `removed` / `all_new_candidates` の並び順を入れ替えても結果は変わらない。
    let mut moved: Vec<MovedSymbol> = Vec::new();
    let mut removed_matched = vec![false; removed.len()];
    let mut matched_new_files: HashMap<ApiRelationKey, (String, Option<String>)> = HashMap::new();
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
        matched_new_files.insert(bucket_key, destination(new));
        moved.push(MovedSymbol {
            from_inline_module: rm.inline_module.clone(),
            to_inline_module: new.inline_module.clone(),
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
        .filter(|a| matched_new_files.get(&api_relation_key(a)) != Some(&destination(a)))
        .collect();

    // 6) ペア化されなかった `removed` を、入力順のまま集める。
    let kept_removed: Vec<ApiSymbolCandidate> = removed
        .into_iter()
        .zip(removed_matched)
        .filter_map(|(sym, matched)| (!matched).then_some(sym))
        .collect();

    (kept_added, kept_removed, moved, ambiguous_relation_keys)
}

/// 確定した blocking エントリだけに、証明が未完了であることを補足する。
pub(crate) fn fill_modification_diagnostics(changes: &mut [ApiSymbolChange]) {
    for change in changes {
        if change.unverified.is_none()
            && crate::language::LangId::from_path(camino::Utf8Path::new(&change.file)).is_ok_and(
                |lang| {
                    matches!(
                        lang,
                        crate::language::LangId::Typescript | crate::language::LangId::Tsx
                    )
                },
            )
        {
            change.unverified = Some(crate::models::review::ApiModificationUnverified {
                reason:
                    crate::models::review::ApiModificationUnverifiedReason::CompatibilityUnverified,
                first_unproven: None,
            });
        }
    }
}

pub(crate) mod added_reference_summary;
pub(crate) mod diff_processing;
pub(crate) mod exported;
mod forwarded_values;
mod impact_compatibility;
mod js_const;
mod js_member_origin;
mod js_public_name;
mod js_ref_sites;
mod js_ts_call_arity;
mod js_ts_shadow;
mod jsonc;
mod moved_members;
mod moved_symbols;
pub(crate) mod prepare;
pub(crate) mod removed;
pub(crate) mod signature;
mod signature_tokens;
mod static_const;
mod type_annotation;

mod python_call_arity;
mod python_contract;
pub(crate) mod python_module_path;

mod python_import_rebinding;
mod python_literal;
mod python_member_origin;
mod python_optional_removal;
mod python_private_owner;

mod python_protocol_arg;
mod python_return_usage;

mod python_signature;

mod ref_index;

mod removed_attribution;
mod removed_cargo;
mod removed_ffi;
mod removed_rust_members;
mod removed_rust_paths;
mod removed_scope;

mod rust_availability;
mod rust_cfg_predicate;
mod rust_declarations;
mod rust_dependencies;
mod rust_enum_api;
mod rust_impl_moves;
mod rust_module_availability;
mod rust_opaque_return;
mod rust_placement_availability;
mod rust_public;
mod rust_reexport_names;
mod rust_ref_sites;
mod rust_struct_api;
mod rust_type_api;
mod rust_union_api;
pub(crate) mod ts_strip_internal;

mod source_pair;

mod ts_const_arg;
mod ts_function_values;
mod ts_literal_union_removal;
mod ts_namespace;

mod ts_required_members;

mod ts_alias_resolution;
mod ts_class_api;
mod ts_module_paths;
mod ts_signature;
mod type_dependency_symbols;

pub(crate) use diff_processing::*;
pub(crate) use exported::*;
pub(crate) use impact_compatibility::compatible_impact_names;
use js_const::detect_callback_body_only_change;
use moved_members::{
    MemberFileFacts, MovedMemberChange, Side, pair_moved_member_changes, proves_member_binding,
};
use moved_symbols::{
    MovedSymbolChange, MovedSymbolFiles, pair_moved_symbol_changes, settle_moved_symbol_changes,
};
pub(crate) use prepare::*;
pub(crate) use python_contract::*;
pub(crate) use python_literal::*;
pub(crate) use python_signature::*;
pub(crate) use removed::*;
pub(crate) use signature::*;

pub(crate) use ref_index::*;

pub(crate) use removed_attribution::*;

pub(crate) use rust_declarations::*;
pub(crate) use rust_dependencies::*;
pub(crate) use rust_public::*;

pub(crate) use source_pair::{
    CompatibleModSite, SignatureSourceCache, load_new_source, load_old_source_with_new,
};

pub(crate) use ts_signature::*;
