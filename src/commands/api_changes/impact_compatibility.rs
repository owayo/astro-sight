//! API の所有型付き互換証拠を、impact の素の名前へ保守的に対応付ける。

use std::collections::{BTreeMap, BTreeSet};

use crate::engine::parser;
use crate::models::impact::DiffFile;
use crate::models::review::{ApiChanges, CompatibleApiModification};

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
    let api = detect_api_changes(dir, base, diff_files);
    compatible_impact_names_for_api(dir, base, diff_files, &api)
}

/// review と同じ基点・新側ソースから得た API の分類を再利用する。
pub(crate) fn compatible_impact_names_for_api(
    dir: &str,
    base: &str,
    diff_files: &[DiffFile],
    api: &ApiChanges,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut candidates: BTreeMap<String, Vec<CompatibleApiModification>> = BTreeMap::new();
    let mut names = BTreeSet::new();
    let mut changes = api.compatible_modified.clone();
    let closed_returns = super::type_dependency_symbols::closed_return_declarations(api);
    let handoffs = if api.modified_closed_in_diff.iter().any(|change| {
        !closed_returns.contains_key(&(change.file.as_str(), change.name.as_str()))
            && matches!(change.kind.as_str(), "function" | "interface" | "trait")
            && matches!(
                crate::language::LangId::from_path(camino::Utf8Path::new(&change.file)),
                Ok(crate::language::LangId::Typescript | crate::language::LangId::Tsx)
            )
    }) {
        let blobs = GitBlobBatch::new(dir, base);
        super::ts_function_values::HandoffProof::build(dir, &blobs, diff_files)
    } else {
        None
    };
    for change in &api.modified_closed_in_diff {
        let reason = if closed_returns.contains_key(&(change.file.as_str(), change.name.as_str())) {
            "optional_return_properties_removed"
        } else if handoffs
            .as_ref()
            .is_some_and(|proof| proof.covers(&change.file, &change.name))
        {
            "closed_function_value_handoff"
        } else {
            continue;
        };
        changes.push(CompatibleApiModification {
            change_evidence: None,
            inline_module: change.inline_module.clone(),
            name: change.name.clone(),
            kind: change.kind.clone(),
            file: change.file.clone(),
            old_signature: change.old_signature.clone(),
            new_signature: change.new_signature.clone(),
            reason: reason.to_owned(),
        });
    }
    for change in changes {
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
        let Some((old, mut new)) = file_declarations(dir, &root, &blobs, file, &names) else {
            return BTreeMap::new();
        };
        let compatible = candidates
            .get(&file.new_path)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        // import へ置き換えた関数は宣言抽出に現れない。同値証明済みの公開束縛だけ補う。
        for change in compatible
            .iter()
            .filter(|change| change.reason == "python_import_rebinding")
        {
            let name = bare_name(&change.name).to_owned();
            if !new.all.contains_key(&name)
                && let Some(signature) = &change.new_signature
            {
                let declaration =
                    vec![(change.name.clone(), change.kind.clone(), signature.clone())];
                new.all.insert(name.clone(), declaration.clone());
                new.public.insert(name, declaration);
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::tests::common::{
        git_commit_files, init_git_repo_for_test, resolve_git_diff_parts,
    };

    #[test]
    fn closed_return_impact_needs_the_result_use_proof() {
        let repo = tempfile::tempdir().unwrap();
        let dir = repo.path();
        let source = "type Reply = { ready?: boolean; legacy?: boolean };\nexport function probe(target: string): Promise<Reply | undefined> { return Promise.resolve(target ? { ready: true } : undefined); }\n";
        let caller = "import { probe } from './probe.js';\nexport async function read() { return (await probe('x'))?.ready; }\n";
        init_git_repo_for_test(dir);
        git_commit_files(
            dir,
            &[("probe.ts", source), ("caller.ts", caller)],
            "initial",
        );
        std::fs::write(
            dir.join("probe.ts"),
            source.replace(" legacy?: boolean", ""),
        )
        .unwrap();
        let files = crate::engine::diff::parse_unified_diff(&resolve_git_diff_parts(dir).0);
        let dir = dir.to_str().unwrap();
        let mut api = detect_api_changes(dir, "HEAD", &files);
        assert!(api.type_dependency_symbols["probe.ts"].contains("probe"));
        let proven = |api: &ApiChanges| compatible_impact_names_for_api(dir, "HEAD", &files, api);
        assert!(proven(&api)["probe.ts"].contains("probe"));
        let original = api.modified_closed_in_diff[0].clone();
        for kind in ["function", "type"] {
            let mut other = original.clone();
            other.kind = kind.to_owned();
            api.modified.push(other.clone());
            assert!(proven(&api).is_empty(), "{kind}");
            api.modified.pop();
            api.modified_closed_in_diff.push(other);
            assert!(proven(&api).is_empty(), "{kind}");
            api.modified_closed_in_diff.pop();
        }
        api.modified_closed_in_diff[0].kind = "type".into();
        assert!(proven(&api).is_empty());
        api.modified_closed_in_diff[0] = original;
        // 型依存の証拠がない closed を同じ形の戻り値とみなさない。
        api.type_dependency_symbols.clear();
        assert!(proven(&api).is_empty());
    }

    #[test]
    fn python_compatible_method_names_require_every_changed_declaration_proven() {
        let order = "class Order:\n    def sealed(self):\n        return True\n";
        let compatible = order.replace("self)", "self, limit=None)");
        for (old_other, new_other) in [
            (
                "class Other:\n    def sealed(self):\n        return True\n",
                "class Other:\n    def sealed(self, required):\n        return True\n",
            ),
            (
                "class _Private:\n    def sealed(self):\n        return True\n",
                "class _Private:\n    def sealed(self, required):\n        return True\n",
            ),
            (
                "class _Private:\n    def sealed(self):\n        return True\n",
                "class _Private:\n    def sealed(self):\n        return False\n",
            ),
            (
                "def factory():\n    def sealed():\n        return True\n    return sealed\n",
                "def factory():\n    def sealed(required):\n        return True\n    return sealed\n",
            ),
            (
                "def factory():\n    def sealed():\n        return True\n    return sealed\n",
                "def factory():\n    def sealed():\n        return False\n    return sealed\n",
            ),
        ] {
            for same_file in [false, true] {
                let repo = tempfile::tempdir().unwrap();
                let dir = repo.path();
                let path = if same_file { "order.py" } else { "other.py" };
                let before = if same_file {
                    format!("{order}\n{old_other}")
                } else {
                    old_other.to_owned()
                };
                init_git_repo_for_test(dir);
                git_commit_files(dir, &[("order.py", order), (path, &before)], "initial");
                std::fs::write(dir.join("order.py"), &compatible).unwrap();
                let after = if same_file {
                    format!("{compatible}\n{new_other}")
                } else {
                    new_other.to_owned()
                };
                std::fs::write(dir.join(path), after).unwrap();
                let (diff, _) = resolve_git_diff_parts(dir);
                let files = crate::engine::diff::parse_unified_diff(&diff);
                let api = detect_api_changes(dir.to_str().unwrap(), "HEAD", &files);
                assert!(
                    api.compatible_modified
                        .iter()
                        .any(|change| change.name == "Order.sealed"),
                    "{api:?}"
                );
                let proven =
                    compatible_impact_names_for_api(dir.to_str().unwrap(), "HEAD", &files, &api);
                assert!(
                    !proven.contains_key(path),
                    "{same_file}/{new_other}: {proven:?}"
                );
                if old_other.lines().find(|line| line.contains("def sealed"))
                    != new_other.lines().find(|line| line.contains("def sealed"))
                {
                    assert!(proven.is_empty(), "{same_file}/{new_other}: {proven:?}");
                }
            }
        }
    }
}
