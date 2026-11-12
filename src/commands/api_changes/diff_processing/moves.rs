//! 移動と同じ差分でシグネチャの変わったメンバー・自由関数・定数と、再公開した削除の判定。

use super::*;

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
    let mut sources = MemberSources::default();
    for change in changes {
        // 移動とともに囲む inherent impl の見出しを強めたなら、同じ差分の追随などの判定より先に blocking
        // (Issue #307)。signature の字面の違いが見出しの型の引数の名前の付け直しだけでも、ここでは名指す名前の
        // 束縛 (Issue #283) を比べないので、互換や変更なしとはせず、ほかの判定に任せる。
        if moved_member_header(inputs, change, &mut sources).is_some_and(|header| {
            header.change == super::rust_type_api::ShapeChange::Breaking || header.value_changed
        }) {
            let symbol = change
                .new
                .clone()
                .into_modified(change.old.signature.clone());
            record_signature_decision(inputs, state, symbol, SignatureDecision::Blocking(None));
            continue;
        }
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

/// 移動したメンバーの見出しを比べるのに読んだ、ファイルごとの宣言の事実と構文木 (旧新それぞれ 1 度だけ)。
#[derive(Default)]
pub(super) struct MemberSources {
    facts: std::collections::HashMap<(bool, String), Option<MemberFacts>>,
    parsed: std::collections::HashMap<
        (bool, String),
        Option<std::rc::Rc<super::rust_declarations::ParsedSource>>,
    >,
    /// (移動元, 移動先) → 囲む impl の見出しの比較 (並べ替えの探索の結果をメンバーの間で共有する)。
    headers: std::collections::HashMap<(String, String), super::rust_declarations::ImplHeaders>,
}

/// ファイルの公開のシンボルと宣言の文脈。
pub(super) type MemberFacts = (ExportedSymbols, Vec<RustDeclarationContext>);

/// 移動したメンバーの旧新の宣言 (名前・種類・signature・inline module で 1 つずつ引けたもの) を囲む
/// inherent impl の見出しを比べた結果 (Issue #307)。Rust でないか、引けなければ None。
pub(super) fn moved_member_header(
    inputs: &DetectionInputs<'_>,
    change: &MovedMemberChange<'_>,
    sources: &mut MemberSources,
) -> Option<super::rust_declarations::ImplHeaderChange> {
    let rust = |path: &str| {
        crate::language::LangId::from_path(camino::Utf8Path::new(path)).ok()
            == Some(crate::language::LangId::Rust)
    };
    if !rust(&change.old.file) || !rust(&change.new.file) {
        return None;
    }
    let read = |new: bool, path: &str| -> Option<Vec<u8>> {
        if new {
            load_new_source(inputs.dir, path).map(|source| source.to_vec())
        } else {
            inputs.base_blobs.read(path)
        }
    };
    let mut find = |new: bool, candidate: &ApiSymbolCandidate| -> Option<RustDeclarationContext> {
        let facts = sources
            .facts
            .entry((new, candidate.file.clone()))
            .or_insert_with(|| {
                let source = read(new, &candidate.file)?;
                let (symbols, declarations) = extract_deleted_exported_facts(
                    &candidate.file,
                    &source,
                    inputs.strip_internal,
                )?;
                Some((symbols, declarations?))
            })
            .as_ref()?;
        let mut found = facts
            .0
            .iter()
            .zip(&facts.1)
            .filter(|((name, kind, signature), declaration)| {
                *name == candidate.name
                    && *kind == candidate.kind
                    && *signature == candidate.signature
                    && inline_module_of(declaration) == candidate.inline_module
            })
            .map(|(_, declaration)| declaration.clone());
        match (found.next(), found.next()) {
            (Some(declaration), None) => Some(declaration),
            _ => None,
        }
    };
    let old = find(false, &change.old)?;
    let new = find(true, &change.new)?;
    if old.container_header == new.container_header {
        return None;
    }
    let mut parsed = |new: bool, path: &str| {
        sources
            .parsed
            .entry((new, path.to_owned()))
            .or_insert_with(|| super::rust_declarations::ParsedSource::parse(read(new, path)?))
            .clone()
    };
    let (Some(old_parsed), Some(new_parsed)) = (
        parsed(false, &change.old.file),
        parsed(true, &change.new.file),
    ) else {
        return Some(super::rust_declarations::ImplHeaderChange::breaking());
    };
    sources
        .headers
        .entry((change.old.file.clone(), change.new.file.clone()))
        .or_insert_with(|| super::rust_declarations::ImplHeaders::of(old_parsed, new_parsed))
        .change(old.start, new.start)
}

/// 再公開の先と組にして比べる、Rust の宣言の種類。マクロ・module は扱わない (公開の規則が違う)。
pub(super) const REEXPORT_COMPARED_KINDS: [&str; 7] = [
    "function", "constant", "struct", "enum", "union", "trait", "type",
];

/// 型の同一性を持つ宣言の種類。差分の外の同じ形の宣言や、ほかの旧い型と共有する宣言を再公開
/// しても、別の型になるので壊す (利用側の trait の実装が重なる、など)。
pub(super) const NOMINAL_KINDS: [&str; 4] = ["struct", "enum", "union", "trait"];

/// 作業ツリーの `file` の、`start` (行, 列) に始まる名前が `name` の宣言の API の signature。
pub(super) fn declaration_signature_at(
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
pub(super) struct ReexportPair {
    target: ReexportTarget,
    /// 先の宣言の API の signature。
    new_signature: String,
    /// 先の型を足したファイルで、base にあった同じ名前の型の束縛が消えたか変わった (型をまとめた
    /// かもしれない)。
    merged: bool,
}

/// 作業ツリーのパス `path` の base での中身。diff の rename (内容同一の rename を含む) は旧いパスを
/// 読み、同じ差分で足したファイルは無いとする。
pub(super) fn base_file_of(
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
pub(super) fn pair_reexported_removal(
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
pub(super) fn reexport_site(
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
        impl_header: None,
        impl_member_renamed: false,
        impl_value_changed: false,
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
            unverified: unverified.then_some(ApiModificationUnverified {
                reason: ApiModificationUnverifiedReason::ReexportTargetUnresolved,
                first_unproven: None,
            }),
            ..ApiSymbolChange::new(
                old.name.clone(),
                old.kind.clone(),
                reexporting_file.clone(),
                Some(old.signature.clone()),
                new_signature,
            )
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
        let rust_site = reexport_site(inputs, old, target);
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
        let candidate = SignatureCandidate {
            rust_site: Some(rust_site),
            ..SignatureCandidate::new(&old.name, &old.kind, &old.signature, &pair.new_signature)
        };
        let (change, decision) = changed.decide(inputs, state, candidate);
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
        let candidate = SignatureCandidate::new(
            &change.new.name,
            &change.new.kind,
            &change.old.signature,
            &change.new.signature,
        );
        let (symbol_change, decision) = files[ix].decide(inputs, state, candidate);
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
