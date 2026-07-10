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
        let mut command = super::super::cargo_bin();
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

        let mut child = super::super::cargo_bin()
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
