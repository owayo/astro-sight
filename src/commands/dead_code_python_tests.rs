//! dead-code の段階で、Python のテストランナーがモジュールをまたいで集めるテストを候補と照合する。
//!
//! ファイル単位の除外 (`ExportSurfaceContext`) は、同じファイルの中で分かる入口だけを候補から
//! 外す。ここでは走査集合の Python のファイルすべての facts から import をたどって基底と束縛を
//! 解決し (`engine::symbols::ProjectTests`)、残った候補のうち入口になったもの (別ファイルの
//! テストクラスが継承する基底の `test*`、テストモジュールへ import した `Test*` のクラス、
//! 別ファイルの TestCase の連鎖など) を外す。名前の解決を確かめられないために集めるかどうか
//! 分からない候補と、中身を見られないファイルのためにたどれなかった候補、構文エラーのある
//! ファイルのクラスとテストランナーが名前で集めうる関数は、dead と断定せず unverified に回す。

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::engine::parse_budget::ParseBudget;
use crate::engine::symbols::{
    ProjectModule, ProjectTests, PythonModuleFacts, is_runner_member_name, is_runner_name,
};
use crate::git_support::normalize_workspace_separators;
use crate::language::LangId;

/// 定義のキー (`dir` 相対のパス, 定義の開始位置)。
type DefinitionKey = (String, (usize, usize));

/// モジュールをまたぐ Python のテストの入口。
#[derive(Default)]
pub(super) struct PythonTestLiveness {
    live: HashSet<DefinitionKey>,
    uncertain: HashSet<DefinitionKey>,
    /// テストランナーが名前で呼ぶメンバーは、どのクラスのものも集めるかどうか分からない。
    any_member_uncertain: bool,
    /// どのクラスも、集めるかどうか分からない。
    any_class_uncertain: bool,
    /// 構文エラーのあるファイル (`dir` 相対)。エラー回復の形によっては、クラスの本体や基底を
    /// 取りこぼしうる。
    broken: HashSet<String>,
    /// 中身を見られなかった Python のファイル (`dir` 相対、パス順, 原因)。
    unobserved: Vec<(String, String)>,
}

/// 判定の対象になる候補の種類。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Class,
    /// クラスのメンバーのうち、テストランナーが名前で集めるか呼ぶもの。
    Member,
}

/// 候補 (qualname, kind) の結果が、モジュールをまたぐ判定で変わりうるか (クラスと、
/// テストランナーが名前で集めうる関数とメソッド。構文エラーのあるファイルの module 直下の
/// 関数を含む)。
pub(super) fn is_python_test_target(name: &str, kind: &str) -> bool {
    target(name, kind).is_some()
        || (kind == "function" && is_runner_name(name.rsplit('.').next().unwrap_or(name)))
}

/// 候補 (qualname, kind) が、モジュールをまたぐ判定の対象になる定義か。
fn target(name: &str, kind: &str) -> Option<Target> {
    match kind {
        "class" => Some(Target::Class),
        "function" | "method" => name
            .rsplit_once('.')
            .is_some_and(|(_, member)| is_runner_member_name(member))
            .then_some(Target::Member),
        _ => None,
    }
}

impl PythonTestLiveness {
    /// `starts` は `candidates` と同じ順の、Python の候補の定義の開始位置。`candidate_facts` は
    /// 候補の抽出で作ったファイル単位の facts (`dir` 相対のパス → facts、構文解析をやり直さない)。
    pub(super) fn build(
        candidates: &[(String, String, String, LangId)],
        starts: &[Option<(usize, usize)>],
        mut candidate_facts: HashMap<String, PythonModuleFacts>,
        canonical_dir: &Path,
        scan_files: &[PathBuf],
        parse_budget: ParseBudget,
    ) -> Self {
        // 集めうる候補 (クラスか、テストランナーが名前で集めるか呼ぶ関数とメソッド) が残って
        // いなければ読まない。
        let has_target = candidates
            .iter()
            .zip(starts)
            .any(|((name, kind, _, lang), start)| {
                *lang == LangId::Python && start.is_some() && is_python_test_target(name, kind)
            });
        if !has_target {
            return Self::default();
        }
        let mut files: Vec<(String, PathBuf)> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for path in scan_files {
            if crate::engine::refs::detect_source_lang(path) != Some(LangId::Python) {
                continue;
            }
            let Ok(canonical) = path.canonicalize() else {
                continue;
            };
            let Ok(relative) = canonical.strip_prefix(canonical_dir) else {
                continue;
            };
            let relative = normalize_workspace_separators(&relative.to_string_lossy());
            // 型スタブは実行時のクラスを持たない。
            if relative.ends_with(".pyi") {
                continue;
            }
            if seen.insert(relative.clone()) {
                files.push((relative, canonical));
            }
        }
        // 走査集合に無い候補のファイル (起こらないはず) も、facts があれば入れる。
        for relative in candidate_facts.keys() {
            if seen.insert(relative.clone()) {
                files.push((relative.clone(), canonical_dir.join(relative)));
            }
        }
        files.sort();

        // 候補の抽出で facts を作っていないファイルだけを、予算の中で並列に構文解析する。
        let to_parse: Vec<&(String, PathBuf)> = files
            .iter()
            .filter(|(relative, _)| !candidate_facts.contains_key(relative))
            .collect();
        let size = |file: &&(String, PathBuf)| std::fs::metadata(&file.1).map_or(0, |m| m.len());
        let parse = |file: &&(String, PathBuf)| parse_facts(&file.1);
        let parsed: Vec<Result<PythonModuleFacts, String>> =
            crate::engine::refs::build_bounded_pool()
                .ok()
                .and_then(|pool| {
                    pool.install(|| {
                        crate::engine::parse_budget::map_with_budget(
                            &to_parse,
                            parse_budget,
                            size,
                            parse,
                        )
                        .ok()
                    })
                })
                .unwrap_or_else(|| to_parse.iter().map(parse).collect());
        let mut parsed: HashMap<&str, Result<PythonModuleFacts, String>> = to_parse
            .iter()
            .map(|(relative, _)| relative.as_str())
            .zip(parsed)
            .collect();

        let mut liveness = Self::default();
        let mut modules = Vec::with_capacity(files.len());
        let mut unobserved_modules = Vec::new();
        for (relative, path) in &files {
            let pytest_module = super::dead_code::file_name_is_pytest_module(Some(relative));
            let facts = match candidate_facts.remove(relative) {
                Some(facts) => facts,
                None => match parsed.remove(relative.as_str()) {
                    Some(Ok(facts)) => facts,
                    Some(Err(cause)) => {
                        // `import` も `class` も書いていないファイルは、どのクラスも継承・束縛しない。
                        if may_bind_names(path) {
                            liveness.unobserved.push((relative.clone(), cause));
                            unobserved_modules.push((relative.clone(), pytest_module));
                        }
                        continue;
                    }
                    None => continue,
                },
            };
            modules.push(ProjectModule {
                path: relative.clone(),
                facts,
                pytest_module,
            });
        }

        let project = ProjectTests::new(modules, unobserved_modules);
        let collected = project.collect();
        let key = |&(module, start): &(usize, (usize, usize))| {
            (project.modules()[module].path.clone(), start)
        };
        liveness.live = collected.live.iter().map(key).collect();
        liveness.uncertain = collected.uncertain.iter().map(key).collect();
        liveness.any_member_uncertain = collected.any_member_uncertain;
        liveness.any_class_uncertain = collected.any_class_uncertain;
        liveness.broken = project
            .modules()
            .iter()
            .filter(|module| module.facts.has_error)
            .map(|module| module.path.clone())
            .collect();
        liveness
    }

    /// テストランナーが集めるか名前で呼ぶ候補か。
    pub(super) fn is_live(&self, file: &str, start: Option<(usize, usize)>) -> bool {
        start.is_some_and(|start| self.live.contains(&(file.to_string(), start)))
    }

    /// 集めるかどうか分からない候補か (名前の解決を確かめられないもの、見えない基底や中身を
    /// 見られないファイルをたどったもの)。
    pub(super) fn is_uncertain(
        &self,
        file: &str,
        name: &str,
        kind: &str,
        start: Option<(usize, usize)>,
    ) -> bool {
        let Some(start) = start else {
            return false;
        };
        // 構文エラーのあるファイルのクラスと、テストランナーが名前で集めうる関数とメソッド
        // (エラー回復でクラスの外に出たメソッドを含む)。
        if self.broken.contains(file)
            && (kind == "class"
                || (matches!(kind, "function" | "method")
                    && is_runner_name(name.rsplit('.').next().unwrap_or(name))))
        {
            return true;
        }
        match target(name, kind) {
            Some(Target::Class) if self.any_class_uncertain => true,
            Some(Target::Member) if self.any_member_uncertain => true,
            Some(_) => self.uncertain.contains(&(file.to_string(), start)),
            None => false,
        }
    }

    /// 中身を見られなかった Python のファイル (`dir` 相対、パス順, 原因)。
    pub(super) fn unobserved_files(&self) -> &[(String, String)] {
        &self.unobserved
    }
}

/// 1 ファイルを読んで facts を作る。失敗したら申告に載せる原因を返す。
fn parse_facts(path: &Path) -> Result<PythonModuleFacts, String> {
    let Some(source) = crate::engine::bounded_read::read_input_file(path) else {
        return Err(crate::engine::refs::read_failure_cause(path));
    };
    let tree = crate::engine::parser::parse_source(&source, LangId::Python)
        .map_err(|_| crate::engine::refs::PARSE_FAILURE_CAUSE.to_string())?;
    Ok(PythonModuleFacts::collect(tree.root_node(), &source))
}

/// 中身を見られなかったファイルが、名前を束縛しうるか (`import` か `class` の字面がある)。
/// どちらも無ければ、どのクラスも継承・再 export しない。読み込みの上限を超えたファイルも
/// メモリを上限付きのまま先頭から走査し、開けない・途中で読めない・走査の上限を超えるときは
/// 束縛しうるとする。
fn may_bind_names(path: &Path) -> bool {
    /// 走査するファイルの上限 (参照の集計のストリーム走査と同じ 1 GiB)。
    const LIMIT: u64 = 1024 * 1024 * 1024;
    const CHUNK: usize = 8 * 1024 * 1024;
    let Ok(mut file) = std::fs::File::open(path) else {
        return true;
    };
    if file
        .metadata()
        .map_or(true, |metadata| metadata.len() > LIMIT)
    {
        return true;
    }
    let needles = [
        memchr::memmem::Finder::new(b"import"),
        memchr::memmem::Finder::new(b"class"),
    ];
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    let mut total: u64 = 0;
    loop {
        let read = match file.read(&mut chunk) {
            Ok(0) => return false,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return true,
        };
        total += read as u64;
        if total > LIMIT {
            return true;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if needles.iter().any(|needle| needle.find(&buffer).is_some()) {
            return true;
        }
        // 次のチャンクと連結して、境界をまたぐ字面を拾えるよう末尾だけを残す。
        let keep = buffer.len().min(b"import".len() - 1);
        buffer.drain(..buffer.len() - keep);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 中身を見られないファイルは、`import` か `class` の字面があるときだけ束縛しうるとする。
    /// 開けないファイルは束縛しうるとする。
    #[test]
    fn unobserved_files_without_import_or_class_bind_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data.py");
        std::fs::write(&data, "VALUES = [1, 2, 3]\n").unwrap();
        assert!(!may_bind_names(&data));
        for body in ["import os\n", "class A:\n    pass\n", "from x import y\n"] {
            let path = dir.path().join("module.py");
            std::fs::write(&path, body).unwrap();
            assert!(may_bind_names(&path), "{body}");
        }
        assert!(may_bind_names(&dir.path().join("missing.py")));
    }
}
