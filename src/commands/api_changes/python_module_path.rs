//! 通常の Python パッケージだけを root / src の正確なモジュール名から解決する。
//! namespace、別 root、相対 import、再 export はこの証明の対象外。

use std::path::{Path, PathBuf};

fn exact_entry(parent: &Path, name: &str) -> Option<PathBuf> {
    let mut result = None;
    for entry in std::fs::read_dir(parent).ok()? {
        let entry = entry.ok()?;
        if entry.file_name() == name {
            if entry.file_type().ok()?.is_symlink() || result.is_some() {
                return None;
            }
            result = Some(entry.path());
        }
    }
    result
}

fn competing_module(parent: &Path, name: &str) -> Option<bool> {
    for entry in std::fs::read_dir(parent).ok()? {
        let entry = entry.ok()?;
        let file = entry.file_name();
        let file = file.to_str()?;
        if file.starts_with(&format!("{name}."))
            && matches!(
                Path::new(file).extension().and_then(|e| e.to_str()),
                Some("so" | "pyd" | "pyc" | "pyi")
            )
        {
            return Some(true);
        }
    }
    Some(false)
}

fn package_initializer(parent: &Path) -> Option<PathBuf> {
    if competing_module(parent, "__init__")? {
        return None;
    }
    exact_entry(parent, "__init__.py")
}

fn inert_initializer(init: &Path) -> Option<()> {
    let path = camino::Utf8Path::from_path(init)?;
    let source = crate::engine::parser::read_file(path).ok()?;
    if source.len() > 1_000_000 {
        return None;
    }
    let tree =
        crate::engine::parser::parse_source(&source, crate::language::LangId::Python).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut cursor = root.walk();
    if root.named_children(&mut cursor).all(|node| {
        matches!(node.kind(), "comment" | "pass_statement")
            || (node.kind() == "expression_statement"
                && node.named_child(0).is_some_and(|n| {
                    let mut cursor = n.walk();
                    n.kind() == "string"
                        && !n
                            .named_children(&mut cursor)
                            .any(|child| child.kind() == "interpolation")
                }))
    }) {
        Some(())
    } else {
        None
    }
}

pub(crate) fn resolve(dir: &str, module: &str) -> Option<String> {
    resolve_with_package_check(dir, module, inert_initializer)
}

pub(super) fn resolve_with_package_check(
    dir: &str,
    module: &str,
    mut package_check: impl FnMut(&Path) -> Option<()>,
) -> Option<String> {
    let parts: Vec<_> = module.split('.').collect();
    if parts.is_empty()
        || parts.iter().any(|part| {
            part.is_empty()
                || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                || part.as_bytes()[0].is_ascii_digit()
        })
    {
        return None;
    }
    let root = std::fs::canonicalize(dir).ok()?;
    let roots = [root.clone(), root.join("src")];
    let mut selected = None;
    for source_root in roots {
        if !source_root.try_exists().ok()? {
            continue;
        }
        if std::fs::symlink_metadata(&source_root)
            .ok()?
            .file_type()
            .is_symlink()
        {
            return None;
        }
        let first = parts[0];
        let has_prefix = source_root.join(first).try_exists().ok()?
            || source_root.join(format!("{first}.py")).try_exists().ok()?
            || competing_module(&source_root, first)?;
        if !has_prefix {
            continue;
        }
        if selected.is_some() {
            return None;
        }
        selected = Some(source_root);
    }
    let mut parent = selected?;
    for package in &parts[..parts.len() - 1] {
        if competing_module(&parent, package)?
            || parent.join(format!("{package}.py")).try_exists().ok()?
        {
            return None;
        }
        parent = exact_entry(&parent, package)?;
        if !parent.is_dir() {
            return None;
        }
        package_check(&package_initializer(&parent)?)?;
    }
    let name = parts.last()?;
    if competing_module(&parent, name)? {
        return None;
    }
    let file = exact_entry(&parent, &format!("{name}.py"));
    if file.is_none() && parent.join(format!("{name}.py")).try_exists().ok()? {
        return None;
    }
    let package_dir = exact_entry(&parent, name);
    if package_dir.is_none() && parent.join(name).try_exists().ok()? {
        return None;
    }
    let package = if let Some(p) = package_dir {
        if p.is_dir()
            && (p.join("__init__.py").try_exists().ok()? || competing_module(&p, "__init__")?)
        {
            package_check(&package_initializer(&p)?)?;
            Some(exact_entry(&p, "__init__.py")?)
        } else {
            None
        }
    } else {
        None
    };
    let path = match (file, package) {
        (Some(file), None) | (None, Some(file)) => file,
        _ => return None,
    };
    if !path.is_file() {
        return None;
    }
    let path = std::fs::canonicalize(path).ok()?;
    let relative = path.strip_prefix(&root).ok()?.to_str()?;
    Some(crate::git_support::normalize_workspace_separators(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_roots_do_not_resolve_suffix_matches_or_competing_packages() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("workspace/pkg")).unwrap();
        std::fs::write(root.join("workspace/pkg/api.py"), "").unwrap();
        assert_eq!(resolve(root.to_str().unwrap(), "pkg.api"), None);
        std::fs::create_dir_all(root.join("src/pkg")).unwrap();
        std::fs::write(root.join("src/pkg/__init__.py"), "").unwrap();
        std::fs::write(root.join("src/pkg/api.py"), "").unwrap();
        assert_eq!(
            resolve(root.to_str().unwrap(), "pkg.api").as_deref(),
            Some("src/pkg/api.py")
        );
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/__init__.py"), "").unwrap();
        // 別 root の同名パッケージには対象モジュールが無くても曖昧。
        assert_eq!(resolve(root.to_str().unwrap(), "pkg.api"), None);
    }

    #[test]
    fn namespace_case_stubs_and_package_initialization_remain_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let text = root.to_str().unwrap();
        std::fs::create_dir_all(root.join("pkg")).unwrap();
        std::fs::write(root.join("pkg/api.py"), "").unwrap();
        assert_eq!(resolve(text, "pkg.api"), None);
        std::fs::write(
            root.join("pkg/__init__.py"),
            "\"package\"\n# marker\npass\n",
        )
        .unwrap();
        assert_eq!(resolve(text, "pkg.api").as_deref(), Some("pkg/api.py"));
        assert_eq!(resolve(text, "pkg.API"), None);
        std::fs::write(root.join("pkg/api.pyi"), "").unwrap();
        assert_eq!(resolve(text, "pkg.api"), None);
        std::fs::remove_file(root.join("pkg/api.pyi")).unwrap();
        std::fs::write(root.join("pkg/api.test.so"), "").unwrap();
        assert_eq!(resolve(text, "pkg.api"), None);
        std::fs::remove_file(root.join("pkg/api.test.so")).unwrap();
        std::fs::write(root.join("pkg/__init__.py"), "__path__ = ['elsewhere']\n").unwrap();
        assert_eq!(resolve(text, "pkg.api"), None);
        std::fs::write(root.join("pkg/__init__.py"), "f'{mutate()}'\n").unwrap();
        assert_eq!(resolve(text, "pkg.api"), None);
        std::fs::write(root.join("pkg/__init__.py"), "").unwrap();
        std::fs::create_dir(root.join("pkg/api")).unwrap();
        std::fs::write(
            root.join("pkg/api/__init__.py"),
            "from elsewhere import *\n",
        )
        .unwrap();
        assert_eq!(resolve(text, "pkg.api"), None);
    }
}
