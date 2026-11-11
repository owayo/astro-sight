//! inherent impl の項目の公開 API 面を、impl の対象の型でも決める (Issue #315)。
//!
//! `impl crate::S { pub fn make() }` の `S::make` は、impl を置いた module が非公開でも、公開の型 `S` を通して
//! crate の外から呼べる。impl の項目は、置いたファイルの module の経路と再公開による今までの判定が外としても、
//! impl のあるスコープで対象の型の経路を名前解決し、その型が外と確かめられなければ公開 API 面の中とする
//! (決め切れなければ公開。見逃さない側)。
//!
//! 今までの判定が公開とする (公開の module に置いた) impl の項目は、対象の型が外でも外にしない。型を外と言い
//! 切るには、impl のファイルの全ての置き場 (`include!`・別の crate からの `#[path]` を含む) と、型が届く全ての
//! 経路を確かめる必要があるため。同じファイルに宣言した `pub` でない型の項目は、今までどおり公開の項目の抽出が
//! 外す。

use super::{
    RustPublicApiContext, is_rust_new_symbol_outside_public_api_surface,
    is_rust_old_symbol_outside_public_api_surface,
};
use crate::commands::api_changes::RustDeclarationContext;
use crate::commands::api_changes::rust_dependencies::{self, Candidate, RustCrates};

/// inherent impl の項目の対象の型の公開 API 面。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwnerSurface {
    /// 対象の型が crate の外から届き得る (`pub` の型で、型そのものの公開 API 面の判定が外としない)。
    Public,
    /// 対象の型が crate の外から届かないと確かめた (`pub` でない型、外から届かない module の型)。
    Outside,
    /// 決められない (経路を解けない、型別名・マクロ、別の束縛に替わり得る条件付きの定義など)。公開として扱う。
    Unknown,
}

/// 旧い版 (`old`) か新しい版の、ファイル `path` の宣言 `name` (文脈 `context`) が、crate の外の公開 API 面の外か。
/// 今までどおりファイルの module の経路と再公開で決め、外でも、inherent impl の項目は impl の対象の型が外と
/// 確かめられなければ外にしない (Issue #315)。
#[allow(clippy::too_many_arguments)]
pub(crate) fn rust_declaration_outside_public_api_surface(
    rust_crates: &RustCrates<'_>,
    rust_public: &mut RustPublicApiContext,
    dir: &str,
    base_blobs: &crate::commands::git_input::GitBlobBatch,
    old: bool,
    path: &str,
    name: &str,
    context: Option<&RustDeclarationContext>,
) -> bool {
    let outside = if old {
        is_rust_old_symbol_outside_public_api_surface(dir, base_blobs, path, name, rust_public)
    } else {
        is_rust_new_symbol_outside_public_api_surface(dir, path, name, rust_public)
    };
    if !outside {
        return false;
    }
    let Some(context) = context.filter(|context| !context.owner.is_empty()) else {
        return true;
    };
    // bin だけの crate の項目は、対象の型によらず外。
    let binary = if old {
        rust_public.is_binary_only_at_base(base_blobs, path)
    } else {
        super::is_binary_only_rust_crate(dir, path)
    };
    binary
        || inherent_member_surface(
            rust_crates,
            rust_public,
            dir,
            base_blobs,
            old,
            path,
            context,
        )
        .is_none_or(|surface| surface == OwnerSurface::Outside)
}

/// 旧い版 (`old`) か新しい版の、ファイル `path` の宣言 (文脈 `context`) が inherent impl の項目なら、impl の対象の
/// 型の公開 API 面。impl の項目でなければ None。
pub(crate) fn inherent_member_surface(
    rust_crates: &RustCrates<'_>,
    rust_public: &mut RustPublicApiContext,
    dir: &str,
    base_blobs: &crate::commands::git_input::GitBlobBatch,
    old: bool,
    path: &str,
    context: &RustDeclarationContext,
) -> Option<OwnerSurface> {
    if context.owner.is_empty() {
        return None;
    }
    // impl か囲む項目に置き換え得る属性 (属性マクロなど) があれば、展開が impl の見出し (対象の型) を書き換え
    // 得るので決めない。同じ対象の型の別の impl と鍵が同じになるので、メモには入れない。
    if context.container_macro_attributes {
        return Some(OwnerSurface::Unknown);
    }
    // 候補が宣言の有る条件を覆うかで決め方が変わるので、宣言の有る条件の字面も鍵に入れる。
    let key = (
        old,
        path.to_owned(),
        context.inline_module.clone(),
        context.owner.clone(),
        context.availability_raw.clone(),
    );
    if let Some(&known) = rust_public.owner_memo.get(&key) {
        return Some(known);
    }
    let surface = match rust_crates.library_versions(path) {
        Some(libraries) => {
            let krate = if old { &libraries.old } else { &libraries.new };
            let scope: Vec<String> = if context.inline_module.is_empty() {
                Vec::new()
            } else {
                context
                    .inline_module
                    .split("::")
                    .map(str::to_owned)
                    .collect()
            };
            match rust_dependencies::impl_owner_candidates(krate, path, &scope, &context.owner) {
                Some((candidates, placements)) => {
                    // 条件付きの候補 (`cfg` の付いた定義や取り込み、条件付きで入ったファイル) は、条件の成り立たない
                    // 設定で、名前が glob などの別の束縛に替わり得るが、名前解決はそれを候補に入れない。impl の項目の
                    // 有る全ての設定 (宣言の有る条件と、impl のファイルを置く条件) で候補のどれかが有ると確かめられ
                    // なければ決めない。
                    if rust_dependencies::candidates_cover(
                        context.availability.as_deref(),
                        &placements,
                        &candidates,
                    ) {
                        type_surface(rust_public, dir, base_blobs, old, &candidates)
                    } else {
                        OwnerSurface::Unknown
                    }
                }
                None => OwnerSurface::Unknown,
            }
        }
        None => OwnerSurface::Unknown,
    };
    rust_public.owner_memo.insert(key, surface);
    Some(surface)
}

/// 型の経路を解いた候補 `candidates` から決めた、その型の公開 API 面。どれも項目を置き換え得る属性の無い
/// 構造体・enum・union の定義で、公開 API 面が揃うときだけ決める (`cfg` で分けた定義の公開範囲が違えば
/// 決めない)。
pub(crate) fn type_surface(
    rust_public: &mut RustPublicApiContext,
    dir: &str,
    base_blobs: &crate::commands::git_input::GitBlobBatch,
    old: bool,
    candidates: &[Candidate],
) -> OwnerSurface {
    let mut found = None;
    for candidate in candidates {
        let Some(definition) = &candidate.definition else {
            return OwnerSurface::Unknown;
        };
        if definition.opaque
            || !matches!(definition.kind, "struct_item" | "enum_item" | "union_item")
        {
            return OwnerSurface::Unknown;
        }
        let outside = !definition.public
            || if old {
                is_rust_old_symbol_outside_public_api_surface(
                    dir,
                    base_blobs,
                    &definition.path,
                    &definition.name,
                    rust_public,
                )
            } else {
                is_rust_new_symbol_outside_public_api_surface(
                    dir,
                    &definition.path,
                    &definition.name,
                    rust_public,
                )
            };
        let surface = if outside {
            OwnerSurface::Outside
        } else {
            OwnerSurface::Public
        };
        match found {
            None => found = Some(surface),
            Some(previous) if previous == surface => {}
            Some(_) => return OwnerSurface::Unknown,
        }
    }
    found.unwrap_or(OwnerSurface::Unknown)
}
