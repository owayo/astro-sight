use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::engine::parse_budget::{ParseBudget, map_with_budget};
use crate::models::reference_evidence::ReferenceObservations;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, QueryCursor};

use crate::engine::{parser, query_cache};
use crate::git_support::normalize_workspace_separators;
use crate::language::LangId;

mod string_names;

/// JS/TS のモジュールを特定できる、識別子以外の参照。
#[derive(Default)]
pub(super) struct ExportLiveness<'a> {
    targets: Option<&'a HashMap<String, ExportNames>>,
    imports: FileUses,
}

type NamedUses = HashMap<String, HashMap<String, ReferenceObservations>>;

#[derive(Default)]
pub(super) struct ExportNames {
    defaults: Vec<String>,
    named: HashSet<String>,
}

impl ExportNames {
    pub(super) fn is_empty(&self) -> bool {
        self.defaults.is_empty() && self.named.is_empty()
    }

    pub(super) fn collect(
        root: Node<'_>,
        source: &[u8],
        file: &str,
        lang: LangId,
        symbols: &super::api_changes::ExportedSymbols,
    ) -> Self {
        let mut result = Self::default();
        if !is_js_ts(lang)
            || file.ends_with(".d.ts")
            || file.ends_with(".d.mts")
            || file.ends_with(".d.cts")
        {
            return result;
        }
        let candidates: HashSet<_> = symbols
            .iter()
            .filter(|(_, kind, _)| matches!(kind.as_str(), "function" | "class"))
            .map(|(name, _, _)| name.as_str())
            .collect();
        if !candidates.is_empty() {
            visit_export_declarations(root, source, |name, default| {
                if candidates.contains(name) {
                    if default {
                        result.defaults.push(name.to_owned());
                    } else {
                        result.named.insert(name.to_owned());
                    }
                }
            });
        }
        result
    }
}

/// 中身を数えられなかったファイル (`dir` 相対のパスと失敗の内容)。
#[derive(Default)]
pub(super) struct UnobservedFiles {
    pub(super) prod: Vec<(String, String)>,
    pub(super) test: Vec<(String, String)>,
    escaped_prod: bool,
    escaped_test: bool,
}

#[derive(Default)]
struct FileUses {
    defaults: HashMap<String, (usize, usize)>,
    named: NamedUses,
    unobserved: UnobservedFiles,
}

impl<'a> ExportLiveness<'a> {
    pub(super) fn build(
        targets: &'a HashMap<String, ExportNames>,
        root: &Path,
        scan_files: &[PathBuf],
        budget: ParseBudget,
    ) -> Self {
        if targets.is_empty() {
            return Self::default();
        }
        let prefilter = specifier_prefilter(targets);
        let has_defaults = targets.values().any(|target| !target.defaults.is_empty());
        let collected = std::sync::Mutex::new(FileUses::default());
        let collect = |path: &PathBuf| {
            let mut file = FileUses::default();
            collect_file_uses(
                root,
                path,
                targets,
                prefilter.as_ref(),
                has_defaults,
                &mut file,
            );
            if file.defaults.is_empty()
                && file.named.is_empty()
                && file.unobserved.prod.is_empty()
                && file.unobserved.test.is_empty()
            {
                return;
            }
            let mut all = collected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (file, (prod, test)) in file.defaults {
                let entry = all.defaults.entry(file).or_default();
                entry.0 += prod;
                entry.1 += test;
            }
            for (path, names) in file.named {
                let target = all.named.entry(path).or_default();
                for (name, refs) in names {
                    super::reference_evidence::merge_observations(
                        target.entry(name).or_default(),
                        &refs,
                    );
                }
            }
            all.unobserved.escaped_prod |= file.unobserved.escaped_prod;
            all.unobserved.escaped_test |= file.unobserved.escaped_test;
            all.unobserved.prod.extend(file.unobserved.prod);
            all.unobserved.test.extend(file.unobserved.test);
        };
        let size = |path: &PathBuf| std::fs::metadata(path).map_or(0, |m| m.len());
        let complete = crate::engine::refs::build_bounded_pool()
            .ok()
            .is_some_and(|pool| {
                pool.install(|| map_with_budget(scan_files, budget, size, collect))
                    .is_ok()
            });
        if !complete {
            *collected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = FileUses::default();
            scan_files.iter().for_each(collect);
        }
        let mut imports = collected
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        imports.unobserved.prod.sort();
        imports.unobserved.test.sort();
        Self {
            targets: Some(targets),
            imports,
        }
    }

    pub(super) fn named_observations(
        &self,
        file: &str,
        name: &str,
    ) -> Option<&ReferenceObservations> {
        self.imports.named.get(file)?.get(name)
    }

    pub(super) fn counts_for(&self, file: &str, name: &str) -> (usize, usize) {
        if let Some(refs) = self.named_observations(file, name) {
            return (refs.production_total, refs.test_total);
        }
        if self
            .targets
            .and_then(|t| t.get(file))
            .is_some_and(|t| t.defaults.iter().any(|n| n == name))
        {
            return self.imports.defaults.get(file).copied().unwrap_or_default();
        }
        (0, 0)
    }

    pub(super) fn may_have_unobserved_uses(&self, file: &str, name: &str) -> (bool, bool) {
        let Some(target) = self.targets.and_then(|t| t.get(file)) else {
            return (false, false);
        };
        let unobserved = &self.imports.unobserved;
        if target.defaults.iter().any(|n| n == name) {
            return (!unobserved.prod.is_empty(), !unobserved.test.is_empty());
        }
        // 名前の生の綴りは refs のストリーム走査で確認済み。エスケープもあり得るときは断定を控える。
        if target.named.contains(name) {
            return (unobserved.escaped_prod, unobserved.escaped_test);
        }
        (false, false)
    }

    /// 数えられなかった JS/TS のファイル (`dir` 相対、パス順)。
    pub(super) fn unobserved_files(&self) -> &UnobservedFiles {
        &self.imports.unobserved
    }
}

fn is_js_ts(lang: LangId) -> bool {
    matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx)
}

fn visit_export_declarations(root: Node<'_>, source: &[u8], mut visit: impl FnMut(&str, bool)) {
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        if statement.kind() != "export_statement" {
            continue;
        }
        let mut children = statement.walk();
        let default = statement
            .children(&mut children)
            .any(|child| !child.is_named() && child.kind() == "default");
        let Some(declaration) = statement.child_by_field_name("declaration") else {
            continue;
        };
        if matches!(
            declaration.kind(),
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "abstract_class_declaration"
        ) && let Some(name) = declaration.child_by_field_name("name")
            && let Ok(name) = name.utf8_text(source)
        {
            visit(name, default);
        }
    }
}

fn collect_file_uses(
    root: &Path,
    path: &Path,
    targets: &HashMap<String, ExportNames>,
    prefilter: Option<&aho_corasick::AhoCorasick>,
    has_defaults: bool,
    uses: &mut FileUses,
) {
    let Some(lang) = crate::engine::refs::detect_source_lang(path) else {
        return;
    };
    if !is_js_ts(lang) {
        return;
    }
    let Ok(canonical) = path.canonicalize() else {
        return;
    };
    let Ok(relative) = canonical.strip_prefix(root) else {
        return;
    };
    let relative = normalize_workspace_separators(&relative.to_string_lossy());
    // テストからの参照かは `dir` 相対で判定する (親ディレクトリ名に `test` があっても
    // 全参照をテスト扱いにしない)。
    let is_test = super::dead_code::is_test_path(Path::new(&relative));
    // 読めない・解析できないファイルは黙って飛ばさず記録する (default import を数えられない)。
    let mut unobserved = |cause: String, escaped: bool| {
        if is_test {
            uses.unobserved.escaped_test |= escaped;
        } else {
            uses.unobserved.escaped_prod |= escaped;
        }
        let bucket = if is_test {
            &mut uses.unobserved.test
        } else {
            &mut uses.unobserved.prod
        };
        bucket.push((relative.clone(), cause));
    };
    let Some(path_utf8) = camino::Utf8Path::from_path(&canonical) else {
        unobserved("the path is not valid UTF-8".to_string(), true);
        return;
    };
    let source = match parser::read_file(path_utf8) {
        Ok(source) => source,
        Err(_) => {
            unobserved(
                crate::engine::refs::read_failure_cause(&canonical),
                crate::engine::refs::uncounted_file_may_contain_escape(path_utf8),
            );
            return;
        }
    };
    if !source.contains(&b'\\')
        && prefilter.is_some_and(|prefilter| !prefilter.is_match(&source[..]))
    {
        return;
    }
    let may_have_named = memchr::memmem::find(&source, b"import").is_some()
        && memchr::memmem::find(&source, b"as").is_some();
    if !has_defaults && !may_have_named {
        return;
    }
    let tree = match parser::parse_source(&source, lang) {
        Ok(tree) => tree,
        Err(_) => {
            unobserved(
                crate::engine::refs::PARSE_FAILURE_CAUSE.to_string(),
                source.contains(&b'\\'),
            );
            return;
        }
    };
    if may_have_named {
        string_names::collect(
            tree.root_node(),
            &source,
            root,
            &relative,
            targets,
            &mut uses.named,
        );
    }
    if !has_defaults {
        return;
    }
    let counts = &mut uses.defaults;
    let queries = [
        Some(DEFAULT_USE_QUERY),
        matches!(lang, LangId::Typescript | LangId::Tsx).then_some(TS_IMPORT_REQUIRE_QUERY),
    ];
    for query_source in queries.into_iter().flatten() {
        let Ok(query) = query_cache::cached_query(lang, query_source) else {
            continue;
        };
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), &source[..]);
        while let Some(matched) = matches.next() {
            let capture = |name| {
                matched.captures().iter().find_map(|capture| {
                    (query.capture_names()[capture.index as usize] == name).then_some(capture.node)
                })
            };
            let (Some(edge), Some(source_node)) = (capture("edge"), capture("source")) else {
                continue;
            };
            if !consumes_default(edge, &source) {
                continue;
            }
            let Some(specifier) = static_specifier(source_node, &source) else {
                continue;
            };
            for target in matching_relative_paths(&relative, specifier, |p| {
                targets.get(p).is_some_and(|t| !t.defaults.is_empty())
            }) {
                let entry = counts.entry(target).or_default();
                if is_test {
                    entry.1 = entry.1.saturating_add(1);
                } else {
                    entry.0 = entry.0.saturating_add(1);
                }
            }
        }
    }
}

fn consumes_default(edge: Node<'_>, source: &[u8]) -> bool {
    // namespace が外へ渡される経路もあるので、default の個別アクセスまでは要求しない。
    if matches!(edge.kind(), "call_expression" | "import_require_clause") {
        return true;
    }
    let mut cursor = edge.walk();
    for clause in edge.named_children(&mut cursor) {
        match clause.kind() {
            "namespace_export" => return true,
            "import_clause" => {
                let mut children = clause.walk();
                for child in clause.named_children(&mut children) {
                    if matches!(child.kind(), "identifier" | "namespace_import")
                        || (child.kind() == "named_imports" && has_default_specifier(child, source))
                    {
                        return true;
                    }
                }
            }
            "export_clause" if has_default_specifier(clause, source) => return true,
            _ => {}
        }
    }
    false
}

fn has_default_specifier(clause: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = clause.walk();
    clause.named_children(&mut cursor).any(|specifier| {
        specifier.child_by_field_name("name").is_some_and(|name| {
            matches!(
                name.utf8_text(source),
                Ok("default" | "\"default\"" | "'default'")
            )
        })
    })
}

pub(super) fn static_specifier<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let mut cursor = node.walk();
    if node
        .named_children(&mut cursor)
        .any(|child| child.kind() == "template_substitution")
    {
        return None;
    }
    let text = node.utf8_text(source).ok()?;
    let inner = text.get(1..text.len().checked_sub(1)?)?;
    // escape の復号や bundler 固有の query / alias 解決は行わない。
    (!inner.contains(['\\', '?', '#', '%', '\0'])).then_some(inner)
}

const JS_TS_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];

/// 候補集合だけに照合する。ディレクトリ外への探索や basename だけの照合はしない。
pub(super) fn resolve_relative_specifier(
    importer: &str,
    specifier: &str,
    targets: &HashMap<String, Vec<String>>,
) -> Vec<String> {
    matching_relative_paths(importer, specifier, |p| targets.contains_key(p))
}

fn matching_relative_paths(
    importer: &str,
    specifier: &str,
    contains: impl Fn(&str) -> bool,
) -> Vec<String> {
    if !(specifier.starts_with("./")
        || specifier.starts_with("../")
        || matches!(specifier, "." | ".."))
    {
        return Vec::new();
    }
    let parent = importer.rsplit_once('/').map_or("", |(parent, _)| parent);
    let mut parts: Vec<_> = parent.split('/').filter(|part| !part.is_empty()).collect();
    for part in specifier.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Vec::new();
                }
            }
            part => parts.push(part),
        }
    }
    let joined = parts.join("/");
    let mut resolved = Vec::new();
    let mut add = |path: String| {
        if contains(&path) && !resolved.contains(&path) {
            resolved.push(path);
        }
    };
    let directory = if joined.is_empty() {
        String::new()
    } else {
        format!("{joined}/")
    };
    if matches!(specifier.rsplit('/').next(), Some("" | "." | "..")) {
        for extension in JS_TS_EXTENSIONS {
            add(format!("{directory}index.{extension}"));
        }
        return resolved;
    }
    add(joined.clone());
    match joined.rsplit_once('.') {
        Some((stem, "js" | "jsx")) => {
            add(format!("{stem}.ts"));
            add(format!("{stem}.tsx"));
            add(format!("{stem}.js"));
            add(format!("{stem}.jsx"));
        }
        Some((stem, "mjs")) => add(format!("{stem}.mts")),
        Some((stem, "cjs")) => add(format!("{stem}.cts")),
        Some((_, "ts" | "tsx" | "mts" | "cts")) => {}
        _ => {
            for extension in JS_TS_EXTENSIONS {
                add(format!("{joined}.{extension}"));
                add(format!("{directory}index.{extension}"));
            }
        }
    }
    resolved
}

/// 解決先の最後の要素は必ず specifier に現れる。index の省略と . / .. だけの経路も残す。
fn specifier_prefilter<T>(targets: &HashMap<String, T>) -> Option<aho_corasick::AhoCorasick> {
    let mut needles = HashSet::new();
    for file in targets.keys() {
        let (parent, name) = file.rsplit_once('/').unwrap_or(("", file));
        let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
        needles.insert(format!("/{stem}"));
        if stem == "index" {
            if let Some(directory) = parent.rsplit('/').next().filter(|name| !name.is_empty()) {
                needles.insert(format!("/{directory}"));
            }
            for quote in ['\'', '"', '`'] {
                needles.insert(format!("{quote}.{quote}"));
                needles.insert(format!("{quote}..{quote}"));
                needles.insert(format!("/.{quote}"));
                needles.insert(format!("/..{quote}"));
                needles.insert(format!("/{quote}"));
            }
        }
    }
    let mut needles: Vec<_> = needles.into_iter().collect();
    needles.sort_unstable();
    aho_corasick::AhoCorasick::new(needles).ok()
}

// imports の DTO は句の形を保持しないため、default を消費する辺だけを AST で選ぶ。
const DEFAULT_USE_QUERY: &str = r#"
(import_statement source: (string) @source) @edge
(export_statement source: (string) @source) @edge
(call_expression
  function: (import)
  arguments: [
    (arguments . [(string) (template_string)] @source)
    (arguments . (comment)+ . [(string) (template_string)] @source)]) @edge
(call_expression
  function: (identifier) @callee
  arguments: [
    (arguments . [(string) (template_string)] @source)
    (arguments . (comment)+ . [(string) (template_string)] @source)]
  (#eq? @callee "require")) @edge
"#;

// JavaScript の文法にはこのノードが無いので、TS / TSX だけでコンパイルする。
const TS_IMPORT_REQUIRE_QUERY: &str = "(import_require_clause source: (string) @source) @edge";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_use_query_compiles_for_all_js_ts_grammars() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            query_cache::cached_query(lang, DEFAULT_USE_QUERY).unwrap();
        }
        for lang in [LangId::Typescript, LangId::Tsx] {
            query_cache::cached_query(lang, TS_IMPORT_REQUIRE_QUERY).unwrap();
        }
    }

    #[test]
    fn relative_resolution_is_bounded_and_keeps_all_matching_candidates() {
        let targets: HashMap<_, _> = [
            "panel.tsx",
            "panel.js",
            "view/index.ts",
            "index.ts",
            "module.mts",
            "module.cts",
        ]
        .into_iter()
        .map(|file| (file.to_string(), Vec::<String>::new()))
        .collect();
        assert_eq!(
            resolve_relative_specifier("app.ts", "./panel.js", &targets),
            ["panel.js", "panel.tsx"]
        );
        assert_eq!(
            resolve_relative_specifier("src/app.ts", "../view", &targets),
            ["view/index.ts"]
        );
        assert_eq!(
            resolve_relative_specifier("app.ts", ".", &targets),
            ["index.ts"]
        );
        assert_eq!(
            resolve_relative_specifier("app.ts", "./module.mjs", &targets),
            ["module.mts"]
        );
        assert_eq!(
            resolve_relative_specifier("app.ts", "./module.cjs", &targets),
            ["module.cts"]
        );
        for specifier in ["../panel", "./../panel", "panel", "/panel", "@/panel"] {
            assert!(resolve_relative_specifier("app.ts", specifier, &targets).is_empty());
        }
    }

    #[test]
    fn defaults_are_top_level_runtime_declarations_only() {
        for (lang, source, expected) in [
            (
                LangId::Javascript,
                "export default function* Stream() {}",
                vec!["Stream"],
            ),
            (
                LangId::Tsx,
                "export default function Panel() { return <div/>; }",
                vec!["Panel"],
            ),
            (
                LangId::Typescript,
                "export default abstract class Shape {}",
                vec!["Shape"],
            ),
            (
                LangId::Typescript,
                "export default interface Shape {}",
                vec![],
            ),
            (
                LangId::Typescript,
                "declare module 'x' { export default function nested(): void; }",
                vec![],
            ),
        ] {
            let tree = parser::parse_source(source.as_bytes(), lang).unwrap();
            assert!(!tree.root_node().has_error(), "{source}");
            assert_eq!(
                {
                    let mut names = Vec::new();
                    visit_export_declarations(
                        tree.root_node(),
                        source.as_bytes(),
                        |name, default| {
                            if default {
                                names.push(name.to_owned());
                            }
                        },
                    );
                    names
                },
                expected
            );
        }
    }

    #[test]
    fn specifier_prefilter_keeps_index_and_dot_only_resolution_paths() {
        let targets = [
            "views/index.tsx",
            "index.ts",
            "panel.config.tsx",
            ".panel.tsx",
        ]
        .into_iter()
        .map(|file| (file.to_string(), Vec::<String>::new()))
        .collect();
        let prefilter = specifier_prefilter(&targets).unwrap();
        for specifier in [
            "./views",
            "./views/index.js",
            ".",
            "./",
            "./panel.config.js",
            "./.panel.js",
        ] {
            assert!(!resolve_relative_specifier("app.ts", specifier, &targets).is_empty());
            for quote in ['\'', '"', '`'] {
                assert!(
                    prefilter.is_match(format!("import({quote}{specifier}{quote})").as_bytes())
                );
            }
        }
        for specifier in ["..", "../", "../..", "../../", "../."] {
            assert!(
                !resolve_relative_specifier("views/app.ts", specifier, &targets).is_empty()
                    || !resolve_relative_specifier("a/b/app.ts", specifier, &targets).is_empty()
            );
            assert!(prefilter.is_match(format!("import('{specifier}')").as_bytes()));
        }
        assert!(!prefilter.is_match(b"import('./unrelated.js')"));
        assert!(!prefilter.is_match(b"const label = 'Done.'; values.map((item, index) => item);"));
    }
}
