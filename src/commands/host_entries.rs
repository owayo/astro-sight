use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

use crate::engine::{
    bounded_read, parser,
    tree_walk::{Children, Walk, walk_preorder},
};
use crate::error::{AstroError, ErrorCode};
use crate::language::LangId;
use crate::models::reference_evidence::{
    ExternalEntryEvidence, ExternalEntryRegistration, ExternalEntryStatus, ReferenceClassification,
};
use crate::models::review::DeadSymbol;

const REGISTRY: &str = ".astro-sight/entrypoints.json";
const METADATA_LIMIT: u64 = 1024 * 1024;
type Identity = (String, String);
type Registrations = BTreeMap<Identity, Vec<ExternalEntryRegistration>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    entries: Vec<DeclaredEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredEntry {
    file: String,
    #[serde(rename = "export")]
    name: String,
    reason: String,
}

#[derive(Default)]
pub(super) struct HostEntries {
    proven: Registrations,
    unverified: Registrations,
    reasons: BTreeMap<Identity, BTreeSet<&'static str>>,
    bindings: BTreeMap<PathBuf, Option<BTreeSet<String>>>,
    tsconfigs: BTreeMap<PathBuf, Option<(Value, usize)>>,
}

/// ホストの登録とソースの対応を確認できた候補は、参照件数による dead から分ける。
impl HostEntries {
    fn typescript(&mut self, root: &Path, config: &Path) -> Result<Option<&(Value, usize)>> {
        if !self.tsconfigs.contains_key(config) {
            let parsed = if config.try_exists()? {
                let content = read_metadata(root, config)?;
                let mut bytes = super::api_changes::jsonc_bytes(&content)
                    .ok_or_else(|| invalid("Invalid JSONC TypeScript configuration"))?;
                for (byte, original) in bytes.iter_mut().zip(content.bytes()) {
                    if *byte == b' ' && original == b'\n' {
                        *byte = b'\n';
                    }
                }
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|e| invalid(format!("Invalid tsconfig.json: {e}")))?;
                validate_tsconfig(&value)?;
                let line = properties(std::str::from_utf8(&bytes)?)
                    .into_iter()
                    .find(|(key, depth, _)| key == "compilerOptions" && *depth == 1)
                    .map(|(_, _, line)| line)
                    .unwrap_or(0);
                Some((value, line))
            } else {
                None
            };
            self.tsconfigs.insert(config.to_path_buf(), parsed);
        }
        Ok(self.tsconfigs[config].as_ref())
    }
    pub(super) fn classify(
        &self,
        dead: &mut Vec<DeadSymbol>,
        test_only: &mut Vec<DeadSymbol>,
        unverified: &mut Vec<DeadSymbol>,
    ) -> Vec<DeadSymbol> {
        let entries = self;
        let mut external = Vec::new();
        let mut uncertain = Vec::new();
        for bucket in [dead, test_only, &mut *unverified] {
            let mut kept = Vec::new();
            for mut symbol in std::mem::take(bucket) {
                let key = (symbol.file.clone(), symbol.name.clone());
                let (registrations, status, classification) =
                    if let Some(proofs) = entries.proven.get(&key) {
                        (
                            proofs,
                            ExternalEntryStatus::Proven,
                            ReferenceClassification::ExternalEntry,
                        )
                    } else if let Some(proofs) = entries.unverified.get(&key) {
                        (
                            proofs,
                            ExternalEntryStatus::Unproven,
                            ReferenceClassification::Unverified,
                        )
                    } else {
                        kept.push(symbol);
                        continue;
                    };
                if let Some(evidence) = &mut symbol.reference_evidence {
                    let mut registrations = registrations.clone();
                    registrations.sort();
                    registrations.dedup();
                    evidence.classification = classification;
                    evidence.external_entry = ExternalEntryEvidence {
                        status,
                        registrations,
                        unproven_reasons: entries
                            .reasons
                            .get(&key)
                            .filter(|_| status == ExternalEntryStatus::Unproven)
                            .map(|r| r.iter().copied().collect())
                            .unwrap_or_default(),
                    };
                }
                if status == ExternalEntryStatus::Proven {
                    external.push(symbol);
                } else {
                    uncertain.push(symbol);
                }
            }
            *bucket = kept;
        }
        unverified.extend(uncertain);
        let sort = |symbols: &mut Vec<DeadSymbol>| {
            symbols.sort_by(|a, b| {
                a.file
                    .cmp(&b.file)
                    .then(a.name.cmp(&b.name))
                    .then(a.line.cmp(&b.line))
            })
        };
        sort(&mut external);
        sort(unverified);
        external
    }

    pub(super) fn is_proven(&self, file: &str, name: &str) -> bool {
        self.proven
            .contains_key(&(file.to_owned(), name.to_owned()))
    }

    pub(super) fn is_known(&self, file: &str, name: &str) -> bool {
        let key = (file.to_owned(), name.to_owned());
        self.proven.contains_key(&key) || self.unverified.contains_key(&key)
    }

    pub(super) fn build(dir: &str, files: &[PathBuf]) -> Result<Self> {
        let root = std::fs::canonicalize(dir)?;
        let mut entries = Self::default();
        entries.load_registry(&root)?;
        let candidates: BTreeSet<_> = files
            .iter()
            .filter(|p| javascript(p))
            .flat_map(|p| {
                ["activate", "deactivate"].map(|name| (relative(&root, p), name.to_owned()))
            })
            .collect();
        let mut packages = BTreeMap::new();
        for (file, name) in candidates {
            let Some(manifest) = nearest_package(&root, &file)? else {
                continue;
            };
            if !packages.contains_key(&manifest) {
                let content = read_metadata(&root, &manifest)?;
                let value = serde_json::from_str::<Value>(&content)
                    .map_err(|e| invalid(format!("Invalid package.json: {e}")))?;
                packages.insert(manifest.clone(), (content, value));
            }
            let (content, value) = &packages[&manifest];
            if !value
                .get("engines")
                .and_then(|v| v.get("vscode"))
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty())
            {
                continue;
            }
            for field in ["main", "browser"] {
                let Some(target) = value.get(field).and_then(Value::as_str) else {
                    continue;
                };
                entries.vscode(&root, &manifest, content, field, target, (&file, &name))?;
            }
        }
        Ok(entries)
    }
}

impl HostEntries {
    fn load_registry(&mut self, root: &Path) -> Result<()> {
        let path = root.join(REGISTRY);
        if !path.try_exists()? {
            return Ok(());
        }
        let content = read_metadata(root, &path)?;
        let registry: Registry = serde_json::from_str(&content)
            .map_err(|e| invalid(format!("Invalid {REGISTRY}: {e}")))?;
        let lines: Vec<_> = properties(&content)
            .into_iter()
            .filter(|(key, depth, _)| key == "export" && *depth == 3)
            .map(|(_, _, line)| line)
            .collect();
        for (index, entry) in registry.entries.into_iter().enumerate() {
            if !identifier(&entry.name) || entry.reason.trim().is_empty() {
                return Err(invalid(
                    "External entry declarations need a named export and a nonempty reason",
                ));
            }
            let target = scoped(root, root, &entry.file)
                .ok_or_else(|| invalid("External entry path leaves the workspace or is invalid"))?;
            if !javascript(&target)
                || !regular_inside(root, &target)
                || !self.binding(&target, &entry.name)
            {
                return Err(invalid(format!(
                    "External entry does not name a unique direct function export: {}#{}",
                    entry.file, entry.name
                )));
            }
            self.proven
                .entry((relative(root, &target), entry.name.clone()))
                .or_default()
                .push(ExternalEntryRegistration {
                    kind: "astro_sight_external_entry",
                    file: REGISTRY.into(),
                    line: lines.get(index).copied().unwrap_or(0),
                    script: entry.reason,
                    target: format!("{}#{}", relative(root, &target), entry.name),
                });
        }
        Ok(())
    }

    fn binding(&mut self, path: &Path, name: &str) -> bool {
        self.bindings
            .entry(path.to_path_buf())
            .or_insert_with(|| direct_function_exports(path))
            .as_ref()
            .is_some_and(|names| names.contains(name))
    }

    fn uncertain(
        &mut self,
        key: Identity,
        registration: ExternalEntryRegistration,
        reason: &'static str,
    ) {
        self.unverified
            .entry(key.clone())
            .or_default()
            .push(registration);
        self.reasons.entry(key).or_default().insert(reason);
    }

    fn vscode(
        &mut self,
        root: &Path,
        manifest: &Path,
        content: &str,
        field: &'static str,
        target: &str,
        identity: (&str, &str),
    ) -> Result<()> {
        let (file, name) = identity;
        let directory = manifest.parent().unwrap();
        let output = scoped(root, directory, target)
            .ok_or_else(|| invalid("VS Code entry path leaves the workspace or is invalid"))?;
        let actual = root.join(file);
        let key = (file.to_owned(), name.to_owned());
        let registration = ExternalEntryRegistration {
            kind: "vscode_extension",
            file: relative(root, manifest),
            line: properties(content)
                .into_iter()
                .find(|(key, depth, _)| key == field && *depth == 1)
                .map(|(_, _, line)| line)
                .unwrap_or(0),
            script: field.into(),
            target: target.into(),
        };
        if output == actual && regular_inside(root, &actual) {
            if self.binding(&actual, name) {
                self.proven.entry(key).or_default().push(registration);
            } else {
                self.uncertain(key, registration, "host_export_binding_unverified");
            }
            return Ok(());
        }
        if !matches!(
            actual.extension().and_then(|v| v.to_str()),
            Some("ts" | "tsx" | "mts" | "cts")
        ) {
            return Ok(());
        }
        let config = directory.join("tsconfig.json");
        let mut verifiable = false;
        let mut mapped = Vec::new();
        let mut config_line = 0;
        if let Some((value, line)) = self.typescript(root, &config)? {
            config_line = *line;
            let options = value.get("compilerOptions");
            let jsx = options.and_then(|o| o.get("jsx")).and_then(Value::as_str);
            if let (Some(from), Some(to)) = (
                options
                    .and_then(|o| o.get("rootDir"))
                    .and_then(Value::as_str),
                options
                    .and_then(|o| o.get("outDir"))
                    .and_then(Value::as_str),
            ) {
                let from = scoped(root, directory, from)
                    .ok_or_else(|| invalid("TypeScript rootDir leaves the workspace"))?;
                let to = scoped(root, directory, to)
                    .ok_or_else(|| invalid("TypeScript outDir leaves the workspace"))?;
                verifiable = value.get("extends").is_none()
                    && value.get("references").is_none()
                    && jsx.is_none_or(|mode| {
                        matches!(
                            mode,
                            "preserve" | "react" | "react-native" | "react-jsx" | "react-jsxdev"
                        )
                    })
                    && !(actual.extension().is_some_and(|v| v == "tsx") && jsx.is_none())
                    && options.is_some_and(|o| {
                        ["outFile", "rootDirs", "moduleSuffixes"]
                            .iter()
                            .all(|k| o.get(k).is_none())
                            && !["noEmit", "emitDeclarationOnly"]
                                .iter()
                                .any(|k| o.get(k).and_then(Value::as_bool) == Some(true))
                    });
                if let Ok(suffix) = output.strip_prefix(&to) {
                    let extensions: &[&str] = match suffix.extension().and_then(|v| v.to_str()) {
                        Some("js") if jsx == Some("preserve") => &["ts"],
                        Some("js") => &["ts", "tsx"],
                        Some("jsx") if jsx == Some("preserve") => &["tsx"],
                        Some("mjs") => &["mts"],
                        Some("cjs") => &["cts"],
                        _ => &[],
                    };
                    for extension in extensions {
                        let source = from.join(suffix).with_extension(extension);
                        if regular_inside(root, &source)
                            && selected_input(directory, &source, &to, value)?
                        {
                            mapped.push(source);
                        }
                    }
                }
            }
        }
        if mapped.len() == 1 && mapped[0] == actual && verifiable {
            if self.binding(&actual, name) {
                let proof = ExternalEntryRegistration {
                    kind: "vscode_typescript_output",
                    file: relative(root, &config),
                    line: config_line,
                    script: "compilerOptions.rootDir/outDir".into(),
                    target: file.into(),
                };
                self.proven
                    .entry(key.clone())
                    .or_default()
                    .extend([registration, proof]);
            } else {
                self.uncertain(key, registration, "host_export_binding_unverified");
            }
        } else if mapped.contains(&actual)
            || (!verifiable && output.file_stem() == actual.file_stem())
        {
            self.uncertain(
                key,
                registration,
                if mapped.len() > 1 {
                    "typescript_output_source_ambiguous"
                } else {
                    "typescript_output_source_unverified"
                },
            );
        }
        Ok(())
    }
}

fn validate_tsconfig(config: &Value) -> Result<()> {
    if !config.is_object() {
        return Err(invalid("TypeScript configuration must contain an object"));
    }
    if let Some(options) = config.get("compilerOptions") {
        if !options.is_object() {
            return Err(invalid("compilerOptions must contain an object"));
        }
        for field in ["rootDir", "outDir", "outFile", "jsx"] {
            if options.get(field).is_some_and(|value| !value.is_string()) {
                return Err(invalid(format!("Invalid TypeScript {field}")));
            }
        }
        for field in ["noEmit", "emitDeclarationOnly", "noEmitOnError", "allowJs"] {
            if options.get(field).is_some_and(|value| !value.is_boolean()) {
                return Err(invalid(format!("Invalid TypeScript {field}")));
            }
        }
    }
    for field in ["files", "include", "exclude"] {
        if let Some(value) = config.get(field)
            && !value
                .as_array()
                .is_some_and(|values| values.iter().all(Value::is_string))
        {
            return Err(invalid(format!("Invalid TypeScript {field}")));
        }
    }
    Ok(())
}

fn selected_input(directory: &Path, file: &Path, output: &Path, config: &Value) -> Result<bool> {
    let relative = file
        .strip_prefix(directory)
        .ok()
        .map(|p| crate::git_support::normalize_workspace_separators(&p.to_string_lossy()));
    let listed = config
        .get("files")
        .and_then(Value::as_array)
        .is_some_and(|files| {
            files
                .iter()
                .filter_map(Value::as_str)
                .any(|name| directory.join(name) == file)
        });
    let matches = |value: &Value| -> Result<bool> {
        let values = value
            .as_array()
            .ok_or_else(|| invalid("TypeScript include/exclude must be arrays"))?;
        for value in values {
            let pattern = value
                .as_str()
                .ok_or_else(|| invalid("TypeScript include/exclude entries must be strings"))?;
            let pattern = pattern.strip_prefix("./").unwrap_or(pattern);
            let pattern =
                if !pattern.contains(['*', '?']) && Path::new(pattern).extension().is_none() {
                    format!("{}/**/*", pattern.trim_end_matches('/'))
                } else {
                    pattern.to_owned()
                };
            let glob = globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .map_err(|e| invalid(format!("Invalid TypeScript input pattern: {e}")))?
                .compile_matcher();
            if relative
                .as_ref()
                .is_some_and(|relative| glob.is_match(relative))
            {
                return Ok(true);
            }
        }
        Ok(false)
    };
    if listed {
        return Ok(true);
    }
    if config.get("exclude").is_none()
        && (file.starts_with(output)
            || file.strip_prefix(directory).is_ok_and(|path| {
                path.components().any(|part| {
                    matches!(part, Component::Normal(name) if matches!(name.to_str(), Some("node_modules" | "bower_components" | "jspm_packages")))
                })
            }))
    {
        return Ok(false);
    }
    let included = if let Some(value) = config.get("include") {
        matches(value)?
    } else {
        config.get("files").is_none() && relative.is_some()
    };
    Ok(included
        && !config
            .get("exclude")
            .map(matches)
            .transpose()?
            .unwrap_or(false))
}

fn direct_function_exports(path: &Path) -> Option<BTreeSet<String>> {
    let file_name = path.file_name()?.to_str()?;
    if [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|suffix| file_name.ends_with(suffix))
    {
        return None;
    }
    let path = camino::Utf8Path::from_path(path)?;
    let source = parser::read_file(path).ok()?;
    let lang = parser::detect_lang(path, &source).ok()?;
    let tree = parser::parse_source(&source, lang).ok()?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    let mut names = BTreeMap::new();
    let mut exported = BTreeSet::new();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        let is_export = statement.kind() == "export_statement";
        let mut children = statement.walk();
        let export = is_export
            && !statement
                .children(&mut children)
                .any(|child| child.kind() == "default");
        let node = if is_export {
            statement
                .child_by_field_name("declaration")
                .unwrap_or(statement)
        } else {
            statement
        };
        if matches!(
            node.kind(),
            "function_declaration" | "generator_function_declaration"
        ) {
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| n.utf8_text(&source).ok())
            {
                *names.entry(name.to_owned()).or_insert(0) += 1;
                if export && node.kind() == "function_declaration" {
                    exported.insert(name.to_owned());
                }
            }
        } else if matches!(node.kind(), "lexical_declaration" | "variable_declaration") {
            let mut cursor = node.walk();
            for declaration in node.named_children(&mut cursor) {
                if let Some(pattern) = declaration.child_by_field_name("name") {
                    let _: std::ops::ControlFlow<()> =
                        walk_preorder(pattern, Children::Named, |part, _| {
                            if matches!(
                                part.kind(),
                                "identifier" | "shorthand_property_identifier_pattern"
                            ) && let Ok(name) = part.utf8_text(&source)
                            {
                                *names.entry(name.to_owned()).or_insert(0) += 1;
                            }
                            std::ops::ControlFlow::Continue(Walk::Descend)
                        });
                    if pattern.kind() == "identifier"
                        && export
                        && declaration.child_by_field_name("value").is_some_and(|v| {
                            matches!(v.kind(), "arrow_function" | "function_expression")
                        })
                        && let Ok(name) = pattern.utf8_text(&source)
                    {
                        exported.insert(name.to_owned());
                    }
                }
            }
        } else if node.kind() == "import_statement" {
            let _: std::ops::ControlFlow<()> = walk_preorder(node, Children::Named, |part, _| {
                if part.kind() == "import_specifier" {
                    if let Some(name) = part
                        .child_by_field_name("alias")
                        .or_else(|| part.child_by_field_name("name"))
                        .and_then(|n| n.utf8_text(&source).ok())
                    {
                        *names.entry(name.to_owned()).or_insert(0) += 1;
                    }
                    return std::ops::ControlFlow::Continue(Walk::Skip);
                }
                if part.kind() == "identifier"
                    && let Ok(name) = part.utf8_text(&source)
                {
                    *names.entry(name.to_owned()).or_insert(0) += 1;
                }
                std::ops::ControlFlow::Continue(Walk::Descend)
            });
        } else if let Some(name) = node
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(&source).ok())
        {
            *names.entry(name.to_owned()).or_insert(0) += 1;
        }
    }
    let mut writes = Vec::new();
    let _: std::ops::ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
        if matches!(
            node.kind(),
            "assignment_expression" | "augmented_assignment_expression" | "update_expression"
        ) {
            let left = node
                .child_by_field_name("left")
                .or_else(|| node.child_by_field_name("argument"));
            if let Some(left) = left {
                writes.push(left.byte_range());
            }
        }
        std::ops::ControlFlow::Continue(Walk::Descend)
    });
    writes.sort_by_key(|span| (span.start, span.end));
    let mut merged: Vec<std::ops::Range<usize>> = Vec::new();
    for span in writes {
        if let Some(previous) = merged.last_mut()
            && span.start <= previous.end
        {
            previous.end = previous.end.max(span.end);
        } else {
            merged.push(span);
        }
    }
    let _: std::ops::ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
        if matches!(
            node.kind(),
            "identifier" | "shorthand_property_identifier_pattern"
        ) {
            let index = merged.partition_point(|span| span.end <= node.start_byte());
            if merged
                .get(index)
                .is_some_and(|span| span.start <= node.start_byte() && node.end_byte() <= span.end)
                && let Ok(name) = node.utf8_text(&source)
            {
                exported.remove(name);
            }
        }
        std::ops::ControlFlow::Continue(Walk::Descend)
    });
    exported.retain(|name| names.get(name) == Some(&1));
    Some(exported)
}

fn nearest_package(root: &Path, file: &str) -> Result<Option<PathBuf>> {
    let mut directory = root.join(file).parent().unwrap_or(root).to_path_buf();
    loop {
        let path = directory.join("package.json");
        if path.try_exists()? {
            return Ok(Some(path));
        }
        if directory == root || !directory.pop() {
            return Ok(None);
        }
    }
}

fn read_metadata(root: &Path, path: &Path) -> Result<String> {
    if !regular_inside(root, path) {
        return Err(invalid(
            "Host entry metadata is missing, symlinked, or outside the workspace",
        ));
    }
    bounded_read::read_utf8_file_limited(path, METADATA_LIMIT)
        .ok_or_else(|| invalid("Host entry metadata is unreadable, invalid UTF-8, or over 1 MiB"))
}

fn regular_inside(root: &Path, path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|m| m.file_type().is_file() && !m.file_type().is_symlink())
        && std::fs::canonicalize(path).is_ok_and(|p| p.starts_with(root))
}

fn scoped(root: &Path, base: &Path, value: &str) -> Option<PathBuf> {
    if value.is_empty() || value.contains(['\\', ':', '\0']) || Path::new(value).is_absolute() {
        return None;
    }
    let mut relative = base.strip_prefix(root).ok()?.to_path_buf();
    for component in Path::new(value).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir if relative.pop() => {}
            _ => return None,
        }
    }
    Some(root.join(relative))
}

fn relative(root: &Path, path: &Path) -> String {
    crate::git_support::normalize_workspace_separators(
        &path.strip_prefix(root).unwrap_or(path).to_string_lossy(),
    )
}
fn javascript(path: &Path) -> bool {
    camino::Utf8Path::from_path(path)
        .and_then(|p| LangId::from_path(p).ok())
        .is_some_and(|l| matches!(l, LangId::Javascript | LangId::Typescript | LangId::Tsx))
}
fn identifier(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().enumerate().all(|(i, b)| {
            b == b'_' || b == b'$' || b.is_ascii_alphabetic() || (i > 0 && b.is_ascii_digit())
        })
}
fn invalid(message: impl Into<String>) -> anyhow::Error {
    AstroError::new(ErrorCode::InvalidRequest, message).into()
}

pub(super) fn attach_api_proofs(result: &mut crate::models::review::ReviewResult) {
    let proofs: BTreeMap<_, _> = result
        .external_entry_symbols
        .iter()
        .chain(result.unverified_symbols.iter())
        .filter_map(|symbol| {
            symbol
                .reference_evidence
                .as_ref()
                .filter(|e| !e.external_entry.registrations.is_empty())
                .map(|e| {
                    (
                        (symbol.file.clone(), symbol.name.clone()),
                        e.external_entry.clone(),
                    )
                })
        })
        .collect();
    for change in result
        .api_changes
        .modified
        .iter_mut()
        .chain(result.api_changes.modified_closed_in_diff.iter_mut())
    {
        if let Some(evidence) = &mut change.reference_evidence
            && let Some(proof) = proofs.get(&(change.file.clone(), change.name.clone()))
        {
            evidence.external_entry = proof.clone();
        }
    }
}

fn properties(source: &str) -> Vec<(String, usize, usize)> {
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut depth: usize = 0;
    let mut line = 0;
    let mut result = Vec::new();
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            b'"' => {
                let start = index;
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        index += 2;
                    } else if bytes[index] == b'"' {
                        index += 1;
                        break;
                    } else {
                        index += 1;
                    }
                }
                if bytes
                    .get(index..)
                    .and_then(|s| s.iter().find(|b| !b.is_ascii_whitespace()))
                    == Some(&b':')
                    && let Ok(key) = serde_json::from_slice::<String>(&bytes[start..index])
                {
                    result.push((key, depth, line));
                }
            }
            b'\n' => {
                line += 1;
                index += 1;
            }
            _ => index += 1,
        }
    }
    result
}
