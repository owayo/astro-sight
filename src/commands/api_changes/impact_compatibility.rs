//! API の所有型付き互換証拠を、impact の素の名前へ保守的に対応付ける。

use std::collections::{BTreeMap, BTreeSet};

use crate::engine::parser;
use crate::models::impact::DiffFile;
use crate::models::review::CompatibleApiModification;

use super::super::git_input::GitBlobBatch;
use super::{ExportedSymbols, unfiltered_declaration_signatures};
use super::{bare_name, detect_api_changes, filter_exported_symbols, load_new_source};

type DeclarationKey<'a> = (&'a str, &'a str, &'a str);

#[derive(Default)]
struct Declarations {
    all: BTreeMap<String, ExportedSymbols>,
    public: BTreeMap<String, ExportedSymbols>,
}

impl Declarations {
    /// 非公開・ローカル宣言や重複 owner を含む名前は証明しない。
    fn proven_names(&self, compatible: &BTreeSet<DeclarationKey<'_>>) -> BTreeSet<String> {
        self.all
            .iter()
            .filter(|(name, declarations)| {
                self.public.get(*name) == Some(*declarations)
                    && declarations
                        .iter()
                        .map(|(name, kind, _)| (name, kind))
                        .collect::<BTreeSet<_>>()
                        .len()
                        == declarations.len()
                    && declarations.iter().all(|(name, kind, signature)| {
                        compatible.contains(&(name.as_str(), kind.as_str(), signature.as_str()))
                    })
            })
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// 同名参照は別ファイルの owner も含み得るため、diff 全体の未証明な宣言変更を残す。
pub(crate) fn compatible_impact_names(
    dir: &str,
    base: &str,
    diff_files: &[DiffFile],
) -> BTreeMap<String, BTreeSet<String>> {
    let mut candidates: BTreeMap<String, Vec<CompatibleApiModification>> = BTreeMap::new();
    let mut names = BTreeSet::new();
    for change in detect_api_changes(dir, base, diff_files).compatible_modified {
        names.insert(bare_name(&change.name).to_string());
        candidates
            .entry(change.file.clone())
            .or_default()
            .push(change);
    }
    if names.is_empty() {
        return BTreeMap::new();
    }
    let Ok(root) = std::fs::canonicalize(dir) else {
        return BTreeMap::new();
    };

    let blobs = GitBlobBatch::new(dir, base);
    let mut result = BTreeMap::new();
    let mut unverified = BTreeSet::new();
    for file in diff_files {
        let Some((old, new)) = file_declarations(dir, &root, &blobs, file, &names) else {
            return BTreeMap::new();
        };
        let compatible = candidates
            .get(&file.new_path)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let old_keys = compatible
            .iter()
            .filter_map(|change| {
                Some((
                    change.name.as_str(),
                    change.kind.as_str(),
                    change.old_signature.as_deref()?,
                ))
            })
            .collect();
        let new_keys = compatible
            .iter()
            .filter_map(|change| {
                Some((
                    change.name.as_str(),
                    change.kind.as_str(),
                    change.new_signature.as_deref()?,
                ))
            })
            .collect();
        let old_proven = old.proven_names(&old_keys);
        let new_proven = new.proven_names(&new_keys);
        let proven: BTreeSet<_> = old_proven.intersection(&new_proven).cloned().collect();
        for name in &names {
            if old.all.get(name) != new.all.get(name) && !proven.contains(name) {
                unverified.insert(name.clone());
            }
        }
        if !proven.is_empty() {
            result.insert(file.new_path.clone(), proven);
        }
    }
    for proven in result.values_mut() {
        proven.retain(|name| !unverified.contains(name));
    }
    result.retain(|_, proven| !proven.is_empty());
    result
}

fn file_declarations(
    dir: &str,
    root: &std::path::Path,
    blobs: &GitBlobBatch,
    file: &DiffFile,
    names: &BTreeSet<String>,
) -> Option<(Declarations, Declarations)> {
    let old = if file.old_path == "/dev/null" {
        Declarations::default()
    } else {
        declarations(&blobs.read(&file.old_path)?, &file.old_path, names)?
    };
    let new = if file.new_path == "/dev/null" {
        Declarations::default()
    } else {
        // API 候補以外の差分も読むため、symlink 経由の workspace 外参照をここでも拒む。
        let canonical = std::fs::canonicalize(root.join(&file.new_path)).ok()?;
        if !canonical.starts_with(root) {
            return None;
        }
        declarations(
            &load_new_source(dir, &file.new_path)?,
            &file.new_path,
            names,
        )?
    };
    Some((old, new))
}

fn declarations(source: &[u8], path: &str, names: &BTreeSet<String>) -> Option<Declarations> {
    let Ok(lang) = parser::detect_lang(camino::Utf8Path::new(path), source) else {
        let extension = std::path::Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default();
        if crate::engine::refs::is_unanalyzable_source_ext(extension) {
            return None;
        }
        return Some(Declarations::default());
    };
    let tree = parser::parse_source(source, lang).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let symbols = crate::engine::symbols::extract_symbols(root, source, lang).ok()?;
    let group = |declarations: ExportedSymbols| {
        let mut grouped: BTreeMap<String, ExportedSymbols> = BTreeMap::new();
        for declaration in declarations {
            let name = bare_name(&declaration.0);
            if names.contains(name) {
                grouped
                    .entry(name.to_string())
                    .or_default()
                    .push(declaration);
            }
        }
        for declarations in grouped.values_mut() {
            declarations.sort();
        }
        grouped
    };
    Some(Declarations {
        all: group(unfiltered_declaration_signatures(
            &symbols, root, source, lang,
        )),
        public: group(filter_exported_symbols(
            &symbols,
            root,
            source,
            lang,
            true,
            false,
            Some(path),
        )),
    })
}
