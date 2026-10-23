//! private 葉モジュールの証明だけで使う、標準 assert の include receiver の限定的な証拠。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tree_sitter::Node;

use crate::engine::bounded_read::{read_input_file, read_input_text_file};
use crate::engine::parser;
use crate::language::LangId;

struct Source {
    bytes: Vec<u8>,
}

pub(super) struct AssertionProof {
    sources: HashMap<PathBuf, Source>,
    packages: HashSet<PathBuf>,
    relocated: Vec<PathBuf>,
}

pub(super) fn candidate_offsets(source: &[u8]) -> HashSet<usize> {
    let Ok(tree) = parser::parse_source(source, LangId::Rust) else {
        return HashSet::new();
    };
    let mut offsets = HashSet::new();
    let mut pending = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        if let Some(receiver) = assertion_receiver(node, source) {
            offsets.insert(receiver.start_byte());
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    offsets
}

impl AssertionProof {
    pub(super) fn build(
        sources: &[(PathBuf, blake3::Hash)],
        manifests: &[PathBuf],
    ) -> Option<Self> {
        if sources.len() > 10_000 || manifests.len() > 10_000 {
            return None;
        }
        let mut packages = HashSet::new();
        for path in manifests {
            let path = path.canonicalize().ok()?;
            let manifest = read_input_text_file(&path)?;
            let value = toml::from_str::<toml::Value>(&manifest).ok()?;
            let package = path.parent()?;
            let targets =
                crate::engine::rust_targets::RustTargets::parse(&manifest, Path::new(""))?;
            if !targets.roots_complete
                || targets.explicit_roots().any(|path| {
                    !["src/lib.rs", "src/main.rs", "build.rs"]
                        .iter()
                        .any(|allowed| path == Path::new(allowed))
                })
                || super::rust_std_origin::has_sysroot_dependency(&value, false)
                || value.get("patch").is_some()
                || value.get("replace").is_some()
            {
                return None;
            }
            if targets.has_package {
                packages.insert(package.to_owned());
            }
        }
        let mut total = 0usize;
        let mut parsed = HashMap::new();
        let paths: HashSet<_> = sources
            .iter()
            .map(|(path, _)| path.canonicalize().ok())
            .collect::<Option<_>>()?;
        for (path, expected) in sources {
            let canonical = path.canonicalize().ok()?;
            let bytes = read_input_file(path)?;
            if blake3::hash(&bytes) != *expected {
                return None;
            }
            total = total.checked_add(bytes.len())?;
            if bytes.len() > 2 * 1024 * 1024 || total > 64 * 1024 * 1024 {
                return None;
            }
            std::str::from_utf8(&bytes).ok()?;
            parsed.insert(canonical, Source { bytes });
        }
        // OnceLock の初期化中に元の Rayon pool の別仕事を実行しないよう、専用 thread で待つ。
        // 最大 4 chunk とし、同時に保持する AST の数も制限する。
        let entries: Vec<_> = parsed.iter().collect();
        let relocated = std::thread::scope(|scope| {
            let handles: Vec<_> = entries
                .chunks(entries.len().div_ceil(4).max(1))
                .map(|chunk| {
                    let paths = &paths;
                    std::thread::Builder::new().spawn_scoped(scope, move || {
                        let mut relocated = Vec::new();
                        for &(path, source) in chunk {
                            let tree = parser::parse_source(&source.bytes, LangId::Rust).ok()?;
                            if tree.root_node().has_error() {
                                return None;
                            }
                            relocated.extend(ordinary_placement(
                                tree.root_node(),
                                &source.bytes,
                                path,
                                paths,
                            )?);
                        }
                        Some(relocated)
                    })
                })
                .collect();
            let results: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.ok().and_then(|handle| handle.join().ok()).flatten())
                .collect();
            results.into_iter().collect::<Option<Vec<_>>>()
        })?
        .into_iter()
        .flatten()
        .collect();
        Some(Self {
            sources: parsed,
            packages,
            relocated,
        })
    }

    pub(super) fn receiver_offsets(&self, path: &Path, expected: &[u8]) -> Option<HashSet<usize>> {
        let canonical = path.canonicalize().ok()?;
        let path = canonical.as_path();
        let source = self.sources.get(path)?;
        if source.bytes != expected {
            return None;
        }
        if self
            .relocated
            .iter()
            .any(|directory| path.starts_with(directory))
        {
            return None;
        }
        let tree = parser::parse_source(&source.bytes, LangId::Rust).ok()?;
        let package = path
            .ancestors()
            .find(|path| self.packages.contains(*path))?;
        let relative = path.strip_prefix(package.join("src")).ok()?;
        // bin 配下は自動 target root になりうるので、この証明の候補にしない。
        if relative.components().next()?.as_os_str() == "bin" {
            return None;
        }

        let mut environments = Vec::new();
        for package in &self.packages {
            for root in ["src/lib.rs", "src/main.rs", "build.rs"] {
                let root = package.join(root);
                if self.sources.contains_key(&root)
                    && let Some(modules) = module_path(&root, path)
                {
                    let mut environment = Environment::default();
                    if self.follow(&root, &modules, path, true, &mut environment)? {
                        environments.push(environment);
                    }
                }
            }
        }
        if environments.is_empty() {
            return None;
        }

        let mut result = HashSet::new();
        let mut pending = vec![tree.root_node()];
        while let Some(node) = pending.pop() {
            if let Some(receiver) = assertion_receiver(node, &source.bytes) {
                let mut local = Environment::default();
                if local
                    .add(tree.root_node(), &source.bytes, Some(node.start_byte()))
                    .is_some()
                    && environments
                        .iter()
                        .all(|environment| environment.compatible(&local))
                {
                    result.insert(receiver.start_byte());
                }
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        Some(result)
    }

    fn follow(
        &self,
        path: &Path,
        modules: &[String],
        target: &Path,
        crate_root: bool,
        environment: &mut Environment,
    ) -> Option<bool> {
        let source = self.sources.get(path)?;
        let tree = parser::parse_source(&source.bytes, LangId::Rust).ok()?;
        if path == target {
            return Some(modules.is_empty());
        }
        environment.add(tree.root_node(), &source.bytes, None)?;
        let Some((name, remaining)) = modules.split_first() else {
            return Some(false);
        };
        let mut cursor = tree.root_node().walk();
        let declarations: Vec<_> = tree
            .root_node()
            .named_children(&mut cursor)
            .filter(|node| node.kind() == "mod_item")
            .filter(|node| {
                node.child_by_field_name("name")
                    .and_then(|name| name.utf8_text(&source.bytes).ok())
                    .is_some_and(|value| unraw(value) == name)
            })
            .collect();
        if declarations.is_empty() {
            return Some(false);
        }
        if declarations.len() != 1 || declarations[0].child_by_field_name("body").is_some() {
            return None;
        }
        let stem = path.file_stem()?.to_str()?;
        let directory = if crate_root || stem == "mod" {
            path.parent()?.to_owned()
        } else {
            path.with_extension("")
        };
        let candidates = [
            directory.join(format!("{name}.rs")),
            directory.join(name).join("mod.rs"),
        ];
        let present: Vec<_> = candidates
            .iter()
            .filter(|path| self.sources.contains_key(*path))
            .collect();
        if present.len() != 1 {
            return None;
        }
        self.follow(present[0], remaining, target, false, environment)
    }
}

fn module_path(root: &Path, target: &Path) -> Option<Vec<String>> {
    if root == target {
        return Some(Vec::new());
    }
    let relative = target.strip_prefix(root.parent()?).ok()?;
    let mut parts: Vec<String> = relative
        .components()
        .map(|part| part.as_os_str().to_str().map(str::to_owned))
        .collect::<Option<_>>()?;
    let file = parts.pop()?;
    let stem = file.strip_suffix(".rs")?;
    if stem != "mod" {
        parts.push(stem.to_owned());
    }
    parts
        .iter()
        .all(|name| {
            !name.is_empty()
                && name.bytes().enumerate().all(|(index, byte)| {
                    byte == b'_'
                        || byte.is_ascii_alphabetic()
                        || (index > 0 && byte.is_ascii_digit())
                })
        })
        .then_some(parts)
}

fn unraw(name: &str) -> &str {
    name.strip_prefix("r#").unwrap_or(name)
}

fn comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment")
}

// 任意 token の転送と属性生成は、通常の親チェーン以外で同じソースを配置できる。
// 兄弟の未知 proc macro が名前や配置を合成する場合は、既存の葉モジュール証明と同じく対象外。
fn ordinary_placement(
    root: Node<'_>,
    source: &[u8],
    path: &Path,
    paths: &HashSet<PathBuf>,
) -> Option<Vec<PathBuf>> {
    let mut relocated = Vec::new();
    let mut pending = vec![(root, false)];
    while let Some((node, definition)) = pending.pop() {
        if matches!(node.kind(), "attribute" | "token_tree")
            && crate::engine::rust_dynamic_source::node_includes_external_source(node, source)
        {
            // ファイル直下の単純な path だけは、再配置先の部分木を記録できる。
            // inline module・cfg_attr・マクロ引数は解釈を増やさず保留する。
            if node.kind() != "attribute" || node.named_child(0)?.utf8_text(source).ok()? != "path"
            {
                return None;
            }
            let item = node.parent()?;
            if item.kind() != "attribute_item" || item.parent()?.id() != root.id() {
                return None;
            }
            let declaration = item.next_named_sibling()?;
            if declaration.kind() != "mod_item" || declaration.child_by_field_name("body").is_some()
            {
                return None;
            }
            let value = node.child_by_field_name("value")?;
            if value.kind() != "string_literal" {
                return None;
            }
            let raw = value
                .utf8_text(source)
                .ok()?
                .strip_prefix('"')?
                .strip_suffix('"')?;
            if raw.is_empty() || raw.contains('\\') || Path::new(raw).is_absolute() {
                return None;
            }
            let target = path.parent()?.join(raw).canonicalize().ok()?;
            if !paths.contains(&target) {
                return None;
            }
            relocated.push(target.parent()?.to_owned());
        }
        if node.kind() == "macro_definition"
            && node
                .child_by_field_name("name")
                .and_then(|name| name.utf8_text(source).ok())
                .is_some_and(|name| {
                    super::rust_path_owner::is_allowed_expression_macro(unraw(name))
                })
        {
            return None;
        }
        if definition
            && ((node.kind() == "#" && !inert_template_attribute(node, source))
                || (node.kind() == "identifier"
                    && crate::engine::rust_macro_name::has_token_tree_ancestor(node)
                    && node.utf8_text(source).ok().is_some_and(|text| {
                        matches!(unraw(text), "macro_rules" | "macro_rules!" | "macro")
                    }))
                || (node.kind() == "fragment_specifier"
                    && node
                        .utf8_text(source)
                        .ok()
                        .is_some_and(|text| matches!(text, "tt" | "item" | "meta" | "block"))))
        {
            return None;
        }
        let definition = definition || node.kind() == "macro_definition";
        let mut cursor = node.walk();
        pending.extend(
            node.children(&mut cursor)
                .filter(|node| !comment(*node))
                .map(|node| (node, definition)),
        );
    }
    Some(relocated)
}

fn inert_template_attribute(node: Node<'_>, source: &[u8]) -> bool {
    let mut following = node.next_sibling();
    while following.is_some_and(comment) {
        following = following.and_then(|node| node.next_sibling());
    }
    let Some(arguments) = following.filter(|node| node.kind() == "token_tree") else {
        return false;
    };
    if arguments.child(0).is_none_or(|node| node.kind() != "[") {
        return false;
    }
    let Ok(attribute) = std::str::from_utf8(&source[node.start_byte()..arguments.end_byte()])
    else {
        return false;
    };
    let wrapped = format!("{attribute}\nfn __attribute_probe() {{}}");
    let Ok(tree) = parser::parse_source(wrapped.as_bytes(), LangId::Rust) else {
        return false;
    };
    !tree.root_node().has_error()
        && tree
            .root_node()
            .named_child(0)
            .and_then(|attribute| {
                super::rust_std_origin::safe_attribute(attribute, wrapped.as_bytes())
            })
            .is_some_and(|names| names.is_empty())
}

fn assertion_receiver<'a>(node: Node<'a>, source: &[u8]) -> Option<Node<'a>> {
    if node.kind() != "macro_invocation"
        || node.child_by_field_name("macro")?.kind() != "identifier"
        || unraw(node.child_by_field_name("macro")?.utf8_text(source).ok()?) != "assert"
        || crate::engine::rust_macro_name::has_token_tree_ancestor(node)
        || !function_ancestor(node)
    {
        return None;
    }
    let tokens = node.named_child(1)?;
    if tokens.kind() != "token_tree" {
        return None;
    }
    let mut cursor = tokens.walk();
    let mut children: Vec<_> = tokens
        .children(&mut cursor)
        .filter(|node| !comment(*node))
        .collect();
    if children.first()?.kind() != "(" || children.pop()?.kind() != ")" {
        return None;
    }
    children.remove(0);
    if children.first()?.kind() == "!" {
        children.remove(0);
    }
    if children.len() != 4
        || children[0].kind() != "identifier"
        || unraw(children[0].utf8_text(source).ok()?) != "include"
        || children[1].kind() != "."
        || children[2].kind() != "identifier"
        || children[3].kind() != "token_tree"
        || children[3].child(0)?.kind() != "("
    {
        return None;
    }
    let mut pending = vec![children[3]];
    while let Some(argument) = pending.pop() {
        if argument.kind() == "!"
            || argument.kind() == "#"
            || (argument.kind() == "identifier"
                && unraw(argument.utf8_text(source).ok()?) == "include")
        {
            return None;
        }
        let mut cursor = argument.walk();
        pending.extend(
            argument
                .children(&mut cursor)
                .filter(|node| !comment(*node)),
        );
    }
    let condition =
        std::str::from_utf8(&source[children[0].start_byte()..children[3].end_byte()]).ok()?;
    let expression = format!("fn __assert_probe() {{ let _ = {condition}; }}");
    if parser::parse_source(expression.as_bytes(), LangId::Rust)
        .ok()?
        .root_node()
        .has_error()
    {
        return None;
    }
    Some(children[0])
}

fn function_ancestor(mut node: Node<'_>) -> bool {
    while let Some(parent) = node.parent() {
        if parent.kind() == "function_item" {
            return true;
        }
        node = parent;
    }
    false
}

#[derive(Clone, Default)]
struct Environment {
    imports: HashSet<String>,
    standard: HashSet<String>,
}

impl Environment {
    fn add(&mut self, root: Node<'_>, source: &[u8], target: Option<usize>) -> Option<()> {
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            match node.kind() {
                "macro_definition" | "extern_crate_declaration" => return None,
                "attribute_item" | "inner_attribute_item" => {
                    self.standard
                        .extend(super::rust_std_origin::safe_attribute(node, source)?);
                }
                "use_declaration" => {
                    let mut parts = vec![node];
                    while let Some(part) = parts.pop() {
                        if part.kind() == "use_wildcard" {
                            return None;
                        }
                        if part.kind() == "identifier" {
                            self.imports
                                .insert(unraw(part.utf8_text(source).ok()?).to_owned());
                        }
                        let mut cursor = part.walk();
                        parts.extend(part.named_children(&mut cursor));
                    }
                }
                "macro_invocation" => {
                    if !super::rust_cargo_reachability::invokes_only_standard_macros(node, source) {
                        return None;
                    }
                    self.standard.insert(
                        unraw(node.child_by_field_name("macro")?.utf8_text(source).ok()?)
                            .to_owned(),
                    );
                    let mut parts = vec![node];
                    while let Some(part) = parts.pop() {
                        if part.kind() == "identifier" {
                            let name = unraw(part.utf8_text(source).ok()?);
                            if super::rust_path_owner::is_allowed_expression_macro(name) {
                                self.standard.insert(name.to_owned());
                            }
                        }
                        let mut cursor = part.walk();
                        parts.extend(part.named_children(&mut cursor));
                    }
                }
                _ => {}
            }
            if node.id() != root.id()
                && matches!(
                    node.kind(),
                    "function_item"
                        | "function_signature_item"
                        | "mod_item"
                        | "block"
                        | "impl_item"
                        | "trait_item"
                        | "closure_expression"
                        | "const_block"
                )
                && target
                    .is_none_or(|target| target < node.start_byte() || target >= node.end_byte())
            {
                continue;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        Some(())
    }

    fn compatible(&self, local: &Self) -> bool {
        self.standard
            .iter()
            .chain(&local.standard)
            .all(|name| !self.imports.contains(name) && !local.imports.contains(name))
    }
}

#[cfg(test)]
mod tests {
    use super::AssertionProof;
    use std::path::PathBuf;

    fn proof(files: &[(&str, &str)]) -> (tempfile::TempDir, Option<AssertionProof>) {
        let root = tempfile::tempdir().unwrap();
        let mut sources = Vec::<(PathBuf, blake3::Hash)>::new();
        let mut manifests = Vec::new();
        for &(path, source) in files {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, source).unwrap();
            if path.extension().is_some_and(|ext| ext == "rs") {
                sources.push((path, blake3::hash(source.as_bytes())));
            } else {
                manifests.push(path);
            }
        }
        let result = AssertionProof::build(&sources, &manifests);
        (root, result)
    }

    #[test]
    fn standard_assert_receiver_requires_the_complete_expression() {
        let (root, proof) = proof(&[
            ("Cargo.toml", "[package]\nname='sample'\nversion='0.1.0'\n"),
            ("src/lib.rs", "mod config;"),
            ("src/config/mod.rs", "mod paths;"),
            (
                "src/config/paths.rs",
                "#[cfg(test)] mod tests { #[test] fn check() { let include = 1; assert!(include.matches()); assert!(!r#include.matches(Path::new(\"x\"))); } }",
            ),
        ]);
        let path = root.path().join("src/config/paths.rs");
        let offsets = proof
            .unwrap()
            .receiver_offsets(&path, &std::fs::read(&path).unwrap())
            .unwrap();
        assert_eq!(offsets.len(), 2);
    }

    #[test]
    fn unknown_and_shadowed_assertions_keep_their_tokens() {
        for source in [
            "fn check() { load!(include.matches()); }",
            "fn check() { assert!(include.matches(), \"message\"); }",
            "fn check() { assert!(include.matches(other!())); }",
            "fn check() { assert!(include.matches(include)); }",
            "macro_rules! assert { ($($x:tt)*) => {} } fn check() { assert!(include.matches()); }",
            "use other::assert; fn check() { assert!(include.matches()); }",
            "use other::*; fn check() { assert!(include.matches()); }",
            "#[custom] fn check() { assert!(include.matches()); }",
        ] {
            let (root, proof) = proof(&[
                ("Cargo.toml", "[package]\nname='sample'\nversion='0.1.0'\n"),
                ("src/lib.rs", "mod paths;"),
                ("src/paths.rs", source),
            ]);
            assert!(
                proof
                    .and_then(|proof| proof
                        .receiver_offsets(&root.path().join("src/paths.rs"), source.as_bytes()))
                    .is_none_or(|offsets| offsets.is_empty()),
                "{source}"
            );
        }
    }

    #[test]
    fn alternate_ordinary_roots_and_shadowable_attributes_are_checked() {
        for (extra_path, extra) in [
            ("src/main.rs", "use other::assert; mod paths;"),
            ("build.rs", "use other::assert; mod src;"),
            ("src/mod.rs", "use other::assert; mod paths;"),
            ("src/lib.rs", "use other::test; mod paths;"),
            ("src/lib.rs", "use other::Debug; mod paths;"),
        ] {
            let mut files = vec![
                ("Cargo.toml", "[package]\nname='sample'\nversion='0.1.0'\n"),
                ("src/lib.rs", "mod paths;"),
                ("build.rs", "mod src;"),
                ("src/mod.rs", "mod paths;"),
                (
                    "src/paths.rs",
                    "#[derive(Debug)] struct A; #[cfg(test)] mod tests { #[test] fn check() { assert!(include.matches()); } }",
                ),
            ];
            files.retain(|(path, _)| *path != extra_path);
            files.push((extra_path, extra));
            let (root, proof) = proof(&files);
            let path = root.path().join("src/paths.rs");
            assert!(
                proof
                    .unwrap()
                    .receiver_offsets(&path, &std::fs::read(&path).unwrap())
                    .is_none_or(|offsets| offsets.is_empty()),
                "{extra_path}: {extra}"
            );
        }
    }

    #[test]
    fn placement_audit_allows_simple_module_generation_and_rejects_relocation() {
        for (extra, allowed) in [
            (
                "macro_rules! department { ($($module:ident),+) => { $(mod $module;)+ }; }",
                true,
            ),
            (
                "macro_rules! fields { ($($field:ident => $name:literal),+) => { #[allow(non_snake_case)] struct Fields; #[inline] fn field_id() {} }; }",
                true,
            ),
            (
                "macro_rules! derive { () => { #[derive(Debug)] struct Fields; }; }",
                false,
            ),
            (
                "macro_rules! forward { ($($input:tt)*) => { $($input)* }; }",
                false,
            ),
            (
                "macro_rules! factory { ($name:ident) => { macro_rules! $name { () => {} } }; }",
                false,
            ),
            (
                "macro_rules! factory { ($name:ident) => { macro $name() {} }; }",
                false,
            ),
            (
                "macro_rules! attr { ($name:ident) => { #[$name=\"paths.rs\"] mod reused; }; }",
                false,
            ),
            ("load! { #[path=\"paths.rs\"] mod reused; }", false),
            ("#[path=\"paths.rs\"] mod reused;", false),
        ] {
            let (root, proof) = proof(&[
                ("Cargo.toml", "[package]\nname='sample'\nversion='0.1.0'\n"),
                ("src/lib.rs", "mod paths;"),
                ("src/paths.rs", "fn check() { assert!(include.matches()); }"),
                ("src/other.rs", extra),
            ]);
            let path = root.path().join("src/paths.rs");
            assert_eq!(
                proof
                    .and_then(|proof| proof.receiver_offsets(&path, &std::fs::read(&path).unwrap()))
                    .is_some_and(|offsets| !offsets.is_empty()),
                allowed,
                "{extra}"
            );
        }
    }

    #[test]
    fn receiver_offsets_require_the_audited_source_bytes() {
        let (root, proof) = proof(&[
            ("Cargo.toml", "[package]\nname='sample'\nversion='0.1.0'\n"),
            ("src/lib.rs", "mod paths;"),
            ("src/paths.rs", "fn check() { assert!(include.matches()); }"),
        ]);
        assert!(
            proof
                .unwrap()
                .receiver_offsets(&root.path().join("src/paths.rs"), b"different")
                .is_none()
        );
    }

    #[test]
    fn exported_standard_macro_cannot_generate_a_shadowing_assertion() {
        let source = "fn check() { println!(); assert!(include.matches()); }";
        let (root, proof) = proof(&[
            ("Cargo.toml", "[package]\nname='sample'\nversion='0.1.0'\n"),
            ("src/lib.rs", "mod other; mod paths;"),
            ("src/paths.rs", source),
            (
                "src/other.rs",
                "#[macro_export] macro_rules! println { () => { macro_rules! assert { ($head:ident.$method:ident()) => { $head!(\"extra.rs\") }; } }; }",
            ),
        ]);
        assert!(
            proof
                .and_then(|proof| proof
                    .receiver_offsets(&root.path().join("src/paths.rs"), source.as_bytes()))
                .is_none_or(|offsets| offsets.is_empty())
        );
    }
}
