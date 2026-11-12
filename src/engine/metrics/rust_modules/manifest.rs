use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;

use crate::engine::bounded_read;
use crate::error::{AstroError, ErrorCode};

use super::super::resolve::join;

#[derive(Default)]
pub(super) struct Package {
    pub(super) name: String,
    pub(super) library: Option<String>,
    pub(super) external: BTreeSet<String>,
    pub(super) edition: Option<u32>,
}

pub(super) fn load_cargo_packages<T>(
    dir: &Path,
    directories: BTreeSet<String>,
    files: &BTreeMap<String, T>,
    roots: &mut BTreeSet<String>,
    validate: &impl Fn(&Path) -> Result<()>,
) -> Result<BTreeMap<String, Package>> {
    let mut packages = BTreeMap::new();
    for directory in directories {
        let Some((content, value)) = read_cargo_manifest(dir, &directory, validate)? else {
            continue;
        };
        roots.extend(cargo_target_roots(&content, &value, &directory, files)?);
        let package = package_from_manifest(&value, &directory, files)?;
        packages.insert(directory, package);
    }
    Ok(packages)
}

fn read_cargo_manifest(
    dir: &Path,
    directory: &str,
    validate: &impl Fn(&Path) -> Result<()>,
) -> Result<Option<(String, toml::Value)>> {
    let manifest = dir.join(join(directory, "Cargo.toml"));
    if !manifest.try_exists()? {
        return Ok(None);
    }
    validate(&manifest)?;
    if !std::fs::canonicalize(&manifest)?.starts_with(dir) {
        return Err(AstroError::new(
            ErrorCode::PathOutOfBounds,
            "Cargo.toml is outside the selected metrics scope",
        )
        .into());
    }
    if std::fs::symlink_metadata(&manifest)?
        .file_type()
        .is_symlink()
    {
        anyhow::bail!(AstroError::new(
            ErrorCode::InvalidRequest,
            "Symlinked Cargo.toml is not supported by metrics"
        ));
    }
    let content =
        bounded_read::read_utf8_file_limited(&manifest, 100 * 1024 * 1024).ok_or_else(|| {
            anyhow::anyhow!(
                "Cannot read Cargo.toml for metrics: {}",
                join(directory, "Cargo.toml")
            )
        })?;
    let value: toml::Value = toml::from_str(&content).map_err(|e| {
        AstroError::new(
            ErrorCode::InvalidRequest,
            format!("Invalid Cargo.toml: {e}"),
        )
    })?;

    Ok(Some((content, value)))
}

fn cargo_target_roots<T>(
    content: &str,
    value: &toml::Value,
    directory: &str,
    files: &BTreeMap<String, T>,
) -> Result<BTreeSet<String>> {
    let mut roots = BTreeSet::new();
    let targets = crate::engine::rust_targets::RustTargets::parse(content, Path::new(directory))
        .ok_or_else(|| anyhow::anyhow!("Invalid Cargo targets in metrics scope"))?;
    if !targets.roots_complete {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            "Cargo target paths are invalid or leave the selected metrics scope",
        )
        .into());
    }
    for target in targets.explicit_roots() {
        let target = crate::git_support::normalize_workspace_separators(&target.to_string_lossy());
        if files.contains_key(&target) {
            roots.insert(target);
        }
    }
    if targets.has_package {
        let prefix = if directory.is_empty() {
            String::new()
        } else {
            format!("{directory}/")
        };
        for (file, _) in files
            .range(prefix.clone()..)
            .take_while(|(file, _)| file.starts_with(&prefix))
        {
            let relative = &file[prefix.len()..];
            let parts: Vec<_> = relative.split('/').collect();
            let enabled = |flag| {
                value
                    .get("package")
                    .and_then(|v| v.get(flag))
                    .and_then(toml::Value::as_bool)
                    != Some(false)
            };
            let automatic_flag = match parts.as_slice() {
                ["src", "bin", name] if name.ends_with(".rs") => Some("autobins"),
                ["src", "bin", _, "main.rs"] => Some("autobins"),
                ["tests", name] if name.ends_with(".rs") => Some("autotests"),
                ["tests", _, "main.rs"] => Some("autotests"),
                ["examples", name] if name.ends_with(".rs") => Some("autoexamples"),
                ["examples", _, "main.rs"] => Some("autoexamples"),
                ["benches", name] if name.ends_with(".rs") => Some("autobenches"),
                ["benches", _, "main.rs"] => Some("autobenches"),
                _ => None,
            };
            let automatic = automatic_flag.is_some_and(enabled)
                || (relative == "build.rs"
                    && value
                        .get("package")
                        .and_then(|v| v.get("build"))
                        .is_none_or(|value| value.as_bool() == Some(true)));
            if automatic {
                roots.insert(file.clone());
            }
        }
    }

    Ok(roots)
}

fn package_from_manifest<T>(
    value: &toml::Value,
    directory: &str,
    files: &BTreeMap<String, T>,
) -> Result<Package> {
    let package_name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(toml::Value::as_str)
        .unwrap_or("")
        .replace('-', "_");
    let name = value
        .get("lib")
        .and_then(|v| v.get("name"))
        .and_then(toml::Value::as_str)
        .map(|name| name.replace('-', "_"))
        .unwrap_or(package_name);
    let library = value
        .get("lib")
        .and_then(|v| v.get("path"))
        .and_then(toml::Value::as_str)
        .map(|p| join(directory, p))
        .or_else(|| {
            let path = join(directory, "src/lib.rs");
            files.contains_key(&path).then_some(path)
        });

    Ok(Package {
        name,
        library,
        external: external_crates(value),
        edition: cargo_edition(value)?,
    })
}

fn external_crates(value: &toml::Value) -> BTreeSet<String> {
    let mut external = BTreeSet::new();
    let mut patched = BTreeSet::new();
    let mut pending = Vec::new();
    for key in ["patch", "replace"] {
        if let Some(table) = value.get(key).and_then(toml::Value::as_table) {
            pending.extend(table.iter());
        }
    }
    while let Some((name, entry)) = pending.pop() {
        if entry.get("path").is_some() {
            patched.insert(name.split(':').next().unwrap_or(name).replace('-', "_"));
        } else if let Some(table) = entry.as_table() {
            pending.extend(table.iter());
        }
    }
    let mut tables = vec![value];
    if let Some(targets) = value.get("target").and_then(toml::Value::as_table) {
        tables.extend(targets.values());
    }
    for table in tables {
        for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(deps) = table.get(key).and_then(toml::Value::as_table) {
                for (alias, dependency) in deps {
                    if dependency.get("path").is_none()
                        && dependency.get("workspace").is_none()
                        && !patched.contains(&alias.replace('-', "_"))
                        && !dependency
                            .get("package")
                            .and_then(toml::Value::as_str)
                            .is_some_and(|name| patched.contains(&name.replace('-', "_")))
                    {
                        external.insert(alias.replace('-', "_"));
                    }
                }
            }
        }
    }

    external
}

fn cargo_edition(value: &toml::Value) -> Result<Option<u32>> {
    let edition = match value.get("package").and_then(|v| v.get("edition")) {
        Some(toml::Value::String(value)) => Some(
            value
                .parse::<u32>()
                .ok()
                .filter(|v| matches!(v, 2015 | 2018 | 2021 | 2024))
                .ok_or_else(|| {
                    AstroError::new(
                        ErrorCode::InvalidRequest,
                        "Unsupported Cargo edition in metrics",
                    )
                })?,
        ),
        Some(toml::Value::Table(table))
            if table.get("workspace").and_then(toml::Value::as_bool) == Some(true) =>
        {
            None
        }
        None if value.get("package").is_some() => Some(2015),
        None => None,
        _ => {
            return Err(AstroError::new(
                ErrorCode::InvalidRequest,
                "Invalid Cargo edition in metrics",
            )
            .into());
        }
    };

    Ok(edition)
}
