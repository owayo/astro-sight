use super::support::{TestRepo, cargo_bin};
use serde_json::Value;

const SOURCE: &str = include_str!("../fixtures/kotlin_when_return.kt");

fn json(repo: &TestRepo, command: &str, args: &[&str]) -> Value {
    let output = cargo_bin()
        .arg(command)
        .arg("--path")
        .arg(repo.path("when.kt"))
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn semantic(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            for key in ["path", "ln", "line", "col", "range", "ctx", "diagnostics"] {
                map.remove(key);
            }
            for item in map.values_mut() {
                *item = semantic(item.take());
            }
        }
        Value::Array(items) => {
            for item in items {
                *item = semantic(item.take());
            }
        }
        _ => {}
    }
    value
}

#[test]
fn kotlin_when_return_cli_matches_braced_symbols_refs_calls_and_imports() {
    let bare = TestRepo::new();
    let braced = TestRepo::new();
    bare.write("when.kt", SOURCE);
    let control = SOURCE
        .lines()
        .map(|line| {
            if let Some((condition, body)) = line.split_once("-> ") {
                format!("{condition}-> {{ {body} }}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    braced.write("when.kt", control);
    for command in ["symbols", "calls", "imports"] {
        let args: &[&str] = if command == "symbols" {
            &["--no-cache"]
        } else {
            &[]
        };
        let actual = json(&bare, command, args);
        assert!(
            actual["diagnostics"].as_array().is_none_or(Vec::is_empty),
            "{actual}"
        );
        assert_eq!(
            semantic(actual),
            semantic(json(&braced, command, args)),
            "{command}"
        );
    }
    for name in ["offset", "skipName", "and"] {
        let a = bare.run_json("refs", &["--name", name]);
        let b = braced.run_json("refs", &["--name", name]);
        assert_eq!(semantic(a), semantic(b), "{name}");
    }
    let ast = json(&bare, "ast", &["--full", "--depth", "32", "--no-cache"]);
    assert!(
        ast["diagnostics"].as_array().is_none_or(Vec::is_empty),
        "{ast}"
    );
    let serialized = serde_json::to_string(&ast).unwrap();
    assert_eq!(serialized.matches("\"kind\":\"when_entry\"").count(), 3);
    assert_eq!(
        serialized.matches("\"kind\":\"jump_expression\"").count(),
        3
    );
    assert!(!serialized.contains("\"kind\":\"call_expression\""));
    bare.write("when.kt", SOURCE.replace("else ->", "else"));
    let malformed = json(&bare, "symbols", &["--no-cache"]);
    assert!(
        !malformed["diagnostics"].as_array().unwrap().is_empty(),
        "{malformed}"
    );
}

#[test]
fn kotlin_member_impact_requires_the_declaring_type_in_the_reference_file() {
    for (consumer, actual) in [
        (
            "import java.util.Properties\n\nval settings = Properties()\nval value = settings.run { getProperty(\"mode\") }\n",
            false,
        ),
        (
            "import example.SettingShelf\n\nval settings = SettingShelf()\nval value = settings.run { getProperty(\"mode\") }\n",
            true,
        ),
        (
            "import example.SettingShelf\n\nval settings = SettingShelf()\nval value = settings.getProperty(\"mode\")\n",
            true,
        ),
    ] {
        let repo = TestRepo::new();
        repo.write(
            "reader.kt",
            "package example\nclass SettingShelf {\n    fun getProperty(key: String): String = key\n}\n",
        );
        repo.write("setup.gradle.kts", consumer);
        repo.init_git();
        repo.commit_all("initial");
        repo.write(
            "reader.kt",
            "package example\nclass SettingShelf {\n    fun getProperty(key: String): String = key.uppercase()\n}\n",
        );
        let review = repo.run_json("review", &["--git"]);
        let has_member_caller = review["impact"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|change| change["impacted_callers"].as_array())
            .flatten()
            .any(|caller| {
                caller["path"] == "setup.gradle.kts"
                    && caller["symbols"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|symbol| symbol == "getProperty")
            });
        assert_eq!(has_member_caller, actual, "{consumer}: {review}");

        let refs = repo.run_json("refs", &["--name", "getProperty"]);
        assert!(
            refs["refs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|reference| reference["path"] == "setup.gradle.kts"),
            "{consumer}: {refs}"
        );
    }
}
