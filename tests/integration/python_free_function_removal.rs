//! Python の自由関数を消したとき、別モジュールに残る同名の定義と呼び出しを区別する。

use super::support::*;

const REMOVED: &str = "import sys\n\n\ndef main():\n    print(\"tally\")\n    return 0\n\n\nif __name__ == \"__main__\":\n    sys.exit(main())\n";

/// `legacy/tally.py` をコミットしてから作業ツリーで消し、`review --git --hook` を流す。
fn remove_tally(extras: &[(&str, String)]) -> std::process::Output {
    let mut files = vec![("legacy/tally.py", REMOVED)];
    files.extend(extras.iter().map(|(path, source)| (*path, source.as_str())));
    a3_review_hook(
        |root| std::fs::remove_file(root.join("legacy/tally.py")).unwrap(),
        &files,
    )
}

#[test]
fn python_free_function_removal_reads_literal_getattr_like_attribute_access() {
    let hello = "import os\n\n\ndef main():\n    uid = getattr(os, \"getuid\", None)\n    print(\"uid:\", uid() if uid else \"-\")\n\n\nmain()\n";
    let one = "import os\n\n\ndef main():\n    return getattr(os, \"sep\", \"/\")\n";
    let two = "def main():\n    return 2\n";
    let cases = [
        (vec![("tools/hello.py", hello.to_owned())], false),
        (
            vec![
                ("tools/one.py", one.to_owned()),
                ("tools/two.py", two.to_owned()),
            ],
            false,
        ),
        // 名前が変数の getattr と、値として束縛した getattr は、今のとおり証明をやめる。
        (
            vec![("tools/hello.py", hello.replace("\"getuid\"", "NAME"))],
            true,
        ),
        (
            vec![(
                "tools/hello.py",
                hello.replace(
                    "uid = getattr(os, \"getuid\", None)",
                    "get = getattr\n    uid = get(os, \"getuid\", None)",
                ),
            )],
            true,
        ),
    ];
    let removed = serde_json::json!([{"f": "legacy/tally.py", "n": "main"}]);
    for (extras, should_block) in cases {
        let output = remove_tally(&extras);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(i32::from(should_block)),
            "{extras:?}\n{stderr}"
        );
        let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        let (kept, dropped) = if should_block {
            ("rm", "rm_dead")
        } else {
            ("rm_dead", "rm")
        };
        assert_eq!(hook["api"][kept], removed, "{extras:?}\n{stderr}");
        assert!(hook["api"].get(dropped).is_none(), "{extras:?}\n{stderr}");
    }
}

const UTIL_OLD: &str = "def total(x):\n    return x\n\n\ndef keep():\n    return 1\n";
const UTIL_NEW: &str = "def keep():\n    return 1\n";

#[test]
fn removal_preserves_reexported_and_rebound_module_receivers() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("tools");
    repo.write("tools/__init__.py", "");
    repo.write("tools/util.py", UTIL_OLD);
    repo.write("tools/other.py", UTIL_OLD);
    repo.write("bridge.py", "from tools import util as u\n");
    repo.create_dir_all("reexport");
    repo.write("reexport/__init__.py", "from tools import util as u\n");
    repo.write("reexport/u.py", UTIL_OLD);
    repo.write("report.py", "");
    repo.commit_all("base");
    repo.write("tools/util.py", UTIL_NEW);

    let control = "from tools import other as u\ndef run(): return u.total(1)\n";
    for (binding, receiver) in [
        (
            "from bridge import u\ndef other():\n    import spare.u as u\n    return u",
            "u",
        ),
        ("import bridge as b", "b.u"),
        (
            "import tools as b\nimport tools.util as target\nb.other = target",
            "b.other",
        ),
        ("from reexport import u", "u"),
        (
            "import math as u\nimport importlib\nglobals()['u'] = importlib.import_module('tools.util')",
            "u",
        ),
        (
            "import tools.util as ｕ\ndef other():\n    import math as u\n    return u.pi",
            "u",
        ),
    ] {
        repo.write(
            "report.py",
            format!("{binding}\ndef run(): return {receiver}.total(1)\n"),
        );
        repo.stage_all();
        repo.write("report.py", control);
        for (flags, bucket) in [
            (vec!["--git", "--staged"], "removed"),
            (vec!["--git"], "removed_dead"),
        ] {
            let review = repo.run_json("review", &flags);
            assert_eq!(
                review["api_changes"][bucket][0]["name"], "total",
                "{binding}: {review}"
            );
        }
    }
    let refs = repo.run_json("refs", &["--name", "total"]);
    assert!(
        refs["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == "report.py" && r["ln"] == 1),
        "{refs}"
    );
}

#[test]
fn removal_resolves_import_aliases_at_the_attribute_use() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("tools");
    repo.write("tools/util.py", UTIL_OLD);
    repo.write("tools/other.py", "def total(value): return value\n");
    let caller = "import tools.util as u\n\ndef run():\n    return u.total(1)\n\ndef other():\n    import math as u\n    return u.pi\n";
    repo.write("report.py", caller);
    repo.commit_all("base");
    repo.write("tools/util.py", UTIL_NEW);
    repo.stage_all();
    repo.write(
        "report.py",
        caller.replace(
            "def run():\n",
            "def run():\n    from tools import other as u\n",
        ),
    );
    for (staged, blocking) in [(true, true), (false, false)] {
        let mut args = vec!["--git"];
        if staged {
            args.push("--staged");
        }
        let review = repo.run_json("review", &args);
        let bucket = if blocking { "removed" } else { "removed_dead" };
        assert_eq!(
            review["api_changes"][bucket][0]["name"], "total",
            "{review}"
        );
        let hook = cargo_bin()
            .args(["review", "--hook", "--dir"])
            .arg(repo.root())
            .args(&args)
            .output()
            .unwrap();
        let result: serde_json::Value = serde_json::from_slice(&hook.stderr).unwrap();
        assert_eq!(hook.status.code(), Some(i32::from(blocking)), "{result}");
        let bucket = if blocking { "rm" } else { "rm_dead" };
        assert_eq!(result["api"][bucket][0]["n"], "total", "{result}");
    }
    let refs = repo.run_json("refs", &["--name", "total"]);
    assert!(
        refs["refs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == "report.py" && r["ln"] == 4),
        "{refs}"
    );
}

/// 削除元と同じモジュールの、引数とローカルの代入で同じ名前を隠す関数 (変更の前後とも置く)。
const SHADOWS: &str = "\n\ndef use_param(total):\n    return total\n\n\ndef use_local():\n    total = 2\n    return total\n";

/// `tools/util.py` の `total` を消して (変更後の内容を `util_new` にして) `review --git --hook` を
/// 流し、`api.rm` に残るか (`rm_dead` に入るか) と終了コードを確かめる。ほかのファイルは
/// `extras` の内容でコミットしておき、変えない。
fn assert_total_removal(util_new: &str, extras: &[(&str, &str)], should_block: bool) {
    assert_total_removal_from(UTIL_OLD, util_new, extras, should_block);
}

fn assert_total_removal_from(
    util_old: &str,
    util_new: &str,
    extras: &[(&str, &str)],
    should_block: bool,
) {
    let mut files = vec![("tools/util.py", util_old)];
    files.extend_from_slice(extras);
    let output = a3_review_hook(
        |root| std::fs::write(root.join("tools/util.py"), util_new).unwrap(),
        &files,
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let context = format!("{util_new}\n{extras:?}\n{stderr}");
    assert_eq!(
        output.status.code(),
        Some(i32::from(should_block)),
        "{context}"
    );
    let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    let (kept, dropped) = if should_block {
        ("rm", "rm_dead")
    } else {
        ("rm_dead", "rm")
    };
    let removed = serde_json::json!([{"f": "tools/util.py", "n": "total"}]);
    assert_eq!(hook["api"][kept], removed, "{context}");
    assert!(hook["api"].get(dropped).is_none(), "{context}");
}

/// 関数の中で `global`・`nonlocal` の宣言なしに束縛した同じ名前は、消したモジュール関数を
/// 指さない (Issue #471)。
#[test]
fn python_free_function_removal_ignores_function_local_bindings() {
    for (util_new, report) in [
        // Issue の再現と、名前を変えた対照。
        (UTIL_NEW, "def run():\n    total = 3\n    return total\n"),
        (UTIL_NEW, "def run():\n    count = 3\n    return count\n"),
        // 引数・`for`・`with`・`except` の束縛 (それぞれ単独で) と、束縛し直さない内側の関数と、
        // async・ジェネレータ。
        (UTIL_NEW, "def run(total):\n    return total\n"),
        (
            UTIL_NEW,
            "def run():\n    for total in range(3):\n        pass\n    return total\n",
        ),
        (
            UTIL_NEW,
            "def run():\n    with open(__file__) as total:\n        return total\n",
        ),
        (
            UTIL_NEW,
            "def run():\n    try:\n        pass\n    except Exception as total:\n        return total\n",
        ),
        (
            UTIL_NEW,
            "def run(total):\n    def inner():\n        return total\n    return inner()\n",
        ),
        (
            UTIL_NEW,
            "async def run():\n    total = 3\n    return total\n\n\ndef gen():\n    total = 3\n    yield total\n",
        ),
        // 同じ名前の裸の引数を持つ別々の関数 (引数の宣言の位置も参照に数えない)。
        (
            UTIL_NEW,
            "def one(total):\n    return total\n\n\ndef two(total):\n    return total + 1\n",
        ),
        // 名前を識別子として含まない文字列 (部分一致) は、証明を止めない。
        (
            UTIL_NEW,
            "from typing import Literal\n\n\ndef run(total: Literal[\"subtotal\"]):\n    return total\n",
        ),
    ] {
        assert_total_removal(util_new, &[("tools/report.py", report)], false);
    }
    // 削除元と同じモジュールの別の関数の引数とローカルの代入 (`tools/report.py` は無関係)。
    assert_total_removal_from(
        &format!("{UTIL_OLD}{SHADOWS}"),
        &format!("{UTIL_NEW}{SHADOWS}"),
        &[("tools/report.py", "def run():\n    return 1\n")],
        false,
    );
}

/// 関数のローカル束縛でない同じ名前、宣言のある名前、モジュールの関数に届く経路は、今のとおり
/// 残る参照に数える (Issue #471)。
#[test]
fn python_free_function_removal_keeps_references_that_reach_the_module_function() {
    let local = "def run():\n    total = 3\n    return total\n";
    for (util_new, report) in [
        // 削除元と同じモジュールに、束縛されていない呼び出しが残る。
        ("def keep():\n    return total(1)\n", local),
        // 別モジュールの import (呼ばないものを含む) と、別名のモジュールの属性アクセス。
        (UTIL_NEW, "from tools.util import total as t\n"),
        (
            UTIL_NEW,
            "import tools.util as u\n\n\ndef run():\n    return u.total(1)\n",
        ),
        // `global` の宣言と、束縛の無い関数の呼び出しが同じファイルにある。
        (
            UTIL_NEW,
            "def run():\n    global total\n    total = 3\n    return total\n",
        ),
        // 右辺の無い括弧付きの注釈は束縛しない (Python では `(total): int` は名前を束縛しない)。
        (
            "def keep():\n    (total): int\n    return total(1)\n",
            local,
        ),
        // 既定値・デコレータ・引数の注釈は関数の外で評価される (同じ行の引数とも区別する)。
        ("def keep(total=total(1)):\n    return total\n", local),
        (
            "@total\ndef keep():\n    total = 1\n    return total\n",
            local,
        ),
        (
            "def keep(x: total(1)):\n    total = 1\n    return total\n",
            local,
        ),
        // 内側の関数の `global` と、関数の中のクラス本体 (先に読む名前はモジュールへ届く)。
        (
            "def keep(total):\n    def inner():\n        global total\n        return total(1)\n    return inner()\n",
            local,
        ),
        (
            "def keep(total):\n    class Holder:\n        saved = total(1)\n        total = 0\n    return Holder\n",
            local,
        ),
        // 文字列の名前で引く経路と、評価されうる文字列の注釈と、f-string の補間の中の呼び出し。
        (
            "def keep():\n    total = globals()[\"total\"]\n    return total(1)\n",
            local,
        ),
        ("def keep(total: \"total(1)\"):\n    return total\n", local),
        ("def keep():\n    return f\"{total(1)}\"\n", local),
        // 関数の globals の辞書を計算した名前で引く (文字列に名前が現れない)。
        (
            "def keep(total):\n    return keep.__globals__[KEY](1)\n\n\nKEY = \"to\" + \"tal\"\n",
            local,
        ),
        // 暗黙の連結・エスケープで綴る文字列の注釈と、`__annotations__` に入れた文字列。
        (
            "def keep(total: \"to\" \"tal(1)\"):\n    return total\n",
            local,
        ),
        (
            "def keep(total: \"\\x74otal(1)\"):\n    return total\n",
            local,
        ),
        (
            "def keep(total):\n    return total\n\n\nkeep.__annotations__[\"return\"] = \"total(1)\"\n",
            local,
        ),
        // 内包表記と lambda の束縛は証明しない。
        (
            UTIL_NEW,
            "def run():\n    return [total for total in range(3)]\n",
        ),
        (UTIL_NEW, "def run():\n    return lambda total: total\n"),
    ] {
        assert_total_removal(util_new, &[("tools/report.py", report)], true);
    }
}

/// decorator で登録した関数 (pytest の fixture) は引数の名前で要求されるので、同じ名前の引数を
/// 関数のローカルの束縛として除かない。decorator の無い同じ関数なら除く (対照、Issue #471)。
#[test]
fn python_free_function_removal_keeps_parameters_that_request_fixtures() {
    let fixture_old = "import pytest\n\n\n@pytest.fixture\ndef total():\n    return 1\n\n\ndef keep():\n    return 1\n";
    let fixture_new = "import pytest\n\n\ndef keep():\n    return 1\n";
    let extras = [
        ("conftest.py", "pytest_plugins = (\"tools.util\",)\n"),
        (
            "tests/test_report.py",
            "def test_value(total):\n    assert total == 1\n",
        ),
    ];
    assert_total_removal_from(fixture_old, fixture_new, &extras, true);
    assert_total_removal_from(
        &fixture_old.replace("@pytest.fixture\n", ""),
        fixture_new,
        &extras,
        false,
    );
}

#[test]
fn python_removed_functions_reached_by_string_names_use_the_review_snapshot() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("pkg");
    repo.write("pkg/__init__.py", "");
    repo.write("pkg/other.py", "def noise(): return 0\n");
    let owner = "def _keep():\n    return globals()['by_global']()\n";
    let caller = r#"from unittest.mock import patch as replace
import operator
import pkg.api as api
import pkg.other as other
from typing import Annotated, Literal

def use(row, monkeypatch):
    getattr(api, 'by_getattr')()
    replace('pkg.api.by_patch')
    vars(api)['by_vars']()
    api.__dict__['by_dict']()
    operator.methodcaller('by_operator')(api)
    monkeypatch.setattr('pkg.api.by_monkeypatch', None)
    getattr(other, 'noise')()
    print('pkg.api.noise', row['noise'])
    replace('elsewhere.api.noise')

def annotated(arg: Literal["noise"]) -> Annotated["list[api.by_annotation]", "noise"]:
    pass
"#;
    let names = [
        "by_export",
        "by_global",
        "by_getattr",
        "by_patch",
        "by_vars",
        "by_dict",
        "by_operator",
        "by_annotation",
        "by_monkeypatch",
        "noise",
    ];
    let declarations: String = names
        .iter()
        .map(|name| format!("def {name}(): pass\n"))
        .collect();
    repo.write("pkg/api.py", format!("{owner}{declarations}"));
    repo.write("caller.py", caller);
    repo.commit_all("base");
    // __all__ は削除後も列挙される名前だけの参照になる。
    repo.write("pkg/api.py", format!("__all__ = ['by_export']\n{owner}"));
    repo.stage_all();
    // index に残る caller が作業ツリーと異なる場合も調べる。
    repo.write("caller.py", "print('by_getattr', 'pkg.api.by_patch')\n");
    for staged in [true, false] {
        let mut flags = vec!["--git"];
        if staged {
            flags.push("--staged");
        }
        let report = repo.run_json("review", &flags);
        let hook = cargo_bin()
            .args(["review", "--dir"])
            .arg(repo.root())
            .args(&flags)
            .arg("--hook")
            .output()
            .unwrap();
        assert_eq!(hook.status.code(), Some(1), "{report}");
        let hook: serde_json::Value = serde_json::from_slice(&hook.stderr).unwrap();
        for name in names {
            let live = name != "noise" && (staged || matches!(name, "by_export" | "by_global"));
            for (entries, field, expected) in [
                (&report["api_changes"]["removed"], "name", live),
                (&report["api_changes"]["removed_dead"], "name", !live),
                (&hook["api"]["rm"], "n", live),
                (&hook["api"]["rm_dead"], "n", !live),
            ] {
                assert_eq!(
                    entries
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|e| e[field] == name),
                    expected,
                    "{staged}/{name}: {entries}, {report}"
                );
            }
        }
    }
}
