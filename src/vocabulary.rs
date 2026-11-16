//! プロジェクト辞書向けの識別子分割と、上限付きのシンボル抽出。
use crate::{
    engine::{
        generated::{self, GeneratedVerdict},
        gitattributes::{AttributeLookup, AttributeResolver},
    },
    error::{AstroError, ErrorCode},
    models::symbol::Symbol,
    service::AppService,
};
use anyhow::Result;
use serde::Serialize;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    ffi::{OsStr, OsString},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

/// 走査と解析で扱うファイルの数の上限。
const MAX_FILES: usize = 10_000;
/// 走査と解析の時間の上限。
const TIME_LIMIT: Duration = Duration::from_secs(30);
/// 読むソースの大きさの上限。
const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
/// 変えると、ほかのファイルの選別 (ignore の規則・生成物の判定) が変わるファイルの名前。
const POLICY_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".gitattributes"];

/// camelCase、頭字語、数字、区切り文字の境目で分ける。表記は変えない。
pub fn split_identifier(input: &str) -> Vec<String> {
    let chars: Vec<_> = input.char_indices().collect();
    let mut result = vec![];
    let mut start = None;
    for (i, (offset, c)) in chars.iter().copied().enumerate() {
        if !c.is_alphanumeric() {
            if let Some(start) = start.take() {
                result.push(input[start..offset].to_owned());
            }
            continue;
        }
        let previous = i.checked_sub(1).and_then(|i| chars.get(i)).map(|(_, c)| *c);
        let next = chars.get(i + 1).map(|(_, c)| *c);
        let boundary = start.is_some()
            && previous.is_some_and(|p| {
                (p.is_lowercase() && c.is_uppercase())
                    || (p.is_uppercase()
                        && c.is_uppercase()
                        && next.is_some_and(char::is_lowercase))
                    || (p.is_ascii_digit() != c.is_ascii_digit())
            });
        if boundary {
            if let Some(start) = start.replace(offset) {
                result.push(input[start..offset].to_owned());
            }
        } else if start.is_none() {
            start = Some(offset);
        }
    }
    if let Some(start) = start {
        result.push(input[start..].to_owned());
    }
    result
}
#[derive(Debug, Serialize)]
pub struct ScannedFile {
    pub path: String,
    pub symbols: Vec<Symbol>,
}
#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub files: Vec<ScannedFile>,
    pub removed: Vec<String>,
    pub skipped_generated: usize,
    pub skipped_unsupported: usize,
    /// プロジェクト全体を走査した結果か (`source_files` が無い、または差分の要求を全体の走査に切り替えた)。
    /// true なら `removed` は空で、`complete` が true のときだけ辞書全体をこの結果で置き換えられる。
    pub full_scan: bool,
    /// 差分の要求を全体の走査に切り替えた理由。
    pub full_scan_reason: Option<&'static str>,
    pub complete: bool,
    pub diagnostics: Vec<String>,
}

/// 差分で要求されたパスの振り分け。
enum Plan {
    /// 全体を走査し直す (理由)。
    Full(&'static str),
    Changes {
        /// 実際の綴りで、全体の走査と同じ選別にかけるパス (根からの相対パス、`/` 区切り)。
        candidates: BTreeSet<String>,
        /// 無くなったパス、走査の届かないパス (途中が symlink やファイル)、実際の綴りと違う要求の綴り。
        removed: BTreeSet<String>,
        /// 実際の綴りを決められなかったパス。
        unjudged: BTreeSet<String>,
    },
}

/// 要求のパスを根からたどった結果。
enum Resolved {
    /// 無い、または走査の届かない場所 (途中が symlink やファイル)。
    Absent,
    /// symlink でないディレクトリ。
    Directory,
    /// ディレクトリでないもの (ファイル・symlink など) の、実際の綴りの相対パス。
    Entry(String),
    /// 実際の綴りを決められない。
    Unresolved,
}

/// 走査で集めたファイル。
struct Inventory {
    /// 根からの相対パス (`/` 区切り) と、そのパス。名前の順。
    files: Vec<(String, PathBuf)>,
    /// 上限・読めないエントリ・読めない ignore の規則で、走査を最後まで確かめられなかったか。
    cut_short: bool,
}

impl AppService {
    /// プロジェクト辞書のためにシンボルを集める。`changed` が無ければプロジェクト全体を、あれば要求した相対パスだけを、
    /// どちらも同じ選別 (ignore の規則と隠しファイル・言語・大きさ・生成物の判定) で読む (Issue #395)。
    ///
    /// 差分の結果では、要求した各パスが `files` か `removed` のどちらかに入る。どちらも、そのパスと、その下のパス
    /// (`path + "/"` で始まるもの) の辞書の項目を表す: `removed` のパスは消し、`files` のパスはその内容で置き換える。
    /// 全体の走査で返らないファイル (ignore の規則で除外・隠し・symlink・未対応・生成物・大きすぎる・解析できない) に
    /// なったパスも `removed` に入る。要求の綴りが実際の綴りと違えば (大文字小文字を区別しないファイルシステム)、
    /// 要求の綴りを `removed` に入れ、実際の綴りで読む。上限や走査の失敗で判定できなかったパスはどちらにも入れず、
    /// `complete` を false にして `diagnostics` に残す。
    ///
    /// 既存のディレクトリ (名前を変えた先・作ったディレクトリ)、プロジェクトの根 (`.`)、ignore や属性の規則のファイル
    /// (`.gitignore`・`.ignore`・`.gitattributes`・`.git/info/exclude`)、上限を超える数のパスを要求されたら、全体の
    /// 走査に切り替える (`full_scan`)。消えたディレクトリは、ほかのパスと同じく `removed` に入る。
    pub fn scan_symbols(&self, dir: &str, changed: Option<&[String]>) -> Result<ScanReport> {
        let root = self.validate_dir(dir)?.canonicalize()?;
        let started = Instant::now();
        let mut report = ScanReport {
            files: vec![],
            removed: vec![],
            skipped_generated: 0,
            skipped_unsupported: 0,
            full_scan: true,
            full_scan_reason: None,
            complete: true,
            diagnostics: vec![],
        };
        let (wanted, mut removed, unjudged) = match changed {
            None => (None, BTreeSet::new(), BTreeSet::new()),
            Some(changed) => match plan_changes(&root, changed)? {
                Plan::Full(reason) => {
                    report.full_scan_reason = Some(reason);
                    (None, BTreeSet::new(), BTreeSet::new())
                }
                Plan::Changes {
                    candidates,
                    removed,
                    unjudged,
                } => {
                    report.full_scan = false;
                    (Some(candidates), removed, unjudged)
                }
            },
        };
        let inventory = inventory(&root, wanted.as_ref(), started, &mut report);
        let resolver = AttributeResolver::open(&root);
        let mut lookup = resolver.lookup();
        let mut judged = HashSet::new();
        for (relative, path) in &inventory.files {
            if started.elapsed() > TIME_LIMIT || report.files.len() >= MAX_FILES {
                report.complete = false;
                report.diagnostics.push("analysis limit exceeded".into());
                break;
            }
            judged.insert(relative.as_str());
            match self.scan_file(&root, relative, path, &mut lookup, &mut report)? {
                Some(file) => report.files.push(file),
                // 差分の要求で、全体の走査でも返らなくなったファイル。
                None if wanted.is_some() => {
                    removed.insert(relative.clone());
                }
                None => {}
            }
        }
        if let Some(wanted) = &wanted {
            let listed: HashSet<&str> = inventory
                .files
                .iter()
                .map(|(relative, _)| relative.as_str())
                .collect();
            for candidate in wanted {
                if judged.contains(candidate.as_str()) {
                    continue;
                }
                if inventory.cut_short || listed.contains(candidate.as_str()) {
                    // 走査を最後まで確かめられなかったか、選別の前に上限に達した。除外と決めない。
                    report.complete = false;
                    report.diagnostics.push(format!("not judged: {candidate}"));
                } else {
                    // 全体の走査でも返らない (ignore の規則・隠しファイル・symlink など)。
                    removed.insert(candidate.clone());
                }
            }
        }
        for path in unjudged {
            report.complete = false;
            report.diagnostics.push(format!("not judged: {path}"));
        }
        report.files.sort_by(|a, b| a.path.cmp(&b.path));
        report.removed = removed.into_iter().collect();
        Ok(report)
    }

    /// 1 ファイルを選別して読む。返さない (選別で外した・読めない) ときは None。
    fn scan_file(
        &self,
        root: &Path,
        relative: &str,
        path: &Path,
        lookup: &mut AttributeLookup<'_>,
        report: &mut ScanReport,
    ) -> Result<Option<ScannedFile>> {
        let canonical = path.canonicalize()?;
        if !canonical.starts_with(root) || std::fs::symlink_metadata(path)?.file_type().is_symlink()
        {
            return Err(AstroError::new(
                ErrorCode::PathOutOfBounds,
                "project path escapes its root",
            )
            .into());
        }
        let source_path = canonical.to_str().ok_or_else(|| {
            AstroError::new(ErrorCode::InvalidRequest, "source path is not UTF-8")
        })?;
        let mut header = [0u8; 4096];
        let len = std::io::Read::read(&mut std::fs::File::open(&canonical)?, &mut header)?;
        if crate::language::LangId::detect(camino::Utf8Path::new(source_path), &header[..len])
            .is_err()
        {
            report.skipped_unsupported += 1;
            return Ok(None);
        }
        if std::fs::metadata(&canonical)?.len() > MAX_SOURCE_BYTES {
            report.complete = false;
            report
                .diagnostics
                .push(format!("oversized source: {relative}"));
            return Ok(None);
        }
        match generated::judge(lookup.linguist_generated(&canonical), &canonical, || {
            generated::is_auto_generated(&canonical)
        }) {
            GeneratedVerdict::Generated(_) => {
                report.skipped_generated += 1;
                return Ok(None);
            }
            GeneratedVerdict::Undetermined(_) => {
                report.complete = false;
                report
                    .diagnostics
                    .push(format!("attributes unavailable: {relative}"));
                return Ok(None);
            }
            GeneratedVerdict::NotGenerated => {}
        }
        match self.extract_symbols(source_path) {
            Ok(response) => {
                if !response.diagnostics.is_empty() {
                    report.complete = false;
                    report
                        .diagnostics
                        .push(format!("parse diagnostics: {relative}"));
                }
                Ok(Some(ScannedFile {
                    path: relative.to_owned(),
                    symbols: response.symbols.unwrap_or_default(),
                }))
            }
            Err(error)
                if error
                    .downcast_ref::<AstroError>()
                    .is_some_and(|e| e.code == ErrorCode::UnsupportedLanguage) =>
            {
                report.skipped_unsupported += 1;
                Ok(None)
            }
            Err(_) => {
                report.complete = false;
                report
                    .diagnostics
                    .push(format!("analysis failed: {relative}"));
                Ok(None)
            }
        }
    }
}

/// 全体と差分の走査で同じ規則 (ignore の既定: `.gitignore`・`.ignore`・`.git/info/exclude`・global の除外・隠し
/// ファイル) を当てる walker で、ファイルを集める。名前の順に辿り、上限で打ち切ったときに残る部分を決定的にする。
/// `wanted` があれば、その祖先とそれ自身だけを辿る (ignore の規則は `filter_entry` より先に当たり、根そのものは
/// 規則にかからないので、要求したファイルから辿り始めない)。
fn inventory(
    root: &Path,
    wanted: Option<&BTreeSet<String>>,
    started: Instant,
    report: &mut ScanReport,
) -> Inventory {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .follow_links(false)
        .sort_by_file_name(|a: &OsStr, b: &OsStr| a.cmp(b));
    if let Some(wanted) = wanted {
        let mut keep = HashSet::new();
        for relative in wanted {
            let mut path = root.to_path_buf();
            for part in relative.split('/') {
                path.push(part);
                keep.insert(path.clone());
            }
        }
        let keep = Arc::new(keep);
        builder.filter_entry(move |entry| keep.contains(entry.path()));
    }
    let mut inventory = Inventory {
        files: vec![],
        cut_short: false,
    };
    for entry in builder.build() {
        if started.elapsed() > TIME_LIMIT || inventory.files.len() >= MAX_FILES {
            report.complete = false;
            report.diagnostics.push("inventory limit exceeded".into());
            inventory.cut_short = true;
            break;
        }
        match entry {
            Ok(entry) => {
                // ignore の規則のファイルを読めなかった (規則の一部しか当たっていない)。
                if entry.error().is_some() {
                    report.complete = false;
                    report.diagnostics.push(format!(
                        "ignore rules unreadable: {}",
                        relative_key(root, entry.path())
                    ));
                    inventory.cut_short = true;
                }
                if entry.file_type().is_some_and(|f| f.is_file()) {
                    let relative = relative_key(root, entry.path());
                    inventory.files.push((relative, entry.into_path()));
                }
            }
            Err(_) => {
                report.complete = false;
                report.diagnostics.push("unreadable project entry".into());
                inventory.cut_short = true;
            }
        }
    }
    inventory
}

/// 差分の要求を振り分ける。
fn plan_changes(root: &Path, changed: &[String]) -> Result<Plan> {
    if changed.len() > MAX_FILES {
        return Ok(Plan::Full("too many changed paths"));
    }
    let mut listings = HashMap::new();
    let mut candidates = BTreeSet::new();
    let mut removed = BTreeSet::new();
    let mut unjudged = BTreeSet::new();
    for requested in changed {
        let parts = request_components(requested)?;
        if parts.is_empty() {
            return Ok(Plan::Full("project root requested"));
        }
        if is_policy_file(&parts) {
            return Ok(Plan::Full("ignore or attribute rules changed"));
        }
        let spelled = join_components(&parts);
        match resolve(root, &parts, &mut listings)? {
            Resolved::Absent => {
                removed.insert(spelled);
            }
            Resolved::Directory => return Ok(Plan::Full("directory changed")),
            Resolved::Entry(actual) => {
                if actual != spelled {
                    removed.insert(spelled);
                }
                candidates.insert(actual);
            }
            Resolved::Unresolved => {
                unjudged.insert(spelled);
            }
        }
    }
    Ok(Plan::Changes {
        candidates,
        removed,
        unjudged,
    })
}

/// 要求のパスを字句で正規化した部分 (`.` を除く)。空の文字列と、根の外へ出得るもの (`..`・絶対パス) は拒む。
fn request_components(relative: &str) -> Result<Vec<OsString>> {
    let out_of_bounds = || {
        AstroError::new(
            ErrorCode::PathOutOfBounds,
            "changed paths must stay inside the project",
        )
    };
    if relative.is_empty() {
        return Err(out_of_bounds().into());
    }
    let mut parts = Vec::new();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_owned()),
            Component::CurDir => {}
            _ => return Err(out_of_bounds().into()),
        }
    }
    Ok(parts)
}

/// ignore や属性の規則を持つファイルか (変えるとほかのファイルの選別が変わる)。
fn is_policy_file(parts: &[OsString]) -> bool {
    parts
        .last()
        .is_some_and(|name| POLICY_FILE_NAMES.iter().any(|policy| name == *policy))
        || matches!(parts, [git, info, exclude] if git == ".git" && info == "info" && exclude == "exclude")
}

/// 要求のパスを根から 1 段ずつたどり、実際の綴りに直す。大文字小文字を区別しないファイルシステムでは綴りの違う
/// 要求も開けるので、そのときは親の一覧から、大文字小文字と Unicode の正規化を無視して一致するただ 1 つの名前を探す。
fn resolve(
    root: &Path,
    parts: &[OsString],
    listings: &mut HashMap<PathBuf, Option<Vec<OsString>>>,
) -> std::io::Result<Resolved> {
    let mut current = root.to_path_buf();
    let mut actual = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        let metadata = match std::fs::symlink_metadata(current.join(part)) {
            Ok(metadata) => metadata,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(Resolved::Absent);
            }
            Err(error) => return Err(error),
        };
        let listing = listings.entry(current.clone()).or_insert_with(|| {
            std::fs::read_dir(&current).ok().map(|entries| {
                entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
                    .collect()
            })
        });
        let Some(listing) = listing else {
            return Ok(Resolved::Unresolved);
        };
        let name = if listing.contains(part) {
            part.clone()
        } else {
            let folded = fold(part);
            let mut matching = listing.iter().filter(|name| fold(name) == folded);
            match (matching.next(), matching.next()) {
                (Some(name), None) => name.clone(),
                _ => return Ok(Resolved::Unresolved),
            }
        };
        current.push(&name);
        actual.push(name);
        let file_type = metadata.file_type();
        let last = index + 1 == parts.len();
        if !last && !file_type.is_dir() {
            // symlink のディレクトリやファイルの下は、全体の走査が辿らない。
            return Ok(Resolved::Absent);
        }
        if last && file_type.is_dir() {
            return Ok(Resolved::Directory);
        }
    }
    Ok(Resolved::Entry(join_components(&actual)))
}

/// 大文字小文字と Unicode の正規化を無視して比べるための形。
fn fold(name: &OsStr) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.to_string_lossy()
        .nfc()
        .collect::<String>()
        .to_lowercase()
}

fn join_components(parts: &[OsString]) -> String {
    parts
        .iter()
        .map(|part| part.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// 根からの相対パスを `/` 区切りにした辞書のキー。
fn relative_key(root: &Path, path: &Path) -> String {
    let parts: Vec<OsString> = path
        .strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_owned()),
            _ => None,
        })
        .collect();
    join_components(&parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn splits_acronyms_camel_case_digits_and_unicode() {
        for (input, want) in [
            ("SessionHandler", vec!["Session", "Handler"]),
            ("HTTPServer_v2", vec!["HTTP", "Server", "v", "2"]),
            ("hello_world", vec!["hello", "world"]),
            ("日本語_入力", vec!["日本語", "入力"]),
        ] {
            assert_eq!(split_identifier(input), want);
        }
    }
    #[test]
    fn incremental_scan_does_not_read_other_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn beta() {}\n").unwrap();
        let service = AppService::sandboxed(dir.path().into()).unwrap();
        let report = service
            .scan_symbols(dir.path().to_str().unwrap(), Some(&["a.rs".into()]))
            .unwrap();
        assert!(report.complete);
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].symbols[0].name, "alpha");
        assert!(
            service
                .scan_symbols(
                    dir.path().to_str().unwrap(),
                    Some(&["../outside.rs".into()])
                )
                .is_err()
        );
    }
    #[test]
    fn generated_sources_are_reported_and_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("gen.rs"),
            "// This file is @generated\nfn generated() {}\n",
        )
        .unwrap();
        let report = AppService::new()
            .scan_symbols(dir.path().to_str().unwrap(), Some(&["gen.rs".into()]))
            .unwrap();
        assert_eq!(report.skipped_generated, 1);
        assert!(report.files.is_empty());
        // 生成物になったパスは、辞書から消すよう `removed` に入る (Issue #395)。
        assert_eq!(report.removed, ["gen.rs"]);
    }

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn scan(root: &Path, changed: Option<&[&str]>) -> ScanReport {
        let changed: Option<Vec<String>> =
            changed.map(|paths| paths.iter().map(|path| (*path).to_owned()).collect());
        AppService::sandboxed(root.into())
            .unwrap()
            .scan_symbols(root.to_str().unwrap(), changed.as_deref())
            .unwrap()
    }

    fn paths(report: &ScanReport) -> Vec<&str> {
        report.files.iter().map(|file| file.path.as_str()).collect()
    }

    /// ignore の規則 (入れ子の `.gitignore`、除外した親の下の否定、`.ignore`) と隠しファイルを、全体の走査と同じく
    /// 差分の要求にも当てる。全体の走査で返らないパスは `removed` に入る (Issue #395)。
    #[test]
    fn incremental_scan_applies_the_full_scan_ignore_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        write(
            root,
            ".gitignore",
            "ignored.rs\nblocked/\n!blocked/keep.rs\n",
        );
        write(root, ".ignore", "dot_ignored.rs\n");
        write(root, "blocked/.gitignore", "!keep.rs\n");
        write(root, "src/.gitignore", "*.rs\n!keep.rs\n");
        write(root, "kept.rs", "pub struct CacheRequest;\n");
        write(root, "ignored.rs", "pub struct HiddenRequest;\n");
        let requested = [
            "kept.rs",
            "ignored.rs",
            "dot_ignored.rs",
            "blocked/keep.rs",
            "src/keep.rs",
            "src/drop.rs",
            ".hidden/keep.rs",
        ];
        for path in &requested[2..] {
            write(root, path, "pub struct Item;\n");
        }
        let full = scan(root, None);
        assert!(
            full.full_scan && full.complete && full.removed.is_empty(),
            "{full:?}"
        );
        assert_eq!(paths(&full), ["kept.rs", "src/keep.rs"]);
        for path in requested {
            let report = scan(root, Some(&[path]));
            assert!(!report.full_scan && report.complete, "{path}: {report:?}");
            if paths(&full).contains(&path) {
                assert_eq!(paths(&report), [path], "{report:?}");
                assert!(report.removed.is_empty(), "{report:?}");
            } else {
                assert!(report.files.is_empty(), "{path}: {report:?}");
                assert_eq!(report.removed, [path], "{report:?}");
            }
        }
        let report = scan(root, Some(&requested));
        assert_eq!(paths(&report), paths(&full));
        let mut excluded: Vec<&str> = requested
            .into_iter()
            .filter(|path| !paths(&full).contains(path))
            .collect();
        excluded.sort();
        assert_eq!(report.removed, excluded);
    }

    /// 全体の走査で返らなくなったパス (消えた・ignore された・生成物・未対応) と、祖先ごと消えたパスは `removed` に
    /// 入る。`./` を外した綴りで返し、同じパスを 2 度数えない (Issue #395)。
    #[test]
    fn incremental_scan_removes_paths_that_leave_the_full_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        write(root, "kept.rs", "pub struct Kept;\n");
        write(root, "later_ignored.rs", "pub struct Later;\n");
        write(root, "notes.txt", "plain text\n");
        write(
            root,
            "gen.rs",
            "// This file is @generated\npub struct Generated;\n",
        );
        write(root, ".gitignore", "later_ignored.rs\n");
        let report = scan(
            root,
            Some(&[
                "./kept.rs",
                "kept.rs",
                "later_ignored.rs",
                "notes.txt",
                "gen.rs",
                "deleted.rs",
                "old/sub/a.rs",
            ]),
        );
        assert!(!report.full_scan && report.complete, "{report:?}");
        assert_eq!(paths(&report), ["kept.rs"]);
        assert_eq!(
            report.removed,
            [
                "deleted.rs",
                "gen.rs",
                "later_ignored.rs",
                "notes.txt",
                "old/sub/a.rs"
            ]
        );
        assert_eq!(
            (report.skipped_generated, report.skipped_unsupported),
            (1, 1)
        );
        // 対照: 全体の走査は `removed` を返さない。
        let full = scan(root, None);
        assert_eq!(paths(&full), ["kept.rs"]);
        assert!(full.removed.is_empty(), "{full:?}");
    }

    /// 既存のディレクトリ、プロジェクトの根、ignore や属性の規則のファイルを要求されたら、全体の走査に切り替える
    /// (Issue #395)。
    #[test]
    fn incremental_scan_falls_back_to_a_full_scan() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        write(root, "kept.rs", "pub struct Kept;\n");
        write(root, "src/keep.rs", "pub struct Keep;\n");
        let full = scan(root, None);
        for request in [
            &["src"][..],
            &["kept.rs", "src"],
            &["."],
            &[".gitignore"],
            &["src/.gitignore"],
            &[".ignore"],
            &[".gitattributes"],
            &[".git/info/exclude"],
        ] {
            let report = scan(root, Some(request));
            assert!(
                report.full_scan && report.full_scan_reason.is_some(),
                "{request:?}: {report:?}"
            );
            assert!(report.removed.is_empty(), "{request:?}: {report:?}");
            assert_eq!(paths(&report), paths(&full), "{request:?}");
        }
        // 空の文字列と根の外は、従来どおり拒む。
        let service = AppService::sandboxed(root.into()).unwrap();
        for request in ["", "../kept.rs"] {
            assert!(
                service
                    .scan_symbols(root.to_str().unwrap(), Some(&[request.to_owned()]))
                    .is_err(),
                "{request:?}"
            );
        }
        // 対照: 空の要求は何も読まない差分の結果。
        let report = scan(root, Some(&[]));
        assert!(!report.full_scan && report.files.is_empty() && report.removed.is_empty());
    }

    /// ignore の規則を読めないときは、要求したパスを除外と決めず、判定できなかったと申告する (Issue #395)。
    #[test]
    fn incremental_scan_does_not_remove_paths_it_could_not_judge() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        write(root, ".gitignore", "ignored.rs\n{unclosed\n");
        write(root, "ignored.rs", "pub struct Hidden;\n");
        let full = scan(root, None);
        assert!(!full.complete, "{full:?}");
        let report = scan(root, Some(&["ignored.rs"]));
        assert!(!report.complete && report.removed.is_empty(), "{report:?}");
        assert!(
            report
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic == "not judged: ignored.rs"),
            "{report:?}"
        );
    }

    /// 大文字小文字を区別しないファイルシステムでは、綴りの違う要求を実際の綴りで読み、要求の綴りを `removed` に
    /// 入れる (名前の大文字小文字だけを変えた rename の旧名も消える。Issue #395)。区別するファイルシステムでは調べない。
    #[test]
    fn incremental_scan_uses_the_actual_spelling() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        write(root, "src/Keep.rs", "pub struct Keep;\n");
        if !root.join("SRC/keep.rs").exists() {
            return;
        }
        let report = scan(root, Some(&["SRC/keep.rs"]));
        assert_eq!(paths(&report), ["src/Keep.rs"]);
        assert_eq!(report.removed, ["SRC/keep.rs"]);
        let report = scan(root, Some(&["src/keep.rs", "src/Keep.rs"]));
        assert_eq!(paths(&report), ["src/Keep.rs"]);
        assert_eq!(report.removed, ["src/keep.rs"]);
    }

    /// symlink は全体の走査が読まないので、差分の要求でも読まずに `removed` に入れる。symlink のディレクトリの下も
    /// 辿らない (Issue #395)。symlink を作れない環境では調べない。
    #[test]
    fn incremental_scan_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join(".git")).unwrap();
        write(root, "src/keep.rs", "pub struct Keep;\n");
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(root.join("src/keep.rs"), root.join("link.rs"))
            .and_then(|()| std::os::unix::fs::symlink(root.join("src"), root.join("linkdir")));
        #[cfg(windows)]
        let linked =
            std::os::windows::fs::symlink_file(root.join("src/keep.rs"), root.join("link.rs"))
                .and_then(|()| {
                    std::os::windows::fs::symlink_dir(root.join("src"), root.join("linkdir"))
                });
        if linked.is_err() {
            return;
        }
        let full = scan(root, None);
        assert_eq!(paths(&full), ["src/keep.rs"]);
        let report = scan(root, Some(&["link.rs", "linkdir/keep.rs", "linkdir"]));
        assert!(!report.full_scan && report.complete, "{report:?}");
        assert!(report.files.is_empty(), "{report:?}");
        assert_eq!(report.removed, ["link.rs", "linkdir", "linkdir/keep.rs"]);
    }
}
