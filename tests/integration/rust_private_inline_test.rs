use super::support::{TestRepo, cargo_bin};

const BEFORE: &str = "struct Packet { data: Vec<u8> }\nfn exists(input: &str)->bool{ !input.is_empty() }\n#[cfg(test)]mod tests{use super::*;#[test]fn sample(){assert!(exists(\"x\"));}}\n";
const AFTER: &str = "struct Packet { data: Vec<u8> }\nfn exists(input: &Packet)->bool{ !input.data.is_empty() }\n#[cfg(test)]mod tests{use super::*;#[test]fn sample(){assert!(exists(&Packet{data:vec![1]}));}}\n";
const SCREEN: &str = "use std::path::Path;\npub fn measure()->bool {Path::exists(Path::new(\"x\"))}\n#[cfg(test)]mod tests{use super::*;#[test]fn sample(){assert!(!measure());}}\n";

fn fixture(root: &str, screen: &str, child: bool) -> TestRepo {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    );
    repo.write(
        format!("src/{root}.rs"),
        "mod calculation; mod screen; fn main() { println!(\"{}\", screen::measure()); }\n",
    );
    repo.write("src/screen.rs", screen);
    if child {
        repo.create_dir_all("src/calculation");
        repo.write(
            "src/calculation/child.rs",
            "pub fn actual() -> bool { super::exists(\"x\") }\n",
        );
    }
    let prefix = if child { "mod child;\n" } else { "" };
    repo.write("src/calculation.rs", format!("{prefix}{BEFORE}"));
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/calculation.rs", format!("{prefix}{AFTER}"));
    repo
}

fn assert_status(repo: &TestRepo, blocking: bool) {
    let output = cargo_bin()
        .args(["impact", "--git", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(i32::from(blocking)),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(i32::from(blocking)), "{hook}");
    assert_eq!(hook["impacts"].is_array(), blocking, "{hook}");
}

#[test]
fn rust_private_function_inline_test_updates_preserve_sibling_methods() {
    for (root, screen) in [
        ("main", SCREEN),
        ("lib", SCREEN),
        (
            "lib",
            "struct Checker; impl Checker { fn exists() -> bool { true } } pub fn measure() -> bool { Checker::exists() }\n",
        ),
    ] {
        let repo = fixture(root, screen, false);
        let context = repo.run_json("context", &["--git"]);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == "src/calculation.rs")
            .unwrap();
        assert!(change["impacted_callers"].is_null(), "{context}");
        let refs = repo.run_json("refs", &["--name", "exists"]);
        assert!(
            refs["refs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reference| reference["path"] == "src/screen.rs"),
            "{refs}"
        );
        assert_status(&repo, false);
    }
}

#[test]
fn rust_private_function_inline_test_keeps_real_descendant_callers() {
    let repo = fixture("lib", SCREEN, true);
    let context = repo.run_json("context", &["--git"]);
    assert!(
        context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|change| change["impacted_callers"].as_array().into_iter().flatten())
            .any(|caller| caller["path"] == "src/calculation/child.rs"
                && caller["symbols"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|name| name == "exists")),
        "{context}"
    );
    assert_status(&repo, true);
}
