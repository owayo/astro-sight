//! 明示した enum と crate 起点の直接 import から、variant の所有元だけを確認する。

use super::*;
use crate::engine::rust_use_tree::{RustPubUseEdge, UsePathOrigin, expand_rust_use_tree_edges_ast};

pub(super) fn static_layout(
    root: &Path,
    project: &RustPathOwnerProjectProof,
) -> Option<Vec<PathBuf>> {
    let root = root.canonicalize().ok()?;
    if root
        .ancestors()
        .skip(1)
        .any(|path| path.join("Cargo.toml").exists() || path.join(".git").exists())
    {
        return None;
    }
    // snapshot が書き出さなかった要素も、下の走査が実際の checkout の symlink に当てる規則で扱う
    // (通常のファイルを指し、`.rs` でも `Cargo.toml` でもない symlink だけを許す)。root の直下の
    // `.git`・`target` のディレクトリの中は読まない (Issue #376)。
    let omitted_allowed = |path: &Path, kind: &crate::git_support::OmittedKind| {
        if super::under_root_git_or_target(path)
            && (path.components().count() > 1
                || matches!(kind, crate::git_support::OmittedKind::Gitlink))
        {
            return true;
        }
        matches!(
            kind,
            crate::git_support::OmittedKind::Symlink(crate::git_support::LinkTarget::RegularFile)
        ) && !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
            && !path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
    };
    if !crate::git_support::tree_view(&root)
        .omitted()
        .is_some_and(|omitted| {
            omitted
                .iter()
                .all(|(path, kind)| omitted_allowed(path, kind))
        })
    {
        return None;
    }
    let mut pending = vec![root.clone()];
    let mut relocated = Vec::new();
    let mut candidates = Vec::new();
    let mut candidate_bytes = 0usize;
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).ok()? {
            let entry = entry.ok()?;
            let kind = entry.file_type().ok()?;
            let file = entry.path();
            let entry_name = entry.file_name();
            if kind.is_symlink() {
                if std::fs::metadata(&file).ok()?.is_file()
                    && !file
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
                    && !entry_name
                        .to_str()
                        .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
                {
                    continue;
                }
                return None;
            }
            if kind.is_dir() {
                if directory != root || !matches!(entry_name.to_str(), Some(".git" | "target")) {
                    pending.push(file);
                }
            } else if kind.is_file()
                && entry_name
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case("Cargo.toml"))
            {
                let manifest = read_input_text_file(&file)?;
                let targets = RustTargets::parse(&manifest, directory.strip_prefix(&root).ok()?)?;
                if !targets.roots_complete {
                    return None;
                }
                for target in targets.explicit_roots() {
                    if target
                        .extension()
                        .is_none_or(|extension| !extension.eq_ignore_ascii_case("rs"))
                    {
                        return None;
                    }
                    let target = root.join(target).canonicalize().ok()?;
                    if !target.starts_with(&root) {
                        return None;
                    }
                    relocated.push(target.parent()?.to_owned());
                }
            } else if kind.is_file()
                && file
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
            {
                let source = read_input_file(&file)?;
                if memchr::memmem::find(&source, b"path").is_none()
                    && memchr::memmem::find(&source, b"include").is_none()
                {
                    continue;
                }
                let tree = parser::parse_source(&source, LangId::Rust).ok()?;
                if tree.root_node().has_error() {
                    return None;
                }
                let mut offsets = Vec::new();
                let result = crate::engine::tree_walk::walk_preorder(
                    tree.root_node(),
                    crate::engine::tree_walk::Children::All,
                    |node, _| {
                        if matches!(node.kind(), "macro_invocation" | "use_declaration")
                            && crate::engine::rust_dynamic_source::node_includes_external_source(
                                node, &source,
                            )
                        {
                            return std::ops::ControlFlow::Break(());
                        }
                        if matches!(node.kind(), "attribute" | "token_tree")
                            && crate::engine::rust_dynamic_source::node_includes_external_source(
                                node, &source,
                            )
                        {
                            let target = if node.kind() == "attribute" {
                                path_target(node, &source, &file)
                            } else {
                                None
                            };
                            let Some(target) = target.filter(|target| target.starts_with(&root))
                            else {
                                return std::ops::ControlFlow::Break(());
                            };
                            let Some(parent) = target.parent() else {
                                return std::ops::ControlFlow::Break(());
                            };
                            relocated.push(target.clone());
                            relocated.push(
                                if target.file_name().is_some_and(|name| name == "mod.rs") {
                                    parent.to_owned()
                                } else {
                                    target.with_extension("")
                                },
                            );
                        }
                        if node.kind() == "identifier"
                            && name(node, &source) == Some("include")
                            && has_token_tree_ancestor(node)
                            && !is_field_name_in_token_tree(node, &source)
                        {
                            offsets.push(node.start_byte());
                        }
                        std::ops::ControlFlow::Continue(crate::engine::tree_walk::Walk::Descend)
                    },
                );
                if result.is_break() {
                    return None;
                }
                if !offsets.is_empty() {
                    candidate_bytes = candidate_bytes.checked_add(source.len())?;
                    if candidate_bytes > 64 * 1024 * 1024 {
                        return None;
                    }
                    candidates.push((file, source, offsets));
                }
            }
        }
    }
    relocated.sort();
    relocated.dedup();
    for (file, source, offsets) in candidates {
        let tree = parser::parse_source(&source, LangId::Rust).ok()?;
        let relative = file.strip_prefix(&root).ok()?.to_str()?;
        for offset in offsets {
            let node = tree
                .root_node()
                .descendant_for_byte_range(offset, offset + "include".len())?;
            if !super::format_owner::stock_expression(
                node, &source, &root, relative, project, &relocated,
            ) {
                return None;
            }
        }
    }
    Some(relocated)
}

fn path_target(attribute: Node<'_>, source: &[u8], file: &Path) -> Option<PathBuf> {
    if attribute_name(attribute, source) != Some("path") {
        return None;
    }
    let item = attribute.parent()?;
    if item.kind() != "attribute_item" || item.parent()?.kind() != "source_file" {
        return None;
    }
    let declaration = item.next_named_sibling()?;
    if declaration.kind() != "mod_item" || declaration.child_by_field_name("body").is_some() {
        return None;
    }
    let raw = attribute
        .child_by_field_name("value")?
        .utf8_text(source)
        .ok()?
        .strip_prefix('"')?
        .strip_suffix('"')?;
    if raw.is_empty() || raw.contains('\\') || Path::new(raw).is_absolute() {
        return None;
    }
    file.parent()?.join(raw).canonicalize().ok()
}

pub(super) struct EnumFile {
    variants: HashMap<String, HashSet<String>>,
}

fn name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let name = node.utf8_text(source).ok()?;
    Some(name.strip_prefix("r#").unwrap_or(name))
}

fn attribute_name<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let attribute = if node.kind() == "attribute" {
        node
    } else {
        node.named_child(0)?
    };
    name(attribute.named_child(0)?, source)
}

fn stable_item(item: Node<'_>, source: &[u8]) -> bool {
    let mut previous = item.prev_named_sibling();
    while let Some(node) = previous {
        match node.kind() {
            "line_comment" | "block_comment" => {}
            "attribute_item" => {
                let attribute = attribute_name(node, source);
                if matches!(attribute, Some("cfg" | "cfg_attr"))
                    || (attribute != Some("derive")
                        && super::super::rust_std_origin::safe_attribute(node, source).is_none())
                {
                    return false;
                }
            }
            _ => break,
        }
        previous = node.prev_named_sibling();
    }
    true
}

fn root_is_plain(root: Node<'_>, source: &[u8]) -> bool {
    if root.has_error() {
        return false;
    }
    let mut cursor = root.walk();
    !root.named_children(&mut cursor).any(|node| {
        node.kind() == "inner_attribute_item"
            && (matches!(attribute_name(node, source), Some("cfg" | "cfg_attr"))
                || super::super::rust_std_origin::safe_attribute(node, source).is_none())
    })
}

impl EnumFile {
    fn read(source: &[u8]) -> Option<Self> {
        let tree = parser::parse_source(source, LangId::Rust).ok()?;
        let root = tree.root_node();
        if !root_is_plain(root, source) {
            return None;
        }
        let mut variants = HashMap::new();
        let mut ambiguous = HashSet::new();
        let mut cursor = root.walk();
        for item in root.named_children(&mut cursor) {
            let Some(identifier) = item
                .child_by_field_name("name")
                .and_then(|node| name(node, source))
            else {
                continue;
            };
            if item.kind() != "enum_item" || !stable_item(item, source) {
                ambiguous.insert(identifier.to_owned());
                continue;
            }
            let body = item.child_by_field_name("body")?;
            let mut names = HashSet::new();
            let mut cursor = body.walk();
            for variant in body.named_children(&mut cursor) {
                if variant.kind() != "enum_variant" {
                    continue;
                }
                // variant の属性は helper 属性だが、cfg で消える形は証明しない。
                let mut previous = variant.prev_named_sibling();
                while let Some(node) = previous {
                    if node.kind() == "attribute_item" {
                        if matches!(attribute_name(node, source), Some("cfg" | "cfg_attr")) {
                            return None;
                        }
                    } else if !matches!(node.kind(), "line_comment" | "block_comment") {
                        break;
                    }
                    previous = node.prev_named_sibling();
                }
                if !names.insert(name(variant.child_by_field_name("name")?, source)?.to_owned()) {
                    return None;
                }
            }
            if variants.insert(identifier.to_owned(), names).is_some() {
                ambiguous.insert(identifier.to_owned());
            }
        }
        for name in ambiguous {
            variants.remove(&name);
        }
        Some(Self { variants })
    }
}

fn ordinary_file(dir: &Path, path: &Path) -> Option<bool> {
    // snapshot が書き出さなかった要素 (やその下) は、無いと決められない (Issue #376)。
    if crate::git_support::tree_view(dir).hides(path) {
        return None;
    }
    match std::fs::symlink_metadata(dir.join(path)) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => dir
            .join(path)
            .canonicalize()
            .ok()?
            .starts_with(dir.canonicalize().ok()?)
            .then_some(true),
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

fn ordinary_module_file(dir: &Path, src: &Path, modules: &[String]) -> Option<PathBuf> {
    let mut found = None;
    for root in [src.join("lib.rs"), src.join("main.rs")] {
        if !ordinary_file(dir, &root)? {
            continue;
        }
        let mut file = root;
        let mut prefix = src.to_owned();
        let mut reachable = true;
        for module in modules {
            let source = read_input_file(&dir.join(&file))?;
            let tree = parser::parse_source(&source, LangId::Rust).ok()?;
            let root = tree.root_node();
            if !root_is_plain(root, &source) {
                return None;
            }
            let mut cursor = root.walk();
            let declarations = root
                .named_children(&mut cursor)
                .filter(|node| {
                    node.kind() == "mod_item"
                        && node
                            .child_by_field_name("name")
                            .and_then(|node| name(node, &source))
                            == Some(module.as_str())
                })
                .collect::<Vec<_>>();
            if declarations.is_empty() {
                reachable = false;
                break;
            }
            let [declaration] = declarations.as_slice() else {
                return None;
            };
            if has_attribute_prefix(*declaration)
                || declaration.child_by_field_name("body").is_some()
            {
                return None;
            }
            let flat = prefix.join(format!("{module}.rs"));
            let nested = prefix.join(module).join("mod.rs");
            file = match (ordinary_file(dir, &flat)?, ordinary_file(dir, &nested)?) {
                (true, false) => flat,
                (false, true) => nested,
                _ => return None,
            };
            prefix.push(module);
        }
        if reachable {
            if found.as_ref().is_some_and(|previous| previous != &file) {
                return None;
            }
            found = Some(file);
        }
    }
    found
}

fn crate_anchored(mut node: Node<'_>) -> bool {
    loop {
        match node.kind() {
            "crate" => return true,
            "scoped_identifier" | "scoped_use_list" | "use_as_clause" => {
                let Some(path) = node.child_by_field_name("path") else {
                    return false;
                };
                node = path;
            }
            _ => return false,
        }
    }
}

impl RustPathOwnerIndex {
    pub(in crate::engine::impact) fn enum_variant_owner(
        &self,
        node: Node<'_>,
        source: &[u8],
        dir: &Path,
        ref_path: &str,
        project: &RustPathOwnerProjectProof,
    ) -> Option<PathBuf> {
        if !self.same_source(source) || has_uncertain_ancestor(node) {
            return None;
        }
        let parent = node.parent()?;
        if parent.kind() == "enum_variant"
            && parent.child_by_field_name("name").map(|node| node.id()) == Some(node.id())
        {
            let declaration = parent.parent()?.parent()?;
            if declaration.kind() != "enum_item" || !stable_item(declaration, source) {
                return None;
            }
            let enum_name = name(declaration.child_by_field_name("name")?, source)?;
            let variant = name(node, source)?;
            return EnumFile::read(source)?
                .variants
                .get(enum_name)?
                .contains(variant)
                .then(|| PathBuf::from(ref_path));
        }
        if parent.kind() != "scoped_identifier"
            || parent.child_by_field_name("name").map(|node| node.id()) != Some(node.id())
            || !self.enum_variant_paths.contains(&parent.id())
        {
            return None;
        }
        let segments = path_segments(parent, source)?;
        let [base, variant] = segments.as_slice() else {
            return None;
        };
        let key = (base.clone(), variant.clone());
        if let Some(owner) = self.enum_owners.borrow().get(&key) {
            return owner.clone();
        }
        let result = (|| {
            let mut root = node;
            while let Some(parent) = root.parent() {
                root = parent;
            }
            let base = base.strip_prefix("r#").unwrap_or(base);
            let mut local = None;
            let mut imported = None;
            let mut cursor = root.walk();
            for item in root.named_children(&mut cursor) {
                if item
                    .child_by_field_name("name")
                    .and_then(|node| name(node, source))
                    == Some(base)
                    && local.replace(item).is_some()
                {
                    return None;
                }
                if item.kind() != "use_declaration" {
                    continue;
                }
                let names = root_use_names(item, source)?;
                if !names.iter().any(|name| name == base) {
                    continue;
                }
                if has_attribute_prefix(item) || imported.is_some() {
                    return None;
                }
                if !crate_anchored(item.child_by_field_name("argument")?) {
                    return None;
                }
                let mut edges = Vec::new();
                expand_rust_use_tree_edges_ast(
                    item.child_by_field_name("argument")?,
                    source,
                    &[],
                    &[],
                    &[],
                    None,
                    &mut edges,
                )?;
                for edge in edges {
                    if let RustPubUseEdge::Named {
                        exported_name,
                        target_module,
                        target_item,
                        origin: UsePathOrigin::Anchored,
                        ..
                    } = edge
                        && exported_name == base
                    {
                        imported = Some((target_module, target_item));
                    }
                }
                imported.as_ref()?;
            }
            let (file, enum_name) = match (local, imported) {
                (Some(local), None)
                    if local.kind() == "enum_item" && stable_item(local, source) =>
                {
                    (PathBuf::from(ref_path), base.to_owned())
                }
                (None, Some((modules, item))) => {
                    if !project.allows_static_source_path(Path::new(ref_path)) {
                        return None;
                    }
                    let (_, src, current) = source_module_path(dir, Path::new(ref_path))?;
                    if ordinary_module_file(dir, &src, &current)? != Path::new(ref_path) {
                        return None;
                    }
                    (ordinary_module_file(dir, &src, &modules)?, item)
                }
                _ => return None,
            };
            if file != Path::new(ref_path) && !project.allows_static_source_path(&file) {
                return None;
            }
            if !self.enum_files.borrow().contains_key(&file) {
                let parsed =
                    read_input_file(&dir.join(&file)).and_then(|source| EnumFile::read(&source));
                self.enum_files.borrow_mut().insert(file.clone(), parsed);
            }
            self.enum_files
                .borrow()
                .get(&file)?
                .as_ref()?
                .variants
                .get(&enum_name)?
                .contains(variant)
                .then_some(file)
        })();
        self.enum_owners.borrow_mut().insert(key, result.clone());
        result
    }
}

/// 参照ファイルから親へ順に並べた module のファイルと、crate root のファイル。参照ファイルが
/// crate root のときは `modules` は参照ファイルだけで、`roots` からは除く。
pub(super) fn ancestor_chain(dir: &Path, ref_path: &str) -> Option<(Vec<PathBuf>, Vec<PathBuf>)> {
    let (_, src, modules) = source_module_path(dir, Path::new(ref_path))?;
    if ordinary_module_file(dir, &src, &modules)? != Path::new(ref_path) {
        return None;
    }
    let mut chain = vec![PathBuf::from(ref_path)];
    for count in (1..modules.len()).rev() {
        chain.push(ordinary_module_file(dir, &src, &modules[..count])?);
    }
    let mut roots = Vec::new();
    for root in [src.join("lib.rs"), src.join("main.rs")] {
        if ordinary_file(dir, &root)? && !chain.contains(&root) {
            roots.push(root);
        }
    }
    Some((chain, roots))
}

pub(super) fn ancestor_files(dir: &Path, ref_path: &str) -> Option<Vec<PathBuf>> {
    let (_, src, modules) = source_module_path(dir, Path::new(ref_path))?;
    if ordinary_module_file(dir, &src, &modules)? != Path::new(ref_path) {
        return None;
    }
    let mut files = vec![PathBuf::from(ref_path)];
    for count in 1..modules.len() {
        files.push(ordinary_module_file(dir, &src, &modules[..count])?);
    }
    for root in [src.join("lib.rs"), src.join("main.rs")] {
        if ordinary_file(dir, &root)? {
            files.push(root);
        }
    }
    files.sort();
    files.dedup();
    Some(files)
}
