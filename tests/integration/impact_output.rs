use super::support::TestRepo;

#[test]
fn cross_file_caller_names_keep_the_first_caller_and_all_symbols_on_a_line() {
    let repo = TestRepo::new();
    let producer = "export function total(x: number) { return x; }\nexport function plus(x: number) { return x + 1; }\n";
    repo.write("producer.ts", producer);
    repo.write("runner.ts", "import { total, plus } from './producer';\nconst left = () => total(1); const right = () => plus(2);\nconst text = 'fn misleading'; total(3);\n");
    repo.init_git();
    repo.commit_all("base");
    repo.write(
        "producer.ts",
        producer.replace("x: number)", "x: number, required: number)"),
    );
    let graph = repo.run_json("context", &["--git"]);
    assert_eq!(
        graph["changes"][0]["impacted_callers"],
        serde_json::json!([
            {"path": "runner.ts", "line": 1, "name": "left", "symbols": ["plus", "total"]},
            {"path": "runner.ts", "line": 2, "name": "total", "symbols": ["total"]}
        ]),
        "{graph}"
    );
}

#[test]
fn cross_file_caller_names_use_the_enclosing_function_in_the_review_snapshot() {
    let repo = TestRepo::new();
    repo.write("producer.py", "def total(x):\n    return x\n");
    let callers = "from producer import total\n\ndef plain():\n    return total(1)\n\nclass Runner:\n    def method(self):\n        def inner():\n            return total(2)\n        return total(3)\n\ntotal(4)\n";
    repo.write("runner.py", callers);
    repo.init_git();
    repo.commit_all("base");
    repo.write("producer.py", "def total(x, required):\n    return x\n");
    repo.stage_all();
    repo.write("runner.py", callers.replace("plain()", "worktree()"));
    for (command, flags, first) in [
        ("context", vec!["--git"], "worktree"),
        ("review", vec!["--git", "--staged"], "plain"),
    ] {
        let result = repo.run_json(command, &flags);
        let graph = result.get("impact").unwrap_or(&result);
        let change = graph["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["path"] == "producer.py")
            .unwrap();
        let impacted = change["impacted_callers"].as_array().unwrap();
        for (line, caller) in [(3, first), (8, "inner"), (9, "method"), (11, "total")] {
            let site = impacted
                .iter()
                .find(|c| c["path"] == "runner.py" && c["line"] == line)
                .unwrap_or_else(|| panic!("{graph}"));
            assert_eq!(site["name"], caller, "{graph}");
            assert_eq!(site["symbols"], serde_json::json!(["total"]), "{graph}");
        }
    }
}

/// GitLab #35: メソッドの型注釈のみ変更 + クラス docblock のコメント修正
/// (`* class SipUserSipUserId` → `* class SipUserId`) の diff で、コメント行の
/// `class SipUserId` テキストがヘッダ変更と誤認され、`new SipUserId(...)` の生成箇所
/// (クラス参照) が impacted_callers に過剰列挙されていた。コメント行はヘッダ照合から
/// 除外し、クラス参照を caller に載せない。
#[test]
fn context_docblock_class_mention_does_not_propagate_class_refs() {
    let repo = TestRepo::new();
    repo.write(
        "SipUserId.php",
        "<?php\n\n/**\n * class SipUserId\n */\nclass SipUserId extends AbstractValueObjectId {\n    public function toEloquent(bool|null $is_eager = null) {\n        return $this->eloquent;\n    }\n}\n",
    );
    repo.write(
        "EntityType.php",
        "<?php\n\nclass EntityType {\n    public function resolve($id) {\n        return SipUserRepository::getOne(new SipUserId($id->getValue()));\n    }\n}\n",
    );

    // docblock のコメント修正 + メソッドの型注釈のみ変更
    let diff = "--- a/SipUserId.php\n\
+++ b/SipUserId.php\n\
@@ -4,7 +4,7 @@\n\
- * class SipUserSipUserId\n\
+ * class SipUserId\n\
  */\n\
 class SipUserId extends AbstractValueObjectId {\n\
-    public function toEloquent(bool $is_eager = null) {\n\
+    public function toEloquent(bool|null $is_eager = null) {\n\
         return $this->eloquent;\n\
     }\n\
 }\n";

    let json = repo.run_json_with_stdin("context", &[], diff.as_bytes());
    let changes = json["changes"].as_array().expect("changes array");
    // まず対象ファイルの変更と toEloquent の signature 変更が実際に検出されていることを
    // 確認する (空結果で negative アサートが素通りするのを防ぐ、codex 指摘)。
    let sip_change = changes
        .iter()
        .find(|c| c["path"].as_str() == Some("SipUserId.php"))
        .expect("SipUserId.php の change が検出されるべき");
    // SipUserId クラス自体が affected_symbols に居ることを確認する (ゲート未到達で
    // negative アサートが素通りするのを防ぐ、codex 指摘)。
    let sip_class_affected = sip_change["affected_symbols"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|s| s["name"].as_str() == Some("SipUserId") && s["kind"].as_str() == Some("class"));
    assert!(
        sip_class_affected,
        "SipUserId クラスが affected_symbols に含まれるべき (ゲート到達確認): {json}"
    );
    let has_to_eloquent_sig = sip_change["signature_changes"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|sc| sc["name"].as_str() == Some("toEloquent"));
    assert!(
        has_to_eloquent_sig,
        "toEloquent の signature 変更が検出されるべき: {json}"
    );
    // そのうえで、クラス参照 (`new SipUserId(...)`) が impacted_callers に載っていないこと。
    for change in changes {
        if let Some(callers) = change["impacted_callers"].as_array() {
            for caller in callers {
                let symbols: Vec<&str> = caller["symbols"]
                    .as_array()
                    .map(|list| list.iter().filter_map(|s| s.as_str()).collect())
                    .unwrap_or_default();
                assert!(
                    !symbols.contains(&"SipUserId"),
                    "docblock コメント内の class 言及でクラス参照を impacted_callers に載せない: {json}"
                );
            }
        }
    }
}

/// 対照: 実際のクラスヘッダ行 (extends 変更) が diff に含まれる場合は、従来どおり
/// クラス参照を impacted_callers として追跡する (コメントスキップの回帰担保)。
#[test]
fn context_real_class_header_change_still_propagates_class_refs() {
    let repo = TestRepo::new();
    repo.write(
        "SipUserId.php",
        "<?php\n\nclass SipUserId extends OtherBase {\n    public function toEloquent(bool|null $is_eager = null) {\n        return $this->eloquent;\n    }\n}\n",
    );
    repo.write(
        "EntityType.php",
        "<?php\n\nclass EntityType {\n    public function resolve($id) {\n        return SipUserRepository::getOne(new SipUserId($id->getValue()));\n    }\n}\n",
    );

    let diff = "--- a/SipUserId.php\n\
+++ b/SipUserId.php\n\
@@ -3,3 +3,3 @@\n\
-class SipUserId extends AbstractValueObjectId {\n\
+class SipUserId extends OtherBase {\n\
     public function toEloquent(bool|null $is_eager = null) {\n";

    let json = repo.run_json_with_stdin("context", &[], diff.as_bytes());
    let has_class_caller = json["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|change| change["impacted_callers"].as_array())
        .flatten()
        .filter_map(|caller| caller["symbols"].as_array())
        .flatten()
        .filter_map(|s| s.as_str())
        .any(|s| s == "SipUserId");
    assert!(
        has_class_caller,
        "実ヘッダ行の変更は従来どおりクラス参照を caller に載せるべき: {json}"
    );
}

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
