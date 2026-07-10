use std::collections::HashSet;

use crate::models::impact::{DiffFile, HunkInfo};

/// hunk 本体の 1 行を分類した結果。
pub(crate) enum HunkBodyLine<'a> {
    /// 追加行 (`+`)。先頭の `+` を除いた内容を保持する。
    Added(&'a str),
    /// 削除行 (`-`)。先頭の `-` を除いた内容を保持する。
    Removed(&'a str),
    /// コンテキスト行 (` ` または空行)。
    Context,
    /// `\ No newline at end of file` 等の metadata 行 (行数に数えない)。
    Metadata,
}

/// hunk ヘッダで宣言された old/new の残り行数を追跡し、本体行を消費する。
///
/// 本体を消費し切る (`is_complete`) まではファイル/hunk ヘッダ判定を行わないことで、
/// 削除/追加行のコンテンツが `--- a/...` / `+++ b/...` の形でも誤認しない
/// (hunk 途中で次ファイルヘッダと誤判定し以降の本体が脱落する false negative の防止)。
pub(crate) struct HunkProgress {
    old_remaining: usize,
    new_remaining: usize,
}

impl HunkProgress {
    pub(crate) fn new(hunk: &HunkInfo) -> Self {
        Self {
            old_remaining: hunk.old_count,
            new_remaining: hunk.new_count,
        }
    }

    /// 本体 1 行を消費して old/new の残数を減らし、行種別を返す。
    pub(crate) fn consume<'a>(&mut self, line: &'a str) -> HunkBodyLine<'a> {
        match line.as_bytes().first() {
            Some(b'+') => {
                self.new_remaining = self.new_remaining.saturating_sub(1);
                HunkBodyLine::Added(&line[1..])
            }
            Some(b'-') => {
                self.old_remaining = self.old_remaining.saturating_sub(1);
                HunkBodyLine::Removed(&line[1..])
            }
            // `\ No newline at end of file` は old/new いずれの行数にも数えない。
            Some(b'\\') => HunkBodyLine::Metadata,
            _ => {
                // 先頭スペースの context 行、および末尾の空行を context として扱う。
                self.old_remaining = self.old_remaining.saturating_sub(1);
                self.new_remaining = self.new_remaining.saturating_sub(1);
                HunkBodyLine::Context
            }
        }
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.old_remaining == 0 && self.new_remaining == 0
    }
}

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
    let mut active_hunk: Option<HunkProgress> = None;
    let mut current_new_line: usize = 0;

    for line in input.lines() {
        // hunk 本体を消費中はヘッダに見える行も本体行として扱う。target 外の hunk でも
        // count を追跡し、本体内の `+++ b/<target>` で対象ファイルへ誤って切り替わるのを防ぐ。
        if let Some(progress) = active_hunk.as_mut() {
            let consumed = progress.consume(line);
            if in_target_file {
                match consumed {
                    HunkBodyLine::Added(_) => {
                        if current_new_line > 0 {
                            result.insert(current_new_line - 1);
                        }
                        current_new_line += 1;
                    }
                    HunkBodyLine::Context => {
                        current_new_line += 1;
                    }
                    // 削除行は new 側に存在せず、metadata 行は行数に数えない。
                    HunkBodyLine::Removed(_) | HunkBodyLine::Metadata => {}
                }
            }
            if progress.is_complete() {
                active_hunk = None;
            }
            continue;
        }

        if line.starts_with("--- ") {
            in_target_file = false;
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            in_target_file = path == file_path;
        } else if line.starts_with("+++ ") {
            in_target_file = false;
        } else if line.starts_with("@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            current_new_line = hunk.new_start;
            active_hunk = Some(HunkProgress::new(&hunk));
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
    let mut active_hunk: Option<HunkProgress> = None;

    for line in input.lines() {
        // hunk 本体を消費中は、`--- a/...` / `+++ b/...` に見える行も本体行として扱い、
        // ファイルヘッダ判定へ落とさない (削除/追加行のコンテンツが diff ヘッダと衝突する
        // ケースで以降の hunk 本体が脱落する false negative を防ぐ)。
        if let Some(progress) = active_hunk.as_mut() {
            let consumed = progress.consume(line);
            if current_new_path.as_deref() == Some("/dev/null")
                && let HunkBodyLine::Removed(removed) = consumed
            {
                // 削除ファイルの hunk 内 `-` 行を旧ソース順で蓄積する。
                current_deleted_lines.extend_from_slice(removed.as_bytes());
                current_deleted_lines.push(b'\n');
            }
            if progress.is_complete() {
                active_hunk = None;
            }
            continue;
        }

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
        } else if line.starts_with("--- /dev/null") {
            flush_file(
                &mut files,
                &mut current_old_path,
                &mut current_new_path,
                &mut current_hunks,
                &mut current_deleted_lines,
            );
            current_old_path = Some("/dev/null".to_string());
        } else if line.starts_with("--- ") {
            // 認識できない旧側ヘッダ (quotepath クォート `--- "a/..."` 等)。
            // 直前ファイルの state を引きずると以降の hunk が誤帰属するため、
            // flush して当該ファイルを解析対象から外す (fail-safe)。
            flush_file(
                &mut files,
                &mut current_old_path,
                &mut current_new_path,
                &mut current_hunks,
                &mut current_deleted_lines,
            );
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            current_new_path = Some(path.to_string());
        } else if line.starts_with("+++ /dev/null") {
            current_new_path = Some("/dev/null".to_string());
        } else if line.starts_with("+++ ") {
            // 認識できない新側ヘッダ。片側だけ認識できたペア (`--- a/x` + `+++ "b/..."`) を
            // 別ファイルの hunk と合成しないよう、新側を未確定に戻す。
            current_new_path = None;
        } else if line.starts_with("@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            active_hunk = Some(HunkProgress::new(&hunk));
            current_hunks.push(hunk);
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
pub(crate) fn parse_hunk_header(line: &str) -> Option<HunkInfo> {
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

    // unified diff 仕様: count > 0 のとき start は 1-origin で必ず 1 以上。
    // start = 0 が正当なのは count = 0 (片側完全空) の hunk のみ。
    // count > 0 && start = 0 の不正ヘッダ (例: `@@ -1,0 +0,1 @@`) は reject し、
    // 後段の current_new_line 補正 (`current_new_line > 0` ガード) が黙って
    // 1 行目の `+` 行を取りこぼす false negative を防ぐ。
    if (old_count > 0 && old_start == 0) || (new_count > 0 && new_start == 0) {
        return None;
    }

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
        // 各 hunk の count を本体行数と一致させた簡略 diff (追加 1 行ずつ)。
        let diff = r#"--- a/src/foo.rs
+++ b/src/foo.rs
@@ -1,0 +1,1 @@
+use bar;
--- a/src/bar.rs
+++ b/src/bar.rs
@@ -5,0 +5,1 @@
+fn new_fn() {}
@@ -20,0 +21,1 @@
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

    /// hunk 本体の削除/追加行コンテンツが `--- a/...` / `+++ b/...` の形でも、
    /// ファイルヘッダと誤認せず後続の本体を取りこぼさない (false negative 回帰防止)。
    #[test]
    fn parse_unified_diff_body_line_looks_like_header() {
        // hunk は old=2,new=2。削除行コンテンツが `-- a/x` (diff 行で `--- a/x`)、
        // 追加行コンテンツが `++ b/x` (diff 行で `+++ b/x`)。
        let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,2 +1,2 @@\n--- a/x\n+++ b/x\n";
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1, "本体行をヘッダ誤認して別ファイル化しない");
        assert_eq!(files[0].new_path, "src/lib.rs");
        assert_eq!(files[0].hunks.len(), 1);
    }

    /// unified diff 仕様違反 (`count > 0 && start == 0`) の hunk header は reject する。
    /// `@@ -1,0 +0,1 @@` のような malformed hunk header を黙って受理すると、
    /// `extract_changed_new_lines` の `current_new_line > 0` ガードで最初の `+` 行が
    /// 取りこぼされる false negative を起こす。回帰防止。
    #[test]
    fn parse_hunk_header_rejects_zero_start_with_positive_count() {
        // new_count > 0 で new_start = 0 は仕様違反
        assert!(parse_hunk_header("@@ -1,0 +0,1 @@").is_none());
        // old_count > 0 で old_start = 0 も仕様違反
        assert!(parse_hunk_header("@@ -0,1 +1,0 @@").is_none());
        // pure-delete (new_count=0, new_start=0) は正当
        assert!(parse_hunk_header("@@ -1,3 +0,0 @@").is_some());
        // pure-add (old_count=0, old_start=0) は正当
        assert!(parse_hunk_header("@@ -0,0 +1,3 @@").is_some());
    }

    /// extract_changed_new_lines も hunk 本体の `+++ b/...` 行を追加行として数え、
    /// ファイルヘッダと誤認しない。
    #[test]
    fn extract_changed_new_lines_body_line_looks_like_header() {
        // target=foo.rs。hunk old=1,new=2。本体に `+++ b/x` (追加行コンテンツ `++ b/x`)。
        let diff = "--- a/foo.rs\n+++ b/foo.rs\n@@ -1,1 +1,2 @@\n existing\n+++ b/x\n";
        let changed = extract_changed_new_lines(diff, "foo.rs");
        // new_start=1: line1 ' existing'(context), line2 '+++ b/x'(added → 0-indexed 1)
        let mut sorted: Vec<_> = changed.into_iter().collect();
        sorted.sort();
        assert_eq!(sorted, vec![1]);
    }

    /// quotepath 形式でクォートされた旧側ヘッダ (`--- "a/..."`) を直前ファイルへ合流させない。
    /// 未 flush のまま後続の `+++ /dev/null` を拾うと直前ファイルの new_path が /dev/null に
    /// 化け、削除 hunk まで誤帰属する (回帰防止)。
    #[test]
    fn parse_unified_diff_unrecognized_quoted_old_header_does_not_merge_into_previous_file() {
        // ascii.rs の通常ブロックの後に、非 ASCII 名 (git quotepath) の削除ブロックが続く。
        let diff = r#"--- a/ascii.rs
+++ b/ascii.rs
@@ -1,1 +1,2 @@
 existing
+added
--- "a/\346\227\245\346\234\254\350\252\236.rs"
+++ /dev/null
@@ -1,2 +0,0 @@
-line1
-line2
"#;
        let files = parse_unified_diff(diff);
        assert_eq!(files.len(), 1, "quotepath ブロックはファイル化されない");
        assert_eq!(files[0].old_path, "ascii.rs");
        assert_ne!(
            files[0].new_path, "/dev/null",
            "後続の +++ /dev/null が ascii.rs に合流しない"
        );
        assert_eq!(files[0].new_path, "ascii.rs");
        assert_eq!(files[0].hunks.len(), 1, "hunk は ascii.rs 自身の 1 個だけ");
        // 削除ブロックの hunk (old_count=2) ではなく ascii.rs 自身の hunk であること
        assert_eq!(files[0].hunks[0].old_count, 1);
        assert_eq!(files[0].hunks[0].new_count, 2);
    }

    /// 片側だけ認識できたペア (`--- a/x` + quotepath の `+++ "b/..."`、rename 風) は
    /// files に現れず、後続の正常ブロックは通常どおり解析される。
    #[test]
    fn parse_unified_diff_unrecognized_quoted_new_header_drops_pair() {
        let diff = r#"--- a/ascii.rs
+++ "b/\346\227\245\346\234\254\350\252\236.rs"
@@ -1,1 +1,1 @@
-old line
+new line
--- a/other.rs
+++ b/other.rs
@@ -1,1 +1,2 @@
 keep
+added
"#;
        let files = parse_unified_diff(diff);
        assert_eq!(
            files.len(),
            1,
            "新側ヘッダを認識できないペアは files に現れない"
        );
        assert!(
            files.iter().all(|f| f.old_path != "ascii.rs"),
            "ascii.rs のブロックは別ファイルの hunk と合成されない"
        );
        assert_eq!(files[0].new_path, "other.rs");
        assert_eq!(files[0].hunks.len(), 1);
        assert_eq!(files[0].hunks[0].new_count, 2);
    }
}
