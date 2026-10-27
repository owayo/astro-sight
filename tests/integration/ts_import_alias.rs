use super::support::{TestRepo, cargo_bin};

const OLD: &str = "import { total as sum } from './pricing';\nconsole.log(sum(100));\n";
const NEW: &str = "import { total as sum } from './pricing';\nconsole.log(sum(100, 10));\n";

fn fixture(extension: &str, before: &str, after: &str, extras: &[(&str, &str)]) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(
        format!("pricing.{extension}"),
        "export function total(price: number): number {\n  return price;\n}\n",
    );
    repo.write(format!("cart.{extension}"), before);
    for (path, source) in extras {
        repo.write(path, source);
    }
    repo.commit_all("base");
    repo.write(
        format!("pricing.{extension}"),
        "export function total(price: number, tax: number): number {\n  return price + tax;\n}\n",
    );
    repo.write(format!("cart.{extension}"), after);
    repo
}

fn assert_hook(repo: &TestRepo, closed: bool, staged: bool, label: &str) {
    let mut command = cargo_bin();
    command
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
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
fn ts_import_alias_updated_calls_close_in_normal_and_staged_snapshots() {
    for extension in ["ts", "tsx"] {
        for (before, after) in [
            (OLD.to_owned(), NEW.to_owned()),
            (
                format!("{OLD}const newline = '\\n';\n"),
                format!("{NEW}const newline = '\\n';\n"),
            ),
            (
                OLD.replace("'./pricing'", "'./pricing.js'"),
                NEW.replace("'./pricing'", "'./pricing.js'"),
            ),
            (OLD.replace(";\n", "; "), NEW.replace(";\n", "; ")),
            (
                OLD.replace("sum(100)", "sum(\n  100,\n)"),
                OLD.replace("sum(100)", "sum(\n  100,\n  10,\n)"),
            ),
            (
                format!("// 日本語\r\n{}", OLD.replace('\n', "\r\n")),
                format!("// 日本語\r\n{}", NEW.replace('\n', "\r\n")),
            ),
        ] {
            let repo = fixture(extension, &before, &after, &[]);
            assert_hook(&repo, true, false, &after);
            repo.stage_all();
            repo.write(format!("cart.{extension}"), OLD);
            assert_hook(&repo, true, true, "index must own alias evidence");
            assert_hook(&repo, false, false, "working-tree call is stale");
        }
    }
}

#[test]
fn ts_import_alias_resolves_inner_shadows_and_checks_every_consumer() {
    let shadow = "function local(sum: () => number) { return sum(); }\nfunction other() { const sum = () => 0; return sum(); }\nfunction third() { function sum() { return 0; } return sum(); }\n";
    let repo = fixture(
        "ts",
        &format!("{OLD}{shadow}"),
        &format!("{NEW}{shadow}"),
        &[],
    );
    assert_hook(&repo, true, false, "inner scopes have independent bindings");

    let repo = fixture(
        "ts",
        "import { total as sum, total as add } from './pricing';\nsum(100); add(200);",
        "import { total as sum, total as add } from './pricing';\nsum(100, 10); add(200, 20);",
        &[],
    );
    assert_hook(&repo, true, false, "multiple aliases");

    let plain = "import { total } from './pricing';\nconsole.log(total(100));\n";
    let repo = fixture("ts", OLD, NEW, &[("plain.ts", plain)]);
    assert_hook(&repo, false, false, "unchanged plain caller");
    repo.write("plain.ts", plain.replace("total(100)", "total(100, 10)"));
    assert_hook(&repo, true, false, "all plain and alias callers updated");
    let repo = fixture("ts", OLD, NEW, &[("other.ts", OLD)]);
    assert_hook(&repo, false, false, "unchanged alias in another file");
}

#[test]
fn ts_import_alias_keeps_unknown_uses_and_bad_calls_blocking() {
    for after in [
        OLD.to_owned(),
        OLD.replace("sum(100)", "sum(200)"),
        OLD.replace("sum(100)", "sum(...args)"),
        OLD.replace("sum(100)", "sum<number>(100, 10)"),
        OLD.replace("sum(100)", "sum?.(100, 10)"),
        format!("{NEW}const copy = sum;\n"),
        format!("{NEW}const box = {{sum}};\n"),
        format!("{NEW}export {{sum}};\n"),
        format!("{NEW}export default sum;\n"),
        format!("{NEW}type T = ReturnType<typeof sum>;\n"),
        format!("{NEW}sum.call(null, 100, 10);\n"),
        format!("{NEW}eval('sum(100)');\n"),
        format!("{NEW}namespace Other {{ function sum() {{}} sum(); }}\n"),
        NEW.replace("sum(100, 10)", "s\\u0075m(100, 10)"),
        NEW.replace("sum(100, 10)", "sum(100, 10"),
        "import { total as sum } from './pricing';\nconsole.log(100);\n".to_owned(),
        "import { total as sum } from './pricing';\nconst sum = () => 0;\nsum(100, 10);\n".to_owned(),
        "import { total as sum } from './pricing';\nimport { other as sum } from './other';\nsum(100, 10);\n".to_owned(),
        NEW.replace("import {", "import type {"),
        NEW.replace("{ total", "{ type total"),
    ] {
        let repo = fixture("ts", OLD, &after, &[]);
        assert_hook(&repo, false, false, &after);
    }
}

#[test]
fn ts_import_alias_requires_direct_unambiguous_plain_relative_resolution() {
    for (before, after, extras) in [
        (
            OLD.replace("'./pricing'", "'pricing'"),
            NEW.replace("'./pricing'", "'pricing'"),
            vec![],
        ),
        (
            OLD.replace("'./pricing'", "'./barrel'"),
            NEW.replace("'./pricing'", "'./barrel'"),
            vec![("barrel.ts", "export {total} from './pricing';")],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![("pricing.tsx", "export function total() { return 0; }")],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![("barrel.ts", "export { total as run } from './pricing';")],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![(
                "other.ts",
                "import { t\\u006ftal as second } from './pricing'; second(100);",
            )],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![(
                "other.ts",
                "import { total as s\\u0075m } from './pricing'; s\\u0075m(100);",
            )],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![(
                "tsconfig.json",
                r#"{"compilerOptions":{"rootDirs":[".","generated"]}}"#,
            )],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![(
                "tsconfig.json",
                r#"{"compilerOptions":{"moduleSuffixes":[".web",""]}}"#,
            )],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![("tsconfig.json", r#"{"extends":"./other.json"}"#)],
        ),
        (
            OLD.replace("{ total as sum }", "sum"),
            NEW.replace("{ total as sum }", "sum"),
            vec![],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![(
                "other.ts",
                "import * as api from './pricing'; api['total'](100);",
            )],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![(
                "other.ts",
                "const api = require('./pricing'); api['total'](100);",
            )],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![("other.ts", "const api = import('./pricing');")],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![("other.ts", "export * from './pricing';")],
        ),
        (
            OLD.to_owned(),
            NEW.to_owned(),
            vec![(
                "other.ts",
                "import * as api from 'unknown'; api['total'](100);",
            )],
        ),
    ] {
        let repo = fixture("ts", &before, &after, &extras);
        assert_hook(&repo, false, false, &format!("{after} extras={extras:?}"));
    }
}

#[test]
fn ts_import_alias_holds_sources_and_alias_sets_beyond_proof_limits() {
    let large = format!("{NEW}\n/*{}*/", "x".repeat(2 * 1024 * 1024));
    let repo = fixture("ts", OLD, &large, &[]);
    assert_hook(&repo, false, false, "oversized source");
    let imports: String = (0..65)
        .map(|i| format!("import {{ total as sum{i} }} from './pricing';\n"))
        .collect();
    let before = format!(
        "{imports}{}",
        (0..65)
            .map(|i| format!("sum{i}(100);\n"))
            .collect::<String>()
    );
    let after = format!(
        "{imports}{}",
        (0..65)
            .map(|i| format!("sum{i}(100, 10);\n"))
            .collect::<String>()
    );
    let repo = fixture("ts", &before, &after, &[]);
    assert_hook(&repo, false, false, "too many aliases");
}

#[test]
fn ts_import_alias_holds_require_import_equals_and_loader_aliases() {
    for source in [
        "import api = require('./pricing'); api['total'](100);",
        "const load = module.require; const api = load('./pricing'); api['total'](100);",
        "const load = module['require']; const api = load('./pricing'); api['total'](100);",
        "const api = module['require']('./pricing'); api['total'](100);",
        "const { require: load } = module; const api = load('./pricing'); api['total'](100);",
    ] {
        let repo = fixture("ts", OLD, NEW, &[("other.ts", source)]);
        assert_hook(&repo, false, false, source);
    }
}

#[test]
fn ts_import_alias_allows_independently_resolved_module_transports() {
    let repo = fixture(
        "ts",
        OLD,
        NEW,
        &[
            ("unrelated.ts", "export function value() { return 1; }"),
            (
                "other.ts",
                "import * as api from './unrelated'; console.log(api.value());",
            ),
            (
                "loader.ts",
                "const api = require('./unrelated'); console.log(api.value());",
            ),
            ("barrel.ts", "export * from './unrelated';"),
        ],
    );
    assert_hook(
        &repo,
        true,
        false,
        "module transports have independent origins",
    );
}
