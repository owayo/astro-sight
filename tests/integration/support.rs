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
        parse_json_output(
            self.command(subcommand, args)
                .output()
                .expect("failed to run astro-sight"),
        )
    }

    /// `run_json` と同じだが、`--dir` をこのディレクトリの下の `dir` にし、子プロセスに `envs`
    /// (`CARGO_HOME` など) を渡す。Cargo の設定は `dir` からこのディレクトリまでの祖先を読む。
    pub(super) fn run_json_in<K, V>(
        &self,
        dir: impl AsRef<Path>,
        envs: impl IntoIterator<Item = (K, V)>,
        subcommand: &str,
        args: &[&str],
    ) -> serde_json::Value
    where
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        parse_json_output(
            self.command_in(&self.path(dir), subcommand, args)
                .envs(envs)
                .output()
                .expect("failed to run astro-sight"),
        )
    }

    pub(super) fn run_json_with_stdin(
        &self,
        subcommand: &str,
        args: &[&str],
        input: &[u8],
    ) -> serde_json::Value {
        use std::io::Write;

        let mut child = self
            .command(subcommand, args)
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

    fn command(&self, subcommand: &str, args: &[&str]) -> Command {
        self.command_in(self.root(), subcommand, args)
    }

    /// `dir` を `--dir` にした astro-sight の起動。Rust の外部クレートの別名の証明が読む Cargo の
    /// 設定の祖先の走査を、Cargo 自身のテストと同じ `__CARGO_TEST_ROOT` でこのディレクトリに
    /// 止める (一時ディレクトリの祖先にある開発機の `.cargo/config.toml` を読まない)。
    fn command_in(&self, dir: &Path, subcommand: &str, args: &[&str]) -> Command {
        let root = std::fs::canonicalize(self.root()).expect("failed to resolve test repository");
        let mut command = cargo_bin();
        command
            .env("__CARGO_TEST_ROOT", root)
            .arg(subcommand)
            .arg("--dir")
            .arg(dir)
            .args(args);
        command
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
    let missing_config = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/.astro-sight-test-config-does-not-exist.toml");
    let missing_cargo_home = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/.astro-sight-test-cargo-home-does-not-exist");
    for path in [&missing_config, &missing_cargo_home] {
        assert!(
            !path.exists(),
            "テスト隔離用のパスが存在しています: {}",
            path.display()
        );
    }

    let mut command = cargo_bin_with_explicit_config();
    // 開発者の実設定に format = "toon" / "auto" があっても、JSON 契約のテストへ漏らさない。
    command.arg("--config").arg(missing_config);
    // Rust の外部クレートの別名の証明が読む Cargo の環境 (`$CARGO_HOME/config.toml`、rustc の
    // 差し替えと rustflags の環境変数) も、開発機のものを漏らさない。
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if upper.starts_with("CARGO_")
            || upper.starts_with("RUSTC")
            || upper == "RUSTFLAGS"
            || upper == "__CARGO_TEST_ROOT"
            || matches!(
                upper.as_str(),
                "LD_PRELOAD" | "LD_AUDIT" | "DYLD_INSERT_LIBRARIES"
            )
        {
            command.env_remove(name);
        }
    }
    command.env("CARGO_HOME", missing_cargo_home);
    command
}

pub(super) fn cargo_bin_with_explicit_config() -> Command {
    Command::new(env!("CARGO_BIN_EXE_astro-sight"))
}

/// MCP テスト用ヘルパー: initialize + initialized 後に追加メッセージを送信し、stdout を返す
pub(super) fn mcp_send_after_init(extra_messages: &[&str]) -> String {
    mcp_send_after_init_with(&[], None, extra_messages)
}

/// `mcp_send_after_init` の拡張版。`global_args` (`--pretty` / `--format toon` 等) を
/// `mcp` サブコマンドの前に付け、`cwd` を指定すればそこをサンドボックスとして起動する。
///
/// initialize の応答を待ってから追加メッセージを送り、`id` を持つ要求の応答がそろう
/// (または 300 秒の期限を過ぎる) まで stdin を閉じない。stdin を閉じるとサーバーは処理中の
/// 要求を残したまま終了するので、要求の直後に閉じていた旧実装は、負荷の高い環境
/// (load average 100〜300) で重い要求の応答が欠け、テストの中身と関係なく落ちていた。
/// サーバーが落ちた場合は出力がその時点で閉じるので、欠けた応答として検出できる。
pub(super) fn mcp_send_after_init_with(
    global_args: &[&str],
    cwd: Option<&Path>,
    extra_messages: &[&str],
) -> String {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    let mut command = cargo_bin();
    command.args(global_args).arg("mcp");
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn mcp");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    // 別スレッドで stdout を 1 行ずつ読み、届いた順に渡す
    let (tx, rx) = mpsc::channel::<String>();
    let reader_handle = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(300);
    let mut lines: Vec<String> = Vec::new();
    // 期限内に `id` の応答が届くまで読む。出力が閉じたら (サーバーの終了) false。
    let wait_for = |id: &serde_json::Value, lines: &mut Vec<String>| -> bool {
        loop {
            if lines
                .iter()
                .any(|line| mcp_response_id(line).as_ref() == Some(id))
            {
                return true;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(remaining) {
                Ok(line) => lines.push(line),
                Err(_) => return false,
            }
        }
    };

    // initialize 送信
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"test","version":"1.0"}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    wait_for(&serde_json::json!(1), &mut lines);

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
    for id in extra_messages.iter().filter_map(|msg| mcp_request_id(msg)) {
        if !wait_for(&id, &mut lines) {
            break;
        }
    }
    drop(stdin);

    let _ = child.wait();
    reader_handle.join().expect("reader thread panicked");
    lines.extend(rx.try_iter());
    lines.join("\n")
}

/// JSON-RPC の要求 (`method` と null でない `id` を持つ) なら、その `id`。
fn mcp_request_id(message: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(message).ok()?;
    value.get("method")?;
    value.get("id").filter(|id| !id.is_null()).cloned()
}

/// JSON-RPC の応答 (`method` を持たない) なら、その `id`。
fn mcp_response_id(line: &str) -> Option<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("method").is_some() {
        return None;
    }
    value.get("id").cloned()
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
