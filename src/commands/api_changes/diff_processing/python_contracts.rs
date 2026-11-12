//! signature に現れない Python の契約の変更 (TypedDict のキーの必須・型など)。

use super::*;

/// シグネチャ差分に乗らない Python の型契約変更を、1 組の source / parse から検出する。
///
/// TypedDict と Literal の検出器を別々に読み込ませると、同じ modified ファイルに対して
/// `git show` と tree-sitter parse が 2 回ずつ走るため、ここで共有する。
pub(super) fn collect_python_contract_changes(
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
            // 疑似シンボルを bare 名で解決すると、無関係な同名変数を呼び出し元にしてしまうので、
            // 参照の証拠は付けない。
            state.buckets.modified.push(ApiSymbolChange {
                contract_change: change.contract,
                ..ApiSymbolChange::new(
                    change.name,
                    "type".to_string(),
                    df.new_path.clone(),
                    Some(change.old_signature),
                    Some(change.new_signature),
                )
            });
        }
    }
}

pub(super) struct ParsedPythonSources<'tree, 'source> {
    old_root: tree_sitter::Node<'tree>,
    old_source: &'source [u8],
    new_root: tree_sitter::Node<'tree>,
    new_source: &'source [u8],
}

/// クラスヘッダが不変で一意な、フィールド契約比較の候補名を I/O 前に抽出する。
pub(super) fn python_typed_dict_field_candidates<'a>(
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
pub(super) fn collect_python_typed_dict_field_changes(
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
            // 疑似シンボルなので呼び出し元の解決はしない。`field` を bare 名で引くと
            // 無関係な同名フィールド / ローカル変数に当たる。
            state.buckets.modified.push(ApiSymbolChange {
                contract_change: Some(change.contract),
                ..ApiSymbolChange::new(
                    change.name,
                    "field".to_string(),
                    df.new_path.clone(),
                    Some(change.old_signature),
                    Some(change.new_signature),
                )
            });
        }
    }
}

/// `Required` / `NotRequired` の出現を素朴に見る安価な前段フィルタ。
/// `NotRequired` は `Required` を含むため 1 パターンで足りる。
/// 過剰にヒットしても後段の AST 判定で落ちるだけなので、保守側に振れている。
pub(super) fn mentions_requiredness_qualifier(source: &[u8]) -> bool {
    source.windows(b"Required".len()).any(|w| w == b"Required")
}

/// `Literal` の出現を素朴に見る安価な前段フィルタ。最終判定は AST と名前解決で行う。
pub(super) fn mentions_literal_qualifier(source: &[u8]) -> bool {
    source.windows(b"Literal".len()).any(|w| w == b"Literal")
}
