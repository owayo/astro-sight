use super::support::{TestRepo, cargo_bin};

const OLD: &str = "interface CleanupScope { release(action: () => void): void; }\ninterface PanelContract { activate(scope: CleanupScope): void; }\nfunction installPanel(spec: PanelContract): PanelContract { return spec; }\nexport default installPanel({\n  activate() { console.log('ready'); },\n});\n";
const NEW: &str = "interface CleanupScope { release(action: () => void): void; }\ninterface PanelContract { activate(scope: CleanupScope): void; }\nfunction installPanel(spec: PanelContract): PanelContract { return spec; }\nexport default installPanel({\n  activate(scope) { scope.release(() => console.log('closed')); },\n});\n";
const CALLER: &str =
    "import panel from './panel';\npanel.activate({ release(action) { void action; } });\n";

fn fixture(extension: &str, old: &str, new: &str, caller: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write(format!("panel.{extension}"), old);
    repo.write(format!("runtime.{extension}"), caller);
    repo.commit_all("initial");
    repo.write(format!("panel.{extension}"), new);
    repo
}

fn hook(repo: &TestRepo) -> (i32, serde_json::Value) {
    let output = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    let json = serde_json::from_slice(&output.stderr).unwrap();
    (output.status.code().unwrap(), json)
}

#[test]
fn fixed_callback_contract_keeps_implementation_parameter_addition_informational() {
    for extension in ["ts", "tsx"] {
        let repo = fixture(extension, OLD, NEW, CALLER);
        repo.write("other.ts", "const other = 1; void other;\n");
        let (code, json) = hook(&repo);
        assert_eq!(code, 0, "{extension}: {json}");
        assert_eq!(json["api"]["mod_compat"][0]["n"], "activate", "{json}");
        assert_eq!(
            json["api"]["mod_compat"][0]["reason"], "fixed_callback_contract",
            "{json}"
        );
        let context = repo.run_json("context", &["--git"]);
        let panel = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|change| change["path"] == format!("panel.{extension}"))
            .unwrap();
        assert!(
            panel["impacted_callers"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|caller| caller["path"] == format!("runtime.{extension}")),
            "{context}"
        );
        let impact = cargo_bin()
            .args(["impact", "--git", "--dir"])
            .arg(repo.root())
            .output()
            .unwrap();
        assert!(
            impact.status.success(),
            "{}",
            String::from_utf8_lossy(&impact.stderr)
        );
    }
}

#[test]
fn fixed_callback_contract_keeps_changed_or_unproven_contracts_blocking() {
    let cases = [
        (
            OLD.to_string(),
            NEW.to_string(),
            "other.activate({ release(action) { void action; } });\n".to_string(),
        ),
        (
            OLD.to_string(),
            NEW.replace("spec: PanelContract", "spec: unknown"),
            CALLER.to_string(),
        ),
        (
            OLD.to_string(),
            NEW.replace("): PanelContract {", "): unknown {"),
            CALLER.to_string(),
        ),
        (
            OLD.to_string(),
            NEW.to_string(),
            "import panel from './panel';\npanel.activate();\n".to_string(),
        ),
        (
            OLD.to_string(),
            NEW.replace("activate(scope)", "activate(scope, extra)"),
            CALLER.to_string(),
        ),
        (
            OLD.replace(
                "function installPanel(spec: PanelContract): PanelContract { return spec; }",
                "function installPanel<T>(spec: T): T { return spec; }",
            ),
            NEW.replace(
                "function installPanel(spec: PanelContract): PanelContract { return spec; }",
                "function installPanel<T>(spec: T): T { return spec; }",
            ),
            CALLER.to_string(),
        ),
        (
            OLD.to_string(),
            NEW.to_string(),
            "import panel from './panel';\nconst callback = panel.activate; callback();\n"
                .to_string(),
        ),
        (
            OLD.to_string(),
            NEW.to_string(),
            "import panel from './panel';\npanel.activate(...args);\n".to_string(),
        ),
        (
            OLD.to_string(),
            NEW.replace("activate(scope)", "async activate(scope)"),
            CALLER.to_string(),
        ),
        (
            OLD.to_string(),
            NEW.replace("activate(scope)", "activate(scope = fallback)"),
            CALLER.to_string(),
        ),
        (
            OLD.to_string(),
            NEW.replace("activate(scope)", "activate({ release })"),
            CALLER.to_string(),
        ),
        (
            OLD.replace("return spec;", "return transform(spec);"),
            NEW.replace("return spec;", "return transform(spec);"),
            CALLER.to_string(),
        ),
        (
            format!("{OLD}\ninstallPanel = alternate;\n"),
            format!("{NEW}\ninstallPanel = alternate;\n"),
            CALLER.to_string(),
        ),
    ];
    for (old, new, caller) in cases {
        let repo = fixture("ts", &old, &new, &caller);
        let (code, json) = hook(&repo);
        assert_eq!(code, 1, "{new}\n{caller}\n{json}");
        assert!(json["api"]["mod_compat"].is_null(), "{json}");
        assert!(
            json["api"]["mod"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|symbol| symbol["n"] == "activate"),
            "{json}"
        );
    }
}

#[test]
fn fixed_callback_contract_rejects_incomplete_or_dynamic_inventory() {
    for (path, extra) in [
        (
            "escape.ts",
            "import panel from './panel'; framework.use(panel);",
        ),
        ("barrel.ts", "export { default as panel } from './panel';"),
        ("dynamic.ts", "import('./panel').then(framework.use);"),
        (
            "dynamic.ts",
            "const panel = require('./panel').default; framework.use(panel);",
        ),
        (
            "dynamic.ts",
            "import panel from './panel'; spyOn(panel, 'activate');",
        ),
        (
            "dynamic.ts",
            "import panel from './panel'; panel['activate']();",
        ),
        (
            "dynamic.ts",
            "import panel from './panel'; Reflect.get(panel, 'activate')();",
        ),
        ("broken.ts", "const broken = ;"),
        (
            "view.vue",
            "<script>import panel from './panel'; panel.activate();</script>",
        ),
        (
            "ambient.ts",
            "declare module './panel' { interface PanelContract {} }",
        ),
    ] {
        let repo = fixture("ts", OLD, NEW, CALLER);
        repo.write(path, extra);
        let (code, json) = hook(&repo);
        assert_eq!(code, 1, "{path}: {extra}: {json}");
        assert!(json["api"]["mod_compat"].is_null(), "{json}");
        assert!(
            json["api"]["mod"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|symbol| symbol["n"] == "activate"),
            "{json}"
        );
    }
}

#[test]
fn fixed_callback_contract_handles_scoped_paths_esm_imports_and_byte_columns() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src/実行");
    repo.write("src/panel.ts", OLD);
    repo.write("src/実行/runtime.ts", "import panel from '../panel.js';\nconst label = '日本語'; panel.activate({ release(action) { void action; } }); void label;\n");
    repo.commit_all("initial");
    repo.write("src/panel.ts", NEW);
    for dir in [repo.root().to_path_buf(), repo.path("src")] {
        let output = cargo_bin()
            .args(["review", "--git", "--hook", "--dir"])
            .arg(&dir)
            .output()
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(output.status.success(), "{}: {json}", dir.display());
        assert_eq!(
            json["api"]["mod_compat"][0]["reason"], "fixed_callback_contract",
            "{json}"
        );
    }
}
