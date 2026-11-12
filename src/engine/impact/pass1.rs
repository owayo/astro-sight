//! Pass 1: 変更ファイルをパースし、シンボルを抽出し、cross-file 参照検索が必要なシンボル名を決定する。
//!
//! ファイルごとの解析 ([`analyze_changed_file`]) は構文木とソースをその中で手放し、完成した
//! [`FileContext`] と、全体の検索名へ登録する情報だけを返す。全ファイルにまたがる検索名の登録は
//! [`CrossFileSymbolRegistry`] が、ファイルの順・ファイルの中の affected の順に行う。

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use camino::Utf8Path;

use super::{
    FileContext, NewSideSource, ci_key, classify_symbol_change, declaration,
    detect_signature_changes, filters, find_parent_type_name, is_in_test_context, jsx_props,
    object_members, reexport_move::ReexportMoveIndex, rust_impl_owner, rust_line_endings,
    rust_visibility_scope, signature, symbol_kind_str, symbol_overlaps_hunks,
};
use crate::commands::log_phase;
use crate::engine::{calls, diff, parser, symbols};
use crate::language::LangId;
use crate::models::impact::{AffectedSymbol, DiffFile, HunkInfo};
use crate::models::symbol::Symbol;

/// Pass 1 の結果。
pub(super) struct AffectedSymbolCollection {
    pub(super) file_contexts: Vec<FileContext>,
    /// cross-file 参照検索で探す名前 (`ci_key` 正規化済み)。初出の順で、method の親型は
    /// その名前より先に並ぶ。
    pub(super) search_names: Vec<String>,
    /// 名前 → method の親型。ファイルの中で同名の収録シンボルの親型が 1 つに決まる名前だけ。
    pub(super) method_parent_types: HashMap<String, String>,
    /// cross-file 検索に載せたシンボルの名前の数 (ログ用)。
    pub(super) included_count: usize,
}

/// 変更ファイルごとの解析で共通の入力。
struct Pass1Inputs<'a> {
    dir: &'a Path,
    new_side_source: &'a NewSideSource,
    reexport_moves: &'a ReexportMoveIndex,
    derived_contract_changes: &'a BTreeMap<String, BTreeSet<String>>,
    /// ファイルごとの判定 (変更行の抽出・シグネチャ検出・cross-file フィルタ) は対象ファイルの
    /// 区間しか読まないので、diff 全体ではなく区間だけを渡す。diff 全体を渡すとファイル数 ×
    /// diff 長の 2 乗になる (`FileSections` の doc 参照)。結果は diff 全体を渡した場合と同一。
    file_sections: diff::FileSections<'a>,
    /// diff が同じパスのファイル 1 つだけを変えた。他ファイルからの移動を追加と誤認しないこと、
    /// 可視性を広げただけの変更を差分だけで確かめることの前提にする。
    single_in_place_file: bool,
}

/// 変更ファイル 1 つの Pass 1 の結果。
struct AnalyzedChangedFile {
    context: FileContext,
    /// 全体の検索名へ登録する (名前, ファイルの中で合意した method の親型)。affected の順。
    registrations: Vec<(String, Option<String>)>,
}

/// 全ファイルにまたがる cross-file 検索の名前の登録。
#[derive(Default)]
struct CrossFileSymbolRegistry {
    /// 検索する名前 (初出の順)。
    names: Vec<String>,
    /// `names` の重複の確認用。
    seen: HashSet<String>,
    method_parent_types: HashMap<String, String>,
    /// cross-file 検索に載せたシンボルの名前 (ログの件数用)。
    included: HashSet<String>,
}

impl CrossFileSymbolRegistry {
    fn push_name(&mut self, key: String) {
        if self.seen.insert(key.clone()) {
            self.names.push(key);
        }
    }

    /// ファイル 1 つの収録シンボルを登録する。すでに別ファイル経由で登録済みの名前でも、
    /// ファイル固有の収録判定 (`FileContext::cross_file_symbol_keys`) は各ファイルが持つ。
    fn register(&mut self, registrations: Vec<(String, Option<String>)>) {
        for (key, parent) in registrations {
            self.included.insert(key.clone());
            if let Some(parent) = parent {
                self.method_parent_types.insert(key.clone(), parent.clone());
                self.push_name(parent);
            }
            self.push_name(key);
        }
    }
}

/// Pass 1: 変更ファイルをパースし、シンボルを抽出し、cross-file 参照検索が必要なシンボル名を決定する。
pub(super) fn collect(
    diff_input: &str,
    diff_files: &[DiffFile],
    dir: &Path,
    new_side_source: &NewSideSource,
    reexport_moves: &ReexportMoveIndex,
    derived_contract_changes: &BTreeMap<String, BTreeSet<String>>,
) -> AffectedSymbolCollection {
    let inputs = Pass1Inputs {
        dir,
        new_side_source,
        reexport_moves,
        derived_contract_changes,
        file_sections: diff::FileSections::split(diff_input),
        single_in_place_file: matches!(diff_files, [only] if only.old_path == only.new_path),
    };
    let mut file_contexts = Vec::new();
    let mut registry = CrossFileSymbolRegistry::default();
    for df in diff_files {
        let Some(analyzed) = analyze_changed_file(&inputs, df) else {
            continue;
        };
        registry.register(analyzed.registrations);
        file_contexts.push(analyzed.context);
    }
    revoke_value_only_constants_with_changed_types(&mut file_contexts);
    AffectedSymbolCollection {
        file_contexts,
        search_names: registry.names,
        method_parent_types: registry.method_parent_types,
        included_count: registry.included.len(),
    }
}

/// 変更ファイル 1 つを解析する。読めない・構文解析できないファイルは None (解析しない)。
fn analyze_changed_file(inputs: &Pass1Inputs<'_>, df: &DiffFile) -> Option<AnalyzedChangedFile> {
    let &Pass1Inputs {
        dir,
        new_side_source,
        reexport_moves,
        derived_contract_changes,
        ref file_sections,
        single_in_place_file,
    } = inputs;
    // 作業ツリーから読むときは実体の存在とワークスペース境界も確かめる。`--staged` は
    // index から読むので、作業ツリーで消した (移した) ファイルも解析する (`accessible`)。
    if !new_side_source.accessible(dir, &df.new_path) {
        return None;
    }
    let file_path = dir.join(&df.new_path);

    log_phase(
        &format!("context.pass1.file path={}", df.new_path),
        "start",
        0,
    );

    let t = std::time::Instant::now();
    let utf8_path = Utf8Path::new(file_path.to_str().unwrap_or(""));
    let source = new_side_source.read(&file_path, &df.new_path)?;
    log_phase("context.pass1.read_file", "end", t.elapsed().as_millis());
    let file_diff = file_sections.get(&df.new_path);
    let file_diff: &str = &file_diff;

    let t = std::time::Instant::now();
    let (tree, lang_id) = parser::parse_file(utf8_path, &source).ok()?;
    let root = tree.root_node();
    log_phase("context.pass1.parse", "end", t.elapsed().as_millis());

    let t = std::time::Instant::now();
    let syms = symbols::extract_symbols(root, &source, lang_id).unwrap_or_default();
    log_phase(
        &format!("context.pass1.extract_symbols n={}", syms.len()),
        "end",
        t.elapsed().as_millis(),
    );

    let t = std::time::Instant::now();
    // diff の `+` 行 (実変更行) を抽出して hunk context-only overlap を除外する。
    // 隣接 hunk の context 3 行に巻き込まれた本体未変更 export
    // (Issue 2026-05-14-private-const-and-unchanged-export-noise) を排除する。
    let changed_line_facts = diff::extract_changed_line_facts(file_diff, &df.new_path);
    let changed_new_lines = &changed_line_facts.added_lines;
    let eol_unchanged = if lang_id == LangId::Rust {
        rust_line_endings::unchanged_symbols(
            file_diff,
            &df.old_path,
            &df.new_path,
            root,
            &source,
            &syms,
        )
    } else {
        HashSet::new()
    };
    // symbols には method の container 解決用の合成 impl owner が含まれる。
    // メンバーだけの変更では owner を影響の起点にしないが、impl ヘッダ変更や
    // メンバー削除は情報を失わないよう "impl" として残す。
    let impact_syms: Cow<'_, [Symbol]> = if lang_id == LangId::Rust {
        Cow::Owned(
            syms.iter()
                .filter(|sym| {
                    !classify_symbol_change(sym, &df.hunks, Some(&changed_line_facts)).is_some_and(
                        |change_type| {
                            rust_impl_owner::is_member_only_change(
                                root,
                                &source,
                                sym,
                                change_type,
                                &changed_line_facts,
                            )
                        },
                    )
                })
                .cloned()
                .collect(),
        )
    } else {
        Cow::Borrowed(&syms)
    };
    let affected_raw: Vec<_> = impact_syms
        .iter()
        .filter(|sym| !eol_unchanged.contains(&(sym.name.as_str(), symbol_kind_str(sym.kind))))
        .filter_map(|sym| {
            classify_symbol_change(sym, &df.hunks, Some(&changed_line_facts)).map(|change_type| {
                (
                    sym,
                    AffectedSymbol {
                        change_evidence: None,
                        name: sym.name.clone(),
                        kind: if lang_id == LangId::Rust
                            && rust_impl_owner::is_impl_owner(root, sym)
                        {
                            "impl"
                        } else {
                            symbol_kind_str(sym.kind)
                        }
                        .to_string(),
                        change_type: change_type.to_string(),
                    },
                )
            })
        })
        .collect();
    log_phase(
        &format!("context.pass1.find_affected n={}", affected_raw.len()),
        "end",
        t.elapsed().as_millis(),
    );

    // テストシンボルとローカルスコープ変数を affected から除外。
    // ローカル変数（関数内 const/let 等）はファイル外への影響を持たないため、
    // affected_symbols 出力と cross-file 伝播の両方からノイズを除去する。
    //
    // 判定は affected を作った元のシンボル (`original`) で行い、以降の cross-file
    // 収録判定と親型の解決にも同じ組を持ち回る。名前で「hunk に重なる最初の同名
    // シンボル」を引き直すと、直前にある同名のローカル const・入れ子の関数・非公開
    // メソッドを対象に判定し、本来の公開関数を落とす (全言語で起きる)。
    let t = std::time::Instant::now();
    let (mut originals, mut affected): (Vec<&Symbol>, Vec<AffectedSymbol>) = affected_raw
        .into_iter()
        .filter(|(original, _)| {
            !(is_in_test_context(root, &source, &original.range, lang_id, &df.new_path)
                || symbols::is_local_scope_symbol(root, &source, lang_id, &original.range))
        })
        .unzip();
    log_phase(
        &format!("context.pass1.filter n={}", affected.len()),
        "end",
        t.elapsed().as_millis(),
    );

    let syntax_evidence = crate::engine::change_evidence::ChangeEvidenceIndex::from_diff(
        file_diff,
        &df.new_path,
        root,
        &source,
        lang_id,
    );
    // 他ファイルからの移動を追加と誤認しないよう、単一の同一パス差分に限る。
    if single_in_place_file {
        for symbol in &mut affected {
            if symbol.change_type == "modified"
                && syntax_evidence.proves_added(&symbol.name, &symbol.kind, lang_id)
            {
                symbol.change_type = "added".to_owned();
            }
        }
    }
    let t = std::time::Instant::now();
    let mut sig_changes = detect_signature_changes(file_diff, &df.new_path, &affected, lang_id);
    // 複数行ヘッダや宣言移動の差分は、旧側 AST と突き合わせて補う。
    let declaration_proof = declaration::reconcile_declaration_changes(
        &declaration::DeclarationChangeInput {
            file_diff,
            file_path: &df.new_path,
            old_file_path: &df.old_path,
            syms: &syms,
            hunks: &df.hunks,
            root,
            source: &source,
            lang_id,
            facts: &changed_line_facts,
            reexport_moves,
        },
        &mut affected,
        &mut sig_changes,
    );
    if lang_id == LangId::Rust {
        let equivalent_names = crate::engine::rust_cfg::exclusive_const_names(root, &source);
        if !equivalent_names.is_empty() {
            let mut seen = HashSet::new();
            // `originals` は affected と同じ並びを保つ (retain は元の順に 1 度ずつ訪れる)。
            let mut index = 0;
            let mut kept_originals = Vec::with_capacity(originals.len());
            affected.retain(|symbol| {
                let keep = symbol.kind != "constant"
                    || !equivalent_names.contains(&symbol.name)
                    || seen.insert((symbol.name.clone(), symbol.change_type.clone()));
                if keep {
                    kept_originals.push(originals[index]);
                }
                index += 1;
                keep
            });
            originals = kept_originals;
        }
    }
    let derived_declarations = if matches!(lang_id, LangId::Typescript | LangId::Tsx)
        && let Some(names) = derived_contract_changes.get(&df.new_path)
    {
        add_derived_contract_declarations(
            names,
            DerivedContractFile {
                root,
                source: &source,
                lang_id,
                path: &df.new_path,
                syms: &syms,
            },
            &mut originals,
            &mut affected,
        )
    } else {
        HashSet::new()
    };
    debug_assert_eq!(affected.len(), originals.len());
    // 透過的な型別名の導入と、関数に `const` を足しただけのシグネチャ変更は互換。
    let compatible_signature_keys: HashSet<String> = if lang_id == LangId::Rust {
        signature::compatible_rust_alias_signature_names(
            file_diff,
            &df.new_path,
            root,
            &source,
            &affected,
            &sig_changes,
        )
        .into_iter()
        .chain(
            sig_changes
                .iter()
                .filter(|change| {
                    declaration_proof
                        .rust_const_additions
                        .contains(&change.name)
                })
                .map(|change| change.name.clone()),
        )
        .map(|name| ci_key(lang_id, &name))
        .collect()
    } else {
        HashSet::new()
    };
    log_phase(
        &format!(
            "context.pass1.detect_sig n={} compatible={}",
            sig_changes.len(),
            compatible_signature_keys.len()
        ),
        "end",
        t.elapsed().as_millis(),
    );

    let t = std::time::Instant::now();
    let call_edges = calls::extract_calls(root, &source, lang_id, None).unwrap_or_default();
    log_phase(
        &format!("context.pass1.extract_calls n={}", call_edges.len()),
        "end",
        t.elapsed().as_millis(),
    );

    let (rust_private_root_names, rust_private_method_owners) = if lang_id == LangId::Rust {
        let old =
            collect_old_rust_private_symbols(file_diff, &df.new_path, &source).unwrap_or_default();
        rust_private_symbols_of_changes(&old, df, root, &source, &syms, &affected)
    } else {
        (HashSet::new(), HashMap::new())
    };

    // 判定材料のうち sym 以外は本ファイルに固定なので、コンテキストは
    // ループの外で 1 度だけ組み立てる。
    let cross_file_filter = filters::CrossFileFilterContext {
        sig_changes: &sig_changes,
        rust_kind_changes: &declaration_proof.rust_kind_changes,
        rust_alias_changes: &declaration_proof.rust_alias_changes,
        rust_value_only_constants: &declaration_proof.rust_value_only_constants,
        rust_private_method_owners: &rust_private_method_owners,
        diff_input: file_diff,
        file_path: &df.new_path,
        root,
        source: &source,
        lang_id,
        changed_new_lines,
        deletion_gaps: &changed_line_facts.deletion_gaps,
    };
    // 収録判定と親型は affected の元のシンボルごとに求める (affected と同じ並び)。
    // 外側の `None` は cross-file 検索に載せないシンボル、`Some(親型)` は載せるシンボル
    // (親型の無いシンボルは `Some(None)`)。
    let included_parents: Vec<Option<Option<String>>> = affected
        .iter()
        .zip(&originals)
        .map(|(sym, original)| {
            (derived_declarations.contains(&declaration_key(original))
                || cross_file_filter.should_include_for_cross_file(sym, original))
            .then(|| {
                find_parent_type_name(root, &source, &original.range, lang_id)
                    .map(|parent| ci_key(lang_id, &parent))
            })
        })
        .collect();
    let (cross_file_symbol_keys, registrations) =
        cross_file_registrations(lang_id, &affected, &included_parents);

    let hunks = df
        .hunks
        .iter()
        .map(|h| HunkInfo {
            old_start: h.old_start,
            old_count: h.old_count,
            new_start: h.new_start,
            new_count: h.new_count,
        })
        .collect();

    let mut affected_name_by_cikey: HashMap<String, String> = HashMap::new();
    for a in &affected {
        affected_name_by_cikey
            .entry(ci_key(lang_id, &a.name))
            .or_insert_with(|| a.name.clone());
    }

    let object_member_changes = object_members::collect_object_member_changes(
        file_diff,
        &df.new_path,
        root,
        &source,
        lang_id,
        &syms,
        &affected,
    );
    let jsx_prop_changes = jsx_props::collect(
        file_diff,
        &df.new_path,
        root,
        &source,
        lang_id,
        &syms,
        &affected,
    );
    let rust_private_widening_names = if lang_id == LangId::Rust && single_in_place_file {
        rust_visibility_scope::private_widening_names(
            file_diff,
            &df.new_path,
            &source,
            root,
            &affected,
        )
    } else {
        HashSet::new()
    };
    let rust_explicit_widenings = if lang_id == LangId::Rust && single_in_place_file {
        rust_visibility_scope::explicit_widenings(file_diff, &df.new_path, &source, root, &affected)
    } else {
        HashMap::new()
    };
    for symbol in &mut affected {
        if symbol.change_type == "modified" {
            symbol.change_evidence = Some(syntax_evidence.get(&symbol.name, &symbol.kind, lang_id));
        }
    }
    Some(AnalyzedChangedFile {
        context: FileContext {
            new_path: df.new_path.clone(),
            lang_id,
            js_modified_direct_function_names: declaration_proof.js_modified_direct_function_names,
            python_modified_direct_function_names: declaration_proof
                .python_modified_direct_function_names,
            rust_function_names: declaration_proof.rust_function_names,
            rust_free_function_names: declaration_proof.rust_free_function_names,
            rust_kind_changes: declaration_proof.rust_kind_changes,
            rust_private_root_names,
            rust_private_method_owners,
            rust_private_widening_names,
            rust_explicit_widenings,
            rust_value_only_constants: declaration_proof
                .rust_value_only_constants
                .into_iter()
                .map(|(name, constant)| (ci_key(lang_id, &name), constant))
                .collect(),
            affected,
            sig_changes,
            compatible_signature_keys,
            hunks,
            call_edges,
            cross_file_symbol_keys,
            affected_name_by_cikey,
            object_member_changes,
            jsx_prop_changes,
        },
        registrations,
    })
}

/// 宣言の位置 (始まりと終わりの行・列)。同じ宣言を指すかの比較に使う。
fn declaration_key(symbol: &Symbol) -> (usize, usize, usize, usize) {
    (
        symbol.range.start.line,
        symbol.range.start.column,
        symbol.range.end.line,
        symbol.range.end.column,
    )
}

/// 派生した契約の変更を照らし合わせる、変更ファイル 1 つの構文木とシンボル。
struct DerivedContractFile<'a, 'tree> {
    root: tree_sitter::Node<'tree>,
    source: &'a [u8],
    lang_id: LangId,
    path: &'a str,
    syms: &'a [Symbol],
}

/// ヘッダの行が不変でも、非公開型の変更で公開契約が変わった TypeScript の宣言 (`names` は
/// `container.name` の qualname) を affected に加え、収録を決めた宣言の位置を返す。実 AST の
/// 宣言へ結び付け、以降の言語・所有元・参照の判定は通常と共有する。ローカルとテストの宣言は外す。
fn add_derived_contract_declarations<'s>(
    names: &BTreeSet<String>,
    file: DerivedContractFile<'s, '_>,
    originals: &mut Vec<&'s Symbol>,
    affected: &mut Vec<AffectedSymbol>,
) -> HashSet<(usize, usize, usize, usize)> {
    let DerivedContractFile {
        root,
        source,
        lang_id,
        path,
        syms,
    } = file;
    let mut derived = HashSet::new();
    for original in syms {
        let qualified = original.container.as_ref().map_or_else(
            || original.name.clone(),
            |container| format!("{container}.{}", original.name),
        );
        if !names.contains(&qualified)
            || symbols::is_local_scope_symbol(root, source, lang_id, &original.range)
            || is_in_test_context(root, source, &original.range, lang_id, path)
        {
            continue;
        }
        derived.insert(declaration_key(original));
        if !originals
            .iter()
            .any(|previous| previous.range == original.range)
        {
            originals.push(original);
            affected.push(AffectedSymbol {
                change_evidence: None,
                name: original.name.clone(),
                kind: symbol_kind_str(original.kind).to_owned(),
                change_type: "modified".to_owned(),
            });
        }
    }
    derived
}

/// Rust の旧い版の、ファイル直下の非公開の宣言の名前と、非公開の inherent メソッドの所有元。
/// どちらも旧い版のファイルで名前が 1 つだけのもの (impl の所有者の合成シンボルは数えない)。
#[derive(Default)]
struct OldRustPrivateSymbols {
    root_names: HashSet<String>,
    method_owners: HashMap<String, String>,
}

/// 差分を new 側へ逆適用して旧い版を復元し、非公開の名前と所有元を集める。復元・構文解析・
/// シンボルの抽出のどれかができなければ None (呼び出し側は空として扱う)。
fn collect_old_rust_private_symbols(
    file_diff: &str,
    new_path: &str,
    source: &[u8],
) -> Option<OldRustPrivateSymbols> {
    let old = diff::reconstruct_old_source(file_diff, new_path, source)?;
    let tree = parser::parse_source(&old.source, LangId::Rust).ok()?;
    let old_root = tree.root_node();
    if old_root.has_error() {
        return None;
    }
    let old_syms = symbols::extract_symbols(old_root, &old.source, LangId::Rust).ok()?;
    let mut counts = HashMap::new();
    for symbol in &old_syms {
        if rust_impl_owner::is_impl_owner(old_root, symbol) {
            continue;
        }
        *counts.entry(symbol.name.as_str()).or_insert(0usize) += 1;
    }
    let mut root_names = HashSet::new();
    for symbol in &old_syms {
        if !rust_impl_owner::is_impl_owner(old_root, symbol)
            && counts.get(symbol.name.as_str()) == Some(&1)
            && declaration::is_rust_file_root_symbol(old_root, symbol)
            && !symbols::is_symbol_exported(
                old_root,
                &old.source,
                LangId::Rust,
                symbol.identity_range(),
            )
        {
            root_names.insert(symbol.name.clone());
        }
    }
    let method_owners: HashMap<String, String> = old_syms
        .iter()
        .filter(|symbol| counts.get(symbol.name.as_str()) == Some(&1))
        .filter_map(|symbol| {
            declaration::rust_private_inherent_method(old_root, &old.source, symbol)
                .map(|owner| (symbol.name.clone(), owner))
        })
        .collect();
    Some(OldRustPrivateSymbols {
        root_names,
        method_owners,
    })
}

/// 変更したシンボルのうち、旧新とも非公開のファイル直下の宣言 (名前が新しい版で 1 つだけ) の
/// 名前と、旧新で同じ型を所有元とする非公開の inherent メソッドの所有元。
fn rust_private_symbols_of_changes(
    old: &OldRustPrivateSymbols,
    df: &DiffFile,
    root: tree_sitter::Node<'_>,
    source: &[u8],
    syms: &[Symbol],
    affected: &[AffectedSymbol],
) -> (HashSet<String>, HashMap<String, String>) {
    let mut root_names = HashSet::new();
    let mut method_owners = HashMap::new();
    for change in affected {
        let mut named = syms.iter().filter(|symbol| {
            symbol.name == change.name && !rust_impl_owner::is_impl_owner(root, symbol)
        });
        if old.root_names.contains(&change.name)
            && let Some(symbol) = named.next()
            && named.next().is_none()
            && declaration::is_rust_file_root_symbol(root, symbol)
            && !symbols::is_symbol_exported(root, source, LangId::Rust, symbol.identity_range())
        {
            root_names.insert(change.name.clone());
        }
        let new_methods: Vec<_> = syms
            .iter()
            .filter(|symbol| symbol.name == change.name && symbol_overlaps_hunks(symbol, &df.hunks))
            .filter_map(|symbol| declaration::rust_private_inherent_method(root, source, symbol))
            .collect();
        if new_methods.len() == 1
            && syms
                .iter()
                .filter(|symbol| {
                    symbol.name == change.name && !rust_impl_owner::is_impl_owner(root, symbol)
                })
                .count()
                == 1
            && old.method_owners.get(&change.name) == Some(&new_methods[0])
        {
            method_owners.insert(change.name.clone(), new_methods[0].clone());
        }
    }
    (root_names, method_owners)
}

/// ファイルの収録シンボルの、このファイルの `FileContext` に cross-file 参照検索の結果を流し込んで
/// よい名前の集合と、全体へ登録する (名前, 合意した method の親型) の並び (affected の順)。
///
/// 収録は同名シンボルでも他の `FileContext` と独立に判定する (グローバルな登録済みの名前で
/// 省いてはいけない)。`method_parent_types` は名前だけを鍵にするので、同じファイルで同名の収録
/// シンボルの親型が 1 つに決まる場合だけ登録する。親型の無い同名シンボル (自由関数) や別の型の
/// 同名メソッドが混ざる場合は、どれかの親型で呼び出し元を絞ると取り違えるため登録しない。
fn cross_file_registrations(
    lang_id: LangId,
    affected: &[AffectedSymbol],
    included_parents: &[Option<Option<String>>],
) -> (HashSet<String>, Vec<(String, Option<String>)>) {
    let mut agreed_parents: HashMap<String, Option<String>> = HashMap::new();
    for (sym, parent) in affected.iter().zip(included_parents) {
        let Some(parent) = parent else {
            continue;
        };
        agreed_parents
            .entry(ci_key(lang_id, &sym.name))
            .and_modify(|agreed| {
                if agreed != parent {
                    *agreed = None;
                }
            })
            .or_insert_with(|| parent.clone());
    }
    let mut keys = HashSet::new();
    let mut registrations = Vec::new();
    for (sym, parent) in affected.iter().zip(included_parents) {
        if parent.is_none() {
            continue;
        }
        let key = ci_key(lang_id, &sym.name);
        keys.insert(key.clone());
        let agreed = agreed_parents.get(&key).cloned().flatten();
        registrations.push((key, agreed));
    }
    (keys, registrations)
}

/// 初期値だけの変更の証明 (`declaration::rust_value_only_constants`) は、定数と同じファイルの
/// 宣言を直接含むスコープしか比べない。型に現れる名前 (たどった名前を含む) と同じ名前の宣言を
/// 差分の Rust のファイルが変えていれば (`use crate::Note;` の先の `struct Note` のフィールド、
/// 同じファイルの別のスコープの `impl Sizes { const N }` など)、同じ字面の型でも意味が変わりうる
/// ので証明を取り消し、参照を通常の振り分けに戻す。定数自身の変更は数えない。全ファイルの
/// 収集の後に行う。
fn revoke_value_only_constants_with_changed_types(file_contexts: &mut [FileContext]) {
    if file_contexts
        .iter()
        .all(|context| context.rust_value_only_constants.is_empty())
    {
        return;
    }
    let changed_names: Vec<HashSet<String>> = file_contexts
        .iter()
        .map(|context| {
            if context.lang_id != LangId::Rust {
                return HashSet::new();
            }
            context
                .affected
                .iter()
                .map(|symbol| ci_key(LangId::Rust, &symbol.name))
                .collect()
        })
        .collect();
    for (ix, context) in file_contexts.iter_mut().enumerate() {
        context.rust_value_only_constants.retain(|key, constant| {
            !changed_names.iter().enumerate().any(|(other, names)| {
                constant.type_names.iter().any(|name| {
                    let name = ci_key(LangId::Rust, name);
                    (other != ix || name != *key) && names.contains(&name)
                })
            })
        });
    }
}
