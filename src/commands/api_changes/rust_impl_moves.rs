//! impl の項目の移動は、対象の型の実定義の対応も確かめる。

use super::*;
use crate::models::review::{ApiModificationUnverified, ApiModificationUnverifiedReason};

type Declaration<'a> = (&'a (String, String, String), &'a RustDeclarationContext);
type Owner = (String, Vec<String>, String, &'static str);

fn declarations<'a>(
    inputs: &'a DetectionInputs<'_>,
    symbol: &MovedSymbol,
    old: bool,
) -> Vec<Declaration<'a>> {
    let mut found = Vec::new();
    for (diff, prepared) in inputs.diff_files.iter().zip(inputs.prepared) {
        let (path, inline_module) = if old {
            (&diff.old_path, &symbol.from_inline_module)
        } else {
            (&diff.new_path, &symbol.to_inline_module)
        };
        if path != if old { &symbol.from } else { &symbol.to } {
            continue;
        }
        let facts = match (old, prepared) {
            (
                true,
                PreparedDiffFile::Deleted {
                    old_syms,
                    rust_declarations,
                    ..
                },
            ) => old_syms.as_deref().zip(rust_declarations.as_deref()),
            (
                false,
                PreparedDiffFile::Added {
                    new_syms,
                    rust_declarations,
                    ..
                },
            ) => new_syms.as_deref().zip(rust_declarations.as_deref()),
            (
                true,
                PreparedDiffFile::Modified {
                    old_syms,
                    rust_declarations,
                    ..
                },
            ) => old_syms
                .as_deref()
                .zip(rust_declarations.as_ref().map(|pair| pair.old.as_slice())),
            (
                false,
                PreparedDiffFile::Modified {
                    new_syms,
                    rust_declarations,
                    ..
                },
            ) => new_syms
                .as_deref()
                .zip(rust_declarations.as_ref().map(|pair| pair.new.as_slice())),
            _ => None,
        };
        if let Some((symbols, contexts)) = facts {
            found.extend(
                symbols
                    .iter()
                    .zip(contexts)
                    .filter(|((name, kind, _), context)| {
                        *name == symbol.name
                            && *kind == symbol.kind
                            && inline_module_of(context) == *inline_module
                    }),
            );
        }
    }
    found
}

fn owner(
    inputs: &DetectionInputs<'_>,
    path: &str,
    declarations: &[Declaration<'_>],
    old: bool,
) -> Option<(String, Owner)> {
    let libraries = inputs.rust_crates.library_versions(path)?;
    let krate = if old { &libraries.old } else { &libraries.new };
    let mut found = None;
    for (_, declaration) in declarations {
        if declaration.owner.is_empty() || declaration.container_macro_attributes {
            return None;
        }
        let scope: Vec<String> = declaration
            .inline_module
            .split("::")
            .filter(|part| !part.is_empty())
            .map(str::to_owned)
            .collect();
        let (candidates, placements) =
            rust_dependencies::impl_owner_candidates(
                krate,
                path,
                &scope,
                &declaration.owner,
                declaration.availability.as_deref(),
            )
                .or_else(|| {
                    // crate 起点の型は配置によらない。全配置を読めた場合だけ root から解き直す。
                    if !matches!(declaration.owner.as_slice(), [anchor, separator, ..] if anchor == "crate:crate" && separator == "::")
                        || krate.tree_modules(path)?.is_empty()
                    {
                        return None;
                    }
                    rust_dependencies::impl_owner_candidates(
                        krate,
                        krate.root_path(),
                        &[],
                        &declaration.owner,
                        declaration.availability.as_deref(),
                    )
                })?;
        if candidates.is_empty()
            || !rust_dependencies::candidates_cover(
                declaration.availability.as_deref(),
                &placements,
                &candidates,
            )
        {
            return None;
        }
        for candidate in candidates {
            let definition = candidate.definition?;
            if definition.opaque
                || !matches!(definition.kind, "struct_item" | "enum_item" | "union_item")
            {
                return None;
            }
            let identity = (
                definition.path.clone(),
                definition.scope.clone(),
                definition.name.clone(),
                definition.kind,
            );
            match &found {
                Some(previous) if previous != &identity => return None,
                None => found = Some(identity),
                _ => {}
            }
        }
    }
    Some((krate.root_path().to_owned(), found?))
}

fn same_owner(
    inputs: &DetectionInputs<'_>,
    old: &(String, Owner),
    new: &(String, Owner),
    moved: &[MovedSymbol],
) -> bool {
    if old.0 != new.0 {
        return false;
    }
    if old.1 == new.1 {
        return true;
    }
    let (old_path, old_scope, old_name, old_kind) = &old.1;
    let (new_path, new_scope, new_name, new_kind) = &new.1;
    if old_name != new_name || old_kind != new_kind {
        return false;
    }
    let kind = match *old_kind {
        "struct_item" => "struct",
        "enum_item" => "enum",
        "union_item" => "union",
        _ => return false,
    };
    let inline = |scope: &[String]| (!scope.is_empty()).then(|| scope.join("::"));
    if moved
        .iter()
        .filter(|symbol| {
            symbol.name == *old_name
                && symbol.kind == kind
                && symbol.from == *old_path
                && symbol.to == *new_path
                && symbol.from_inline_module == inline(old_scope)
                && symbol.to_inline_module == inline(new_scope)
        })
        .count()
        == 1
    {
        return true;
    }
    // 型の移動は API の moved に載らない場合もある。束縛解析が一意の定義の移動と
    // 証明した対応を使い、元からある同名の型への付け替えとは区別する。
    let Some(pair) = inputs.rust_crates.pair(inputs.dir, new_path) else {
        return false;
    };
    if pair.old.root_path() != old.0 || pair.new.root_path() != new.0 {
        return false;
    }
    let Some((old_file, new_file)) = pair.current_files(old_path, new_path) else {
        return false;
    };
    let (Some(old_module), Some(new_module)) = (old_file.file_module(), new_file.file_module())
    else {
        return false;
    };
    let old_module: Vec<_> = old_module.iter().chain(old_scope).cloned().collect();
    let new_module: Vec<_> = new_module.iter().chain(new_scope).cloned().collect();
    pair.moves.get(&(new_module, new_name.clone())) == Some(&old_module)
}

pub(super) fn verify(
    inputs: &DetectionInputs<'_>,
    moved: Vec<MovedSymbol>,
    modified: &mut Vec<ApiSymbolChange>,
) -> Vec<MovedSymbol> {
    let mut kept = Vec::new();
    for symbol in &moved {
        let rust = |path: &str| {
            crate::language::LangId::from_path(camino::Utf8Path::new(path)).ok()
                == Some(crate::language::LangId::Rust)
        };
        if !rust(&symbol.from)
            || !rust(&symbol.to)
            || !matches!(symbol.kind.as_str(), "function" | "method" | "constant")
        {
            kept.push(symbol.clone());
            continue;
        }
        let old = declarations(inputs, symbol, true);
        let new = declarations(inputs, symbol, false);
        // 裸の型名の impl の移動は既存の規則に任せ、経路付きの型が名前から落ちる場合を補う。
        if !old
            .iter()
            .chain(&new)
            .any(|(_, context)| context.owner.iter().any(|token| token == "::"))
        {
            kept.push(symbol.clone());
            continue;
        }
        let old_owner = owner(inputs, &symbol.from, &old, true);
        let new_owner = owner(inputs, &symbol.to, &new, false);
        if old_owner
            .as_ref()
            .zip(new_owner.as_ref())
            .is_some_and(|(old, new)| same_owner(inputs, old, new, &moved))
        {
            kept.push(symbol.clone());
            continue;
        }
        modified.push(ApiSymbolChange {
            inline_module: symbol.to_inline_module.clone(),
            unverified: Some(ApiModificationUnverified {
                reason: if old_owner.is_some() && new_owner.is_some() {
                    ApiModificationUnverifiedReason::RustSignatureDependencyChanged
                } else {
                    ApiModificationUnverifiedReason::RustSignatureDependencyUnresolved
                },
                first_unproven: None,
            }),
            ..ApiSymbolChange::new(
                symbol.name.clone(),
                symbol.kind.clone(),
                symbol.to.clone(),
                old.first().map(|(signature, _)| signature.2.clone()),
                new.first().map(|(signature, _)| signature.2.clone()),
            )
        });
    }
    kept
}
