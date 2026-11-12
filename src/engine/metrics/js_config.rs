use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::engine::bounded_read;
use crate::error::{AstroError, ErrorCode};
use crate::language::LangId;
use crate::models::metrics::MetricsDiagnostic;

use super::resolve::{diagnostic, join, js_candidates, parent};

#[derive(Clone, Default)]
struct Config {
    base: Option<String>,
    mappings: BTreeMap<String, Vec<String>>,
    origin: String,
    unsupported: bool,
}

#[derive(Clone, Default)]
struct Package {
    name: String,
    external: BTreeSet<String>,
}

#[derive(Default)]
pub(super) struct JsConfigs {
    files: BTreeMap<String, (Config, Package)>,
    pub diagnostics: Vec<MetricsDiagnostic>,
}

impl JsConfigs {
    pub fn build(
        dir: &Path,
        paths: &BTreeMap<String, usize>,
        validate: &impl Fn(&Path) -> Result<()>,
    ) -> Result<Self> {
        let mut result = Self::default();
        let mut configs = BTreeMap::new();
        let mut packages = BTreeMap::new();
        for file in paths.keys() {
            let Ok(lang) = LangId::from_path(camino::Utf8Path::new(file)) else {
                continue;
            };
            if !matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx) {
                continue;
            }
            let names = if lang == LangId::Javascript {
                ["jsconfig.json", "tsconfig.json"]
            } else {
                ["tsconfig.json", "jsconfig.json"]
            };
            let config = if let Some(path) = nearest(dir, file, &names)? {
                if !configs.contains_key(&path) {
                    let config = load_config(dir, &path, validate, &mut result.diagnostics)?;
                    configs.insert(path.clone(), config);
                }
                configs[&path].clone()
            } else {
                Config::default()
            };
            let package = if let Some(path) = nearest(dir, file, &["package.json"])? {
                if !packages.contains_key(&path) {
                    let text = read(dir, &path, validate)?;
                    let value: Value = serde_json::from_str(&text)
                        .map_err(|e| invalid(format!("Invalid {path}: {e}")))?;
                    if !value.is_object() {
                        return Err(invalid(format!("{path} must contain an object")));
                    }
                    let mut package = Package {
                        name: value
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                        ..Default::default()
                    };
                    for field in [
                        "dependencies",
                        "devDependencies",
                        "peerDependencies",
                        "optionalDependencies",
                    ] {
                        if let Some(dependencies) = value.get(field) {
                            let dependencies = dependencies
                                .as_object()
                                .ok_or_else(|| invalid(format!("Invalid {field} in {path}")))?;
                            for (name, version) in dependencies {
                                let version = version.as_str().ok_or_else(|| {
                                    invalid(format!("Invalid dependency version in {path}"))
                                })?;
                                if !["workspace:", "file:", "link:"]
                                    .iter()
                                    .any(|prefix| version.starts_with(prefix))
                                {
                                    package.external.insert(name.clone());
                                }
                            }
                        }
                    }
                    packages.insert(path.clone(), package);
                }
                packages[&path].clone()
            } else {
                Package::default()
            };
            result.files.insert(file.clone(), (config, package));
        }
        let local_names: BTreeSet<_> = result
            .files
            .values()
            .map(|(_, package)| package.name.clone())
            .filter(|name| !name.is_empty())
            .collect();
        for (_, package) in result.files.values_mut() {
            package.external.retain(|name| !local_names.contains(name));
        }
        result.diagnostics.sort_by(|a, b| {
            a.path
                .cmp(&b.path)
                .then(a.reason.cmp(&b.reason))
                .then(a.message.cmp(&b.message))
        });
        result
            .diagnostics
            .dedup_by(|a, b| a.path == b.path && a.reason == b.reason && a.message == b.message);
        Ok(result)
    }

    pub fn candidates(
        &self,
        file: &str,
        target: &str,
        lang: LangId,
    ) -> (Vec<Vec<String>>, &'static str, &'static str) {
        if target.is_empty() {
            return (Vec::new(), "bare_import", "empty_import_specifier");
        }
        if target.starts_with("./") || target.starts_with("../") {
            return (
                js_candidates(&join(&parent(file), target), lang),
                "relative_import",
                "local_target_unresolved",
            );
        }
        let Some((config, package)) = self.files.get(file) else {
            return (Vec::new(), "bare_import", "bare_specifier_unresolved");
        };
        if config.unsupported {
            return (Vec::new(), "unsupported_config", "unsupported_config");
        }
        let package_name = if target.starts_with('@') {
            target.split('/').take(2).collect::<Vec<_>>().join("/")
        } else {
            target.split('/').next().unwrap_or("").to_string()
        };
        let external = target.starts_with("node:")
            || (package_name != package.name && package.external.contains(&package_name));
        let mut matches = Vec::new();
        for (pattern, targets) in &config.mappings {
            if pattern == target {
                matches.push((usize::MAX, targets, ""));
                continue;
            }
            if let Some((prefix, suffix)) = pattern.split_once('*')
                && target.starts_with(prefix)
                && target.ends_with(suffix)
                && target.len() >= prefix.len() + suffix.len()
            {
                matches.push((
                    prefix.len(),
                    targets,
                    &target[prefix.len()..target.len() - suffix.len()],
                ));
            }
        }
        matches.sort_by_key(|entry| std::cmp::Reverse(entry.0));
        if let Some((score, targets, wildcard)) = matches.first() {
            if matches.iter().filter(|m| m.0 == *score).count() != 1 {
                return (Vec::new(), "typescript_paths", "ambiguous_path_mapping");
            }
            let base = config.base.as_deref().unwrap_or(&config.origin);
            let tiers = targets
                .iter()
                .flat_map(|path| {
                    js_candidates(
                        &join(base, &path.replace('*', wildcard).replace('\\', "/")),
                        lang,
                    )
                })
                .collect();
            return (tiers, "typescript_paths", "configured_target_unresolved");
        }
        if let Some(base) = &config.base {
            return (
                js_candidates(&join(base, target), lang),
                "typescript_base_url",
                if external {
                    "external"
                } else {
                    "bare_specifier_unresolved"
                },
            );
        }
        if external {
            return (Vec::new(), "declared_package", "external");
        }
        (Vec::new(), "bare_import", "bare_specifier_unresolved")
    }
}

fn nearest(dir: &Path, file: &str, names: &[&str]) -> Result<Option<String>> {
    let mut directory = parent(file);
    loop {
        for name in names {
            let path = join(&directory, name);
            if dir.join(&path).try_exists()? {
                return Ok(Some(path));
            }
        }
        if directory.is_empty() {
            return Ok(None);
        }
        directory = parent(&directory);
    }
}

fn read(dir: &Path, path: &str, validate: &impl Fn(&Path) -> Result<()>) -> Result<String> {
    let path = dir.join(path);
    validate(&path)?;
    if !std::fs::canonicalize(&path)?.starts_with(dir) {
        return Err(AstroError::new(
            ErrorCode::PathOutOfBounds,
            "Metrics project configuration is outside the selected scope",
        )
        .into());
    }
    if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
        return Err(invalid(
            "Symlinked metrics project configuration is not supported",
        ));
    }
    bounded_read::read_utf8_file_limited(&path, 100 * 1024 * 1024).ok_or_else(|| {
        invalid("Cannot read metrics project configuration within the 100 MiB limit")
    })
}

fn load_config(
    dir: &Path,
    path: &str,
    validate: &impl Fn(&Path) -> Result<()>,
    diagnostics: &mut Vec<MetricsDiagnostic>,
) -> Result<Config> {
    let mut chain = Vec::new();
    let mut seen = BTreeSet::new();
    let mut path = path.to_string();
    loop {
        if !seen.insert(path.clone()) {
            return Err(invalid("Cycle in TypeScript configuration extends chain"));
        }
        let text = read(dir, &path, validate)?;
        let value = jsonc(&text).map_err(|e| invalid(format!("Invalid {path}: {e}")))?;
        if !value.is_object() {
            return Err(invalid(format!("{path} must contain an object")));
        }
        let extends = value.get("extends").cloned();
        chain.push((path.clone(), value));
        let Some(extends) = extends else {
            break;
        };
        if !extends.is_string()
            && !extends
                .as_array()
                .is_some_and(|values| values.iter().all(Value::is_string))
        {
            return Err(invalid(format!("Invalid extends in {path}")));
        }
        let Some(base) = extends
            .as_str()
            .filter(|s| s.starts_with("./") || s.starts_with("../"))
        else {
            diagnostics.push(diagnostic(
                &path,
                None,
                "typescript_config_unsupported",
                "Only single relative-file extends chains are resolved".into(),
            ));
            return Ok(Config {
                unsupported: true,
                ..Default::default()
            });
        };
        path = join(&parent(&path), &base.replace('\\', "/"));
        if !path.ends_with(".json") {
            path.push_str(".json");
        }
        if path.starts_with("../") || path.starts_with("<unresolved>") {
            return Err(invalid(
                "TypeScript configuration extends escapes the selected scope",
            ));
        }
    }
    let mut result = Config::default();
    for (path, value) in chain.into_iter().rev() {
        let Some(options) = value.get("compilerOptions") else {
            continue;
        };
        let options = options
            .as_object()
            .ok_or_else(|| invalid(format!("Invalid compilerOptions in {path}")))?;
        for field in ["rootDirs", "moduleSuffixes"] {
            if options.contains_key(field) {
                result.unsupported = true;
                diagnostics.push(diagnostic(
                    &path,
                    None,
                    "typescript_config_unsupported",
                    format!("{field} resolution is not implemented"),
                ));
            }
        }
        if let Some(base) = options.get("baseUrl") {
            let base = base
                .as_str()
                .ok_or_else(|| invalid(format!("Invalid baseUrl in {path}")))?;
            result.base = Some(join(&parent(&path), &base.replace('\\', "/")));
        }
        if let Some(mappings) = options.get("paths") {
            let mappings = mappings
                .as_object()
                .ok_or_else(|| invalid(format!("Invalid paths in {path}")))?;
            result.origin = parent(&path);
            result.mappings.clear();
            for (pattern, targets) in mappings {
                if pattern.matches('*').count() > 1 {
                    return Err(invalid("TypeScript paths pattern has multiple wildcards"));
                }
                let targets = targets
                    .as_array()
                    .ok_or_else(|| invalid("TypeScript paths targets must be arrays"))?;
                if targets.is_empty() {
                    return Err(invalid("TypeScript paths target array must not be empty"));
                }
                let targets: Result<Vec<_>> = targets
                    .iter()
                    .map(|v| {
                        let value = v
                            .as_str()
                            .ok_or_else(|| invalid("TypeScript paths targets must be strings"))?;
                        if value.matches('*').count() > 1
                            || (!pattern.contains('*') && value.contains('*'))
                        {
                            return Err(invalid("TypeScript paths target has multiple wildcards"));
                        }
                        Ok(value.to_string())
                    })
                    .collect();
                result.mappings.insert(pattern.clone(), targets?);
            }
        }
    }
    Ok(result)
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    AstroError::new(ErrorCode::InvalidRequest, message).into()
}

fn jsonc(text: &str) -> Result<Value> {
    let mut bytes = text.as_bytes().to_vec();
    let mut index = 0;
    let mut quoted = false;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            quoted = !quoted;
            index += 1;
        } else if quoted && bytes[index] == b'\\' {
            index += 2;
        } else if !quoted && bytes.get(index..index + 2) == Some(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                bytes[index] = b' ';
                index += 1;
            }
        } else if !quoted && bytes.get(index..index + 2) == Some(b"/*") {
            bytes[index..index + 2].fill(b' ');
            index += 2;
            while index + 1 < bytes.len() && bytes.get(index..index + 2) != Some(b"*/") {
                bytes[index] = b' ';
                index += 1;
            }
            if index + 1 >= bytes.len() {
                anyhow::bail!("Unterminated JSON comment");
            }
            bytes[index..index + 2].fill(b' ');
            index += 2;
        } else {
            index += 1;
        }
    }
    index = 0;
    quoted = false;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            quoted = !quoted;
        } else if quoted && bytes[index] == b'\\' {
            index += 1;
        } else if !quoted && bytes[index] == b',' {
            let next = bytes[index + 1..].iter().find(|c| !c.is_ascii_whitespace());
            if matches!(next, Some(b'}' | b']')) {
                bytes[index] = b' ';
            }
        }
        index += 1;
    }
    Ok(serde_json::from_slice(&bytes)?)
}
