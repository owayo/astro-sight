use super::support::TestRepo;

/// `--dead-scope touched-symbols` の class member 回帰テスト。DeadSymbol.name が
/// qualname (`Store.orphanA`) の member は `extract_symbol_lines` の bare name キーと
/// 一致せず「宣言行不明 → touched 扱い」で常に残っていた (bare name fallback の検証)。
#[test]
fn review_dead_scope_touched_symbols_filters_class_members() {
    let repo = TestRepo::new();
    repo.write(
        "store.ts",
        "export class Store {\n    orphanA(): void {}\n\n    useMe(): void {}\n}\n",
    );
    // Store / useMe を live にする caller (orphanA だけが元から dead)
    repo.write(
        "main.ts",
        "import { Store } from './store';\n\nexport function boot(): void {\n    new Store().useMe();\n}\n",
    );
    repo.init_git();
    repo.commit_all("initial");

    // orphanA の宣言行に触れないコメント 1 行だけの diff を作る
    repo.write(
        "store.ts",
        "export class Store {\n    orphanA(): void {}\n\n    useMe(): void {}\n}\n// メモ: コメント追加のみ\n",
    );

    let dead_names = |args: &[&str]| -> Vec<String> {
        repo.run_json("review", args)["dead_symbols"]
            .as_array()
            .expect("dead_symbols should be an array")
            .iter()
            .filter_map(|s| s["name"].as_str().map(str::to_string))
            .collect()
    };

    // touched-symbols: 宣言行が hunk と重ならない Store.orphanA は除外される
    let touched = dead_names(&["--git", "--dead-scope", "touched-symbols"]);
    assert!(
        !touched.iter().any(|n| n.contains("orphanA")),
        "宣言行が diff に触れていない Store.orphanA は touched-symbols で除外されるべき: {touched:?}"
    );

    // 対照: --dead-scope all では元から存在する dead として残る
    let all = dead_names(&["--git", "--dead-scope", "all"]);
    assert!(
        all.iter().any(|n| n.contains("orphanA")),
        "--dead-scope all では Store.orphanA が dead_symbols に残るべき: {all:?}"
    );
}
