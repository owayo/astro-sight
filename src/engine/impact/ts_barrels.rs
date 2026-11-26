//! Narrow proof for unchanged TS/JS references imported through named or star barrels.
//! Unknown module resolution and ambiguous exports keep the existing low-confidence route.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::engine::parser;
use crate::language::LangId;

use super::FileContext;

type OriginKey = (String, String, String, String, bool);

#[derive(Default)]
pub(super) struct TsBarrelIndex {
    pub(super) const_alias_reads: Mutex<HashSet<(String, String)>>,
    index_reader: Option<Mutex<crate::commands::GitBlobBatch>>,
    const_candidates: Vec<(String, String, u32)>,
    root: PathBuf,
    files: HashSet<String>,
    modules: Mutex<Modules>,
    memo: Mutex<HashMap<OriginKey, Origin>>,
    alias_modules: Mutex<HashMap<(String, String), Option<String>>>,
    proofs: Mutex<HashMap<(String, String, String), bool>>,
    reachability: Mutex<HashMap<(String, String), bool>>,
    paths: Vec<(String, String, String, bool)>,
}

type Modules = HashMap<String, Option<Arc<Module>>>;

#[derive(Clone, Default)]
struct Module {
    possible_commonjs: bool,
    imports: HashMap<String, Vec<String>>,
    named: HashMap<String, Vec<(String, String)>>,
    stars: Vec<String>,
    local_exports: HashSet<String>,
    literal_exports: HashSet<String>,
    declared_functions: HashSet<String>,
    const_exports: HashSet<String>,
    dependencies: Vec<String>,
    opaque: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Origin {
    Changed,
    Other,
    Absent,
    Unknown,
}

impl TsBarrelIndex {
    pub(super) fn build(dir: &Path, files: &[PathBuf], changes: &[FileContext]) -> Self {
        if !changes.iter().any(|change| is_ts(change.lang_id)) {
            return Self::default();
        }
        Self {
            const_candidates: changes
                .iter()
                .enumerate()
                .filter(|(_, c)| is_ts(c.lang_id))
                .flat_map(|(fc, c)| {
                    c.affected
                        .iter()
                        .filter(|s| s.change_type == "modified" && s.kind == "variable")
                        .map(move |s| (c.new_path.clone(), s.name.clone(), fc as u32))
                })
                .collect(),
            root: dir.to_path_buf(),
            files: files
                .iter()
                .filter_map(|path| path.strip_prefix(dir).ok().and_then(Path::to_str))
                .map(crate::git_support::normalize_workspace_separators)
                .collect(),
            paths: read_paths(dir),
            ..Self::default()
        }
    }

    pub(super) fn for_index_writes(dir: &Path) -> Self {
        let Some((dir, files)) = dir
            .to_str()
            .and_then(|dir| Some((dir, crate::commands::plain_index_module_paths(dir)?)))
        else {
            return Self::default();
        };
        Self {
            root: dir.into(),
            files,
            index_reader: Some(Mutex::new(crate::commands::GitBlobBatch::index(dir))),
            ..Self::default()
        }
    }

    pub(super) fn snapshot_paths(&self) -> impl Iterator<Item = &str> {
        self.files.iter().map(String::as_str)
    }
    pub(super) fn omits_index_path(&self, path: &str) -> bool {
        self.index_reader.is_some() && !self.files.contains(path)
    }

    pub(super) fn has_changed_variables(&self) -> bool {
        !self.const_candidates.is_empty()
    }

    pub(super) fn alias_owner(
        &self,
        caller: &str,
        specifier: &str,
        imported: &str,
    ) -> Option<(u32, &str)> {
        let target = self.resolve_for::<true>(caller, specifier)?;
        let module = self.module(&target)?;
        if module.opaque {
            return None;
        }
        self.const_candidates.iter().find_map(|(path, name, fc)| {
            if module.local_exports.contains(imported) && (path != &target || name != imported) {
                return None;
            }
            let mut visiting = HashSet::new();
            (self.origin::<true>(&target, imported, path, name, &mut visiting) == Origin::Changed
                && self.module(path)?.const_exports.contains(name))
            .then_some((*fc, name.as_str()))
        })
    }

    pub(super) fn proves_origin(&self, caller: &str, name: &str, changed: &str) -> bool {
        // `export *` never forwards a default export.
        if name == "default" {
            return false;
        }
        let key = (caller.to_owned(), name.to_owned(), changed.to_owned());
        if let Ok(proofs) = self.proofs.lock()
            && let Some(&result) = proofs.get(&key)
        {
            return result;
        }
        let result = self.proves_origin_uncached(caller, name, changed);
        if let Ok(mut proofs) = self.proofs.lock() {
            proofs.insert(key, result);
        }
        result
    }

    /// 直接のオブジェクトなら問い合わせたパスを比較し、独立なリテラルなら影響しない。
    /// 再輸出や変数経由の別名では、オブジェクトがパスの途中に現れる可能性も残す。
    pub(super) fn query_observes_change(
        &self,
        caller: &str,
        specifier: &str,
        exported: Option<&str>,
        changed: &str,
        name: &str,
        member_unchanged: bool,
    ) -> bool {
        let Some(target) = self.resolve(caller, specifier) else {
            return true;
        };
        if target == changed && exported == Some(name) {
            return !member_unchanged;
        }
        !exported
            .zip(self.module(&target))
            .is_some_and(|(export, module)| {
                !module.opaque && module.literal_exports.contains(export)
            })
            && self.module_may_reach(&target, changed)
    }

    pub(super) fn module_may_reach(&self, from: &str, changed: &str) -> bool {
        self.reaches::<false>(from, changed, None, &self.reachability)
    }

    fn reaches<const WRITE: bool>(
        &self,
        from: &str,
        changed: &str,
        overrides: Option<&Modules>,
        cache: &Mutex<HashMap<(String, String), bool>>,
    ) -> bool {
        let key = (from.to_owned(), changed.to_owned());
        if let Some(result) = cache.lock().ok().and_then(|cache| cache.get(&key).copied()) {
            return result;
        }
        let mut seen = HashSet::new();
        let mut pending = vec![from.to_owned()];
        let result = 'search: loop {
            let Some(path) = pending.pop() else {
                break false;
            };
            if path == changed {
                break true;
            }
            if !seen.insert(path.clone()) {
                continue;
            }
            let Some(module) = self.module_with_overrides(&path, overrides) else {
                break true;
            };
            if module.opaque || WRITE && module.possible_commonjs {
                break true;
            }
            for specifier in &module.dependencies {
                let Some(target) = self.resolve_for::<WRITE>(&path, specifier) else {
                    break 'search true;
                };
                pending.push(target);
            }
        };
        if let Ok(mut cache) = cache.lock() {
            cache.insert(key, result);
        }
        result
    }

    fn module_with_overrides(
        &self,
        path: &str,
        overrides: Option<&Modules>,
    ) -> Option<Arc<Module>> {
        overrides
            .and_then(|o| o.get(path))
            .cloned()
            .unwrap_or_else(|| self.module(path))
    }

    fn proves_origin_uncached(&self, caller: &str, name: &str, changed: &str) -> bool {
        let Some(module) = self.module(caller) else {
            return false;
        };
        let Some(specifiers) = module.imports.get(name) else {
            return false;
        };
        // Multiple import bindings or unresolved module candidates cannot prove ownership.
        if specifiers.len() != 1 {
            return false;
        }
        let Some(path) = self.resolve(caller, &specifiers[0]) else {
            return false;
        };
        let mut visiting = HashSet::new();
        self.origin::<false>(&path, name, changed, name, &mut visiting) == Origin::Changed
    }

    fn module(&self, path: &str) -> Option<Arc<Module>> {
        if !self.files.contains(path) {
            return None;
        }
        if let Some(cached) = self.modules.lock().ok()?.get(path).cloned() {
            return cached;
        }
        let parsed = self.parse_module(path);
        self.modules
            .lock()
            .ok()?
            .entry(path.to_owned())
            .or_insert_with(|| parsed.clone())
            .clone()
    }

    fn parse_module(&self, path: &str) -> Option<Arc<Module>> {
        let lang = LangId::from_path(camino::Utf8Path::new(path)).ok()?;
        if !is_ts(lang) {
            return None;
        }
        let source = if let Some(index) = &self.index_reader {
            index
                .lock()
                .ok()?
                .read_limited(path, crate::commands::MAX_INPUT_SIZE)?
        } else {
            crate::engine::bounded_read::read_input_file(&self.root.join(path))?
        };
        module_from_source(&source, lang)
    }

    fn origin<const CONST_ALIAS: bool>(
        &self,
        path: &str,
        name: &str,
        changed: &str,
        changed_name: &str,
        visiting: &mut HashSet<(String, String)>,
    ) -> Origin {
        let memo_key = (
            path.to_owned(),
            name.to_owned(),
            changed.to_owned(),
            changed_name.to_owned(),
            CONST_ALIAS,
        );
        if let Ok(memo) = self.memo.lock()
            && let Some(&result) = memo.get(&memo_key)
        {
            return result;
        }
        let visit_key = (path.to_owned(), name.to_owned());
        if !visiting.insert(visit_key.clone()) {
            return Origin::Unknown;
        }
        let result = self.origin_inner::<CONST_ALIAS>(path, name, changed, changed_name, visiting);
        visiting.remove(&visit_key);
        if result != Origin::Unknown
            && let Ok(mut memo) = self.memo.lock()
        {
            memo.insert(memo_key, result);
        }
        result
    }

    fn origin_inner<const CONST_ALIAS: bool>(
        &self,
        path: &str,
        name: &str,
        changed: &str,
        changed_name: &str,
        visiting: &mut HashSet<(String, String)>,
    ) -> Origin {
        let Some(module) = self.module(path) else {
            return Origin::Unknown;
        };
        if module.opaque {
            return Origin::Unknown;
        }
        if module.local_exports.contains(name) {
            return if path == changed && name == changed_name {
                Origin::Changed
            } else {
                Origin::Other
            };
        }
        if let Some(edges) = module.named.get(name) {
            if edges.len() != 1 {
                return Origin::Unknown;
            }
            let (specifier, source_name) = &edges[0];
            let Some(target) = self.resolve_for::<CONST_ALIAS>(path, specifier) else {
                return Origin::Unknown;
            };
            return self.origin::<CONST_ALIAS>(
                &target,
                source_name,
                changed,
                changed_name,
                visiting,
            );
        }
        let mut found = Origin::Absent;
        for specifier in &module.stars {
            let Some(target) = self.resolve_for::<CONST_ALIAS>(path, specifier) else {
                return Origin::Unknown;
            };
            match self.origin::<CONST_ALIAS>(&target, name, changed, changed_name, visiting) {
                Origin::Unknown => return Origin::Unknown,
                Origin::Absent => {}
                result if found == Origin::Absent => found = result,
                _ => return Origin::Unknown,
            }
        }
        found
    }

    fn path_alias_bases(&self, from: &str, specifier: &str) -> Option<Vec<String>> {
        if !self.root_config_applies(from) {
            return None;
        }
        let mut best = 0;
        let mut matches = Vec::new();
        for (prefix, suffix, replacement, exact) in &self.paths {
            if *exact && specifier != prefix {
                continue;
            }
            let Some(rest) = specifier
                .strip_prefix(prefix)
                .and_then(|s| s.strip_suffix(suffix))
            else {
                continue;
            };
            let specificity = if *exact { usize::MAX } else { prefix.len() };
            if specificity > best {
                best = specificity;
                matches.clear();
            }
            if specificity == best {
                matches.push(replacement.replace('*', rest));
            }
        }
        Some(matches)
    }

    fn resolve_for<const CONST_ALIAS: bool>(&self, from: &str, specifier: &str) -> Option<String> {
        if self.index_reader.is_some() {
            if !specifier.starts_with('.') || specifier.contains(['?', '#', '\\']) {
                return None;
            }
            let here = Path::new(from).parent()?.join(specifier);
            let base = normalize(here.to_str()?)?;
            if self.files.contains(&format!("{base}/package.json")) {
                return None;
            }
            return self.resolve(from, specifier);
        }
        if !CONST_ALIAS {
            return self.resolve(from, specifier);
        }
        // 同じディレクトリの import は相対パス・設定・package.json の証明を共用する。
        let key = (
            Path::new(from).parent()?.to_str()?.to_owned(),
            specifier.to_owned(),
        );
        if let Some(cached) = self.alias_modules.lock().ok()?.get(&key).cloned() {
            return cached;
        }
        let result = (|| {
            let mapped;
            let relative = if specifier.starts_with('.') {
                specifier
            } else {
                let bases = self.path_alias_bases(from, specifier)?;
                let [base] = bases.as_slice() else {
                    return None;
                };
                // paths の置換先はプロジェクトのルート基準。参照ファイル基準へ戻し、
                // 拡張子・package.json・設定を相対 import と同じ条件で確認する。
                let up = "../".repeat(Path::new(from).parent()?.components().count());
                mapped = format!("./{up}{base}");
                &mapped
            };
            crate::commands::ts_const_rebinding::resolved_const_import(
                self.root.to_str()?,
                from,
                relative,
            )
        })();
        self.alias_modules.lock().ok()?.insert(key, result.clone());
        result
    }

    fn resolve(&self, from: &str, specifier: &str) -> Option<String> {
        if specifier.contains('?') || specifier.contains('#') || specifier.contains('\\') {
            return None;
        }
        let bases: Vec<String> = if specifier == "."
            || specifier == ".."
            || specifier.starts_with("./")
            || specifier.starts_with("../")
        {
            let parent = Path::new(from).parent()?.to_str()?;
            vec![if parent.is_empty() {
                specifier.to_owned()
            } else {
                format!("{parent}/{specifier}")
            }]
        } else {
            self.path_alias_bases(from, specifier)?
        };
        let mut found = None;
        for base in bases {
            let base = normalize(&base)?;
            let stem = Path::new(&base);
            let ext = stem.extension().and_then(|ext| ext.to_str());
            let (direct, index): (Vec<String>, Vec<String>) = if matches!(
                ext,
                Some("ts" | "tsx" | "js" | "jsx" | "mts" | "cts" | "mjs" | "cjs")
            ) {
                let root = stem.with_extension("").to_string_lossy().into_owned();
                let mut candidates = vec![base.clone()];
                if matches!(ext, Some("js" | "jsx" | "mjs" | "cjs")) {
                    for mapped in ["ts", "tsx", "mts", "cts"] {
                        candidates.push(format!("{root}.{mapped}"));
                    }
                }
                (candidates, Vec::new())
            } else {
                let direct = ["ts", "tsx", "d.ts", "js", "jsx", "mts", "cts", "mjs", "cjs"]
                    .iter()
                    .map(|ext| format!("{base}.{ext}"))
                    .collect();
                let index = ["ts", "tsx", "d.ts", "js", "jsx"]
                    .iter()
                    .map(|ext| format!("{base}/index.{ext}"))
                    .collect();
                (direct, index)
            };
            let direct_hit = self.unique_existing(direct)?;
            let candidate = if direct_hit.is_some() {
                direct_hit
            } else {
                self.unique_existing(index)?
            };
            if let Some(candidate) = candidate {
                if found.is_some() {
                    return None;
                }
                found = Some(candidate);
            }
        }
        found
    }

    fn unique_existing(&self, candidates: Vec<String>) -> Option<Option<String>> {
        let mut found = None;
        for candidate in candidates {
            if self.index_reader.as_ref().map_or_else(
                || self.root.join(&candidate).is_file(),
                |_| self.files.contains(&candidate),
            ) {
                if found.is_some() {
                    return None;
                }
                found = Some(candidate);
            }
        }
        Some(found)
    }

    fn root_config_applies(&self, from: &str) -> bool {
        let Some(mut dir) = Path::new(from).parent() else {
            return false;
        };
        while !dir.as_os_str().is_empty() {
            let path = self.root.join(dir);
            if path.join("tsconfig.json").is_file() || path.join("jsconfig.json").is_file() {
                return false;
            }
            let Some(parent) = dir.parent() else {
                return false;
            };
            dir = parent;
        }
        true
    }
}

pub(super) struct WriteOrigins<'a> {
    index: &'a TsBarrelIndex,
    versions: Modules,
    resolution_changed: bool,
    reachability: Mutex<HashMap<(String, String), bool>>,
}

impl<'a> WriteOrigins<'a> {
    pub(super) fn new(index: &'a TsBarrelIndex) -> Self {
        Self {
            index,
            versions: Modules::new(),
            resolution_changed: false,
            reachability: Mutex::default(),
        }
    }
    pub(super) fn observe_change(&mut self, path: &str, old: impl FnOnce() -> Option<Vec<u8>>) {
        if let Ok(lang) = LangId::from_path(camino::Utf8Path::new(path))
            && is_ts(lang)
        {
            // どちらかの版で依存するモジュールを辿る。独立した公開値は両版での証明が要る。
            let summary = old()
                .and_then(|s| module_from_source(&s, lang))
                .zip(self.index.module(path))
                .map(|(old, new)| {
                    let mut combined = Arc::unwrap_or_clone(old);
                    combined
                        .dependencies
                        .extend(new.dependencies.iter().cloned());
                    combined.dependencies.sort_unstable();
                    combined.dependencies.dedup();
                    combined
                        .literal_exports
                        .retain(|name| new.literal_exports.contains(name));
                    combined
                        .declared_functions
                        .retain(|name| new.declared_functions.contains(name));
                    combined.opaque |= new.opaque;
                    combined.possible_commonjs |= new.possible_commonjs;
                    Arc::new(combined)
                });
            self.versions.insert(path.to_owned(), summary);
        } else if path.ends_with("package.json") || path.ends_with("config.json") {
            self.resolution_changed = true;
        }
    }
    pub(super) fn may_call(
        &self,
        caller: &str,
        module: &str,
        called: &str,
        file: &str,
        getter: &str,
    ) -> bool {
        if !self.resolution_changed
            && let Some(path) = self.index.resolve_for::<true>(caller, module)
            && path == file
            && called != getter
            && self
                .index
                .module_with_overrides(&path, Some(&self.versions))
                .is_some_and(|m| {
                    !m.opaque && !m.possible_commonjs && m.declared_functions.contains(called)
                })
        {
            return false;
        }
        self.may_target(caller, module, Some(called), file, getter)
    }

    pub(super) fn may_target(
        &self,
        caller: &str,
        specifier: &str,
        exported: Option<&str>,
        file: &str,
        name: &str,
    ) -> bool {
        if self.resolution_changed {
            return true;
        }
        let Some(path) = self.index.resolve_for::<true>(caller, specifier) else {
            return true;
        };
        if path == file && exported == Some(name) {
            return true;
        }
        if let Some(export) = exported
            && self
                .index
                .module_with_overrides(&path, Some(&self.versions))
                .is_some_and(|m| {
                    !m.opaque && !m.possible_commonjs && m.literal_exports.contains(export)
                })
        {
            return false;
        }
        self.index
            .reaches::<true>(&path, file, Some(&self.versions), &self.reachability)
    }
}

fn module_from_source(source: &[u8], lang: LangId) -> Option<Arc<Module>> {
    let tree = parser::parse_source(source, lang).ok()?;
    if tree.root_node().has_error() {
        return None;
    }
    let mut module = Module {
        possible_commonjs: [b"exports".as_slice(), br"\u"]
            .into_iter()
            .any(|word| memchr::memmem::find(source, word).is_some()),
        dependencies: crate::engine::imports::extract_imports(tree.root_node(), source, lang)
            .ok()?
            .into_iter()
            .map(|edge| edge.source)
            .collect(),
        opaque: source
            .windows(b"module.exports".len())
            .any(|window| window == b"module.exports")
            || source
                .windows(b"exports.".len())
                .any(|window| window == b"exports."),
        ..Module::default()
    };
    let mut cursor = tree.root_node().walk();
    for node in tree.root_node().named_children(&mut cursor) {
        match node.kind() {
            "import_statement" => collect_import(node, source, &mut module),
            "export_statement" => collect_export(node, source, &mut module),
            _ => {}
        }
    }
    module.dependencies.sort_unstable();
    module.dependencies.dedup();
    Some(Arc::new(module))
}

fn is_ts(lang: LangId) -> bool {
    matches!(lang, LangId::Typescript | LangId::Tsx | LangId::Javascript)
}

fn normalize(path: &str) -> Option<String> {
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    Some(parts.join("/"))
}

fn source_specifier(node: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    let string = node.child_by_field_name("source")?;
    let raw = string.utf8_text(source).ok()?;
    let inner = raw.strip_prefix(['\'', '"'])?.strip_suffix(['\'', '"'])?;
    (!inner.is_empty()).then(|| inner.to_owned())
}

fn collect_import(node: tree_sitter::Node<'_>, source: &[u8], module: &mut Module) {
    let Some(specifier) = source_specifier(node, source) else {
        return;
    };
    let mut cursor = node.walk();
    for clause in node.named_children(&mut cursor) {
        if clause.kind() != "import_clause" {
            continue;
        }
        let mut cc = clause.walk();
        for group in clause.named_children(&mut cc) {
            if group.kind() != "named_imports" {
                continue;
            }
            let mut gc = group.walk();
            for binding in group.named_children(&mut gc) {
                if binding.kind() != "import_specifier" {
                    continue;
                }
                let Ok(text) = binding.utf8_text(source) else {
                    continue;
                };
                // refs searches the imported spelling; aliases require a separate name map.
                if is_plain_name(text) {
                    module
                        .imports
                        .entry(text.to_owned())
                        .or_default()
                        .push(specifier.clone());
                }
            }
        }
    }
}

fn collect_export(node: tree_sitter::Node<'_>, source: &[u8], module: &mut Module) {
    let mut token_cursor = node.walk();
    if node
        .children(&mut token_cursor)
        .any(|child| child.kind() == "default")
    {
        return;
    }
    let specifier = source_specifier(node, source);
    module.dependencies.extend(specifier.iter().cloned());
    let mut cursor = node.walk();
    let clause = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "export_clause");
    if let Some(clause) = clause {
        let mut cc = clause.walk();
        for binding in clause.named_children(&mut cc) {
            if binding.kind() != "export_specifier" {
                continue;
            }
            let Some(name) = binding.child_by_field_name("name") else {
                continue;
            };
            let Ok(name) = name.utf8_text(source) else {
                continue;
            };
            let exported = binding
                .child_by_field_name("alias")
                .and_then(|alias| alias.utf8_text(source).ok())
                .unwrap_or(name);
            if !is_plain_name(name) || !is_plain_name(exported) {
                continue;
            }
            if let Some(specifier) = &specifier {
                module
                    .named
                    .entry(exported.to_owned())
                    .or_default()
                    .push((specifier.clone(), name.to_owned()));
            } else {
                module.local_exports.insert(exported.to_owned());
            }
        }
    } else if let Some(specifier) = specifier {
        let mut nc = node.walk();
        if let Some(namespace) = node
            .named_children(&mut nc)
            .find(|child| child.kind() == "namespace_export")
        {
            if let Some(alias) = namespace.named_child(0)
                && let Ok(name) = alias.utf8_text(source)
            {
                module.local_exports.insert(name.to_owned());
            }
            return;
        }
        let Ok(statement) = node.utf8_text(source) else {
            return;
        };
        let tail = statement
            .trim_start()
            .strip_prefix("export")
            .map(str::trim_start);
        let tail = tail.and_then(|tail| {
            tail.strip_prefix("type")
                .map(str::trim_start)
                .or(Some(tail))
        });
        if tail.is_some_and(|tail| {
            tail.strip_prefix('*')
                .is_some_and(|tail| tail.trim_start().starts_with("from"))
        }) {
            module.stars.push(specifier);
        } else {
            module.opaque = true;
        }
    } else if let Some(declaration) = node.child_by_field_name("declaration") {
        // Only declaration names take precedence over a star. The initializer, parameter list,
        // and function body may contain unrelated identifiers, even in the changed file.
        if matches!(
            declaration.kind(),
            "lexical_declaration" | "variable_declaration"
        ) {
            let mut dc = declaration.walk();
            for declarator in declaration.named_children(&mut dc) {
                if declarator.kind() != "variable_declarator" {
                    continue;
                }
                if let Some(pattern) = declarator.child_by_field_name("name") {
                    if pattern.kind() == "identifier"
                        && declarator.child_by_field_name("type").is_none()
                        && declarator
                            .child_by_field_name("value")
                            .is_some_and(super::object_members::type_queries::independent_literal)
                        && let Ok(name) = pattern.utf8_text(source)
                    {
                        module.literal_exports.insert(name.to_owned());
                    }
                    let _ = crate::engine::js_binding_pattern::visit_pattern_bindings(
                        pattern,
                        &mut |binding| {
                            if let Ok(name) = binding.utf8_text(source) {
                                if declaration
                                    .child_by_field_name("kind")
                                    .is_some_and(|kind| kind.kind() == "const")
                                {
                                    module.const_exports.insert(name.to_owned());
                                }
                                module.local_exports.insert(name.to_owned());
                            }
                            ControlFlow::Continue(())
                        },
                    );
                }
            }
        } else if let Some(name) = declaration.child_by_field_name("name")
            && let Ok(name) = name.utf8_text(source)
        {
            if declaration.kind() == "function_declaration" {
                module.declared_functions.insert(name.to_owned());
            }
            module.local_exports.insert(name.to_owned());
        } else {
            module.opaque = true;
        }
    } else {
        module.opaque = true;
    }
}

fn is_plain_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c == '$' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c == '$' || c.is_alphanumeric())
}

fn read_paths(dir: &Path) -> Vec<(String, String, String, bool)> {
    let config_path = if dir.join("tsconfig.json").is_file() {
        dir.join("tsconfig.json")
    } else {
        dir.join("jsconfig.json")
    };
    let Ok(raw) = std::fs::read_to_string(config_path) else {
        return Vec::new();
    };
    let Some(json) = jsonc_to_json(raw.trim_start_matches('\u{feff}')) else {
        return Vec::new();
    };
    let Ok(config) = serde_json::from_str::<serde_json::Value>(&json) else {
        return Vec::new();
    };
    // Inherited baseUrl/paths can change the meaning of a target path. This index does not
    // resolve extends, so only standalone configs can provide positive evidence.
    if config.get("extends").is_some() || config.get("references").is_some() {
        return Vec::new();
    }
    let Some(options) = config.get("compilerOptions") else {
        return Vec::new();
    };
    if options.get("rootDirs").is_some() || options.get("moduleSuffixes").is_some() {
        return Vec::new();
    }
    let base = options
        .get("baseUrl")
        .and_then(|value| value.as_str())
        .unwrap_or(".");
    if Path::new(base).is_absolute()
        || base.starts_with('/')
        || base.contains('\\')
        || base.contains(':')
    {
        return Vec::new();
    }
    let Some(paths) = options.get("paths").and_then(|value| value.as_object()) else {
        return Vec::new();
    };
    let mut rules = Vec::new();
    for (pattern, values) in paths {
        let (prefix, suffix, exact) = match pattern.split_once('*') {
            Some((prefix, suffix)) => (prefix, suffix, false),
            None => (pattern.as_str(), "", true),
        };
        if suffix.contains('*') {
            continue;
        }
        let Some(values) = values.as_array() else {
            continue;
        };
        for value in values {
            if let Some(target) = value.as_str() {
                if Path::new(target).is_absolute()
                    || target.starts_with('/')
                    || target.contains('\\')
                    || target.contains(':')
                {
                    continue;
                }
                rules.push((
                    prefix.to_owned(),
                    suffix.to_owned(),
                    format!("{base}/{target}"),
                    exact,
                ));
            }
        }
    }
    rules
}

fn jsonc_to_json(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut quoted = false;
    while i < bytes.len() {
        let byte = bytes[i];
        if quoted {
            out.push(byte);
            if byte == b'\\' && i + 1 < bytes.len() {
                i += 1;
                out.push(bytes[i]);
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
            out.push(byte);
        } else if byte == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        } else if byte == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 == bytes.len() {
                return None;
            }
            i += 2;
            continue;
        } else {
            out.push(byte);
        }
        i += 1;
    }
    if quoted {
        return None;
    }
    let mut clean = Vec::with_capacity(out.len());
    let mut i = 0;
    let mut quoted = false;
    while i < out.len() {
        let byte = out[i];
        if byte == b'"'
            && (i == 0 || out[..i].iter().rev().take_while(|&&b| b == b'\\').count() % 2 == 0)
        {
            quoted = !quoted;
        }
        if !quoted && byte == b',' {
            let mut next = i + 1;
            while next < out.len() && out[next].is_ascii_whitespace() {
                next += 1;
            }
            if matches!(out.get(next), Some(b']' | b'}')) {
                i += 1;
                continue;
            }
        }
        clean.push(byte);
        i += 1;
    }
    String::from_utf8(clean).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_file_only_owns_its_explicit_export_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("real.ts"), "export const LIMIT = 1;\n").unwrap();
        std::fs::write(
            dir.path().join("barrel.ts"),
            "export * from './real';\nexport function f() { const LIMIT = 2; return LIMIT; }\nexport default function LIMIT() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("destructured.ts"),
            "export * from './real';\nexport const { LIMIT } = source;\n",
        )
        .unwrap();
        let index = TsBarrelIndex {
            root: dir.path().to_path_buf(),
            files: ["real.ts", "barrel.ts", "destructured.ts"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            ..TsBarrelIndex::default()
        };
        assert_eq!(
            index.origin::<false>(
                "barrel.ts",
                "LIMIT",
                "barrel.ts",
                "LIMIT",
                &mut HashSet::new()
            ),
            Origin::Other
        );
        assert_eq!(
            index.origin::<false>(
                "destructured.ts",
                "LIMIT",
                "real.ts",
                "LIMIT",
                &mut HashSet::new()
            ),
            Origin::Other
        );
    }

    #[test]
    fn path_alias_proof_requires_a_local_relative_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tsconfig.json"),
            r#"{"references":[{"path":"./app"}],"compilerOptions":{"paths":{"@/*":["src/*"]}}}"#,
        )
        .unwrap();
        assert!(read_paths(dir.path()).is_empty());
        std::fs::write(
            dir.path().join("tsconfig.json"),
            r#"{"compilerOptions":{"paths":{"@/*":["/outside/*"]}}}"#,
        )
        .unwrap();
        assert!(read_paths(dir.path()).is_empty());
    }
}
