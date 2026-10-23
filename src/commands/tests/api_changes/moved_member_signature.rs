//! 型の移動と同じ差分でメンバーのシグネチャを変えた変更を、移さずに変えたときと同じ規則で判定する。

use crate::commands::tests::common::*;
use crate::models::review::ApiChanges;
use std::fs;

const CARGO: &str = "[package]\nname = \"shelf\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
const OLD_TYPE: &str = "pub struct Packer;\n\nimpl Packer {\n    pub fn pack_with_progress(&self, count: u32) -> u32 {\n        count * 2\n    }\n}\n";
const NEW_TYPE: &str = "pub struct Packer;\n\nimpl Packer {\n    pub fn pack_with_progress(&self, count: u32, report: impl Fn(u32)) -> u32 {\n        report(count);\n        count * 2\n    }\n}\n";
const OLD_CALL: &str = "Packer.pack_with_progress(3)";
const NEW_CALL: &str = "Packer.pack_with_progress(3, |_| {})";
const METHOD: &str = "Packer.pack_with_progress";

/// 型のファイルを移した差分を、どの形で渡すか。
#[derive(Clone, Copy, Debug, PartialEq)]
enum Move {
    /// `git mv` で移す。ファイルが小さいので、git は削除と追加として渡す。
    DeleteAdd,
    /// `git mv` で移し、変わらない行を足して git に rename として検出させる。
    Rename,
    /// 旧ファイルを消し、移動先を git に足さない (未追跡の新しいファイル)。
    Untracked,
}

const MOVES: [Move; 3] = [Move::DeleteAdd, Move::Rename, Move::Untracked];

fn caller(module: &str, call: &str) -> String {
    format!("use crate::{module}::Packer;\n\npub fn handle() -> u32 {{\n    {call}\n}}\n")
}

/// 変更前をコミットし、型のファイルを移してから変更後の内容を書いた作業ツリーの API 差分。
fn api_after_move(old_type: &str, new_type: &str, call: &str, how: Move) -> ApiChanges {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let padding: String = if how == Move::Rename {
        (0..40).map(|i| format!("// shelf note {i}\n")).collect()
    } else {
        String::new()
    };
    git_commit_files(
        repo,
        &[
            ("Cargo.toml", CARGO),
            ("src/lib.rs", "pub mod caller;\npub mod old_place;\n"),
            ("src/old_place.rs", &format!("{padding}{old_type}")),
            ("src/caller.rs", &caller("old_place", OLD_CALL)),
        ],
        "base",
    );
    if how == Move::Untracked {
        fs::remove_file(repo.join("src/old_place.rs")).unwrap();
    } else {
        git_in(repo, &["mv", "src/old_place.rs", "src/new_place.rs"]);
    }
    fs::write(
        repo.join("src/lib.rs"),
        "pub mod caller;\npub mod new_place;\n",
    )
    .unwrap();
    fs::write(
        repo.join("src/new_place.rs"),
        format!("{padding}{new_type}"),
    )
    .unwrap();
    fs::write(repo.join("src/caller.rs"), caller("new_place", call)).unwrap();
    detect_api_changes_from_worktree(repo)
}

fn packer_moved(api: &ApiChanges) -> bool {
    api.moved
        .iter()
        .any(|m| m.name == "Packer" && m.from == "src/old_place.rs" && m.to == "src/new_place.rs")
}

/// メソッドが api.mod (`blocking`) か mod_closed のどちらか一方にだけ、移動先のファイルで出る。
fn assert_modified(api: &ApiChanges, blocking: bool, label: &str) {
    let (reported, other) = if blocking {
        (&api.modified, &api.modified_closed_in_diff)
    } else {
        (&api.modified_closed_in_diff, &api.modified)
    };
    assert!(
        reported
            .iter()
            .any(|s| s.name == METHOD && s.file == "src/new_place.rs"),
        "{label}: {api:?}"
    );
    assert!(other.iter().all(|s| s.name != METHOD), "{label}: {api:?}");
    assert!(
        api.removed.iter().all(|s| s.name != METHOD),
        "{label}: {api:?}"
    );
    assert!(
        api.removed_dead.iter().all(|s| s.name != METHOD),
        "{label}: {api:?}"
    );
}

/// 削除と追加として渡る差分でも、rename として渡る差分と同じく、呼び出しを直したメソッドは
/// mod_closed になる。型は削除と追加の経路でだけ moved に出る (rename では型の宣言は変わらない)。
#[test]
fn moved_member_signature_change_with_updated_call_is_closed_in_diff() {
    for how in MOVES {
        let api = api_after_move(OLD_TYPE, NEW_TYPE, NEW_CALL, how);
        if how != Move::Untracked {
            assert_eq!(
                packer_moved(&api),
                how == Move::DeleteAdd,
                "{how:?}: {api:?}"
            );
        }
        assert_modified(&api, false, &format!("{how:?}"));
    }
}

/// 呼び出しを直していなければ、移さずに引数を足したときと同じく blocking な api.mod に残る。
#[test]
fn moved_member_signature_change_without_updated_call_stays_blocking() {
    for how in MOVES {
        let api = api_after_move(OLD_TYPE, NEW_TYPE, OLD_CALL, how);
        if how != Move::Untracked {
            assert_eq!(
                packer_moved(&api),
                how == Move::DeleteAdd,
                "{how:?}: {api:?}"
            );
        }
        assert_modified(&api, true, &format!("{how:?}"));
    }
}

/// derive などの属性、owner を右辺で使うだけの型別名、別の名前の use がある普通のファイルでも
/// 組にできる (owner を束縛しないので、impl の型は移した型を指す)。
#[test]
fn moved_member_signature_change_allows_ordinary_attributes_and_aliases() {
    let decorate = |ty: &str| {
        format!(
            "use std::sync::Arc;\n\n{}\npub type SharedPacker = Arc<Packer>;\n",
            ty.replace(
                "pub struct Packer;",
                "#[derive(Debug, Clone)]\npub struct Packer;"
            )
        )
    };
    for how in MOVES {
        let api = api_after_move(&decorate(OLD_TYPE), &decorate(NEW_TYPE), NEW_CALL, how);
        assert_modified(&api, false, &format!("{how:?}"));
    }
}

/// 移動先のファイルの中の呼び出しは、移動元から持ち込んだだけなら追随の証拠にならない。
/// 削除と追加として渡ると移動先の全行が追加行になるが、rename と同じく直した行だけを数える。
#[test]
fn moved_member_signature_change_checks_calls_inside_the_moved_file() {
    let helper = |call: &str| format!("\npub fn helper() -> u32 {{\n    {call}\n}}\n");
    let old_type = format!("{OLD_TYPE}{}", helper("Packer.pack_with_progress(1)"));
    for (helper_call, blocking) in [
        ("Packer.pack_with_progress(1)", true),
        ("Packer.pack_with_progress(1, |_| {})", false),
    ] {
        let new_type = format!("{NEW_TYPE}{}", helper(helper_call));
        for how in MOVES {
            let api = api_after_move(&old_type, &new_type, NEW_CALL, how);
            assert_modified(&api, blocking, &format!("{how:?} {helper_call}"));
        }
    }
}

/// 移した先に同じ名前のメソッドや関数、同じ名前の型が別にあると組が一意に決まらないので、
/// 今と同じく api.rm に残す。
#[test]
fn moved_member_signature_change_with_ambiguous_destination_stays_removed() {
    for new_type in [
        format!(
            "{NEW_TYPE}pub struct Other;\n\nimpl Other {{\n    pub fn pack_with_progress(&self) -> u32 {{\n        0\n    }}\n}}\n"
        ),
        format!("{NEW_TYPE}pub fn pack_with_progress() -> u32 {{\n    0\n}}\n"),
        // 移した型はメソッドを持たず、別モジュールの同名の型がシグネチャの違う同名メソッドを持つ。
        format!("pub struct Packer;\n\npub mod other {{\n{NEW_TYPE}}}\n"),
        // 別モジュールの同名の型が非公開でも、そのメソッドは公開面に残り得る。
        format!(
            "pub struct Packer;\n\nmod other {{\n{}}}\n",
            NEW_TYPE.replace("pub struct Packer;", "struct Packer;")
        ),
        // 別モジュールで別の型に owner と同じ別名を付けた impl (`use ... as` と型別名)。
        format!(
            "pub struct Packer;\n\nmod other {{\n{}}}\n",
            NEW_TYPE.replace("pub struct Packer;", "use crate::Other as Packer;")
        ),
        format!(
            "pub struct Packer;\n\nmod other {{\n{}}}\n",
            NEW_TYPE.replace("pub struct Packer;", "type Packer = crate::Other;")
        ),
        // cfg で無効にした型宣言と、同じスコープで別の型に付けた同名の別名。
        NEW_TYPE.replace(
            "pub struct Packer;",
            "#[cfg(any())]\npub struct Packer;\n\npub use crate::Other as Packer;",
        ),
    ] {
        let api = api_after_move(OLD_TYPE, &new_type, NEW_CALL, Move::DeleteAdd);
        assert!(packer_moved(&api), "{new_type}: {api:?}");
        assert!(
            api.removed
                .iter()
                .any(|s| s.name == METHOD && s.file == "src/old_place.rs"),
            "{new_type}: {api:?}"
        );
        assert!(
            api.modified_closed_in_diff.iter().all(|s| s.name != METHOD),
            "{new_type}: {api:?}"
        );
    }
}

/// 型を移すだけの変更は、従来どおりメソッドを削除にも変更にも出さない。
#[test]
fn moved_member_without_signature_change_is_unchanged() {
    for how in MOVES {
        let api = api_after_move(OLD_TYPE, OLD_TYPE, OLD_CALL, how);
        for name in ["Packer", METHOD] {
            assert!(
                api.removed.iter().all(|s| s.name != name),
                "{how:?}: {api:?}"
            );
            assert!(
                api.modified.iter().all(|s| s.name != name),
                "{how:?}: {api:?}"
            );
            assert!(
                api.modified_closed_in_diff.iter().all(|s| s.name != name),
                "{how:?}: {api:?}"
            );
        }
    }
}

/// クラスを `git mv` で別のモジュールへ移し、メソッド `Packer.pack` の引数を変えた差分の判定。
/// `files` は (旧パス, 新パス, 変更前, 変更後)、`caller` は (パス, 変更前, 変更後)。
fn assert_moved_class_method(
    files: [&str; 4],
    caller: [&str; 3],
    extra: &[(&str, &str)],
    updated: bool,
) {
    let [old_path, new_path, old_type, new_type] = files;
    let [caller_path, old_caller, new_caller] = caller;
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let mut base = vec![(old_path, old_type), (caller_path, old_caller)];
    base.extend_from_slice(extra);
    git_commit_files(repo, &base, "base");
    git_in(repo, &["mv", old_path, new_path]);
    fs::write(repo.join(new_path), new_type).unwrap();
    fs::write(repo.join(caller_path), new_caller).unwrap();
    let api = detect_api_changes_from_worktree(repo);
    let label = format!("{new_path} updated={updated}");
    assert!(
        api.moved.iter().any(|m| m.name == "Packer"),
        "{label}: {api:?}"
    );
    let (reported, other) = if updated {
        (&api.modified_closed_in_diff, &api.modified)
    } else {
        (&api.modified, &api.modified_closed_in_diff)
    };
    assert!(
        reported
            .iter()
            .any(|s| s.name == "Packer.pack" && s.file == new_path),
        "{label}: {api:?}"
    );
    assert!(
        other.iter().all(|s| s.name != "Packer.pack"),
        "{label}: {api:?}"
    );
    assert!(
        api.removed.iter().all(|s| s.name != "Packer.pack"),
        "{label}: {api:?}"
    );
    assert!(
        api.removed_dead.iter().all(|s| s.name != "Packer.pack"),
        "{label}: {api:?}"
    );
}

/// 言語を限定しない。Python のクラスを別のモジュールへ移してメソッドの引数を変えた差分も、
/// 呼び出しを直していれば mod_closed、直していなければ api.mod になる。
#[test]
fn moved_python_method_signature_change_follows_the_same_rules() {
    for updated in [true, false] {
        let call = if updated {
            "Packer().pack(3, print)"
        } else {
            "Packer().pack(3)"
        };
        assert_moved_class_method(
            [
                "pkg/old_place.py",
                "pkg/new_place.py",
                "class Packer:\n    def pack(self, count):\n        return count * 2\n",
                "class Packer:\n    def pack(self, count, report):\n        report(count)\n        return count * 2\n",
            ],
            [
                "pkg/caller.py",
                "from pkg.old_place import Packer\n\n\ndef handle():\n    return Packer().pack(3)\n",
                &format!(
                    "from pkg.new_place import Packer\n\n\ndef handle():\n    return {call}\n"
                ),
            ],
            &[("pkg/__init__.py", "")],
            updated,
        );
    }
}

/// TS のクラスも同じ。修正前は呼び出しを直していなくても移動元の削除が removed_dead に落ち、
/// 必須引数を足した変更を見逃していた。
#[test]
fn moved_typescript_method_signature_change_follows_the_same_rules() {
    for updated in [true, false] {
        let call = if updated {
            "new Packer().pack(3, () => {})"
        } else {
            "new Packer().pack(3)"
        };
        assert_moved_class_method(
            [
                "src/oldPlace.ts",
                "src/newPlace.ts",
                "export class Packer {\n  pack(count: number): number {\n    return count * 2;\n  }\n}\n",
                "export class Packer {\n  pack(count: number, report: (n: number) => void): number {\n    report(count);\n    return count * 2;\n  }\n}\n",
            ],
            [
                "src/caller.ts",
                "import { Packer } from \"./oldPlace\";\n\nexport function handle(): number {\n  return new Packer().pack(3);\n}\n",
                &format!(
                    "import {{ Packer }} from \"./newPlace\";\n\nexport function handle(): number {{\n  return {call};\n}}\n"
                ),
            ],
            &[(
                "package.json",
                "{\"name\":\"shelf\",\"version\":\"1.0.0\"}\n",
            )],
            updated,
        );
    }
}
