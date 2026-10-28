//! 参照と同じソース選択に、限定した JSON・埋め込みソースの残存検査を加える。

use std::cell::{Cell, OnceCell, RefCell};
use std::path::Path;

use super::*;

pub(super) const MAX_SOURCE: usize = 2 * 1024 * 1024;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_FILES: usize = 10_000;
const MAX_PARSES: usize = 512;
const MAX_NODES: usize = 100_000;

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Source(LangId),
    Json,
    Embedded,
}
pub(super) struct File {
    pub(super) source: String,
    pub(super) kind: Kind,
    tree: OnceCell<Option<tree_sitter::Tree>>,
    json: OnceCell<Option<JsonSummary>>,
}
pub(super) struct Inventory {
    pub(super) files: BTreeMap<String, File>,
    pub(super) dir: std::path::PathBuf,
    logical_dir: std::path::PathBuf,
    config_audit: RefCell<ConfigAudit>,
    parses: Cell<usize>,
    nodes: Cell<usize>,
}

impl Inventory {
    pub(super) fn load(dir: &str) -> Option<Self> {
        let logical = std::path::absolute(Path::new(dir)).ok()?;
        let metadata = std::fs::symlink_metadata(&logical).ok()?;
        if metadata.file_type().is_symlink() || reparse_point(&metadata) {
            return None;
        }
        let root = logical.canonicalize().ok()?;
        let config_audit = audit_ancestor_configs(&logical, &root)?;
        // 共有 selector を呼ぶ前に entry 数も制限し、無限に候補を蓄積しない。
        let mut walked = Vec::new();
        for entry in ignore::WalkBuilder::new(&root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .build()
        {
            if walked.len() >= 100_000 {
                return None;
            }
            walked.push(entry.ok()?);
        }
        let sources = crate::engine::refs::collect_files_scan(
            &root,
            None,
            crate::engine::refs::FileScanOptions::default(),
        )
        .ok()?;
        let selected = sources
            .files
            .into_iter()
            .filter_map(|path| {
                crate::engine::refs::detect_source_lang(&path)
                    .filter(|lang| {
                        matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx)
                    })
                    .map(|lang| (path, lang))
            })
            .collect::<BTreeMap<_, _>>();
        let attributes_path = root.join(".gitattributes");
        let attributes = if attributes_path.exists() {
            let metadata = std::fs::symlink_metadata(&attributes_path).ok()?;
            if !metadata.is_file() || metadata.file_type().is_symlink() || reparse_point(&metadata)
            {
                return None;
            }
            crate::engine::gitattributes::GitAttributes::parse(
                &crate::engine::bounded_read::read_utf8_file_limited(
                    &attributes_path,
                    1024 * 1024,
                )?,
            )
        } else {
            crate::engine::gitattributes::GitAttributes::default()
        };
        let mut files = BTreeMap::new();
        let mut bytes = 0usize;
        let include_generated = std::env::var("ASTRO_SIGHT_NO_GENERATED_EXCLUSION")
            .ok()
            .is_some_and(|value| !value.is_empty() && value != "0");
        for entry in walked {
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(path).ok()?;
            let extension = path
                .extension()
                .and_then(|extension| extension.to_str())
                .unwrap_or_default();
            let relevant = matches!(
                extension,
                "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" | "json"
            ) || crate::engine::refs::is_unanalyzable_source_ext(extension);
            if (metadata.file_type().is_symlink() || reparse_point(&metadata))
                && (relevant || path.is_dir() || extension.is_empty())
            {
                return None;
            }
            if !metadata.is_file() {
                continue;
            }
            // 拡張子なしの候補は selector が shebang を読めなかった場合も黙って外さない。
            if path.extension().is_none() {
                use std::io::Read;
                let mut head = [0; 4096];
                let _read = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
            }
            let kind = if let Some(lang) = selected.get(path) {
                Kind::Source(*lang)
            } else if extension == "json" {
                if !include_generated && crate::engine::generated::is_auto_generated(path) {
                    continue;
                }
                Kind::Json
            } else if crate::engine::refs::is_unanalyzable_source_ext(extension) {
                if !include_generated && crate::engine::generated::is_auto_generated(path) {
                    continue;
                }
                Kind::Embedded
            } else {
                continue;
            };
            let relative = crate::git_support::normalize_workspace_separators(
                path.strip_prefix(&root).ok()?.to_str()?,
            );
            if !include_generated && attributes.is_generated(&relative) {
                continue;
            }
            let source =
                crate::engine::bounded_read::read_utf8_file_limited(path, MAX_SOURCE as u64)?;
            bytes = bytes.checked_add(source.len())?;
            if bytes > MAX_BYTES || files.len() >= MAX_FILES {
                return None;
            }
            let basename = path.file_name()?.to_str()?;
            if (basename.starts_with("tsconfig") || basename.starts_with("jsconfig"))
                && basename.ends_with(".json")
            {
                plain_config(&source)?;
            }
            files.insert(
                relative,
                File {
                    source,
                    kind,
                    tree: OnceCell::new(),
                    json: OnceCell::new(),
                },
            );
        }
        Some(Self {
            files,
            dir: root,
            logical_dir: logical,
            config_audit: RefCell::new(config_audit),
            parses: Cell::new(0),
            nodes: Cell::new(0),
        })
    }

    pub(super) fn caller_resolution_is_plain(&self, from: &str) -> Option<()> {
        let relative = Path::new(from);
        if relative.components().any(|component| {
            !matches!(
                component,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        }) {
            return None;
        }
        self.files.get(from)?;
        let logical = self.logical_dir.join(relative);
        let physical = self.dir.join(relative);
        let mut audit = self.config_audit.borrow_mut();
        audit.ancestors(logical.parent()?)?;
        audit.ancestors(physical.parent()?)
    }

    pub(super) fn directory_index_is_plain(&self, target: &str) -> Option<()> {
        let parent = Path::new(target).parent()?;
        let mut audit = self.config_audit.borrow_mut();
        audit.path_absent(&self.logical_dir.join(parent).join("package.json"))?;
        audit.path_absent(&self.dir.join(parent).join("package.json"))
    }

    pub(super) fn alternative_inputs_are_absent(&self, alternatives: &[String]) -> Option<()> {
        let mut audit = self.config_audit.borrow_mut();
        for alternative in alternatives {
            let path = Path::new(alternative);
            if path.components().any(|component| {
                !matches!(
                    component,
                    std::path::Component::Normal(_) | std::path::Component::CurDir
                )
            }) {
                return None;
            }
            audit.path_absent(&self.logical_dir.join(path))?;
            audit.path_absent(&self.dir.join(path))?;
        }
        Some(())
    }

    pub(super) fn root(&self, path: &str) -> Option<Node<'_>> {
        let file = self.files.get(path)?;
        let Kind::Source(lang) = file.kind else {
            return None;
        };
        file.tree
            .get_or_init(|| {
                self.parses.set(self.parses.get().checked_add(1)?);
                if self.parses.get() > MAX_PARSES {
                    return None;
                }
                let tree =
                    crate::engine::parser::parse_source(file.source.as_bytes(), lang).ok()?;
                if tree.root_node().has_error() {
                    return None;
                }
                let result = crate::engine::tree_walk::walk_preorder(
                    tree.root_node(),
                    Children::All,
                    |node, depth| {
                        self.nodes.set(self.nodes.get().saturating_add(1));
                        if self.nodes.get() > MAX_NODES
                            || depth > MAX_DEPTH
                            || node.is_error()
                            || node.is_missing()
                        {
                            std::ops::ControlFlow::Break(())
                        } else {
                            std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
                        }
                    },
                );
                result.is_continue().then_some(tree)
            })
            .as_ref()
            .map(tree_sitter::Tree::root_node)
    }

    pub(super) fn json_root(&self, path: &str) -> Option<&JsonSummary> {
        let file = self.files.get(path)?;
        if !matches!(file.kind, Kind::Json) {
            return None;
        }
        file.json
            .get_or_init(|| {
                self.parses.set(self.parses.get().checked_add(1)?);
                if self.parses.get() > MAX_PARSES {
                    return None;
                }
                let value = json(&file.source)?;
                self.nodes.set(self.nodes.get().checked_add(value.nodes)?);
                (self.nodes.get() <= MAX_NODES).then_some(value)
            })
            .as_ref()
    }

    pub(super) fn source(&self, path: &str) -> Option<&str> {
        Some(&self.files.get(path)?.source)
    }
    pub(super) fn relevant<'a>(
        &'a self,
        names: &'a BTreeSet<String>,
    ) -> impl Iterator<Item = (&'a str, &'a File)> {
        self.files
            .iter()
            .filter(|(_, file)| {
                matches!(file.kind, Kind::Source(_))
                    && (file.source.contains('\\')
                        || names.iter().any(|name| {
                            super::super::ref_index::contains_js_word(&file.source, name)
                        }))
            })
            .map(|(path, file)| (path.as_str(), file))
    }
}

#[cfg(windows)]
fn reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}
#[cfg(not(windows))]
fn reparse_point(_: &std::fs::Metadata) -> bool {
    false
}

fn plain_config(source: &str) -> Option<()> {
    let value = json(source)?;
    if value.nodes > 10_000
        || value.keys.iter().any(|key| {
            matches!(
                key.as_str(),
                "extends" | "references" | "rootDirs" | "moduleSuffixes" | "paths" | "baseUrl"
            )
        })
    {
        return None;
    }
    Some(())
}
#[derive(Default)]
struct ConfigAudit {
    visited: BTreeSet<std::path::PathBuf>,
    path_results: BTreeMap<std::path::PathBuf, bool>,
    entries: usize,
    bytes: usize,
    failed: bool,
}
impl ConfigAudit {
    fn path_absent(&mut self, path: &Path) -> Option<()> {
        if self.failed {
            return None;
        }
        if let Some(absent) = self.path_results.get(path) {
            return absent.then_some(());
        }
        self.entries = self.entries.checked_add(1)?;
        if self.entries > 100_000 {
            self.failed = true;
            return None;
        }
        let absent = match std::fs::symlink_metadata(path) {
            Ok(_) => false,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                true
            }
            Err(_) => {
                self.failed = true;
                return None;
            }
        };
        // 代替入力の存在はこの解決だけを不明にし、別の明示 file 解決を汚さない。
        self.path_results.insert(path.to_path_buf(), absent);
        absent.then_some(())
    }
    /// caller の設定は ignore・generated の選択と独立に読み、directory ごとに一度だけ監査する。
    fn ancestors(&mut self, root: &Path) -> Option<()> {
        if self.failed {
            return None;
        }
        let result = (|| {
            for (depth, ancestor) in root.ancestors().enumerate() {
                if depth > MAX_DEPTH {
                    return None;
                }
                if self.visited.contains(ancestor) {
                    continue;
                }
                for entry in std::fs::read_dir(ancestor).ok()? {
                    self.entries = self.entries.checked_add(1)?;
                    if self.entries > 100_000 {
                        return None;
                    }
                    let entry = entry.ok()?;
                    let name = entry.file_name();
                    let name = name.to_str()?;
                    if !(name.starts_with("tsconfig") || name.starts_with("jsconfig"))
                        || !name.ends_with(".json")
                    {
                        continue;
                    }
                    let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
                    if !metadata.is_file()
                        || metadata.file_type().is_symlink()
                        || reparse_point(&metadata)
                    {
                        return None;
                    }
                    let source = crate::engine::bounded_read::read_utf8_file_limited(
                        &entry.path(),
                        1024 * 1024,
                    )?;
                    self.bytes = self.bytes.checked_add(source.len())?;
                    if self.bytes > 8 * 1024 * 1024 {
                        return None;
                    }
                    plain_config(&source)?;
                }
                self.visited.insert(ancestor.to_path_buf());
            }
            Some(())
        })();
        if result.is_none() {
            self.failed = true;
        }
        result
    }
}
fn audit_ancestor_configs(logical: &Path, physical: &Path) -> Option<ConfigAudit> {
    let mut audit = ConfigAudit::default();
    audit.ancestors(logical)?;
    audit.ancestors(physical)?;
    Some(audit)
}

/// JSONC のコメントと末尾 comma だけを空白へ置き換え、文字列と escape は JSON に解釈させる。
pub(super) struct JsonSummary {
    pub(super) strings: Vec<String>,
    keys: BTreeSet<String>,
    nodes: usize,
}
fn json_bytes(source: &str) -> Option<Vec<u8>> {
    let mut bytes = source.as_bytes().to_vec();
    let mut index = 0;
    let mut quoted = false;
    while index < bytes.len() {
        if quoted {
            if bytes[index] == b'\\' {
                index += 2;
                continue;
            }
            if bytes[index] == b'"' {
                quoted = false;
            }
            index += 1;
            continue;
        }
        if bytes[index] == b'"' {
            quoted = true;
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && !matches!(bytes[index], b'\n' | b'\r') {
                bytes[index] = b' ';
                index += 1;
            }
        } else if bytes[index..].starts_with(b"/*") {
            bytes[index] = b' ';
            bytes[index + 1] = b' ';
            index += 2;
            loop {
                if index + 1 >= bytes.len() {
                    return None;
                }
                if bytes[index..].starts_with(b"*/") {
                    bytes[index] = b' ';
                    bytes[index + 1] = b' ';
                    index += 2;
                    break;
                }
                bytes[index] = b' ';
                index += 1;
            }
        } else {
            index += 1;
        }
    }
    let mut quoted = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if quoted => {
                index += 2;
                continue;
            }
            b'"' => quoted = !quoted,
            b',' if !quoted => {
                let next = bytes[index + 1..]
                    .iter()
                    .copied()
                    .find(|byte| !byte.is_ascii_whitespace());
                if matches!(next, Some(b'}' | b']')) {
                    bytes[index] = b' ';
                }
            }
            _ => {}
        }
        index += 1;
    }
    Some(bytes)
}

/// Value の map 化で重複 key 配下の値を落とさず、全 string/key を残す。
fn json(source: &str) -> Option<JsonSummary> {
    let bytes = json_bytes(source)?;
    serde_json::from_slice::<serde::de::IgnoredAny>(&bytes).ok()?;
    let mut strings = Vec::new();
    let mut keys = BTreeSet::new();
    let mut nodes = 0usize;
    let mut depth = 0usize;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                nodes += 1;
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                index += 1;
            }
            b',' | b':' | b' ' | b'\r' | b'\n' | b'\t' => {
                index += 1;
            }
            b'"' => {
                let start = index;
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'\\' => index += 2,
                        b'"' => {
                            index += 1;
                            break;
                        }
                        _ => index += 1,
                    }
                }
                let value = serde_json::from_slice::<String>(bytes.get(start..index)?).ok()?;
                if bytes[index..]
                    .iter()
                    .copied()
                    .find(|byte| !byte.is_ascii_whitespace())
                    == Some(b':')
                {
                    keys.insert(value.clone());
                }
                strings.push(value);
                nodes += 1;
            }
            _ => {
                while index < bytes.len()
                    && !bytes[index].is_ascii_whitespace()
                    && !matches!(bytes[index], b',' | b'}' | b']')
                {
                    index += 1;
                }
                nodes += 1;
            }
        }
        if nodes > MAX_NODES || depth > MAX_DEPTH {
            return None;
        }
    }
    Some(JsonSummary {
        strings,
        keys,
        nodes,
    })
}

#[cfg(test)]
pub(super) fn fixture(files: &[(&str, &str)]) -> (tempfile::TempDir, Inventory) {
    let directory = tempfile::tempdir().unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(directory.path())
            .status()
            .unwrap()
            .success()
    );
    for (path, source) in files {
        let file = directory.path().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source).unwrap();
    }
    let inventory =
        Inventory::load(directory.path().to_str().unwrap()).expect("bounded fixture inventory");
    (directory, inventory)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_union_inventory_matches_refs_hidden_ignore_and_generated_scope() {
        let (_directory, inventory) = fixture(&[
            (".gitignore", "ignored.ts\nignored.json\n"),
            (".gitattributes", "generated.json linguist-generated\n"),
            ("src/entry.ts", "export type Mode = 'all' | 'recent';"),
            ("bin/check", "#!/usr/bin/env node\nconst value = 'all';"),
            (".hidden.ts", "export const hidden = 'legacy';"),
            (".hidden.json", "{\"value\":\"legacy\"}"),
            ("ignored.ts", "export const ignored = 'legacy';"),
            ("ignored.json", "{\"value\":\"legacy\"}"),
            ("generated.json", "{\"value\":\"legacy\"}"),
            ("visible.json", "{\"value\":\"all\"}"),
        ]);
        assert!(inventory.files.contains_key("src/entry.ts"));
        assert!(matches!(
            inventory.files.get("bin/check").map(|file| file.kind),
            Some(Kind::Source(LangId::Javascript))
        ));
        assert!(inventory.files.contains_key("visible.json"));
        let selected = crate::engine::refs::collect_files_scan(
            &inventory.dir,
            None,
            crate::engine::refs::FileScanOptions::default(),
        )
        .unwrap()
        .files
        .into_iter()
        .filter(|path| {
            matches!(
                crate::engine::refs::detect_source_lang(path),
                Some(LangId::Javascript | LangId::Typescript | LangId::Tsx)
            )
        })
        .map(|path| {
            crate::git_support::normalize_workspace_separators(
                path.strip_prefix(&inventory.dir).unwrap().to_str().unwrap(),
            )
        })
        .collect::<BTreeSet<_>>();
        let inventoried = inventory
            .files
            .iter()
            .filter(|(_, file)| matches!(file.kind, Kind::Source(_)))
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(inventoried, selected);
        for path in [
            ".hidden.ts",
            ".hidden.json",
            "ignored.ts",
            "ignored.json",
            "generated.json",
        ] {
            assert!(!inventory.files.contains_key(path), "{path}");
        }
    }
    #[test]
    fn literal_union_config_checks_cooked_keys_jsonc_and_failures() {
        for source in [
            r#"{"compilerOptions":{"rootDirs":["src"]}}"#,
            r#"{"compilerOptions":{"ro\u006ftDirs":["src"]}}"#,
            "{ // ordinary comment\n \"extends\": \"./other\", }",
            "{broken}",
            r#"{"compilerOptions":{"paths":{}},"compilerOptions":{"strict":true}}"#,
        ] {
            assert!(plain_config(source).is_none(), "{source}");
        }
        assert!(
            plain_config("{ /* comment */ \"compilerOptions\": { \"strict\": true, }, }").is_some()
        );
        assert!(json("{\"text\":\"//quoted\\n\",}").is_some());
    }
    #[test]
    fn literal_union_inventory_retains_parse_and_node_boundaries() {
        let (_directory, inventory) = fixture(&[("src/broken.ts", "export const x = ;")]);
        assert!(inventory.root("src/broken.ts").is_none());
        assert!(inventory.root("src/broken.ts").is_none());
        assert_eq!(inventory.parses.get(), 1);
        let (_directory, inventory) = fixture(&[("src/ok.ts", "export const x = 1;")]);
        inventory.nodes.set(MAX_NODES);
        assert!(inventory.root("src/ok.ts").is_none());
        let (_directory, inventory) = fixture(&[("src/ok.ts", "export const x = 1;")]);
        inventory.parses.set(MAX_PARSES);
        assert!(inventory.root("src/ok.ts").is_none());
        let (_directory, inventory) = fixture(&[("data.json", "{\"value\":\"all\"}")]);
        assert!(inventory.json_root("data.json").is_some());
        assert!(inventory.json_root("data.json").is_some());
        assert_eq!(inventory.parses.get(), 1);
        let (_directory, inventory) = fixture(&[("data.json", "{\"value\":\"all\"}")]);
        inventory.nodes.set(MAX_NODES);
        assert!(inventory.json_root("data.json").is_none());
        let (_directory, inventory) = fixture(&[("data.json", "{\"value\":\"all\"}")]);
        inventory.parses.set(MAX_PARSES);
        assert!(inventory.json_root("data.json").is_none());
    }
    #[test]
    fn literal_union_ancestor_config_depth_limit_is_not_a_partial_success() {
        let directory = tempfile::tempdir().unwrap();
        let mut path = directory.path().to_path_buf();
        for _ in 0..=MAX_DEPTH {
            path.push("d");
        }
        std::fs::create_dir_all(&path).unwrap();
        assert!(audit_ancestor_configs(&path, &path).is_none());
    }

    #[test]
    fn literal_union_inventory_normalizes_relative_logical_roots() {
        let directory = tempfile::Builder::new()
            .prefix(".literal-union-test-")
            .tempdir_in(".")
            .unwrap();
        std::fs::write(directory.path().join("mode.ts"), "export type Mode='all';").unwrap();
        let logical = std::path::absolute(directory.path()).unwrap();
        let root = logical.canonicalize().unwrap();
        assert!(audit_ancestor_configs(&logical, &root).is_some());
        let inventory = Inventory::load(directory.path().to_str().unwrap()).unwrap();
        assert!(inventory.files.contains_key("mode.ts"));
    }
    #[cfg(unix)]
    #[test]
    fn literal_union_inventory_rejects_root_links_before_canonicalizing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("real");
        std::fs::create_dir(&root).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(root, &alias).unwrap();
        assert!(Inventory::load(alias.to_str().unwrap()).is_none());
    }
    #[test]
    fn literal_union_inventory_audits_ignored_caller_configs_before_relative_resolution() {
        for config in [
            r#"{"compilerOptions":{"moduleSuffixes":[".native",""]}}"#,
            r#"{"compilerOptions":{"moduleSuff\u0069xes":[".native",""]}}"#,
            "{broken}",
        ] {
            let (_directory, inventory) = fixture(&[
                (".gitignore", "src/tsconfig.json\n"),
                ("src/tsconfig.json", config),
                ("src/view.ts", "import { value } from './mode'; value();"),
                (
                    "src/mode.ts",
                    "export function value(): string {return 'all';}",
                ),
                (
                    "src/mode.native.ts",
                    "export function value(): string {return 'recent';}",
                ),
            ]);
            assert!(!inventory.files.contains_key("src/tsconfig.json"));
            assert!(slots::module(&inventory, "src/view.ts", "./mode").is_none());
            let entries = inventory.config_audit.borrow().entries;
            assert!(slots::module(&inventory, "src/view.ts", "./mode").is_none());
            assert_eq!(inventory.config_audit.borrow().entries, entries);
        }
        let (_directory, inventory) = fixture(&[
            (".gitignore", "src/tsconfig.json\n"),
            (
                "src/tsconfig.json",
                r#"{"compilerOptions":{"strict":true}}"#,
            ),
            ("src/view.ts", "import { value } from './mode'; value();"),
            (
                "src/mode.ts",
                "export function value(): string {return 'all';}",
            ),
        ]);
        assert_eq!(
            slots::module(&inventory, "src/view.ts", "./mode").as_deref(),
            Some("src/mode.ts")
        );
        let entries = inventory.config_audit.borrow().entries;
        assert_eq!(
            slots::module(&inventory, "src/view.ts", "./mode").as_deref(),
            Some("src/mode.ts")
        );
        let audit = inventory.config_audit.borrow();
        assert_eq!(audit.entries, entries);
        assert!(audit.visited.contains(&inventory.logical_dir.join("src")));
        assert!(audit.visited.contains(&inventory.dir.join("src")));
    }
    #[test]
    fn literal_union_inventory_preserves_shared_config_resource_failures() {
        let directory = tempfile::tempdir().unwrap();
        let mut audit = ConfigAudit {
            entries: 100_000,
            ..ConfigAudit::default()
        };
        std::fs::write(directory.path().join("tsconfig.json"), "{}").unwrap();
        assert!(audit.ancestors(directory.path()).is_none());
        assert!(audit.failed);
        let mut audit = ConfigAudit {
            bytes: 8 * 1024 * 1024,
            ..ConfigAudit::default()
        };
        assert!(audit.ancestors(directory.path()).is_none());
        assert!(audit.failed);
    }
    #[test]
    fn literal_union_inventory_does_not_assume_directory_index_with_hidden_package_metadata() {
        for directory_first in [false, true] {
            let (_directory, inventory) = fixture(&[
                (".gitignore", "src/feature/package.json\n"),
                ("src/feature/package.json", r#"{"types":"./native.ts"}"#),
                ("src/feature/index.ts", "export const value='other';"),
                ("src/feature/native.ts", "export const value='all';"),
                ("src/view.ts", "import {value} from './feature';"),
            ]);
            assert!(!inventory.files.contains_key("src/feature/package.json"));
            if directory_first {
                assert!(slots::module(&inventory, "src/view.ts", "./feature").is_none());
            }
            assert_eq!(
                slots::module(&inventory, "src/view.ts", "./feature/index").as_deref(),
                Some("src/feature/index.ts")
            );
            assert!(slots::module(&inventory, "src/view.ts", "./feature").is_none());
            let entries = inventory.config_audit.borrow().entries;
            assert!(slots::module(&inventory, "src/view.ts", "./feature").is_none());
            assert_eq!(inventory.config_audit.borrow().entries, entries);
            assert!(!inventory.config_audit.borrow().failed);
            assert_eq!(
                slots::module(&inventory, "src/view.ts", "./feature/index").as_deref(),
                Some("src/feature/index.ts")
            );
        }
        let (_directory, inventory) = fixture(&[
            ("src/feature/index.ts", "export const value='all';"),
            ("src/view.ts", "import {value} from './feature';"),
        ]);
        assert_eq!(
            slots::module(&inventory, "src/view.ts", "./feature").as_deref(),
            Some("src/feature/index.ts")
        );
        let entries = inventory.config_audit.borrow().entries;
        assert_eq!(
            slots::module(&inventory, "src/view.ts", "./feature").as_deref(),
            Some("src/feature/index.ts")
        );
        assert_eq!(inventory.config_audit.borrow().entries, entries);
    }
    #[test]
    fn literal_union_inventory_keeps_project_references_unproven_without_following_external_configs()
     {
        for key in ["references", r"refer\u0065nces"] {
            for path in ["native", ".native"] {
                let directory = tempfile::tempdir().unwrap();
                std::fs::create_dir(directory.path().join(path)).unwrap();
                std::fs::write(directory.path().join(".gitignore"), "native/\n").unwrap();
                std::fs::write(
                    directory.path().join("tsconfig.json"),
                    format!(r#"{{"files":[],"{key}":[{{"path":"./{path}"}}]}}"#),
                )
                .unwrap();
                std::fs::write(
                    directory.path().join(path).join("tsconfig.json"),
                    r#"{"compilerOptions":{"moduleSuffixes":[".native",""]}}"#,
                )
                .unwrap();
                assert!(Inventory::load(directory.path().to_str().unwrap()).is_none());
            }
        }
        assert!(plain_config(r#"{"references":[{"path":"./plain"}]}"#).is_none());
        assert!(plain_config(r#"{"references":[]}"#).is_none());
    }
    #[test]
    fn literal_union_inventory_directory_entry_guard_does_not_depend_on_index_stem() {
        let candidates = super::super::super::removed_attribution::relative_import_candidates(
            "src/view.ts",
            "./feature",
        )
        .unwrap();
        assert!(candidates.contains("src/feature/index.ts"));
        assert!(!candidates.contains("src/feature/index.d.ts"));
        let (_directory, inventory) = fixture(&[
            (".gitignore", "src/feature/package.json\n"),
            ("src/feature/package.json", r#"{"types":"./native.ts"}"#),
            ("src/feature/index.d.ts", "export type Value=string;"),
            ("src/feature/native.ts", "export type Value='all';"),
            ("src/view.ts", "import {type Value} from './feature';"),
        ]);
        assert!(
            inventory
                .directory_index_is_plain("src/feature/index.d.ts")
                .is_none()
        );
        assert!(!inventory.config_audit.borrow().failed);
        assert!(slots::module(&inventory, "src/view.ts", "./feature").is_none());
        assert_eq!(
            slots::module(&inventory, "src/view.ts", "./feature/index.d.ts").as_deref(),
            Some("src/feature/index.d.ts")
        );
    }
    #[test]
    fn literal_union_inventory_holds_missing_declaration_candidates_before_independent_module_resolution()
     {
        for (specifier, declaration, other_path, direct_expected) in [
            ("./mode", "src/mode.d.ts", "src/mode/index.ts", true),
            ("./mode", "src/mode.d.ts", "src/mode.js", true),
            ("./mode.js", "src/mode.d.ts", "src/mode.js", true),
            ("./mode.mjs", "src/mode.d.mts", "src/mode.mjs", true),
            ("./mode.cjs", "src/mode.d.cts", "src/mode.cjs", true),
            ("./mode.css", "src/mode.d.css.ts", "src/mode.css.ts", true),
            ("./mode", "src/mode/index.d.ts", "src/mode/index.ts", false),
        ] {
            let ignored = format!("{declaration}\n");
            let (_directory, inventory) = fixture(&[
                (".gitignore", &ignored),
                ("src/view.ts", "export const view=1;"),
                (declaration, "export type Mode='all'|'recent';"),
                (other_path, "export const unrelated=1;"),
            ]);
            assert!(!inventory.files.contains_key(declaration));
            assert!(
                slots::module(&inventory, "src/view.ts", specifier).is_none(),
                "{specifier}/{declaration}"
            );
            let entries = inventory.config_audit.borrow().entries;
            assert!(slots::module(&inventory, "src/view.ts", specifier).is_none());
            assert_eq!(inventory.config_audit.borrow().entries, entries);
            assert!(!inventory.config_audit.borrow().failed);
            let (_directory, inventory) = fixture(&[
                ("src/view.ts", "export const view=1;"),
                (declaration, "export type Mode='all'|'recent';"),
                (other_path, "export const unrelated=1;"),
            ]);
            assert!(slots::module(&inventory, "src/view.ts", specifier).is_none());
            let direct = format!("./{}", declaration.strip_prefix("src/").unwrap());
            assert_eq!(
                slots::module(&inventory, "src/view.ts", &direct).as_deref(),
                direct_expected.then_some(declaration)
            );
        }
        for extension in ["ts", "tsx", "mts", "cts"] {
            let declaration = format!("src/mode.d.{extension}");
            let specifier = format!("./mode.{extension}");
            let (_directory, inventory) = fixture(&[
                ("src/view.ts", "export const view=1;"),
                (&declaration, "export type Mode='all';"),
            ]);
            // 明示 source が無ければ、既存候補は別の index/source へ戻らない。
            assert!(slots::module(&inventory, "src/view.ts", &specifier).is_none());
        }
        let (_directory, inventory) = fixture(&[
            ("src/view.ts", "export const view=1;"),
            ("src/mode.native.ts", "export const value=1;"),
        ]);
        assert_eq!(
            slots::module(&inventory, "src/view.ts", "./mode.native").as_deref(),
            Some("src/mode.native.ts")
        );
    }
    #[test]
    fn literal_union_inventory_holds_implementation_and_legacy_resolution_precedence() {
        for (specifier, alternative, other) in [
            ("./mode.d.ts", "src/mode.ts", "src/mode.d.ts"),
            ("./mode.d.ts", "src/mode.tsx", "src/mode.d.ts"),
            ("./mode.d.mts", "src/mode.mts", "src/mode.d.mts"),
            ("./mode.d.cts", "src/mode.cts", "src/mode.d.cts"),
            ("./mode.js", "src/mode.js.d.ts", "src/mode.js"),
            ("./mode.js", "src/mode.js.ts", "src/mode.js"),
            ("./mode.mjs", "src/mode.mjs.d.ts", "src/mode.mjs"),
            ("./mode.cjs", "src/mode.cjs.d.ts", "src/mode.cjs"),
        ] {
            let (_directory, inventory) = fixture(&[
                ("src/view.ts", "export const view=1;"),
                (alternative, "export type Mode='all';"),
                (other, "export type Mode=string;"),
            ]);
            assert!(
                slots::module(&inventory, "src/view.ts", specifier).is_none(),
                "{specifier}/{alternative}"
            );
            assert!(!inventory.config_audit.borrow().failed);
        }
        let (_directory, inventory) = fixture(&[
            (".gitignore", "src/mode/package.json\n"),
            ("src/view.ts", "export const view=1;"),
            ("src/mode.js", "export class Mode {}"),
            ("src/mode/package.json", r#"{"types":"./types.d.ts"}"#),
            ("src/mode/types.d.ts", "export type Mode='all';"),
        ]);
        assert!(slots::module(&inventory, "src/view.ts", "./mode").is_none());
        assert!(!inventory.config_audit.borrow().failed);
        for extension in ["js", "mjs", "cjs"] {
            let source = format!("src/mode.{extension}");
            let specifier = format!("./mode.{extension}");
            let (_directory, inventory) = fixture(&[
                ("src/view.ts", "export const view=1;"),
                (&source, "export class Mode {}"),
            ]);
            assert_eq!(
                slots::module(&inventory, "src/view.ts", &specifier).as_deref(),
                Some(source.as_str())
            );
            assert!(!inventory.config_audit.borrow().failed);
            let entries = inventory.config_audit.borrow().entries;
            assert_eq!(
                slots::module(&inventory, "src/view.ts", &specifier).as_deref(),
                Some(source.as_str())
            );
            assert_eq!(inventory.config_audit.borrow().entries, entries);
        }
        let (_directory, inventory) = fixture(&[
            ("src/view.ts", "export const view=1;"),
            ("src/mode.ts", "export type Mode='all';"),
            ("src/mode.tsx", "export type Mode='recent';"),
        ]);
        assert_eq!(
            slots::module(&inventory, "src/view.ts", "./mode.tsx").as_deref(),
            Some("src/mode.tsx")
        );
        assert_eq!(
            slots::module(&inventory, "src/view.ts", "./mode.ts").as_deref(),
            Some("src/mode.ts")
        );
    }
}
