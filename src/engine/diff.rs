use std::collections::HashSet;

use crate::models::impact::{DiffFile, HunkInfo};

/// 単一ファイルの unified diff から、new 側で実際に追加された行 (`+` 行) の
/// 0-indexed 行番号 set を抽出する。
///
/// `find_affected_symbols` は HunkInfo の `new_start..new_start+new_count` 全域
/// (= context 行込み) で symbol range と overlap 判定するため、隣接 hunk の
/// context 3 行に巻き込まれた未変更 symbol が affected に残る (Issue
/// 2026-05-14-private-const-and-unchanged-export-noise)。
/// 本関数で返す set を symbol range と照合することで、`+` 行が 1 つも range に
/// 入らない symbol を context-only overlap として弾ける。
///
/// 注意:
/// - pure-delete hunk (new_count==0) は `+` 行が無いため、本 set には反映されない。
///   呼び出し側はそのケースを別判定 (change_type=removed) で処理すること。
/// - file_path は `+++ b/<path>` の `<path>` と完全一致で照合する。
pub fn extract_changed_new_lines(input: &str, file_path: &str) -> HashSet<usize> {
    let mut result: HashSet<usize> = HashSet::new();
    let mut in_target_file = false;
    let mut in_hunk = false;
    let mut current_new_line: usize = 0;

    for line in input.lines() {
        if line.starts_with("--- ") {
            in_target_file = false;
            in_hunk = false;
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            in_target_file = path == file_path;
            in_hunk = false;
        } else if line.starts_with("+++ ") {
            in_target_file = false;
            in_hunk = false;
        } else if line.starts_with("@@ ") {
            if in_target_file && let Some(hunk) = parse_hunk_header(line) {
                current_new_line = hunk.new_start;
                in_hunk = true;
            } else {
                in_hunk = false;
            }
        } else if in_hunk && in_target_file {
            match line.as_bytes().first() {
                Some(b'+') => {
                    if current_new_line > 0 {
                        result.insert(current_new_line - 1);
                    }
                    current_new_line += 1;
                }
                Some(b'-') => {
                    // 削除行は new 側に存在しないため new_line は進めない
                }
                Some(b' ') => {
                    current_new_line += 1;
                }
                Some(b'\\') => {
                    // `\ No newline at end of file` 等の metadata は無視
                }
                _ => {
                    // 空行は context 扱い
                    current_new_line += 1;
                }
            }
        }
    }

    result
}

/// unified diff 文字列を `DiffFile` の配列に変換する。
///
/// 削除ファイル (`+++ /dev/null`) の hunk 内 `-` 行は旧ソース復元用に蓄積し、
/// `DiffFile.deleted_old_source` にセットする。`extract_exported_symbols_from_git`
/// が base mismatch で失敗した際の API 差分検出フォールバックで使う。
pub fn parse_unified_diff(input: &str) -> Vec<DiffFile> {
    let mut files = Vec::new();
    let mut current_old_path: Option<String> = None;
    let mut current_new_path: Option<String> = None;
    let mut current_hunks: Vec<HunkInfo> = Vec::new();
    let mut current_deleted_lines: Vec<u8> = Vec::new();
    let mut in_hunk = false;

    for line in input.lines() {
        if let Some(path) = line.strip_prefix("--- a/") {
            // 直前のファイル情報を確定
            flush_file(
                &mut files,
                &mut current_old_path,
                &mut current_new_path,
                &mut current_hunks,
                &mut current_deleted_lines,
            );
            current_old_path = Some(path.to_string());
            in_hunk = false;
        } else if line.starts_with("--- /dev/null") {
            flush_file(
                &mut files,
                &mut current_old_path,
                &mut current_new_path,
                &mut current_hunks,
                &mut current_deleted_lines,
            );
            current_old_path = Some("/dev/null".to_string());
            in_hunk = false;
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            current_new_path = Some(path.to_string());
        } else if line.starts_with("+++ /dev/null") {
            current_new_path = Some("/dev/null".to_string());
        } else if line.starts_with("@@ ") {
            if let Some(hunk) = parse_hunk_header(line) {
                current_hunks.push(hunk);
                in_hunk = true;
            } else {
                in_hunk = false;
            }
        } else if in_hunk
            && current_new_path.as_deref() == Some("/dev/null")
            && let Some(removed) = line.strip_prefix('-')
        {
            // 削除ファイルの hunk 内 `-` 行を旧ソース順で蓄積する
            // (`---` ヘッダは `--- a/` / `--- /dev/null` で先に処理済み)。
            current_deleted_lines.extend_from_slice(removed.as_bytes());
            current_deleted_lines.push(b'\n');
        }
    }

    // 最後のファイル情報を確定
    flush_file(
        &mut files,
        &mut current_old_path,
        &mut current_new_path,
        &mut current_hunks,
        &mut current_deleted_lines,
    );

    files
}

fn flush_file(
    files: &mut Vec<DiffFile>,
    old_path: &mut Option<String>,
    new_path: &mut Option<String>,
    hunks: &mut Vec<HunkInfo>,
    deleted_lines: &mut Vec<u8>,
) {
    if let (Some(old), Some(new)) = (old_path.take(), new_path.take())
        && !hunks.is_empty()
    {
        let deleted_old_source = if new == "/dev/null" && !deleted_lines.is_empty() {
            Some(std::mem::take(deleted_lines))
        } else {
            None
        };
        files.push(DiffFile {
            old_path: old,
            new_path: new,
            hunks: std::mem::take(hunks),
            deleted_old_source,
        });
    }
    hunks.clear();
    deleted_lines.clear();
}

/// `"@@ -10,5 +10,8 @@"` や `"@@ -10,5 +10,8 @@ fn foo()"` の hunk ヘッダを解析する。
fn parse_hunk_header(line: &str) -> Option<HunkInfo> {
    // 先頭の `"@@ "` を除去
    let rest = line.strip_prefix("@@ ")?;
    // 終端の `" @@"` の位置を探す
    let end = rest.find(" @@")?;
    let range_part = &rest[..end];

    // old/new の範囲に分割: "-10,5 +10,8"
    let mut parts = range_part.split_whitespace();
    let old_part = parts.next()?.strip_prefix('-')?;
    let new_part = parts.next()?.strip_prefix('+')?;

    let (old_start, old_count) = parse_range_spec(old_part)?;
    let (new_start, new_count) = parse_range_spec(new_part)?;

    Some(HunkInfo {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

/// `"10,5"` または `"10"` を `(start, count)` に変換する。
fn parse_range_spec(spec: &str) -> Option<(usize, usize)> {
    if let Some((start, count)) = spec.split_once(',') {
        Some((start.parse().ok()?, count.parse().ok()?))
    } else {
        Some((spec.parse().ok()?, 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_diff() {
        let diff = r#"diff --git a/src/main.rs b/src/main.rs
index abc1234..def5678 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,5 +10,8 @@ fn main() {
+    new_line();
     existing();
-    removed();
"#;
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].new_path, "src/main.rs");
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[0].hunks[0].old_start, 10);
        assert_eq!(files[0].hunks[0].old_count, 5);
        assert_eq!(files[0].hunks[0].new_start, 10);
        assert_eq!(files[0].hunks[0].new_count, 8);
    }

    #[test]
    fn parse_multi_file_diff() {
        let diff = r#"--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,3 +1,4 @@
+use bar;
--- a/src/bar.rs
+++ b/src/bar.rs
@@ -5,2 +5,3 @@
+fn new_fn() {}
@@ -20,4 +21,6 @@
+// comment
"#;
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].new_path, "src/foo.rs");
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[1].new_path, "src/bar.rs");
        assert_eq!(files[1].hunks.len(), 2);
    }

    #[test]
    fn parse_hunk_no_count() {
        let hunk = parse_hunk_header("@@ -1 +1 @@");
        assert!(hunk.is_some());
        let h = hunk.unwrap();
        assert_eq!(h.old_start, 1);
        assert_eq!(h.old_count, 1);
        assert_eq!(h.new_start, 1);
        assert_eq!(h.new_count, 1);
    }

    #[test]
    fn parse_new_file_diff_with_dev_null() {
        let diff = r#"diff --git a/src/new.rs b/src/new.rs
new file mode 100644
index 0000000..1234567
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,2 @@
+fn new_fn() {}
+"#;
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path, "/dev/null");
        assert_eq!(files[0].new_path, "src/new.rs");
        // 新規ファイルでは旧ソース無し
        assert!(files[0].deleted_old_source.is_none());
    }

    #[test]
    fn parse_deleted_file_captures_old_source() {
        // 削除ファイルの hunk 内 `-` 行を旧ソースとして保持できることを確認する
        let diff = r#"diff --git a/src/old.rs b/src/old.rs
deleted file mode 100644
index 1234567..0000000
--- a/src/old.rs
+++ /dev/null
@@ -1,3 +0,0 @@
-pub fn removed_fn() {
-    println!("gone");
-}
"#;
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].old_path, "src/old.rs");
        assert_eq!(files[0].new_path, "/dev/null");
        let restored = files[0]
            .deleted_old_source
            .as_ref()
            .expect("deleted file should have old source");
        let text = std::str::from_utf8(restored).expect("utf-8");
        assert_eq!(text, "pub fn removed_fn() {\n    println!(\"gone\");\n}\n");
    }

    #[test]
    fn parse_deleted_file_skips_no_newline_marker() {
        // `\ No newline at end of file` などの diff metadata 行は旧ソースに混入させない
        let diff = "diff --git a/foo.txt b/foo.txt\ndeleted file mode 100644\n--- a/foo.txt\n+++ /dev/null\n@@ -1,1 +0,0 @@\n-only line\n\\ No newline at end of file\n";
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1);
        let restored = files[0].deleted_old_source.as_ref().expect("captured");
        assert_eq!(std::str::from_utf8(restored).unwrap(), "only line\n");
    }

    /// `+` 行のみを new_line ベースで集計する。context 行と `-` 行は new_line を
    /// 進めるが set には含めない。
    #[test]
    fn extract_changed_new_lines_records_added_lines_only() {
        let diff = "--- a/foo.rs\n+++ b/foo.rs\n@@ -10,3 +10,4 @@\n existing\n+added_at_line_11\n existing2\n existing3\n";
        let changed = extract_changed_new_lines(diff, "foo.rs");
        // new_start=10, line 10 ' existing' (context), line 11 '+added' (changed),
        // line 12 ' existing2' (context), line 13 ' existing3' (context)
        // 0-indexed: 11 - 1 = 10
        let mut sorted: Vec<_> = changed.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec![10]);
    }

    /// 削除行は new_line を進めず、`+` 直前の add 行だけ記録される。
    #[test]
    fn extract_changed_new_lines_handles_deletion_correctly() {
        let diff =
            "--- a/foo.rs\n+++ b/foo.rs\n@@ -1,3 +1,3 @@\n line_a\n-deleted\n+added\n line_c\n";
        let changed = extract_changed_new_lines(diff, "foo.rs");
        // new_start=1, line 1 ' line_a' (context), '-deleted' (skip new_line),
        // line 2 '+added' (changed → 0-indexed 1), line 3 ' line_c' (context)
        let mut sorted: Vec<_> = changed.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec![1]);
    }

    /// 対象ファイルでない `+` 行は無視する。
    #[test]
    fn extract_changed_new_lines_skips_other_files() {
        let diff = "--- a/foo.rs\n+++ b/foo.rs\n@@ -1 +1,2 @@\n existing\n+foo_added\n--- a/bar.rs\n+++ b/bar.rs\n@@ -1 +1,2 @@\n existing\n+bar_added\n";
        let changed = extract_changed_new_lines(diff, "foo.rs");
        assert_eq!(changed.len(), 1, "foo.rs の add のみ");
        assert!(changed.contains(&1)); // line 2 → 0-indexed 1
        let bar_changed = extract_changed_new_lines(diff, "bar.rs");
        assert_eq!(bar_changed.len(), 1);
        assert!(bar_changed.contains(&1));
    }

    /// pure-add (new file) では全 `+` 行が記録される。
    #[test]
    fn extract_changed_new_lines_pure_add_collects_all_lines() {
        let diff = "--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1,3 @@\n+line1\n+line2\n+line3\n";
        let changed = extract_changed_new_lines(diff, "new.rs");
        let mut sorted: Vec<_> = changed.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec![0, 1, 2]);
    }
}
