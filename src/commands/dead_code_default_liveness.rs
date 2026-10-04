use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Node, QueryCursor};

use crate::engine::{parser, query_cache};
use crate::git_support::normalize_workspace_separators;
use crate::language::LangId;

/// default は宣言名ではなくモジュールの依存辺から利用される。refs の出現は変更しない。
#[derive(Default)]
pub(super) struct DefaultExportLiveness {
    counts: HashMap<String, HashMap<String, (usize, usize)>>,
}

impl DefaultExportLiveness {
    pub(super) fn build(
        candidates: &[(String, String, String, LangId)],
        root: &Path,
        scan_files: &[PathBuf],
    ) -> Self {
        let mut candidate_names: HashMap<&str, HashSet<&str>> = HashMap::new();
        for (name, kind, file, lang) in candidates {
            if is_js_ts(*lang) && matches!(kind.as_str(), "function" | "class") {
                candidate_names.entry(file).or_default().insert(name);
            }
        }
        let targets: HashMap<_, _> = candidate_names
            .into_par_iter()
            .filter_map(|(file, names)| {
                if file.ends_with(".d.ts") || file.ends_with(".d.mts") || file.ends_with(".d.cts") {
                    return None;
                }
                let path = root.join(file);
                let path = camino::Utf8Path::from_path(&path)?;
                let source = parser::read_file(path).ok()?;
                if memchr::memmem::find(&source, b"default").is_none()
                    || memchr::memmem::find(&source, b"export").is_none()
                {
                    return None;
                }
                let (tree, lang) = parser::parse_file(path, &source).ok()?;
                if !is_js_ts(lang) {
                    return None;
                }
                let defaults: Vec<_> = default_declaration_names(tree.root_node(), &source)
                    .into_iter()
                    .filter(|name| names.contains(name.as_str()))
                    .collect();
                (!defaults.is_empty()).then(|| (file.to_string(), defaults))
            })
            .collect();
        if targets.is_empty() {
            return Self::default();
        }
        let prefilter = specifier_prefilter(&targets);

        // ファイル毎に一度だけ解析し、worker 内で集計する。参照全件の中間 Vec は作らない。
        let imports = scan_files
            .par_iter()
            .fold(HashMap::new, |mut counts, path| {
                collect_file_uses(root, path, &targets, prefilter.as_ref(), &mut counts);
                counts
            })
            .reduce(HashMap::new, |mut counts, other| {
                for (file, (prod, tests)) in other {
                    let entry = counts.entry(file).or_insert((0usize, 0usize));
                    entry.0 = entry.0.saturating_add(prod);
                    entry.1 = entry.1.saturating_add(tests);
                }
                counts
            });
        let mut counts = HashMap::new();
        for (file, uses) in imports {
            let mut names = HashMap::new();
            for name in &targets[&file] {
                names.insert(name.clone(), uses);
            }
            counts.insert(file, names);
        }
        Self { counts }
    }

    pub(super) fn counts_for(&self, file: &str, name: &str) -> (usize, usize) {
        self.counts
            .get(file)
            .and_then(|names| names.get(name))
            .copied()
            .unwrap_or_default()
    }
}

fn is_js_ts(lang: LangId) -> bool {
    matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx)
}

fn default_declaration_names(root: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = root.walk();
    for statement in root.named_children(&mut cursor) {
        if statement.kind() != "export_statement" {
            continue;
        }
        let mut children = statement.walk();
        if !statement
            .children(&mut children)
            .any(|child| !child.is_named() && child.kind() == "default")
        {
            continue;
        }
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
            names.push(name.to_string());
        }
    }
    names
}

fn collect_file_uses(
    root: &Path,
    path: &Path,
    targets: &HashMap<String, Vec<String>>,
    prefilter: Option<&aho_corasick::AhoCorasick>,
    counts: &mut HashMap<String, (usize, usize)>,
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
    let Some(path_utf8) = camino::Utf8Path::from_path(&canonical) else {
        return;
    };
    let Ok(source) = parser::read_file(path_utf8) else {
        return;
    };
    if prefilter.is_some_and(|prefilter| !prefilter.is_match(&source[..])) {
        return;
    }
    let Ok(tree) = parser::parse_source(&source, lang) else {
        return;
    };
    let is_test = super::dead_code::is_test_path(path);
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
            for target in resolve_relative_specifier(&relative, specifier, targets) {
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

fn static_specifier<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
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
fn resolve_relative_specifier(
    importer: &str,
    specifier: &str,
    targets: &HashMap<String, Vec<String>>,
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
        if targets.contains_key(&path) && !resolved.contains(&path) {
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
fn specifier_prefilter(
    targets: &HashMap<String, Vec<String>>,
) -> Option<aho_corasick::AhoCorasick> {
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
        .map(|file| (file.to_string(), Vec::new()))
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
                default_declaration_names(tree.root_node(), source.as_bytes()),
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
        .map(|file| (file.to_string(), Vec::new()))
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
