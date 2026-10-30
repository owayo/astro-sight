//! 現在の import 候補とパスの記述を、変更義務とは分けて扱う。

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};

use super::recent_cochange::git;
use crate::language::LangId;
use crate::models::cochange::{
    CochangeDependencyDirection as Direction, CochangeDependencyEvidence as Evidence,
    CochangeDependencyKind as Kind, CochangeReviewFilters as Filters,
};
use crate::models::impact::DiffFile;

const FILE_LIMIT: usize = 256;
const BYTE_LIMIT: usize = 32 * 1024 * 1024;
const SINGLE_FILE_LIMIT: usize = 1024 * 1024;

struct FileFacts {
    text: String,
    targets: Vec<String>,
    assessed_lang: Option<LangId>,
}

pub(super) struct Scope<'a> {
    dir: &'a str,
    excluded_dirs: &'a [String],
    excluded: GlobSet,
    gitlinks: HashSet<String>,
    indexed: Option<HashSet<String>>,
    snapshot: bool,
    indexed_regular: HashSet<String>,
    blobs: Option<crate::commands::git_input::GitBlobBatch>,
    old_paths: HashSet<String>,
    deleted: HashSet<String>,
    cache: HashMap<String, Option<FileFacts>>,
    bytes: usize,
    pub filters: Filters,
}

/// `-z` で区切った git の一覧を 1 件ずつ `each` に渡す。`each` が `None` を返したら中断する。
///
/// 一覧はファイル数に比例して大きくなるので、メタデータ用の `git` (2 MiB 上限) を通さない。
/// 2 MiB で切れると、2 万ファイル規模のリポジトリで依存の証拠と gitlink の除外が効かなくなった。
/// 上限 (`MAX_INPUT_SIZE`) を超えた場合と git が失敗した場合は `None`。
fn git_records(dir: &str, args: &[&str], mut each: impl FnMut(&[u8]) -> Option<()>) -> Option<()> {
    use std::io::BufRead;
    let mut child = crate::git_support::git_command_for_dir(dir)
        .args(args)
        .current_dir(dir)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut reader = std::io::BufReader::new(child.stdout.take()?);
    let mut record = Vec::new();
    let mut total = 0usize;
    let complete = loop {
        record.clear();
        match reader.read_until(0, &mut record) {
            Ok(0) => break true,
            Ok(read) => {
                total = total.saturating_add(read);
                let entry = record.strip_suffix(&[0]).unwrap_or(&record);
                if total > crate::commands::common::MAX_INPUT_SIZE
                    || (!entry.is_empty() && each(entry).is_none())
                {
                    break false;
                }
            }
            Err(_) => break false,
        }
    };
    // 途中で止めたときに git が書き込みで詰まらないよう、待つ前にパイプを閉じる。
    drop(reader);
    let status = child.wait().ok()?;
    (complete && status.success()).then_some(())
}

/// `dir` のリポジトリのルートからの相対 prefix。`raw` は `git rev-parse --show-prefix` の出力
/// (終端の改行の有無は問わない)。UTF-8 でなければ `None` (tree の一覧を使えない)。
fn workspace_prefix(raw: Vec<u8>) -> Option<String> {
    Some(
        String::from_utf8(raw)
            .ok()?
            .trim_end_matches('\n')
            .to_owned(),
    )
}

fn tree_entries(
    dir: &str,
    prefix: &str,
    revision: &str,
) -> Option<(HashSet<String>, HashSet<String>, HashSet<String>)> {
    let mut paths = HashSet::new();
    let mut links = HashSet::new();
    let mut regular = HashSet::new();
    git_records(
        dir,
        &["ls-tree", "--full-tree", "-r", "-z", revision, "--"],
        |entry| {
            let (header, path) = std::str::from_utf8(entry).ok()?.split_once('\t')?;
            let Some(path) = path.strip_prefix(prefix) else {
                return Some(());
            };
            paths.insert(path.to_owned());
            if header.starts_with("100644 ") || header.starts_with("100755 ") {
                regular.insert(path.to_owned());
            }
            if header.starts_with("160000 ") {
                links.insert(path.to_owned());
            }
            Some(())
        },
    )?;
    Some((paths, links, regular))
}

fn normalized(path: &Path) -> Option<String> {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(name) if name != ".git" => result.push(name),
            Component::CurDir => {}
            Component::ParentDir if result.pop() => {}
            _ => return None,
        }
    }
    Some(crate::git_support::normalize_workspace_separators(
        result.to_str()?,
    ))
}

fn is_js(lang: LangId) -> bool {
    matches!(lang, LangId::Javascript | LangId::Typescript | LangId::Tsx)
}

fn dynamic_loader(root: tree_sitter::Node<'_>, source: &[u8]) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if (node.kind() == "export_statement" && node.child_by_field_name("source").is_some())
            || matches!(
                node.kind(),
                "import_require_clause" | "import_from_statement"
            )
        {
            return true;
        }
        let name = node.utf8_text(source).ok();
        if matches!(node.kind(), "identifier" | "property_identifier")
            && name.is_some_and(|n| {
                matches!(
                    n,
                    "__import__" | "import_module" | "exec" | "eval" | "getattr" | "open"
                )
            })
        {
            return true;
        }
        if name == Some("require") && matches!(node.kind(), "identifier" | "property_identifier") {
            let direct = node.parent().is_some_and(|p| {
                p.kind() == "call_expression"
                    && p.child_by_field_name("function")
                        .is_some_and(|f| f.id() == node.id())
            });
            if !direct {
                return true;
            }
        }
        if node.kind() == "call_expression"
            && node.child_by_field_name("function").is_some_and(|f| {
                f.kind() == "import" || f.utf8_text(source).ok() == Some("require")
            })
        {
            let argument = node.child_by_field_name("arguments").and_then(|args| {
                let mut cursor = args.walk();
                args.named_children(&mut cursor)
                    .find(|child| child.kind() != "comment")
            });
            if argument.is_none_or(|a| {
                a.kind() != "string"
                    && (a.kind() != "template_string" || {
                        let mut cursor = a.walk();
                        a.named_children(&mut cursor)
                            .any(|child| child.kind() == "template_substitution")
                    })
            }) {
                return true;
            }
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    false
}

fn mentions(text: &str, path: &str) -> bool {
    let is_path = |ch: char| ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | '\\');
    text.match_indices(path).any(|(index, _)| {
        let before = text[..index].chars().next_back();
        let after = text[index + path.len()..].chars().next();
        before.is_none_or(|c| !is_path(c))
            && (after.is_none_or(|c| !is_path(c))
                || (after == Some('.')
                    && text[index + path.len() + 1..]
                        .chars()
                        .next()
                        .is_none_or(|c| !is_path(c))))
    })
}

fn relative_path(from: &str, to: &str) -> String {
    let from: Vec<_> = Path::new(from)
        .parent()
        .unwrap_or(Path::new(""))
        .components()
        .collect();
    let to: Vec<_> = Path::new(to).components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut path = PathBuf::new();
    for _ in common..from.len() {
        path.push("..");
    }
    for part in &to[common..] {
        path.push(part.as_os_str());
    }
    crate::git_support::normalize_workspace_separators(&path.to_string_lossy())
}

impl<'a> Scope<'a> {
    /// prefix を自分で問い合わせる版。review は差分の取得で取れた prefix を渡す
    /// ([`Self::with_known_prefix`])。
    #[cfg(test)]
    pub fn new(
        dir: &'a str,
        base: Option<&str>,
        diff: &[DiffFile],
        excluded_dirs: &'a [String],
        excluded_globs: &[String],
    ) -> Result<Self> {
        Self::with_known_prefix(dir, None, base, diff, excluded_dirs, excluded_globs)
    }

    /// `known_prefix` は `dir` で `git rev-parse --show-prefix` を実行した出力 (差分の取得で
    /// 同じ問い合わせを済ませていれば渡す)。無ければ tree の一覧が最初に要るときに 1 回だけ
    /// 問い合わせ、base と staged の index tree の一覧で共有する (失敗も共有する)。
    pub fn with_known_prefix(
        dir: &'a str,
        known_prefix: Option<&[u8]>,
        base: Option<&str>,
        diff: &[DiffFile],
        excluded_dirs: &'a [String],
        excluded_globs: &[String],
    ) -> Result<Self> {
        let mut builder = GlobSetBuilder::new();
        for glob in excluded_globs {
            builder.add(Glob::new(glob.trim_start_matches('!'))?);
        }
        let mut resolved_prefix: Option<Option<String>> =
            known_prefix.map(|raw| workspace_prefix(raw.to_vec()));
        let mut prefix = || {
            resolved_prefix
                .get_or_insert_with(|| {
                    git(dir, &["rev-parse", "--show-prefix"]).and_then(workspace_prefix)
                })
                .clone()
        };
        let mut gitlinks = HashSet::new();
        let old_tree = base.and_then(|b| tree_entries(dir, &prefix()?, b));
        let mut unavailable = base.is_some() && old_tree.is_none();
        let old_paths = old_tree
            .map(|(paths, links, _)| {
                gitlinks.extend(links);
                paths
            })
            .unwrap_or_default();
        let snapshot = crate::git_support::index_snapshot_tree(dir);
        let mut indexed_regular = HashSet::new();
        let blobs = snapshot
            .as_ref()
            .map(|tree| crate::commands::git_input::GitBlobBatch::new(dir, tree));
        let indexed = if let Some(tree) = snapshot.as_ref() {
            prefix()
                .and_then(|prefix| tree_entries(dir, &prefix, tree))
                .map(|(paths, links, regular)| {
                    indexed_regular = regular;
                    gitlinks.extend(links);
                    paths
                })
        } else {
            git_records(dir, &["ls-files", "--stage", "-z", "--"], |entry| {
                if let Ok(entry) = std::str::from_utf8(entry)
                    && let Some((header, path)) = entry.split_once('\t')
                    && header.starts_with("160000 ")
                {
                    gitlinks.insert(path.to_owned());
                }
                Some(())
            });
            None
        };
        unavailable |= snapshot.is_some() && indexed.is_none();
        let deleted = diff
            .iter()
            .filter(|f| f.new_path == "/dev/null")
            .map(|f| f.old_path.clone())
            .collect();
        Ok(Self {
            dir,
            excluded_dirs,
            excluded: builder.build()?,
            gitlinks,
            indexed,
            snapshot: snapshot.is_some(),
            indexed_regular,
            blobs,
            old_paths,
            deleted,
            cache: HashMap::new(),
            bytes: 0,
            filters: Filters {
                metadata_unavailable: unavailable,
                ..Filters::default()
            },
        })
    }

    pub fn excluded(&self, path: &str) -> bool {
        self.excluded.is_match(path)
            || path
                .split('/')
                .any(|part| self.excluded_dirs.iter().any(|d| d == part))
    }

    pub fn source(&mut self, path: &str) -> bool {
        if self.excluded(path) {
            self.filters.excluded_sources += 1;
            return false;
        }
        if self.gitlinks.contains(path) {
            self.filters.gitlink_sources += 1;
            return false;
        }
        true
    }

    pub fn candidate(&mut self, path: &str) -> bool {
        if self.excluded(path) {
            self.filters.excluded_candidates += 1;
            return false;
        }
        if self.gitlinks.contains(path) {
            self.filters.gitlink_candidates += 1;
            return false;
        }
        self.exists(path)
    }

    fn exists(&self, path: &str) -> bool {
        if self.gitlinks.contains(path) {
            return false;
        }
        match &self.indexed {
            Some(paths) => paths.contains(path),
            None if self.snapshot => true,
            None => match std::fs::symlink_metadata(Path::new(self.dir).join(path)) {
                Ok(metadata) => metadata.is_file() || metadata.file_type().is_symlink(),
                Err(error) => error.kind() != std::io::ErrorKind::NotFound,
            },
        }
    }

    fn load(&mut self, path: &str) {
        if self.cache.contains_key(path) {
            return;
        }
        if self.cache.len() >= FILE_LIMIT {
            self.filters.dependency_probes_truncated = true;
            return;
        }
        let facts = (|| {
            let source = if self.snapshot {
                if !self.indexed_regular.contains(path) {
                    return None;
                }
                match self.blobs.as_ref()?.read_limited(path, SINGLE_FILE_LIMIT) {
                    Some(source) => source,
                    None => {
                        self.filters.dependency_probes_truncated = true;
                        return None;
                    }
                }
            } else {
                let full = Path::new(self.dir).join(path);
                let metadata = std::fs::symlink_metadata(&full).ok()?;
                if !metadata.file_type().is_file() {
                    return None;
                }
                let root = std::fs::canonicalize(self.dir).ok()?;
                let canonical = std::fs::canonicalize(&full).ok()?;
                let relative = canonical.strip_prefix(&root).ok()?;
                if relative
                    .components()
                    .any(|p| p.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git"))
                {
                    return None;
                }
                if metadata.len() > SINGLE_FILE_LIMIT as u64 {
                    self.filters.dependency_probes_truncated = true;
                    return None;
                }
                crate::engine::parser::read_file(camino::Utf8Path::new(canonical.to_str()?))
                    .ok()?
                    .to_vec()
            };
            if source.len() > SINGLE_FILE_LIMIT || self.bytes + source.len() > BYTE_LIMIT {
                self.filters.dependency_probes_truncated = true;
                return None;
            }
            self.bytes += source.len();
            let text = std::str::from_utf8(&source).ok()?.to_owned();
            let parsed = crate::engine::parser::detect_lang(camino::Utf8Path::new(path), &source)
                .ok()
                .and_then(|lang| {
                    crate::engine::parser::parse_source(&source, lang)
                        .ok()
                        .map(|tree| (lang, tree))
                })
                .filter(|(_, tree)| !tree.root_node().has_error());
            let mut assessed_lang = None;
            let mut targets = Vec::new();
            if let Some((lang, tree)) = parsed
                && let Ok(imports) =
                    crate::engine::imports::extract_imports(tree.root_node(), &source, lang)
            {
                let mut complete = matches!(
                    lang,
                    LangId::Javascript | LangId::Typescript | LangId::Tsx | LangId::Python
                ) && !dynamic_loader(tree.root_node(), &source);
                for edge in imports {
                    if let Some(target) = self.resolve_import(path, lang, &edge.source) {
                        targets.push(target);
                    } else {
                        complete = false;
                    }
                }
                if complete {
                    assessed_lang = Some(lang);
                }
            }
            Some(FileFacts {
                text,
                targets,
                assessed_lang,
            })
        })();
        self.cache.insert(path.to_owned(), facts);
    }

    fn resolve_import(&self, from: &str, lang: LangId, specifier: &str) -> Option<String> {
        if specifier.contains(['\\', '\0', '\n', '\r']) {
            return None;
        }
        let python_fallback;
        let candidates = match lang {
            LangId::Javascript | LangId::Typescript | LangId::Tsx if specifier.starts_with('.') => {
                python_fallback = false;
                let path = normalized(
                    &Path::new(from)
                        .parent()
                        .unwrap_or(Path::new(""))
                        .join(specifier),
                )?;
                let mut paths = vec![path.clone()];
                if Path::new(&path).extension().is_none() {
                    for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "json"] {
                        paths.push(format!("{path}.{ext}"));
                        paths.push(format!("{path}/index.{ext}"));
                    }
                }
                paths
            }
            LangId::Python => {
                if let Some(path) =
                    crate::commands::api_changes::python_module_path::resolve(self.dir, specifier)
                {
                    return Some(path);
                }
                if specifier.split('.').any(|part| {
                    part.is_empty() || !part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                }) {
                    return None;
                }
                python_fallback = true;
                let module = specifier.replace('.', "/");
                vec![
                    format!("{module}.py"),
                    format!("{module}/__init__.py"),
                    format!("src/{module}.py"),
                    format!("src/{module}/__init__.py"),
                ]
            }
            _ => return None,
        };
        let mut present = candidates.into_iter().filter(|path| {
            self.exists(path) || (self.deleted.contains(path) && self.old_paths.contains(path))
        });
        let path = present.next()?;
        if present.next().is_some() || (python_fallback && !self.deleted.contains(&path)) {
            return None;
        }
        Some(path)
    }

    pub fn evidence(&mut self, source: &str, candidate: &str) -> Option<Evidence> {
        self.load(source);
        self.load(candidate);
        let imports = |from: &str, to: &str| {
            self.cache
                .get(from)
                .and_then(Option::as_ref)
                .is_some_and(|f| f.targets.iter().any(|path| path == to))
        };
        let forward = imports(source, candidate);
        let backward = imports(candidate, source);
        if forward || backward {
            return Some(Evidence {
                kind: Kind::StaticImportCandidate,
                direction: if forward && backward {
                    Direction::Bidirectional
                } else if forward {
                    Direction::ChangedToRelated
                } else {
                    Direction::RelatedToChanged
                },
            });
        }
        let mention = |from: &str, to: &str| {
            self.cache
                .get(from)
                .and_then(Option::as_ref)
                .is_some_and(|f| {
                    let relative = relative_path(from, to);
                    mentions(&f.text, to)
                        || mentions(&f.text, &relative)
                        || mentions(&f.text, &format!("./{relative}"))
                })
        };
        let forward = mention(source, candidate);
        let backward = mention(candidate, source);
        (forward || backward).then_some(Evidence {
            kind: Kind::PathMention,
            direction: if forward && backward {
                Direction::Bidirectional
            } else if forward {
                Direction::ChangedToRelated
            } else {
                Direction::RelatedToChanged
            },
        })
    }

    pub fn deleted_without_reference(
        &mut self,
        source: &str,
        candidate: &str,
        evidence: Option<Evidence>,
    ) -> bool {
        if self.deleted.contains(source)
            && evidence.is_none()
            && !self.filters.metadata_unavailable
            && self
                .cache
                .get(candidate)
                .and_then(Option::as_ref)
                .and_then(|f| f.assessed_lang)
                .zip(LangId::from_path(camino::Utf8Path::new(source)).ok())
                .is_some_and(|(candidate, source)| {
                    candidate == source || (is_js(candidate) && is_js(source))
                })
        {
            self.filters.deleted_pairs_without_observed_reference += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::tests::common::{git_commit_files, init_git_repo_for_test};

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn static_import_directions_and_path_mentions_are_distinct() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let name = root.to_str().unwrap();
        write(
            root,
            "src/a.ts",
            "import { b } from './b';\nexport const a = b;\n",
        );
        write(root, "src/b.ts", "export const b = 1;\n");
        write(
            root,
            "docs/notes.md",
            "Check ../src/a.ts and ./../src/b.ts.\n",
        );
        let mut scope = Scope::new(name, None, &[], &[], &[]).unwrap();
        assert_eq!(
            scope.evidence("src/a.ts", "src/b.ts"),
            Some(Evidence {
                kind: Kind::StaticImportCandidate,
                direction: Direction::ChangedToRelated
            })
        );
        assert_eq!(
            scope.evidence("src/b.ts", "src/a.ts"),
            Some(Evidence {
                kind: Kind::StaticImportCandidate,
                direction: Direction::RelatedToChanged
            })
        );
        assert_eq!(
            scope.evidence("src/a.ts", "docs/notes.md"),
            Some(Evidence {
                kind: Kind::PathMention,
                direction: Direction::RelatedToChanged
            })
        );
        write(
            root,
            "src/b.ts",
            "import { a } from './a';\nexport const b = a;\n",
        );
        let mut scope = Scope::new(name, None, &[], &[], &[]).unwrap();
        assert_eq!(
            scope.evidence("src/a.ts", "src/b.ts").unwrap().direction,
            Direction::Bidirectional
        );
    }

    #[test]
    fn import_candidates_are_unique_and_dynamic_specifiers_are_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let name = root.to_str().unwrap();
        write(root, "a.ts", "export const a = 1;\n");
        write(
            root,
            "caller.ts",
            "export const load = () => import('./a');\n",
        );
        let mut scope = Scope::new(name, None, &[], &[], &[]).unwrap();
        assert_eq!(
            scope.evidence("a.ts", "caller.ts").unwrap().kind,
            Kind::StaticImportCandidate
        );
        write(root, "a.js", "export const a = 1;\n");
        let mut scope = Scope::new(name, None, &[], &[], &[]).unwrap();
        assert_eq!(scope.evidence("a.ts", "caller.ts"), None);
        write(
            root,
            "caller.ts",
            "export const load = (name: string) => import(`./${name}`);\n",
        );
        let mut scope = Scope::new(name, None, &[], &[], &[]).unwrap();
        assert_eq!(scope.evidence("a.ts", "caller.ts"), None);
    }

    #[test]
    fn deleted_sources_keep_observed_imports_and_path_mentions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let name = root.to_str().unwrap();
        init_git_repo_for_test(root);
        git_commit_files(
            root,
            &[
                ("a.ts", "export const a = 1;\n"),
                ("b.ts", "import { a } from './a';\nexport const b = a;\n"),
                ("notes.md", "Run ./a.ts\n"),
                ("unrelated.ts", "export const other = 1;\n"),
            ],
            "base",
        );
        std::fs::remove_file(root.join("a.ts")).unwrap();
        let diff = [DiffFile {
            old_path: "a.ts".into(),
            new_path: "/dev/null".into(),
            hunks: vec![],
            deleted_old_source: None,
        }];
        let mut scope = Scope::new(name, Some("HEAD"), &diff, &[], &[]).unwrap();
        for candidate in ["b.ts", "notes.md"] {
            let evidence = scope.evidence("a.ts", candidate);
            assert!(evidence.is_some());
            assert!(!scope.deleted_without_reference("a.ts", candidate, evidence));
        }
        let evidence = scope.evidence("a.ts", "unrelated.ts");
        assert!(scope.deleted_without_reference("a.ts", "unrelated.ts", evidence));
        assert!(!scope.deleted_without_reference("a.ts", "unreadable.ts", None));
    }

    #[test]
    fn explicit_exclusions_apply_to_sources_and_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = vec!["runtime".into()];
        let globs = vec!["cache/**".into()];
        let mut scope = Scope::new(dir.path().to_str().unwrap(), None, &[], &dirs, &globs).unwrap();
        for path in [
            "runtime/state.json",
            "nested/runtime/state.json",
            "cache/state.json",
        ] {
            assert!(!scope.source(path));
            assert!(!scope.candidate(path));
        }
        assert_eq!(scope.filters.excluded_sources, 3);
        assert_eq!(scope.filters.excluded_candidates, 3);
    }

    #[test]
    fn gitlinks_are_not_review_sources_or_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let name = root.to_str().unwrap();
        init_git_repo_for_test(root);
        git_commit_files(root, &[("a.ts", "export const a = 1;\n")], "base");
        let id = String::from_utf8(git(name, &["rev-parse", "HEAD"]).unwrap()).unwrap();
        let output = crate::git_support::isolated_git_command()
            .args([
                "update-index",
                "--add",
                "--cacheinfo",
                "160000",
                id.trim(),
                "module",
            ])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let mut scope = Scope::new(name, Some("HEAD"), &[], &[], &[]).unwrap();
        assert!(!scope.source("module"));
        assert!(!scope.candidate("module"));
        assert_eq!(scope.filters.gitlink_sources, 1);
        assert_eq!(scope.filters.gitlink_candidates, 1);
    }

    /// 差分の取得で取れた prefix を渡すと、Scope は prefix を問い合わせ直さずにそれを使い、
    /// 自分で問い合わせた場合と同じ tree の一覧になる (サブディレクトリからの実行)。
    #[test]
    fn known_prefix_gives_the_same_scope_as_asking_git() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        init_git_repo_for_test(root);
        git_commit_files(
            root,
            &[
                ("app/a.ts", "export const a = 1;\n"),
                ("root.ts", "export const r = 1;\n"),
            ],
            "base",
        );
        let app = root.join("app");
        let name = app.to_str().unwrap();
        let asked = Scope::new(name, Some("HEAD"), &[], &[], &[]).unwrap();
        let known =
            Scope::with_known_prefix(name, Some(b"app/"), Some("HEAD"), &[], &[], &[]).unwrap();
        assert_eq!(asked.old_paths, HashSet::from(["a.ts".to_owned()]));
        assert_eq!(known.old_paths, asked.old_paths);
        assert_eq!(known.gitlinks, asked.gitlinks);
        assert_eq!(known.filters, asked.filters);
        // 渡した prefix を使っている (問い合わせ直していれば `app/` になり、一覧が空にならない)。
        let other =
            Scope::with_known_prefix(name, Some(b"other/"), Some("HEAD"), &[], &[], &[]).unwrap();
        assert!(other.old_paths.is_empty());
    }

    /// 一覧系の git 出力は 2 MiB を超えても読み切る。メタデータ用の上限で切っていたころは、
    /// 2 万ファイル規模で tree の一覧が取れず (`metadata_unavailable`)、gitlink が起点になっていた。
    #[test]
    fn large_tree_listings_keep_metadata_and_gitlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let name = root.to_str().unwrap();
        init_git_repo_for_test(root);
        git_commit_files(root, &[("a.ts", "export const a = 1;\n")], "base");
        let text = |args: &[&str]| {
            String::from_utf8(git(name, args).unwrap())
                .unwrap()
                .trim()
                .to_owned()
        };
        let head = text(&["rev-parse", "HEAD"]);
        let blob = text(&["rev-parse", "HEAD:a.ts"]);
        // 作業ツリーには書かず、長い名前のエントリを index に積んで 2 MiB を超えさせる。
        let mut entries = String::new();
        for n in 0..8000 {
            entries.push_str(&format!("100644 {blob}\tbulk/{n:05}{}\n", "x".repeat(235)));
        }
        entries.push_str(&format!("160000 {head}\tmodule\n"));
        let mut child = std::process::Command::new("git")
            .args(["update-index", "--index-info"])
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), entries.as_bytes()).unwrap();
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        let output = std::process::Command::new("git")
            .args(["commit", "-qm", "bulk"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let listing = std::process::Command::new("git")
            .args(["ls-tree", "--full-tree", "-r", "-z", "HEAD"])
            .current_dir(root)
            .output()
            .unwrap()
            .stdout;
        assert!(
            listing.len() > 2 * 1024 * 1024,
            "前提: 一覧が 2 MiB を超える"
        );

        let mut scope = Scope::new(name, Some("HEAD"), &[], &[], &[]).unwrap();
        assert!(!scope.filters.metadata_unavailable);
        assert!(!scope.source("module"), "gitlink は起点にしない");
        assert_eq!(scope.filters.gitlink_sources, 1);
        assert!(scope.source("a.ts"), "通常のファイルは起点のまま");
    }

    #[test]
    fn staged_imports_and_documents_use_captured_blobs_instead_of_worktree_edits() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let name = root.to_str().unwrap();
        init_git_repo_for_test(root);
        git_commit_files(
            root,
            &[
                ("source.ts", "export const value = 1;\n"),
                (
                    "caller.ts",
                    "import { value } from './source';\nexport const output = value;\n",
                ),
                ("notes.md", "Check source.ts\n"),
            ],
            "base",
        );
        let base = String::from_utf8(git(name, &["rev-parse", "HEAD"]).unwrap()).unwrap();
        let base = base.trim();
        write(root, "source.ts", "export const value = 2;\n");
        let output = crate::git_support::isolated_git_command()
            .args(["add", "source.ts"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success());
        write(root, "caller.ts", "export const output = 1;\n");
        write(root, "notes.md", "No reference\n");
        let diff = crate::commands::git_input::run_git_diff(name, base, true).unwrap();
        let super::super::index_snapshot::PreparedSnapshot::Ready(snapshot) =
            super::super::index_snapshot::IndexSnapshot::prepare(
                name,
                base,
                &diff,
                super::super::index_snapshot::SnapshotBudget::DEFAULT,
            )
            .unwrap()
        else {
            panic!("small staged tree must be materialized");
        };
        // 解析対象の言語でないファイルも index の内容のまま書き出す (作業ツリーの編集は入れない)。
        assert_eq!(
            std::fs::read_to_string(Path::new(snapshot.dir().unwrap()).join("notes.md")).unwrap(),
            "Check source.ts\n"
        );
        let mut scope = Scope::new(snapshot.dir().unwrap(), Some(base), &[], &[], &[]).unwrap();
        assert_eq!(
            scope.evidence("source.ts", "caller.ts").unwrap().kind,
            Kind::StaticImportCandidate
        );
        assert_eq!(
            scope.evidence("source.ts", "notes.md").unwrap().kind,
            Kind::PathMention
        );
        let mut worktree = Scope::new(name, Some(base), &[], &[], &[]).unwrap();
        assert_eq!(worktree.evidence("source.ts", "caller.ts"), None);
        assert_eq!(worktree.evidence("source.ts", "notes.md"), None);
    }

    #[test]
    fn budget_limits_keep_dependency_absence_unproven() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let name = root.to_str().unwrap();
        write(root, "a.ts", "export const a = 1;\n");
        std::fs::File::create(root.join("large.ts"))
            .unwrap()
            .set_len(SINGLE_FILE_LIMIT as u64 + 1)
            .unwrap();
        let mut scope = Scope::new(name, None, &[], &[], &[]).unwrap();
        assert_eq!(scope.evidence("a.ts", "large.ts"), None);
        assert!(scope.filters.dependency_probes_truncated);
        assert!(scope.cache.get("large.ts").unwrap().is_none());
    }

    #[test]
    fn unresolved_imports_other_languages_and_parse_errors_keep_deleted_history() {
        for (source, candidate, text) in [
            ("src/foo.rs", "src/lib.rs", "mod foo;\n"),
            (
                "src/foo.ts",
                "src/caller.ts",
                "import { foo } from '@/foo';\n",
            ),
            ("pkg/foo.py", "pkg/caller.py", "from .foo import value\n"),
            (
                "src/foo.ts",
                "src/caller.ts",
                "import { foo } from './foo';\nfunction broken(\n",
            ),
            (
                "src/foo.ts",
                "src/caller.ts",
                "const loader = require; loader('./foo');\n",
            ),
            (
                "src/foo.ts",
                "src/caller.ts",
                "const name = 'foo'; import(`./${name}`);\n",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write(root, candidate, text);
            let diff = [DiffFile {
                old_path: source.into(),
                new_path: "/dev/null".into(),
                hunks: vec![],
                deleted_old_source: None,
            }];
            let mut scope = Scope::new(root.to_str().unwrap(), None, &diff, &[], &[]).unwrap();
            let evidence = scope.evidence(source, candidate);
            assert!(
                !scope.deleted_without_reference(source, candidate, evidence),
                "{source}/{candidate}"
            );
        }
    }

    #[test]
    fn path_mentions_accept_sentence_punctuation_without_matching_longer_paths() {
        assert!(mentions("see src/foo.ts.", "src/foo.ts"));
        assert!(mentions("see src/foo.ts.\n", "src/foo.ts"));
        assert!(!mentions("see src/foo.ts.bak", "src/foo.ts"));
        assert!(!mentions("see other/src/foo.ts", "src/foo.ts"));
    }

    #[test]
    fn unextracted_reexports_require_imports_and_python_submodules_stay_unassessed() {
        for code in [
            "export * from './foo';\n",
            "export { foo } from './foo';\n",
            "import foo = require('./foo');\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write(root, "src/index.ts", code);
            let diff = [DiffFile {
                old_path: "src/foo.ts".into(),
                new_path: "/dev/null".into(),
                hunks: vec![],
                deleted_old_source: None,
            }];
            let mut scope = Scope::new(root.to_str().unwrap(), None, &diff, &[], &[]).unwrap();
            let evidence = scope.evidence("src/foo.ts", "src/index.ts");
            assert!(
                !scope.deleted_without_reference("src/foo.ts", "src/index.ts", evidence),
                "{code}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "pkg/__init__.py", "");
        write(root, "pkg/caller.py", "from pkg import foo\n");
        let diff = [DiffFile {
            old_path: "pkg/foo.py".into(),
            new_path: "/dev/null".into(),
            hunks: vec![],
            deleted_old_source: None,
        }];
        let mut scope = Scope::new(root.to_str().unwrap(), None, &diff, &[], &[]).unwrap();
        let evidence = scope.evidence("pkg/foo.py", "pkg/caller.py");
        assert!(!scope.deleted_without_reference("pkg/foo.py", "pkg/caller.py", evidence));
    }

    #[test]
    #[cfg(unix)]
    fn dependency_probes_do_not_follow_symlinks_outside_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(dir.path(), "a.ts", "export const a = 1;\n");
        write(outside.path(), "notes.md", "a.ts\n");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("external")).unwrap();
        let mut scope = Scope::new(dir.path().to_str().unwrap(), None, &[], &[], &[]).unwrap();
        assert_eq!(scope.evidence("a.ts", "external/notes.md"), None);
    }
}
