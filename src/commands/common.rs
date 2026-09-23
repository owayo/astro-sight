use anyhow::Result;
use std::collections::HashSet;
use std::io::Read;

use crate::cache::store::CacheStore;
use crate::error::{AstroError, ErrorCode};

pub const MAX_INPUT_SIZE: usize = 100 * 1024 * 1024;

/// 現在プロセスの RSS を KB 単位で取得 (Linux のみ正確、その他 OS は None)。
/// `astro-sight review` の各フェーズが何 GB 消費しているかを CI の artifacts ログで
/// 観測するため。
pub(crate) fn current_rss_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::fs;
        let status = fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if let Some(kb) = parts.first().and_then(|s| s.parse::<u64>().ok()) {
                    return Some(kb);
                }
            }
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `ASTRO_SIGHT_LOG_PHASES=1` のときのみ stderr に進捗ログを出す。
///
/// CI で `astro-sight review` がどのフェーズで何 GB を確保するかを観測するための
/// 軽量プロファイラ。出力フォーマットは:
/// `[as] phase=<NAME> status=<start|end> rss=<MB> elapsed=<MS>`
pub(crate) fn log_phase(phase: &str, status: &str, elapsed_ms: u128) {
    if std::env::var("ASTRO_SIGHT_LOG_PHASES").ok().as_deref() != Some("1") {
        return;
    }
    let rss_str = current_rss_kb()
        .map(|kb| format!("{}MB", kb / 1024))
        .unwrap_or_else(|| "?MB".to_string());
    eprintln!("[as] phase={phase} status={status} rss={rss_str} elapsed={elapsed_ms}ms");
}

/// `log_phase(start)` → 処理 → `log_phase(end, 経過ms)` の定型を包む (無謬処理用)。
pub(crate) fn timed<T>(phase: &str, f: impl FnOnce() -> T) -> T {
    log_phase(phase, "start", 0);
    let phase_t = std::time::Instant::now();
    let r = f();
    log_phase(phase, "end", phase_t.elapsed().as_millis());
    r
}

/// `timed` の失敗しうる処理版。**エラー時は `end` ログを出さずに伝播する**ため、
/// `start` だけが残ったフェーズが失敗箇所を指す (計測完了 = 成功を意味する)。
pub(crate) fn timed_ok<T>(phase: &str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    log_phase(phase, "start", 0);
    let phase_t = std::time::Instant::now();
    let r = f()?;
    log_phase(phase, "end", phase_t.elapsed().as_millis());
    Ok(r)
}

pub fn classify_error(e: &anyhow::Error) -> (String, String) {
    if let Some(ae) = e.downcast_ref::<AstroError>() {
        (ae.code.to_string(), ae.message.clone())
    } else {
        ("IO_ERROR".to_string(), e.to_string())
    }
}

pub(crate) fn make_error_line(e: &anyhow::Error) -> String {
    let (code, message) = classify_error(e);
    let obj = serde_json::json!({ "error": { "code": code, "message": message } });
    serde_json::to_string(&obj).unwrap()
}

pub(crate) fn read_bytes_limited<R: std::io::Read>(
    reader: R,
    max_bytes: usize,
    source_name: &str,
) -> Result<Vec<u8>> {
    let mut limited = reader.take((max_bytes + 1) as u64);
    let mut buf = Vec::new();
    limited.read_to_end(&mut buf)?;

    if buf.len() > max_bytes {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "{source_name} exceeds maximum size ({} bytes > {} bytes)",
                buf.len(),
                max_bytes
            ),
        )
        .into());
    }

    Ok(buf)
}

pub(crate) fn read_bytes_limited_and_drain<R: std::io::Read>(
    mut reader: R,
    max_bytes: usize,
    source_name: &str,
) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    let mut total_bytes = 0usize;
    let mut chunk = [0u8; 8192];

    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }

        total_bytes = total_bytes.saturating_add(read);
        if buf.len() <= max_bytes {
            let remaining = max_bytes.saturating_add(1).saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..read.min(remaining)]);
        }
    }

    if total_bytes > max_bytes {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "{source_name} exceeds maximum size ({} bytes > {} bytes)",
                total_bytes, max_bytes
            ),
        )
        .into());
    }

    Ok(buf)
}

pub(crate) fn read_to_string_limited<R: std::io::Read>(
    reader: R,
    max_bytes: usize,
    source_name: &str,
) -> Result<String> {
    let buf = read_bytes_limited(reader, max_bytes, source_name)?;
    String::from_utf8(buf).map_err(|e| {
        AstroError::new(
            ErrorCode::InvalidRequest,
            format!("{source_name} is not valid UTF-8: {e}"),
        )
        .into()
    })
}

pub(crate) fn read_file_to_string_limited(path: &str, max_bytes: usize) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > max_bytes as u64 {
        return Err(AstroError::new(
            ErrorCode::InvalidRequest,
            format!(
                "{path} exceeds maximum size ({} bytes > {} bytes)",
                metadata.len(),
                max_bytes
            ),
        )
        .into());
    }
    read_to_string_limited(file, max_bytes, path)
}

pub fn read_paths_file_limited(path: &str, max_bytes: usize) -> Result<Vec<String>> {
    let content = match read_file_to_string_limited(path, max_bytes) {
        Ok(content) => content,
        Err(e) if e.downcast_ref::<AstroError>().is_some() => return Err(e),
        Err(e) => {
            return Err(AstroError::new(
                ErrorCode::IoError,
                format!("failed to read paths file {path}: {e}"),
            )
            .into());
        }
    };

    Ok(content
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

pub(crate) fn cache_hash_for_path(path: &camino::Utf8Path, content_hash: &str) -> String {
    let path_key = std::fs::canonicalize(path.as_std_path())
        .ok()
        .and_then(|p| p.to_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| path.as_str().to_string());

    // 応答には path/lang が含まれるため、内容が同じ別ファイルとはキャッシュを分離する。
    // さらに astro-sight のバージョンを混ぜ、解析ロジックや出力スキーマが変わった
    // 新バイナリが旧バージョンのキャッシュ結果を返さないようにする（アップグレード時に
    // 自動失効）。ファイル内容が不変でも解析結果が変わる場合に stale を防ぐ。
    let version = env!("CARGO_PKG_VERSION");
    CacheStore::hash(format!("{version}\0{path_key}\0{content_hash}").as_bytes())
}

/// diff に含まれる変更ファイル集合。caller のパスが変更ファイルに含まれるか
/// (= 影響が diff 内で解決済みか) を canonicalize ベースで判定する。
///
/// canonicalize が成功したパスは `canonical` 集合、失敗したパスは文字列 fallback として
/// `abs_strs` 集合に持つ。判定時も **canonicalize 成功なら canonical 集合だけ / 失敗なら
/// 文字列集合だけ** を見る (両集合の OR を取ると挙動が変わるため、この分岐を維持する)。
pub(crate) struct ChangedFileSet {
    canonical: std::collections::HashSet<std::path::PathBuf>,
    abs_strs: std::collections::HashSet<String>,
}

impl ChangedFileSet {
    /// 変更ファイルの (相対または絶対) パス列から集合を構築する。相対パスは `dir` 基準で
    /// 絶対化する。事前に `HashSet<&str>` で重複を除いてから canonicalize することで
    /// syscall 回数を抑える (O(M) syscall)。
    pub(crate) fn build<'a, I>(dir: &str, paths: I) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let changed_paths: std::collections::HashSet<&str> = paths.into_iter().collect();
        let canonical: std::collections::HashSet<std::path::PathBuf> = changed_paths
            .iter()
            .filter_map(|cp| {
                let abs = if std::path::Path::new(cp).is_relative() {
                    std::path::Path::new(dir).join(cp)
                } else {
                    std::path::PathBuf::from(cp)
                };
                std::fs::canonicalize(&abs).ok()
            })
            .collect();
        // canonicalize 失敗時のフォールバック用に文字列セットも保持する。
        let abs_strs: std::collections::HashSet<String> = changed_paths
            .iter()
            .map(|cp| {
                if std::path::Path::new(cp).is_relative() {
                    std::path::Path::new(dir)
                        .join(cp)
                        .to_string_lossy()
                        .to_string()
                } else {
                    cp.to_string()
                }
            })
            .collect();
        Self {
            canonical,
            abs_strs,
        }
    }

    /// `caller_path` が変更ファイル集合に含まれるかを判定する。相対パスは `dir` 基準で
    /// 絶対化し、canonicalize 成功時は canonical 集合だけ、失敗時は文字列集合だけを参照する。
    pub(crate) fn contains_caller(&self, dir: &str, caller_path: &str) -> bool {
        let caller_abs = if std::path::Path::new(caller_path).is_relative() {
            std::path::Path::new(dir)
                .join(caller_path)
                .to_string_lossy()
                .to_string()
        } else {
            caller_path.to_string()
        };
        match std::fs::canonicalize(&caller_abs) {
            Ok(canon) => self.canonical.contains(&canon),
            Err(_) => self.abs_strs.contains(&caller_abs),
        }
    }
}

/// 呼び出し側が「diff 内で解決済み」か (`impact --hook` / `review --hook` の共通判定)。
///
/// - 影響分析の結果に現れたファイル (affected シンボルを持つ変更ファイル) の呼び出し側は、
///   ファイル単位で解決済みとみなす (従来の判定)。
/// - それ以外の diff 内ファイルの呼び出し側は、**呼び出し行そのものが変更された (`+` 行)**
///   ときだけ解決済みとみなす。トップレベルの文だけを変更したスクリプト (Python / JS) は
///   affected シンボルを持たないため、従来は呼び出しを更新済みでも diff 外扱いになり
///   "Unresolved impacts found" で誤ってブロックしていた。一方でファイル単位に広げると、
///   定義を `pub use` に置き換えただけのファイルに残る**未変更の**呼び出し (型変更で壊れる)
///   まで黙って解決済みになる (`review_hook_reexport_move_with_type_change_stays_blocking`)。
///   呼び出し行の変更は、API 差分の `modified_closed_in_diff` と同じ強さの証拠。
pub(crate) struct DiffCallerResolution {
    affected_files: ChangedFileSet,
    /// diff 内ファイルの変更行 (`+` 行、0-indexed)。照合規約は `ChangedFileSet` と同じで、
    /// 呼び出し側を canonicalize できれば canonical、できなければ絶対パス文字列で引く。
    changed_lines_canonical: std::collections::HashMap<std::path::PathBuf, HashSet<usize>>,
    changed_lines_abs: std::collections::HashMap<String, HashSet<usize>>,
}

impl DiffCallerResolution {
    pub(crate) fn build<'a, I>(
        dir: &str,
        affected_paths: I,
        diff_input: &str,
        diff_files: &[crate::models::impact::DiffFile],
    ) -> Self
    where
        I: IntoIterator<Item = &'a str>,
    {
        let sections = crate::engine::diff::FileSections::split(diff_input);
        let mut changed_lines_canonical: std::collections::HashMap<
            std::path::PathBuf,
            HashSet<usize>,
        > = std::collections::HashMap::new();
        let mut changed_lines_abs: std::collections::HashMap<String, HashSet<usize>> =
            std::collections::HashMap::new();
        for df in diff_files {
            if df.new_path == "/dev/null" || !crate::engine::impact::is_safe_diff_path(&df.new_path)
            {
                continue;
            }
            let added = crate::engine::diff::extract_changed_line_facts(
                &sections.get(&df.new_path),
                &df.new_path,
            )
            .added_lines;
            if added.is_empty() {
                continue;
            }
            let abs = absolute_path_string(dir, &df.new_path);
            if let Ok(canonical) = std::fs::canonicalize(&abs) {
                changed_lines_canonical
                    .entry(canonical)
                    .or_default()
                    .extend(added.iter().copied());
            }
            changed_lines_abs.entry(abs).or_default().extend(added);
        }
        Self {
            affected_files: ChangedFileSet::build(dir, affected_paths),
            changed_lines_canonical,
            changed_lines_abs,
        }
    }

    /// `caller_path` の `line` (0-indexed) にある呼び出しが diff 内で解決済みか。
    pub(crate) fn is_resolved(&self, dir: &str, caller_path: &str, line: usize) -> bool {
        if self.affected_files.contains_caller(dir, caller_path) {
            return true;
        }
        let abs = absolute_path_string(dir, caller_path);
        let lines = match std::fs::canonicalize(&abs) {
            Ok(canonical) => self.changed_lines_canonical.get(&canonical),
            Err(_) => self.changed_lines_abs.get(&abs),
        };
        lines.is_some_and(|lines| lines.contains(&line))
    }
}

/// 相対パスを `dir` 基準の絶対パス文字列にする (`ChangedFileSet` と同じ規約)。
fn absolute_path_string(dir: &str, path: &str) -> String {
    if std::path::Path::new(path).is_relative() {
        std::path::Path::new(dir)
            .join(path)
            .to_string_lossy()
            .to_string()
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod common_tests {
    use super::*;

    #[test]
    fn timed_returns_inner_value() {
        assert_eq!(timed("unit_test_phase", || 7), 7);
    }

    #[test]
    fn timed_ok_returns_inner_value() {
        let v = timed_ok("unit_test_phase", || Ok(3)).expect("closure succeeds");
        assert_eq!(v, 3);
    }

    #[test]
    fn timed_ok_propagates_error() {
        let err = timed_ok::<()>("unit_test_phase", || Err(anyhow::anyhow!("boom")))
            .expect_err("error should propagate");
        assert!(err.to_string().contains("boom"));
    }

    /// 影響分析の結果に現れない diff 内ファイル (トップレベルの呼び出しだけを更新した
    /// スクリプト等) の呼び出し側は、呼び出し行そのものが変更されたときだけ解決済み。
    /// 旧実装は結果に現れたファイルしか見ず、更新済みの呼び出しで誤ってブロックしていた。
    #[test]
    fn diff_caller_resolution_accepts_updated_call_line_in_diff_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_str = dir.path().to_str().expect("utf-8 path");
        std::fs::write(
            dir.path().join("util.py"),
            "def helper(a, b):\n    return a\n",
        )
        .expect("write util");
        std::fs::write(
            dir.path().join("script.py"),
            "# note\nfrom util import helper\n\nprint(helper(1, 2))\nprint(helper(3))\n",
        )
        .expect("write script");
        std::fs::write(dir.path().join("other.py"), "print(helper(1))\n").expect("write other");
        let diff = concat!(
            "--- a/util.py\n",
            "+++ b/util.py\n",
            "@@ -1,2 +1,2 @@\n",
            "-def helper(a):\n",
            "+def helper(a, b):\n",
            "     return a\n",
            "--- a/script.py\n",
            "+++ b/script.py\n",
            "@@ -1,4 +1,5 @@\n",
            "+# note\n",
            " from util import helper\n",
            " \n",
            "-print(helper(1))\n",
            "+print(helper(1, 2))\n",
            " print(helper(3))\n",
        );
        let diff_files = crate::engine::diff::parse_unified_diff(diff);
        let resolution = DiffCallerResolution::build(dir_str, ["util.py"], diff, &diff_files);

        assert!(
            resolution.is_resolved(dir_str, "script.py", 3),
            "更新済みの呼び出し行 (script.py:4) は解決済み"
        );
        // 対照: 同じ diff 内のファイルでも、変更していない呼び出し行は未解決のまま
        // (ファイル単位に広げると、未変更の呼び出しまで黙って解決済みになる)。
        assert!(
            !resolution.is_resolved(dir_str, "script.py", 4),
            "未変更の呼び出し行 (script.py:5) は未解決"
        );
        // 対照: diff 外のファイルは未解決。影響分析の結果に現れたファイルは従来どおり解決済み。
        assert!(!resolution.is_resolved(dir_str, "other.py", 0));
        assert!(resolution.is_resolved(dir_str, "util.py", 1));
    }
}
