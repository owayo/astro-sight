use super::support::{TestRepo, cargo_bin};

const OLD: &str = "package example\n\ndata class Gauge(val count: Int)\n";
const NEW: &str = "package example\n\ndata class Gauge(val count: Int, val bonus: Int = 0)\n";

fn repo_with(old: &str, new: &str, caller: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.write("Measure.kt", old);
    repo.write("Caller.kt", caller);
    repo.commit_all("initial");
    repo.write("Measure.kt", new);
    repo
}

fn hook(repo: &TestRepo, staged: bool) -> (std::process::Output, serde_json::Value) {
    let mut command = cargo_bin();
    command
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
    if staged {
        command.arg("--staged");
    }
    let output = command.output().unwrap();
    let json = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    (output, json)
}

#[test]
fn kotlin_data_class_trailing_defaults_preserve_source_calls() {
    for caller in [
        "package example\nfun sample(): Gauge = Gauge(2)\n",
        "package example\nfun sample(): Gauge = Gauge(count = 2)\n",
    ] {
        let repo = repo_with(OLD, NEW, caller);
        let review = repo.run_json("review", &["--git"]);
        assert_eq!(
            review["api_changes"]["compatible_modified"][0]["reason"],
            "kotlin_trailing_default_constructor_params",
            "{review}"
        );
        assert!(
            review["api_changes"]["modified"]
                .as_array()
                .unwrap()
                .is_empty(),
            "{review}"
        );
        for staged in [false, true] {
            if staged {
                repo.stage_all();
            }
            let (output, json) = hook(&repo, staged);
            assert!(output.status.success(), "{json}");
            assert!(json["impacts"].is_null(), "{json}");
            assert_eq!(json["api"]["mod_compat"][0]["n"], "Gauge", "{json}");
        }
    }
}

#[test]
fn kotlin_data_class_defaults_support_local_imports_and_existing_defaults() {
    for (old, new, caller) in [
        (
            OLD.to_owned(),
            NEW.to_owned(),
            "package consumer\nimport example.Gauge\nfun sample(): Gauge = Gauge(count = 2)\n",
        ),
        (
            OLD.replace("count: Int", "count: Int = 1"),
            NEW.replace("count: Int", "count: Int = 1"),
            "package example\nfun sample(): Gauge = Gauge()\n",
        ),
        (
            OLD.to_owned(),
            NEW.replace(
                "bonus: Int = 0",
                "bonus: Int = 0, var label: String? = null",
            ),
            "package example\nfun sample(): Gauge = Gauge(2)\n",
        ),
    ] {
        let repo = repo_with(&old, &new, caller);
        let (output, json) = hook(&repo, false);
        assert!(output.status.success(), "{json}");
        assert_eq!(json["api"]["mod_compat"][0]["n"], "Gauge", "{json}");
    }
}

fn assert_unverified(repo: &TestRepo) {
    let (output, json) = hook(repo, false);
    assert_eq!(output.status.code(), Some(1), "{json}");
    assert!(json["api"]["mod_compat"].is_null(), "{json}");
    assert!(
        json["api"]["mod"]
            .as_array()
            .is_some_and(|changes| changes.iter().any(|change| change["n"] == "Gauge"
                && change["unverified"]["reason"] == "compatibility_unverified")),
        "{json}"
    );
}

#[test]
fn kotlin_data_class_other_contract_changes_remain_unverified() {
    for (old, new) in [
        (OLD.to_owned(), NEW.replace("bonus: Int = 0", "bonus: Int")),
        (OLD.to_owned(), NEW.replace("count: Int", "count: Long")),
        (OLD.to_owned(), NEW.replace("count: Int, ", "")),
        ("package example\ndata class Gauge(val count: Int, val label: String)\n".to_owned(), "package example\ndata class Gauge(val label: String, val count: Int, val bonus: Int = 0)\n".to_owned()),
        (OLD.replace("count: Int", "count: Int = 2"), NEW.replace("count: Int", "count: Int = 3")),
        (OLD.replace("count: Int", "count: String = \"a b\""), NEW.replace("count: Int", "count: String = \"a  b\"")),
        (OLD.to_owned(), NEW.replace("bonus: Int = 0", "bonus: Int = unknown()")),
        (OLD.to_owned(), NEW.replace("bonus: Int = 0", "bonus: Int = \"wrong type\"")),
        (OLD.to_owned(), NEW.replace("bonus: Int = 0", "vararg val bonus: Int")),
        (OLD.to_owned(), NEW.replace("data class", "@Serializable data class")),
        (OLD.to_owned(), NEW.replace(")\n", ") { fun extra() = 1 }\n")),
        (OLD.to_owned(), NEW.replace(")\n", ") : Base()\n")),
        (OLD.to_owned(), NEW.replace("Gauge(", "Gauge<T>(")),
        (OLD.to_owned(), NEW.replace("data class", "expect data class")),
        (OLD.to_owned(), format!("{NEW}val unrelated = 1\n")),
    ] {
        let repo = repo_with(&old, &new, "package example\nfun sample(): Gauge = Gauge(2)\n");
        assert_unverified(&repo);
    }
}

#[test]
fn kotlin_data_class_ambiguous_calls_and_generated_members_remain_unverified() {
    for extra in [
        "val maker = ::Gauge\n",
        "val copier = Gauge(2)::copy\n",
        "val copier = Gauge::copy\n",
        "fun relay(g: Gauge) = g.copy()\n",
        "fun relay(g: Gauge) = g.component1()\n",
        "fun relay(g: Gauge) { val (a, b) = g }\n",
        "fun Gauge.copy(bonus: Int): String = \"\"\n",
        "operator fun Gauge.component2(): String = \"\"\n",
        "val Gauge.bonus: String get() = \"\"\n",
        "fun Gauge(count: Int, tag: String = \"\"): String = \"\"\n",
        "typealias Alias = Gauge\nval another = Alias(2)\n",
        "fun extra(): Gauge = Gauge(2, 3)\n",
        "fun extra(): Gauge = Gauge()\n",
        "fun extra(): Gauge = Gauge(count = 2, bonus = 3)\n",
        "fun extra(): Gauge = Gauge(*intArrayOf(2))\n",
        "fun extra(): Gauge = Gauge(2) { println(1) }\n",
    ] {
        let caller = format!("package example\nfun sample(): Gauge = Gauge(2)\n{extra}");
        let repo = repo_with(OLD, NEW, &caller);
        assert_unverified(&repo);
    }
    for (old, new, caller) in [
        (
            "package example\ndata class Gauge(val block: () -> Unit)\n",
            "package example\ndata class Gauge(val block: () -> Unit, val bonus: Int = 0)\n",
            "package example\nfun sample(): Gauge = Gauge {}\n",
        ),
        (
            "package example\ndata class Gauge(val count: Int) { constructor(value: Int, tag: String = \"\") : this(count = value) }\n",
            "package example\ndata class Gauge(val count: Int, val bonus: Int = 0) { constructor(value: Int, tag: String = \"\") : this(count = value) }\n",
            "package example\nfun sample(): Gauge = Gauge(2)\n",
        ),
        (
            "package example\ndata class Gauge(val count: Int)\n",
            "package example\ndata class Gauge(val count: Int, val bonus: Int = 0)\n",
            "package example\nclass Int\nfun sample(): Gauge = Gauge(Int())\n",
        ),
    ] {
        let repo = repo_with(old, new, caller);
        assert_unverified(&repo);
    }
}

#[test]
fn kotlin_data_class_incomplete_or_external_sources_remain_unverified() {
    for (file, extra) in [
        (
            "Caller.java",
            "package example; class Caller { Object sample() { return new Gauge(2); } }\n",
        ),
        ("Broken.kt", "package example\nfun broken( =\n"),
        (
            "Generated.kt",
            "// This file is @generated\npackage example\nval copier = Gauge(2)::copy\n",
        ),
        ("Widget.vue", "<script>const item = Gauge(2);</script>\n"),
        (
            "build.gradle",
            "dependencies { implementation 'example:factory:1' }\n",
        ),
        (
            "build.gradle.kts",
            "dependencies { implementation(\"example:factory:1\") }\n",
        ),
        ("pom.xml", "<project/>\n"),
    ] {
        let repo = repo_with(
            OLD,
            NEW,
            "package example\nfun sample(): Gauge = Gauge(2)\n",
        );
        repo.write(file, extra);
        assert_unverified(&repo);
    }
    for caller in [
        "package consumer\nimport example.Gauge as Alias\nfun sample(): Alias = Alias(2)\n",
        "package consumer\nimport example.*\nfun sample(): Gauge = Gauge(2)\n",
        "package example\nimport external.helpers.*\nfun sample(): Gauge = Gauge(2)\n",
    ] {
        let repo = repo_with(OLD, NEW, caller);
        assert_unverified(&repo);
    }
    let repo = repo_with(
        OLD,
        NEW,
        "package example\nfun sample(): Gauge = Gauge(2)\n",
    );
    for index in 0..512 {
        repo.write(format!("Empty{index}.kt"), "");
    }
    assert_unverified(&repo);

    let repo = repo_with(
        OLD,
        NEW,
        "package example\nfun sample(): Gauge = Gauge(2)\n",
    );
    repo.write(".gitignore", "build.gradle.kts\n");
    repo.write(
        "build.gradle.kts",
        "dependencies { implementation(\"example:factory:1\") }\n",
    );
    assert_unverified(&repo);

    let repo = repo_with(
        OLD,
        NEW,
        "package example\nfun sample(): Gauge = Gauge(2)\n",
    );
    repo.write("Large.kt", format!("//{}\n", "x".repeat(2 * 1024 * 1024)));
    assert_unverified(&repo);
}

#[test]
fn kotlin_data_class_staged_proofs_use_the_index_sources() {
    let caller = "package example\nfun sample(): Gauge = Gauge(2)\n";
    let required = NEW.replace("bonus: Int = 0", "bonus: Int");
    let repo = repo_with(OLD, NEW, caller);
    repo.stage_all();
    repo.write("Measure.kt", &required);
    let (output, json) = hook(&repo, true);
    assert!(output.status.success(), "{json}");
    assert_eq!(json["api"]["mod_compat"][0]["n"], "Gauge", "{json}");

    let repo = repo_with(OLD, &required, caller);
    repo.stage_all();
    repo.write("Measure.kt", NEW);
    let (output, json) = hook(&repo, true);
    assert_eq!(output.status.code(), Some(1), "{json}");
    assert!(json["api"]["mod_compat"].is_null(), "{json}");
}
