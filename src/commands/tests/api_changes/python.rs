//! Python の API 差分検出テスト。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use crate::commands::*;
#[allow(unused_imports)]
use crate::models::review::{
    ApiChanges, ApiSymbol, ApiSymbolChange, CompatibleApiModification, MissingCochange,
    MovedSymbol, PropertyToFieldChange, ReviewResult,
};
#[allow(unused_imports)]
use std::collections::HashSet;
#[allow(unused_imports)]
use std::fs;
#[allow(unused_imports)]
use std::io::Cursor;
#[allow(unused_imports)]
use std::process::Command;

#[test]
fn extract_python_class_fields_collects_typed_annotations_only() {
    let dir = tempfile::tempdir().expect("tempdir");
    let py = "\
from dataclasses import dataclass


@dataclass
class A:
    x: int
    y: str = \"default\"
    untyped = 1


class B:
    z: float
";
    fs::write(dir.path().join("m.py"), py).expect("write");

    let a_fields = extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "A");
    assert!(
        a_fields.contains("x"),
        "typed annotation は採取される: {a_fields:?}"
    );
    assert!(
        a_fields.contains("y"),
        "default 値付き typed annotation も採取される: {a_fields:?}"
    );
    assert!(
        !a_fields.contains("untyped"),
        "type annotation が無い代入は採取しない: {a_fields:?}"
    );

    let b_fields = extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "B");
    assert!(
        b_fields.contains("z"),
        "@dataclass でないクラスでも採取する: {b_fields:?}"
    );

    let none = extract_python_class_fields(dir.path().to_str().expect("utf-8"), "m.py", "Missing");
    assert!(none.is_empty(), "存在しないクラス名は空集合: {none:?}");
}

/// Python で同一ファイル内から呼ばれている新規 public 関数は api.add に出ない。
#[test]
fn detect_api_changes_python_internally_called_function_is_not_added() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "def main():\n    print(\"hi\")\n";
    git_commit_files(repo, &[("svc.py", before)], "initial");

    // helper を追加し、main から呼ぶ
    let after = "def helper() -> str:\n    return \"x\"\n\n\
def main():\n    helper()\n    print(\"hi\")\n";
    fs::write(repo.join("svc.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "svc.py".to_string(),
        new_path: "svc.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let added: Vec<&str> = api_changes.added.iter().map(|s| s.name.as_str()).collect();
    assert!(
        !added.contains(&"helper"),
        "同一ファイル内で呼ばれている Python 関数は api.add に出してはならない。got: {added:?}"
    );
}

/// Python CLI スクリプト（同一ファイル内でのみ呼ばれる関数）のシグネチャ変更は
/// caller が同じ diff 内で追随できるため api.mod に出さない。
/// (レポート 2026-04-22-closed-in-diff-signature-change-noise.md の再現)
#[test]
fn detect_api_changes_python_cli_signature_change_not_modified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
def run_osv_scanner(path: str) -> int:
    return 0


def scan_worktree(path: str) -> int:
    rc = run_osv_scanner(path)
    return rc


if __name__ == \"__main__\":
    scan_worktree(\".\")
";
    git_commit_files(repo, &[("osv_scan.py", before)], "initial");

    // run_osv_scanner の戻り値型を int -> tuple[int, float] に変更。
    // caller (scan_worktree) も同じ diff 内で追随する。
    let after = "\
def run_osv_scanner(path: str) -> tuple[int, float]:
    return (0, 0.0)


def scan_worktree(path: str) -> int:
    _rc, _elapsed = run_osv_scanner(path)
    return _rc


if __name__ == \"__main__\":
    scan_worktree(\".\")
";
    fs::write(repo.join("osv_scan.py"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "osv_scan.py".to_string(),
        new_path: "osv_scan.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 11,
            new_start: 1,
            new_count: 11,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let mod_names: Vec<&str> = api_changes
        .modified
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !mod_names.contains(&"run_osv_scanner"),
        "同一ファイル内でのみ呼ばれる関数のシグネチャ変更は api.mod に出してはならない。got: {mod_names:?}"
    );
}

/// `detect_python_property_to_field` は old_path が Python の場合のみ判定する
/// (他言語の `Container.member` 削除が diff 内 .py の偶然の同名 class+field で
/// informational に降格しない)。
#[test]
fn detect_python_property_to_field_requires_python_old_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("new.py"),
        "from dataclasses import dataclass\n@dataclass\nclass Container:\n    member: int\n",
    )
    .expect("write");
    let dir_str = dir.path().to_str().expect("utf-8 path");
    let diff_new_paths: HashSet<String> = HashSet::from(["new.py".to_string()]);

    assert_eq!(
        detect_python_property_to_field(dir_str, "old.py", "Container.member", &diff_new_paths),
        Some("new.py".to_string()),
        "Python の old_path なら置き換え先 new.py を検出する"
    );
    assert_eq!(
        detect_python_property_to_field(dir_str, "old.ts", "Container.member", &diff_new_paths),
        None,
        "Python 以外の old_path は言語ガードで対象外"
    );
}

// ---------------------------------------------------------------------------
// TypedDict の `total=` 変更を型契約変更として分類する
// (Issue 2026-08-18-python-typeddict-contract-change-classification)
// ---------------------------------------------------------------------------

/// ファイル全体を 1 hunk として覆う `DiffFile` を作る。
/// closed-in-diff 判定に「参照行が変更 hunk に含まれるか」を渡すため、降格させたい側の
/// テストで意図せず hunk 外になるのを防ぐ。
fn whole_file_diff(path: &str, line_count: usize) -> crate::models::impact::DiffFile {
    crate::models::impact::DiffFile {
        old_path: path.to_string(),
        new_path: path.to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: line_count,
            new_start: 1,
            new_count: line_count,
        }],
        deleted_old_source: None,
    }
}

fn contract_of<'a>(
    api: &'a ApiChanges,
    name: &str,
) -> Option<&'a crate::models::review::ApiContractChange> {
    api.modified
        .iter()
        .find(|c| c.name == name)
        .and_then(|c| c.contract_change.as_ref())
}

#[test]
fn typed_dict_total_false_removal_is_classified_as_producer_break() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict


class Payload(TypedDict, total=False):
    a: int
    b: str
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int
    b: str
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 6)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let contract =
        contract_of(&api, "Payload").expect("total=False の除去は型契約変更として分類されるべき");
    assert_eq!(
        contract.kind,
        crate::models::review::ApiContractChangeKind::TypedDictTotalFalseRemoved
    );
    assert_eq!(
        contract.breaks,
        crate::models::review::ApiContractSide::Producer,
        "省略可キーの必須化で壊れるのは値を作る側"
    );
}

#[test]
fn typed_dict_total_false_addition_is_classified_as_consumer_break() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int
    b: str
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import TypedDict


class Payload(TypedDict, total=False):
    a: int
    b: str
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 6)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let contract =
        contract_of(&api, "Payload").expect("total=False の追加は型契約変更として分類されるべき");
    assert_eq!(
        contract.kind,
        crate::models::review::ApiContractChangeKind::TypedDictTotalFalseAdded
    );
    assert_eq!(
        contract.breaks,
        crate::models::review::ApiContractSide::Consumer,
        "必須キーが省略可になって壊れるのは値を読む側"
    );
}

/// fail-open 回帰テスト。値を作る側 (producer) が別ファイルにあり、同一 diff 内で完全に
/// 更新済みでも、`total=False` の除去は blocking な `api.mod` に残さなければならない。
/// 外部リポジトリ / 動的生成された dict は静的に追えないため。
///
/// **前提の明記**: 現状 Python の class は `is_modified_closed_in_diff` の call ref 判定で
/// `Payload(a=1)` が Call として記録されず (`refs` の kind は `ref`)、`call_refs` が空になって
/// そもそも降格経路に到達しない。したがってバケットの assert は「今日の降格を止めている証明」
/// ではなく、**Python の参照解決が強化されて class が降格対象になった日に効く前方ガード**である
/// (それを目指す Issue が実在する: 2026-08-18-api-mod-python-method-owner-aware-resolution)。
///
/// 今日この時点で非 vacuous なのは `contract_change` の assert で、
/// `classify_signature_change` の早期 return を外すと失敗することを確認済み。
/// 降格経路そのものが生きていることは
/// `non_typed_dict_change_with_updated_callers_is_still_demoted` (関数版) が担保する。
#[test]
fn typed_dict_total_change_is_never_demoted_even_if_callers_updated_in_diff() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let models_before = "\
from typing import TypedDict


class Payload(TypedDict, total=False):
    a: int
    b: str
";
    let producer_before = "\
from models import Payload


def build() -> Payload:
    return Payload(a=1)
";
    git_commit_files(
        repo,
        &[
            ("models.py", models_before),
            ("producer.py", producer_before),
        ],
        "initial",
    );

    let models_after = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int
    b: str
";
    // producer も同一 diff 内で必須化に追随する
    let producer_after = "\
from models import Payload


def build() -> Payload:
    return Payload(a=1, b=\"x\")
";
    fs::write(repo.join("models.py"), models_after).expect("write");
    fs::write(repo.join("producer.py"), producer_after).expect("write");

    let diff_files = vec![
        whole_file_diff("models.py", 6),
        whole_file_diff("producer.py", 5),
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api.modified.iter().any(|c| c.name == "Payload"),
        "呼び出し側が同一 diff で更新済みでも blocking な api.mod に残すこと。got modified: {:?}",
        api.modified.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    assert!(
        !api.modified_closed_in_diff
            .iter()
            .any(|c| c.name == "Payload"),
        "modified_closed_in_diff へ降格してはならない"
    );
    assert!(
        !api.compatible_modified.iter().any(|c| c.name == "Payload"),
        "compatible_modified へ降格してはならない"
    );
    assert_eq!(
        contract_of(&api, "Payload").map(|c| c.kind),
        Some(crate::models::review::ApiContractChangeKind::TypedDictTotalFalseRemoved)
    );
}

/// 「全部 blocking のまま」でも通るテストにしないための対照ケース。
/// 同じ形 (別ファイルの参照を同一 diff で更新) でも、TypedDict でない**関数**の
/// シグネチャ変更は従来どおり `modified_closed_in_diff` へ降格する。
/// これが赤くなったら降格経路自体が壊れており、上の非降格テストも意味を失う。
#[test]
fn non_typed_dict_change_with_updated_callers_is_still_demoted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let api_before = "\
def build(a: int) -> int:
    return a
";
    let caller_before = "\
from api import build


def run() -> int:
    return build(1)
";
    git_commit_files(
        repo,
        &[("api.py", api_before), ("caller.py", caller_before)],
        "initial",
    );

    let api_after = "\
def build(a: int, b: str) -> int:
    return a + len(b)
";
    let caller_after = "\
from api import build


def run() -> int:
    return build(1, \"x\")
";
    fs::write(repo.join("api.py"), api_after).expect("write");
    fs::write(repo.join("caller.py"), caller_after).expect("write");

    let diff_files = vec![
        whole_file_diff("api.py", 2),
        whole_file_diff("caller.py", 5),
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api.modified_closed_in_diff
            .iter()
            .any(|c| c.name == "build"),
        "対照ケースは従来どおり降格すること (降格経路が生きている証明)。\
         got modified: {:?} / closed: {:?}",
        api.modified.iter().map(|c| &c.name).collect::<Vec<_>>(),
        api.modified_closed_in_diff
            .iter()
            .map(|c| &c.name)
            .collect::<Vec<_>>()
    );
}

#[test]
fn omitted_total_to_explicit_true_is_not_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    // 実効 total は変わらない (省略 = True) ため、素の api.mod に留める。
    let after = "\
from typing import TypedDict


class Payload(TypedDict, total=True):
    a: int
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 5)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        contract_of(&api, "Payload").is_none(),
        "省略 ↔ total=True は意味的に同値なので分類しない"
    );
}

#[test]
fn total_change_on_non_typed_dict_class_is_not_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
class Base:
    pass


class Payload(Base, total=False):
    a: int
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
class Base:
    pass


class Payload(Base):
    a: int
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 6)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        contract_of(&api, "Payload").is_none(),
        "TypedDict と証明できない基底クラスでは分類しない"
    );
    assert!(
        api.modified.iter().any(|c| c.name == "Payload")
            || api
                .modified_closed_in_diff
                .iter()
                .any(|c| c.name == "Payload"),
        "分類できなくても従来どおり api.mod としては検出されること"
    );
}

#[test]
fn total_change_with_not_required_field_is_not_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict, NotRequired


class Payload(TypedDict, total=False):
    a: int
    b: NotRequired[str]
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import TypedDict, NotRequired


class Payload(TypedDict):
    a: int
    b: NotRequired[str]
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 6)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        contract_of(&api, "Payload").is_none(),
        "NotRequired が同居するファイルは実効 requiredness を証明できないので分類しない (別 Issue)"
    );
}

#[test]
fn total_change_with_simultaneous_field_change_is_not_classified() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict


class Payload(TypedDict, total=False):
    a: int
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    // total とフィールド追加が同時に起きると「壊れる側」が一意に決まらない。
    let after = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int
    b: str
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 6)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        contract_of(&api, "Payload").is_none(),
        "total 以外の変更が混ざる場合は分類しない"
    );
}

/// **severity ガードの回帰テスト** (レビュー 3 巡目の指摘)。
///
/// `NotRequired` が同居して**種別を確定できない** `total=` 変更でも、blocking な `api.mod` に
/// 残さなければならない。分類できないことは「破壊的でない」証明ではなく、ここでは bare な
/// `a: int` の requiredness が実際に反転している。
///
/// 前提は `typed_dict_total_change_is_never_demoted_even_if_callers_updated_in_diff` と同じで、
/// 現状 Python の class は降格経路に到達しないため、バケットの assert は前方ガードとして効く。
#[test]
fn unclassifiable_total_change_is_still_not_demoted() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let models_before = "\
from typing import TypedDict, NotRequired


class Payload(TypedDict, total=False):
    a: int
    b: NotRequired[str]
";
    let producer_before = "\
from models import Payload


def build() -> Payload:
    return Payload(a=1)
";
    git_commit_files(
        repo,
        &[
            ("models.py", models_before),
            ("producer.py", producer_before),
        ],
        "initial",
    );

    let models_after = "\
from typing import TypedDict, NotRequired


class Payload(TypedDict):
    a: int
    b: NotRequired[str]
";
    let producer_after = "\
from models import Payload


def build() -> Payload:
    return Payload(a=1)
";
    fs::write(repo.join("models.py"), models_after).expect("write");
    fs::write(repo.join("producer.py"), producer_after).expect("write");

    let diff_files = vec![
        whole_file_diff("models.py", 7),
        whole_file_diff("producer.py", 5),
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api.modified.iter().any(|c| c.name == "Payload"),
        "種別を確定できなくても blocking な api.mod に残すこと。got modified: {:?}",
        api.modified.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    assert!(
        !api.modified_closed_in_diff
            .iter()
            .any(|c| c.name == "Payload"),
        "分類できないことを理由に降格してはならない"
    );
    assert!(
        !api.compatible_modified.iter().any(|c| c.name == "Payload"),
        "compatible_modified へ降格してはならない"
    );
    assert!(
        contract_of(&api, "Payload").is_none(),
        "証明できていないのでラベルは付けない"
    );
}

/// **severity ガードの回帰テスト 2** (レビュー 4 巡目の指摘)。
///
/// 同一ファイル内でしか使われず cross-file 参照が 0 件のシンボルは、通常
/// `is_internally_connected` の早期除外で api.mod にすら出ない。しかし TypedDict の
/// `total=` 変更はキーの必須化という破壊的変更なので、この除外に落としてはならない。
#[test]
fn typed_dict_total_change_used_only_in_same_file_is_still_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
from typing import TypedDict


class Payload(TypedDict, total=False):
    a: int


def build() -> Payload:
    return Payload()
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
from typing import TypedDict


class Payload(TypedDict):
    a: int


def build() -> Payload:
    return Payload(a=1)
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 9)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        api.modified.iter().any(|c| c.name == "Payload"),
        "同一ファイル内利用だけでも TypedDict の total 変更は api.mod に出すこと。got: {:?}",
        api.modified.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
    assert_eq!(
        contract_of(&api, "Payload").map(|c| c.kind),
        Some(crate::models::review::ApiContractChangeKind::TypedDictTotalFalseRemoved)
    );
}

/// 対照ケース: TypedDict でない Python class は従来どおり早期除外される。
/// これが赤くなったら迂回が広すぎる (無関係なシンボルまで api.mod に出している)。
#[test]
fn non_typed_dict_class_used_only_in_same_file_is_still_excluded() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "\
class Base:
    pass


class Payload(Base, total=False):
    a: int


def build() -> Payload:
    return Payload()
";
    git_commit_files(repo, &[("models.py", before)], "initial");

    let after = "\
class Base:
    pass


class Payload(Base):
    a: int


def build() -> Payload:
    return Payload()
";
    fs::write(repo.join("models.py"), after).expect("write");

    let diff_files = vec![whole_file_diff("models.py", 10)];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    assert!(
        !api.modified.iter().any(|c| c.name == "Payload"),
        "TypedDict と証明できないクラスは従来どおり早期除外されること。got: {:?}",
        api.modified.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
}
