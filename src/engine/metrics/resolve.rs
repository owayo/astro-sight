use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::path::Path;

use tree_sitter::Node;

use crate::engine::rust_use_tree::{RustPubUseEdge, UsePathOrigin, expand_rust_use_tree_edges_ast};
use crate::engine::tree_walk::{Children, Walk, walk_preorder};
use crate::language::LangId;
use crate::models::metrics::MetricsDiagnostic;

pub(super) struct ResolutionIndex {
    pub rust: super::rust_modules::RustModules,
    pub javascript: super::js_config::JsConfigs,
}

pub(super) struct Dependency {
    pub source: String,
    pub line: usize,
    pub tiers: Vec<Vec<String>>,
    pub kind: String,
    pub rule: &'static str,
    pub failure: &'static str,
}

impl Dependency {
    pub fn resolve(&self, paths: &BTreeMap<String, usize>) -> Result<usize, &'static str> {
        for tier in &self.tiers {
            let matches: BTreeSet<_> = tier.iter().filter_map(|p| paths.get(p).copied()).collect();
            if matches.len() > 1 {
                return Err("ambiguous");
            }
            if let Some(&id) = matches.first() {
                return Ok(id);
            }
        }
        Err(self.failure)
    }
}

pub(super) fn extract(
    root: Node<'_>,
    source: &[u8],
    lang: LangId,
    path: &str,
    paths: &BTreeMap<String, usize>,
    index: &ResolutionIndex,
    diagnostics: &mut Vec<MetricsDiagnostic>,
) -> anyhow::Result<Vec<Dependency>> {
    if lang == LangId::Rust {
        return Ok(rust_dependencies(
            root,
            source,
            path,
            &index.rust,
            diagnostics,
        ));
    }
    let supported = match lang {
        LangId::Javascript
        | LangId::Typescript
        | LangId::Tsx
        | LangId::Python
        | LangId::C
        | LangId::Cpp
        | LangId::Ruby
        | LangId::Bash
        | LangId::Zig => true,
        LangId::Go
        | LangId::Java
        | LangId::Kotlin
        | LangId::Swift
        | LangId::CSharp
        | LangId::Php
        | LangId::Xojo => false,
        LangId::Rust => unreachable!(),
    };
    if !supported {
        diagnostics.push(diagnostic(path, None, "dependency_resolution_unsupported",
            format!("Local dependency resolution is not implemented for {lang}; complexity is still measured")));
        return Ok(crate::engine::imports::extract_imports(root, source, lang)?
            .into_iter()
            .map(|import| Dependency {
                source: import.source,
                line: import.line,
                tiers: Vec::new(),
                kind: format!("{:?}", import.kind).to_lowercase(),
                rule: "unsupported_language",
                failure: "unsupported_language",
            })
            .collect());
    }
    let mut imports = crate::engine::imports::extract_imports(root, source, lang)?;
    // 再 export は既存 imports の出力を変えず、このグラフの辺として加える。
    let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
        if matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx)
            && node.kind() == "call_expression"
            && let Some(function) = node.child_by_field_name("function")
            && matches!(text(function, source), "import" | "require")
            && let Some(arguments) = node.child_by_field_name("arguments")
        {
            let mut cursor = arguments.walk();
            let argument = arguments
                .named_children(&mut cursor)
                .find(|child| child.kind() != "comment");
            let literal = argument.is_some_and(|arg| {
                arg.kind() == "string"
                    || (text(function, source) == "import"
                        && arg.kind() == "template_string"
                        && !text(arg, source).contains("${"))
            });
            if !literal {
                diagnostics.push(diagnostic(
                    path,
                    Some(node.start_position().row),
                    "dynamic_import",
                    "Import target is not a static string; no dependency edge was inferred".into(),
                ));
            }
        }
        if matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx)
            && node.kind() == "export_statement"
            && let Some(target) = node.child_by_field_name("source")
        {
            imports.push(crate::models::import::ImportEdge {
                source: text(target, source).trim_matches(['\'', '"']).to_string(),
                line: node.start_position().row,
                kind: crate::models::import::ImportKind::Import,
                context: text(node, source).to_string(),
            });
        }
        ControlFlow::Continue(Walk::Descend)
    });
    let parent = parent(path);
    let mut dependencies = Vec::new();
    for import in imports {
        let target = import.source.as_str();
        let javascript = matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx);
        let (js_tiers, js_rule, js_failure) = if javascript {
            index.javascript.candidates(path, target, lang)
        } else {
            (Vec::new(), "static_local_import", "external_or_unresolved")
        };
        let tiers = match lang {
            LangId::Javascript | LangId::Typescript | LangId::Tsx => js_tiers,
            LangId::Python => python_candidates(&parent, target),
            LangId::C | LangId::Cpp => {
                if import.context.contains('"') && !target.contains(['\n', '$']) {
                    vec![vec![join(&parent, target)], vec![normalize(target)]]
                } else {
                    Vec::new()
                }
            }
            LangId::Ruby => {
                if import.context.trim_start().starts_with("require_relative")
                    && !target.contains("#{")
                {
                    let base = join(&parent, target);
                    vec![vec![base.clone(), format!("{base}.rb")]]
                } else {
                    Vec::new()
                }
            }
            LangId::Bash => {
                // source の相対パスはスクリプトの場所ではなく実行時の cwd に依存する。
                Vec::new()
            }
            LangId::Zig => {
                if target.ends_with(".zig") {
                    vec![vec![join(&parent, target)]]
                } else {
                    Vec::new()
                }
            }
            LangId::Rust
            | LangId::Go
            | LangId::Java
            | LangId::Kotlin
            | LangId::Swift
            | LangId::CSharp
            | LangId::Php
            | LangId::Xojo => unreachable!(),
        };
        dependencies.push(Dependency {
            source: import.source,
            line: import.line,
            tiers,
            kind: if import.context.trim_start().starts_with("export") {
                "reexport".into()
            } else {
                format!("{:?}", import.kind).to_lowercase()
            },
            rule: js_rule,
            failure: js_failure,
        });
    }
    // `from . import child` の child は package initializer と別の依存先になり得る。
    if lang == LangId::Python {
        let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, _| {
            if node.kind() == "import_from_statement"
                && let Some(module) = node.child_by_field_name("module_name")
            {
                let module = text(module, source);
                let mut cursor = node.walk();
                for child in node.children_by_field_name("name", &mut cursor) {
                    let name = child.child_by_field_name("name").unwrap_or(child);
                    let name = text(name, source);
                    let target = if module.ends_with('.') {
                        format!("{module}{name}")
                    } else {
                        format!("{module}.{name}")
                    };
                    let tiers = python_candidates(&parent, &target);
                    // from の属性 import をモジュールの不明依存として二重申告しない。
                    if tiers.iter().flatten().any(|p| paths.contains_key(p)) {
                        dependencies.push(Dependency {
                            source: target,
                            line: node.start_position().row,
                            tiers,
                            kind: "import".into(),
                            rule: "python_module",
                            failure: "external_or_unresolved",
                        });
                    }
                }
            }
            ControlFlow::Continue(Walk::Descend)
        });
    }
    Ok(dependencies)
}

pub(super) fn js_candidates(base: &str, lang: LangId) -> Vec<Vec<String>> {
    let exact = vec![base.to_string()];
    let typescript = matches!(lang, LangId::Typescript | LangId::Tsx);
    if typescript && let Some(stem) = base.strip_suffix(".js") {
        return vec![vec![format!("{stem}.ts"), format!("{stem}.tsx")], exact];
    }
    if typescript && let Some(stem) = base.strip_suffix(".mjs") {
        return vec![vec![format!("{stem}.mts")], exact];
    }
    if typescript && let Some(stem) = base.strip_suffix(".cjs") {
        return vec![vec![format!("{stem}.cts")], exact];
    }
    if Path::new(base).extension().is_some() {
        return vec![exact];
    }
    let extensions = ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
    vec![
        exact,
        extensions
            .iter()
            .map(|ext| format!("{base}.{ext}"))
            .collect(),
        extensions
            .iter()
            .map(|ext| format!("{base}/index.{ext}"))
            .collect(),
    ]
}

fn python_candidates(parent: &str, target: &str) -> Vec<Vec<String>> {
    let level = target.bytes().take_while(|&c| c == b'.').count();
    let module = target[level..].replace('.', "/");
    let bases = if level == 0 {
        vec![module.clone(), join("src", &module)]
    } else {
        let mut base = parent.to_string();
        for _ in 1..level {
            if base.is_empty() {
                return Vec::new();
            }
            base = self::parent(&base);
        }
        vec![join(&base, &module)]
    };
    vec![
        bases
            .into_iter()
            .flat_map(|base| [format!("{base}.py"), join(&base, "__init__.py")])
            .collect(),
    ]
}

fn rust_dependencies(
    root: Node<'_>,
    source: &[u8],
    path: &str,
    index: &super::rust_modules::RustModules,
    diagnostics: &mut Vec<MetricsDiagnostic>,
) -> Vec<Dependency> {
    let context = index.context(path);
    let mut module = context
        .map(|context| context.module.clone())
        .unwrap_or_default();
    let mut scopes = Vec::new();
    let mut lexical_scopes = Vec::new();
    let mut out = Vec::new();
    let _: ControlFlow<()> = walk_preorder(root, Children::Named, |node, depth| {
        while lexical_scopes.last().is_some_and(|&d| d >= depth) {
            lexical_scopes.pop();
        }
        if node.kind() == "block" {
            lexical_scopes.push(depth);
        }
        while scopes.last().is_some_and(|&(d, _)| d >= depth) {
            let (_, len) = scopes.pop().unwrap();
            module.truncate(len);
        }
        if node.kind() == "mod_item"
            && let Some(name) = node.child_by_field_name("name")
        {
            let name = text(name, source)
                .strip_prefix("r#")
                .unwrap_or(text(name, source));
            if node.child_by_field_name("body").is_some() {
                scopes.push((depth, module.len()));
                module.push(name.to_string());
            } else {
                let mut target = module.clone();
                target.push(name.to_string());
                let targets = context
                    .filter(|_| lexical_scopes.is_empty())
                    .map(|context| index.module(context, &target))
                    .unwrap_or_default();
                out.push(Dependency {
                    source: format!("mod {name}"),
                    line: node.start_position().row,
                    tiers: (!targets.is_empty())
                        .then_some(targets)
                        .into_iter()
                        .collect(),
                    kind: "module".into(),
                    rule: "rust_module_declaration",
                    failure: "rust_module_unresolved",
                });
            }
        } else if node.kind() == "extern_crate_declaration" {
            diagnostics.push(diagnostic(
                path,
                Some(node.start_position().row),
                "rust_extern_crate_unverified",
                "Extern crate bindings are not resolved by metrics".into(),
            ));
        } else if node.kind() == "use_declaration"
            && let Some(argument) = node.child_by_field_name("argument")
        {
            let mut edges = Vec::new();
            if expand_rust_use_tree_edges_ast(
                argument,
                source,
                &[],
                &module,
                &module,
                None,
                &mut edges,
            )
            .is_none()
            {
                diagnostics.push(diagnostic(
                    path,
                    Some(node.start_position().row),
                    "rust_use_unresolved",
                    text(node, source).into(),
                ));
            }
            for edge in edges {
                let origin = edge.origin();
                let (mut target, item) = match edge {
                    RustPubUseEdge::Named {
                        target_module,
                        target_item,
                        ..
                    } => (target_module, Some(target_item)),
                    RustPubUseEdge::Wildcard { target_module, .. } => (target_module, None),
                };
                if let Some(item) = item {
                    target.push(item);
                }
                for segment in &mut target {
                    *segment = segment.strip_prefix("r#").unwrap_or(segment).to_string();
                }
                let mut current = context.cloned();
                if let Some(current) = &mut current {
                    current.module.clone_from(&module);
                }
                let (targets, rule) = match current.as_ref() {
                    // ブロック内の束縛はモジュールの索引だけでは証明できない。
                    _ if !lexical_scopes.is_empty() => {
                        (Vec::new(), "rust_lexical_scope_unverified")
                    }
                    Some(context) if origin == UsePathOrigin::Anchored => (
                        index.lookup(context, &target).unwrap_or_default(),
                        "rust_module_index",
                    ),
                    Some(context) if origin == UsePathOrigin::Global => {
                        let mut global = context.clone();
                        global.module = vec!["<extern-prelude>".into()];
                        index.bare(&global, path, &target)
                    }
                    Some(context) => index.bare(context, path, &target),
                    None => (Vec::new(), "rust_module_context_unverified"),
                };
                out.push(Dependency {
                    source: text(argument, source).into(),
                    line: node.start_position().row,
                    tiers: (!targets.is_empty())
                        .then_some(targets)
                        .into_iter()
                        .collect(),
                    kind: "use".into(),
                    rule,
                    failure: match rule {
                        "external"
                        | "rust_lexical_scope_unverified"
                        | "rust_module_context_unverified" => rule,
                        _ => "rust_binding_unresolved",
                    },
                });
            }
        }
        ControlFlow::Continue(Walk::Descend)
    });
    if context.is_none() && !out.is_empty() {
        diagnostics.push(diagnostic(
            path,
            None,
            "rust_module_context_unverified",
            "The file has no unique declared module owner in the selected scope".into(),
        ));
    }
    out
}

pub(super) fn text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

pub(super) fn parent(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(p, _)| p)
        .unwrap_or("")
        .to_string()
}

pub(super) fn join(base: &str, target: &str) -> String {
    if target.starts_with('/') || target.contains(['\\', ':', '$']) {
        return format!("<unresolved>/{target}");
    }
    if base.is_empty() {
        normalize(target)
    } else {
        normalize(&format!("{base}/{target}"))
    }
}

fn normalize(path: &str) -> String {
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return format!("../{path}");
                }
            }
            _ => parts.push(part),
        }
    }
    parts.join("/")
}

pub(super) fn diagnostic(
    path: &str,
    line: Option<usize>,
    reason: &str,
    message: String,
) -> MetricsDiagnostic {
    MetricsDiagnostic {
        path: path.into(),
        line,
        reason: reason.into(),
        message,
    }
}
