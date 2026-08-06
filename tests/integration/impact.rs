//! impact サブコマンド (変更後の未解決影響検出) の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

#[test]
fn impact_rust_local_var_not_treated_as_cross_file_ref() {
    // Issue 2026-06-13-ai-status-json-symbol-fp: 別ファイルのローカル変数 `let json` が
    // 同名の自由関数 `render::json` への cross-file 参照に誤マッチしないこと。
    // qualified call (`render::json`) を持つ main.rs は high のまま、`let json` だけの
    // profiles.rs は未解決影響に出ないことを検証する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i32) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();
    // profiles.rs: render::json を import / qualified 参照せず bare `let json` のみ
    std::fs::write(
        root.join("src/profiles.rs"),
        "use std::fs;\npub fn discover() -> i32 {\n    let json = fs::read_to_string(\"x\").unwrap_or_default();\n    json.trim().parse().unwrap_or(0)\n}\n",
    )
    .unwrap();
    // main.rs: render::json を qualified path で呼ぶ実 caller
    std::fs::write(
        root.join("src/main.rs"),
        "mod render;\nmod profiles;\nfn main() {\n    println!(\"{}\", render::json(profiles::discover()));\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    // render::json のシグネチャを変更 (i32 -> i64)
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i64) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // main.rs (qualified call) は未解決影響として残る
    assert!(
        stderr.contains("main.rs"),
        "qualified call (render::json) を持つ main.rs は high のまま残るべき: {stderr}"
    );
    // profiles.rs (local `let json`) は cross-file 参照ではないので出ない
    assert!(
        !stderr.contains("profiles.rs"),
        "local 変数 `let json` だけの profiles.rs は未解決影響に出るべきでない: {stderr}"
    );
}

#[test]
fn impact_kotlin_nested_function_is_not_cross_file_origin() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("Local.kt"),
        "fun outer() {\n    fun parseLocal(value: Int): Int = value\n    parseLocal(1)\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("Consumer.kt"),
        "fun unrelated() {\n    parseLocal(\"unrelated\")\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git の起動に失敗");
        assert!(status.success(), "git {args:?} が失敗");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("Local.kt"),
        "fun outer() {\n    fun parseLocal(value: Long): Long = value\n    parseLocal(1)\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("impact の起動に失敗");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "ローカル関数の変更だけなら未解決影響を報告すべきでない: {stderr}"
    );
    assert!(
        !stderr.contains("Consumer.kt"),
        "別ファイルの同名呼び出しをローカル関数の利用箇所にすべきでない: {stderr}"
    );
}

#[test]
fn impact_rust_macro_arg_ident_stays_high() {
    // Issue 2026-06-13 codex 指摘: `call_render!(json, ..)` のように macro が `crate::render::json`
    // を補うケースでは、caller に `::json` 証拠がなくても本物の参照なので high 維持すべき
    // (証拠なし bare identifier の low routing が macro 引数を取りこぼさないこと)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i32) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();
    // macro が path を補い、caller は bare ident `json` を渡すだけ。
    // codex 指摘の混在行 (`let json = call_render!(json, 1); json`) も同行に local binding と
    // macro 引数が混ざるケースとして含め、macro 引数側を取りこぼさないことを検証する。
    std::fs::write(
        root.join("src/caller.rs"),
        "#[macro_export]\nmacro_rules! call_render {\n    ($name:ident, $arg:expr) => { $crate::render::$name($arg) };\n}\npub fn run() -> String { let json = call_render!(json, 1); json }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "#[macro_use]\nmod caller;\nmod render;\nfn main() { let _ = caller::run(); }\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i64) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    // caller.rs の macro 引数 `json` は high のまま未解決影響に残る (fail-closed)。
    assert!(
        stderr.contains("caller.rs"),
        "macro 引数の bare ident `json` は high 維持されるべき (fail-closed): {stderr}"
    );
}

#[test]
fn impact_rust_macro_callee_ident_routes_low() {
    // Issue 2026-06-27-render-json-vs-serde-json-macro:
    // `serde_json::json!` の callee 名は同名の Rust function とは別名前空間なので、
    // `render::json` のシグネチャ変更の未解決 impact にしない。一方で macro 引数は
    // `impact_rust_macro_arg_ident_stays_high` で high 維持を別途担保する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i32) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/caller.rs"),
        "use serde_json::json;\npub fn run() {\n    let _payload = json!({ \"ok\": true });\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "mod caller;\nmod render;\nfn main() { caller::run(); }\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/render.rs"),
        "pub fn json(value: i64) -> String {\n    format!(\"{}\", value)\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "macro callee だけなら unresolved impact は出ないべき: {stderr}"
    );
    assert!(
        !stderr.contains("caller.rs"),
        "serde_json::json! の callee 名は render::json の impact caller ではない: {stderr}"
    );
}

// ---------------------------------------------------------------------------
// impact command tests
// ---------------------------------------------------------------------------

#[test]
fn impact_clean_pass() {
    use std::io::Write;
    use std::process::Stdio;

    // Empty diff → exit 0, no output
    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"")
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success(), "expected exit 0 for empty diff");
    assert!(output.stdout.is_empty(), "expected no stdout");
}

#[test]
fn impact_with_unresolved() {
    use std::io::Write;
    use std::process::Stdio;

    // diff: extract_symbols のシグネチャを変更 → 他ファイルの caller が未解決になる
    // 行番号は実コードから動的に取得する。
    let symbols_src = std::fs::read_to_string("src/engine/symbols/mod.rs")
        .expect("read src/engine/symbols/mod.rs");
    let extract_line_idx = symbols_src
        .lines()
        .position(|l| l.starts_with("pub fn extract_symbols("))
        .expect("extract_symbols 関数が見つからない");
    let line_no = extract_line_idx + 1;
    let diff = format!(
        "--- a/src/engine/symbols/mod.rs\n\
         +++ b/src/engine/symbols/mod.rs\n\
         @@ -{line_no},7 +{line_no},7 @@\n\
         -pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId) -> Result<Vec<Symbol>> {{\n\
         +pub fn extract_symbols(root: Node<'_>, source: &[u8], lang_id: LangId, flag: bool) -> Result<Vec<Symbol>> {{\n\
             let query_src = symbol_query(lang_id);\n"
    );

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", "."])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    assert!(
        !output.status.success(),
        "expected exit 1 for unresolved impacts"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Unresolved impacts found"),
        "expected 'Unresolved impacts found' in stderr, got: {stderr}"
    );
    assert!(
        stderr.contains("extract_symbols"),
        "expected 'extract_symbols' in stderr, got: {stderr}"
    );
}

#[test]
fn impact_git_mode() {
    // --git with HEAD base on a clean repo → exit 0 (no diff = no unresolved)
    let output = cargo_bin()
        .args(["impact", "--dir", ".", "--git"])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "expected exit 0 for clean git diff"
    );
    assert!(output.stdout.is_empty(), "expected no stdout");
}

#[test]
fn impact_excludes_target_test_refs() {
    use std::io::Write;
    use std::process::Stdio;

    // Create fixture: lib.rs with pub fn, consumer.rs with prod + test usage
    let dir = tempfile::tempdir().expect("tempdir");
    let lib_rs = dir.path().join("lib.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    std::fs::write(
        &lib_rs,
        r#"pub fn do_work(x: i32) -> i32 {
    x + 1
}
"#,
    )
    .unwrap();

    std::fs::write(
        &consumer_rs,
        r#"use crate::lib::do_work;

pub fn run() -> i32 {
    do_work(42)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run() {
        assert_eq!(do_work(1), 2);
    }
}
"#,
    )
    .unwrap();

    // Diff that changes do_work signature
    let diff = r#"--- a/lib.rs
+++ b/lib.rs
@@ -1,3 +1,3 @@
-pub fn do_work(x: i32) -> i32 {
+pub fn do_work(x: i32, y: i32) -> i32 {
     x + 1
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Should detect unresolved impact in consumer.rs production code (line 4: do_work(42))
    assert!(
        !output.status.success(),
        "expected exit 1 for unresolved impacts"
    );
    assert!(
        stderr.contains("consumer.rs"),
        "expected consumer.rs in stderr: {stderr}"
    );

    // Verify test-context refs are excluded:
    // Only production-code caller (line 4) should appear, NOT the #[cfg(test)] ref (line 14)
    let lines: Vec<&str> = stderr.lines().collect();
    let consumer_lines: Vec<&str> = lines
        .iter()
        .filter(|l| l.contains("consumer.rs:"))
        .copied()
        .collect();

    assert!(
        !consumer_lines.is_empty(),
        "expected at least one consumer.rs caller"
    );

    for line in &consumer_lines {
        // Extract line number after "consumer.rs:"
        if let Some(pos) = line.find("consumer.rs:") {
            let after = &line[pos + "consumer.rs:".len()..];
            let line_num: usize = after
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            assert!(
                line_num < 8,
                "test-context ref at line {line_num} should be excluded: {line}"
            );
        }
    }
}

#[test]
fn impact_additive_impl_block_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Fixture: types.rs with a struct, consumer.rs that uses the struct
    let dir = tempfile::tempdir().expect("tempdir");
    let types_rs = dir.path().join("types.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // types.rs: struct with an existing impl and a NEW impl block being added
    std::fs::write(
        &types_rs,
        r#"pub struct HookInput {
    pub name: String,
}

impl HookInput {
    pub fn new(name: String) -> Self {
        Self { name }
    }
}

impl HookInput {
    pub fn bash_command(&self) -> &str {
        &self.name
    }

    pub fn file_path(&self) -> &str {
        &self.name
    }
}
"#,
    )
    .unwrap();

    // consumer.rs: uses HookInput (struct construction + method call)
    std::fs::write(
        &consumer_rs,
        r#"use crate::types::HookInput;

pub fn run() -> String {
    let input = HookInput::new("test".to_string());
    input.name.clone()
}
"#,
    )
    .unwrap();

    // Diff: adding a new impl block with new methods (backward-compatible)
    let diff = r#"--- a/types.rs
+++ b/types.rs
@@ -9,3 +9,13 @@
     }
 }
+
+impl HookInput {
+    pub fn bash_command(&self) -> &str {
+        &self.name
+    }
+
+    pub fn file_path(&self) -> &str {
+        &self.name
+    }
+}
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Adding new methods to a type is backward-compatible.
    // consumer.rs should NOT be reported as impacted.
    assert!(
        output.status.success(),
        "expected exit 0 (no unresolved impacts) for additive impl block.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("consumer.rs"),
        "consumer.rs should not appear as impacted for additive impl block: {stderr}"
    );
}

/// エクスポートシンボルの body (内部実装) のみが変わったとき、
/// import/re-export 行しか参照のないファイルは impact に載せない。
/// (レポート 2026-04-08-commitstore-internal-change-false-positive.md の再現)
#[test]
fn impact_body_only_change_import_only_callers_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let commit_store = dir.path().join("commitStore.ts");
    let index_ts = dir.path().join("index.ts");
    let app_tsx = dir.path().join("App.tsx");

    std::fs::write(
        &commit_store,
        r#"export function useCommitStore() {
  return { validate: async () => true };
}
"#,
    )
    .unwrap();
    std::fs::write(
        &index_ts,
        r#"export { useCommitStore } from "./commitStore";
"#,
    )
    .unwrap();
    std::fs::write(
        &app_tsx,
        r#"import { useCommitStore } from "./commitStore";
function App() { return null; }
"#,
    )
    .unwrap();

    // useCommitStore の body のみを変更 (宣言行は不変)
    let diff = r#"--- a/commitStore.ts
+++ b/commitStore.ts
@@ -1,3 +1,7 @@
 export function useCommitStore() {
-  return { validate: async () => true };
+  let currentRequestId = 0;
+  return {
+    validate: async () => { currentRequestId += 1; return currentRequestId > 0; },
+  };
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // import / re-export しかしていないファイルは impact に載せない
    assert!(
        output.status.success(),
        "expected exit 0 (no unresolved impacts) for body-only change.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("index.ts"),
        "re-export-only file should not appear in impact: {stderr}"
    );
    assert!(
        !stderr.contains("App.tsx"),
        "import-only file should not appear in impact: {stderr}"
    );
}

/// 変更ファイルに新規追加シンボルが複数あっても、影響先ファイルの行が実際に
/// 参照しているシンボルだけが impact に紐付き、他の無関係な変更シンボルが
/// 巻き添えで紐付かない（バルク紐付け禁止）。private シンボルも同様に外部ファイルへ
/// 影響伝播しない。
/// (レポート 2026-03-19-dnspacket-bulk-symbol-binding.md の再現)
#[test]
fn impact_bulk_symbol_binding_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let src_kt = dir.path().join("DnsPacket.kt");
    let test_kt = dir.path().join("DnsPacketTest.kt");

    std::fs::write(
        &src_kt,
        r#"package pkg

class DnsPacket(val data: ByteArray) {
    companion object {
        fun createZeroResponse(packet: DnsPacket): ByteArray = byteArrayOf()
    }
}
"#,
    )
    .unwrap();
    std::fs::write(
        &test_kt,
        r#"package pkg

class DnsPacketTest {
    fun zero_response() {
        val packet = DnsPacket(byteArrayOf())
        DnsPacket.createZeroResponse(packet)
    }
}
"#,
    )
    .unwrap();

    // createServFailResponse と private createIpResponse を新規追加
    let diff = r#"--- a/DnsPacket.kt
+++ b/DnsPacket.kt
@@ -4,5 +4,7 @@
 class DnsPacket(val data: ByteArray) {
     companion object {
         fun createZeroResponse(packet: DnsPacket): ByteArray = byteArrayOf()
+        fun createServFailResponse(packet: DnsPacket): ByteArray = byteArrayOf(1)
+        private fun createIpResponse(packet: DnsPacket, ip: String): ByteArray = byteArrayOf()
     }
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("failed to wait");
    let stderr = String::from_utf8_lossy(&output.stderr);

    // DnsPacketTest.kt は createZeroResponse しか参照していない & 新規追加は未使用
    // → 無関係な変更シンボル (createServFailResponse / createIpResponse) が巻き添えで
    //   紐付かないこと、および impact として unresolved 扱いにならないことを確認する
    assert!(
        !stderr.contains("createServFailResponse"),
        "未使用の新規関数は他ファイルの impact に紐付けてはならない: {stderr}"
    );
    assert!(
        !stderr.contains("createIpResponse"),
        "private 関数は他ファイルの impact に紐付けてはならない: {stderr}"
    );
}

/// Rust の `pub use submodule::Foo;` で再エクスポートしているだけのファイルは、
/// エクスポート元シンボルの body-only 変更で impact に載せてはならない。
/// (R7 の TypeScript `export from` と同等、Rust 版の回帰ガード)
#[test]
fn impact_rust_pub_use_reexport_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let inner_rs = dir.path().join("inner.rs");
    let lib_rs = dir.path().join("lib.rs");

    std::fs::write(
        &inner_rs,
        r#"pub fn do_work(x: i32) -> i32 {
    x + 1
}
"#,
    )
    .unwrap();
    std::fs::write(
        &lib_rs,
        r#"pub mod inner;
pub use inner::do_work;
"#,
    )
    .unwrap();

    // do_work の body のみ変更 (シグネチャは不変)
    let diff = r#"--- a/inner.rs
+++ b/inner.rs
@@ -1,3 +1,4 @@
 pub fn do_work(x: i32) -> i32 {
-    x + 1
+    let y = x;
+    y + 1
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected exit 0.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("lib.rs"),
        "`pub use` 再エクスポートのみの行は impact に載せてはならない: {stderr}"
    );
}

/// 新規クレートで `pub mod foo; pub mod bar;` のようなモジュール宣言のみを
/// 追加した場合、他クレート内で同名のローカル変数 (`tensor` / `ops` 等) が
/// impact に巻き添えで紐付かないことを確認する。モジュール名は
/// `should_include_for_cross_file` の段階で除外される。
/// (レポート triage-ocrus-nn-impact.md の再現)
#[test]
fn impact_module_declaration_no_cross_file_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let nn_dir = dir.path().join("crates/ocrus-nn/src");
    let cli_dir = dir.path().join("crates/ocrus-cli/src");
    std::fs::create_dir_all(&nn_dir).unwrap();
    std::fs::create_dir_all(&cli_dir).unwrap();

    std::fs::write(nn_dir.join("lib.rs"), "// empty\n").unwrap();
    // consumer 側に tensor / ops という名前のローカル変数 / クロージャ引数を持つコード
    std::fs::write(
        cli_dir.join("main.rs"),
        r#"fn char_accuracy() {
    let tensor = normalize_line();
    let _shape = tensor.shape();
    for (_i, tensor) in [1, 2].iter().enumerate() {
        let _ = tensor;
    }
    let ops: Vec<u8> = vec![];
    let _ = ops;
}
fn normalize_line() -> Vec<u8> { vec![] }
"#,
    )
    .unwrap();
    // 新規モジュール宣言を追加する diff
    let diff = r#"--- a/crates/ocrus-nn/src/lib.rs
+++ b/crates/ocrus-nn/src/lib.rs
@@ -1 +1,3 @@
-// empty
+pub mod arena;
+pub mod ops;
+pub mod tensor;
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected exit 0 (module 宣言追加は impact を出さない).\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stderr.contains("main.rs"),
        "consumer 側のローカル変数 tensor/ops は impact に載せてはならない: {stderr}"
    );
}

#[test]
fn impact_trait_unchanged_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Pattern 1: trait definition is unchanged, but free functions using the trait
    // have signature changes. The trait name appears in changed lines but the trait
    // definition header (`trait GuestMemory`) is NOT changed.
    let dir = tempfile::tempdir().expect("tempdir");
    let mem_rs = dir.path().join("mem.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // mem.rs: trait + free function using the trait
    std::fs::write(
        &mem_rs,
        r#"pub trait GuestMemory {
    fn read(&self, addr: u64, buf: &mut [u8]);
    fn write(&self, addr: u64, data: &[u8]);
}

pub fn read_obj<M: GuestMemory + ?Sized>(mem: &M, addr: u64) -> u32 {
    let mut buf = [0u8; 4];
    mem.read(addr, &mut buf);
    u32::from_le_bytes(buf)
}
"#,
    )
    .unwrap();

    // consumer.rs: imports and uses GuestMemory
    std::fs::write(
        &consumer_rs,
        r#"use crate::mem::GuestMemory;

pub fn process(mem: &dyn GuestMemory) {
    let val = crate::mem::read_obj(mem, 0x1000);
    println!("{val}");
}
"#,
    )
    .unwrap();

    // Diff: only read_obj signature changes (dyn → impl + ?Sized), trait is unchanged
    let diff = r#"--- a/mem.rs
+++ b/mem.rs
@@ -5,7 +5,7 @@
 }

-pub fn read_obj(mem: &dyn GuestMemory, addr: u64) -> u32 {
+pub fn read_obj<M: GuestMemory + ?Sized>(mem: &M, addr: u64) -> u32 {
     let mut buf = [0u8; 4];
     mem.read(addr, &mut buf);
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // GuestMemory trait is NOT changed, so `use crate::mem::GuestMemory` imports
    // in consumer.rs should NOT be reported as impacted.
    // read_obj signature DID change, so read_obj callers may be reported.
    let ctx: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_default();
    if let Some(changes) = ctx["changes"].as_array() {
        for change in changes {
            // GuestMemory should NOT be in affected_symbols
            let empty = vec![];
            let affected = change["affected_symbols"].as_array().unwrap_or(&empty);
            let has_guest_memory = affected
                .iter()
                .any(|s| s["name"].as_str() == Some("GuestMemory"));
            assert!(
                !has_guest_memory,
                "GuestMemory trait should not be affected when its definition is unchanged.\nstdout: {stdout}\nstderr: {stderr}"
            );
        }
    }
}

#[test]
fn impact_test_symbols_excluded_from_affected() {
    use std::io::Write;
    use std::process::Stdio;

    // Pattern 2: test symbols (#[cfg(test)] mod tests) should not appear
    // in affected_symbols list.
    let dir = tempfile::tempdir().expect("tempdir");
    let lib_rs = dir.path().join("lib.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // lib.rs: pub fn + test module
    std::fs::write(
        &lib_rs,
        r#"pub fn compute(x: i32, y: i32) -> i32 {
    x * y + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> i32 {
        42
    }

    #[test]
    fn test_compute() {
        assert_eq!(compute(setup(), 2), 85);
    }
}
"#,
    )
    .unwrap();

    std::fs::write(
        &consumer_rs,
        r#"use crate::lib::compute;

pub fn run() -> i32 {
    compute(1, 2)
}
"#,
    )
    .unwrap();

    // Diff: changes both compute signature and test helper
    let diff = r#"--- a/lib.rs
+++ b/lib.rs
@@ -1,3 +1,3 @@
-pub fn compute(x: i32) -> i32 {
-    x + 1
+pub fn compute(x: i32, y: i32) -> i32 {
+    x * y + 1
 }
@@ -8,8 +8,8 @@

-    fn setup() -> i32 {
-        0
+    fn setup() -> i32 {
+        42
     }

     #[test]
-    fn test_compute() {
-        assert_eq!(compute(setup()), 1);
+    fn test_compute() {
+        assert_eq!(compute(setup(), 2), 85);
     }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let ctx: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_default();
    if let Some(changes) = ctx["changes"].as_array() {
        for change in changes {
            let empty = vec![];
            let affected = change["affected_symbols"].as_array().unwrap_or(&empty);
            let test_symbols: Vec<&str> = affected
                .iter()
                .filter_map(|s| s["name"].as_str())
                .filter(|name| *name == "tests" || *name == "setup" || *name == "test_compute")
                .collect();
            assert!(
                test_symbols.is_empty(),
                "Test symbols should not appear in affected_symbols: {:?}\nstdout: {stdout}",
                test_symbols
            );
        }
    }
}

#[test]
fn impact_same_name_method_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Pattern: same-name method across different types.
    // `Transport::write` changes should NOT be reported as impacting
    // `Device::write` references in another file.
    let dir = tempfile::tempdir().expect("tempdir");
    let transport_rs = dir.path().join("transport.rs");
    let device_rs = dir.path().join("device.rs");

    // transport.rs: impl Transport with write method
    std::fs::write(
        &transport_rs,
        r#"pub struct Transport {
    pub base: u64,
}

impl Transport {
    pub fn write(&mut self, offset: u64, value: u32) {
        // write to MMIO register
        let addr = self.base + offset;
        unsafe { core::ptr::write_volatile(addr as *mut u32, value) };
    }
}
"#,
    )
    .unwrap();

    // device.rs: different trait with same-name method, plus usage of both
    std::fs::write(
        &device_rs,
        r#"pub trait Device {
    fn write(&mut self, offset: u64, size: u8, value: u64);
}

pub struct Keyboard;

impl Device for Keyboard {
    fn write(&mut self, offset: u64, size: u8, value: u64) {
        // handle keyboard write
    }
}

pub fn dispatch(dev: &mut dyn Device, offset: u64, value: u64) {
    dev.write(offset, 1, value);
}
"#,
    )
    .unwrap();

    // Diff: Transport::write signature changes
    let diff = r#"--- a/transport.rs
+++ b/transport.rs
@@ -5,7 +5,7 @@

 impl Transport {
-    pub fn write(&mut self, offset: u64, value: u32) {
+    pub fn write(&mut self, offset: u64, value: u32, mem: &[u8]) {
         // write to MMIO register
         let addr = self.base + offset;
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // device.rs has its own `write` definition (in trait Device).
    // Transport::write change should NOT impact Device::write references.
    assert!(
        !stdout.contains("device.rs"),
        "device.rs should not appear as impacted for Transport::write change.\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn impact_module_decl_no_false_positive() {
    use std::io::Write;
    use std::process::Stdio;

    // Fixture: lib.rs with module declarations, consumer.rs with same-name local variables
    let dir = tempfile::tempdir().expect("tempdir");
    let lib_rs = dir.path().join("lib.rs");
    let consumer_rs = dir.path().join("consumer.rs");

    // lib.rs: new crate with pub mod declarations
    std::fs::write(
        &lib_rs,
        r#"pub mod arena;
pub mod ops;
pub mod tensor;
"#,
    )
    .unwrap();

    // consumer.rs: uses "tensor" as a local variable name (unrelated crate)
    std::fs::write(
        &consumer_rs,
        r#"pub fn process() {
    let tensor = vec![1.0, 2.0, 3.0];
    let shape = tensor.len();
    println!("{shape}");
}
"#,
    )
    .unwrap();

    // Diff: adding new module declarations
    let diff = r#"--- /dev/null
+++ b/lib.rs
@@ -0,0 +1,3 @@
+pub mod arena;
+pub mod ops;
+pub mod tensor;
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["impact", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn impact");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Module declarations should NOT cause false positives on same-name local variables.
    assert!(
        output.status.success(),
        "expected exit 0 for module declarations.\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(
        !stdout.contains("consumer.rs"),
        "consumer.rs should not appear as impacted for module declarations: {stdout}"
    );
}

/// Phase 4 設計バグの回帰テスト:
/// 同名 method (modified + added) が異なるファイルに存在するとき、
/// added 側 (新規追加ファイル) の fc_ix へ cross-file 参照が漏れないこと。
///
/// 旧実装: pass2 の `sym_to_fc` を **グローバル** `included_symbols.contains(sym_key)`
/// で判定していたため、Factory.php の `new` (modified) が include されるだけで、
/// Id.php (added) の `new` も同じ sym_key を持つため両 fc_ix に caller が流れていた。
///
/// 新実装: `FileContext.cross_file_symbol_keys` で per-file 判定するため、
/// added の Id 側には何も流れない。
#[test]
fn impact_per_file_routing_excludes_added_with_same_method_name() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let existing_rs = dir.path().join("existing.rs");
    let added_rs = dir.path().join("added.rs");
    let caller_rs = dir.path().join("caller.rs");

    // existing.rs: 既存ファイル (modified) — Existing::run シグネチャ変更後の状態
    std::fs::write(
        &existing_rs,
        r#"pub struct Existing;

impl Existing {
    pub fn run(&self, x: u32) -> u32 { x + 1 }
}
"#,
    )
    .unwrap();

    // added.rs: 新規ファイル (added) — Added::run も同名メソッドを持つ
    std::fs::write(
        &added_rs,
        r#"pub struct Added;

impl Added {
    pub fn run(&self) -> u32 { 0 }
}
"#,
    )
    .unwrap();

    // caller.rs: Existing::run の caller がいる (Added は使わない)
    std::fs::write(
        &caller_rs,
        r#"use crate::existing::Existing;

pub fn use_existing(e: &Existing) -> u32 {
    e.run(42)
}
"#,
    )
    .unwrap();

    // diff: existing.rs を modify + added.rs を新規追加
    let diff = r#"--- a/existing.rs
+++ b/existing.rs
@@ -2,4 +2,4 @@
 pub struct Existing;

 impl Existing {
-    pub fn run(&self) -> u32 { 1 }
+    pub fn run(&self, x: u32) -> u32 { x + 1 }
 }
--- /dev/null
+++ b/added.rs
@@ -0,0 +1,5 @@
+pub struct Added;
+
+impl Added {
+    pub fn run(&self) -> u32 { 0 }
+}
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "context failed.\nstdout: {stdout}\nstderr: {stderr}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("changes array missing: {stdout}"));

    // added.rs の change を取り出して impacted_callers が空であることを確認。
    let added_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("added.rs"))
        .unwrap_or_else(|| {
            panic!("added.rs change entry not found in: {stdout}");
        });
    let added_callers = added_change
        .get("impacted_callers")
        .and_then(|c| c.as_array());
    assert!(
        added_callers.is_none_or(|c| c.is_empty()),
        "added.rs (added file) must have no impacted_callers, but got: {added_change}"
    );
    let added_low = added_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array());
    assert!(
        added_low.is_none_or(|c| c.is_empty()),
        "added.rs (added file) must have no low_confidence_callers, but got: {added_change}"
    );
}

/// PHP trait の親型認識テスト (load-bearing バグの回帰テスト):
/// `trait Factory { public static function new() }` のような trait scope の
/// メソッドが変更された際、Stage 4b の parent_in_this_file チェックが
/// 効くようにするため、`trait_declaration` が親型として認識されること。
///
/// 旧実装は `class_declaration` だけを親型として認識していたため、PHP trait
/// 内の同名メソッド (`new` 等) で `parent_ix_by_sym = None` となり、Stage 4b が
/// 完全にバイパスされ、`Other::new()` 系の同名 method 全件が誤って
/// impacted_callers に流れていた。
#[test]
fn impact_php_trait_method_filters_unrelated_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let trait_php = dir.path().join("Factory.php");
    let other_php = dir.path().join("Other.php");
    let unrelated_php = dir.path().join("Unrelated.php");

    // Factory.php: trait scope の static method `new`
    std::fs::write(
        &trait_php,
        r#"<?php
namespace App\Factories;

trait Factory {
    public static function new(int $x): self {
        return new self($x + 1);
    }
}
"#,
    )
    .unwrap();

    // Other.php: 別 class の同名 static method `new`
    std::fs::write(
        &other_php,
        r#"<?php
namespace App\Other;

class Other {
    public static function new(): self {
        return new self();
    }
}
"#,
    )
    .unwrap();

    // Unrelated.php: Other::new() を呼ぶが Factory trait は use していない
    std::fs::write(
        &unrelated_php,
        r#"<?php
namespace App\Consumers;

use App\Other\Other;

class Consumer {
    public function consume(): void {
        $obj = Other::new();
    }
}
"#,
    )
    .unwrap();

    // diff: Factory trait の new シグネチャを変更
    let diff = r#"--- a/Factory.php
+++ b/Factory.php
@@ -3,7 +3,7 @@
 namespace App\Factories;

 trait Factory {
-    public static function new(int $x): self {
+    public static function new(int $x, int $y): self {
         return new self($x + 1);
     }
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");

    let factory_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("Factory.php"))
        .unwrap_or_else(|| panic!("Factory.php change not found: {stdout}"));

    let impacted = factory_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = factory_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    // Unrelated.php は Other::new() を呼ぶだけで Factory trait は触らない。
    // trait_declaration が親型認識されれば parent_in_this_file=false で skip される。
    let unrelated_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("Unrelated.php"));
    let unrelated_in_low = low
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("Unrelated.php"));
    assert!(
        !unrelated_in_impacted,
        "Unrelated.php must NOT appear in impacted_callers (Stage 4b parent check). impacted: {impacted:?}"
    );
    assert!(
        !unrelated_in_low,
        "Unrelated.php must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// Rust trait_item 親型認識の回帰テスト。
///
/// PHP の trait_declaration と同様、Rust の `trait Foo { fn bar() {} }` も
/// 親型認識されないと Stage 4b parent_in_this_file が常に false になり、
/// 別 struct の同名 method が impacted_callers に流れる。
#[test]
fn impact_rust_trait_item_filters_unrelated_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let trait_rs = dir.path().join("factory.rs");
    let other_rs = dir.path().join("other.rs");
    let unrelated_rs = dir.path().join("unrelated.rs");

    // factory.rs: trait scope の method `new`
    std::fs::write(
        &trait_rs,
        r#"pub trait Factory {
    fn new(x: i32) -> Self
    where
        Self: Sized;
}
"#,
    )
    .unwrap();

    // other.rs: 別 struct の同名 method `new`
    std::fs::write(
        &other_rs,
        r#"pub struct Other;

impl Other {
    pub fn new() -> Self {
        Other
    }
}
"#,
    )
    .unwrap();

    // unrelated.rs: Other::new() を呼ぶだけ。Factory trait は触らない。
    std::fs::write(
        &unrelated_rs,
        r#"use crate::other::Other;

pub fn consume() {
    let _ = Other::new();
}
"#,
    )
    .unwrap();

    // diff: Factory trait の `new` シグネチャを変更
    let diff = r#"--- a/factory.rs
+++ b/factory.rs
@@ -1,5 +1,5 @@
 pub trait Factory {
-    fn new(x: i32) -> Self
+    fn new(x: i32, y: i32) -> Self
     where
         Self: Sized;
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");

    let factory_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("factory.rs"))
        .unwrap_or_else(|| panic!("factory.rs change not found: {stdout}"));

    let impacted = factory_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = factory_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let unrelated_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.rs"));
    let unrelated_in_low = low
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.rs"));
    assert!(
        !unrelated_in_impacted,
        "unrelated.rs must NOT appear in impacted_callers (Rust trait_item parent check). impacted: {impacted:?}"
    );
    assert!(
        !unrelated_in_low,
        "unrelated.rs must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// TypeScript abstract_class_declaration 親型認識の回帰テスト。
///
/// `abstract class Foo { abstract bar(): void }` は通常の class_declaration ではなく
/// abstract_class_declaration ノードになるため、別途認識を追加しないと parent が消える。
#[test]
fn impact_typescript_abstract_class_filters_unrelated_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let abs_ts = dir.path().join("base.ts");
    let other_ts = dir.path().join("other.ts");
    let unrelated_ts = dir.path().join("unrelated.ts");

    // base.ts: abstract class scope の method `process`。
    // 本体のない abstract シグネチャは affected symbol にならないため、
    // 具象メソッドを abstract class 内に置いて parent 認識 (abstract_class_declaration)
    // を実際に効かせる (空 FileImpact はスキップされるようになったため、
    // affected が空だとフィルタ検証自体が空振りする)。
    std::fs::write(
        &abs_ts,
        r#"export abstract class Base {
    process(x: number): number {
        return x;
    }
}
"#,
    )
    .unwrap();

    // other.ts: 別 class の同名 method `process`
    std::fs::write(
        &other_ts,
        r#"export class Other {
    process(): number {
        return 0;
    }
}
"#,
    )
    .unwrap();

    // unrelated.ts: Other.process() を呼ぶだけ
    std::fs::write(
        &unrelated_ts,
        r#"import { Other } from "./other";

export function consume(): number {
    const o = new Other();
    return o.process();
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/base.ts
+++ b/base.ts
@@ -1,5 +1,5 @@
 export abstract class Base {
-    process(x: number): number {
+    process(x: number, y: number): number {
         return x;
     }
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");

    let base_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("base.ts"))
        .unwrap_or_else(|| panic!("base.ts change not found: {stdout}"));

    let impacted = base_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = base_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let unrelated_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.ts"));
    let unrelated_in_low = low
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("unrelated.ts"));
    assert!(
        !unrelated_in_impacted,
        "unrelated.ts must NOT appear in impacted_callers (TS abstract_class_declaration parent check). impacted: {impacted:?}"
    );
    assert!(
        !unrelated_in_low,
        "unrelated.ts must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// impact のデフォルト除外ディレクトリ動作確認。
///
/// vendor / node_modules / target などの 3rd-party / build artifact ディレクトリ内に
/// 同名メソッドが置かれていても、`impacted_callers` には流れ込まないこと。
#[test]
fn impact_default_excluded_dirs_drops_vendor_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let owner_php = dir.path().join("Owner.php");
    let vendor_php = dir.path().join("vendor").join("Caller.php");
    let target_rs_dir = dir.path().join("target").join("debug");
    std::fs::create_dir_all(target_rs_dir.parent().unwrap()).unwrap();
    std::fs::create_dir_all(vendor_php.parent().unwrap()).unwrap();

    // Owner.php: trait scope の static method `new`
    std::fs::write(
        &owner_php,
        r#"<?php
trait Factory {
    public static function new(int $x): self {
        return new self($x);
    }
}
"#,
    )
    .unwrap();

    // vendor/Caller.php: 同名 static method `new` を持つ別 class
    std::fs::write(
        &vendor_php,
        r#"<?php
class VendorThing {
    public static function new(): self {
        return new self();
    }
}
function consume_vendor(): void {
    $obj = VendorThing::new();
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/Owner.php
+++ b/Owner.php
@@ -1,5 +1,5 @@
 <?php
 trait Factory {
-    public static function new(int $x): self {
+    public static function new(int $x, int $y): self {
         return new self($x);
     }
 }
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let owner_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("Owner.php"))
        .unwrap_or_else(|| panic!("Owner.php change not found: {stdout}"));

    let impacted = owner_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = owner_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let vendor_in_impacted = impacted.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("vendor"))
            .unwrap_or(false)
    });
    let vendor_in_low = low.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("vendor"))
            .unwrap_or(false)
    });

    assert!(
        !vendor_in_impacted,
        "vendor/ caller must NOT appear in impacted_callers. impacted: {impacted:?}"
    );
    assert!(
        !vendor_in_low,
        "vendor/ caller must NOT appear in low_confidence_callers. low: {low:?}"
    );
}

/// `--exclude-dir` が impact 解析の cross-file 検索でも作用する回帰テスト (v26.5.117)。
///
/// `IMPACT_DEFAULT_EXCLUDED_DIRS` の固定リストに含まれない命名 (バージョン入りの
/// `pjproject-2.15` 等) でも、ユーザーが `--exclude-dir` で渡せば impact から除外される。
#[test]
fn impact_user_exclude_dir_drops_callers() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    let owner_php = dir.path().join("Owner.php");
    let custom_dir = dir.path().join("pjproject-2.15");
    std::fs::create_dir_all(&custom_dir).unwrap();
    let custom_caller = custom_dir.join("Caller.php");

    std::fs::write(
        &owner_php,
        r#"<?php
trait Factory {
    public static function new(int $x): self {
        return new self($x);
    }
}
"#,
    )
    .unwrap();
    // 命名が IMPACT_DEFAULT_EXCLUDED_DIRS に含まれないので、デフォルトでは impact 対象。
    std::fs::write(
        &custom_caller,
        r#"<?php
class CustomThing {
    public static function new(): self {
        return new self();
    }
}
function consume_custom(): void {
    $obj = CustomThing::new();
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/Owner.php
+++ b/Owner.php
@@ -1,5 +1,5 @@
 <?php
 trait Factory {
-    public static function new(int $x): self {
+    public static function new(int $x, int $y): self {
         return new self($x);
     }
 }
"#;

    // ユーザーが --exclude-dir pjproject-2.15 を渡せば、impact からも除外される。
    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args([
            "context",
            "--dir",
            dir.path().to_str().unwrap(),
            "--exclude-dir",
            "pjproject-2.15",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("context stdout must be JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let owner_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("Owner.php"))
        .unwrap_or_else(|| panic!("Owner.php change not found: {stdout}"));

    let impacted = owner_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = owner_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let custom_in_impacted = impacted.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("pjproject-2.15"))
            .unwrap_or(false)
    });
    let custom_in_low = low.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .map(|p| p.contains("pjproject-2.15"))
            .unwrap_or(false)
    });
    assert!(
        !custom_in_impacted,
        "pjproject-2.15/ caller must NOT appear in impacted_callers when --exclude-dir is set. impacted: {impacted:?}"
    );
    assert!(
        !custom_in_low,
        "pjproject-2.15/ caller must NOT appear in low_confidence_callers either. low: {low:?}"
    );
}

/// 不正な `--exclude-glob` (構文エラー) がエラーで終了することを確認する。
///
/// silent empty 結果 (= 全 impact が消える) の方がユーザーにとって危険なので、
/// `validate_exclude_globs` で先行検証して `INVALID_REQUEST` で落とす。
#[test]
fn impact_invalid_exclude_glob_returns_error() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("foo.rs"), "fn x() {}\n").unwrap();
    let diff = r#"--- a/foo.rs
+++ b/foo.rs
@@ -1,1 +1,1 @@
-fn x() {}
+fn x(y: i32) {}
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args([
            "context",
            "--dir",
            dir.path().to_str().unwrap(),
            "--exclude-glob",
            "[",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");

    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "invalid --exclude-glob should fail. stdout: {stdout}"
    );
    assert!(
        stdout.contains("INVALID_REQUEST") || stdout.contains("invalid exclude-glob"),
        "expected JSON error mentioning invalid exclude-glob. got: {stdout}"
    );
}

// ---- impact パストラバーサル検証テスト ----

#[test]
fn impact_rejects_path_traversal_in_diff() {
    use std::io::Write;
    use std::process::Stdio;

    // diff パス内の .. はスキップされるべき
    let diff = r#"--- a/../../../etc/passwd
+++ b/../../../etc/passwd
@@ -1,3 +1,3 @@
-root:x:0:0:root
+root:x:0:0:hacked
"#;

    let dir = tempfile::tempdir().expect("tempdir");

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(diff.as_bytes())
        .unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("failed to wait");
    assert!(output.status.success());

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let changes = json["changes"].as_array().unwrap();
    assert!(
        changes.is_empty(),
        "パストラバーサルを含む diff は変更として認識されないべき"
    );
}

#[test]
fn impact_routes_import_only_refs_to_informational_callers() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src_dir = tmp.path().join("src");
    std::fs::create_dir_all(&src_dir).expect("create src dir");
    std::fs::write(
        src_dir.join("lib.ts"),
        "export function compute(input: number, offset: number = 0): number {\n  return input + offset;\n}\n",
    )
    .expect("write changed file");
    std::fs::write(
        src_dir.join("import_only.ts"),
        "import { compute } from './lib';\nexport const label = 'only import';\n",
    )
    .expect("write import-only file");
    std::fs::write(
        src_dir.join("barrel.ts"),
        "import { compute } from './lib';\n  export { compute };\n",
    )
    .expect("write barrel file");
    std::fs::write(
        src_dir.join("barrel_call.ts"),
        "import { compute } from './lib';\n  export { compute }; export const value = compute(1);\n",
    )
    .expect("write barrel call file");
    std::fs::write(
        src_dir.join("caller.ts"),
        "import { compute } from './lib';\nexport const value = compute(1);\n",
    )
    .expect("write caller file");

    let diff = "\
diff --git a/src/lib.ts b/src/lib.ts\n\
--- a/src/lib.ts\n\
+++ b/src/lib.ts\n\
@@ -1,3 +1,3 @@\n\
-export function compute(input: number): number {\n\
-  return input;\n\
+export function compute(input: number, offset: number = 0): number {\n\
+  return input + offset;\n\
 }\n";
    let service = astro_sight::service::AppService::new();
    let result = service
        .analyze_context(
            diff,
            tmp.path().to_str().expect("utf-8 path"),
            &astro_sight::models::impact::ContextAnalysisOptions::default(),
        )
        .expect("analyze_context should succeed");
    let change = result
        .changes
        .iter()
        .find(|c| c.path == "src/lib.ts")
        .expect("changed file impact should be present");

    assert!(
        change.impacted_callers.iter().any(|caller| {
            caller.path.ends_with("src/caller.ts") && caller.symbols == vec!["compute".to_string()]
        }),
        "actual call should remain in impacted_callers: {:?}",
        change.impacted_callers
    );
    assert!(
        change.impacted_callers.iter().any(|caller| {
            caller.path.ends_with("src/barrel_call.ts")
                && caller.symbols == vec!["compute".to_string()]
        }),
        "same-line call after re-export should remain in impacted_callers: {:?}",
        change.impacted_callers
    );
    assert!(
        change.informational_callers.iter().any(|caller| {
            caller.path.ends_with("src/import_only.ts")
                && caller.symbols == vec!["compute".to_string()]
                && caller.confidence.as_deref() == Some("informational")
        }),
        "import-only ref should be informational: {:?}",
        change.informational_callers
    );
    assert!(
        change.informational_callers.iter().any(|caller| {
            caller.path.ends_with("src/barrel.ts")
                && caller.symbols == vec!["compute".to_string()]
                && caller.confidence.as_deref() == Some("informational")
        }),
        "barrel re-export should be informational: {:?}",
        change.informational_callers
    );
    assert!(
        change
            .informational_callers
            .iter()
            .all(|caller| std::path::Path::new(&caller.path).is_relative()),
        "informational paths should stay workspace-relative: {:?}",
        change.informational_callers
    );
    assert!(
        !change
            .impacted_callers
            .iter()
            .any(|caller| caller.path.ends_with("src/import_only.ts")),
        "import-only ref must not be mixed into impacted_callers"
    );
    assert!(
        !change
            .impacted_callers
            .iter()
            .any(|caller| caller.path.ends_with("src/barrel.ts")),
        "barrel re-export must not be mixed into impacted_callers"
    );
}

// ---- impact --hook フラグテスト ----

#[test]
fn impact_hook_shows_triage_message() {
    // 変更のある diff を使って impact を実行し、--hook 時にトリアージメッセージが出力されることを確認
    let tmp_dir = tempfile::tempdir().unwrap();
    let src_dir = tmp_dir.path().join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        src_dir.join("lib.rs"),
        "pub fn compute(x: i32) -> i32 { x + 1 }\n",
    )
    .unwrap();
    std::fs::write(
        src_dir.join("main.rs"),
        "use crate::lib::compute;\nfn main() { compute(1); }\n",
    )
    .unwrap();

    // compute の署名変更 diff
    let diff = r#"--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1 +1 @@
-pub fn compute(x: i32) -> i32 { x + 1 }
+pub fn compute(x: i32, y: i32) -> i32 { x + y }
"#;

    let output = cargo_bin()
        .args([
            "impact",
            "--dir",
            tmp_dir.path().to_str().unwrap(),
            "--hook",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(diff.as_bytes())
                .unwrap();
            child.wait_with_output()
        })
        .expect("failed to run");

    // 未解決の影響がある場合、exit 1 で --hook メッセージが出る
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("astro-sight-triage"),
            "--hook 時にトリアージスキルの案内が表示されるべき: {}",
            stderr
        );
    }
    // 未解決影響がない場合（main.rs が解析できない等）は exit 0 で問題なし
}

/// TS/TSX 名前衝突 false positive 抑制 (Issue 2026-06-05-multi-attachment-conversations-fp):
/// `schema.ts` を import していない `ConversationList.tsx` の props 変数 `conversations`
/// (Drizzle table と同名) は `impacted_callers` ではなく `low_confidence_callers` に振り分け
/// られて Stop hook の blocking から外れる。
#[test]
fn impact_ts_name_collision_without_direct_import_routed_to_low_confidence() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("lib/db")).unwrap();
    std::fs::create_dir_all(dir.path().join("components")).unwrap();

    // schema.ts: Drizzle table export
    let schema_path = dir.path().join("lib/db/schema.ts");
    std::fs::write(&schema_path, "export const conversations = { id: 0 };\n").unwrap();

    // ConversationList.tsx: schema.ts を import せず、独自の interface + destructured props
    let tsx_path = dir.path().join("components/ConversationList.tsx");
    std::fs::write(
        &tsx_path,
        r#"interface Conversation { id: number }
interface Props { conversations: Conversation[] }
export function ConversationList({ conversations }: Props) {
    return conversations.length;
}
"#,
    )
    .unwrap();

    // diff: schema.ts の conversations を変更
    let diff = r#"--- a/lib/db/schema.ts
+++ b/lib/db/schema.ts
@@ -1 +1 @@
-export const conversations = { id: 0 };
+export const conversations = { id: 0, title: "" };
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let schema_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("lib/db/schema.ts"))
        .unwrap_or_else(|| panic!("schema.ts change not found: {stdout}"));
    let impacted = schema_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let low = schema_change
        .get("low_confidence_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    // ConversationList.tsx は schema.ts を import していないため、impacted_callers には
    // 出さず low_confidence_callers (informational) に振り分けるべき。Stop hook blocking から
    // 外しつつ Information として残す。
    let tsx_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("components/ConversationList.tsx"));
    assert!(
        !tsx_in_impacted,
        "ConversationList.tsx must NOT appear in impacted_callers (no direct import of schema.ts). got: {impacted:?} | low: {low:?}"
    );
    // 低 confidence の振り分け先に出ることも assert (codex 助言: low 出力自体を仕様化)。
    // path は absolute (tempdir) になりうるので末尾マッチで判定する。
    let tsx_in_low = low.iter().any(|c| {
        c.get("path")
            .and_then(|p| p.as_str())
            .is_some_and(|s| s.ends_with("components/ConversationList.tsx"))
    });
    assert!(
        tsx_in_low,
        "ConversationList.tsx は low_confidence_callers に出るべき (informational として残す)。got low: {low:?}"
    );
}

/// 直接 import している場合は high impact (`impacted_callers`) に残る (逆方向の回帰テスト):
/// `ConversationList.tsx` が `schema.ts` を直接 import している場合は従来通り
/// `impacted_callers` に出る。
#[test]
fn impact_ts_name_collision_with_direct_import_stays_high() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("lib/db")).unwrap();
    std::fs::create_dir_all(dir.path().join("components")).unwrap();

    let schema_path = dir.path().join("lib/db/schema.ts");
    std::fs::write(&schema_path, "export const conversations = { id: 0 };\n").unwrap();

    // ConversationList.tsx: schema.ts を直接 import する
    let tsx_path = dir.path().join("components/ConversationList.tsx");
    std::fs::write(
        &tsx_path,
        r#"import { conversations } from "../lib/db/schema";
export function getList() {
    return conversations;
}
"#,
    )
    .unwrap();

    let diff = r#"--- a/lib/db/schema.ts
+++ b/lib/db/schema.ts
@@ -1 +1 @@
-export const conversations = { id: 0 };
+export const conversations = { id: 0, title: "" };
"#;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .args(["context", "--dir", dir.path().to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn context");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(diff.as_bytes())
        .expect("write stdin");
    let output = child.wait_with_output().expect("failed to wait");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "context failed: {stdout}");

    let json: serde_json::Value = serde_json::from_str(&stdout).expect("JSON");
    let changes = json
        .get("changes")
        .and_then(|c| c.as_array())
        .expect("changes array");
    let schema_change = changes
        .iter()
        .find(|c| c.get("path").and_then(|p| p.as_str()) == Some("lib/db/schema.ts"))
        .unwrap_or_else(|| panic!("schema.ts change not found: {stdout}"));
    let impacted = schema_change
        .get("impacted_callers")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();

    let tsx_in_impacted = impacted
        .iter()
        .any(|c| c.get("path").and_then(|p| p.as_str()) == Some("components/ConversationList.tsx"));
    assert!(
        tsx_in_impacted,
        "ConversationList.tsx は schema.ts を直接 import しているため impacted_callers に出るべき。got: {impacted:?}"
    );
}

#[test]
fn impact_fn_value_passed_ref_is_informational_for_signature_only_change() {
    // Issue 2026-07-12-bevy-systemparam-optional-res-impact-fp:
    // pub fn の引数型のみ変更 (arity 不変) のとき、タプル要素として値渡しされる
    // だけの参照 (callee でない) は unresolved impact に出ない (informational 格下げ)。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"sysparam-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub mod sys;\npub mod reg;\n").unwrap();
    std::fs::write(
        root.join("src/sys.rs"),
        "pub struct Res<T>(pub T);\npub fn my_system(r: Res<u32>) {\n    let _ = r;\n}\npub fn other_a() {}\npub fn other_b() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/reg.rs"),
        "use crate::sys::{my_system, other_a, other_b};\n\npub fn register<F>(_f: F) {}\n\npub fn setup() {\n    register((other_a, my_system, other_b));\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    // Res<u32> -> Option<Res<u32>> (arity 不変の型のみ変更)
    std::fs::write(
        root.join("src/sys.rs"),
        "pub struct Res<T>(pub T);\npub fn my_system(r: Option<Res<u32>>) {\n    let _ = r;\n}\npub fn other_a() {}\npub fn other_b() {}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "値渡し参照のみなら unresolved impact を出さず exit 0: {stderr}"
    );
}

#[test]
fn impact_fn_callee_ref_stays_blocking_for_signature_change() {
    // 対照: callee として呼び出している参照はシグネチャ変更で blocking のまま。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"callee-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub mod sys;\npub mod caller;\n").unwrap();
    std::fs::write(
        root.join("src/sys.rs"),
        "pub struct Res<T>(pub T);\npub fn my_system(r: Res<u32>) {\n    let _ = r;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/caller.rs"),
        "use crate::sys::{Res, my_system};\n\npub fn drive() {\n    my_system(Res(1));\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    std::fs::write(
        root.join("src/sys.rs"),
        "pub struct Res<T>(pub T);\npub fn my_system(r: Option<Res<u32>>) {\n    let _ = r;\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "callee 参照はシグネチャ変更で blocking (exit 1) のまま"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("caller.rs:4"),
        "人間向け表示はエディタと同じ 1-indexed 行番号: {stderr}"
    );
}

#[test]
fn impact_fn_direct_argument_ref_stays_blocking() {
    // codex レビュー指摘 (重大1): 直接引数 (`accept(my_system)`) は呼び出し先が
    // `fn accept(_: fn(u32))` のような fn ポインタ引数を取る場合に型変更で壊れる。
    // AST だけでは渡し先シグネチャを解決できないため、タプル/配列要素と違い
    // blocking を維持する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"directarg-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub mod sys;\npub mod reg;\n").unwrap();
    std::fs::write(
        root.join("src/sys.rs"),
        "pub fn my_system(x: u32) {\n    let _ = x;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/reg.rs"),
        "use crate::sys::my_system;\n\npub fn accept(_f: fn(u32)) {}\n\npub fn setup() {\n    accept(my_system);\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    // u32 -> String (arity 不変の型のみ変更) — fn ポインタ引数へ渡しているため壊れる
    std::fs::write(
        root.join("src/sys.rs"),
        "pub fn my_system(x: String) {\n    let _ = x;\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "直接引数の関数値渡しは blocking (exit 1) を維持すべき"
    );
}

#[test]
fn impact_fn_typed_tuple_ref_stays_blocking() {
    // codex 再レビュー指摘 (重大1): `let handlers: (fn(u32),) = (my_system,);` のように
    // タプル/配列の外側に明示型注釈がある場合、要素は fn ポインタ型に固定され
    // シグネチャ変更 (arity 不変でも) でコンパイルエラーになる。informational へ
    // 格下げせず blocking を維持する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"typedtuple-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\npath = \"src/lib.rs\"\n",
    )
    .unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub mod sys;\npub mod reg;\n").unwrap();
    std::fs::write(
        root.join("src/sys.rs"),
        "pub fn my_system(x: u32) {\n    let _ = x;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/reg.rs"),
        "use crate::sys::my_system;\n\npub fn setup() {\n    let handlers: (fn(u32),) = (my_system,);\n    let _ = handlers;\n}\n",
    )
    .unwrap();

    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "astro-sight@example.com"]);
    git(&["config", "user.name", "astro-sight"]);
    git(&["add", "."]);
    git(&["commit", "-m", "initial", "-q"]);

    // u32 -> String (arity 不変の型のみ変更) — fn ポインタ型固定のため壊れる
    std::fs::write(
        root.join("src/sys.rs"),
        "pub fn my_system(x: String) {\n    let _ = x;\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["impact", "--dir", root.to_str().unwrap(), "--git"])
        .output()
        .expect("failed to run");
    assert!(
        !output.status.success(),
        "明示型付きタプル要素の関数値渡しは blocking (exit 1) を維持すべき"
    );
}
