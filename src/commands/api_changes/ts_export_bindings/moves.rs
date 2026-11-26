//! 宣言の移動で、元のモジュールの再公開が指す宣言まで保たれたかを確かめる。

use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};

use super::{Binding, Exports, RetargetedExports, Target};
use crate::commands::api_changes::moved_members::MemberFileFacts;
use crate::commands::api_changes::prepare::DetectionInputs;
use crate::commands::api_changes::{
    ApiSymbolCandidate, load_new_source,
    ref_index::{ApiClosureCaches, unique_relative_file},
    removed_attribution::relative_directory_has_manifest,
    ts_alias_resolution::ordinary_relative_imports,
};
use crate::engine::{js_string::decoded, parser};
use crate::language::LangId;
use crate::models::review::{ApiSymbolChange, MovedSymbol};

mod callers;

type NameAt = (String, String);

#[derive(Default)]
pub(in crate::commands::api_changes) struct MoveExports {
    blocked: HashSet<NameAt>,
    removed: Vec<ApiSymbolCandidate>,
}

impl MoveExports {
    pub(in crate::commands::api_changes) fn settle(
        self,
        moved: &mut Vec<MovedSymbol>,
        removed: &mut Vec<ApiSymbolCandidate>,
    ) {
        removed.extend(self.removed);
        if !self.blocked.is_empty() {
            moved.retain(|m| !self.blocked.contains(&(m.from.clone(), m.name.clone())));
        }
    }
}

enum Origin {
    Moved,
    Missing,
    Unverified,
}

#[derive(Default)]
struct Modules {
    exports: HashMap<String, Option<Exports>>,
    imports: Imports,
}

#[derive(Default)]
struct Imports {
    targets: HashMap<NameAt, Option<String>>,
    ordinary: OnceCell<bool>,
}

impl Imports {
    fn target(&mut self, dir: &str, path: &str, module: &str) -> Option<&str> {
        self.targets
            .entry((path.to_owned(), module.to_owned()))
            .or_insert_with(|| {
                if !*self.ordinary.get_or_init(|| ordinary_relative_imports(dir))
                    || relative_directory_has_manifest(dir, path, module)
                {
                    return None;
                }
                unique_relative_file(dir, path, module)
            })
            .as_deref()
    }
}

fn module_exports(inputs: &DetectionInputs<'_>, path: &str) -> Option<Exports> {
    let source = load_new_source(inputs.dir, path)?;
    let (tree, lang) = parser::parse_file(camino::Utf8Path::new(path), &source).ok()?;
    let root = tree.root_node();
    if root.has_error() || !matches!(lang, LangId::Typescript | LangId::Tsx | LangId::Javascript) {
        return None;
    }
    // CommonJS の export = や通常のスクリプトからは ESM の公開名を証明できない。
    let mut module = false;
    for stmt in root.named_children(&mut root.walk()) {
        if stmt.kind() == "export_statement" && super::has_token(stmt, "=") {
            return None;
        }
        module |= matches!(stmt.kind(), "export_statement" | "import_statement");
    }
    module.then(|| {
        Exports::collect(
            root,
            &source,
            inputs.strip_internal.applies(path, lang, &source),
        )
    })
}

impl Modules {
    fn origin(
        &mut self,
        inputs: &DetectionInputs<'_>,
        path: &str,
        public: &str,
        type_only: bool,
        moved: &MovedSymbol,
    ) -> Origin {
        let Some(local) = decoded(&moved.name) else {
            return Origin::Unverified;
        };
        let mut pending = vec![(path.to_owned(), public.to_owned(), false)];
        let mut visited = HashSet::new();
        let mut predecessors = HashMap::<_, Vec<_>>::new();
        let mut required = HashSet::new();
        let mut terminals = Vec::new();
        while let Some(site) = pending.pop() {
            if !visited.insert(site.clone()) {
                continue;
            }
            let (path, name, restricted) = &site;
            let Some(exports) = self
                .exports
                .entry(path.clone())
                .or_insert_with(|| module_exports(inputs, path))
            else {
                return Origin::Unverified;
            };
            let mut follow = |module: &str, name: &str, type_only, named| {
                let target = self.imports.target(inputs.dir, path, module)?;
                let next = (target.to_owned(), name.to_owned(), *restricted || type_only);
                predecessors
                    .entry(next.clone())
                    .or_default()
                    .push(site.clone());
                if named {
                    required.insert(next.clone());
                }
                pending.push(next);
                Some(())
            };
            if let Some(binding) = exports.names.get(name) {
                match &binding.target {
                    Target::Local(n)
                        if path == &moved.to
                            && n == local.as_ref()
                            && (type_only || !(*restricted || binding.type_only)) =>
                    {
                        terminals.push(site)
                    }
                    Target::Remote {
                        module,
                        name: Some(n),
                    } if follow(module, n, binding.type_only, true).is_some() => {}
                    _ => return Origin::Unverified,
                }
            } else if name != "default" {
                for (module, type_only) in &exports.stars {
                    if follow(module, name, *type_only, false).is_none() {
                        return Origin::Unverified;
                    }
                }
            }
        }
        // 名前を指定した辺には終端が必要。循環や別の star から同じ終端へ至る経路は許す。
        if terminals.is_empty() && required.is_empty() {
            return Origin::Missing;
        }
        let mut reaches = HashSet::new();
        while !required.is_empty() {
            let Some(site) = terminals.pop() else {
                return Origin::Unverified;
            };
            if reaches.insert(site.clone()) {
                required.remove(&site);
                terminals.extend(predecessors.remove(&site).into_iter().flatten());
            }
        }
        Origin::Moved
    }
}

impl RetargetedExports {
    /// 名前が残ったため削除から外れた宣言も、同じ差分に移動先の候補があれば通常の移動照合に戻す。
    pub(in crate::commands::api_changes) fn restore_move_candidates(
        &self,
        files: &MemberFileFacts<'_>,
        added: &[ApiSymbolCandidate],
        removed: &mut Vec<ApiSymbolCandidate>,
    ) {
        if self.moved_exports.is_empty() {
            return;
        }
        let added: HashSet<_> = added
            .iter()
            .map(|c| (&c.name, &c.kind, &c.signature))
            .collect();
        let existing: HashSet<_> = removed
            .iter()
            .map(|c| (&c.file, &c.name, &c.kind, &c.signature))
            .collect();
        let mut restored = Vec::new();
        for (path, names) in &self.moved_exports {
            let Some(&old) = files.old.get(path.as_str()) else {
                continue;
            };
            for (name, kind, signature) in old {
                if decoded(name).is_some_and(|n| names.contains_key(n.as_ref()))
                    && added.contains(&(name, kind, signature))
                    && !existing.contains(&(path, name, kind, signature))
                {
                    restored.push(ApiSymbolCandidate {
                        name: name.clone(),
                        kind: kind.clone(),
                        signature: signature.clone(),
                        file: path.clone(),
                        inline_module: None,
                    });
                }
            }
        }
        removed.extend(restored);
    }

    pub(in crate::commands::api_changes) fn verify_moves(
        &mut self,
        inputs: &DetectionInputs<'_>,
        files: &MemberFileFacts<'_>,
        moved: &[MovedSymbol],
        caches: &mut ApiClosureCaches,
    ) -> MoveExports {
        let mut modules = Modules::default();
        let mut result = MoveExports::default();
        let mut verified = HashSet::new();
        let mut changes = Vec::new();
        let mut missing = Vec::new();
        for symbol in moved {
            let Some(publics) = self
                .moved_exports
                .get(&symbol.from)
                .and_then(|names| names.get(decoded(&symbol.name)?.as_ref()))
            else {
                continue;
            };
            let Some(&path) = files.new_path_of.get(symbol.from.as_str()) else {
                continue;
            };
            for (public, type_only) in publics {
                verified.insert((path.to_owned(), public.clone()));
                let local = (symbol.from.clone(), symbol.name.clone());
                let before = Binding {
                    target: Target::Local(symbol.name.clone()),
                    type_only: *type_only,
                }
                .signature(public);
                match modules.origin(inputs, path, public, *type_only, symbol) {
                    Origin::Moved => {}
                    Origin::Missing => {
                        missing.push((
                            local,
                            ApiSymbolCandidate {
                                file: path.to_owned(),
                                name: public.clone(),
                                kind: symbol.kind.clone(),
                                signature: before,
                                inline_module: None,
                            },
                        ));
                    }
                    Origin::Unverified => {
                        result.blocked.insert(local);
                        let after = modules
                            .exports
                            .get(path)
                            .and_then(Option::as_ref)
                            .and_then(|exports| exports.names.get(public))
                            .map(|binding| binding.signature(public));
                        changes.push(ApiSymbolChange::new(
                            public.clone(),
                            "export".into(),
                            path.to_owned(),
                            Some(before),
                            after,
                        ));
                    }
                }
            }
        }
        for (local, removal) in
            callers::still_imported(inputs.dir, missing, &mut modules.imports, caches)
        {
            result.blocked.insert(local);
            result.removed.push(removal);
        }
        self.changes
            .retain(|c| !verified.contains(&(c.file.clone(), c.name.clone())));
        self.changes.extend(changes);
        result
    }
}
