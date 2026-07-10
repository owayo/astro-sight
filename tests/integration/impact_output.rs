use super::support::TestRepo;

/// シンボル範囲外 (ファイル先頭コメント) だけの変更は affected が空になる。
/// streaming 経路と同様に no-cross 分岐でも空 FileImpact をスキップし、
/// changes が空配列になることを検証する。
#[test]
fn context_comment_only_change_emits_no_empty_file_impact() {
    let repo = TestRepo::new();
    repo.write(
        "sample.ts",
        "// ヘッダコメント\nexport function realWork(): number {\n    return 1;\n}\n",
    );

    let diff = "--- a/sample.ts\n\
+++ b/sample.ts\n\
@@ -1,1 +1,1 @@\n\
-// ヘッダコメント\n\
+// ヘッダコメント (更新)\n";

    let json = repo.run_json_with_stdin("context", &[], diff.as_bytes());
    assert_eq!(
        json["changes"].as_array().unwrap().len(),
        0,
        "シンボル範囲外のコメントのみの変更は空 FileImpact を出力しないべき: {json}"
    );
}
