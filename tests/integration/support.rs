use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

pub(super) struct TestRepo {
    temp_dir: tempfile::TempDir,
}

impl TestRepo {
    pub(super) fn new() -> Self {
        Self {
            temp_dir: tempfile::tempdir().expect("failed to create test repository"),
        }
    }

    pub(super) fn root(&self) -> &Path {
        self.temp_dir.path()
    }

    pub(super) fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root().join(relative)
    }

    pub(super) fn create_dir_all(&self, relative: impl AsRef<Path>) {
        std::fs::create_dir_all(self.path(relative)).expect("failed to create fixture directory");
    }

    pub(super) fn write(&self, relative: impl AsRef<Path>, content: impl AsRef<[u8]>) {
        std::fs::write(self.path(relative), content).expect("failed to write fixture file");
    }

    pub(super) fn remove_file(&self, relative: impl AsRef<Path>) {
        std::fs::remove_file(self.path(relative)).expect("failed to remove fixture file");
    }

    pub(super) fn init_git(&self) {
        self.git(["init", "-q"]);
        self.git(["config", "user.email", "astro-sight@example.com"]);
        self.git(["config", "user.name", "astro-sight"]);
    }

    pub(super) fn commit_all(&self, message: &str) {
        self.stage_all();
        self.git(["commit", "-m", message, "-q"]);
    }

    pub(super) fn stage_all(&self) {
        self.git(["add", "."]);
    }

    pub(super) fn git<I, S>(&self, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let status = Command::new("git")
            .args(args)
            .current_dir(self.root())
            .status()
            .expect("failed to run git");
        assert!(status.success(), "git command failed");
    }

    pub(super) fn run_json(&self, subcommand: &str, args: &[&str]) -> serde_json::Value {
        let mut command = cargo_bin();
        command
            .arg(subcommand)
            .arg("--dir")
            .arg(self.root())
            .args(args);
        parse_json_output(command.output().expect("failed to run astro-sight"))
    }

    pub(super) fn run_json_with_stdin(
        &self,
        subcommand: &str,
        args: &[&str],
        input: &[u8],
    ) -> serde_json::Value {
        use std::io::Write;

        let mut child = cargo_bin()
            .arg(subcommand)
            .arg("--dir")
            .arg(self.root())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn astro-sight");
        child
            .stdin
            .as_mut()
            .expect("missing child stdin")
            .write_all(input)
            .expect("failed to write command input");
        drop(child.stdin.take());
        parse_json_output(
            child
                .wait_with_output()
                .expect("failed to wait for astro-sight"),
        )
    }
}

fn parse_json_output(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "astro-sight failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("astro-sight returned invalid JSON")
}

// --- tests/integration.rs から集約した共有ヘルパー ---

pub(super) const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");

pub(super) fn cargo_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_astro-sight"))
}

/// MCP テスト用ヘルパー: initialize + initialized 後に追加メッセージを送信し、stdout を返す
pub(super) fn mcp_send_after_init(extra_messages: &[&str]) -> String {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_astro-sight"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    // 別スレッドで stdout を最後まで読み取る
    let reader_handle = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        let mut lines = Vec::new();
        for line in reader.lines() {
            match line {
                Ok(l) => lines.push(l),
                Err(_) => break,
            }
        }
        lines
    });

    // initialize 送信
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"test","version":"1.0"}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    // サーバーが initialize を処理する時間を確保
    std::thread::sleep(std::time::Duration::from_millis(500));

    // initialized 通知 + 追加メッセージを送信
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
    )
    .unwrap();
    for msg in extra_messages {
        writeln!(stdin, "{msg}").unwrap();
    }
    stdin.flush().unwrap();
    drop(stdin);

    let _ = child.wait();
    let lines = reader_handle.join().expect("reader thread panicked");
    lines.join("\n")
}

/// A3 用 helper: base_files を commit し、`setup` で削除/追加した temp git repo を作って
/// `review --git --hook` の `Output` (exit code + stderr の hook JSON) を返す。
pub(super) fn a3_review_hook(
    setup: impl FnOnce(&std::path::Path),
    base_files: &[(&str, &str)],
) -> std::process::Output {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?} failed");
    };
    for (rel, content) in base_files {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }
    git(&["init", "-q"]);
    git(&["config", "user.email", "a@b.c"]);
    git(&["config", "user.name", "t"]);
    git(&["add", "."]);
    git(&["commit", "-m", "init", "-q"]);
    setup(root);
    cargo_bin()
        .args(["review", "--dir", root.to_str().unwrap(), "--git", "--hook"])
        .output()
        .expect("run review")
}

/// 新規ファイル追加の unified diff 片を組み立てるテストヘルパー。
pub(super) fn make_new_file_diff(path: &str, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let n = lines.len();
    let body: String = lines.iter().map(|l| format!("+{l}\n")).collect();
    format!(
        "diff --git a/{path} b/{path}\nnew file mode 100644\n--- /dev/null\n+++ b/{path}\n@@ -0,0 +1,{n} @@\n{body}"
    )
}
