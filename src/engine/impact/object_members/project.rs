//! 利用者を行別に振り分ける前に、全ファイルの書き込みと型問い合わせを調べる。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{ObjectMemberChange, type_queries, writes};
use crate::engine::impact::{
    FileContext, NewSideSource,
    ts_barrels::{TsBarrelIndex, WriteOrigins},
};
use crate::engine::{diff, parser};
use crate::language::LangId;
use crate::models::impact::{DiffFile, DiffNewSide};

pub(super) struct Candidate<'a> {
    pub(super) path: &'a str,
    pub(super) name: &'a str,
    pub(super) change: &'a mut ObjectMemberChange,
    pub(super) getters: writes::GetterUses,
}

pub(in crate::engine::impact) fn check(
    dir: &Path,
    (files, extra_paths): (&[PathBuf], &[String]),
    contexts: &mut [FileContext],
    barrels: &TsBarrelIndex,
    index: Option<&TsBarrelIndex>,
    side: DiffNewSide,
    (diff_input, changes): (&str, &[DiffFile]),
) {
    let mut candidates: Vec<_> = contexts
        .iter_mut()
        .flat_map(|ctx| {
            let path = ctx.new_path.as_str();
            ctx.object_member_changes
                .iter_mut()
                .map(move |(name, change)| Candidate {
                    path,
                    name,
                    change,
                    getters: Default::default(),
                })
        })
        .collect();
    if candidates.is_empty() {
        return;
    }
    let inputs = NewSideSource::open(side, dir);
    let changed: HashMap<_, _> = changes.iter().map(|f| (f.new_path.as_str(), f)).collect();
    let sections = diff::FileSections::split(diff_input);
    let mut origins = WriteOrigins::new(index.unwrap_or(barrels));
    for df in changes {
        let path = if df.new_path == "/dev/null" {
            &df.old_path
        } else {
            &df.new_path
        };
        origins.observe_change(path, || {
            (df.new_path == df.old_path)
                .then(|| inputs.read_checked(dir, path))
                .flatten()
                .and_then(|new| diff::reconstruct_old_source(&sections.get(path), path, &new))
                .map(|old| old.source)
        });
    }
    let mut forwards = writes::GetterForwards::new();
    let mut inspect = |path: &str, lang, source: Option<&[u8]>, old: bool| {
        let complete = source.is_some_and(|source| {
            let query = !old
                && lang != LangId::Javascript
                && candidates.iter().any(|c| !c.change.type_changes.is_empty())
                && memchr::memmem::find(source, b"typeof").is_some();
            let write = candidates.iter().any(|c| c.path == path)
                || memchr::memmem::find(source, b"require").is_some()
                || memchr::memmem::find_iter(source, b"import").any(|i| {
                    let tail = source[i + 6..].trim_ascii_start();
                    !(tail.starts_with(b"type {") || tail.starts_with(b"type *"))
                });
            if !query && !write {
                return true;
            }
            let Ok(tree) = parser::parse_source(source, lang) else {
                return false;
            };
            let root = tree.root_node();
            if root.has_error() {
                return false;
            }
            if query {
                type_queries::check_file(root, source, path, barrels, &mut candidates);
            }
            if write {
                writes::check_file(root, source, path, &origins, &mut forwards, &mut candidates);
            }
            true
        });
        if !complete {
            for c in &mut candidates {
                c.change.written = true;
            }
        }
        candidates.retain(|c| !c.change.written && !c.change.queried_type);
        candidates.is_empty()
    };
    let language = |path: &str| {
        LangId::from_path(camino::Utf8Path::new(path))
            .ok()
            .filter(|l| matches!(l, LangId::Javascript | LangId::Typescript | LangId::Tsx))
    };
    let removed: HashMap<_, _> = changes
        .iter()
        .filter(|f| f.new_path == "/dev/null")
        .map(|f| (f.old_path.as_str(), f))
        .collect();
    let paths = files.iter().filter_map(|path| {
        Some(crate::git_support::normalize_workspace_separators(
            path.strip_prefix(dir).ok()?.to_str()?,
        ))
    });
    for caller in paths.chain(extra_paths.iter().cloned()) {
        let Some(lang) = language(&caller) else {
            continue;
        };
        if let Some(df) = removed.get(caller.as_str()) {
            if inspect(&caller, lang, df.deleted_old_source.as_deref(), true) {
                return;
            }
            continue;
        }
        if index.is_some_and(|index| index.omits_index_path(&caller)) {
            continue;
        }
        let source = inputs.read(&dir.join(&caller), &caller);
        if inspect(&caller, lang, source.as_deref(), false) {
            return;
        }
        if let Some(df) = changed.get(caller.as_str())
            && df.old_path != "/dev/null"
        {
            let old = source
                .and_then(|s| diff::reconstruct_old_source(&sections.get(&caller), &caller, &s));
            if inspect(
                &df.old_path,
                lang,
                old.as_ref().map(|s| s.source.as_slice()),
                true,
            ) {
                return;
            }
        }
    }
    for c in candidates {
        c.change.written |= c.getters.may_write(&origins, &forwards);
    }
}
