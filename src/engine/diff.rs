use crate::models::impact::{DiffFile, HunkInfo};

/// unified diff 文字列を `DiffFile` の配列に変換する。
pub fn parse_unified_diff(input: &str) -> Vec<DiffFile> {
    let mut files = Vec::new();
    let mut current_old_path: Option<String> = None;
    let mut current_new_path: Option<String> = None;
    let mut current_hunks: Vec<HunkInfo> = Vec::new();

    for line in input.lines() {
        if let Some(path) = line.strip_prefix("--- a/") {
            // 直前のファイル情報を確定
            flush_file(
                &mut files,
                &mut current_old_path,
                &mut current_new_path,
                &mut current_hunks,
            );
            current_old_path = Some(path.to_string());
        } else if line.starts_with("--- /dev/null") {
            flush_file(
                &mut files,
                &mut current_old_path,
                &mut current_new_path,
                &mut current_hunks,
            );
            current_old_path = Some("/dev/null".to_string());
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            current_new_path = Some(path.to_string());
        } else if line.starts_with("+++ /dev/null") {
            current_new_path = Some("/dev/null".to_string());
        } else if line.starts_with("@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            current_hunks.push(hunk);
        }
    }

    // 最後のファイル情報を確定
    flush_file(
        &mut files,
        &mut current_old_path,
        &mut current_new_path,
        &mut current_hunks,
    );

    files
}

fn flush_file(
    files: &mut Vec<DiffFile>,
    old_path: &mut Option<String>,
    new_path: &mut Option<String>,
    hunks: &mut Vec<HunkInfo>,
) {
    if let (Some(old), Some(new)) = (old_path.take(), new_path.take())
        && !hunks.is_empty()
    {
        files.push(DiffFile {
            old_path: old,
            new_path: new,
            hunks: std::mem::take(hunks),
        });
    }
    hunks.clear();
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
    }
}
