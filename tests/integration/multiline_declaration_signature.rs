//! 複数行に折り返した宣言の signature_changes の統合テスト。
//!
//! 行ベースの検出は宣言の名前がある行しか見ないため、複数行に折り返した宣言では
//! `def f(` / `pub fn f(` のように署名が途中で切れていた (Issue #190)。
//! 新旧の宣言を 1 つずつ確認できて契約も違うときは、正規化したヘッダへ差し替える。

#[allow(unused_imports)]
use super::support::*;

/// `signature_changes` から 1 件だけ取り出す。
fn only_signature_change(context: &serde_json::Value) -> (String, String) {
    let changes = context["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("changes が無い: {context}"));
    let signatures = changes
        .iter()
        .flat_map(|change| {
            change["signature_changes"]
                .as_array()
                .map(|array| array.iter())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    assert_eq!(signatures.len(), 1, "{context}");
    (
        signatures[0]["old_signature"]
            .as_str()
            .unwrap_or_else(|| panic!("old_signature が無い: {context}"))
            .to_string(),
        signatures[0]["new_signature"]
            .as_str()
            .unwrap_or_else(|| panic!("new_signature が無い: {context}"))
            .to_string(),
    )
}

#[test]
fn python_multiline_def_reports_full_signature() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "a.py",
        "def measure(module, size: int, repeats: int) -> dict:\n    return {\"size\": size}\n",
    );
    repo.write(
        "b.py",
        "from a import measure\n\n\ndef run():\n    return measure(None, 1, 2)\n",
    );
    repo.commit_all("initial");
    repo.write(
        "a.py",
        concat!(
            "def measure(\n",
            "    module,\n",
            "    size: int,\n",
            "    repeats: int,\n",
            "    chunk_samples: int = 1,\n",
            ") -> dict:\n",
            "    return {\"size\": size, \"chunk\": chunk_samples}\n",
        ),
    );

    let (old_signature, new_signature) =
        only_signature_change(&repo.run_json("context", &["--git"]));
    assert_eq!(
        old_signature,
        "def measure(module, size: int, repeats: int) -> dict:"
    );
    assert_eq!(
        new_signature,
        "def measure(module, size: int, repeats: int, chunk_samples: int = 1) -> dict:"
    );
}

#[test]
fn rust_multiline_fn_reports_full_signature() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.create_dir_all("src");
    repo.write(
        "src/lib.rs",
        concat!(
            "pub fn measure(module: &str, size: usize) -> usize {\n",
            "    module.len() + size\n",
            "}\n",
            "\n",
            "pub fn run() -> usize {\n",
            "    measure(\"a\", 1)\n",
            "}\n",
        ),
    );
    repo.commit_all("initial");
    repo.write(
        "src/lib.rs",
        concat!(
            "pub fn measure(\n",
            "    module: &str,\n",
            "    size: usize,\n",
            "    repeats: usize,\n",
            ") -> usize {\n",
            "    module.len() + size + repeats\n",
            "}\n",
            "\n",
            "pub fn run() -> usize {\n",
            "    measure(\"a\", 1, 2)\n",
            "}\n",
        ),
    );

    let (old_signature, new_signature) =
        only_signature_change(&repo.run_json("context", &["--git"]));
    assert_eq!(
        old_signature,
        "pub fn measure(module: &str, size: usize) -> usize"
    );
    assert_eq!(
        new_signature,
        "pub fn measure(module: &str, size: usize, repeats: usize) -> usize"
    );
}

#[test]
fn typescript_multiline_function_reports_full_signature() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "a.ts",
        "export function measure(module: string, size: number): number {\n  return module.length + size;\n}\n",
    );
    repo.write(
        "b.ts",
        "import { measure } from './a';\n\nexport function run(): number {\n  return measure('a', 1);\n}\n",
    );
    repo.commit_all("initial");
    repo.write(
        "a.ts",
        concat!(
            "export function measure(\n",
            "  module: string,\n",
            "  size: number,\n",
            "  repeats: number,\n",
            "): number {\n",
            "  return module.length + size + repeats;\n",
            "}\n",
        ),
    );

    let (old_signature, new_signature) =
        only_signature_change(&repo.run_json("context", &["--git"]));
    assert_eq!(
        old_signature,
        "export function measure(module: string, size: number): number"
    );
    assert_eq!(
        new_signature,
        "export function measure(module: string, size: number, repeats: number): number"
    );
}

#[test]
fn single_line_declaration_keeps_the_line_based_signature() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    repo.create_dir_all("src");
    repo.write(
        "src/lib.rs",
        concat!(
            "pub fn measure(module: &str, size: usize) -> usize {\n",
            "    module.len() + size\n",
            "}\n",
            "\n",
            "pub fn run() -> usize {\n",
            "    measure(\"a\", 1)\n",
            "}\n",
        ),
    );
    repo.commit_all("initial");
    repo.write(
        "src/lib.rs",
        concat!(
            "pub fn measure(module: &str, size: usize, repeats: usize) -> usize {\n",
            "    module.len() + size + repeats\n",
            "}\n",
            "\n",
            "pub fn run() -> usize {\n",
            "    measure(\"a\", 1, 2)\n",
            "}\n",
        ),
    );

    // 1 行の宣言は行ベースの検出だけで全体が取れるため、表示を差し替えない。
    let (old_signature, new_signature) =
        only_signature_change(&repo.run_json("context", &["--git"]));
    assert_eq!(
        old_signature,
        "pub fn measure(module: &str, size: usize) -> usize {"
    );
    assert_eq!(
        new_signature,
        "pub fn measure(module: &str, size: usize, repeats: usize) -> usize {"
    );
}

#[test]
fn multiline_wrap_without_contract_change_reports_nothing() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        "a.py",
        "def measure(module, size: int, repeats: int) -> dict:\n    return {\"size\": size}\n",
    );
    repo.commit_all("initial");
    // 折り返しだけで契約は変わらないので、シグネチャ変更として報告しない。
    repo.write(
        "a.py",
        concat!(
            "def measure(\n",
            "    module,\n",
            "    size: int,\n",
            "    repeats: int,\n",
            ") -> dict:\n",
            "    return {\"size\": size}\n",
        ),
    );

    let context = repo.run_json("context", &["--git"]);
    let signatures: Vec<_> = context["changes"]
        .as_array()
        .unwrap_or_else(|| panic!("changes が無い: {context}"))
        .iter()
        .flat_map(|change| {
            change["signature_changes"]
                .as_array()
                .map(|array| array.iter())
                .unwrap_or_default()
        })
        .collect();
    assert!(signatures.is_empty(), "{context}");
}
