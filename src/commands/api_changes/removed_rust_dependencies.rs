//! 監査済み Cargo 入力から、extern 名が指す通常の path 依存の lib root を証明する。

use super::{ModuleRef, layout};
use crate::engine::impact::rust_cargo_config::CargoConfigProof;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

type DependencyTargets = HashMap<(Rc<str>, String), Option<usize>>;

pub(super) struct DependencyProof {
    root: PathBuf,
    physical_root: Option<PathBuf>,
    config: CargoConfigProof,
    targets: RefCell<DependencyTargets>,
}

impl DependencyProof {
    pub(super) fn new(root: &Path) -> Self {
        Self {
            root: root.to_owned(),
            physical_root: std::fs::canonicalize(root).ok(),
            config: CargoConfigProof::capture(root),
            targets: RefCell::new(HashMap::new()),
        }
    }

    pub(super) fn root(
        &self,
        layout: &layout::Layout,
        context: usize,
        name: &str,
    ) -> Option<ModuleRef> {
        let input = layout
            .crate_inputs
            .get(&layout.crate_root(context).occurrence?)?;
        // 通常依存の存在は build script の extern prelude の根拠にはならない。
        if input.kind == layout::TargetKind::BuildScript {
            return None;
        }
        let key = (input.package.clone(), name.to_owned());
        if let Some(target) = self.targets.borrow().get(&key) {
            return target.map(|root| layout.crate_root(root));
        }
        let target = self.resolve(layout, &input.package, name);
        self.targets.borrow_mut().insert(key, target);
        target.map(|root| layout.crate_root(root))
    }

    fn resolve(&self, layout: &layout::Layout, package: &str, name: &str) -> Option<usize> {
        if !self.config.keeps_dependency_sources(&self.root)
            || !self
                .config
                .keeps_dependency_sources(self.physical_root.as_deref()?)
            || layout.packages.values().any(|manifest| {
                manifest.get("patch").is_some() || manifest.get("replace").is_some()
            })
        {
            return None;
        }
        let manifest = layout.packages.get(package)?;
        if crate_names(manifest).iter().any(|own| own == name) {
            return None;
        }
        let mut target: Option<Rc<str>> = None;
        let mut normal = false;
        for entry in dependency_entries(manifest)? {
            let (spec, base) = dependency_spec(layout, package, manifest, &entry)?;
            validate_spec(&spec)?;
            let path = match spec.get("path") {
                Some(path) => Some(layout.package_at(&base, path.as_str()?)?),
                None => None,
            };
            let extern_name = match &path {
                Some(path) => {
                    let dependency = layout.packages.get(path.as_ref())?;
                    let expected = spec
                        .get("package")
                        .map_or(Some(entry.key), toml::Value::as_str)?;
                    if dependency.get("package")?.get("name")?.as_str()? != expected {
                        return None;
                    }
                    if spec.contains_key("package") {
                        crate_name(entry.key)
                    } else {
                        lib_name(dependency)?
                    }
                }
                None => crate_name(entry.key),
            };
            if extern_name != name {
                continue;
            }
            let path = path?;
            if target.as_ref().is_some_and(|known| *known != path) {
                return None;
            }
            normal |=
                entry.normal && spec.get("optional").and_then(toml::Value::as_bool) != Some(true);
            target = Some(path);
        }
        let target = target.filter(|target| normal && target.as_ref() != package)?;
        is_rust_library(layout.packages.get(target.as_ref())?).then_some(())?;
        let root = *layout.library_roots.get(target.as_ref())?;
        let module = layout.crate_root(root);
        let scopes = layout.file_scopes(&module.file)?;
        (!scopes.scopes.get(module.scope)?.inner_cfg).then_some(root)
    }
}

struct DependencyEntry<'a> {
    key: &'a str,
    spec: &'a toml::Value,
    normal: bool,
}

fn dependency_entries(manifest: &toml::Value) -> Option<Vec<DependencyEntry<'_>>> {
    fn collect<'a>(
        owner: &'a toml::Value,
        top_level: bool,
        entries: &mut Vec<DependencyEntry<'a>>,
    ) -> Option<()> {
        for table_name in [
            "dependencies",
            "dev-dependencies",
            "build-dependencies",
            "dev_dependencies",
            "build_dependencies",
        ] {
            if let Some(table) = owner.get(table_name) {
                for (key, spec) in table.as_table()? {
                    entries.push(DependencyEntry {
                        key,
                        spec,
                        normal: top_level && table_name == "dependencies",
                    });
                }
            }
        }
        Some(())
    }
    let mut entries = Vec::new();
    collect(manifest, true, &mut entries)?;
    if let Some(targets) = manifest.get("target") {
        for target in targets.as_table()?.values() {
            collect(target, false, &mut entries)?;
        }
    }
    Some(entries)
}

fn dependency_spec(
    layout: &layout::Layout,
    package: &str,
    manifest: &toml::Value,
    entry: &DependencyEntry<'_>,
) -> Option<(toml::Table, Rc<str>)> {
    let spec = match entry.spec {
        toml::Value::String(_) => return Some((toml::Table::new(), Rc::from(package))),
        toml::Value::Table(table) => table,
        _ => return None,
    };
    validate_spec(spec)?;
    if spec.get("workspace").and_then(toml::Value::as_bool) != Some(true) {
        return Some((spec.clone(), Rc::from(package)));
    }
    if spec.keys().any(|key| {
        !matches!(
            key.as_str(),
            "workspace" | "features" | "optional" | "default-features" | "default_features"
        )
    }) {
        return None;
    }
    let workspace = if let Some(path) = manifest.get("package")?.get("workspace") {
        layout.package_at(package, path.as_str()?)?
    } else {
        let mut current = Path::new(package);
        loop {
            let candidate = crate::git_support::normalize_workspace_separators(current.to_str()?);
            if layout
                .packages
                .get(candidate.as_str())
                .is_some_and(|manifest| manifest.get("workspace").is_some())
            {
                break Rc::from(candidate);
            }
            current = current.parent()?;
        }
    };
    let inherited = layout
        .packages
        .get(workspace.as_ref())?
        .get("workspace")?
        .get("dependencies")?
        .get(entry.key)?;
    let mut merged = match inherited {
        toml::Value::String(_) => toml::Table::new(),
        toml::Value::Table(table) => table.clone(),
        _ => return None,
    };
    if merged.contains_key("workspace") {
        return None;
    }
    if let Some(optional) = spec.get("optional") {
        merged.insert("optional".to_owned(), optional.clone());
    }
    Some((merged, workspace))
}

/// Cargo が解釈できない既知フィールドを、欠落と同じ意味で扱わない。
fn validate_spec(spec: &toml::Table) -> Option<()> {
    if spec.keys().any(|key| {
        !matches!(
            key.as_str(),
            "path"
                | "package"
                | "version"
                | "git"
                | "branch"
                | "tag"
                | "rev"
                | "registry"
                | "registry-index"
                | "optional"
                | "workspace"
                | "default-features"
                | "default_features"
                | "features"
                | "public"
        )
    }) {
        return None;
    }
    for key in [
        "path",
        "package",
        "version",
        "git",
        "branch",
        "tag",
        "rev",
        "registry",
        "registry-index",
    ] {
        if let Some(value) = spec.get(key) {
            value.as_str()?;
        }
    }
    for key in [
        "optional",
        "workspace",
        "default-features",
        "default_features",
        "public",
    ] {
        if let Some(value) = spec.get(key) {
            value.as_bool()?;
        }
    }
    if let Some(features) = spec.get("features") {
        for feature in features.as_array()? {
            feature.as_str()?;
        }
    }
    if spec.contains_key("path")
        && ["git", "branch", "tag", "rev"]
            .iter()
            .any(|key| spec.contains_key(*key))
    {
        return None;
    }
    if spec
        .get("workspace")
        .is_some_and(|value| value.as_bool() != Some(true))
    {
        return None;
    }
    Some(())
}

fn crate_name(name: &str) -> String {
    name.replace('-', "_")
}

fn lib_name(manifest: &toml::Value) -> Option<String> {
    manifest
        .get("lib")
        .and_then(|lib| lib.get("name"))
        .or_else(|| manifest.get("package")?.get("name"))?
        .as_str()
        .map(crate_name)
}

fn crate_names(manifest: &toml::Value) -> Vec<String> {
    let mut names: Vec<String> = manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .map(crate_name)
        .into_iter()
        .collect();
    names.extend(lib_name(manifest));
    names
}

fn is_rust_library(manifest: &toml::Value) -> bool {
    let Some(package) = manifest.get("package") else {
        return false;
    };
    let lib = manifest.get("lib");
    if let Some(autolib) = package.get("autolib") {
        match autolib.as_bool() {
            Some(true) => {}
            Some(false) if lib.is_some() => {}
            _ => return false,
        }
    }
    for key in ["proc-macro", "proc_macro"] {
        if let Some(value) = lib.and_then(|lib| lib.get(key))
            && value.as_bool() != Some(false)
        {
            return false;
        }
    }
    let crate_types = lib.and_then(|lib| lib.get("crate-type").or_else(|| lib.get("crate_type")));
    crate_types.is_none_or(|types| {
        types.as_array().is_some_and(|types| {
            types.iter().all(|kind| {
                matches!(
                    kind.as_str(),
                    Some("lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro")
                )
            }) && types
                .iter()
                .any(|kind| matches!(kind.as_str(), Some("lib" | "rlib")))
        })
    })
}
