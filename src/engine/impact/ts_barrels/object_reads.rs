//! オブジェクトの読み取りに対し、import の指定子を解析中のツリーで解く。

use std::path::{Component, Path};

use super::{TsBarrelIndex, normalize};
use crate::commands::ts_const_rebinding::directory_specifier;

impl TsBarrelIndex {
    pub(in crate::engine::impact) fn exports_object(
        &self,
        target: &str,
        name: &str,
        changed: &str,
    ) -> bool {
        target == changed
            || self.origin(
                super::OriginUse::ObjectRead,
                target,
                name,
                changed,
                name,
                &mut Default::default(),
            ) == super::Origin::Changed
    }

    pub(in crate::engine::impact) fn object_read_source(
        &self,
        from: &str,
        specifier: &str,
    ) -> Option<String> {
        if self.root.as_os_str().is_empty() {
            return None;
        }
        let parent = Path::new(from).parent()?;
        let key = (parent.to_str()?.to_owned(), specifier.to_owned());
        if let Some(result) = self.object_sources.lock().ok()?.get(&key).cloned() {
            return result;
        }
        let result = self.resolve_read(from, parent, specifier);
        self.object_sources.lock().ok()?.insert(key, result.clone());
        result
    }

    fn resolve_read(&self, from: &str, parent: &Path, specifier: &str) -> Option<String> {
        if !(specifier.starts_with("./") || specifier.starts_with("../")) {
            return self.resolve_for::<true>(from, specifier);
        }
        if specifier.contains(['?', '#', '\\']) {
            return None;
        }
        let base = normalize(parent.join(specifier).to_str()?)?;
        if base.is_empty() {
            return None;
        }
        if !directory_specifier(specifier) {
            let found = self.resolve(from, specifier)?;
            if Path::new(&found).parent() != Some(Path::new(&base)) {
                if let Some(suffix) = found.strip_prefix(&base).filter(|s| s.starts_with('.')) {
                    // 拡張子を補ったファイルは、同名ディレクトリより先に選ばれる。
                    return self
                        .resolve_for::<true>(from, &format!("{specifier}{suffix}"))
                        .filter(|target| target == &found);
                }
                return self.resolve_for::<true>(from, specifier);
            }
        }
        let entry = self.directory_entry(&base)?;
        let target = self.resolve_for::<true>(from, &format!("{specifier}/{entry}"))?;
        // entry 自体がさらにディレクトリの場合の探索は証明しない。
        (Path::new(&target).parent() == Path::new(&base).join(entry).parent()).then_some(target)
    }

    fn directory_entry(&self, base: &str) -> Option<String> {
        let file = format!("{base}/package.json");
        let bytes = if let Some(index) = &self.index_reader {
            if !self.files.contains(&file) {
                return Some("index".into());
            }
            index.lock().ok()?.read_limited(&file, 1024 * 1024)?
        } else {
            if crate::git_support::tree_view(&self.root).hides(Path::new(&file)) {
                return None;
            }
            let path = self.root.join(file);
            match std::fs::symlink_metadata(&path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Some("index".into()),
                Ok(m) if m.file_type().is_file() => {}
                _ => return None,
            }
            crate::engine::bounded_read::read_utf8_file_limited(&path, 1024 * 1024)?.into_bytes()
        };
        let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
        let manifest = json.as_object()?;
        if manifest.keys().any(|k| {
            !matches!(
                k.as_str(),
                "name" | "version" | "private" | "description" | "license" | "main"
            )
        }) {
            return None;
        }
        let value = match manifest.get("main") {
            Some(main) => main.as_str()?,
            None => "index",
        };
        let value = value.strip_prefix("./").unwrap_or(value);
        (!value.is_empty()
            && !value.contains(['\\', ':', '?', '#'])
            && !directory_specifier(value)
            && Path::new(value)
                .components()
                .all(|c| matches!(c, Component::Normal(_))))
        .then(|| value.to_owned())
    }

    pub(in crate::engine::impact) fn may_import_object(
        &self,
        caller: &str,
        name: &str,
        changed: &str,
    ) -> bool {
        self.module(caller).is_some_and(|module| {
            module.imports.get(name).is_some_and(|specifiers| {
                specifiers.iter().any(|s| {
                    self.object_read_source(caller, s).is_none_or(|target| {
                        self.reaches::<true>(&target, changed, None, &self.object_reachability)
                    })
                })
            })
        })
    }
}
