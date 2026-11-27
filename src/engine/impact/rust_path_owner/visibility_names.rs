//! 可視性だけを広げた自由関数が、参照位置の非修飾名に入る経路を確かめる。

use super::*;

#[derive(Default)]
pub(super) struct Imports {
    module: std::cell::OnceCell<Option<format_owner::Environment>>,
    blocks: RefCell<HashMap<usize, Option<format_owner::Environment>>>,
}

impl RustPathOwnerIndex {
    pub(in crate::engine::impact) fn outside_widened_binding(
        &self,
        ancestors: &[Node<'_>],
        lexical: bool,
        source: &[u8],
        (changed, name): (&Path, &str),
        file: &Path,
        project: &RustPathOwnerProjectProof,
    ) -> bool {
        let Some((&root, scopes)) = ancestors.split_first() else {
            return false;
        };
        if root.kind() != "source_file" || root.has_error() {
            return false;
        }
        for &scope in scopes.iter().rev() {
            match scope.kind() {
                "mod_item"
                | "macro_definition"
                | "token_tree"
                | "attribute_item"
                | "use_declaration"
                | "scoped_identifier"
                | "scoped_type_identifier" => return false,
                "block" => {
                    let mut blocks = self.visibility_imports.blocks.borrow_mut();
                    let imports = blocks.entry(scope.id()).or_insert_with(|| {
                        if format_owner::has_parent_glob(scope) {
                            return None;
                        }
                        format_owner::environment(scope, source, None, true)
                    });
                    if !imports.as_ref().is_some_and(|env| env.plain_block(name)) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        let Some(imports) = self
            .visibility_imports
            .module
            .get_or_init(|| module_imports(root, source, file, changed, project))
        else {
            return lexical
                && project.outside_parent(changed, file)
                && file.to_str().is_some_and(|path| {
                    self.standard_macro_base(&project.root, path, project)
                        .is_some_and(|env| env.closed_imports() && env.plain(name))
                });
        };
        if imports.plain(name) {
            return lexical;
        }
        !self.rebinds(root, Rebind::WidenedFunction, name, || {
            let path = ["self".to_owned(), name.to_owned()];
            !matches!(super::super::rust_reexport_owner::declared_call_owner(project, file, &path),
                Some(QualifiedCallOwner::Function(other)) if other != changed)
        })
    }
}

fn module_imports(
    root: Node<'_>,
    source: &[u8],
    file: &Path,
    changed: &Path,
    project: &RustPathOwnerProjectProof,
) -> Option<format_owner::Environment> {
    if !project.callable_layout_stable()
        || !project.unrelocated(file)
        || !project.unrelocated(changed)
    {
        return None;
    }
    let reader = project.ordinary_private_module(file)?;
    let producer = project.ordinary_private_module(changed)?;
    if reader.src != producer.src
        || reader.path.is_empty()
        || producer.path.is_empty()
        || project
            .declared_module_file(&reader.src, &reader.path)?
            .as_path()
            != file
        || project
            .declared_module_file(&producer.src, &producer.path)?
            .as_path()
            != changed
    {
        return None;
    }
    let mut combined = format_owner::Environment::default();
    let mut check = |root: Node<'_>, bytes: &[u8], path: &Path| {
        let (env, glob) = format_owner::visibility_environment(root, bytes, path, project)?;
        if !env.closed_imports() || glob && !external_glob(root, bytes, &reader, project) {
            return None;
        }
        combined.merge(env);
        Some(format_owner::has_parent_glob(root))
    };
    let mut parent_glob = check(root, source, file)?;
    let mut module = reader.path.as_slice();
    while parent_glob {
        module = module.split_last()?.1;
        let parent = project.declared_module_file(&reader.src, module)?;
        if parent == changed || !project.unrelocated(&parent) {
            return None;
        }
        let bytes = read_input_file(&project.root.join(&parent))?;
        let tree = parser::parse_source(&bytes, LangId::Rust).ok()?;
        parent_glob = check(tree.root_node(), &bytes, &parent)?;
    }
    combined.consistent().then_some(combined)
}

/// 外部クレートは、このクレートの pub(super) 関数を再公開できない。
fn external_glob(
    root: Node<'_>,
    bytes: &[u8],
    reader: &PrivateModule,
    project: &RustPathOwnerProjectProof,
) -> bool {
    let prove = || {
        let package = reader.src.parent()?;
        if !reader.library_only
            || !project.edition_2018_or_later(package)?
            || !project
                .alias_cache
                .pointer_prelude(project, &reader.src.join("lib.rs"))
        {
            return None;
        }
        let names = super::super::rust_owner_alias::Names::collect(root, bytes, &reader.path)?;
        let [(crate::engine::rust_use_tree::UsePathOrigin::Bare, path)] = names.globs.as_slice()
        else {
            return None;
        };
        let first = path.first()?;
        if names.items.contains(first) || names.imports.contains_key(first) {
            return None;
        }
        let manifest = project.alias_cache.manifest(&project.root, package)?;
        manifest.get("dependencies")?.get(first).map(|_| ())
    };
    prove().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visibility_imports_check_the_sources_of_data_derives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "mod reader;\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname='reports'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nserde='1.0.229'\nschemars='1.2.3'\n").unwrap();
        let file = Path::new("src/reader.rs");
        for (version, extra, known) in [
            ("1.2.3", "", true),
            ("1.2.4", "", false),
            ("1.2.3", "use crate::custom as schemars;", false),
        ] {
            let mut lock = "version=4\n".to_owned();
            for (name, version, derive) in [
                ("serde", "1.0.229", Some("serde_derive")),
                ("serde_derive", "1.0.229", None),
                ("schemars", version, Some("schemars_derive")),
                ("schemars_derive", version, None),
            ] {
                lock.push_str(&format!("[[package]]\nname='{name}'\nversion='{version}'\nsource='registry+https://github.com/rust-lang/crates.io-index'\nchecksum='{}'\n", "a".repeat(64)));
                if let Some(derive) = derive {
                    lock.push_str(&format!("dependencies=['{derive}']\n"));
                }
            }
            std::fs::write(root.join("Cargo.lock"), lock).unwrap();
            let source = format!(
                "use serde::{{Serialize, Deserialize}};\n{extra}\n#[derive(Serialize, Deserialize, schemars::JsonSchema)]\nstruct Row;\n"
            );
            std::fs::write(root.join(file), &source).unwrap();
            let project = RustPathOwnerProjectProof::new(root, true, true, true, &[], "");
            let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
            let environment = format_owner::visibility_environment(
                tree.root_node(),
                source.as_bytes(),
                file,
                &project,
            );
            assert_eq!(
                environment.is_some_and(|(env, glob)| !glob
                    && env.closed_imports()
                    && env.plain("record")),
                known,
                "{version} {extra}"
            );
        }
    }
}
