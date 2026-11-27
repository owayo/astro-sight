//! Pass 1: 変更ファイルをパースし、シンボルを抽出し、cross-file 参照検索が必要なシンボル名を決定する。
//!
//! ファイルごとの解析 ([`analyze_changed_file`]) は構文木とソースをその中で手放し、完成した
//! [`FileContext`] と、全体の検索名へ登録する情報だけを返す。全ファイルにまたがる検索名の登録は
//! [`CrossFileSymbolRegistry`] が、ファイルの順・ファイルの中の affected の順に行う。

use std::borrow::Cow;
use std::cell::OnceCell;
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
use crate::engine::change_evidence::{MAX_EVIDENCE_SOURCE_BYTES, has_js_unicode_escape};
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
    diff_input: &'a str,
    diff_files: &'a [DiffFile],
    new_side_source: &'a NewSideSource,
    reexport_moves: &'a ReexportMoveIndex,
    derived_contract_changes: &'a BTreeMap<String, BTreeSet<String>>,
    /// ファイルごとの判定 (変更行の抽出・シグネチャ検出・cross-file フィルタ) は対象ファイルの
    /// 区間しか読まないので、diff 全体ではなく区間だけを渡す。diff 全体を渡すとファイル数 ×
    /// diff 長の 2 乗になる (`FileSections` の doc 参照)。結果は diff 全体を渡した場合と同一。
    file_sections: diff::FileSections<'a>,
    /// hunk を持つ変更が同一パスの 1 ファイルだけ。単一ファイル用の宣言の証明に使う。
    single_in_place_file: bool,
    /// JS 系の旧側監査は、追加候補を見つけたときに一度だけ実行する。
    old_js_declarations: OnceCell<Option<OldJsDeclarations>>,
}

#[derive(Default)]
struct OldJsDeclarations {
    names: HashSet<(String, &'static str)>,
    opaque_exports: HashSet<String>,
}

impl Pass1Inputs<'_> {
    fn has_no_matching_old_js_declaration(&self, name: &str, kind: &'static str) -> bool {
        self.old_js_declarations
            .get_or_init(|| collect_old_js_declarations(self))
            .as_ref()
            .is_some_and(|declarations| !declarations.names.contains(&(name.to_owned(), kind)))
    }
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
        diff_input,
        diff_files,
        new_side_source,
        reexport_moves,
        derived_contract_changes,
        file_sections: diff::FileSections::split(diff_input),
        single_in_place_file: matches!(diff_files, [only] if only.old_path == only.new_path),
        old_js_declarations: OnceCell::new(),
    };
    let mut file_contexts = Vec::new();
    let mut registry = CrossFileSymbolRegistry::default();
    let mut const_exports = crate::commands::ts_const_rebinding::AddedExports::default();
    for df in diff_files {
        let Some(analyzed) = analyze_changed_file(&inputs, df, &mut const_exports) else {
            continue;
        };
        registry.register(analyzed.registrations);
        file_contexts.push(analyzed.context);
    }
    if matches!(new_side_source, NewSideSource::WorkingTree) {
        let mut callers =
            const_exports.rebound_callers(dir, diff_files, &inputs.file_sections, |path| {
                new_side_source.read_checked(dir, path)
            });
        for context in &mut file_contexts {
            if let Some(rebound) = callers.remove(&context.new_path) {
                context.rebound_const_callers = rebound;
            }
        }
    }
    revoke_value_only_constants_with_changed_types(&mut file_contexts);
    AffectedSymbolCollection {
        file_contexts,
        search_names: registry.names,
        method_parent_types: registry.method_parent_types,
        included_count: registry.included.len(),
    }
}

fn is_js_family(lang: LangId) -> bool {
    matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx)
}

/// 証明が必要になったときだけ旧側を走査する。構文木はファイルごとに手放す。
fn collect_old_js_declarations(inputs: &Pass1Inputs<'_>) -> Option<OldJsDeclarations> {
    if !complete_text_diff(inputs.diff_input, inputs.diff_files.len()) {
        return None;
    }
    let mut declarations = OldJsDeclarations::default();
    // 単一の同一パス差分は、このファイルの構文証拠だけで足りる。
    if inputs.single_in_place_file {
        return Some(declarations);
    }
    for file in inputs.diff_files {
        if file.old_path == "/dev/null" {
            if file
                .hunks
                .iter()
                .any(|hunk| hunk.old_count != 0 || hunk.old_start != 0)
            {
                return None;
            }
            continue;
        }
        if !super::is_safe_diff_path(&file.old_path)
            || (file.new_path != "/dev/null" && !super::is_safe_diff_path(&file.new_path))
        {
            return None;
        }
        let lang = LangId::from_path(Utf8Path::new(&file.old_path)).ok();
        if lang.is_some_and(|lang| !is_js_family(lang)) {
            continue;
        }
        let old = old_source_for_js_declarations(inputs, file)?;
        let Some(lang) = lang
            .or_else(|| LangId::from_shebang_source(&old))
            .filter(|lang| is_js_family(*lang))
        else {
            continue;
        };
        if old.len() > MAX_EVIDENCE_SOURCE_BYTES
            || std::str::from_utf8(&old).is_err()
            || has_js_unicode_escape(&old)
        {
            return None;
        }
        let tree = parser::parse_source(&old, lang).ok()?;
        let root = tree.root_node();
        if root.has_error() {
            return None;
        }
        let symbols = symbols::extract_symbols_without_complexity(root, &old, lang).ok()?;
        if !crate::commands::ts_const_rebinding::has_direct_exports_only(root) {
            declarations.opaque_exports.insert(file.old_path.clone());
        }
        declarations.names.extend(
            symbols
                .into_iter()
                .map(|symbol| (ci_key(lang, &symbol.name), symbol_kind_str(symbol.kind))),
        );
    }
    Some(declarations)
}

fn old_source_for_js_declarations<'a>(
    inputs: &Pass1Inputs<'_>,
    file: &'a DiffFile,
) -> Option<Cow<'a, [u8]>> {
    if file.new_path == "/dev/null" {
        let [hunk] = file.hunks.as_slice() else {
            return None;
        };
        let source = file.deleted_old_source.as_deref()?;
        return (hunk.old_start == 1
            && hunk.new_start == 0
            && hunk.new_count == 0
            && memchr::memchr_iter(b'\n', source).count() == hunk.old_count)
            .then_some(Cow::Borrowed(source));
    }
    let new = inputs
        .new_side_source
        .read_checked(inputs.dir, &file.new_path)?;
    if new.len() > MAX_EVIDENCE_SOURCE_BYTES {
        return None;
    }
    let section = inputs.file_sections.get(&file.new_path);
    diff::reconstruct_old_source(&section, &file.new_path, &new).map(|old| Cow::Owned(old.source))
}

/// hunk が無い移動、読めないヘッダ、不完全なパッチを旧宣言の不在とみなさない。
fn complete_text_diff(input: &str, file_count: usize) -> bool {
    let mut blocks = 0;
    let mut old_headers = 0;
    let mut new_headers = 0;
    let mut has_move_metadata = false;
    let mut lines = diff::DiffLines::new(input);
    while let Some(line) = lines.next() {
        match line {
            diff::DiffLine::OldHeader(_) => old_headers += 1,
            diff::DiffLine::NewHeader(_) => new_headers += 1,
            diff::DiffLine::Other => {
                let text = &input[lines.line_start()..];
                if text.starts_with("diff --git ") {
                    blocks += 1;
                    continue;
                }
                let old_mode = text.lines().next().and_then(|line| {
                    let metadata = line
                        .strip_prefix("index ")
                        .or_else(|| line.strip_prefix("deleted file mode "))?;
                    metadata.split_ascii_whitespace().last()
                });
                if matches!(old_mode, Some("120000" | "160000"))
                    || [
                        "diff --",
                        "@@",
                        "Binary files ",
                        "GIT binary patch",
                        "old mode ",
                        "new mode ",
                    ]
                    .iter()
                    .any(|prefix| text.starts_with(prefix))
                {
                    return false;
                }
                if text.starts_with("rename from ") || text.starts_with("copy from ") {
                    has_move_metadata = true;
                }
            }
            _ => {}
        }
    }
    old_headers == file_count
        && new_headers == file_count
        && (blocks == file_count || (blocks == 0 && !has_move_metadata))
        && !lines.in_hunk()
        && !lines.malformed_body()
}

/// 変更ファイル 1 つを解析する。読めない・構文解析できないファイルは None (解析しない)。
fn analyze_changed_file(
    inputs: &Pass1Inputs<'_>,
    df: &DiffFile,
    const_exports: &mut crate::commands::ts_const_rebinding::AddedExports,
) -> Option<AnalyzedChangedFile> {
    let &Pass1Inputs {
        dir,
        new_side_source,
        reexport_moves,
        derived_contract_changes,
        ref file_sections,
        single_in_place_file,
        ..
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
    let declaration_input = declaration::DeclarationChangeInput {
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
    };
    let old_side = declaration::LazyOldSide::default();
    let foreign = declaration::ForeignChanges::collect(&declaration_input, &old_side);
    let start =
        |sym: &Symbol| tree_sitter::Point::new(sym.range.start.line, sym.range.start.column);
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
        .filter_map(|sym| {
            let pos = start(sym);
            let classification = if foreign.changed.contains_key(&pos) {
                Some("modified")
            } else if foreign.unchanged.contains(&pos)
                || eol_unchanged.contains(&(sym.name.as_str(), symbol_kind_str(sym.kind)))
            {
                None
            } else {
                classify_symbol_change(sym, &df.hunks, Some(&changed_line_facts))
            };
            classification.map(|change_type| {
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
    let js_in_place = df.old_path == df.new_path && is_js_family(lang_id);
    if single_in_place_file || js_in_place {
        for (symbol, original) in affected.iter_mut().zip(&originals) {
            if symbol.change_type == "modified"
                && syntax_evidence.proves_added(&symbol.name, &symbol.kind, lang_id)
            {
                if !js_in_place
                    || inputs.has_no_matching_old_js_declaration(
                        &symbol.name,
                        symbol_kind_str(original.kind),
                    )
                {
                    symbol.change_type = "added".to_owned();
                } else if matches!(lang_id, LangId::Typescript | LangId::Tsx)
                    && symbol.kind == "variable"
                    && inputs
                        .old_js_declarations
                        .get()
                        .and_then(Option::as_ref)
                        .is_some_and(|old| !old.opaque_exports.contains(&df.old_path))
                {
                    const_exports.record(&df.new_path, &symbol.name, root, &source);
                }
            }
        }
    }
    let t = std::time::Instant::now();
    let mut sig_changes = detect_signature_changes(file_diff, &df.new_path, &affected, lang_id);
    // 複数行ヘッダや宣言移動の差分は、旧側 AST と突き合わせて補う。
    let declaration_proof = declaration::reconcile_declaration_changes(
        old_side,
        &declaration_input,
        &mut affected,
        &mut sig_changes,
    );
    if !foreign.changed.is_empty() {
        // 行比較の結果があっても、外側の ABI を含めた宣言を表示する。
        let mut replaced: HashSet<&str> = foreign
            .changed
            .values()
            .map(|change| change.name.as_str())
            .collect();
        for original in &originals {
            if !foreign.changed.contains_key(&start(original)) {
                replaced.remove(original.name.as_str());
            }
        }
        sig_changes.retain(|prior| !replaced.contains(prior.name.as_str()));
        for (original, affected) in originals.iter().zip(&mut affected) {
            if let Some(change) = foreign.changed.get(&start(original)) {
                affected.change_type = "modified".to_owned();
                sig_changes.push(change.clone());
            }
        }
    }
    // 同名の別クラスのメソッドを、行比較による旧宣言との対応付けに混ぜない。
    // 宣言比較の後に、元の所有型で証明した新規追加だけを確定する。
    if lang_id == LangId::Python && single_in_place_file {
        for (symbol, original) in affected.iter_mut().zip(&originals) {
            let name = original.container.as_ref().map_or_else(
                || original.name.clone(),
                |owner| format!("{owner}.{}", original.name),
            );
            if symbol.change_type == "modified"
                && syntax_evidence.proves_added(&name, &symbol.kind, lang_id)
            {
                symbol.change_type = "added".to_owned();
            }
        }
    }
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

    let in_place = df.old_path == df.new_path && !root.has_error();
    let old = (lang_id == LangId::Rust)
        .then(|| collect_old_rust_private_symbols(file_diff, &df.new_path, &source, in_place))
        .flatten();
    let new_bindings = old
        .as_ref()
        .filter(|old| old.closed_root)
        .and_then(|_| ordinary_private_bindings(root, &source));
    let (rust_private_types, rust_inherent_method_owners, rust_owner_derive) = old
        .as_ref()
        .zip(new_bindings)
        .map(|(old, mut bindings)| {
            bindings.variants.extend(old.variant_names.iter().cloned());
            let private = private_nominal_changes(old, root, &source, &syms, &affected, &bindings);
            let mut owners =
                rust_inherent_method_owners(old, root, &source, &syms, &affected, &bindings);
            let derive = old.derive || bindings.derive;
            if derive {
                // Debug の derive が生成する trait メソッドも同名の宣言になり得る。
                owners.remove("fmt");
            }
            (private, owners, derive)
        })
        .unwrap_or_default();
    let (rust_private_root_names, rust_private_method_owners) = old
        .as_ref()
        .map(|old| rust_private_symbols_of_changes(old, df, root, &source, &syms, &affected))
        .unwrap_or_default();
    let rust_private_root_functions = old
        .as_ref()
        .filter(|_| in_place)
        .map(|old| private_root_functions(old, &rust_private_root_names, root, &source, &syms))
        .unwrap_or_default();

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
            rebound_const_callers: HashMap::new(),
            new_path: df.new_path.clone(),
            lang_id,
            js_modified_direct_function_names: declaration_proof.js_modified_direct_function_names,
            python_modified_direct_function_names: declaration_proof
                .python_modified_direct_function_names,
            rust_function_names: declaration_proof.rust_function_names,
            rust_free_function_names: declaration_proof.rust_free_function_names,
            rust_super_functions: declaration_proof.rust_super_functions,
            rust_receiverless_associated_names: declaration_proof
                .rust_receiverless_associated_names,
            rust_kind_changes: declaration_proof.rust_kind_changes,
            rust_private_root_names,
            rust_private_root_functions,
            rust_private_method_owners,
            rust_inherent_method_owners,
            rust_private_types,
            rust_owner_derive,
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
    explicit_names: HashSet<String>,
    closed_root: bool,
    derive: bool,
    variant_names: HashSet<String>,
    root_names: HashSet<String>,
    /// `root_names` のうち、名前を束縛し直さない組み込みの属性だけの関数と、その属性の綴り
    /// ([`plain_root_function_attributes`])。
    plain_functions: HashMap<String, Vec<String>>,
    method_owners: HashMap<String, declaration::PrivateMethodOwner>,
    /// 旧い版の名前ごとの inherent メソッドの所有型 ([`rust_inherent_owners_by_name`])。
    inherent_owners: HashMap<String, Option<BTreeSet<String>>>,
}

/// 名前 (`r#` を外して正規化) ごとの、inherent メソッドの所有型の名前の集合。その名前の宣言に
/// inherent メソッドでないもの (自由関数、trait の宣言・実装のメソッド、型、定数など) が 1 つでも
/// あれば `None`。ブロック内の impl もブロック外から呼べるので、入れ子の同名宣言も保留する。
fn rust_inherent_owners_by_name(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    syms: &[Symbol],
    nominal: &HashSet<&str>,
) -> HashMap<String, Option<BTreeSet<String>>> {
    let mut owners_by_name: HashMap<String, Option<BTreeSet<String>>> = HashMap::new();
    for symbol in syms {
        let owner = declaration::rust_inherent_method_owner(root, source, symbol)
            .filter(|name| nominal.contains(name));
        let slot = owners_by_name
            .entry(ci_key(LangId::Rust, &symbol.name))
            .or_insert_with(|| Some(BTreeSet::new()));
        match (slot.as_mut(), owner) {
            (Some(owners), Some(owner)) => {
                owners.insert(ci_key(LangId::Rust, owner));
            }
            _ => *slot = None,
        }
    }
    owners_by_name
}

/// 変更したメソッドの名前ごとの、inherent メソッドの所有型の名前 (旧新の和集合、昇順)。
///
/// 旧い版と新しい版のどちらでも、その名前の宣言がすべて inherent メソッドのときだけ持つ。自由関数や
/// trait のメソッドが同じ名前にあれば、所有型と違う修飾のパスがそれを指し得るので持たない。
/// 旧い版の所有型も入れるのは、impl の所有型を書き換えた変更で、旧い所有型のパスの呼び出しを
/// 残すため。
fn rust_inherent_method_owners(
    old: &OldRustPrivateSymbols,
    root: tree_sitter::Node<'_>,
    source: &[u8],
    syms: &[Symbol],
    affected: &[AffectedSymbol],
    bindings: &RootBindings<'_>,
) -> HashMap<String, Vec<String>> {
    let new_owners = rust_inherent_owners_by_name(root, source, syms, &bindings.nominal);
    let mut result = HashMap::new();
    for change in affected {
        if !matches!(change.kind.as_str(), "function" | "method") {
            continue;
        }
        let key = ci_key(LangId::Rust, &change.name);
        if result.contains_key(&key) || bindings.variants.contains(&key) {
            continue;
        }
        let Some(Some(new)) = new_owners.get(&key) else {
            continue;
        };
        let mut owners = new.clone();
        match old.inherent_owners.get(&key) {
            Some(Some(old)) => owners.extend(old.iter().cloned()),
            Some(None) => continue,
            None => {}
        }
        if !owners.is_empty() {
            result.insert(key, owners.into_iter().collect());
        }
    }
    result
}

/// 差分を new 側へ逆適用して旧い版を復元し、非公開の名前と所有元を集める。復元・構文解析・
/// シンボルの抽出のどれかができなければ None (呼び出し側は空として扱う)。
fn collect_old_rust_private_symbols(
    file_diff: &str,
    new_path: &str,
    source: &[u8],
    in_place: bool,
) -> Option<OldRustPrivateSymbols> {
    let old = diff::reconstruct_old_source(file_diff, new_path, source)?;
    let tree = parser::parse_source(&old.source, LangId::Rust).ok()?;
    let old_root = tree.root_node();
    if old_root.has_error() {
        return None;
    }
    let old_syms = symbols::extract_symbols(old_root, &old.source, LangId::Rust).ok()?;
    let mut counts = HashMap::new();
    // 非公開メソッドの所有型は、綴りごとに数えて一意なら引く (`fn translate` のメソッドと
    // `fn r#translate` の自由関数が並んでも、メソッドの呼び出しを探す)。
    let mut spelling_counts = HashMap::new();
    for symbol in &old_syms {
        if rust_impl_owner::is_impl_owner(old_root, symbol) {
            continue;
        }
        // refs と同じく `r#` を外して数える (`translate` と `r#translate` は同じ名前の参照になる)。
        *counts
            .entry(ci_key(LangId::Rust, &symbol.name))
            .or_insert(0usize) += 1;
        *spelling_counts
            .entry(symbol.name.as_str())
            .or_insert(0usize) += 1;
    }
    let mut root_names = HashSet::new();
    let mut plain_functions = HashMap::new();
    // 同じ名前の宣言の数は、旧版の AST を 1 回だけたどって数える (非公開の関数ごとにたどらない)。
    let old_declarations = std::cell::OnceCell::<NamedDeclarations>::new();
    for symbol in &old_syms {
        if !rust_impl_owner::is_impl_owner(old_root, symbol)
            && counts.get(&ci_key(LangId::Rust, &symbol.name)) == Some(&1)
            && declaration::is_rust_file_root_symbol(old_root, symbol)
            && !symbols::is_symbol_exported(
                old_root,
                &old.source,
                LangId::Rust,
                symbol.identity_range(),
            )
        {
            root_names.insert(symbol.name.clone());
            if let Some(attributes) = super::descendant_for_range(old_root, &symbol.range)
                .and_then(|node| plain_root_function_attributes(old_root, &old.source, node))
                && old_declarations
                    .get_or_init(|| NamedDeclarations::count(old_root, &old.source))
                    .get(&ci_key(LangId::Rust, &symbol.name))
                    == 1
            {
                plain_functions.insert(symbol.name.clone(), attributes);
            }
        }
    }
    let method_owners: HashMap<String, declaration::PrivateMethodOwner> = old_syms
        .iter()
        .filter(|symbol| spelling_counts.get(symbol.name.as_str()) == Some(&1))
        .filter_map(|symbol| {
            declaration::rust_private_inherent_method(old_root, &old.source, symbol)
                .map(|owner| (symbol.name.clone(), owner))
        })
        .collect();
    let mut old_symbols = OldRustPrivateSymbols {
        root_names,
        plain_functions,
        method_owners,
        ..Default::default()
    };
    if in_place && let Some(bindings) = ordinary_private_bindings(old_root, &old.source) {
        old_symbols.derive = bindings.derive;
        old_symbols.variant_names = bindings.variants;
        old_symbols.closed_root = true;
        old_symbols.explicit_names = counts
            .keys()
            .map(|name| ci_key(LangId::Rust, name))
            .collect();
        old_symbols.inherent_owners =
            rust_inherent_owners_by_name(old_root, &old.source, &old_syms, &bindings.nominal);
    }
    Some(old_symbols)
}

struct RootBindings<'a> {
    nominal: HashSet<&'a str>,
    variants: HashSet<String>,
    derive: bool,
}

/// 非公開の glob は外部向けの名前を作らない。公開の再エクスポートや項目を生成する構文は保留する。
fn ordinary_private_bindings<'a>(
    root: tree_sitter::Node<'_>,
    source: &'a [u8],
) -> Option<RootBindings<'a>> {
    if (memchr::memmem::find(source, b"path").is_some()
        || memchr::memmem::find(source, b"include").is_some())
        && crate::engine::rust_dynamic_source::tree_includes_external_source(root, source)
    {
        return None;
    }
    let mut cursor = root.walk();
    let mut nominal = HashSet::new();
    let mut derive = false;
    let mut unconditional = true;
    for item in root.named_children(&mut cursor) {
        match item.kind() {
            "attribute_item" | "inner_attribute_item" => {
                let names = super::rust_std_origin::safe_attribute(item, source)?;
                if names == ["Debug", "derive"] {
                    derive = true;
                } else if names.is_empty() {
                    unconditional = false;
                } else {
                    return None;
                }
                continue;
            }
            "line_comment" | "block_comment" => continue,
            "use_declaration" => {
                let mut children = item.walk();
                if item
                    .named_children(&mut children)
                    .any(|child| child.kind() == "visibility_modifier")
                {
                    return None;
                }
            }
            "struct_item" | "enum_item" | "union_item" => {
                if unconditional {
                    nominal.insert(item.child_by_field_name("name")?.utf8_text(source).ok()?);
                }
            }
            "impl_item" | "function_item" | "type_item" | "const_item" | "static_item"
            | "trait_item" | "mod_item" => {}
            _ => return None,
        }
        unconditional = true;
    }
    Some(RootBindings {
        nominal,
        variants: variant_names(root, source),
        derive,
    })
}

fn variant_names(root: tree_sitter::Node<'_>, source: &[u8]) -> HashSet<String> {
    use crate::engine::tree_walk::{Children, Walk, walk_preorder};
    let mut names = HashSet::new();
    let _: std::ops::ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
        if node.kind() == "enum_variant"
            && let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(source).ok())
        {
            names.insert(ci_key(LangId::Rust, name));
        }
        std::ops::ControlFlow::Continue(Walk::Descend)
    });
    names
}

/// 新側の一意な非公開型と、旧側で同名の外部向け宣言が無かったことを照合する。
fn private_nominal_changes(
    old: &OldRustPrivateSymbols,
    root: tree_sitter::Node<'_>,
    source: &[u8],
    symbols: &[Symbol],
    affected: &[AffectedSymbol],
    bindings: &RootBindings<'_>,
) -> HashSet<String> {
    let mut private = HashSet::new();
    for change in affected
        .iter()
        .filter(|change| matches!(change.kind.as_str(), "struct" | "enum" | "union"))
    {
        let key = ci_key(LangId::Rust, &change.name);
        if bindings.variants.contains(&key) {
            continue;
        }
        if old.explicit_names.contains(&key) && !old.root_names.contains(&change.name) {
            continue;
        }
        let mut named = symbols.iter().filter(|sym| {
            ci_key(LangId::Rust, &sym.name) == key && !rust_impl_owner::is_impl_owner(root, sym)
        });
        if let Some(sym) = named.next()
            && named.next().is_none()
            && let Some(item) = super::descendant_for_range(root, &sym.range)
            && item.parent() == Some(root)
            && matches!(item.kind(), "struct_item" | "enum_item" | "union_item")
            && item
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                .is_some_and(|name| bindings.nominal.contains(name))
            && !symbols::is_symbol_exported(root, source, LangId::Rust, sym.identity_range())
        {
            private.insert(key);
        }
    }
    private
}

/// `private_root_names` (旧新とも非公開のファイル直下の宣言) のうち、旧新とも関数で、名前を束縛し
/// 直さない組み込みの属性だけのもの (Issue #472)。別のクレートの参照はこの関数を名指せない。
fn private_root_functions(
    old: &OldRustPrivateSymbols,
    private_root_names: &HashSet<String>,
    root: tree_sitter::Node<'_>,
    source: &[u8],
    syms: &[Symbol],
) -> HashSet<String> {
    let declarations = std::cell::OnceCell::<NamedDeclarations>::new();
    private_root_names
        .iter()
        .filter(|name| {
            let Some(old_attributes) = old.plain_functions.get(*name) else {
                return false;
            };
            let key = ci_key(LangId::Rust, name);
            let mut named = syms.iter().filter(|symbol| {
                ci_key(LangId::Rust, &symbol.name) == key
                    && !rust_impl_owner::is_impl_owner(root, symbol)
            });
            named
                .next()
                .filter(|_| named.next().is_none())
                .and_then(|symbol| super::descendant_for_range(root, &symbol.range))
                .and_then(|node| plain_root_function_attributes(root, source, node))
                .is_some_and(|attributes| attributes == *old_attributes)
                && declarations
                    .get_or_init(|| NamedDeclarations::count(root, source))
                    .get(&key)
                    == 1
        })
        // 参照は `r#` を外した名前で引く。
        .map(|name| ci_key(LangId::Rust, name))
        .collect()
}

/// ファイルの中の、正規化した名前ごとの宣言の数 (入れ子・本体の中を含む)。シンボルの抽出が
/// 数えない宣言 (`extern` のブロックの中の本体の無い関数、enum の variant など。`use Op::*` の先で
/// `translate(1)` と呼べる) と、マクロの token tree の中の `fn translate` のような宣言の形も数える。
/// モジュールの項目の位置のマクロの呼び出しがあれば、どの名前の宣言も作り得るとして 1 つ足す。
struct NamedDeclarations {
    counts: HashMap<String, usize>,
    item_macro: bool,
}

impl NamedDeclarations {
    fn count(root: tree_sitter::Node<'_>, source: &[u8]) -> Self {
        let mut declarations = Self {
            counts: HashMap::new(),
            item_macro: false,
        };
        named_declarations(root, source, &mut declarations);
        declarations
    }

    /// 正規化した名前 `key` の宣言の数。
    fn get(&self, key: &str) -> usize {
        self.counts.get(key).copied().unwrap_or(0) + usize::from(self.item_macro)
    }
}

fn named_declarations(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    declarations: &mut NamedDeclarations,
) {
    const KINDS: &[&str] = &[
        "function_item",
        "function_signature_item",
        "enum_variant",
        "struct_item",
        "enum_item",
        "union_item",
        "type_item",
        "trait_item",
        "mod_item",
        "const_item",
        "static_item",
        "macro_definition",
        "field_declaration",
        "associated_type",
    ];
    const ITEM_KEYWORDS: &[&str] = &[
        "fn", "struct", "enum", "union", "const", "static", "mod", "type", "trait",
    ];
    let key_of = |node: tree_sitter::Node<'_>| {
        (node.kind() == "identifier")
            .then(|| node.utf8_text(source).ok())
            .flatten()
            .map(|name| ci_key(LangId::Rust, name))
    };
    // モジュールの項目の位置のマクロの呼び出しがあるか。別のファイルで定義したマクロは、引数の名前
    // (`generate!(translate)`) でも、本体に固定した名前 (`generate!((_: u8))` が `pub fn translate`
    // を作る) でも、標準のマクロの名前で再公開したもの (`crate::defs::vec!(...)`) でも、同じ名前の
    // 関数を作り得るので、呼び出しの名前と引数では決めない。
    let _: std::ops::ControlFlow<()> = crate::engine::tree_walk::walk_preorder(
        root,
        crate::engine::tree_walk::Children::Named,
        |node, _| {
            if KINDS.contains(&node.kind())
                && let Some(name) = node
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(source).ok())
            {
                *declarations
                    .counts
                    .entry(ci_key(LangId::Rust, name))
                    .or_insert(0) += 1;
            }
            if node.kind() == "macro_invocation" && module_item_position(node) {
                declarations.item_macro = true;
            }
            if node.kind() == "token_tree" {
                let mut cursor = node.walk();
                let mut previous: Option<&str> = None;
                for token in node.children(&mut cursor).filter(|token| !token.is_extra()) {
                    let item_keyword = previous.is_some_and(|word| ITEM_KEYWORDS.contains(&word));
                    if item_keyword && let Some(key) = key_of(token) {
                        *declarations.counts.entry(key).or_insert(0) += 1;
                    }
                    previous = token.utf8_text(source).ok();
                }
            }
            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
        },
    );
}

/// マクロの呼び出しが、モジュールの項目の位置 (ファイルの直下・`mod`・`extern` のブロックの中) に
/// あるか。関数の本体の中の呼び出しが作る項目はその本体の中だけで見え、`impl`・trait の中の
/// 呼び出しが作るのは関連項目で、どちらも別のクレートの `translate(..)` の行き先にならない。
fn module_item_position(invocation: tree_sitter::Node<'_>) -> bool {
    let Some(mut parent) = invocation.parent() else {
        return false;
    };
    // `generate!(translate);` は文の形 (`expression_statement`) になる。
    if parent.kind() == "expression_statement" {
        let Some(outer) = parent.parent() else {
            return false;
        };
        parent = outer;
    }
    match parent.kind() {
        "source_file" => true,
        "declaration_list" => parent
            .parent()
            .is_none_or(|owner| !matches!(owner.kind(), "impl_item" | "trait_item")),
        _ => false,
    }
}

/// ファイル直下の関数で、宣言の前の属性と本体の直下の内側の属性が、名前も可視性も変えない組み込みの
/// 属性 (`cfg`・`allow`・`inline`・`doc` など) だけのとき、その属性の綴り (空白を詰めて、書いた順)。
/// `test` と derive (別名の import で属性マクロに差し替え得る)、`no_mangle`・`export_name`
/// (依存の無いクレートからも記号の名前で呼べる)、`cfg_attr`・属性マクロは許さない。
///
/// 呼び出し側は旧新の綴りが同じことも求める。`#[cfg(any())]` を `#[cfg(all())]` に変えると、
/// 無かった非公開の関数が現れて、同じ名前の公開の glob の再輸出 (`pub use api::*;`) を隠し、
/// 別のクレートの `a::translate` が非公開の関数を指して壊れる。
fn plain_root_function_attributes(
    root: tree_sitter::Node<'_>,
    source: &[u8],
    node: tree_sitter::Node<'_>,
) -> Option<Vec<String>> {
    let plain = |item: tree_sitter::Node<'_>| {
        super::rust_std_origin::safe_attribute(item, source).is_some_and(|names| names.is_empty())
    };
    // 属性のトークンの並び。文字列・文字のリテラルは中身ごと 1 つのトークンにする
    // (`#[cfg(mode = "a  b")]` と `#[cfg(mode = "a b")]` は別の条件)。
    let spelling = |item: tree_sitter::Node<'_>| {
        let mut tokens = Vec::new();
        let mut pending = vec![item];
        while let Some(node) = pending.pop() {
            if matches!(node.kind(), "line_comment" | "block_comment") {
                continue;
            }
            if node.child_count() == 0
                || matches!(
                    node.kind(),
                    "string_literal" | "raw_string_literal" | "char_literal"
                )
            {
                tokens.push(node.utf8_text(source).ok()?.to_owned());
                continue;
            }
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            pending.extend(children.into_iter().rev());
        }
        Some(tokens.join("\u{1f}"))
    };
    if node.kind() != "function_item" || node.parent() != Some(root) {
        return None;
    }
    let mut attributes = Vec::new();
    let mut previous = node.prev_named_sibling();
    while let Some(sibling) = previous {
        match sibling.kind() {
            "attribute_item" if !plain(sibling) => return None,
            "attribute_item" => attributes.push(spelling(sibling)?),
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        previous = sibling.prev_named_sibling();
    }
    attributes.reverse();
    let body = node.child_by_field_name("body")?;
    let mut cursor = body.walk();
    for child in body.named_children(&mut cursor) {
        if child.kind() == "inner_attribute_item" {
            if !plain(child) {
                return None;
            }
            attributes.push(spelling(child)?);
        }
    }
    Some(attributes)
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
) -> (
    HashSet<String>,
    HashMap<String, declaration::PrivateMethodOwner>,
) {
    let mut root_names = HashSet::new();
    let mut method_owners = HashMap::new();
    for change in affected {
        let key = ci_key(LangId::Rust, &change.name);
        let mut named = syms.iter().filter(|symbol| {
            ci_key(LangId::Rust, &symbol.name) == key
                && !rust_impl_owner::is_impl_owner(root, symbol)
        });
        if old.root_names.contains(&change.name)
            && let Some(symbol) = named.next()
            && named.next().is_none()
            && declaration::is_rust_file_root_symbol(root, symbol)
            && !symbols::is_symbol_exported(root, source, LangId::Rust, symbol.identity_range())
        {
            root_names.insert(change.name.clone());
        }
        let mut new_methods = syms
            .iter()
            .filter(|symbol| symbol.name == change.name && symbol_overlaps_hunks(symbol, &df.hunks))
            .filter_map(|symbol| declaration::rust_private_inherent_method(root, source, symbol));
        if let Some(mut new_owner) = new_methods.next()
            && new_methods.next().is_none()
            && syms
                .iter()
                .filter(|symbol| {
                    symbol.name == change.name && !rust_impl_owner::is_impl_owner(root, symbol)
                })
                .count()
                == 1
            && let Some(old_owner) = old.method_owners.get(&change.name)
            && old_owner.name == new_owner.name
        {
            new_owner.unconditional &= old_owner.unconditional;
            method_owners.insert(change.name.clone(), new_owner);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(source: &str) -> (tree_sitter::Tree, Vec<Symbol>) {
        let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
        assert!(!tree.root_node().has_error(), "{source}");
        let syms =
            symbols::extract_symbols(tree.root_node(), source.as_bytes(), LangId::Rust).unwrap();
        (tree, syms)
    }

    /// 所有型の集合を持つのは、同じ名前の宣言がすべて inherent メソッドの名前だけ (Issue #470)。
    #[test]
    fn inherent_owners_need_every_same_named_declaration_to_be_an_inherent_method() {
        let source = "struct A;\nstruct B;\nstruct W<T>(T);\nimpl A {\n    fn new() -> Self { A }\n    fn only_a(&self) {}\n}\nimpl B {\n    fn new() -> Self { B }\n}\nimpl<T> W<T> {\n    fn generic(&self) {}\n}\nimpl m::Scoped {\n    fn scoped(&self) {}\n}\ntrait T {\n    fn in_trait(&self);\n    fn provided(&self) {}\n}\nimpl T for A {\n    fn in_trait(&self) {}\n}\nfn free() {}\nimpl A {\n    fn free(&self) {}\n}\nimpl <Vec<A> as IntoIterator>::Item {\n    fn projected(&self) {}\n}\nimpl dyn T {\n    fn on_object(&self) {}\n}\nimpl &A {\n    fn on_reference(&self) {}\n}\nfn outer() {\n    fn only_a() {}\n}\n";
        let (tree, syms) = parsed(source);
        let bindings = ordinary_private_bindings(tree.root_node(), source.as_bytes()).unwrap();
        let owners = rust_inherent_owners_by_name(
            tree.root_node(),
            source.as_bytes(),
            &syms,
            &bindings.nominal,
        );
        let expect = |name: &str, expected: Option<&[&str]>| {
            let expected = expected.map(|names| {
                names
                    .iter()
                    .map(|name| (*name).to_owned())
                    .collect::<BTreeSet<_>>()
            });
            assert_eq!(owners.get(name), Some(&expected), "{name}");
        };
        expect("new", Some(&["A", "B"]));
        expect("only_a", None);
        expect("generic", Some(&["W"]));
        expect("scoped", None);
        // trait の宣言・実装のメソッド、同じ名前の自由関数、名前 1 つで表せない型の impl。
        for name in [
            "in_trait",
            "provided",
            "free",
            "projected",
            "on_object",
            "on_reference",
        ] {
            expect(name, None);
        }
    }
}
