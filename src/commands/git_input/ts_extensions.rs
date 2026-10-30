//! TS/TSX の拡張子変更を、静的な相対 import が継続する場合だけ同じモジュールにする。

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};
use std::process::Stdio;

use super::{
    GitBlobBatch, MAX_INPUT_SIZE, split_into_file_blocks, synthesize_added_file_diff,
    synthesize_modified_file_diff,
};
use crate::engine::{bounded_read, diff, parser};
use crate::language::LangId;

const MAX_FILE: usize = 2 * 1024 * 1024;
const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 10_000;

struct Candidate {
    old: String,
    new: String,
    old_block: usize,
    new_block: usize,
}

fn counterpart(path: &str) -> Option<String> {
    if path.ends_with(".d.ts") || path.contains(['\\', '\n', '\r', '\t', ':']) {
        return None;
    }
    path.strip_suffix(".tsx")
        .map(|stem| format!("{stem}.ts"))
        .or_else(|| path.strip_suffix(".ts").map(|stem| format!("{stem}.tsx")))
}

pub(super) fn normalize(dir: &str, base: &str, staged: bool, input: &mut String) {
    // 候補が無い通常の差分では、Git の追加起動もファイル走査も行わない。
    let files = diff::parse_unified_diff(input);
    let mut candidates = Vec::new();
    for (ix, file) in files.iter().enumerate() {
        let Some(new) = counterpart(&file.old_path) else {
            continue;
        };
        if file.new_path == new {
            candidates.push(Candidate {
                old: file.old_path.clone(),
                new,
                old_block: ix,
                new_block: ix,
            });
        } else if file.new_path == "/dev/null" {
            let additions: Vec<_> = files
                .iter()
                .enumerate()
                .filter(|(_, f)| f.old_path == "/dev/null" && f.new_path == new)
                .collect();
            if let [(new_ix, _)] = additions[..] {
                candidates.push(Candidate {
                    old: file.old_path.clone(),
                    new,
                    old_block: ix,
                    new_block: new_ix,
                });
            }
        }
    }
    if candidates.is_empty() {
        return;
    }
    let blocks = split_into_file_blocks(input);
    // hunk を持たない block などで、配列の位置を取り違えない。
    let mut by_path = HashMap::new();
    for (ix, block) in blocks.iter().enumerate() {
        let parsed = diff::parse_unified_diff(block);
        if let [file] = parsed.as_slice()
            && by_path
                .insert((file.old_path.clone(), file.new_path.clone()), ix)
                .is_some()
        {
            return;
        }
    }
    let old_blobs = GitBlobBatch::new(dir, base);
    let snapshot = crate::git_support::index_snapshot_tree(dir);
    let new_blobs = snapshot
        .as_ref()
        .map(|tree| GitBlobBatch::new(dir, tree))
        .or_else(|| staged.then(|| GitBlobBatch::index(dir)));
    let inventory = Inventory::read(dir, base, new_blobs.as_ref());
    let mut replacements: HashMap<usize, String> = HashMap::new();
    for candidate in candidates {
        let Some(&old_ix) = by_path.get(&(
            files[candidate.old_block].old_path.clone(),
            files[candidate.old_block].new_path.clone(),
        )) else {
            continue;
        };
        let Some(&new_ix) = by_path.get(&(
            files[candidate.new_block].old_path.clone(),
            files[candidate.new_block].new_path.clone(),
        )) else {
            continue;
        };
        if replacements.contains_key(&old_ix) || replacements.contains_key(&new_ix) {
            continue;
        }
        let Some(old) = old_blobs
            .read_limited(&candidate.old, MAX_FILE)
            .and_then(|v| String::from_utf8(v).ok())
        else {
            continue;
        };
        let Some(new) = read_source(dir, new_blobs.as_ref(), &candidate.new) else {
            continue;
        };
        let proven = inventory
            .as_ref()
            .is_some_and(|inventory| inventory.proves(&candidate.old, &candidate.new));
        let replacement = if proven {
            synthesize_modified_file_diff(&candidate.old, &candidate.new, old.as_bytes(), &new)
                .unwrap_or_default()
        } else if old_ix == new_ix {
            // Git 自身が R とした場合も、旧拡張子を直接使う参照を互換変更で隠さない。
            format!(
                "{}{}",
                deleted_diff(&candidate.old, &old),
                synthesize_added_file_diff(&candidate.new, &new)
            )
        } else {
            continue;
        };
        replacements.insert(old_ix, replacement);
        if old_ix != new_ix {
            replacements.insert(new_ix, String::new());
        }
    }
    if replacements.is_empty() {
        return;
    }
    let size = blocks
        .iter()
        .enumerate()
        .try_fold(0usize, |total, (ix, block)| {
            total.checked_add(replacements.get(&ix).unwrap_or(block).len())
        });
    if size.is_none_or(|size| size > MAX_INPUT_SIZE) {
        return;
    }
    *input = blocks
        .into_iter()
        .enumerate()
        .map(|(ix, block)| replacements.remove(&ix).unwrap_or(block))
        .collect();
}

fn deleted_diff(path: &str, source: &str) -> String {
    let lines: Vec<_> = source.lines().collect();
    let mut out = format!(
        "diff --git a/{path} b/{path}\ndeleted file mode 100644\n--- a/{path}\n+++ /dev/null\n@@ -1,{} +0,0 @@\n",
        lines.len()
    );
    for line in lines {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn read_source(dir: &str, blobs: Option<&GitBlobBatch>, path: &str) -> Option<String> {
    match blobs {
        Some(blobs) => String::from_utf8(blobs.read_limited(path, MAX_FILE)?).ok(),
        None => bounded_read::read_utf8_file_limited(&Path::new(dir).join(path), MAX_FILE as u64),
    }
}

struct Inventory {
    old: HashSet<String>,
    new: HashSet<String>,
    imports: Vec<(String, String)>,
}

impl Inventory {
    fn read(dir: &str, base: &str, new_blobs: Option<&GitBlobBatch>) -> Option<Self> {
        let prefix = git_output(dir, &["rev-parse", "--show-prefix"], 4096)?;
        let prefix = std::str::from_utf8(&prefix)
            .ok()?
            .trim_end_matches(['\r', '\n']);
        if prefix.contains(['\\', '\n', '\r']) {
            return None;
        }
        if !plain_external_configs(dir) {
            return None;
        }
        let old = list_git_files(dir, Some(base), prefix)?;
        let new = match new_blobs {
            Some(blobs) if blobs.rev() != ":0" => list_git_files(dir, Some(blobs.rev()), prefix)?,
            Some(_) => list_git_files(dir, None, prefix)?,
            None => list_worktree(dir)?,
        };
        let old_blobs = GitBlobBatch::new(dir, base);
        let mut imports = Vec::new();
        let mut bytes_left = MAX_BYTES;
        let mut nodes_left = 100_000usize;
        for (paths, blobs) in [(&old, Some(&old_blobs)), (&new, new_blobs)] {
            let mut paths: Vec<_> = paths.iter().collect();
            paths.sort();
            for path in paths {
                if is_config(path) {
                    let source = read_source(dir, blobs, path)?;
                    bytes_left = bytes_left.checked_sub(source.len())?;
                    if !plain_config(&source) {
                        return None;
                    }
                    continue;
                }
                if Path::new(path)
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| matches!(ext, "vue" | "svelte" | "astro" | "html"))
                {
                    return None;
                }
                let Ok(lang) = LangId::from_path(camino::Utf8Path::new(path)) else {
                    continue;
                };
                if !matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx) {
                    continue;
                }
                let source = read_source(dir, blobs, path)?;
                bytes_left = bytes_left.checked_sub(source.len())?;
                let tree = parser::parse_source(source.as_bytes(), lang).ok()?;
                if tree.root_node().has_error()
                    || source.contains("<reference")
                    || source.contains('\\')
                {
                    return None;
                }
                let mut pending = vec![tree.root_node()];
                while let Some(node) = pending.pop() {
                    nodes_left = nodes_left.checked_sub(1)?;
                    match node.kind() {
                        "string" => {
                            let value = static_specifier(node, &source)?;
                            if value.starts_with("./") || value.starts_with("../") {
                                imports.push((path.clone(), value.to_owned()));
                            }
                        }
                        "import_statement" | "export_statement" => {
                            if let Some(value) = node.child_by_field_name("source") {
                                imports.push((
                                    path.clone(),
                                    static_specifier(value, &source)?.to_owned(),
                                ));
                            }
                        }
                        "call_expression" => {
                            if let Some(function) = node.child_by_field_name("function") {
                                let text = function.utf8_text(source.as_bytes()).ok()?;
                                if matches!(
                                    text,
                                    "import" | "require" | "require.resolve" | "module.require"
                                ) {
                                    let args = node.child_by_field_name("arguments")?;
                                    let mut cursor = args.walk();
                                    let value = args
                                        .named_children(&mut cursor)
                                        .find(|part| part.kind() != "comment")?;
                                    imports.push((
                                        path.clone(),
                                        static_specifier(value, &source)?.to_owned(),
                                    ));
                                }
                            }
                        }
                        "identifier" => {
                            let name = node.utf8_text(source.as_bytes()).ok()?;
                            if name.contains('\\') || matches!(name, "eval" | "Function") {
                                return None;
                            }
                            if name == "require"
                                && !node.parent().is_some_and(|parent| {
                                    (parent.kind() == "call_expression"
                                        && parent.child_by_field_name("function") == Some(node))
                                        || (parent.kind() == "member_expression"
                                            && parent.child_by_field_name("object") == Some(node)
                                            && parent.parent().is_some_and(|call| {
                                                call.kind() == "call_expression"
                                                    && call.child_by_field_name("function")
                                                        == Some(parent)
                                            }))
                                })
                            {
                                return None;
                            }
                        }
                        _ => {}
                    }
                    let mut cursor = node.walk();
                    pending.extend(node.named_children(&mut cursor));
                }
            }
        }
        Some(Self { old, new, imports })
    }

    fn proves(&self, old: &str, new: &str) -> bool {
        if !self.old.contains(old)
            || self.old.contains(new)
            || !self.new.contains(new)
            || self.new.contains(old)
        {
            return false;
        }
        let stem = old
            .strip_suffix(".tsx")
            .or_else(|| old.strip_suffix(".ts"))
            .unwrap();
        let mut saw_import = false;
        for (from, spec) in &self.imports {
            // alias・package の解決はこの証拠で広げない。
            if !spec.starts_with("./") && !spec.starts_with("../") {
                return false;
            }
            let Some(target) = relative_specifier(from, spec) else {
                return false;
            };
            let target_stem = target
                .strip_suffix(".tsx")
                .or_else(|| target.strip_suffix(".ts"))
                .or_else(|| target.strip_suffix(".js"))
                .or_else(|| target.strip_suffix(".jsx"))
                .unwrap_or(&target);
            if target_stem == stem {
                if target != stem {
                    return false;
                }
                saw_import = true;
            }
        }
        saw_import
    }
}

fn static_specifier<'a>(node: tree_sitter::Node<'_>, source: &'a str) -> Option<&'a str> {
    if !matches!(node.kind(), "string" | "template_string") {
        return None;
    }
    let text = node.utf8_text(source.as_bytes()).ok()?;
    let value = text.get(1..text.len().checked_sub(1)?)?;
    (!value.contains(['\\', '\n', '\r', '$', '?', '#'])).then_some(value)
}

fn relative_specifier(from: &str, spec: &str) -> Option<String> {
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop()?;
    for part in spec.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            _ if part.contains(['\\', ':', '\n', '\r']) => return None,
            _ => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn is_config(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            (name.starts_with("tsconfig") || name.starts_with("jsconfig"))
                && name.ends_with(".json")
        })
}

fn plain_config(source: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(source).is_ok_and(|value| value.is_object())
        && ![
            "rootDirs",
            "moduleSuffixes",
            "extends",
            "baseUrl",
            "paths",
            "references",
            "\\",
        ]
        .iter()
        .any(|key| source.contains(key))
}

fn git_output(dir: &str, args: &[&str], max: usize) -> Option<Vec<u8>> {
    let mut child = crate::git_support::git_command_for_dir(dir)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let bytes =
        super::read_bytes_limited_and_drain(child.stdout.take()?, max, "TS extension inventory");
    if !child.wait().ok()?.success() {
        return None;
    }
    bytes.ok()
}

fn list_git_files(dir: &str, revision: Option<&str>, prefix: &str) -> Option<HashSet<String>> {
    let bytes = match revision {
        Some(revision) => git_output(
            dir,
            &["ls-tree", "-r", "-z", "--full-tree", revision],
            4 * 1024 * 1024,
        )?,
        None => git_output(
            dir,
            &["ls-files", "--stage", "--full-name", "-z"],
            4 * 1024 * 1024,
        )?,
    };
    let mut paths = HashSet::new();
    for (ix, record) in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .enumerate()
    {
        let (metadata, path) = std::str::from_utf8(record).ok()?.split_once('\t')?;
        let fields: Vec<_> = metadata.split(' ').collect();
        if fields.len() != 3
            || !matches!(fields[0], "100644" | "100755")
            || (revision.is_none() && fields[2] != "0")
            || path.contains(['\\', '\n', '\r'])
            || ix == MAX_ENTRIES
        {
            return None;
        }
        let relative = if let Some(relative) = path.strip_prefix(prefix) {
            relative.to_owned()
        } else if is_config(path) {
            let parent = Path::new(path).parent()?.to_str()?;
            let parent = if parent.is_empty() {
                String::new()
            } else {
                format!("{parent}/")
            };
            let Some(rest) = prefix.strip_prefix(&parent) else {
                continue;
            };
            let levels = rest.split('/').filter(|part| !part.is_empty()).count();
            format!(
                "{}{}",
                "../".repeat(levels),
                Path::new(path).file_name()?.to_str()?
            )
        } else {
            continue;
        };
        if !paths.insert(relative) {
            return None;
        }
    }
    Some(paths)
}

fn list_worktree(dir: &str) -> Option<HashSet<String>> {
    let root = Path::new(dir);
    let mut paths = HashSet::new();
    let mut entries_left = 100_000usize;
    for entry in ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
    {
        entries_left = entries_left.checked_sub(1)?;
        let entry = entry.ok()?;
        let metadata = std::fs::symlink_metadata(entry.path()).ok()?;
        if metadata.file_type().is_symlink() {
            return None;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return None;
            }
        }
        if metadata.is_file() {
            if paths.len() == MAX_ENTRIES {
                return None;
            }
            let path = entry.path().strip_prefix(root).ok()?;
            if path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
            {
                return None;
            }
            paths.insert(crate::git_support::normalize_workspace_separators(
                path.to_str()?,
            ));
        }
    }
    // ignore 済みの設定も TypeScript の解決には作用する。観測したソースの各祖先を直接読む。
    let mut directories = HashSet::new();
    for path in &paths {
        let mut directory = Path::new(path).parent();
        while let Some(relative) = directory {
            directories.insert(relative.to_owned());
            directory = relative.parent();
        }
    }
    directories.insert(Path::new("").to_owned());
    for directory in directories {
        for entry in std::fs::read_dir(root.join(&directory)).ok()? {
            entries_left = entries_left.checked_sub(1)?;
            let entry = entry.ok()?;
            let name = entry.file_name();
            let name = name.to_str()?;
            if is_config(name) {
                if !plain_metadata(&entry.path()) {
                    return None;
                }
                paths.insert(crate::git_support::normalize_workspace_separators(
                    directory.join(name).to_str()?,
                ));
            }
        }
    }
    Some(paths)
}

fn plain_metadata(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        if metadata.file_type().is_symlink() {
            return false;
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return false;
            }
        }
        true
    })
}

fn plain_external_configs(dir: &str) -> bool {
    let logical = if Path::new(dir).is_absolute() {
        Path::new(dir).to_owned()
    } else {
        let Ok(cwd) = std::env::current_dir() else {
            return false;
        };
        cwd.join(dir)
    };
    let logical: std::path::PathBuf = logical.components().collect();
    if !plain_metadata(&logical) {
        return false;
    }
    let Ok(physical) = std::fs::canonicalize(&logical) else {
        return false;
    };
    let mut remaining = 10_000usize;
    for root in [&logical, &physical] {
        for (depth, ancestor) in root.ancestors().skip(1).enumerate() {
            if depth >= 128 {
                return false;
            }
            let Ok(entries) = std::fs::read_dir(ancestor) else {
                return false;
            };
            for entry in entries {
                let Some(next) = remaining.checked_sub(1) else {
                    return false;
                };
                remaining = next;
                let Ok(entry) = entry else { return false };
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    return false;
                };
                if is_config(name)
                    && (!plain_metadata(&entry.path())
                        || !bounded_read::read_utf8_file_limited(&entry.path(), MAX_FILE as u64)
                            .is_some_and(|source| plain_config(&source)))
                {
                    return false;
                }
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_nodes_and_fields_exist_in_all_js_grammars() {
        for lang in [LangId::Javascript, LangId::Typescript, LangId::Tsx] {
            for query in [
                "(import_statement source: (_))",
                "(export_statement source: (_))",
                "(call_expression function: (_) arguments: (_))",
                "(member_expression object: (_))",
                "(identifier)",
                "(string)",
                "(template_string)",
                "(comment)",
            ] {
                tree_sitter::Query::new(&lang.ts_language(), query).unwrap();
            }
        }
    }
}
