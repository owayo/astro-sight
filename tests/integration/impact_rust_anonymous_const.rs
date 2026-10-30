use super::support::*;

const API_BASE: &str = "pub const LIMITS: [u8; 3] = [0, 1, 2];\n\npub fn limit(index: u64) -> Option<u8> {\n    Some(match index {\n        0 => LIMITS[0],\n        1 => LIMITS[1],\n        2 => LIMITS[2],\n        _ => return None,\n    })\n}\n";

const API_WITH_ANONYMOUS_CONST: &str = "pub const LIMITS: [u8; 3] = [0, 1, 2];\n\npub fn limit(index: u64) -> Option<u8> {\n    let index = usize::try_from(index).ok()?;\n    LIMITS.get(index).copied()\n}\n\nconst _: () = {\n    assert!(LIMITS.len() == 3);\n};\n";

/// 変更したシンボルを参照せず、型推論の `_` だけを持つ別ファイル。
/// 匿名定数を `_` という名前のシンボルとして登録すると、ここが呼び出し元に化ける。
const UNRELATED: &str = "pub fn names(items: Vec<String>) -> Vec<String> {\n    items.into_iter().collect::<Vec<_>>()\n}\n";

fn fixture() -> TestRepo {
    let repo = TestRepo::new();
    repo.write("api.rs", API_BASE);
    repo.write("unrelated.rs", UNRELATED);
    repo.init_git();
    repo.commit_all("base");
    repo
}

fn origins(context: &serde_json::Value) -> Vec<&str> {
    context["changes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|change| change["affected_symbols"].as_array().unwrap())
        .map(|symbol| symbol["name"].as_str().unwrap())
        .collect()
}

fn symbol_names(repo: &TestRepo, relative: &str) -> Vec<String> {
    let output = cargo_bin()
        .args(["symbols", "--path"])
        .arg(repo.path(relative))
        .output()
        .expect("failed to run astro-sight");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("astro-sight returned invalid JSON");
    json["symbols"]
        .as_array()
        .expect("symbols is not an array")
        .iter()
        .map(|symbol| symbol["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn rust_anonymous_const_is_not_a_symbol_or_an_impact_origin() {
    let repo = fixture();
    repo.write("api.rs", API_WITH_ANONYMOUS_CONST);

    // `const _` は名前を導入しない項目なのでシンボルにならない。
    assert_eq!(symbol_names(&repo, "api.rs"), ["LIMITS", "limit"]);

    // 影響の起点は実際に変えた関数だけで、`_` は入らない。
    let context = repo.run_json("context", &["--git"]);
    assert_eq!(origins(&context), ["limit"], "{context}");

    // 変更した関数を誰も呼んでいないので、型推論の `_` を呼び出し元に数えなければ
    // 未解決の影響は無く、終了コードは 0 になる。
    let output = cargo_bin()
        .args(["impact", "--dir"])
        .arg(repo.root())
        .arg("--git")
        .output()
        .expect("failed to run astro-sight");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let review = repo.run_json("review", &["--git"]);
    assert_eq!(origins(&review["impact"]), ["limit"], "{review}");
}

#[test]
fn named_rust_constants_are_still_symbols_and_impact_origins() {
    let repo = fixture();
    repo.write(
        "api.rs",
        API_BASE.replace("[u8; 3] = [0, 1, 2]", "[u8; 4] = [0, 1, 2, 3]"),
    );

    assert_eq!(symbol_names(&repo, "api.rs"), ["LIMITS", "limit"]);
    let context = repo.run_json("context", &["--git"]);
    assert_eq!(origins(&context), ["LIMITS"], "{context}");
}

/// `_` は Rust では識別子になり得ないが、他言語では普通の名前になる
/// (JavaScript の lodash など)。除外を言語で区切っていないと、こちらが壊れる。
#[test]
fn javascript_underscore_binding_is_still_a_symbol() {
    let repo = TestRepo::new();
    repo.write(
        "util.js",
        "export const _ = { map(items) { return items; } };\n",
    );
    assert!(
        symbol_names(&repo, "util.js").contains(&"_".to_owned()),
        "JavaScript の `_` はシンボルから外さない"
    );
}
