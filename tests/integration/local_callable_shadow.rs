use super::support::{TestRepo, cargo_bin};

fn fixture(extension: &str, local: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    let code = |source: &str| {
        if extension == "js" {
            source.replace(": number", "")
        } else {
            source.to_owned()
        }
    };
    repo.write(
        format!("pricing.{extension}"),
        code("export function total(price: number): number {\n  return price;\n}\n"),
    );
    repo.write(
        format!("cart.{extension}"),
        "import { total } from './pricing';\nconsole.log(total(100));\n",
    );
    repo.write(format!("report.{extension}"), code(local));
    repo.commit_all("base");
    repo.write(
        format!("pricing.{extension}"),
        code("export function total(price: number, tax: number): number {\n  return price + tax;\n}\n"),
    );
    repo.write(
        format!("cart.{extension}"),
        "import { total } from './pricing';\nconsole.log(total(100, 10));\n",
    );
    repo
}

fn assert_hook(repo: &TestRepo, closed: bool, staged: bool, label: &str) {
    let mut command = cargo_bin();
    command.args(["review", "--git", "--hook", "--dir"]);
    command.arg(repo.root());
    if staged {
        command.arg("--staged");
    }
    let output = command.output().unwrap();
    let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    let bucket = if closed { "mod_closed" } else { "mod" };
    assert!(
        json["api"][bucket]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item["n"] == "total")),
        "{label}: {json}"
    );
    assert_eq!(
        output.status.code(),
        Some(i32::from(!closed)),
        "{label}: {json}"
    );
}

#[test]
fn local_callable_shadow_closes_updated_api_in_js_ts_and_tsx() {
    for extension in ["js", "ts", "tsx"] {
        for declaration in [
            "function total(): number { return 0; }",
            "const total = (): number => 0;",
            "const total = function (): number { return 0; };",
            "let total = (): number => 0;",
            "var total = function (): number { return 0; };",
            "const total = (() => 0);",
        ] {
            let source =
                format!("export function report() {{\n  {declaration}\n  return total();\n}}\n");
            let repo = fixture(extension, &source);
            assert_hook(&repo, true, false, &source);
            repo.stage_all();
            assert_hook(&repo, true, true, &source);
            let refs = repo.run_json("refs", &["--name", "total"]);
            assert!(
                refs["refs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|reference| { reference["path"] == format!("report.{extension}") }),
                "{refs}"
            );
        }
    }
}

#[test]
fn local_callable_shadow_keeps_unproven_bindings_and_real_callers_blocking() {
    for source in [
        "export function report() { let total = () => 0; total = other; return total(); }",
        "export function report() { let total = () => 0; function reset() { total = other; } return total(); }",
        "export function report() { let total = () => 0; ({total} = other); return total(); }",
        "export function report() { let total = () => 0; total += other; return total(); }",
        "export function report() { let total = () => 0; total++; return total(); }",
        "export function report() { let total = () => 0; (total as any) = other; return total(); }",
        "export function report() { let total = () => 0; for (total of others) {} return total(); }",
        "export function report() { var total = () => 0; var total = other; return total(); }",
        "export function report() { if (flag) { var total = () => 0; } return total(); }",
        "export function report() { let total = () => 0; with (other) { total = other; } return total(); }",
        "export function report() { const total = useCallback(() => 0, []); return total(); }",
        "export function report() { const total = other; return total(); }",
        "export function report() { total(); const total = () => 0; }",
        "export function report() { let total = () => 0; eval('total = other'); return total(); }",
        "export function report() { let total = () => 0; (eval)('total = other'); return total(); }",
        "export function report() { let total = () => 0; t\\u006ftal = other; return total(); }",
        "export function report() { const total = () => 0; enum total { A } return total(); }",
        "export function report() { const total = () => 0; return total( ; }",
        "import { total } from './pricing'; export function report() { const total = () => 0; return total(); } console.log(total(100));",
        "import { total } from './pricing'; export function report() { if (flag) { const total = () => 0; total(); } return total(100); }",
    ] {
        let repo = fixture("ts", source);
        assert_hook(&repo, false, false, source);
    }
}
