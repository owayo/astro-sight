//! Rust の変更ファイルで、宣言の場所で組にした旧新の宣言を比べる (Issue #282)。

use super::*;

/// Rust の変更ファイルで、宣言の場所 (inline module の経路・qualname・種類) で組にした旧新の
/// 宣言を比べる (Issue #282)。同じキーの宣言が複数あって組にできず、cfg の条件と signature の
/// 対応が変わったものは、比べられない変更として blocking にする。
pub(super) fn collect_rust_modified_symbols(
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
    // 囲む inherent impl の見出し (対象の型・型の引数の境界・`where`) は、メソッドと関連定数を使える型を
    // 決める (Issue #307)。見出しの字句が変わった組だけ、旧新のソースを 1 度読んで比べる (読めなければ
    // 壊すとする)。
    let mut impl_headers: Option<Option<super::rust_declarations::ImplHeaders>> = None;
    for &(old_index, new_index) in &pairing.pairs {
        let (name, _, old_sig) = &facts.old_syms[old_index];
        let (_, kind, new_sig) = &facts.new_syms[new_index];
        let representative = pairing
            .branch_representatives
            .contains(&(old_index, new_index));
        // `#[cfg]` の分岐の代表の組は、同じ鍵の宣言の全てで束縛を比べる (分岐ごとに宣言の有る条件で
        // 絞るので、代表だけではほかの分岐の束縛を比べない。Issue #312)。
        let group = representative
            .then(|| {
                pairing
                    .groups
                    .iter()
                    .find(|(olds, news)| olds.contains(&old_index) && news.contains(&new_index))
            })
            .flatten();
        let dependency = match group {
            Some((olds, news)) => dependency(olds, news),
            None => dependency(&[old_index], &[new_index]),
        };
        let impl_change = (declarations.old[old_index].container_header
            != declarations.new[new_index].container_header)
            .then(|| {
                let headers = impl_headers.get_or_insert_with(|| {
                    let old_source = inputs.base_blobs.read(&df.old_path)?;
                    let new_source = load_new_source(inputs.dir, &df.new_path)?.to_vec();
                    super::rust_declarations::ImplHeaders::new(old_source, new_source)
                });
                match headers {
                    Some(headers) => headers.change(
                        declarations.old[old_index].start,
                        declarations.new[new_index].start,
                    ),
                    None => Some(super::rust_declarations::ImplHeaderChange::breaking()),
                }
            })
            .flatten();
        // 見出しの型の引数の名前を付け直すと signature が同じなら、signature の字面の違いは付け直しだけ。
        let renamed = impl_change
            .as_ref()
            .is_some_and(|change| change.member_renamed);
        // 関連定数の値のマクロの引数の中の型の引数の対応が変わったなら、signature の字面が同じでも値の変更。
        let value_changed = impl_change
            .as_ref()
            .is_some_and(|change| change.value_changed);
        let header = impl_change
            .filter(|change| change.change != super::rust_type_api::ShapeChange::Unchanged);
        // signature が同じ組は、名指す名前の束縛も囲む impl の見出しも同じなら報告しない (型注釈と静的
        // container の段階は TS・JS だけ)。
        if dependency.is_none()
            && header.is_none()
            && !value_changed
            && (old_sig == new_sig || renamed || facts.parameter_pattern_only_names.contains(name))
        {
            continue;
        }
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
            impl_header: header.as_ref().map(|change| change.change),
            impl_member_renamed: renamed,
            impl_value_changed: value_changed,
        };
        let inline_module = declarations.new_inline_module(new_index);
        // 見出しが変わったときは、signature に見出しを添えて示す。
        let display = header.map(|change| {
            (
                format!("{} {{ {old_sig} }}", change.old_text),
                format!("{} {{ {new_sig} }}", change.new_text),
            )
        });
        files.classify_rust(
            inputs,
            state,
            name,
            kind,
            old_sig,
            new_sig,
            site,
            &declarations.old[old_index],
            inline_module,
            display,
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
            impl_header: None,
            impl_member_renamed: false,
            impl_value_changed: false,
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
            &declarations.old[old_index],
            inline_module,
            None,
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
            ..ApiSymbolChange::new(name.clone(), kind.clone(), df.new_path.clone(), None, None)
        };
        record_unverified_modification(
            inputs,
            state,
            change,
            ApiModificationUnverified {
                reason: ApiModificationUnverifiedReason::RustDeclarationPairingAmbiguous,
                first_unproven: None,
            },
        );
    }
}
