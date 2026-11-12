//! 変更ファイルの追加・削除・変更の候補と、追加・削除したファイルの分類。

use super::*;

/// 変更ファイルの新旧 exported シンボルから作る突き合わせ用インデックス。
///
/// 同名シンボルが旧/新いずれかに複数存在する場合、`HashMap<name, sig>` は最後の 1 件しか
/// 保持できず、別のオーバーロードや誤パースされた定義同士を突き合わせて api.mod を
/// 誤検出する。出現回数も併せて持ち、複数あるシンボルは曖昧として modified 判定から除外する
/// (Issue #13: C++ overload / マクロ誤パースの api.mod 誤検出対策)。
pub(super) struct SymbolMaps<'a> {
    pub(super) old_map: std::collections::HashMap<&'a str, &'a str>,
    pub(super) new_map: std::collections::HashMap<&'a str, (&'a str, &'a str)>,
    pub(super) old_name_counts: std::collections::HashMap<&'a str, usize>,
    pub(super) new_name_counts: std::collections::HashMap<&'a str, usize>,
    /// Rust のとき、旧新を宣言の場所 (inline module の経路・qualname・種類) で組にした結果
    /// (Issue #282)。あれば追加・削除・変更は名前ではなくこれで決める。
    pub(super) rust: Option<RustPairing>,
}

impl<'a> SymbolMaps<'a> {
    pub(super) fn build(facts: &ModifiedFileFacts<'a>) -> Self {
        let mut old_name_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for (name, _, _) in facts.old_syms {
            *old_name_counts.entry(name.as_str()).or_default() += 1;
        }
        let mut new_name_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for (name, _, _) in facts.new_syms {
            *new_name_counts.entry(name.as_str()).or_default() += 1;
        }
        Self {
            old_map: facts
                .old_syms
                .iter()
                .map(|(name, _kind, sig)| (name.as_str(), sig.as_str()))
                .collect(),
            new_map: facts
                .new_syms
                .iter()
                .map(|(name, kind, sig)| (name.as_str(), (kind.as_str(), sig.as_str())))
                .collect(),
            old_name_counts,
            new_name_counts,
            rust: facts.rust_declarations.and_then(|declarations| {
                RustPairing::build(
                    facts.old_syms,
                    &declarations.old,
                    facts.new_syms,
                    &declarations.new,
                )
            }),
        }
    }
}

/// `from` の各エントリが「同名が `to` にも残るのに、`to` 側より件数が多い名前の余剰分」かを
/// `from` の並び順のマスクで返す。
///
/// 余剰分は、同じ (name, kind, signature) が `to` に 1 件も残っていないエントリ。名前ごと
/// 片側にしか無いものと、件数が同じか少ない名前は対象外 (前者は呼び出し側が別途扱い、
/// 後者はどの定義同士が対応するか決められないため api.mod と同じく曖昧として扱う)。
///
/// 同じ (name, kind, signature) が `to` に残るエントリは、件数が減っていても余剰にしない。
/// `#[cfg(..)]` / `#ifdef` の分岐ごとに同じシグネチャで定義した関数を 1 つにまとめると
/// 件数だけが減るが、利用者から見た API は何も失われていない (多重集合の差分で数えると
/// 生きている関数が blocking な api.rm に化ける)。
pub(super) fn same_name_surplus_mask(
    from: &[(String, String, String)],
    to: &[(String, String, String)],
) -> Vec<bool> {
    use std::collections::{HashMap, HashSet};
    let mut from_counts: HashMap<&str, usize> = HashMap::new();
    for (name, _, _) in from {
        *from_counts.entry(name.as_str()).or_default() += 1;
    }
    let mut to_counts: HashMap<&str, usize> = HashMap::new();
    let mut to_entries: HashSet<(&str, &str, &str)> = HashSet::new();
    for (name, kind, sig) in to {
        *to_counts.entry(name.as_str()).or_default() += 1;
        to_entries.insert((name.as_str(), kind.as_str(), sig.as_str()));
    }
    from.iter()
        .map(|(name, kind, sig)| {
            let to_count = to_counts.get(name.as_str()).copied().unwrap_or(0);
            to_count > 0
                && from_counts[name.as_str()] > to_count
                && !to_entries.contains(&(name.as_str(), kind.as_str(), sig.as_str()))
        })
        .collect()
}

/// 旧ツリーに存在しない新規シンボルを `all_new_candidates` / `added` に振り分ける。
///
/// 返り値は同ファイル内の新規シンボル名集合。削除判定が「rename + 実装置換」の
/// 除外条件として使うため、追加判定の副産物として返す。
pub(super) fn collect_added_symbols(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
    maps: &SymbolMaps<'_>,
) -> std::collections::HashSet<String> {
    let &DetectionInputs { dir, .. } = inputs;
    let &ModifiedFileFacts {
        old_syms,
        new_syms,
        in_file_callees,
        ..
    } = facts;
    let SymbolMaps { old_map, rust, .. } = maps;

    let is_binary_rust_crate = is_binary_only_rust_crate(dir, &df.new_path);

    // rename 検出用: 同ファイル内に新規追加された全シンボル名を追跡する。
    let mut new_symbols_in_current_file: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    // Rust は宣言の場所で組にしたので、同じ名前の数の差ではなく、旧い版に同じキーの宣言が
    // 無いものを追加とする (Issue #282)。
    let same_name_additions = match rust {
        Some(_) => vec![false; new_syms.len()],
        None => same_name_surplus_mask(new_syms, old_syms),
    };
    for (index, ((name, kind, sig), same_name_addition)) in
        new_syms.iter().zip(same_name_additions).enumerate()
    {
        if same_name_addition {
            // 同名が旧側にもある追加 (overload の追加など)。api.add には載せないが、
            // 同名の 1 つを別ファイルへ移した変更の移動先として move 突き合わせの候補にする。
            // 候補にしないと、移動元で件数の減った削除が相殺されず api.rm に残る。
            state.buckets.all_new_candidates.push(ApiSymbolCandidate {
                inline_module: None,
                name: name.clone(),
                kind: kind.clone(),
                file: df.new_path.clone(),
                signature: sig.clone(),
            });
            continue;
        }
        let is_new = match rust {
            Some(pairing) => pairing.added[index],
            None => !old_map.contains_key(name.as_str()),
        };
        if is_new {
            new_symbols_in_current_file.insert(name.clone());
            // module の置き場の付け替えで旧版にもある項目は、比べ直しが扱う (Issue #305)。
            if facts
                .rust_declarations
                .and_then(|declarations| declarations.new.get(index))
                .is_some_and(|context| {
                    inputs.module_availability.drops_candidate(
                        super::rust_module_availability::Version::New,
                        &df.new_path,
                        context,
                        name,
                        kind,
                    )
                })
            {
                continue;
            }
            let candidate = ApiSymbolCandidate {
                inline_module: facts
                    .rust_declarations
                    .and_then(|declarations| declarations.new_inline_module(index)),
                name: name.clone(),
                kind: kind.clone(),
                file: df.new_path.clone(),
                signature: sig.clone(),
            };
            record_added_candidate(
                inputs,
                state,
                candidate,
                is_binary_rust_crate,
                facts
                    .rust_declarations
                    .and_then(|declarations| declarations.new.get(index)),
                in_file_callees,
            );
        }
    }
    new_symbols_in_current_file
}

/// 新ツリーに存在しない削除シンボルを `removed` / `property_to_field` に振り分ける。
pub(super) fn collect_removed_symbols(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
    maps: &SymbolMaps<'_>,
    new_symbols_in_current_file: &std::collections::HashSet<String>,
) {
    let &DetectionInputs {
        dir,
        base,
        diff_files,
        ref_index,
        old_ref_index,
        index_names,
        prepared,
        base_blobs,
        ..
    } = inputs;
    let &ModifiedFileFacts {
        old_syms,
        new_syms,
        in_file_callees,
        new_export_surface_names,
        ..
    } = facts;
    let SymbolMaps { new_map, rust, .. } = maps;

    // Bash スクリプトでは関数定義は `export -f` (または `declare -fx`/`declare -xf`) で
    // 明示しない限りサブプロセスへ波及しない。
    let is_bash_old_file = is_bash_script_path(&df.old_path);
    // base 側 blob の `export -f` 宣言は必要になった時点でファイル単位に 1 回だけ読む。
    let mut bash_exported: Option<std::collections::HashSet<String>> = None;
    // 対象となる値の置き換えがあるときだけ、ファイル単位で 1 回読む。
    let mut forwarded_signatures = None;
    let is_rust_pair = [&df.old_path, &df.new_path].iter().all(|path| {
        crate::language::LangId::from_path(camino::Utf8Path::new(path)).ok()
            == Some(crate::language::LangId::Rust)
    });
    let is_js_ts_pair = [&df.old_path, &df.new_path].iter().all(|path| {
        matches!(
            crate::language::LangId::from_path(camino::Utf8Path::new(path)).ok(),
            Some(
                crate::language::LangId::Typescript
                    | crate::language::LangId::Tsx
                    | crate::language::LangId::Javascript
            )
        )
    });
    // 同名が新側にも残る削除 (overload の片方など)。名前の有無だけで判定すると検出
    // できないが、件数が減り旧シグネチャも残っていない以上、消えた定義があるので曖昧ではない。
    // Rust は宣言の場所で組にしたので、新しい版に同じキーの宣言が無いものを削除とする
    // (Issue #282)。同じ名前の宣言がほかの場所に残る削除は、名前で推測する rename や field 化の
    // 判定に流さない。
    let (removals, same_name_removals): (Vec<bool>, Vec<bool>) = match rust {
        Some(pairing) => old_syms
            .iter()
            .zip(&pairing.removed)
            .map(|((name, _, _), &removed)| {
                (removed, removed && new_map.contains_key(name.as_str()))
            })
            .unzip(),
        None => old_syms
            .iter()
            .zip(same_name_surplus_mask(old_syms, new_syms))
            .map(|((name, _, _), same_name_removal)| {
                (
                    same_name_removal || !new_map.contains_key(name.as_str()),
                    same_name_removal,
                )
            })
            .unzip(),
    };
    let mut unparsed_by_name = std::collections::HashMap::<&str, Vec<_>>::new();
    let mut old_name_counts = std::collections::HashMap::<&str, usize>::new();
    // 旧側も Bash 文法で解析していた場合だけ照合する。別言語の同名 API を
    // シェルへ置き換えた削除は抑制しない。拡張子なしの旧 shebang も確認する。
    if !facts.unparsed_declarations.is_empty()
        && base_blobs.read(&df.old_path).is_some_and(|source| {
            parser::detect_lang(camino::Utf8Path::new(&df.old_path), &source).ok()
                == Some(crate::language::LangId::Bash)
        })
    {
        for decl in facts.unparsed_declarations {
            unparsed_by_name.entry(&decl.name).or_default().push(decl);
        }
        for (name, _, _) in old_syms {
            *old_name_counts.entry(name).or_default() += 1;
        }
    }
    // TS/JS: 新ツリーで export clause (`export { name } from "..."` / `import ...;
    // export { name };`) により name が公開され続けているシンボルは、利用者から見た
    // API 面が維持されているため api.rm から除外する。
    // `new_export_surface_names` は Phase 0 の単一 parse で先取り済み (perf #2)。
    for (index, (((name, kind, sig), same_name_removal), removed)) in old_syms
        .iter()
        .zip(same_name_removals)
        .zip(removals)
        .enumerate()
    {
        if removed {
            // module の置き場の付け替えで新版にもある項目は、比べ直しが扱う (Issue #305)。
            if facts
                .rust_declarations
                .and_then(|declarations| declarations.old.get(index))
                .is_some_and(|context| {
                    inputs.module_availability.drops_candidate(
                        super::rust_module_availability::Version::Old,
                        &df.old_path,
                        context,
                        name,
                        kind,
                    )
                })
            {
                continue;
            }
            let inline_module = facts
                .rust_declarations
                .and_then(|declarations| declarations.old_inline_module(index));
            // inherent impl の項目は impl の対象の型で決める (Issue #315)。
            if rust_declaration_outside_public_api_surface(
                inputs.rust_crates,
                state.rust_public,
                dir,
                base_blobs,
                true,
                &df.old_path,
                name,
                facts
                    .rust_declarations
                    .and_then(|declarations| declarations.old.get(index)),
            ) {
                continue;
            }
            if new_export_surface_names.contains(name.as_str()) {
                if is_js_ts_pair && matches!(kind.as_str(), "variable" | "constant") {
                    let signatures = forwarded_signatures.get_or_insert_with(|| {
                        super::forwarded_values::signatures(dir, &df.new_path)
                    });
                    // 読み込み・解析に失敗した場合も、同名だけで変化を相殺しない。
                    if signatures.as_ref().is_none_or(|map| map.contains_key(name)) {
                        state.buckets.modified.push(ApiSymbolChange::new(
                            name.clone(),
                            kind.clone(),
                            df.new_path.clone(),
                            Some(sig.clone()),
                            signatures.as_ref().and_then(|map| map.get(name)).cloned(),
                        ));
                    }
                } else if is_rust_pair && REEXPORT_COMPARED_KINDS.contains(&kind.as_str()) {
                    // Rust は、再公開の先の宣言と組にして比べる (Issue #281)。
                    state.buckets.reexported_removals.push((
                        ApiSymbolCandidate {
                            inline_module: inline_module.clone(),
                            name: name.clone(),
                            kind: kind.clone(),
                            file: df.old_path.clone(),
                            signature: sig.clone(),
                        },
                        df.new_path.clone(),
                    ));
                }
                continue;
            }
            // 以降の「rename + 実装置換」と `@property` → field の置き換えは、名前ごと
            // 消えた場合の判定。同名が残る削除は rename でも field 化でもない。
            if same_name_removal {
                state.buckets.removed.push(ApiSymbolCandidate {
                    inline_module,
                    name: name.clone(),
                    kind: kind.clone(),
                    file: df.old_path.clone(),
                    signature: sig.clone(),
                });
                continue;
            }
            // 同名宣言が 1 件ずつの場合だけ、ERROR 内のヘッダを未検証の証拠にする。
            // 複数定義の一部削除を隠さず、move / dead の相殺にも流さない。
            if kind == "function"
                && old_name_counts.get(name.as_str()) == Some(&1)
                && let Some(decls) = unparsed_by_name.get(name.as_str())
                && let [decl] = decls.as_slice()
            {
                state
                    .buckets
                    .uncertain_removals
                    .push(crate::models::review::UncertainApiRemoval {
                        name: name.clone(),
                        kind: kind.clone(),
                        file: df.new_path.clone(),
                        line: decl.line,
                        old_signature: sig.clone(),
                        reason: crate::models::truncation::TruncationReason::ParseErrorRegion,
                    });
                continue;
            }
            // 変更後の参照 0 件だけでは、元から未使用だった API の削除と、caller も
            // 追随した rename を区別できない。変更前に AST 参照があり、新しい
            // シンボルにも参照があるときだけ rename とみなす。
            // 変更前の参照が残っている未変更ファイルは、変更後の参照 0 件という
            // 条件を満たさないため、差分ファイルの旧 blob だけを調べれば足りる。
            let bash_pure_removal_skip = is_bash_old_file
                && new_symbols_in_current_file.is_empty()
                && !bash_exported
                    .get_or_insert_with(|| bash_exported_functions_in_git(dir, base, &df.old_path))
                    .contains(name);
            let new_unreferenced = is_removed_symbol_unreferenced(ref_index, name);
            let replacement_is_used = new_unreferenced
                && new_symbols_in_current_file.iter().any(|new_name| {
                    is_internally_connected(in_file_callees, new_name)
                        || ref_index.refs_for(bare_name(new_name)).is_some_and(|refs| {
                            refs.iter().any(|r| {
                                r.kind != Some(crate::models::reference::RefKind::Definition)
                            })
                        })
                });
            let has_old_reference = new_unreferenced
                && replacement_is_used
                && old_ref_index
                    .get_or_init(|| {
                        OldRefIndex::build(index_names, diff_files, prepared, base_blobs)
                    })
                    .has_surviving_old_reference(name);
            if new_unreferenced && (bash_pure_removal_skip || has_old_reference) {
                continue;
            }
            // Python の @property → 同じクラスの field 置き換えなら removed 扱いせず
            // property_to_field に振り替える。
            if let Some(target_file) =
                detect_python_property_to_field(dir, base, &df.old_path, &df.new_path, name)
            {
                state.buckets.property_to_field.push(PropertyToFieldChange {
                    name: name.clone(),
                    file: target_file,
                });
                continue;
            }
            state.buckets.removed.push(ApiSymbolCandidate {
                inline_module,
                name: name.clone(),
                kind: kind.clone(),
                file: df.old_path.clone(),
                signature: sig.clone(),
            });
        }
    }
}

/// 新旧どちらにも存在しシグネチャが変わったシンボルを分類する。
pub(super) fn collect_modified_symbols(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
    maps: &SymbolMaps<'_>,
) {
    let &DetectionInputs {
        dir, base_blobs, ..
    } = inputs;
    let &ModifiedFileFacts {
        new_syms,
        in_file_callees,
        ..
    } = facts;
    let SymbolMaps {
        old_map,
        old_name_counts,
        new_name_counts,
        ..
    } = maps;
    let mut files =
        ChangedSignatureFiles::new(inputs, state, &df.old_path, &df.new_path, in_file_callees);
    files.rust_const_additions = Some(facts.rust_const_additions);
    files.type_dependencies = Some(facts.type_dependencies);
    if let (Some(pairing), Some(declarations)) = (&maps.rust, facts.rust_declarations) {
        collect_rust_modified_symbols(inputs, state, df, facts, pairing, declarations, &mut files);
        return;
    }
    let recovered = super::ts_namespace::recover_root_changes(inputs, df, facts);

    // 同一 (file, qualname) の modified を重複排除するためのキーセット
    let mut seen_modified: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    for (name, kind, new_sig) in new_syms {
        if let Some(root) = recovered.get(name) {
            if seen_modified.insert((df.new_path.clone(), name.clone())) {
                files.classify(
                    inputs,
                    state,
                    name,
                    &root.kind,
                    &root.old_sig,
                    &root.new_sig,
                );
            }
            continue;
        }
        // 正規化済み signature が同じでも、静的 container の整形が impact に残り得る。
        if let Some(old_sig) = old_map.get(name.as_str())
            && old_sig == &new_sig.as_str()
        {
            let site = CompatibleModSite {
                dir,
                base: inputs.base,
                old_path: &df.old_path,
                new_path: &df.new_path,
                name,
                kind,
                old_sig,
                new_sig,
                lang_id: files.lang_id,
            };
            let mut sources =
                SignatureSourceCache::with_base_blobs(base_blobs, inputs.strip_internal);
            // signature が同じなので、型注釈と静的 container の段階だけを見る。
            let decision = if facts.type_dependencies.get(name)
                == Some(&super::ts_signature::DependencyStatus::Changed)
            {
                local_props_alias_removal_decision(
                    inputs,
                    state.closure_caches,
                    &site,
                    &mut sources,
                )
                .unwrap_or(SignatureDecision::Blocking(None))
            } else {
                match type_annotation_decision(&site, &mut sources) {
                    Some(decision) => decision,
                    None => static_const_decision(&site, &mut sources)
                        .unwrap_or(SignatureDecision::Omit),
                }
            };
            let change = ApiSymbolChange::new(
                name.clone(),
                kind.clone(),
                df.new_path.clone(),
                Some(old_sig.to_string()),
                Some(new_sig.clone()),
            );
            record_signature_decision(inputs, state, change, decision);
            continue;
        }
        if let Some(old_sig) = old_map.get(name.as_str())
            && old_sig != &new_sig.as_str()
            && seen_modified.insert((df.new_path.clone(), name.clone()))
        {
            // 同名が旧/新いずれかに複数あるシンボルは曖昧として modified から除外
            if old_name_counts.get(name.as_str()).copied().unwrap_or(0) > 1
                || new_name_counts.get(name.as_str()).copied().unwrap_or(0) > 1
            {
                continue;
            }
            if facts.parameter_pattern_only_names.contains(name) {
                continue;
            }
            files.classify(inputs, state, name, kind, old_sig, new_sig);
        }
    }
}

/// Python の root-level スクリプト (package 外の単体スクリプト、例: `build_font.py`) の削除/変更
/// シンボルが公開 API 面外かを判定する (codex 設計合意の厳格版 A3、Issue
/// 2026-06-14-python-script-move-api-rm)。
///
/// 以下を全て満たすとき script-local = true:
/// - `old_path` が `.py`、path 区切りを含まない (リポジトリルート直下)、`__init__.py` でない
/// - モジュール名 (stem) が新ツリーの Python ファイルから参照 (import) されていない
/// - base の pyproject.toml の `[project.scripts]` / `[project.gui-scripts]` が当モジュールを
///   entrypoint に指定していない
///
/// 直接実行されるスクリプトの公開面は「ファイルを実行できること」であって内部 helper の
/// signature ではないため、これらの削除/シグネチャ変更を api.rm / api.mod にしない。package
/// module (サブディレクトリ配下) は対象外 = 従来どおり API 扱い (false negative 回避)。判定は
/// file 単位で 1 度行えば足りる (find_references の全走査は root-level .py 削除時のみ走る)。
pub(crate) fn is_python_root_script_local_file(dir: &str, base: &str, old_path: &str) -> bool {
    let path = std::path::Path::new(old_path);
    if path.extension().and_then(|e| e.to_str()) != Some("py") {
        return false;
    }
    // root-level のみ (package module は API 扱いで安全側に倒す)
    if old_path.contains('/') {
        return false;
    }
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    if stem == "__init__" {
        return false;
    }
    // base の pyproject が当モジュールを script entrypoint にしている、または pyproject が
    // 解析不能なら公開面ありとして扱い script-local 判定を止める (keep, fail-closed)。
    if base_pyproject_marks_module_as_api(dir, base, stem) {
        return false;
    }
    // 新ツリーで stem が参照 (import) されていなければ script-local
    !python_module_referenced_in_tree(dir, stem)
}

/// 新ツリーの Python ファイルで `stem` (モジュール名) が identifier として参照されているか。
/// `import build_font` / `build_font.foo()` 等を 1 件でも見つければ true。判定不能 (検索失敗) は
/// true (= 参照あり扱いで api.rm を残す fail-closed)。
pub(crate) fn python_module_referenced_in_tree(dir: &str, stem: &str) -> bool {
    match crate::engine::refs::find_references(stem, std::path::Path::new(dir), Some("**/*.py")) {
        Ok(refs) => !refs.is_empty(),
        Err(_) => true,
    }
}

/// base の pyproject.toml が `stem` モジュールを script entrypoint に宣言しているか、または
/// pyproject が存在するが解析不能なら `true` (= 公開 API/CLI 面ありとして script-local 判定を
/// 止める fail-closed)。pyproject が存在しなければ `false` (script 宣言なし → 参照判定へ進む)。
///
/// 対応形式:
/// - PEP 621 `[project.scripts]` / `[project.gui-scripts]` (`name = "module:func"`)
/// - Poetry `[tool.poetry.scripts]` (string 形式 `name = "module:func"`)
///
/// fail-closed: pyproject 解析失敗、または script 値が string で取れない (Poetry 拡張テーブル
/// 形式など) 場合は `true` を返し、real な CLI/API 面を誤って隠さない。
pub(crate) fn base_pyproject_marks_module_as_api(dir: &str, base: &str, stem: &str) -> bool {
    let Some(content) = git_show_base_file(dir, base, "pyproject.toml") else {
        return false;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&content) else {
        return true; // 存在するが解析不能 → fail-closed (API 扱い)
    };
    let script_tables = [
        value.get("project").and_then(|p| p.get("scripts")),
        value.get("project").and_then(|p| p.get("gui-scripts")),
        value
            .get("tool")
            .and_then(|t| t.get("poetry"))
            .and_then(|p| p.get("scripts")),
    ];
    for table in script_tables.into_iter().flatten() {
        let Some(table) = table.as_table() else {
            // script セクションが存在するが table でない (schema 不正) → 解析不能なので
            // fail-closed (API 扱い)。codex 指摘: ここを continue にすると安全側から漏れる。
            return true;
        };
        for v in table.values() {
            let Some(target) = v.as_str() else {
                // string で取れない値 (Poetry 拡張テーブル形式など) は解析不能 → fail-closed。
                return true;
            };
            let module = target.split(':').next().unwrap_or("");
            if module == stem || module.split('.').next() == Some(stem) {
                return true;
            }
        }
    }
    false
}

/// `git show <base>:<rel>` でファイル内容を UTF-8 文字列として取得する。失敗時は None。
pub(crate) fn git_show_base_file(dir: &str, base: &str, rel: &str) -> Option<String> {
    String::from_utf8(git_show_blob(dir, base, rel)?).ok()
}

/// 削除ファイル (`new_path == "/dev/null"`) 由来の exported シンボルを `removed` に分類する。
/// 対応言語でないファイルへの rename も新側に API 面が無いため、ここで旧側 (`old_path`) の
/// 削除として扱う。
///
/// `old_syms` は `detect_api_changes` の Phase 0 で旧版から抽出済みのもの
/// (`extract_deleted_file_exported_symbols`)。
///
/// Rust private module / bin-only crate / Bash の未 export 関数 / Python root-level
/// スクリプトの helper は `removed` から除外する。Python `@property` → field 置き換え
/// (`property_to_field`) は変更後の同じファイルを要するため、削除ファイルでは判定しない。
pub(crate) fn process_deleted_file(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    old_syms: &[(String, String, String)],
    old_source_unverified: bool,
    rust_declarations: Option<&[RustDeclarationContext]>,
) {
    let &DetectionInputs {
        dir,
        base,
        ref_index,
        base_blobs,
        ..
    } = inputs;
    // Python の root-level スクリプト (package 外の単体スクリプト) の top-level helper は公開
    // API 面外なので api.rm にしない (A3、Issue 2026-06-14-python-script-move-api-rm)。file 単位の
    // 判定なので全シンボルをまとめて除外する。
    if !old_source_unverified && is_python_root_script_local_file(dir, base, &df.old_path) {
        return;
    }
    let is_bash_old_file = is_bash_script_path(&df.old_path);
    // base 側 blob の `export -f` 宣言はファイル単位で 1 回だけ読む (関数ごとの git show を避ける)。
    let mut bash_exported: Option<std::collections::HashSet<String>> = None;
    for (index, (name, kind, sig)) in old_syms.iter().enumerate() {
        // module の置き場の付け替えで新版にもある項目は、比べ直しが扱う (Issue #305)。
        if rust_declarations
            .and_then(|declarations| declarations.get(index))
            .is_some_and(|context| {
                inputs.module_availability.drops_candidate(
                    super::rust_module_availability::Version::Old,
                    &df.old_path,
                    context,
                    name,
                    kind,
                )
            })
        {
            continue;
        }
        // inherent impl の項目は impl の対象の型で決める (Issue #315)。
        if !old_source_unverified
            && rust_declaration_outside_public_api_surface(
                inputs.rust_crates,
                state.rust_public,
                dir,
                base_blobs,
                true,
                &df.old_path,
                name,
                rust_declarations.and_then(|declarations| declarations.get(index)),
            )
        {
            continue;
        }
        if !old_source_unverified
            && is_bash_old_file
            && !bash_exported
                .get_or_insert_with(|| bash_exported_functions_in_git(dir, base, &df.old_path))
                .contains(name)
            && is_removed_bash_symbol_unreferenced(ref_index, dir, name)
        {
            continue;
        }
        state.buckets.removed.push(ApiSymbolCandidate {
            inline_module: rust_declarations
                .and_then(|declarations| declarations.get(index))
                .and_then(inline_module_of),
            name: name.clone(),
            kind: kind.clone(),
            file: df.old_path.clone(),
            signature: sig.clone(),
        });
    }
}

/// 新規ファイル (`old_path == "/dev/null"`) 由来の exported シンボルを `added` /
/// `all_new_candidates` に分類する。
///
/// bin-only crate (`src/lib.rs` なし) / private module / 内部のみ参照 / 同一 diff 内で
/// 完結したシンボルは `added` から除外する。
///
/// `new_syms` / `in_file_callees` は `detect_api_changes` の Phase 0 で抽出済みのものを
/// 受け取り、cross-file 参照判定は事前構築済みの `ref_index` を参照する。
pub(crate) fn process_added_file(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &AddedFileFacts<'_>,
) {
    let &DetectionInputs { dir, .. } = inputs;
    let &AddedFileFacts {
        new_syms,
        in_file_callees,
        rust_declarations,
    } = facts;
    let is_binary_rust_crate = is_binary_only_rust_crate(dir, &df.new_path);
    for (index, (name, kind, sig)) in new_syms.iter().enumerate() {
        // module の置き場の付け替えで旧版にもある項目は、比べ直しが扱う (Issue #305)。
        if rust_declarations
            .and_then(|declarations| declarations.get(index))
            .is_some_and(|context| {
                inputs.module_availability.drops_candidate(
                    super::rust_module_availability::Version::New,
                    &df.new_path,
                    context,
                    name,
                    kind,
                )
            })
        {
            continue;
        }
        let candidate = ApiSymbolCandidate {
            inline_module: rust_declarations
                .and_then(|declarations| declarations.get(index))
                .and_then(inline_module_of),
            name: name.clone(),
            kind: kind.clone(),
            file: df.new_path.clone(),
            signature: sig.clone(),
        };
        record_added_candidate(
            inputs,
            state,
            candidate,
            is_binary_rust_crate,
            rust_declarations.and_then(|declarations| declarations.get(index)),
            in_file_callees,
        );
    }
}

/// 新しい版に現れた宣言の候補 1 件を、追加の bucket へ振り分ける (変更ファイルと追加ファイルで同じ規則)。
///
/// 移動の突き合わせには常に候補にする。bin-only crate と公開 API 面の外 (inherent impl の項目は
/// impl の対象の型で決める。Issue #315) は外す。参照の要約には載せ、同じファイルの中か差分の
/// ほかのファイルで使われていれば api.add には載せない。
pub(super) fn record_added_candidate(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    candidate: ApiSymbolCandidate,
    is_binary_rust_crate: bool,
    declaration: Option<&RustDeclarationContext>,
    in_file_callees: &std::collections::HashSet<String>,
) {
    state.buckets.all_new_candidates.push(candidate.clone());
    if is_binary_rust_crate
        || rust_declaration_outside_public_api_surface(
            inputs.rust_crates,
            state.rust_public,
            inputs.dir,
            inputs.base_blobs,
            false,
            &candidate.file,
            &candidate.name,
            declaration,
        )
    {
        return;
    }
    state
        .buckets
        .added_reference_candidates
        .push(candidate.clone());
    if is_internally_connected(in_file_callees, &candidate.name)
        || is_used_in_diff_paths(
            inputs.ref_index,
            inputs.dir,
            &candidate.name,
            &candidate.file,
            inputs.diff_new_paths,
        )
    {
        return;
    }
    state.buckets.added.push(candidate);
}
