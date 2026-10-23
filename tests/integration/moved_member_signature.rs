//! 型の移動と同じ差分でのメソッドのシグネチャ変更が、Stop フックの review で止まるか。

use super::support::*;

const OLD_TYPE: &str = "pub struct Packer;\n\nimpl Packer {\n    pub fn pack_with_progress(&self, count: u32) -> u32 {\n        count * 2\n    }\n}\n";
const NEW_TYPE: &str = "pub struct Packer;\n\nimpl Packer {\n    pub fn pack_with_progress(&self, count: u32, report: impl Fn(u32)) -> u32 {\n        report(count);\n        count * 2\n    }\n}\n";

fn caller(module: &str, call: &str) -> String {
    format!("use crate::{module}::Packer;\n\npub fn handle() -> u32 {{\n    {call}\n}}\n")
}

fn names(hook: &serde_json::Value, key: &str) -> Vec<String> {
    hook["api"][key]
        .as_array()
        .map(|symbols| {
            symbols
                .iter()
                .filter_map(|symbol| symbol["n"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn moved_member_signature_hook_closes_updated_calls_and_blocks_stale_calls() {
    for updated in [true, false] {
        let repo = TestRepo::new();
        repo.create_dir_all("src");
        repo.write(
            "Cargo.toml",
            "[package]\nname = \"shelf\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        repo.write("src/lib.rs", "pub mod caller;\npub mod old_place;\n");
        repo.write("src/old_place.rs", OLD_TYPE);
        repo.write(
            "src/caller.rs",
            caller("old_place", "Packer.pack_with_progress(3)"),
        );
        repo.init_git();
        repo.commit_all("base");
        repo.git(["mv", "src/old_place.rs", "src/new_place.rs"]);
        repo.write("src/lib.rs", "pub mod caller;\npub mod new_place;\n");
        repo.write("src/new_place.rs", NEW_TYPE);
        let call = if updated {
            "Packer.pack_with_progress(3, |_| {})"
        } else {
            "Packer.pack_with_progress(3)"
        };
        repo.write("src/caller.rs", caller("new_place", call));

        let output = cargo_bin()
            .args([
                "review",
                "--dir",
                repo.root().to_str().unwrap(),
                "--git",
                "--hook",
            ])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(i32::from(!updated)), "{stderr}");
        let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(
            names(&hook, "moved").iter().any(|n| n == "Packer"),
            "{hook}"
        );
        assert!(names(&hook, "rm").is_empty(), "{hook}");
        let key = if updated { "mod_closed" } else { "mod" };
        assert!(
            names(&hook, key)
                .iter()
                .any(|n| n == "Packer.pack_with_progress"),
            "{hook}"
        );
    }
}
