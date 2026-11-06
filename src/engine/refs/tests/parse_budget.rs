//! 参照の集計で、大きいファイルの構文解析を予算で制限しても件数と申告が変わらないこと。

use super::*;

type Snapshot = (Vec<(String, (usize, usize))>, Vec<UncountedFile>);

fn count_with(
    names: &[String],
    files: &[std::path::PathBuf],
    budget: ParseBudget,
    threads: usize,
) -> Snapshot {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();
    let result = pool
        .install(|| {
            count_non_definition_refs_split_in_files(
                names,
                files,
                |path| path.components().any(|part| part.as_os_str() == "tests"),
                budget,
            )
        })
        .unwrap();
    let mut counts: Vec<_> = result.counts.into_iter().collect();
    counts.sort();
    (counts, result.uncounted)
}

/// 閾値を下げて全ファイルを予算の対象にし、予算を 0 (1 本ずつ)・小さい値・無制限に変えても、
/// 件数 (production / test)、数えられなかったファイルとその参照しうる名前は同じになる。
/// 予算の対象のファイルには、名前を含まないもの (事前フィルタで数えずに終わる)、言語を
/// 判定できず構文解析に進めないもの、開けないもの、予算を超える 1 本を含める。ワーカー
/// 1 本でも同じ。
#[test]
fn budgeted_counts_match_unbudgeted_counts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    let padding = format!("// {}\n", "x".repeat(60)).repeat(40);
    let files = [
        (
            "src/lib.rs",
            format!("pub fn alpha() {{}}\npub fn beta() {{}}\n{padding}"),
        ),
        (
            "src/user.rs",
            "fn main() { alpha(); alpha(); gamma(); }\n".to_string(),
        ),
        (
            "src/big.rs",
            format!("{padding}{padding}{padding}fn run() {{ beta(); delta(); }}\n"),
        ),
        ("src/none.rs", format!("{padding}fn unrelated() {{}}\n")),
        (
            "src/notes.unknownext",
            format!("{padding}alpha and gamma\n"),
        ),
        (
            "src/app.ts",
            format!("{padding}export function gamma() {{ return alpha(); }}\n"),
        ),
        (
            "tests/it.rs",
            format!("#[test]\nfn it() {{ beta(); delta(); }}\n{padding}"),
        ),
    ];
    let mut paths = vec![root.join("src/missing.rs")];
    for (path, content) in &files {
        std::fs::write(root.join(path), content).unwrap();
        paths.push(root.join(path));
    }
    paths.sort();
    let names: Vec<String> = ["alpha", "beta", "gamma", "delta", "unused"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // 閾値を最大にすると予算の対象が無く、従来どおり全部をその場で数える。
    let expected = count_with(&names, &paths, ParseBudget::new(u64::MAX, 0), 4);
    let counts: std::collections::HashMap<_, _> = expected.0.iter().cloned().collect();
    assert_eq!(counts["alpha"], (3, 0), "{expected:?}");
    assert_eq!(counts["beta"], (1, 1), "{expected:?}");
    assert_eq!(counts["gamma"], (1, 0), "{expected:?}");
    assert_eq!(counts["delta"], (1, 1), "{expected:?}");
    assert_eq!(counts["unused"], (0, 0), "{expected:?}");
    let uncounted: Vec<_> = expected
        .1
        .iter()
        .map(|file| (file.path.clone(), file.possible_names.clone()))
        .collect();
    assert_eq!(
        uncounted,
        [
            (root.join("src/missing.rs"), None),
            (root.join("src/notes.unknownext"), Some(vec![0, 2])),
        ],
        "{expected:?}"
    );

    let largest = files.iter().map(|(_, c)| c.len() as u64).max().unwrap();
    for budget in [
        ParseBudget::new(1, 0),
        ParseBudget::new(1, largest / 2),
        ParseBudget::new(1, largest * 2),
        ParseBudget::new(1, u64::MAX),
        ParseBudget::new(largest, 0),
        ParseBudget::default(),
    ] {
        for threads in [1, 4] {
            assert_eq!(
                count_with(&names, &paths, budget, threads),
                expected,
                "{budget:?} threads={threads}"
            );
        }
    }
}
