//! dead-code の参照の集計で、大きいファイルの構文解析の予算 (`ASTRO_SIGHT_PARSE_BUDGET_MIB`)
//! を変えても結果が変わらないこと、不正な値は既定に戻さず失敗させることの統合テスト。

#[allow(unused_imports)]
use super::support::*;

const PARSE_BUDGET_ENV: &str = "ASTRO_SIGHT_PARSE_BUDGET_MIB";

/// 1 MiB を超える (予算の対象の) ファイル 2 本からだけ呼ばれる関数は、予算を 0 (1 本ずつ)・
/// 1 (どちらも 1 MiB の予算を超えて単独)・既定・十分大きい値のどれでも生きていると数え、未使用の関数は
/// dead のままにする。出力は予算に依らず同じ。
#[test]
fn dead_code_output_is_identical_across_parse_budgets() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write(
        "src/lib.ts",
        "export function usedOnlyFromLarge(): number {\n  return 1;\n}\n\n\
         export function unusedHelper(): number {\n  return 2;\n}\n\n\
         export function usedFromSmall(): number {\n  return 3;\n}\n",
    );
    repo.write(
        "src/small.ts",
        "import { usedFromSmall } from \"./lib\";\nusedFromSmall();\n",
    );
    // 大きさだけを 1 MiB 超にする (1 つのブロックコメントなので debug ビルドでも解析が軽い)。
    let filler = format!("/* {} */\n", "x".repeat(1024 * 1024));
    for index in 0..2 {
        repo.write(
            format!("src/large{index}.ts"),
            format!(
                "import {{ usedOnlyFromLarge }} from \"./lib\";\n{filler}usedOnlyFromLarge();\n"
            ),
        );
    }

    let baseline = repo.run_json("dead-code", &[]);
    let dead: Vec<_> = baseline["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|symbol| symbol["name"].as_str())
        .collect();
    assert_eq!(dead, ["unusedHelper"], "{baseline}");
    for value in ["0", "1", "4096"] {
        let json = repo.run_json_in(".", [(PARSE_BUDGET_ENV, value)], "dead-code", &[]);
        assert_eq!(json, baseline, "{PARSE_BUDGET_ENV}={value}");
    }
}

/// MiB の非負の整数でない値や、バイトに直すと溢れる値は INVALID_REQUEST で失敗させる
/// (黙って既定の予算に戻さない)。
#[test]
fn dead_code_rejects_invalid_parse_budget() {
    let repo = TestRepo::new();
    repo.write(
        "a.ts",
        "export function onlyExport(): number {\n  return 1;\n}\n",
    );
    for value in ["-1", "1.5", "32MiB", "17592186044416"] {
        let output = cargo_bin()
            .args(["dead-code", "--dir", repo.root().to_str().unwrap()])
            .env(PARSE_BUDGET_ENV, value)
            .output()
            .expect("failed to run astro-sight");
        assert!(!output.status.success(), "{value}");
        let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
        assert_eq!(json["error"]["code"], "INVALID_REQUEST", "{value}: {json}");
        let message = json["error"]["message"].as_str().unwrap_or_default();
        assert!(message.contains(PARSE_BUDGET_ENV), "{value}: {message}");
    }
}
