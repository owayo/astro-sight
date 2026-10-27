//! 省略可能な Python 引数の削除を、変更後の直接呼び出しで閉じる。

use super::support::*;

const BEFORE: &str = "def convert(data: bytes, *, limit: int = 10, hook=None) -> str:\n    if hook is not None:\n        hook(data)\n    return data[:limit].decode()\n";
const AFTER: &str =
    "def convert(data: bytes, *, limit: int = 10) -> str:\n    return data[:limit].decode()\n";

fn fixture(before: &str, usage: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("pkg");
    repo.write("pkg/__init__.py", "");
    repo.write("pkg/conv.py", before);
    repo.write("pkg/use.py", usage);
    repo.commit_all("initial");
    repo
}

fn hook(repo: &TestRepo, staged: bool) -> (std::process::Output, serde_json::Value) {
    let mut cmd = cargo_bin();
    cmd.args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root());
    if staged {
        cmd.arg("--staged");
    }
    let output = cmd.output().unwrap();
    let json = serde_json::from_slice(&output.stderr)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    (output, json)
}

#[test]
fn python_optional_removal_closes_changed_and_unchanged_calls() {
    for staged in [false, true] {
        for changed_caller in [false, true] {
            let old_usage = format!(
                "from pkg.conv import convert\n\ndef first(data: bytes) -> str:\n    return convert(data, limit=5{})\n\ndef plain(data: bytes) -> str:\n    return convert(data)\n",
                if changed_caller { ", hook=print" } else { "" }
            );
            let repo = fixture(BEFORE, &old_usage);
            repo.write("pkg/conv.py", AFTER);
            if changed_caller {
                repo.write("pkg/use.py", old_usage.replace(", hook=print", ""));
            }
            if staged {
                repo.stage_all();
            }
            let args = if staged {
                &["--git", "--staged"][..]
            } else {
                &["--git"][..]
            };
            let review = repo.run_json("review", args);
            assert_eq!(
                review["api_changes"]["modified_closed_in_diff"][0]["name"], "convert",
                "{review}"
            );
            assert!(
                review["api_changes"]["modified"]
                    .as_array()
                    .unwrap()
                    .is_empty(),
                "{review}"
            );
            let (output, json) = hook(&repo, staged);
            assert!(
                output.status.success(),
                "staged={staged}, changed={changed_caller}: {json}"
            );
            assert_eq!(json["api"]["mod_closed"][0]["n"], "convert", "{json}");
            assert!(json["impacts"].is_null(), "{json}");
            assert!(
                json["blocking_categories"].as_array().unwrap().is_empty(),
                "{json}"
            );
            let refs = repo.run_json("refs", &["--name", "convert"]);
            assert!(refs["refs"].as_array().unwrap().len() >= 4, "{refs}");
        }
    }
}

#[test]
fn python_optional_removal_rejects_invalid_and_indirect_uses() {
    for usage in [
        "convert(b'x', hook=print)",
        "convert(b'x', **options)",
        "convert(*values)",
        "convert()",
        "convert(b'x', 5)",
        "convert(b'x', unknown=True)",
        "alias = convert\nalias(b'x')",
        "consume(convert)",
        "registry = {'convert': convert}",
        "convert.__call__(b'x')",
        "obj.convert(b'x')",
        "def nested(convert):\n    return convert(b'x')",
    ] {
        let repo = fixture(BEFORE, &format!("from pkg.conv import convert\n{usage}\n"));
        repo.write("pkg/conv.py", AFTER);
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{usage}: {json}");
        assert!(json["api"]["mod_closed"].is_null(), "{usage}: {json}");
    }
}

#[test]
fn python_optional_removal_rejects_hidden_transport_and_incomplete_sources() {
    for extra in [
        "from pkg.conv import convert as renamed\nrenamed(b'x', hook=print)\n",
        "from pkg.conv import *\n",
        "import pkg.conv as module\nconsume(module)\n",
        "from pkg import conv as module\nconsume(module)\n",
        "from .conv import convert as renamed\n",
        "from pkg.use import convert as renamed\n",
        "eval('convert(data, hook=print)')\n",
        "execute = exec\n",
        "getattr(module, 'convert')(b'x', hook=print)\n",
        "globals()['convert'](b'x')\n",
        "def broken(:\n",
        "ｃｏｎｖｅｒｔ(b'x', hook=print)\n",
    ] {
        let repo = fixture(BEFORE, "from pkg.conv import convert\nconvert(b'x')\n");
        repo.write("pkg/extra.py", extra);
        repo.commit_all("transport");
        repo.write("pkg/conv.py", AFTER);
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{extra}: {json}");
        assert!(json["api"]["mod_closed"].is_null(), "{extra}: {json}");
    }
}

#[test]
fn python_optional_removal_staged_uses_index_callers() {
    for index_valid in [false, true] {
        let repo = fixture(
            BEFORE,
            "from pkg.conv import convert\nconvert(b'x', hook=print)\n",
        );
        repo.write("pkg/conv.py", AFTER);
        if index_valid {
            repo.write(
                "pkg/use.py",
                "from pkg.conv import convert\nconvert(b'x')\n",
            );
        }
        repo.stage_all();
        repo.write(
            "pkg/use.py",
            if index_valid {
                "from pkg.conv import convert\nconvert(b'x', hook=print)\n"
            } else {
                "from pkg.conv import convert\nconvert(b'x')\n"
            },
        );
        let (output, json) = hook(&repo, true);
        assert_eq!(output.status.success(), index_valid, "{json}");
        assert_eq!(json["api"]["mod_closed"].is_array(), index_valid, "{json}");
    }
}

#[test]
fn python_optional_removal_preserves_surviving_contract_and_parameter_modes() {
    for (before, after, closes) in [
        ("data, *, hook=None", "data", true),
        ("data, *, hook=None, limit=10", "data, *, limit=10", true),
        ("data, hook=None", "data", true),
        ("data, *, hook", "data", false),
        ("data, *, hook=None, limit=10", "data, *, limit=20", false),
        ("data: bytes, *, hook=None", "data: str", false),
        ("data, *, limit=10, hook=None", "data, limit=10", false),
        ("data, hook=None, limit=10", "data, limit=10", false),
        ("data, *, hook=None, **kwargs", "data, **kwargs", false),
        ("data, *args, hook=None", "data, *args", false),
    ] {
        let repo = fixture(
            &format!("def convert({before}) -> str:\n    return str(data)\n"),
            "from pkg.conv import convert\nconvert(b'x')\n",
        );
        repo.write(
            "pkg/conv.py",
            format!("def convert({after}) -> str:\n    return str(data)\n"),
        );
        let (output, json) = hook(&repo, false);
        assert_eq!(
            output.status.success(),
            closes,
            "{before} -> {after}: {json}"
        );
        assert_eq!(
            json["api"]["mod_closed"].is_array(),
            closes,
            "{before} -> {after}: {json}"
        );
    }
}

#[test]
fn python_optional_removal_handles_src_layout_multiline_calls_and_crlf() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src/pkg");
    repo.write("src/pkg/__init__.py", "");
    repo.write("src/pkg/conv.py", BEFORE.replace('\n', "\r\n"));
    repo.write("src/pkg/use.py", "# 日本語の位置を保持\r\nfrom pkg.conv import convert\r\nconvert(\r\n    b'x',\r\n    limit=5,\r\n)\r\n");
    repo.commit_all("initial");
    repo.write("src/pkg/conv.py", AFTER.replace('\n', "\r\n"));
    let (output, json) = hook(&repo, false);
    assert!(output.status.success(), "{json}");
    assert_eq!(
        json["api"]["mod_closed"][0]["f"], "src/pkg/conv.py",
        "{json}"
    );
}

#[test]
fn python_optional_removal_keeps_ambiguous_declarations_and_missing_calls_blocking() {
    for declaration in [
        format!("@decorator\n{BEFORE}"),
        format!("{BEFORE}\n{BEFORE}"),
        format!("{BEFORE}\nconvert = replacement\n"),
    ] {
        let repo = fixture(
            &declaration,
            "from pkg.conv import convert\nconvert(b'x')\n",
        );
        repo.write("pkg/conv.py", declaration.replace(BEFORE, AFTER));
        let (output, json) = hook(&repo, false);
        assert_eq!(output.status.code(), Some(1), "{declaration}: {json}");
        assert!(json["api"]["mod_closed"].is_null(), "{json}");
    }
    let repo = fixture(BEFORE, "from pkg.conv import convert\n");
    repo.write("pkg/conv.py", AFTER);
    let (output, json) = hook(&repo, false);
    assert_eq!(output.status.code(), Some(1), "{json}");
    assert!(json["api"]["mod_closed"].is_null(), "{json}");
}
