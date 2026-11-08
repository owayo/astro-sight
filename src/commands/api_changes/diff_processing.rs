//! 追加 / 削除 / 変更されたファイルごとの API 差分分類。

use super::*;
use crate::models::review::{ApiModificationUnverified, ApiModificationUnverifiedReason};

/// 内部用: reconcile のために signature を保持する一時構造。
#[derive(Debug, Clone)]
pub(crate) struct ApiSymbolCandidate {
    pub(crate) name: String,
    /// Rust の項目が inline module の中にあれば、ファイルの中のその経路 (Issue #282)。
    pub(crate) inline_module: Option<String>,
    pub(crate) kind: String,
    pub(crate) file: String,
    pub(crate) signature: String,
}

impl ApiSymbolCandidate {
    pub(crate) fn into_api_symbol(self) -> ApiSymbol {
        ApiSymbol {
            name: self.name,
            inline_module: self.inline_module,
            kind: self.kind,
            file: self.file,
            refs_internal: 0,
            cfg_availability: None,
        }
    }

    /// 同一ファイル内参照数を添えて `ApiSymbol` にする (api.add 専用)。
    pub(crate) fn into_added_api_symbol(self, refs_internal: usize) -> ApiSymbol {
        ApiSymbol {
            refs_internal,
            ..self.into_api_symbol()
        }
    }
}

/// 通常の modified ファイル (old_path / new_path がどちらも `/dev/null` でない) で、
/// added (新規シンボル) / removed (削除シンボル) / modified (シグネチャ変更) を分類する。
///
/// 全ての pub/exported シンボルの組み合わせを評価し、Rust private module 抑制 /
/// bin-only crate 抑制 / 内部参照 / 同一 diff 内 closed-in-diff / TS optional destructure 等
/// の各種抑制ルールを適用する。最も複雑な処理パス。
///
/// `old_syms` / `new_syms` / `in_file_callees` は `detect_api_changes` の Phase 0 で
/// 抽出済みのものを受け取る (rename 差分では base 側に新パスが存在しないため、旧版は
/// old_path 由来)。cross-file 参照判定は事前構築済みの `ref_index` を参照する。
pub(crate) fn process_modified_file(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
) {
    let maps = SymbolMaps::build(facts);
    // 追加 → 削除 → 変更 の順で振り分ける。削除判定は追加判定が集めた
    // 「同ファイル内の新規シンボル名」を rename 判定に使うため、この順序に依存する。
    let new_symbols_in_current_file = collect_added_symbols(inputs, state, df, facts, &maps);
    collect_removed_symbols(
        inputs,
        state,
        df,
        facts,
        &maps,
        &new_symbols_in_current_file,
    );
    collect_modified_symbols(inputs, state, df, facts, &maps);
    if let (Some(pairing), Some(declarations)) = (&maps.rust, facts.rust_declarations) {
        collect_rust_availability_changes(inputs, state, df, facts, pairing, declarations);
    }
    // シグネチャ差分に乗らない契約変更。上の 3 つと違い「old/new でシグネチャが同じ」
    // シンボルを見るため、最後に独立した経路として走らせる。
    collect_python_contract_changes(inputs, state, df, facts, &maps);
}

/// シグネチャ差分に乗らない Python の型契約変更を、1 組の source / parse から検出する。
///
/// TypedDict と Literal の検出器を別々に読み込ませると、同じ modified ファイルに対して
/// `git show` と tree-sitter parse が 2 回ずつ走るため、ここで共有する。
fn collect_python_contract_changes(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
    maps: &SymbolMaps<'_>,
) {
    if crate::language::LangId::from_path(camino::Utf8Path::new(df.new_path.as_str())).ok()
        != Some(crate::language::LangId::Python)
    {
        return;
    }
    // テストファイルのシンボルは公開 API 差分の対象外。TypedDict 経路は
    // `new_syms` 由来の候補で自然に除外されるが、Literal 経路は直接 AST を読むため
    // ここで既存の export 抽出と同じ規約を適用する。
    if is_test_path(std::path::Path::new(df.new_path.as_str())) {
        return;
    }
    let &DetectionInputs {
        dir, base_blobs, ..
    } = inputs;
    let typed_dict_candidates = python_typed_dict_field_candidates(facts, maps);
    let Some(new_source) = load_new_source(dir, &df.new_path) else {
        return;
    };
    // Literal alias は old/new の双方に `Literal` が無ければ比較対象にならない。
    // TypedDict 候補も無ければ、base blob を取る `git show` の前に落とせる。
    let new_mentions_literal = mentions_literal_qualifier(&new_source);
    if typed_dict_candidates.is_empty() && !new_mentions_literal {
        return;
    }
    let Some(src) = load_old_source_with_new(base_blobs, &df.old_path, &df.new_path, new_source)
    else {
        return;
    };
    let check_typed_dict = !typed_dict_candidates.is_empty()
        && (mentions_requiredness_qualifier(&src.old) || mentions_requiredness_qualifier(&src.new));
    let check_literal = new_mentions_literal && mentions_literal_qualifier(&src.old);
    if !check_typed_dict && !check_literal {
        return;
    }
    let Some((old_tree, new_tree)) = src.parse_pair(crate::language::LangId::Python) else {
        return;
    };
    let parsed = ParsedPythonSources {
        old_root: old_tree.root_node(),
        old_source: &src.old,
        new_root: new_tree.root_node(),
        new_source: &src.new,
    };

    if check_typed_dict {
        collect_python_typed_dict_field_changes(state, df, &typed_dict_candidates, &parsed);
    }
    if check_literal {
        for change in detect_python_literal_alias_changes(
            parsed.old_root,
            parsed.old_source,
            parsed.new_root,
            parsed.new_source,
        ) {
            state.buckets.modified.push(ApiSymbolChange {
                inline_module: None,
                unverified: None,
                name: change.name,
                kind: "type".to_string(),
                file: df.new_path.clone(),
                old_signature: Some(change.old_signature),
                new_signature: Some(change.new_signature),
                // 疑似シンボルを bare 名で解決すると、無関係な同名変数を呼び出し元にしてしまう。
                no_resolved_internal_callers: false,
                contract_change: change.contract,
            });
        }
    }
}

struct ParsedPythonSources<'tree, 'source> {
    old_root: tree_sitter::Node<'tree>,
    old_source: &'source [u8],
    new_root: tree_sitter::Node<'tree>,
    new_source: &'source [u8],
}

/// クラスヘッダが不変で一意な、フィールド契約比較の候補名を I/O 前に抽出する。
fn python_typed_dict_field_candidates<'a>(
    facts: &ModifiedFileFacts<'a>,
    maps: &SymbolMaps<'_>,
) -> Vec<&'a str> {
    let &ModifiedFileFacts { new_syms, .. } = facts;
    let SymbolMaps {
        old_map,
        old_name_counts,
        new_name_counts,
        ..
    } = maps;

    // 対象は「シグネチャ (= クラスヘッダ) が変わっていない」クラスだけ。変わっていれば
    // `detect_python_typed_dict_total_change` が担当するので、ここで扱うと二重報告になる。
    // 同名クラスが複数あるファイルは、どの定義を見ているか確定できないため除外する。
    new_syms
        .iter()
        .filter(|(name, kind, new_sig)| {
            kind.as_str() == "class"
                && !name.contains('.')
                && old_map.get(name.as_str()) == Some(&new_sig.as_str())
                && old_name_counts.get(name.as_str()).copied().unwrap_or(0) == 1
                && new_name_counts.get(name.as_str()).copied().unwrap_or(0) == 1
        })
        .map(|(name, _, _)| name.as_str())
        .collect()
}

/// クラスヘッダが変わらない Python TypedDict の、フィールド単位 requiredness 変更を検出する。
///
/// `y: NotRequired[str]` → `y: str` は**クラスヘッダ行が変わらない**ため
/// `collect_modified_symbols` のシグネチャ差分ループ (`old_sig != new_sig`) に乗らず、
/// api.mod 候補にすらならなかった (Issue
/// 2026-08-19-python-typeddict-field-requiredness-detection)。ここはシグネチャ差分と
/// 独立した新規検出経路で、証明できたものだけ blocking な `modified` へ足す。
///
/// **通常の exported symbol 集合には混ぜない**。`Class.field` はあくまで出力時の疑似シンボルで、
/// `symbols` / `refs` / `dead-code` / `ApiRefIndex` の意味は一切変えない。
fn collect_python_typed_dict_field_changes(
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    candidates: &[&str],
    parsed: &ParsedPythonSources<'_, '_>,
) {
    for &class_name in candidates {
        for change in detect_typed_dict_field_requiredness_changes(
            parsed.old_root,
            parsed.old_source,
            parsed.new_root,
            parsed.new_source,
            class_name,
        ) {
            state.buckets.modified.push(ApiSymbolChange {
                inline_module: None,
                unverified: None,
                name: change.name,
                kind: "field".to_string(),
                file: df.new_path.clone(),
                old_signature: Some(change.old_signature),
                new_signature: Some(change.new_signature),
                // 疑似シンボルなので呼び出し元の解決はしない。`field` を bare 名で引くと
                // 無関係な同名フィールド / ローカル変数に当たる。
                no_resolved_internal_callers: false,
                contract_change: Some(change.contract),
            });
        }
    }
}

/// `Required` / `NotRequired` の出現を素朴に見る安価な前段フィルタ。
/// `NotRequired` は `Required` を含むため 1 パターンで足りる。
/// 過剰にヒットしても後段の AST 判定で落ちるだけなので、保守側に振れている。
fn mentions_requiredness_qualifier(source: &[u8]) -> bool {
    source.windows(b"Required".len()).any(|w| w == b"Required")
}

/// `Literal` の出現を素朴に見る安価な前段フィルタ。最終判定は AST と名前解決で行う。
fn mentions_literal_qualifier(source: &[u8]) -> bool {
    source.windows(b"Literal".len()).any(|w| w == b"Literal")
}

/// 変更ファイルの新旧 exported シンボルから作る突き合わせ用インデックス。
///
/// 同名シンボルが旧/新いずれかに複数存在する場合、`HashMap<name, sig>` は最後の 1 件しか
/// 保持できず、別のオーバーロードや誤パースされた定義同士を突き合わせて api.mod を
/// 誤検出する。出現回数も併せて持ち、複数あるシンボルは曖昧として modified 判定から除外する
/// (Issue #13: C++ overload / マクロ誤パースの api.mod 誤検出対策)。
struct SymbolMaps<'a> {
    old_map: std::collections::HashMap<&'a str, &'a str>,
    new_map: std::collections::HashMap<&'a str, (&'a str, &'a str)>,
    old_name_counts: std::collections::HashMap<&'a str, usize>,
    new_name_counts: std::collections::HashMap<&'a str, usize>,
    /// Rust のとき、旧新を宣言の場所 (inline module の経路・qualname・種類) で組にした結果
    /// (Issue #282)。あれば追加・削除・変更は名前ではなくこれで決める。
    rust: Option<RustPairing>,
}

impl<'a> SymbolMaps<'a> {
    fn build(facts: &ModifiedFileFacts<'a>) -> Self {
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
fn same_name_surplus_mask(
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
fn collect_added_symbols(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
    maps: &SymbolMaps<'_>,
) -> std::collections::HashSet<String> {
    let &DetectionInputs {
        dir,
        diff_new_paths,
        ref_index,
        ..
    } = inputs;
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
            state.buckets.all_new_candidates.push(candidate.clone());
            if is_binary_rust_crate {
                continue;
            }
            if is_rust_new_symbol_outside_public_api_surface(
                dir,
                &df.new_path,
                name,
                state.rust_public,
            ) {
                continue;
            }
            if is_internally_connected(in_file_callees, name) {
                continue;
            }
            if is_used_in_diff_paths(ref_index, dir, name, &df.new_path, diff_new_paths) {
                continue;
            }
            state.buckets.added.push(candidate);
        }
    }
    new_symbols_in_current_file
}

/// 新ツリーに存在しない削除シンボルを `removed` / `property_to_field` に振り分ける。
fn collect_removed_symbols(
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
            if is_rust_old_symbol_outside_public_api_surface(
                dir,
                base_blobs,
                &df.old_path,
                name,
                state.rust_public,
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
                        state.buckets.modified.push(ApiSymbolChange {
                            inline_module: None,
                            unverified: None,
                            name: name.clone(),
                            kind: kind.clone(),
                            file: df.new_path.clone(),
                            old_signature: Some(sig.clone()),
                            new_signature: signatures
                                .as_ref()
                                .and_then(|map| map.get(name))
                                .cloned(),
                            no_resolved_internal_callers: false,
                            contract_change: None,
                        });
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
fn collect_modified_symbols(
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
                SignatureDecision::Blocking(None)
            } else {
                match type_annotation_decision(&site, &mut sources) {
                    Some(decision) => decision,
                    None => static_const_decision(&site, &mut sources)
                        .unwrap_or(SignatureDecision::Omit),
                }
            };
            let change = ApiSymbolChange {
                inline_module: None,
                unverified: None,
                name: name.clone(),
                kind: kind.clone(),
                file: df.new_path.clone(),
                old_signature: Some(old_sig.to_string()),
                new_signature: Some(new_sig.clone()),
                no_resolved_internal_callers: false,
                contract_change: None,
            };
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

/// Rust の変更ファイルで、宣言の場所 (inline module の経路・qualname・種類) で組にした旧新の
/// 宣言を比べる (Issue #282)。同じキーの宣言が複数あって組にできず、cfg の条件と signature の
/// 対応が変わったものは、比べられない変更として blocking にする。
fn collect_rust_modified_symbols(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
    pairing: &RustPairing,
    declarations: &RustDeclarationPair,
    files: &mut ChangedSignatureFiles<'_>,
) {
    // 契約が名指す名前の束縛を旧新で比べる (Issue #283)。旧新のソースを読めて構文エラーが無い
    // ときだけ比べる。同じ package の lib crate で束縛が変わったときは、ファイルの外へ出るパスも
    // 版ごとにほかのファイルの束縛で解く (Issue #300)。
    let pair = inputs
        .rust_crates
        .pair(inputs.dir, &df.new_path)
        .or_else(|| inputs.rust_crates.pair(inputs.dir, &df.old_path));
    let versioned = pair.and_then(|pair| {
        let files = pair.current_files(&df.old_path, &df.new_path)?;
        Some((pair, files))
    });
    let resolvers = match &versioned {
        Some((pair, (old, new))) => Some((
            Resolver::new(
                Some(&pair.old as &dyn CrateFiles),
                old.clone(),
                None,
                Some(&pair.changes),
            ),
            Resolver::new(
                Some(&pair.new as &dyn CrateFiles),
                new.clone(),
                Some(&pair.moves),
                Some(&pair.changes),
            ),
        )),
        None => {
            // 束縛が変わった crate で、比べるファイルの module を版ごとに決められなければ、変わった
            // 束縛に触れるパスは同じと言えない (lib の crate の外と分かれば、今までどおり字面で比べる)。
            let unplaced = pair
                .filter(|pair| pair.may_contain(&df.old_path, &df.new_path))
                .map(|pair| &pair.changes);
            let textual = |index: DependencyIndex| {
                Resolver::new(None, std::rc::Rc::new(index), None, unplaced)
            };
            inputs.base_blobs.read(&df.old_path).and_then(|old_source| {
                let new_source = load_new_source(inputs.dir, &df.new_path)?;
                Some((
                    textual(DependencyIndex::new(&old_source, &df.old_path)?),
                    textual(DependencyIndex::new(&new_source, &df.new_path)?),
                ))
            })
        }
    };
    // 定義を差分の中のほかのファイルへ移したかは、そのファイルの旧新を引いて確かめる (索引は
    // 引くときに作る)。
    let modules = DiffModules::new(
        &df.new_path,
        inputs.diff_files,
        Box::new(|path: &str| inputs.base_blobs.read(path)),
        Box::new(|path: &str| load_new_source(inputs.dir, path).map(|source| source.to_vec())),
    );
    let dependency = |olds: &[usize], news: &[usize]| {
        let (old_resolver, new_resolver) = resolvers.as_ref()?;
        let old: Vec<RustDeclarationContext> =
            olds.iter().map(|&i| declarations.old[i].clone()).collect();
        let new: Vec<RustDeclarationContext> =
            news.iter().map(|&j| declarations.new[j].clone()).collect();
        compare_dependencies(old_resolver, &old, new_resolver, &new, &modules).unverified_reason()
    };
    for &(old_index, new_index) in &pairing.pairs {
        let (name, _, old_sig) = &facts.old_syms[old_index];
        let (_, kind, new_sig) = &facts.new_syms[new_index];
        let dependency = dependency(&[old_index], &[new_index]);
        // signature が同じ組は、名指す名前の束縛も同じなら報告しない (型注釈と静的 container の
        // 段階は TS・JS だけ)。
        if dependency.is_none()
            && (old_sig == new_sig || facts.parameter_pattern_only_names.contains(name))
        {
            continue;
        }
        let representative = pairing
            .branch_representatives
            .contains(&(old_index, new_index));
        // `#[cfg]` の分岐の代表の組の構造体・enum・union は、分岐の組ごとに形を比べる (Issue #293)。
        let shape_override =
            (representative && matches!(kind.as_str(), "struct" | "enum" | "union"))
                .then(|| {
                    let group = pairing.groups.iter().find(|(olds, news)| {
                        olds.contains(&old_index) && news.contains(&new_index)
                    })?;
                    let old_source = inputs.base_blobs.read(&df.old_path)?;
                    let new_source = load_new_source(inputs.dir, &df.new_path)?;
                    let old: Vec<&RustDeclarationContext> =
                        group.0.iter().map(|&i| &declarations.old[i]).collect();
                    let new: Vec<&RustDeclarationContext> =
                        group.1.iter().map(|&j| &declarations.new[j]).collect();
                    branch_shape_change(kind, &old_source, &new_source, &old, &new)
                })
                .map(|change| change.unwrap_or(super::rust_type_api::ShapeChange::Breaking));
        let site = RustDeclarationSite {
            old_start: declarations.old[old_index].start,
            new_start: declarations.new[new_index].start,
            name_shared: pairing.name_is_shared(name),
            callee_shared: declarations.new_shared_callables.contains(bare_name(name)),
            dependency,
            availability_compared: !representative,
            shape_override,
        };
        let inline_module = declarations.new_inline_module(new_index);
        files.classify_rust(
            inputs,
            state,
            name,
            kind,
            old_sig,
            new_sig,
            site,
            inline_module,
        );
    }
    // 同じ場所の分岐で変わっていないとみなしたものも、名指す名前の束縛は分岐ごとに比べる。
    for (olds, news) in &pairing.unchanged_groups {
        let Some(dependency) = dependency(olds, news) else {
            continue;
        };
        let (Some(&old_index), Some(&new_index)) = (olds.first(), news.first()) else {
            continue;
        };
        let (name, _, old_sig) = &facts.old_syms[old_index];
        let (_, kind, new_sig) = &facts.new_syms[new_index];
        let site = RustDeclarationSite {
            old_start: declarations.old[old_index].start,
            new_start: declarations.new[new_index].start,
            name_shared: true,
            callee_shared: true,
            dependency: Some(dependency),
            availability_compared: false,
            shape_override: None,
        };
        let inline_module = declarations.new_inline_module(new_index);
        files.classify_rust(
            inputs,
            state,
            name,
            kind,
            old_sig,
            new_sig,
            site,
            inline_module,
        );
    }
    if files.skip_for_binary_crate {
        return;
    }
    for (olds, _) in &pairing.ambiguous {
        let Some(&first) = olds.first() else {
            continue;
        };
        let (name, kind, _) = &facts.old_syms[first];
        let change = ApiSymbolChange {
            inline_module: declarations.old_inline_module(first),
            unverified: None,
            name: name.clone(),
            kind: kind.clone(),
            file: df.new_path.clone(),
            old_signature: None,
            new_signature: None,
            no_resolved_internal_callers: false,
            contract_change: None,
        };
        record_signature_decision(
            inputs,
            state,
            change,
            SignatureDecision::Unverified(ApiModificationUnverified {
                reason: ApiModificationUnverifiedReason::RustDeclarationPairingAmbiguous,
                first_unproven: None,
            }),
        );
    }
}

/// Rust の変更ファイルで、旧新の両方に宣言のある項目の有る設定を比べる (Issue #291)。項目の API の
/// signature は項目自身の `#[cfg]` を含まないので、signature の比較とは別に、同じキーの宣言
/// (`#[cfg]` の分岐をまとめて) の有る条件を比べる。旧版で有った設定の一部から消えた項目は削除と
/// 同じ区分の候補にし、無かった設定に増えた項目は追加と同じ絞り込みを通して追加の候補にする
/// (別の条件への付け替えは両方)。比べられなければ blocking の変更にする。
fn collect_rust_availability_changes(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    df: &crate::models::impact::DiffFile,
    facts: &ModifiedFileFacts<'_>,
    pairing: &RustPairing,
    declarations: &RustDeclarationPair,
) {
    use super::rust_availability::{AvailabilityChange, compare, describe};
    let &DetectionInputs {
        dir,
        base_blobs,
        diff_new_paths,
        ref_index,
        ..
    } = inputs;
    if is_binary_only_rust_crate(dir, &df.new_path) {
        return;
    }
    for (olds, news) in &pairing.groups {
        // module の宣言の条件の変化で比べ直した項目は、置き場の条件を含めて比べてある (Issue #305)。
        fn described<'a>(
            indices: &[usize],
            symbols: &'a [(String, String, String)],
            contexts: &'a [RustDeclarationContext],
        ) -> Vec<(&'a RustDeclarationContext, &'a str, &'a str)> {
            indices
                .iter()
                .map(|&k| (&contexts[k], symbols[k].0.as_str(), symbols[k].1.as_str()))
                .collect()
        }
        if inputs.module_availability.covers_availability(
            df,
            &described(olds, facts.old_syms, &declarations.old),
            &described(news, facts.new_syms, &declarations.new),
        ) {
            continue;
        }
        let old: Vec<&RustDeclarationContext> =
            olds.iter().map(|&i| &declarations.old[i]).collect();
        let new: Vec<&RustDeclarationContext> =
            news.iter().map(|&j| &declarations.new[j]).collect();
        let change = compare(&old, &new);
        if change == AvailabilityChange::Same {
            continue;
        }
        let (Some(&first_old), Some(&first_new)) = (olds.first(), news.first()) else {
            continue;
        };
        let (name, kind, old_sig) = &facts.old_syms[first_old];
        let new_sig = &facts.new_syms[first_new].2;
        let candidate = ApiSymbolCandidate {
            inline_module: declarations.new_inline_module(first_new),
            name: name.clone(),
            kind: kind.clone(),
            file: df.new_path.clone(),
            signature: new_sig.clone(),
        };
        let removable = !is_rust_old_symbol_outside_public_api_surface(
            dir,
            base_blobs,
            &df.old_path,
            name,
            state.rust_public,
        );
        let item = AvailabilityItem {
            candidate,
            old_signature: old_sig.clone(),
            availability: describe(&old, &new),
            removable,
        };
        record_availability_change(inputs, state, change, item, |state| {
            !is_rust_new_symbol_outside_public_api_surface(
                dir,
                &df.new_path,
                name,
                state.rust_public,
            ) && !is_internally_connected(facts.in_file_callees, name)
                && !is_used_in_diff_paths(ref_index, dir, name, &df.new_path, diff_new_paths)
        });
    }
}

/// 有る設定の比較 (Issue #291) で振り分ける項目。
struct AvailabilityItem {
    /// 新しい版の項目 (削除・追加の候補にする)。
    candidate: ApiSymbolCandidate,
    /// 旧い版の signature (比べられなかったときの変更に添える)。
    old_signature: String,
    /// 旧新の有る条件の表示。
    availability: crate::models::review::CfgAvailability,
    /// 旧い版で公開 API 面にあり、消える設定を削除と同じ区分の候補にしてよいか。
    removable: bool,
}

/// 有る設定の比較の結果 (Issue #291) を、変更ファイルと移動で共通の規則で振り分ける。消える設定が
/// あれば削除と同じ区分の候補、増える設定があれば (`addable` なら) 追加の候補、比べられなければ
/// blocking の変更にする。
fn record_availability_change(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    change: super::rust_availability::AvailabilityChange,
    item: AvailabilityItem,
    addable: impl FnOnce(&mut DetectionState<'_>) -> bool,
) {
    use super::rust_availability::AvailabilityChange;
    let AvailabilityItem {
        candidate,
        old_signature,
        availability,
        removable,
    } = item;
    match change {
        AvailabilityChange::Same => {}
        AvailabilityChange::Unverified => {
            if !removable {
                return;
            }
            let change = ApiSymbolChange {
                inline_module: candidate.inline_module,
                unverified: None,
                name: candidate.name,
                kind: candidate.kind,
                file: candidate.file,
                old_signature: Some(old_signature),
                new_signature: Some(candidate.signature),
                no_resolved_internal_callers: false,
                contract_change: None,
            };
            record_signature_decision(
                inputs,
                state,
                change,
                SignatureDecision::Unverified(ApiModificationUnverified {
                    reason: ApiModificationUnverifiedReason::RustCfgAvailabilityUnverified,
                    first_unproven: None,
                }),
            );
        }
        AvailabilityChange::Changed { lost, gained } => {
            if lost && removable {
                state
                    .buckets
                    .cfg_removed
                    .push((candidate.clone(), availability.clone()));
            }
            if gained && addable(state) {
                state.buckets.cfg_added.push((candidate, availability));
            }
        }
    }
}

/// module の宣言の条件が変わった module の項目の比べ直しの結果 (Issue #305) を、ファイルごとの比較と
/// 同じ区分に振り分ける。ファイルごとの比較が同じ項目を出していれば重ねない。
pub(crate) fn record_module_availability(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
) {
    use super::rust_availability::AvailabilityChange;
    use super::rust_module_availability::Outcome;
    // 同じ項目の同じ条件の報告は重ねない (同じファイルを 2 つの module に使えば、条件の違う報告は別に
    // 残す)。
    let identity = |candidate: &ApiSymbolCandidate,
                    availability: &crate::models::review::CfgAvailability| {
        format!(
            "{}\u{0}{:?}\u{0}{}\u{0}{}\u{0}{:?}",
            candidate.file, candidate.inline_module, candidate.name, candidate.kind, availability
        )
    };
    let mut reported_removed: std::collections::HashSet<String> = state
        .buckets
        .cfg_removed
        .iter()
        .map(|(candidate, availability)| identity(candidate, availability))
        .collect();
    let mut reported_added: std::collections::HashSet<String> = state
        .buckets
        .cfg_added
        .iter()
        .map(|(candidate, availability)| identity(candidate, availability))
        .collect();
    let mut callees: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for outcome in inputs.module_availability.outcomes() {
        match outcome {
            Outcome::Availability {
                change,
                candidate,
                old_signature,
                availability,
                removable,
            } => {
                let reported = identity(candidate, availability);
                let change = match *change {
                    AvailabilityChange::Changed { lost, gained } => {
                        let lost = lost && reported_removed.insert(reported.clone());
                        let gained = gained && reported_added.insert(reported);
                        if !lost && !gained {
                            continue;
                        }
                        AvailabilityChange::Changed { lost, gained }
                    }
                    other => other,
                };
                let item = AvailabilityItem {
                    candidate: candidate.clone(),
                    old_signature: old_signature.clone(),
                    availability: availability.clone(),
                    removable: *removable,
                };
                let name = candidate.name.clone();
                let file = candidate.file.clone();
                record_availability_change(inputs, state, change, item, |_| {
                    let callees = callees.entry(file.clone()).or_insert_with(|| {
                        super::prepare::extract_new_file_facts(
                            inputs.dir,
                            &file,
                            inputs.strip_internal,
                        )
                        .callees
                    });
                    !is_internally_connected(callees, &name)
                        && !is_used_in_diff_paths(
                            inputs.ref_index,
                            inputs.dir,
                            &name,
                            &file,
                            inputs.diff_new_paths,
                        )
                });
            }
            Outcome::Ambiguous {
                candidate,
                old_signature,
            } => {
                let change = ApiSymbolChange {
                    inline_module: candidate.inline_module.clone(),
                    unverified: None,
                    name: candidate.name.clone(),
                    kind: candidate.kind.clone(),
                    file: candidate.file.clone(),
                    old_signature: Some(old_signature.clone()),
                    new_signature: Some(candidate.signature.clone()),
                    no_resolved_internal_callers: false,
                    contract_change: None,
                };
                record_signature_decision(
                    inputs,
                    state,
                    change,
                    SignatureDecision::Unverified(ApiModificationUnverified {
                        reason: ApiModificationUnverifiedReason::RustDeclarationPairingAmbiguous,
                        first_unproven: None,
                    }),
                );
            }
        }
    }
}

/// 別のファイルへ移した Rust の項目 (`moved`) の、移動元 (base) と移動先 (作業ツリー) の宣言 (同じ
/// 場所の `#[cfg]` の分岐をまとめて) の有る設定を比べる (Issue #291)。
pub(crate) fn compare_moved_availability(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    moved: &[crate::models::review::MovedSymbol],
) {
    type Facts = Option<(ExportedSymbols, Vec<RustDeclarationContext>)>;
    /// 宣言と、その (名前, 種類, signature)。
    type Declarations<'a> = Vec<(&'a (String, String, String), &'a RustDeclarationContext)>;
    let rust = |path: &str| {
        crate::language::LangId::from_path(camino::Utf8Path::new(path)).ok()
            == Some(crate::language::LangId::Rust)
    };
    let facts = |path: &str, source: Option<Vec<u8>>| -> Facts {
        let (symbols, declarations) =
            extract_deleted_exported_facts(path, &source?, inputs.strip_internal)?;
        Some((symbols, declarations?))
    };
    let mut old_files: std::collections::HashMap<String, Facts> = std::collections::HashMap::new();
    let mut new_files: std::collections::HashMap<String, Facts> = std::collections::HashMap::new();
    // `#[cfg]` の分岐ごとに移動が並ぶ (signature ごとに 1 つ) ので、同じ項目は 1 度だけ比べる。
    let mut compared = std::collections::HashSet::new();
    for symbol in moved {
        if !rust(&symbol.from) || !rust(&symbol.to) {
            continue;
        }
        if !compared.insert((
            &symbol.from,
            &symbol.to,
            &symbol.name,
            &symbol.kind,
            &symbol.from_inline_module,
            &symbol.to_inline_module,
        )) {
            continue;
        }
        let old = old_files
            .entry(symbol.from.clone())
            .or_insert_with(|| facts(&symbol.from, inputs.base_blobs.read(&symbol.from)));
        let new = new_files.entry(symbol.to.clone()).or_insert_with(|| {
            facts(
                &symbol.to,
                load_new_source(inputs.dir, &symbol.to).map(|source| source.to_vec()),
            )
        });
        /// 同じ名前・種類・inline module の宣言 (`#[cfg]` の分岐を含む) と、その signature。
        fn select<'a>(
            facts: &'a Facts,
            symbol: &crate::models::review::MovedSymbol,
            inline_module: &Option<String>,
        ) -> Option<Declarations<'a>> {
            let (symbols, declarations) = facts.as_ref()?;
            let selected: Declarations<'a> = symbols
                .iter()
                .zip(declarations)
                .filter(|((name, kind, _), declaration)| {
                    *name == symbol.name
                        && *kind == symbol.kind
                        && inline_module_of(declaration) == *inline_module
                })
                .collect();
            (!selected.is_empty()).then_some(selected)
        }
        let (Some(old_side), Some(new_side)) = (
            select(old, symbol, &symbol.from_inline_module),
            select(new, symbol, &symbol.to_inline_module),
        ) else {
            continue;
        };
        // 宣言を (impl の対象の型, signature) ごとに組にする。同じ名前の別の型のメソッド (`S<u8>` と
        // `S<u16>`) や、`#[cfg]` の分岐の間で signature を入れ替えた移動を混ぜない。同じ組が両方に無い
        // 宣言は、型の書き方を変えた移動 (`crate::S<u8>` と `super::S<u8>`) として signature だけで組に
        // する。それでも一意に決まらず、条件の付いた (か読めない) 宣言があれば、比べられなかったものと
        // する。片方にしかない signature の宣言 (移動先に元からある別のメソッドなど) は比べない。
        type Key = (Vec<String>, String);
        let keyed = |side: &Declarations<'_>| {
            let mut keyed: std::collections::BTreeMap<Key, Vec<usize>> =
                std::collections::BTreeMap::new();
            for (index, (symbol, declaration)) in side.iter().enumerate() {
                keyed
                    .entry((declaration.owner.clone(), symbol.2.clone()))
                    .or_default()
                    .push(index);
            }
            keyed
        };
        let (old_keyed, new_keyed) = (keyed(&old_side), keyed(&new_side));
        // 移動先で同じ型の宣言が同じ設定に並び (移動元では並ばなかった)、その設定でビルドできない
        // なら、組を決められない変更として blocking にする。
        let duplicated = new_keyed.keys().map(|(owner, _)| owner).any(|owner| {
            let of_owner = |side: &Declarations<'_>| {
                side.iter()
                    .filter(|(_, declaration)| declaration.owner == *owner)
                    .count()
            };
            let new_owner = new_side
                .iter()
                .filter(|(_, declaration)| declaration.owner == *owner)
                .map(|(_, declaration)| *declaration);
            let old_owner = old_side
                .iter()
                .filter(|(_, declaration)| declaration.owner == *owner)
                .map(|(_, declaration)| *declaration);
            of_owner(&new_side) > 1
                && !exclusive_declarations(new_owner)
                && exclusive_declarations(old_owner)
        });
        if duplicated {
            let change = ApiSymbolChange {
                inline_module: symbol.to_inline_module.clone(),
                unverified: None,
                name: symbol.name.clone(),
                kind: symbol.kind.clone(),
                file: symbol.to.clone(),
                old_signature: Some(old_side[0].0.2.clone()),
                new_signature: Some(new_side[0].0.2.clone()),
                no_resolved_internal_callers: false,
                contract_change: None,
            };
            record_signature_decision(
                inputs,
                state,
                change,
                SignatureDecision::Unverified(ApiModificationUnverified {
                    reason: ApiModificationUnverifiedReason::RustDeclarationPairingAmbiguous,
                    first_unproven: None,
                }),
            );
            continue;
        }
        let mut groups: Vec<(Vec<usize>, Vec<usize>)> = Vec::new();
        // signature ごとの、同じ組の無かった旧新の鍵。
        let mut unmatched: std::collections::BTreeMap<String, (Vec<&Key>, Vec<&Key>)> =
            std::collections::BTreeMap::new();
        for (key, olds) in &old_keyed {
            match new_keyed.get(key) {
                Some(news) => groups.push((olds.clone(), news.clone())),
                None => unmatched.entry(key.1.clone()).or_default().0.push(key),
            }
        }
        for key in new_keyed.keys().filter(|key| !old_keyed.contains_key(*key)) {
            if let Some((_, news)) = unmatched.get_mut(&key.1) {
                news.push(key);
            }
        }
        let conditional = |declaration: &RustDeclarationContext| {
            declaration
                .availability
                .as_ref()
                .is_none_or(|attributes| !attributes.is_empty())
        };
        for (old_keys, new_keys) in unmatched.into_values() {
            if new_keys.is_empty() {
                continue;
            }
            let olds: Vec<usize> = old_keys
                .iter()
                .flat_map(|key| old_keyed[*key].iter().copied())
                .collect();
            let news: Vec<usize> = new_keys
                .iter()
                .flat_map(|key| new_keyed[*key].iter().copied())
                .collect();
            if old_keys.len() == 1 && new_keys.len() == 1 {
                groups.push((olds, news));
            } else if olds.iter().any(|&index| conditional(old_side[index].1))
                || news.iter().any(|&index| conditional(new_side[index].1))
            {
                groups.push((olds, Vec::new()));
            }
        }
        for (olds, news) in groups {
            let old_declarations: Vec<&RustDeclarationContext> =
                olds.iter().map(|&index| old_side[index].1).collect();
            let new_declarations: Vec<&RustDeclarationContext> =
                news.iter().map(|&index| new_side[index].1).collect();
            // 組を決められなかったとき (`news` が空) は、比べられなかったものとする。
            let change = if new_declarations.is_empty() {
                super::rust_availability::AvailabilityChange::Unverified
            } else {
                super::rust_availability::compare(&old_declarations, &new_declarations)
            };
            if change == super::rust_availability::AvailabilityChange::Same {
                continue;
            }
            let item = AvailabilityItem {
                candidate: ApiSymbolCandidate {
                    inline_module: symbol.to_inline_module.clone(),
                    name: symbol.name.clone(),
                    kind: symbol.kind.clone(),
                    file: symbol.to.clone(),
                    signature: news.first().map_or_else(
                        || old_side[olds[0]].0.2.clone(),
                        |&index| new_side[index].0.2.clone(),
                    ),
                },
                old_signature: old_side[olds[0]].0.2.clone(),
                availability: super::rust_availability::describe(
                    &old_declarations,
                    &new_declarations,
                ),
                removable: true,
            };
            record_availability_change(inputs, state, change, item, |_| {
                !is_used_in_diff_paths(
                    inputs.ref_index,
                    inputs.dir,
                    &symbol.name,
                    &symbol.to,
                    inputs.diff_new_paths,
                )
            });
        }
    }
}

/// 型の移動と同じ差分でシグネチャの変わったメンバーを、rename した変更ファイルと同じ規則で
/// 判定する。移動元を旧ファイル、移動先を新ファイルとして扱う。組の一意性は
/// [`pair_moved_member_changes`] が確かめている。
pub(crate) fn process_moved_member_changes(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    changes: &[MovedMemberChange<'_>],
) {
    // (移動元, 移動先) ごとに文脈を 1 つ作り、型別名の読み込みなどを共有する。
    let mut files: Vec<ChangedSignatureFiles<'_>> = Vec::new();
    for change in changes {
        let (old_path, new_path) = (change.old.file.as_str(), change.new.file.as_str());
        let ix = match files
            .iter()
            .position(|files| files.old_path == old_path && files.new_path == new_path)
        {
            Some(ix) => ix,
            None => {
                let mut moved = ChangedSignatureFiles::new(
                    inputs,
                    state,
                    old_path,
                    new_path,
                    change.new_in_file_callees,
                );
                moved.moved_from = Some(old_path);
                files.push(moved);
                files.len() - 1
            }
        };
        files[ix].classify(
            inputs,
            state,
            &change.new.name,
            &change.new.kind,
            &change.old.signature,
            &change.new.signature,
        );
    }
}

/// 再公開の先と組にして比べる、Rust の宣言の種類。マクロ・module は扱わない (公開の規則が違う)。
const REEXPORT_COMPARED_KINDS: [&str; 7] = [
    "function", "constant", "struct", "enum", "union", "trait", "type",
];

/// 型の同一性を持つ宣言の種類。差分の外の同じ形の宣言や、ほかの旧い型と共有する宣言を再公開
/// しても、別の型になるので壊す (利用側の trait の実装が重なる、など)。
const NOMINAL_KINDS: [&str; 4] = ["struct", "enum", "union", "trait"];

/// 作業ツリーの `file` の、`start` (行, 列) に始まる名前が `name` の宣言の API の signature。
fn declaration_signature_at(
    dir: &str,
    file: &str,
    name: &str,
    start: (usize, usize),
) -> Option<String> {
    let source = load_new_source(dir, file)?;
    let tree = crate::engine::parser::parse_source(&source, crate::language::LangId::Rust).ok()?;
    let root = tree.root_node();
    let symbols =
        crate::engine::symbols::extract_symbols(root, &source, crate::language::LangId::Rust)
            .ok()?;
    let declarations =
        unfiltered_declaration_signatures(&symbols, root, &source, crate::language::LangId::Rust);
    let mut matching = symbols
        .iter()
        .zip(declarations)
        .filter(|(symbol, (qualname, _, _))| {
            qualname == name && (symbol.range.start.line, symbol.range.start.column) == start
        });
    match (matching.next(), matching.next()) {
        (Some((_, (_, _, signature))), None) => Some(signature),
        _ => None,
    }
}

/// 再公開の先と組にできた旧い宣言。
struct ReexportPair {
    target: ReexportTarget,
    /// 先の宣言の API の signature。
    new_signature: String,
    /// 先の型を足したファイルで、base にあった同じ名前の型の束縛が消えたか変わった (型をまとめた
    /// かもしれない)。
    merged: bool,
}

/// 作業ツリーのパス `path` の base での中身。diff の rename (内容同一の rename を含む) は旧いパスを
/// 読み、同じ差分で足したファイルは無いとする。
fn base_file_of(
    inputs: &DetectionInputs<'_>,
    renames: &[crate::models::impact::DiffFile],
    path: &str,
) -> BaseFile {
    let entry =
        inputs.diff_files.iter().chain(renames).find(|file| {
            crate::git_support::normalize_workspace_separators(&file.new_path) == path
        });
    let old_path = match entry {
        Some(file) if file.old_path == "/dev/null" => return BaseFile::Missing,
        Some(file) => file.old_path.as_str(),
        None => path,
    };
    inputs
        .base_blobs
        .read(old_path)
        .map_or(BaseFile::Unknown, BaseFile::Content)
}

/// 旧い宣言 `old` (再公開したファイル `reexporting_file`) を再公開の先の宣言と組にする。旧い宣言が
/// 旧いファイルの直下の、その名前の唯一の宣言である (inline module の中の宣言は別のパスなので
/// 組にしない。削除の候補は宣言の場所で組にした結果なので、どの宣言かは `inline_module` で分かる)
/// こと、先を解けて種類が同じこと、先の宣言の signature と base での有無を決められることを求める。
fn pair_reexported_removal(
    inputs: &DetectionInputs<'_>,
    renames: &[crate::models::impact::DiffFile],
    old: &ApiSymbolCandidate,
    reexporting_file: &str,
) -> Option<ReexportPair> {
    // 旧いファイルと再公開したファイル (rename した新しいパス) は、同じ crate の同じ module
    // (旧いパスが同じ)。
    if source_module_path(&old.file)? != source_module_path(reexporting_file)? {
        return None;
    }
    let old_source = inputs.base_blobs.read(&old.file)?;
    if old.inline_module.is_some()
        || declaration_in_scope(&old_source, &[], &old.name)? != Some(old.kind.as_str())
    {
        return None;
    }
    let target = resolve_reexport_target(inputs.dir, reexporting_file, &old.name, &|path| {
        base_file_of(inputs, renames, path)
    })?;
    if target.kind != old.kind {
        return None;
    }
    let new_signature =
        declaration_signature_at(inputs.dir, &target.file, &old.name, target.start)?;
    // 型を同じ差分で足したなら、先のファイルの base の module のスコープ (直下と inline module の
    // 中) で型の名前空間にその名前を束縛し得たもの (型の宣言・型別名・可視性付きの `use`・item 位置の
    // マクロ) が、新しい版の同じ module に同じ字面ですべて残っていること。消えたか変わったものが
    // あれば、別の型を指していたパスが移した型を指すようになった (型をまとめた) かもしれない。
    // 同じファイルの同じ名前の宣言どうしの変更は、名前で組にできず比べられないので、ここで止める。
    let merged = NOMINAL_KINDS.contains(&old.kind.as_str())
        && target.added_in_diff
        && match base_file_of(inputs, renames, &target.file) {
            BaseFile::Missing => false,
            BaseFile::Content(base) => !type_namespace_bindings_kept(
                &base,
                load_new_source(inputs.dir, &target.file).as_deref(),
                &old.name,
            ),
            BaseFile::Unknown => true,
        };
    Some(ReexportPair {
        target,
        new_signature,
        merged,
    })
}

/// 再公開の組の、旧新の宣言の場所と名前の共有。読めなければ名前を共有するとみなす (名前で引く
/// 判定を使わない)。
fn reexport_site(
    inputs: &DetectionInputs<'_>,
    old: &ApiSymbolCandidate,
    target: &ReexportTarget,
) -> RustDeclarationSite {
    let old_source = inputs.base_blobs.read(&old.file);
    let new_source = load_new_source(inputs.dir, &target.file);
    let old_start = old_source
        .as_deref()
        .and_then(|source| root_declaration(source, &old.name))
        .map_or((usize::MAX, usize::MAX), |(start, _)| start);
    let declarations = |source: Option<&[u8]>| {
        source.and_then(|source| module_scope_declarations(source, &old.name))
    };
    let name_shared = [
        declarations(old_source.as_deref()),
        declarations(new_source.as_deref()),
    ]
    .iter()
    .any(|count| count.is_none_or(|count| count > 1));
    let callee_shared = new_source
        .as_deref()
        .and_then(|source| callable_declarations(source, bare_name(&old.name)))
        .is_none_or(|count| count > 1);
    RustDeclarationSite {
        old_start,
        new_start: target.start,
        name_shared,
        callee_shared,
        dependency: None,
        availability_compared: false,
        shape_override: None,
    }
}

/// 項目を別のファイルへ移して元のファイルから `pub use` で再公開した差分の、旧い宣言
/// (`removals`、再公開したファイルと組) を再公開の先の宣言と組にして比べる (Issue #281)。先は
/// 同じ名前の候補から推測せず、経路から解き ([`resolve_reexport_target`])、その宣言そのものの
/// signature と比べる。
///
/// - 先が同じ差分で足した宣言なら、シグネチャが同じなら移動 (`moved`)。違えば移さない場合と同じ
///   規則で判定し、壊す変更でなければ移動にもする。どちらも先の追加は `added` から外す
/// - 構造体・enum・union・trait は、先が差分の外からある宣言か、ほかの旧い宣言と同じ先を共有する
///   (通常の移動でその先へ来た型を含む) なら、型の同一性が変わるので壊す変更
/// - 関数・定数・型別名の先が差分の外からある宣言なら、シグネチャが同じなら変更なし、違えば移さない
///   場合と同じ規則で判定する
/// - 組にできなければ、対応を確かめられないものとして blocking な api.mod
///   (`reexport_target_unresolved`) に残す
pub(crate) fn process_reexported_removals(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    removals: &[(ApiSymbolCandidate, String)],
    renames: &[crate::models::impact::DiffFile],
    mut added: Vec<ApiSymbolCandidate>,
    mut moved: Vec<crate::models::review::MovedSymbol>,
    files: &MemberFileFacts<'_>,
) -> (
    Vec<ApiSymbolCandidate>,
    Vec<crate::models::review::MovedSymbol>,
) {
    let pairs: Vec<Option<ReexportPair>> = removals
        .iter()
        .map(|(old, reexporting_file)| {
            pair_reexported_removal(inputs, renames, old, reexporting_file)
        })
        .collect();
    // 通常の移動でファイルへ来た宣言 (名前, 種類, 先のファイル)。型なら、再公開の先と共有し得る。
    let moved_into: std::collections::HashSet<(String, String, String)> = moved
        .iter()
        .map(|symbol| (symbol.name.clone(), symbol.kind.clone(), symbol.to.clone()))
        .collect();
    // 同じ先の宣言を共有する旧い宣言の数 (型を 1 つにまとめると、別だった型が同じ型になる)。
    let mut sharing: std::collections::HashMap<(&str, (usize, usize)), usize> =
        std::collections::HashMap::new();
    for pair in pairs.iter().flatten() {
        *sharing
            .entry((pair.target.file.as_str(), pair.target.start))
            .or_default() += 1;
    }
    let no_callees = std::collections::HashSet::new();
    let mut moved_files = MovedSymbolFiles::new(inputs.dir, inputs.base_blobs);
    for ((old, reexporting_file), pair) in removals.iter().zip(&pairs) {
        let blocking = |new_signature: Option<String>, unverified: bool| ApiSymbolChange {
            inline_module: None,
            unverified: unverified.then_some(ApiModificationUnverified {
                reason: ApiModificationUnverifiedReason::ReexportTargetUnresolved,
                first_unproven: None,
            }),
            name: old.name.clone(),
            kind: old.kind.clone(),
            file: reexporting_file.clone(),
            old_signature: Some(old.signature.clone()),
            new_signature,
            no_resolved_internal_callers: false,
            contract_change: None,
        };
        let Some(pair) = pair else {
            state.buckets.modified.push(blocking(None, true));
            continue;
        };
        let target = &pair.target;
        // 先の追加の候補は、先の宣言の signature まで同じものだけ (同じファイルの別の module の
        // 同じ名前の宣言を外さない)。
        let is_destination = |candidate: &ApiSymbolCandidate| {
            candidate.name == old.name
                && candidate.kind == old.kind
                && candidate.file == target.file
                && candidate.signature == pair.new_signature
        };
        if NOMINAL_KINDS.contains(&old.kind.as_str())
            && (!target.added_in_diff
                || pair.merged
                || sharing[&(target.file.as_str(), target.start)] > 1
                || moved_into.contains(&(old.name.clone(), old.kind.clone(), target.file.clone())))
        {
            state
                .buckets
                .modified
                .push(blocking(Some(pair.new_signature.clone()), false));
            if target.added_in_diff {
                added.retain(|candidate| !is_destination(candidate));
            }
            continue;
        }
        let moved_symbol = || crate::models::review::MovedSymbol {
            from_inline_module: old.inline_module.clone(),
            to_inline_module: (!target.inline.is_empty()).then(|| target.inline.join("::")),
            name: old.name.clone(),
            kind: old.kind.clone(),
            from: old.file.clone(),
            to: target.file.clone(),
        };
        if pair.new_signature == old.signature {
            if target.added_in_diff {
                added.retain(|candidate| !is_destination(candidate));
                moved.push(moved_symbol());
            }
            continue;
        }
        let callees = files
            .new
            .get(target.file.as_str())
            .map_or(&no_callees, |(_, callees)| *callees);
        let mut changed =
            ChangedSignatureFiles::new(inputs, state, &old.file, &target.file, callees);
        changed.moved_from = Some(&old.file);
        // 旧新の宣言そのものの場所で判定し、同じ名前の宣言がどちらかのファイルに複数あれば、名前で
        // 引く判定は使わない (Issue #282)。
        changed.rust_site = Some(reexport_site(inputs, old, target));
        // 関数に `const` を足しただけなら、通常の移動と同じ証明で互換 (#113・#114)。
        let destination = ApiSymbolCandidate {
            inline_module: None,
            name: old.name.clone(),
            kind: old.kind.clone(),
            file: target.file.clone(),
            signature: pair.new_signature.clone(),
        };
        if moved_files.const_addition_is_compatible(&MovedSymbolChange {
            old: old.clone(),
            new: destination,
            new_in_file_callees: callees,
            old_file_after: None,
            new_file_before: None,
        }) {
            changed.rust_moved_const_additions.insert(old.name.clone());
        }
        let (change, decision) = changed.decide(
            inputs,
            state,
            &old.name,
            &old.kind,
            &old.signature,
            &pair.new_signature,
        );
        let breaking = matches!(
            decision,
            SignatureDecision::Blocking(_) | SignatureDecision::Unverified(_)
        );
        record_signature_decision(inputs, state, change, decision);
        if target.added_in_diff {
            added.retain(|candidate| !is_destination(candidate));
            if !breaking {
                moved.push(moved_symbol());
            }
        }
    }
    (added, moved)
}

/// 自由関数・定数を別のファイルへ移した同じ差分でシグネチャの変わった組
/// ([`pair_moved_symbol_changes`]) を、rename した変更ファイルと同じ規則で判定する。移動元を
/// 旧ファイル、移動先を新ファイルとして扱う。
///
/// 宣言を構文で確かめ ([`MovedSymbolFiles::proves_declaration`])、判定が移動に使える証拠
/// ([`SignatureDecision::proves_compatible_move`]) で、契約部分に現れる名前が移動元と移動先で同じ
/// ものを指すと確かめられた組だけ、判定を bucket へ入れて返す。それ以外の組は何も入れない
/// (呼び出し側が今と同じ削除と追加に残す)。owner の移動のような対応の証拠が無いので、呼び出し側の
/// 追随で閉じる変更 (mod_closed) や api.mod の組を移動と見なすと、別物の削除を隠し得る。
pub(crate) fn process_moved_symbol_changes<'c, 'a>(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    changes: &'c [MovedSymbolChange<'a>],
) -> Vec<&'c MovedSymbolChange<'a>> {
    let mut sources = MovedSymbolFiles::new(inputs.dir, inputs.base_blobs);
    // (移動元, 移動先) ごとに文脈を 1 つ作り、型別名の読み込みなどを共有する。
    let mut files: Vec<ChangedSignatureFiles<'_>> = Vec::new();
    let mut file_index: std::collections::HashMap<(&str, &str), usize> =
        std::collections::HashMap::new();
    let mut accepted = Vec::new();
    for change in changes {
        if is_rust_new_symbol_outside_public_api_surface(
            inputs.dir,
            &change.new.file,
            &change.new.name,
            state.rust_public,
        ) || !sources.proves_declaration(change)
            || !sources.references_reach_destination(change, inputs.ref_index)
        {
            continue;
        }
        let (old_path, new_path) = (change.old.file.as_str(), change.new.file.as_str());
        let ix = *file_index.entry((old_path, new_path)).or_insert_with(|| {
            let mut pair = ChangedSignatureFiles::new(
                inputs,
                state,
                old_path,
                new_path,
                change.new_in_file_callees,
            );
            pair.moved_from = Some(old_path);
            files.push(pair);
            files.len() - 1
        });
        if sources.const_addition_is_compatible(change) {
            files[ix]
                .rust_moved_const_additions
                .insert(change.new.name.clone());
        }
        let (symbol_change, decision) = files[ix].decide(
            inputs,
            state,
            &change.new.name,
            &change.new.kind,
            &change.old.signature,
            &change.new.signature,
        );
        if !decision.proves_compatible_move() {
            continue;
        }
        // 値だけの変更は型を字面で比べるので、型に現れる名前が同じものを指すか別に確かめる
        // (型別名の証明は `prove_moved_function` の中で確かめる)。
        if matches!(decision, SignatureDecision::ConstValue)
            && !sources.constant_type_resolves_alike(change)
        {
            continue;
        }
        record_signature_decision(inputs, state, symbol_change, decision);
        accepted.push(change);
    }
    accepted
}

/// シグネチャの変わったシンボルを判定する、旧新のファイルの組ごとの文脈。
///
/// 変更ファイル (rename を含む) と、型の移動と同じ差分でシグネチャの変わったメンバー
/// ([`process_moved_member_changes`])、移動と同じ差分でシグネチャの変わった自由関数・定数
/// ([`process_moved_symbol_changes`]) が同じ規則で判定するために共有する。ファイル内の
/// 対応付け (同名の複数定義・引数パターンだけの変更) の検査は呼び出し側が済ませる。
struct ChangedSignatureFiles<'a> {
    type_dependencies: Option<&'a super::ts_signature::DependencyChanges>,
    old_path: &'a str,
    new_path: &'a str,
    /// 移動と同じ差分でシグネチャの変わった宣言なら移動元 (`old_path`)。
    moved_from: Option<&'a str>,
    in_file_callees: &'a std::collections::HashSet<String>,
    /// Rust bin-only crate 判定 (api.mod 抑制用)。lib → bin / bin → lib どちらかが bin-only なら
    /// 外部 API 面の変更ではないとみなす。
    skip_for_binary_crate: bool,
    /// 値バインディングの value-only 変更を const_value_changes へ振り分けるための言語判定。
    lang_id: Option<crate::language::LangId>,
    /// Rust の同一ファイル内 public type alias は、最初の候補で old/new source を 1 回だけ
    /// 読み込んで以降の候補へ再利用する。候補ごとの git show + parse を避ける。
    rust_type_aliases: Option<Option<crate::engine::rust_signature::RustTypeAliasEquivalence>>,
    /// 関数に `const` を足しただけと Phase 0 で証明した qualname。型の移動と同じ差分の
    /// メンバーは旧新で別ファイルを比べるので持たない。
    rust_const_additions: Option<&'a std::collections::HashSet<String>>,
    /// 自由関数の移動で、const 追加と別ファイルの型所有元まで証明した名前。
    rust_moved_const_additions: std::collections::HashSet<String>,
    /// 判定中の変更が、Rust の宣言を宣言の場所で組にしたものなら、その場所 (Issue #282)。
    rust_site: Option<RustDeclarationSite>,
}

impl<'a> ChangedSignatureFiles<'a> {
    fn new(
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        old_path: &'a str,
        new_path: &'a str,
        in_file_callees: &'a std::collections::HashSet<String>,
    ) -> Self {
        let skip_for_binary_crate = state
            .rust_public
            .is_binary_only_at_base(inputs.base_blobs, old_path)
            || is_binary_only_rust_crate(inputs.dir, new_path);
        Self {
            type_dependencies: None,
            old_path,
            new_path,
            moved_from: None,
            in_file_callees,
            skip_for_binary_crate,
            lang_id: crate::language::LangId::from_path(camino::Utf8Path::new(new_path)).ok(),
            rust_type_aliases: None,
            rust_const_additions: None,
            rust_moved_const_additions: std::collections::HashSet::new(),
            rust_site: None,
        }
    }

    /// Rust の宣言を宣言の場所で組にした変更 1 件を判定し、対応する bucket へ入れる。形は
    /// 組にした宣言そのものから読み、同じ名前の宣言がファイルに複数あれば、名前で引く判定は
    /// 使わない (Issue #282)。
    #[allow(clippy::too_many_arguments)]
    fn classify_rust(
        &mut self,
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        name: &str,
        kind: &str,
        old_sig: &str,
        new_sig: &str,
        site: RustDeclarationSite,
        inline_module: Option<String>,
    ) {
        self.rust_site = Some(site);
        let (mut change, decision) = self.decide(inputs, state, name, kind, old_sig, new_sig);
        self.rust_site = None;
        change.inline_module = inline_module;
        record_signature_decision(inputs, state, change, decision);
    }

    /// 判定中の変更の名前が、ファイルの複数の宣言に共有されている (名前で引く判定は別の宣言に
    /// 当たり得る)。
    fn name_is_shared(&self) -> bool {
        self.rust_site.is_some_and(|site| site.name_shared)
    }

    /// 判定中の変更の名前の末尾と同じ名前の関数・メソッドが、新しい版のファイルに複数ある
    /// (同じファイルの中の呼び出しがどの宣言のものか決められない)。
    fn callee_is_shared(&self) -> bool {
        self.rust_site.is_some_and(|site| site.callee_shared)
    }

    /// シグネチャの変わったシンボル 1 件を判定し、対応する bucket へ入れる。
    fn classify(
        &mut self,
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        name: &str,
        kind: &str,
        old_sig: &str,
        new_sig: &str,
    ) {
        let (change, decision) = self.decide(inputs, state, name, kind, old_sig, new_sig);
        record_signature_decision(inputs, state, change, decision);
    }

    /// シグネチャの変わったシンボル 1 件を判定する。bucket は書き換えない。
    fn decide(
        &mut self,
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        name: &str,
        kind: &str,
        old_sig: &str,
        new_sig: &str,
    ) -> (ApiSymbolChange, SignatureDecision) {
        let &DetectionInputs {
            dir,
            ref_index,
            base_blobs,
            ..
        } = inputs;
        let change = ApiSymbolChange {
            inline_module: None,
            unverified: None,
            name: name.to_owned(),
            kind: kind.to_owned(),
            file: self.new_path.to_owned(),
            old_signature: Some(old_sig.to_owned()),
            new_signature: Some(new_sig.to_owned()),
            // blocking な api.mod に確定した時点でのみ算出する (record_signature_decision)。
            no_resolved_internal_callers: false,
            // 型契約変更として種別が確定した時点で埋める (record_signature_decision)。
            contract_change: None,
        };
        // 契約が名指す名前の指す先が変わったか、同じと確かめられなければ、公開面の外でない限り、
        // 値だけの変更・互換・同じファイルの中だけ・追随済みとせず blocking にする (Issue #283)。
        if let Some(reason) = self.rust_site.and_then(|site| site.dependency) {
            if self.skip_for_binary_crate
                || is_rust_old_symbol_outside_public_api_surface(
                    dir,
                    base_blobs,
                    self.old_path,
                    name,
                    state.rust_public,
                )
            {
                return (change, SignatureDecision::Omit);
            }
            return (
                change,
                SignatureDecision::Unverified(ApiModificationUnverified {
                    reason,
                    first_unproven: None,
                }),
            );
        }
        // 値変更の方針は外部API面から独立。通常のRust定数変更だけ、公開面の抑制より先に残す。
        // 移動では公開面や型所有元の検査を省けないため、従来の判定経路を使う。
        if self.lang_id == Some(crate::language::LangId::Rust)
            && self.moved_from.is_none()
            && self.old_path == self.new_path
            && is_const_value_only_change(old_sig, new_sig, kind, crate::language::LangId::Rust)
        {
            return (change, SignatureDecision::ConstValue);
        }
        if self.skip_for_binary_crate {
            return (change, SignatureDecision::Omit);
        }
        // private module 抑制
        if is_rust_old_symbol_outside_public_api_surface(
            dir,
            base_blobs,
            self.old_path,
            name,
            state.rust_public,
        ) {
            return (change, SignatureDecision::Omit);
        }
        // Rust の公開構造体・enum・union の形の変更は、中の利用を同じ差分で直しても外の利用者
        // (構造体式・フィールドの読み書き・variant の構築と照合) を直せないので、閉包や同じファイルの
        // 中だけの利用による除外より先に決める (Issue #270・#276・#277)。旧新の形を読めなければ
        // blocking に残す。
        if self.lang_id == Some(crate::language::LangId::Rust)
            && matches!(kind, "struct" | "enum" | "union")
        {
            let site = CompatibleModSite {
                dir,
                base: inputs.base,
                old_path: self.old_path,
                new_path: self.new_path,
                name,
                kind,
                old_sig,
                new_sig,
                lang_id: self.lang_id,
            };
            let shapes = match self.rust_site.and_then(|site| site.shape_override) {
                // `#[cfg]` の分岐の代表の組は、分岐の組ごとに比べた結果 (Issue #293)。
                Some(change) => Some(change),
                None => super::source_pair::load_new_source(dir, self.new_path)
                    .and_then(|new| {
                        super::source_pair::load_old_source_with_new(
                            base_blobs,
                            self.old_path,
                            self.new_path,
                            new,
                        )
                    })
                    .and_then(|sources| {
                        // 宣言の場所で組にした変更は、その宣言そのものの形を比べる (Issue #282)。
                        let site = self.rust_site;
                        match kind {
                            "struct" => {
                                let (old, new) = match site {
                                    Some(site) => (
                                        super::struct_shape_at(&sources.old, site.old_start)?,
                                        super::struct_shape_at(&sources.new, site.new_start)?,
                                    ),
                                    None => (
                                        super::rust_struct_api::unique_struct_shape(
                                            &sources.old,
                                            name,
                                        )?,
                                        super::rust_struct_api::unique_struct_shape(
                                            &sources.new,
                                            name,
                                        )?,
                                    ),
                                };
                                Some(super::rust_struct_api::classify(
                                    &old,
                                    &new,
                                    site.is_some_and(|site| site.availability_compared),
                                ))
                            }
                            "enum" => {
                                let (old, new) = match site {
                                    Some(site) => (
                                        super::enum_shape_at(&sources.old, site.old_start)?,
                                        super::enum_shape_at(&sources.new, site.new_start)?,
                                    ),
                                    None => (
                                        super::rust_enum_api::unique_enum_shape(
                                            &sources.old,
                                            name,
                                        )?,
                                        super::rust_enum_api::unique_enum_shape(
                                            &sources.new,
                                            name,
                                        )?,
                                    ),
                                };
                                Some(super::rust_enum_api::classify(
                                    &old,
                                    &new,
                                    site.is_some_and(|site| site.availability_compared),
                                ))
                            }
                            _ => {
                                let (old, new) = match site {
                                    Some(site) => (
                                        super::union_shape_at(&sources.old, site.old_start)?,
                                        super::union_shape_at(&sources.new, site.new_start)?,
                                    ),
                                    None => (
                                        super::rust_union_api::unique_union_shape(
                                            &sources.old,
                                            name,
                                        )?,
                                        super::rust_union_api::unique_union_shape(
                                            &sources.new,
                                            name,
                                        )?,
                                    ),
                                };
                                Some(super::rust_union_api::classify(
                                    &old,
                                    &new,
                                    site.is_some_and(|site| site.availability_compared),
                                ))
                            }
                        }
                    }),
            };
            let reason = match kind {
                "struct" => "rust_struct_shape_compatible",
                "enum" => "rust_enum_shape_compatible",
                _ => "rust_union_shape_compatible",
            };
            let decision = match shapes {
                // 整形・型の引数の改名・外から見えない変更だけで、公開の形は同じ。
                Some(super::rust_type_api::ShapeChange::Unchanged) => SignatureDecision::Omit,
                Some(super::rust_type_api::ShapeChange::Compatible) => {
                    SignatureDecision::Compatible(site.compatible(reason))
                }
                Some(super::rust_type_api::ShapeChange::Breaking) | None => {
                    SignatureDecision::Blocking(None)
                }
            };
            return (change, decision);
        }
        // TypeScript のクラスの公開のメンバー (フィールド・パラメータプロパティ・アクセサ・
        // コンストラクタなど) と enum のメンバーの変更も、中の利用を同じ差分で直しても外の利用者
        // (フィールドの読み書き・継承・`implements`・`Record<Mode, T>` など) を直せないので、閉包や
        // 同じファイルの中だけの利用による除外より先に決める (Issue #286)。見出しだけの変更は従来
        // どおりに判定する。旧新の形を読めなければ blocking に残す。
        if matches!(
            self.lang_id,
            Some(crate::language::LangId::Typescript | crate::language::LangId::Tsx)
        ) && matches!(kind, "class" | "enum")
        {
            let site = CompatibleModSite {
                dir,
                base: inputs.base,
                old_path: self.old_path,
                new_path: self.new_path,
                name,
                kind,
                old_sig,
                new_sig,
                lang_id: self.lang_id,
            };
            let decision = if kind == "enum" {
                super::ts_class_api::enum_members_changed(old_sig, new_sig)
                    .then_some(SignatureDecision::Blocking(None))
            } else {
                let mut sources =
                    SignatureSourceCache::with_base_blobs(base_blobs, inputs.strip_internal);
                let shape_change = site.lang_id.and_then(|lang| {
                    let pair = sources.get(&site)?;
                    let (old, new) = pair.parse_pair(lang)?;
                    Some(super::ts_class_api::classify_class(
                        &super::ts_class_api::unique_class_shape(old.root_node(), &pair.old, name)?,
                        &super::ts_class_api::unique_class_shape(new.root_node(), &pair.new, name)?,
                    ))
                });
                match shape_change {
                    Some(super::ts_class_api::ClassChange::HeaderOnly) => None,
                    // optional のフィールドの追加だけでも、メンバーの型が名指す同じファイルの型が
                    // 変わっていれば互換と言えない。
                    Some(super::ts_class_api::ClassChange::Compatible)
                        if self.type_dependencies.and_then(|types| types.get(name))
                            == Some(&super::ts_signature::DependencyStatus::Changed) =>
                    {
                        Some(SignatureDecision::Blocking(None))
                    }
                    Some(super::ts_class_api::ClassChange::Compatible) => {
                        Some(SignatureDecision::Compatible(
                            site.compatible("ts_class_optional_members_added"),
                        ))
                    }
                    Some(super::ts_class_api::ClassChange::Breaking) | None => {
                        Some(SignatureDecision::Blocking(None))
                    }
                }
            };
            if let Some(decision) = decision {
                return (change, decision);
            }
        }
        // 移動先への全参照と const 追加を証明した組は、同一ファイル内だけの caller でも記録する。
        if self.lang_id == Some(crate::language::LangId::Rust)
            && self.rust_moved_const_additions.contains(name)
        {
            let site = CompatibleModSite {
                dir,
                base: inputs.base,
                old_path: self.old_path,
                new_path: self.new_path,
                name,
                kind,
                old_sig,
                new_sig,
                lang_id: self.lang_id,
            };
            return (
                change,
                SignatureDecision::MovedConstAddition(site.compatible("rust_const_fn_added")),
            );
        }
        if self.type_dependencies.and_then(|types| types.get(name))
            == Some(&super::ts_signature::DependencyStatus::Changed)
        {
            return (change, SignatureDecision::Blocking(None));
        }
        // closed-in-diff: 同一ファイル内でしか呼ばれていない関数のシグネチャ変更は除外。
        //
        // ただし Python の TypedDict `total=` 変更候補だけはここで落とさない。
        // 落とすと契約変更の 3 値判定に到達できず、「キーが必須化する破壊的変更」が
        // api.mod にすら出なくなる。判定が `NotApplicable` なら
        // `decide_signature_change` 側で従来どおり捨てる。
        // default export / 別名 export (`export { f as g }`) の利用者は宣言名で現れないので、
        // 宣言名の cross-file 参照 0 件は「同一ファイル内でしか使われていない」証明にならない。
        // 同じ名前の宣言がファイルに複数あると、同じファイルの中の呼び出しがどの宣言のものか
        // 決められないので、同じファイルの中だけで使う証明にしない (Issue #282)。
        let internally_closed = !self.name_is_shared()
            && !self.callee_is_shared()
            && is_internally_connected(self.in_file_callees, name)
            && !has_cross_file_refs(ref_index, self.new_path, name)
            && !super::js_public_name::published_names_for_diff_file(
                dir,
                base_blobs,
                self.old_path,
                self.new_path,
                state.closure_caches,
            )
            .is_some_and(|names| !name.contains('.') && names.may_differ(name));
        if internally_closed
            && !may_be_python_typed_dict_total_change(kind, old_sig, new_sig, self.lang_id)
        {
            return (change, SignatureDecision::Omit);
        }

        // Rust の関数に `const` を足しただけなら、const の文脈の外からの呼び出しは変わらない
        // ので非 blocking (Issue #113)。外す変更とほかの部分も変えた変更は証明に入らない。
        if self.lang_id == Some(crate::language::LangId::Rust)
            && self
                .rust_const_additions
                .is_some_and(|names| names.contains(name))
        {
            let site = CompatibleModSite {
                dir,
                base: inputs.base,
                old_path: self.old_path,
                new_path: self.new_path,
                name,
                kind,
                old_sig,
                new_sig,
                lang_id: self.lang_id,
            };
            return (
                change,
                SignatureDecision::Compatible(site.compatible("rust_const_fn_added")),
            );
        }

        // Rust の透過的な root public type alias の導入・展開だけで説明できる表記変更は
        // 非 blocking。同一ファイルの候補間では old/new alias index を再利用する。
        // 同じ名前の宣言がファイルに複数あると、名前で引く関数の見出しが別の宣言のものになり
        // 得るので証明しない (Issue #282)。
        if self.lang_id == Some(crate::language::LangId::Rust)
            && kind == "function"
            && !self.name_is_shared()
        {
            let (old_path, new_path) = (self.old_path, self.new_path);
            let context = self.rust_type_aliases.get_or_insert_with(|| {
                let new_source = super::source_pair::load_new_source(dir, new_path)?;
                let prepared =
                    crate::engine::rust_signature::RustTypeAliasEquivalence::prepare_new_source(
                        &new_source,
                    )?;
                let sources = super::source_pair::load_old_source_with_new(
                    base_blobs, old_path, new_path, new_source,
                )?;
                prepared.with_old_source(&sources.old)
            });
            // 移動では旧新が別のファイルなので、名前が同じものを指すことも確かめる。
            let moved = self.moved_from.is_some();
            let proven = context.as_ref().is_some_and(|context| {
                if moved {
                    context.prove_moved_function(name)
                } else {
                    context.prove_function(name)
                }
            });
            if proven {
                let site = CompatibleModSite {
                    dir,
                    base: inputs.base,
                    old_path,
                    new_path,
                    name,
                    kind,
                    old_sig,
                    new_sig,
                    lang_id: self.lang_id,
                };
                return (
                    change,
                    SignatureDecision::Compatible(site.compatible("transparent_rust_type_alias")),
                );
            }
        }

        // 同じ名前の宣言がファイルに複数あると、呼び出し側の追随も名前では宣言に結び付けられない
        // ので、互換・追随済みの証明に進まず blocking にする (Issue #282)。
        if self.name_is_shared() {
            return (change, SignatureDecision::Blocking(None));
        }
        // TypeScript の関数・メソッドのオーバーロード: 宣言ファイルに出るのはオーバーロードの宣言の
        // 並びで、既存の判定 (末尾の optional 引数の追加などの互換・呼び出し側の追随) は 1 つの
        // シグネチャを前提にする。旧新のどちらかにオーバーロードがあれば blocking にする
        // (Issue #286)。オーバーロードの足し引きと並べ替えは、呼び出しの解決や推論を変え得る。
        if matches!(
            self.lang_id,
            Some(crate::language::LangId::Typescript | crate::language::LangId::Tsx)
        ) && matches!(kind, "function" | "method")
        {
            let site = CompatibleModSite {
                dir,
                base: inputs.base,
                old_path: self.old_path,
                new_path: self.new_path,
                name,
                kind,
                old_sig,
                new_sig,
                lang_id: self.lang_id,
            };
            let mut sources =
                SignatureSourceCache::with_base_blobs(base_blobs, inputs.strip_internal);
            let overloaded = site.lang_id.and_then(|lang| {
                let pair = sources.get(&site)?;
                let (old, new) = pair.parse_pair(lang)?;
                Some(
                    super::ts_class_api::has_overloads(old.root_node(), &pair.old, name)
                        == Some(true)
                        || super::ts_class_api::has_overloads(new.root_node(), &pair.new, name)
                            == Some(true),
                )
            });
            if overloaded == Some(true) {
                return (change, SignatureDecision::Blocking(None));
            }
        }
        let site = SignatureChangeSite {
            change,
            old_path: self.old_path,
            new_path: self.new_path,
            moved_from: self.moved_from,
            kind,
            old_sig,
            new_sig,
            lang_id: self.lang_id,
            internally_closed,
        };
        let decision =
            decide_signature_change(inputs, state.closure_caches, &site, self.type_dependencies);
        (site.change, decision)
    }
}

/// シグネチャ変更 1 件の分類結果。判定 ([`decide_signature_change`]) と bucket への格納
/// ([`record_signature_decision`]) を分け、格納を 1 か所にまとめる。
enum SignatureDecision {
    /// どの bucket にも出さない (bin-only crate・private module・同一ファイル内でしか
    /// 使われていない変更)。
    Omit,
    /// 型注釈の表記だけが変わり、型は同じ (unchanged_type_annotations)。
    UnchangedTypeAnnotation,
    /// 型注釈だけの変更 (type_annotation_changes)。
    TypeAnnotationOnly,
    /// 値バインディングの値だけの変更 (const_value_changes)。
    ConstValue,
    /// 型注釈のない静的 container の整形だけの変更 (unchanged_static_consts)。
    UnchangedStaticConst,
    /// 呼び出し側を壊さない変更 (compatible_modified)。
    Compatible(CompatibleApiModification),
    /// const 追加に加え、移動元と移動先の型所有元まで証明した自由関数。
    MovedConstAddition(CompatibleApiModification),
    /// 全 cross-file 参照が同一 diff で追随済み (modified_closed_in_diff)。
    ClosedInDiff,
    /// callback 本体だけの変更 (callback_body_changes)。
    CallbackBodyOnly,
    /// blocking な api.mod。型契約の種別を確定できたときだけ添える。
    Blocking(Option<crate::models::review::ApiContractChange>),
    Unverified(ApiModificationUnverified),
}

impl SignatureDecision {
    /// 移動と同じ差分で変わった自由関数・定数の組 ([`process_moved_symbol_changes`]) を移動と
    /// みなせる判定か。宣言の比較だけで決まり、移動先で名前が同じものを指すことを確かめられる
    /// 証拠 (値だけの変更と、Rust の透過的な型別名) に限る。互換性を確かめていない変更
    /// (`TypeAnnotationOnly`・`CallbackBodyOnly`)、出さないだけの変更 (`Omit`)、呼び出し側の追随で
    /// 閉じた変更 (`ClosedInDiff`)、参照の用途で決まる互換判定 (旧パスを指す参照を区別しない) は
    /// 含めない。区分や理由を新しく足しても、ここに足さない限り移動には使わない。
    fn proves_compatible_move(&self) -> bool {
        match self {
            Self::ConstValue | Self::MovedConstAddition(_) => true,
            Self::Compatible(compat) => compat.reason == "transparent_rust_type_alias",
            _ => false,
        }
    }
}

/// 分類の段階を記述順に評価し、最初に確定した段階の結果を返す。段階の順序がそのまま
/// 分類の優先順位になる (型契約の変更を互換判定や同一 diff での追随より先に確定させる等)。
/// bucket は書き換えない (格納は [`record_signature_decision`] だけが行う)。
fn decide_signature_change(
    inputs: &DetectionInputs<'_>,
    caches: &mut ApiClosureCaches,
    site: &SignatureChangeSite<'_>,
    dependencies: Option<&super::ts_signature::DependencyChanges>,
) -> SignatureDecision {
    let dependency_status = dependencies
        .and_then(|types| types.get(&site.change.name))
        .copied()
        .unwrap_or_default();
    let &DetectionInputs {
        dir,
        base,
        ref_index,
        base_blobs,
        ..
    } = inputs;
    let &SignatureChangeSite {
        ref change,
        old_path,
        new_path,
        moved_from,
        kind,
        old_sig,
        new_sig,
        lang_id: lang_id_for_file,
        internally_closed,
    } = site;
    let name = change.name.as_str();
    // 宣言名と違う公開名 (default export / 別名 export) の利用者は宣言名の参照に現れない。
    // 参照の集合から互換・追随済みを証明する判定 (JSX だけの利用、省略可能引数の削除、
    // 同一 diff での追随) はどれも使えない。
    let public_name_differs = !name.contains('.')
        && super::js_public_name::published_names_for_diff_file(
            dir, base_blobs, old_path, new_path, caches,
        )
        .is_some_and(|names| names.may_differ(name));
    // 以降の互換判定器はすべて同じ現場情報を見る。old/new ソースは最初に必要になった
    // 判定器で 1 度だけ取得して使い回す (旧実装は判定器ごとに git show を起動していた)。
    let site = CompatibleModSite {
        dir,
        base,
        old_path,
        new_path,
        name,
        kind,
        old_sig,
        new_sig,
        lang_id: lang_id_for_file,
    };
    let sources = &mut SignatureSourceCache::with_base_blobs(base_blobs, inputs.strip_internal);
    if let Some(decision) = type_annotation_decision(&site, sources) {
        return decision;
    }
    // 既存の値変更判定を優先し、直接 TS callable の型注釈は元ソースで照合する。
    if lang_id_for_file.is_some_and(|lid| is_const_value_only_change(old_sig, new_sig, kind, lid))
        && super::ts_signature::ts_const_value_header_guard(&site, sources)
    {
        return SignatureDecision::ConstValue;
    }
    if let Some(decision) = static_const_decision(&site, sources) {
        return decision;
    }
    if let Some(decision) = python_contract_decision(&site, sources, internally_closed) {
        return decision;
    }
    if let Some(compat) = compatible_modification(
        ref_index,
        &site,
        sources,
        caches,
        public_name_differs,
        inputs.diff_files,
    ) {
        if dependency_status == super::ts_signature::DependencyStatus::Unknown
            && matches!(
                compat.reason.as_str(),
                "trailing_optional_params"
                    | "trailing_optional_params_removed"
                    | "optional_object_props"
            )
            && !super::ts_signature::compatible_dependencies_unchanged(
                &site,
                sources,
                &mut caches.optional_removal,
                inputs.diff_files,
                &compat.reason,
                dependencies,
            )
        {
            return SignatureDecision::Blocking(None);
        }
        return SignatureDecision::Compatible(compat);
    }
    closure_or_blocking_decision(
        inputs,
        caches,
        &site,
        sources,
        public_name_differs,
        moved_from,
    )
}

/// 型注釈だけの変更か。
fn type_annotation_decision(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<SignatureDecision> {
    match super::type_annotation::classify_type_annotation_change(site, sources) {
        super::type_annotation::TypeAnnotationDetection::Unchanged => {
            Some(SignatureDecision::UnchangedTypeAnnotation)
        }
        super::type_annotation::TypeAnnotationDetection::AnnotationOnly => {
            Some(SignatureDecision::TypeAnnotationOnly)
        }
        super::type_annotation::TypeAnnotationDetection::NotApplicable => None,
    }
}

/// 型注釈のない静的 container は、構造に加えて葉の値も比較して整形だけを除く。
fn static_const_decision(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
) -> Option<SignatureDecision> {
    match super::static_const::classify_static_const_change(site, sources) {
        super::static_const::StaticConstChange::Unchanged => {
            Some(SignatureDecision::UnchangedStaticConst)
        }
        super::static_const::StaticConstChange::ValueOnly => Some(SignatureDecision::ConstValue),
        super::static_const::StaticConstChange::NotApplicable => None,
    }
}

/// Python の TypedDict で `total=` が絡む変更は blocking な api.mod に確定させる。
/// 互換判定器や closed-in-diff 判定より**前**に置くのが要点で、「リポジトリ内の参照が
/// 同一 diff で更新済み」でも降格させないため (外部リポジトリ / 動的生成された dict は
/// 静的に追えない。Issue 2026-08-18-python-typeddict-contract-change-classification)。
///
/// **種別を確定できなくても降格させない**。分類できないことは「破壊的でない」証明では
/// ないため (`NotRequired` 同居で分類を諦めたケースでも bare フィールドの requiredness は
/// 実際に反転する)。ラベル (`contract_change`) は確定したときだけ添える。
///
/// | 検出 | `internally_closed` | 結果 |
/// |---|---|---|
/// | `NotApplicable` | false | 次の段階へ |
/// | `NotApplicable` | true | 出さない |
/// | `PotentialBreakingChange` | false | blocking |
/// | `PotentialBreakingChange` | true | 出さない |
/// | `Classified` | どちらでも | 契約ラベル付きの blocking |
fn python_contract_decision(
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    internally_closed: bool,
) -> Option<SignatureDecision> {
    match detect_python_typed_dict_total_change(site, sources) {
        // TypedDict の `total=` 変更だと**証明できなかった**ので、同一ファイル内でしか
        // 使われていないシンボルは従来どおり api.mod に出さない (呼び出し元の早期除外の続き)。
        // 証明できないケースまで出すと、`class X(Base, total=False)` のような無関係な
        // Python class を新たに blocking にしてしまう。この早期除外そのものの是非は
        // 言語横断の既存ルールなので本 Issue では触らない。
        PythonContractDetection::NotApplicable => {
            internally_closed.then_some(SignatureDecision::Omit)
        }
        PythonContractDetection::PotentialBreakingChange if internally_closed => {
            Some(SignatureDecision::Omit)
        }
        PythonContractDetection::PotentialBreakingChange => Some(SignatureDecision::Blocking(None)),
        PythonContractDetection::Classified(contract) => {
            Some(SignatureDecision::Blocking(Some(contract)))
        }
    }
}

/// 呼び出し側を壊さない変更を、互換判定器を記述順に試して最初に当てはまったもので返す。
/// 同じ変更に複数の判定器が当てはまるときは先の判定器の reason を採るので、順序を変えない。
fn compatible_modification(
    ref_index: &ApiRefIndex,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    caches: &mut ApiClosureCaches,
    public_name_differs: bool,
    diff_files: &[crate::models::impact::DiffFile],
) -> Option<CompatibleApiModification> {
    if let Some(compat) =
        detect_fixed_callback_contract_compatible_mod(ref_index, site, sources, caches)
    {
        return Some(compat);
    }
    // React component を memo / forwardRef 等の HOC でラップしただけは compatible_modified
    if !public_name_differs
        && let Some(compat) = detect_react_wrapper_compatible_mod(ref_index, site, sources, caches)
    {
        return Some(compat);
    }
    // exported object の未参照プロパティ削除も compatible_modified
    if let Some(compat) = detect_object_members_compatible_mod(site, sources) {
        return Some(compat);
    }
    if let Some(compat) = detect_type_members_compatible_mod(site, sources) {
        return Some(compat);
    }
    // TS/TSX の関数末尾へ optional/default 引数を追加しただけなら、既存呼び出しの required
    // arity は変わらないため compatible_modified として扱う。
    if let Some(compat) = detect_trailing_optional_params_compatible_mod(site, sources) {
        return Some(compat);
    }
    if !public_name_differs
        && let Some(compat) = detect_trailing_optional_params_removed_compatible_mod(
            ref_index,
            site,
            sources,
            &mut caches.optional_removal,
        )
    {
        return Some(compat);
    }
    // TS/TSX の関数コンポーネントの inline object 型の props から省略可能プロパティを消し、
    // リポジトリ内のどの JSX 要素・直接呼び出しもそのキーを渡さないと証明できたものも、
    // 呼び出し側の書き換えが要らないので compatible_modified とする (Issue #208)。
    if !public_name_differs
        && let Some(compat) = detect_optional_object_props_removed_compatible_mod(
            ref_index,
            site,
            sources,
            &mut caches.optional_removal,
            diff_files,
        )
    {
        return Some(compat);
    }
    // TS/TSX の引数 object type literal へ optional プロパティを追加しただけなら、
    // 既存呼び出しが渡す object はそのまま受理されるため compatible_modified として扱う。
    if let Some(compat) = detect_optional_object_props_compatible_mod(
        site,
        sources,
        &mut caches.react_memo,
        diff_files,
    ) {
        return Some(compat);
    }
    // 同一ファイル内で有限 literal union の値集合が old/new 同値と証明できる type alias
    // 変更は、表記・alias 経路だけの変更なので compatible_modified とする。
    // import 越し・循環・非 literal・集合差分ありは判定器が None に倒し blocking を維持する。
    if let Some(compat) = detect_equivalent_literal_union_alias_compatible_mod(site, sources) {
        return Some(compat);
    }
    // React Server Component の async 化 (async キーワード追加のみ + 全参照が JSX タグ利用)
    // は呼び出し側の書き換えが不要なため compatible_modified として扱う。
    if !public_name_differs
        && let Some(compat) =
            detect_async_jsx_component_compatible_mod(ref_index, site, sources, caches)
    {
        return Some(compat);
    }
    // 引数なし関数コンポーネントへ default 値の無い全 optional の destructured props を
    // 足しただけで、参照がすべて JSX タグ利用なら `<X />` はそのまま通るため
    // compatible_modified とする (default 値 / `?` 付きは trailing_optional_params が扱う)。
    if !public_name_differs
        && let Some(compat) =
            detect_optional_props_jsx_component_compatible_mod(ref_index, site, sources, caches)
    {
        return Some(compat);
    }
    // Python のトップレベル関数 / モジュール直下クラスのメソッドへ末尾 optional/default
    // 引数 (`*` 後の kwonly+default 含む) を追加しただけなら、既存呼び出しが壊れないため
    // compatible_modified として扱う。
    if let Some(compat) = detect_python_trailing_optional_params_compatible_mod(site, sources) {
        return Some(compat);
    }
    // Python のデフォルト値・型注釈にある純粋な隣接文字列リテラルを分割・結合しただけで、
    // 正規化後のシグネチャ全体が同一なら compatible_modified とする。f-string、escape、
    // 演算子、異種 prefix 等を含む場合は判定器が None に倒し blocking を維持する。
    detect_python_implicit_string_concat_compatible_mod(site, sources)
}

/// 全 cross-file 参照が同一 diff で追随済みなら informational、callback 本体だけの変更なら
/// その分類、どちらでもなければ blocking な api.mod にする。
fn closure_or_blocking_decision(
    inputs: &DetectionInputs<'_>,
    caches: &mut ApiClosureCaches,
    site: &CompatibleModSite<'_>,
    sources: &mut SignatureSourceCache<'_>,
    public_name_differs: bool,
    moved_from: Option<&str>,
) -> SignatureDecision {
    caches
        .type_member_unproven_sites
        .remove(&(site.new_path.to_owned(), site.name.to_owned()));
    if !public_name_differs
        && moved_from.is_none()
        && let Some(closed) = super::python_optional_removal::prove_removal(
            inputs.ref_index,
            site,
            sources,
            &mut caches.python_optional_removal,
        )
    {
        return if closed {
            SignatureDecision::ClosedInDiff
        } else {
            SignatureDecision::Blocking(None)
        };
    }
    if !public_name_differs
        && moved_from.is_none()
        && let Some(closed) = super::ts_literal_union_removal::prove_removal(
            inputs.ref_index,
            site,
            sources,
            &mut caches.literal_union_removal,
        )
    {
        return if closed {
            SignatureDecision::ClosedInDiff
        } else {
            SignatureDecision::Unverified(ApiModificationUnverified {
                reason: ApiModificationUnverifiedReason::LiteralUnionProofFailed,
                first_unproven: None,
            })
        };
    }
    let removed_props = match super::ts_signature::removed_parameter_props(site, sources) {
        Ok(props) => props,
        Err(()) => return SignatureDecision::Blocking(None),
    };
    if removed_props.is_empty()
        && !public_name_differs
        && moved_from.is_none()
        && caches.function_handoffs.covers(inputs, site, sources)
    {
        return SignatureDecision::ClosedInDiff;
    }
    // object type literal 引数への必須プロパティ追加のみの変更なら、閉包判定へ追加証拠として
    // 渡す (呼び出し式が無変更でも共有 const の定義側が同一 diff で更新されていれば追随済み)。
    // 互換変更ではないので compatible_modified には入れず、あくまで closed 判定の入力。
    let added_required_props = detect_added_required_object_props(site, sources);
    let python_protocol =
        super::python_protocol_arg::prepare_protocol_argument_change(site, sources);
    // interface / object 型の type alias のメンバー変更は、値を作る箇所・受け取る箇所
    // ごとの証明だけで閉じる。証明に対応しない形 (generic / extends / メソッド追加など) は、
    // 参照行が変更行にあるだけでは追随済みと言えないので閉じない (Issue #89)。
    let required_members =
        match super::ts_signature::classify_required_member_addition(site, sources) {
            super::ts_required_members::RequiredMemberChange::NotApplicable => {
                match super::ts_signature::classify_member_property_removal(site, sources) {
                    super::ts_required_members::RequiredMemberChange::NotApplicable => {
                        super::ts_signature::classify_member_type_narrowing(site, sources)
                    }
                    classified => classified,
                }
            }
            classified => classified,
        };
    if matches!(
        required_members,
        super::ts_required_members::RequiredMemberChange::Unsupported
    ) {
        return SignatureDecision::Unverified(ApiModificationUnverified {
            reason: ApiModificationUnverifiedReason::UnsupportedTypeChange,
            first_unproven: None,
        });
    }
    // Python の関数・メソッドの戻り値の注釈だけの変更なら、結果の利用の書き換えを追加証拠に
    // する (Issue #251)。移動と同じ差分の変更は、呼び出し側の旧版を移動元と比べられないので除く。
    let python_return = (python_protocol.is_none() && moved_from.is_none())
        .then(|| super::python_return_usage::prepare(site, sources, inputs.base_blobs))
        .flatten();
    let arg_evidence = if !removed_props.is_empty() {
        super::ref_index::ClosureArgEvidence::TsRemovedProps(&removed_props)
    } else {
        match (
            &added_required_props,
            &python_protocol,
            &required_members,
            &python_return,
        ) {
            (Some(props), _, _, _) => {
                super::ref_index::ClosureArgEvidence::TsAddedRequiredProps(props)
            }
            (None, Some(proof), _, _) => {
                super::ref_index::ClosureArgEvidence::PythonProtocolParams(proof)
            }
            (None, None, super::ts_required_members::RequiredMemberChange::Supported(delta), _) => {
                super::ref_index::ClosureArgEvidence::TsRequiredTypeMembers(delta)
            }
            (None, None, _, Some(contract)) => {
                super::ref_index::ClosureArgEvidence::PythonReturnContract(contract)
            }
            _ => super::ref_index::ClosureArgEvidence::None,
        }
    };
    // 真偽判定の不変性は関数のシグネチャ変更だけの証拠。値から関数への種類変更は除く。
    // ソースは互換判定器と共有し、候補ごとに一度だけ旧新の宣言を確かめる。
    let js_direct_function_change = site.kind == "function"
        && !site.name.contains('.')
        && site
            .lang_in(&[
                crate::language::LangId::Javascript,
                crate::language::LangId::Typescript,
                crate::language::LangId::Tsx,
            ])
            .and_then(|lang| sources.get(site)?.parse_pair(lang))
            .is_some_and(|(old, new)| {
                let Some(pair) = sources.get(site) else {
                    return false;
                };
                crate::engine::js_occurrence::unique_direct_function(
                    old.root_node(),
                    &pair.old,
                    site.name,
                ) && crate::engine::js_occurrence::unique_direct_function(
                    new.root_node(),
                    &pair.new,
                    site.name,
                )
            });
    let rust_module_function_change = (site.kind == "function"
        && !site.name.contains('.')
        && site.lang_id == Some(crate::language::LangId::Rust))
    .then(|| sources.get(site))
    .flatten()
    .and_then(|pair| {
        let (old, new) = pair.parse_pair(crate::language::LangId::Rust)?;
        super::rust_ref_sites::ModuleFunctions::from_root(old.root_node(), &pair.old)?
            .prove(site.name, site.old_sig)?;
        super::rust_ref_sites::ModuleFunctions::from_root(new.root_node(), &pair.new)?
            .prove(site.name, site.new_sig)
    });
    // 全 cross-file 参照が同一 diff 内で追随済みなら informational
    if is_modified_closed_in_diff(
        ModifiedClosureInput {
            index: inputs.ref_index,
            dir: inputs.dir,
            name: site.name,
            kind: site.kind,
            base: inputs.base,
            target_new_path: site.new_path,
            diff_files: inputs.diff_files,
            arg_evidence,
            public_name_differs,
            js_direct_function_change,
            rust_module_function_change,
            moved_from,
        },
        caches,
    ) {
        SignatureDecision::ClosedInDiff
    } else if detect_callback_body_only_change(site, sources) {
        SignatureDecision::CallbackBodyOnly
    } else if matches!(
        required_members,
        super::ts_required_members::RequiredMemberChange::Supported(_)
    ) {
        SignatureDecision::Unverified(ApiModificationUnverified {
            reason: ApiModificationUnverifiedReason::TypeMemberProofFailed,
            first_unproven: caches
                .type_member_unproven_sites
                .remove(&(site.new_path.to_owned(), site.name.to_owned())),
        })
    } else if site
        .lang_in(&[
            crate::language::LangId::Typescript,
            crate::language::LangId::Tsx,
        ])
        .is_some()
        && matches!(site.kind, "type" | "trait" | "interface")
    {
        SignatureDecision::Unverified(ApiModificationUnverified {
            reason: ApiModificationUnverifiedReason::UnsupportedTypeChange,
            first_unproven: None,
        })
    } else {
        SignatureDecision::Blocking(None)
    }
}

/// 分類結果を対応する bucket へ格納する。bucket を書き換えるのはここだけ。
fn record_signature_decision(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    mut change: ApiSymbolChange,
    decision: SignatureDecision,
) {
    let buckets = &mut *state.buckets;
    match decision {
        SignatureDecision::Omit => {}
        SignatureDecision::UnchangedTypeAnnotation => {
            buckets.unchanged_type_annotations.push(ApiSymbol {
                inline_module: change.inline_module,
                name: change.name,
                kind: change.kind,
                file: change.file,
                refs_internal: 0,
                cfg_availability: None,
            });
        }
        SignatureDecision::TypeAnnotationOnly => {
            buckets
                .type_annotation_changes
                .push(crate::models::review::TypeAnnotationChange {
                    change,
                    reason: crate::models::review::TypeAnnotationChangeReason::TypeAnnotationOnly,
                    compatibility: crate::models::review::TypeAnnotationCompatibility::Unverified,
                });
        }
        SignatureDecision::ConstValue => buckets.const_value_changes.push(change),
        SignatureDecision::UnchangedStaticConst => {
            buckets.unchanged_static_consts.push(ApiSymbol {
                inline_module: change.inline_module,
                name: change.name,
                kind: change.kind,
                file: change.file,
                refs_internal: 0,
                cfg_availability: None,
            });
        }
        SignatureDecision::Compatible(mut compat)
        | SignatureDecision::MovedConstAddition(mut compat) => {
            // 互換の判定器は名前で作るので、変更の inline module の経路を引き継ぐ。
            if compat.inline_module.is_none() {
                compat.inline_module = change.inline_module;
            }
            buckets.compatible_modified.push(compat)
        }
        SignatureDecision::ClosedInDiff => buckets.modified_closed_in_diff.push(change),
        SignatureDecision::CallbackBodyOnly => {
            buckets
                .callback_body_changes
                .push(crate::models::review::CallbackBodyChange {
                    change,
                    reason: crate::models::review::CallbackBodyChangeReason::CallbackBodyChange,
                });
        }
        SignatureDecision::Blocking(contract) => {
            record_blocking_modification(inputs, state, change, contract);
        }
        SignatureDecision::Unverified(diagnostic) => {
            change.unverified = Some(diagnostic);
            record_blocking_modification(inputs, state, change, None);
        }
    }
}

fn record_blocking_modification(
    inputs: &DetectionInputs<'_>,
    state: &mut DetectionState<'_>,
    mut change: ApiSymbolChange,
    contract: Option<crate::models::review::ApiContractChange>,
) {
    if let Some(contract) = contract {
        change.contract_change = Some(contract);
    }
    change.no_resolved_internal_callers = has_no_resolved_internal_callers(
        inputs.ref_index,
        inputs.dir,
        &change.name,
        state.closure_caches,
    );
    state.buckets.modified.push(change);
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
        if !old_source_unverified
            && is_rust_old_symbol_outside_public_api_surface(
                dir,
                base_blobs,
                &df.old_path,
                name,
                state.rust_public,
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
    let &DetectionInputs {
        dir,
        diff_new_paths,
        ref_index,
        ..
    } = inputs;
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
        state.buckets.all_new_candidates.push(candidate.clone());
        if is_binary_rust_crate {
            continue;
        }
        if is_rust_new_symbol_outside_public_api_surface(dir, &df.new_path, name, state.rust_public)
        {
            continue;
        }
        if is_internally_connected(in_file_callees, name) {
            continue;
        }
        if is_used_in_diff_paths(ref_index, dir, name, &df.new_path, diff_new_paths) {
            continue;
        }
        state.buckets.added.push(candidate);
    }
}
