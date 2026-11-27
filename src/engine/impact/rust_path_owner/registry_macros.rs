//! serde_json・serde・tracing のマクロを提供する crates.io 依存の出所。
use super::*;

/// 対応する版を crates.io から直接使う単一 package に限定する。Cargo の設定や rename、
/// path・git・patch による同名の差し替え、別ターゲットだけの依存は使わない。
pub(super) fn stock_dependency(
    project: &RustPathOwnerProjectProof,
    file: &Path,
    name: &str,
) -> bool {
    let location = || {
        let module = project.ordinary_private_module(file)?;
        if module.src != Path::new("src")
            || !module.library_only
            || !project.lockfile_unchanged
            || !project.callable_layout_stable()
            || !project.unrelocated(file)
            || project
                .declared_module_file(&module.src, &module.path)
                .as_deref()
                != Some(file)
            || !project.edition_2018_or_later(Path::new(""))?
        {
            return None;
        }
        let known_settings = if name == "tracing" {
            // staged review の中の設定は index、その外側は呼び出し元の checkout で検証する。
            project.layout_cargo_settings_known()
        } else {
            project
                .cargo_configs
                .keeps_dependency_sources(&project.layout_checkout)
                && project
                    .cargo_configs
                    .keeps_dependency_sources(&project.root)
        };
        known_settings.then_some(())
    };
    if location().is_none() {
        return false;
    }
    let cached = project
        .registry_macros
        .lock()
        .expect("registry macro cache poisoned")
        .get(name)
        .copied();
    if let Some(ok) = cached {
        return ok;
    }
    let prove = || {
        let checkout =
            super::super::rust_cargo_config::absolute_package_root(&project.layout_checkout)?;
        let physical = checkout.canonicalize().ok()?;
        for root in [&checkout, &physical] {
            if project
                .cargo_configs
                .ancestors(root)
                .skip(1)
                .any(|parent| parent.join("Cargo.toml").exists())
            {
                return None;
            }
        }
        let manifest = project.alias_cache.manifest(&project.root, Path::new(""))?;
        if [
            "dev-dependencies",
            "build-dependencies",
            "dev_dependencies",
            "build_dependencies",
        ]
        .iter()
        .any(|key| manifest.get(key).and_then(|t| t.get(name)).is_some())
            || manifest.get("workspace").is_some()
            || manifest.get("replace").is_some()
            || manifest.get("package")?.get("workspace").is_some()
            || manifest
                .get("lib")
                .and_then(|t| t.get("name"))
                .or_else(|| manifest.get("package")?.get("name"))?
                .as_str()?
                .replace('-', "_")
                == name
        {
            return None;
        }
        let lock_source = read_input_text_file(&project.root.join("Cargo.lock"))?;
        let lock = toml::from_str::<toml::Value>(&lock_source).ok()?;
        let names = registry_closure(&lock, name)?;
        if name == "serde" && !names.contains("serde_derive") {
            return None;
        }
        if let Some(patches) = manifest.get("patch") {
            for table in patches.as_table()?.values() {
                if table.as_table()?.iter().any(|(alias, spec)| {
                    names.contains(alias.as_str())
                        || spec
                            .get("package")
                            .and_then(toml::Value::as_str)
                            .is_some_and(|n| names.contains(n))
                }) {
                    return None;
                }
            }
        }
        if let Some(targets) = manifest.get("target") {
            for target in targets.as_table()?.values() {
                if [
                    "dependencies",
                    "dev-dependencies",
                    "build-dependencies",
                    "dev_dependencies",
                    "build_dependencies",
                ]
                .iter()
                .any(|k| target.get(k).and_then(|t| t.get(name)).is_some())
                {
                    return None;
                }
            }
        }
        let spec = manifest.get("dependencies")?.get(name)?;
        let version = if let Some(table) = spec.as_table() {
            if !table.keys().all(|k| {
                matches!(
                    k.as_str(),
                    "version" | "features" | "default-features" | "optional"
                )
            }) || table
                .get("optional")
                .is_some_and(|v| v.as_bool() != Some(false))
            {
                return None;
            }
            table.get("version")?.as_str()?
        } else {
            spec.as_str()?
        };
        let version = version.trim_start_matches(['=', '~', '^']);
        let series = registry_series(name);
        if !(version == series.trim_end_matches('.') || version.starts_with(series))
            || !version.bytes().all(|b| b == b'.' || b.is_ascii_digit())
        {
            return None;
        }
        // tracing の名前解決は呼び出し位置の module と block で別途確かめる。
        if name == "tracing" {
            return Some(());
        }
        let root_file = Path::new("src/lib.rs");
        let root = read_input_file(&project.root.join(root_file))?;
        let previous = match project.reexport_old_sources.get(root_file) {
            Some(source) => Some(source.as_deref()?),
            None => None,
        };
        for source in std::iter::once(root.as_slice()).chain(previous) {
            let tree = parser::parse_source(source, LangId::Rust).ok()?;
            if !unshadowed_root(tree.root_node(), source, name) {
                return None;
            }
        }
        Some(())
    };
    let known = prove().is_some();
    project
        .registry_macros
        .lock()
        .expect("registry macro cache poisoned")
        .insert(name.to_owned(), known);
    known
}

fn registry_series(name: &str) -> &'static str {
    if name == "tracing" { "0.1." } else { "1." }
}

/// 同名の複数バージョンも含め、derive が実際に使う実装（serde_derive・quote など）まで調べる。
fn registry_closure<'a>(lock: &'a toml::Value, name: &'a str) -> Option<HashSet<&'a str>> {
    let mut packages = HashMap::<&str, Vec<&toml::Value>>::new();
    for pkg in lock.get("package")?.as_array()? {
        packages
            .entry(pkg.get("name")?.as_str()?)
            .or_default()
            .push(pkg);
    }
    let mut seen = HashSet::new();
    let mut pending = vec![name];
    while let Some(next) = pending.pop() {
        if !seen.insert(next) {
            continue;
        }
        for pkg in packages.get(next)? {
            if pkg.get("source")?.as_str()?
                != "registry+https://github.com/rust-lang/crates.io-index"
                || pkg.get("checksum")?.as_str()?.len() != 64
                || next == name
                    && !pkg
                        .get("version")?
                        .as_str()?
                        .starts_with(registry_series(name))
            {
                return None;
            }
            if let Some(deps) = pkg.get("dependencies") {
                for dep in deps.as_array()? {
                    pending.push(dep.as_str()?.split_ascii_whitespace().next()?);
                }
            }
        }
    }
    Some(seen)
}

fn unshadowed_root(root: Node<'_>, source: &[u8], name: &str) -> bool {
    let mut cursor = root.walk();
    !root.has_error()
        && root
            .named_children(&mut cursor)
            .all(|item| match item.kind() {
                "use_declaration" => root_use_names(item, source)
                    .is_some_and(|names| names.iter().all(|n| n.is_ascii() && n != name)),
                "attribute_item" | "inner_attribute_item" => {
                    super::super::rust_std_origin::safe_attribute(item, source)
                        .is_some_and(|names| names.is_empty())
                }
                "macro_invocation" | "extern_crate_declaration" => false,
                _ => item
                    .child_by_field_name("name")
                    .and_then(|n| n.utf8_text(source).ok())
                    .is_none_or(|n| n.is_ascii() && n.trim_start_matches("r#") != name),
            })
}

impl RustPathOwnerIndex {
    /// json! の JSON 値に埋めた式。別マクロ・属性・マクロ定義を含む入力は保留する。
    pub(in crate::engine::impact) fn json_expression(
        &self,
        node: Node<'_>,
        source: &[u8],
        file: &Path,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        let mut parent = node.parent();
        while parent.is_some_and(|n| n.kind() == "token_tree") {
            parent = parent.and_then(|n| n.parent());
        }
        let Some(invocation) = parent.filter(|n| n.kind() == "macro_invocation") else {
            return false;
        };
        let Some(callee) = invocation.child_by_field_name("macro") else {
            return false;
        };
        if callee.utf8_text(source).ok() != Some("serde_json::json")
            || !self.unshadowed_qualified_paths.contains(&callee.id())
        {
            return false;
        }
        if let Some(result) = self.json_calls.borrow().get(&invocation.id()) {
            return *result;
        }
        let mut root = invocation;
        while let Some(p) = root.parent() {
            root = p;
        }
        let text = invocation.utf8_text(source).unwrap_or("");
        let safe = text
            .strip_prefix("serde_json::json!")
            .is_some_and(|args| !args.contains(['!', '#', '$']) && !args.contains("macro_rules"))
            && !project.reexport_old_sources.contains_key(file)
            && unshadowed_root(root, source, "serde_json")
            && stock_dependency(project, file, "serde_json");
        self.json_calls.borrow_mut().insert(invocation.id(), safe);
        safe
    }
}

/// 直接 import した Serialize/Deserialize か、明示した serde::Deserialize などに限る。
pub(super) fn serde_attribute(
    root: Node<'_>,
    item: Node<'_>,
    source: &[u8],
) -> Option<Vec<String>> {
    let direct = |name: &str| {
        let mut cursor = root.walk();
        root.named_children(&mut cursor).any(|n| {
            n.kind() == "use_declaration"
                && !has_attribute_prefix(n)
                && n.child_by_field_name("argument")
                    .and_then(|a| a.utf8_text(source).ok())
                    .is_some_and(|path| path.strip_prefix("serde::") == Some(name))
        })
    };
    let known_derive = |attr: Node<'_>| {
        let text = attr
            .utf8_text(source)
            .ok()?
            .strip_prefix("#[derive(")?
            .strip_suffix(")]")?;
        let mut serde = false;
        let mut shadowable = vec!["derive".to_owned(), "serde".to_owned()];
        for name in text.split(',').map(str::trim) {
            if matches!(name, "serde::Deserialize" | "serde::Serialize")
                || matches!(name, "Serialize" | "Deserialize") && direct(name)
            {
                serde = true;
            } else if !super::super::rust_std_origin::builtin_derive(name) {
                return None;
            } else {
                shadowable.push(name.to_owned());
            }
        }
        serde.then_some(shadowable)
    };
    if let Some(names) = known_derive(item) {
        return Some(names);
    }
    if !item
        .utf8_text(source)
        .is_ok_and(|text| text.starts_with("#[serde("))
    {
        return None;
    }
    let mut prev = item.prev_named_sibling();
    while let Some(a) = prev.filter(|a| a.kind() == "attribute_item") {
        if let Some(names) = known_derive(a) {
            return Some(names);
        }
        prev = a.prev_named_sibling();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::registry_closure;

    #[test]
    fn serde_macro_closure_requires_registry_procedural_macro_dependencies() {
        let source = "registry+https://github.com/rust-lang/crates.io-index";
        let checksum = "a".repeat(64);
        let registry = format!("source='{source}'\nchecksum='{checksum}'");
        let base = format!(
            "version=4\n[[package]]\nname='serde'\nversion='1.0.0'\n{registry}\ndependencies=['serde_derive']\n[[package]]\nname='serde_derive'\nversion='1.0.0'\n{registry}\ndependencies=['quote 1.0.0']\n[[package]]\nname='quote'\nversion='1.0.0'\n"
        );
        for (origin, known) in [(registry.as_str(), true), ("", false)] {
            let lock = toml::from_str(&format!("{base}{origin}")).unwrap();
            let names = registry_closure(&lock, "serde");
            assert_eq!(
                names
                    .as_ref()
                    .is_some_and(|set| set.contains("serde_derive") && set.contains("quote")),
                known
            );
        }
    }
}
