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
