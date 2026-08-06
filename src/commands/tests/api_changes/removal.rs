//! 削除シンボルの帰属判定 (api.rm / removed_dead) のテスト。

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

/// GitLab #33: PHP メソッドへの Eloquent リレーション戻り型付与 (`monitorLogs()` →
/// `monitorLogs(): HasOne`) は removed ではなく modified。Laravel entrypoint 除外が
/// API 差分経路 (exclude_framework_entrypoints=false) に効いて新側だけ除外され、
/// 実在メソッドが api.rm に誤分類されていた。
#[test]
fn detect_api_changes_php_eloquent_relation_return_type_added_is_modified_not_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/Models/VoiceLogSummaryEloquent.php",
                "<?php\n\nclass VoiceLogSummaryEloquent extends AbstractEloquent {\n    public function monitorLogs() {\n        return $this->hasMany(MonitorLogEloquent::class, 'request_id', 'request_id');\n    }\n}\n",
            ),
            (
                "src/Repositories/VoiceLogSummaryRepositoryQuery.php",
                "<?php\n\nclass VoiceLogSummaryRepositoryQuery {\n    public function fetch($eloquent) {\n        $monitorLog = $eloquent->monitorLogs;\n        return $monitorLog;\n    }\n}\n",
            ),
        ],
        "base",
    );
    fs::write(
        repo.join("src/Models/VoiceLogSummaryEloquent.php"),
        "<?php\n\nuse Illuminate\\Database\\Eloquent\\Relations\\HasOne;\n\nclass VoiceLogSummaryEloquent extends AbstractEloquent {\n    public function monitorLogs(): HasOne {\n        return $this->hasOne(MonitorLogEloquent::class, 'request_id', 'request_id');\n    }\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/Models/VoiceLogSummaryEloquent.php".to_string(),
        new_path: "src/Models/VoiceLogSummaryEloquent.php".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 7,
            new_start: 1,
            new_count: 9,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        !api.removed
            .iter()
            .chain(api.removed_dead.iter())
            .any(|s| s.name.ends_with("monitorLogs")),
        "実在メソッドの返り型付与を removed/removed_dead に分類しない。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        api.modified
            .iter()
            .any(|m| m.name == "VoiceLogSummaryEloquent.monitorLogs"),
        "返り型付与はシグネチャ変更として modified に分類する。modified={:?}",
        api.modified.iter().map(|m| &m.name).collect::<Vec<_>>()
    );
}

/// GitLab #33 の裏面: Eloquent リレーションメソッドの実削除は api.rm として検出する
/// (旧実装は old 側抽出でも entrypoint 除外され silent false negative だった)。
#[test]
fn detect_api_changes_php_eloquent_relation_removed_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "src/Models/VoiceLogSummaryEloquent.php",
                "<?php\n\nuse Illuminate\\Database\\Eloquent\\Relations\\HasOne;\n\nclass VoiceLogSummaryEloquent extends AbstractEloquent {\n    public function monitorLogs(): HasOne {\n        return $this->hasOne(MonitorLogEloquent::class, 'request_id', 'request_id');\n    }\n\n    public function keepMe() {\n        return 1;\n    }\n}\n",
            ),
            (
                "src/Repositories/VoiceLogSummaryRepositoryQuery.php",
                "<?php\n\nclass VoiceLogSummaryRepositoryQuery {\n    public function fetch($eloquent) {\n        $monitorLog = $eloquent->monitorLogs;\n        return $monitorLog;\n    }\n}\n",
            ),
        ],
        "base",
    );
    fs::write(
        repo.join("src/Models/VoiceLogSummaryEloquent.php"),
        "<?php\n\nclass VoiceLogSummaryEloquent extends AbstractEloquent {\n    public function keepMe() {\n        return 1;\n    }\n}\n",
    )
    .expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/Models/VoiceLogSummaryEloquent.php".to_string(),
        new_path: "src/Models/VoiceLogSummaryEloquent.php".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 13,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed
            .iter()
            .any(|s| s.name == "VoiceLogSummaryEloquent.monitorLogs"),
        "参照が残る Eloquent リレーションメソッドの削除は removed として報告する。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// Issue 2026-07-15-ts-add-refactor-delete-chain-api-rm-fp: クラスをファイルごと削除し
/// 呼び出し側を別クラスへ切替えた diff で、owner クラス (`GwsCalendarClient`) は参照 0 件で
/// removed_dead (informational) になるのに、メソッド (`GwsCalendarClient.listEvents`) は
/// bare name カウントが切替先クラスの同名メソッド参照を拾って removed (blocking) に残って
/// いた。owner 型が removed_dead なら member も追従して removed_dead へ移す。
#[test]
fn detect_api_changes_deleted_class_member_follows_dead_owner_to_removed_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let gws_src = "export class GwsCalendarClient {\n    async listEvents(day: string): Promise<string[]> {\n        return [day];\n    }\n}\n";
    git_commit_files(
        repo,
        &[
            ("src/services/gwsCalendar.ts", gws_src),
            (
                "src/services/googleCalendar.ts",
                "export class GoogleCalendarClient {\n    async listEvents(day: string): Promise<string[]> {\n        return [\"g:\" + day];\n    }\n}\n",
            ),
            (
                "src/index.ts",
                "import { GwsCalendarClient } from './services/gwsCalendar';\n\nexport async function main() {\n    const client = new GwsCalendarClient();\n    return client.listEvents(\"2026-07-15\");\n}\n",
            ),
        ],
        "base",
    );
    // gws 実装をファイルごと削除し、呼び出し側は google 実装へ切替
    std::fs::remove_file(repo.join("src/services/gwsCalendar.ts")).expect("rm");
    fs::write(
        repo.join("src/index.ts"),
        "import { GoogleCalendarClient } from './services/googleCalendar';\n\nexport async function main() {\n    const client = new GoogleCalendarClient();\n    return client.listEvents(\"2026-07-15\");\n}\n",
    )
    .expect("write");
    let diff_files = vec![
        crate::models::impact::DiffFile {
            old_path: "src/services/gwsCalendar.ts".to_string(),
            new_path: "/dev/null".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 5,
                new_start: 0,
                new_count: 0,
            }],
            deleted_old_source: Some(gws_src.as_bytes().to_vec()),
        },
        crate::models::impact::DiffFile {
            old_path: "src/index.ts".to_string(),
            new_path: "src/index.ts".to_string(),
            hunks: vec![crate::models::impact::HunkInfo {
                old_start: 1,
                old_count: 6,
                new_start: 1,
                new_count: 6,
            }],
            deleted_old_source: None,
        },
    ];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed_dead
            .iter()
            .any(|s| s.name == "GwsCalendarClient.listEvents"),
        "owner クラスが removed_dead なら member も removed_dead に追従すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        !api.removed
            .iter()
            .any(|s| s.name == "GwsCalendarClient.listEvents"),
        "member を blocking な removed に残さない。removed={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// 負ケース: owner クラス名への参照が新ツリーに残っている (owner が removed_kept) 場合、
/// member は従来どおり removed (blocking) に残す — owner 経由の到達経路が残り得るため。
#[test]
fn detect_api_changes_deleted_member_with_live_owner_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let gws_src = "export class GwsCalendarClient {\n    async listEvents(day: string): Promise<string[]> {\n        return [day];\n    }\n}\n";
    git_commit_files(
        repo,
        &[
            ("src/services/gwsCalendar.ts", gws_src),
            (
                "src/index.ts",
                "import { GwsCalendarClient } from './services/gwsCalendar';\n\nexport async function main() {\n    const client = new GwsCalendarClient();\n    return client.listEvents(\"2026-07-15\");\n}\n",
            ),
        ],
        "base",
    );
    // クラスファイルだけ削除し、呼び出し側 (owner 名への参照) は残したまま
    std::fs::remove_file(repo.join("src/services/gwsCalendar.ts")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/services/gwsCalendar.ts".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(gws_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed
            .iter()
            .any(|s| s.name == "GwsCalendarClient.listEvents")
            && api.removed.iter().any(|s| s.name == "GwsCalendarClient"),
        "owner への参照が残る削除は owner / member とも blocking な removed を維持する。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// 負ケース (codex レビュー指摘): owner 型の別定義が新ツリーに残る (定義 1・参照 0) 場合、
/// owner は第 1 パスで removed_dead に入るが型は生存しているため、member を removed_dead へ
/// 降格してはならない。partial class / open class / extension を模した構成で、削除ファイルの
/// `Svc` と同名の `Svc` が別ファイルに残り、削除メソッド名 `doWork` は別コードから参照される。
#[test]
fn detect_api_changes_deleted_member_with_surviving_owner_definition_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let gone_src = "export class Svc {\n    doWork(): void {}\n}\n";
    git_commit_files(
        repo,
        &[
            ("src/gone.ts", gone_src),
            // 同名 Svc の別定義 (新ツリーに残る = owner の def_count を 1 に押し上げる)。
            // Svc 名は誰からも参照されないため ref_count は 0。
            (
                "src/keep.ts",
                "export class Svc {\n    other(): void {}\n}\n",
            ),
            // 削除される member 名 `doWork` への参照だけを残す (owner Svc は参照しない)。
            (
                "src/consumer.ts",
                "export function run(r: any): void {\n    r.doWork();\n}\n",
            ),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("src/gone.ts")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/gone.ts".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(gone_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|s| s.name == "Svc.doWork"),
        "owner 型の別定義が新ツリーに残る場合、member は blocking な removed を維持すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// Issue 2026-07-19-bulk-subsystem-removal: 削除された bash 関数と同名のローカル関数が
/// 複数の残存スクリプトに定義され、参照がすべて各定義ファイル内で閉じている場合、bare
/// name カウントは def_count > 1 + ref_count > 0 で従来 blocking に残していた。参照の
/// 帰属確認 (同ファイル定義 = 削除ファイルが消えても未定義にならない) により
/// removed_dead (informational) へ降格する。
#[test]
fn detect_api_changes_bulk_removal_bash_same_name_local_functions_demoted_to_removed_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "#!/bin/bash\nusage() {\n  echo \"usage: deleted-tool\"\n}\nusage\n";
    git_commit_files(
        repo,
        &[
            ("scripts/deleted-tool.sh", deleted_src),
            (
                "scripts/keep-a.sh",
                "#!/bin/bash\nusage() {\n  echo \"usage: keep-a\"\n}\nusage\n",
            ),
            (
                "scripts/keep-b.sh",
                "#!/bin/bash\nusage() {\n  echo \"usage: keep-b\"\n}\nif [ -z \"$1\" ]; then usage >&2; fi\n",
            ),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("scripts/deleted-tool.sh")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "scripts/deleted-tool.sh".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed_dead.iter().any(|s| s.name == "usage"),
        "同名ローカル関数へ帰属確認できた削除は removed_dead に降格すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        !api.removed.iter().any(|s| s.name == "usage"),
        "blocking な removed に残さない。removed={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// Issue 2026-07-19-bulk-subsystem-removal: 削除された mjs export と同名の独立シンボルが
/// 残存し、参照が残存側への相対 import で束縛されている場合、bare name カウントは
/// 「削除シンボルへの残存参照」と誤認して blocking に残していた。import specifier の
/// 相対解決で残存定義ファイルへの帰属を証明し removed_dead へ降格する。
#[test]
fn detect_api_changes_bulk_removal_import_attributed_to_survivor_demoted_to_removed_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src =
        "export function loadEnvFiles(dir) {\n  return { dir };\n}\nloadEnvFiles(\".\");\n";
    git_commit_files(
        repo,
        &[
            ("plugins/setup.mjs", deleted_src),
            (
                "api/src/config.ts",
                "export function loadEnvFiles(baseDir = \".\", env = {}) {\n  return { baseDir, env };\n}\n",
            ),
            (
                "api/test/config.test.ts",
                "import { loadEnvFiles } from \"../src/config\";\n\nexport function testConfig() {\n  return loadEnvFiles(\".\", {});\n}\n",
            ),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("plugins/setup.mjs")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "plugins/setup.mjs".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed_dead.iter().any(|s| s.name == "loadEnvFiles"),
        "残存定義への import で帰属確認できた削除は removed_dead に降格すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
    assert!(
        !api.removed.iter().any(|s| s.name == "loadEnvFiles"),
        "blocking な removed に残さない。removed={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// Issue 2026-08-06-api-rm-atomic-module-deletion: Python モジュールを呼び出し元ごと
/// アトミックに削除しても、polyglot リポジトリでは無関係な他言語の同名シンボル
/// (PHP / JS / C の `search`) への参照が bare name カウントに混入し、blocking な
/// api.rm に残っていた。参照ファイルの言語から削除ファイルの言語へ識別子束縛の経路が
/// 無いことを証明し、残る同一言語の参照も属性アクセス (`re.search`) と判れば
/// removed_dead (informational) へ降格する。
#[test]
fn detect_api_changes_atomic_python_module_deletion_demoted_to_removed_dead() {
    let (removed, removed_dead) = removed_names_after_atomic_python_module_deletion(
        "import re\n\ndef check(pw):\n    return re.search(\"x\", pw)\n",
    );
    assert!(
        removed_dead.iter().any(|n| n == "search"),
        "他言語の同名参照と stdlib 属性アクセスしか残らない削除は removed_dead へ降格すべき。removed={removed:?} removed_dead={removed_dead:?}"
    );
    assert!(
        !removed.iter().any(|n| n == "search"),
        "blocking な removed に残さない。removed={removed:?}"
    );
}

/// 上の降格は fail-closed を保つ。同一言語 (Python) から削除モジュールへ到達する参照が
/// 残っていれば、他言語ノイズが同居していても blocking な api.rm に残す。
#[test]
fn detect_api_changes_atomic_python_module_deletion_keeps_blocking_on_residual_python_ref() {
    // モジュール修飾呼び出し: レシーバ `core` が削除モジュール名と一致する
    let (removed, removed_dead) = removed_names_after_atomic_python_module_deletion(
        "import core\n\ndef check(pw):\n    return core.search(pw)\n",
    );
    assert!(
        removed.iter().any(|n| n == "search"),
        "削除モジュールを修飾した属性アクセスが残る場合は blocking を維持すべき。removed={removed:?} removed_dead={removed_dead:?}"
    );

    // bare 呼び出し: 属性アクセスではないので証明できない
    let (removed, removed_dead) = removed_names_after_atomic_python_module_deletion(
        "from core import search\n\ndef check(pw):\n    return search(pw)\n",
    );
    assert!(
        removed.iter().any(|n| n == "search"),
        "削除モジュールからの import + bare 呼び出しが残る場合は blocking を維持すべき。removed={removed:?} removed_dead={removed_dead:?}"
    );
}

/// 負ケース: 参照ファイルの import specifier が削除ファイル自身に解決される場合は、
/// 同名の残存定義があっても破壊的削除として blocking な removed を維持する。
#[test]
fn detect_api_changes_removed_function_imported_from_deleted_file_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "export function doWork() {\n  return 1;\n}\n";
    git_commit_files(
        repo,
        &[
            ("src/deleted.ts", deleted_src),
            (
                "src/keep.ts",
                "export function doWork() {\n  return 2;\n}\n",
            ),
            (
                "src/caller.ts",
                "import { doWork } from \"./deleted\";\n\nexport function run() {\n  return doWork();\n}\n",
            ),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("src/deleted.ts")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/deleted.ts".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|s| s.name == "doWork"),
        "削除ファイルへの import が残る削除は blocking な removed を維持すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

/// 負ケース: 参照スクリプト自身に同名関数が定義されていても、リテラル `source` が
/// 削除ファイルを指している (削除実装への明示依存が残る) 場合は blocking を維持する。
#[test]
fn detect_api_changes_bash_literal_source_of_deleted_file_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    let deleted_src = "#!/bin/bash\nhelper() {\n  echo \"deleted helper\"\n}\n";
    git_commit_files(
        repo,
        &[
            ("src/deleted-lib.sh", deleted_src),
            (
                "src/runner.sh",
                "#!/bin/bash\nhelper() {\n  echo \"local fallback\"\n}\nsource ./deleted-lib.sh\nhelper\n",
            ),
        ],
        "base",
    );
    std::fs::remove_file(repo.join("src/deleted-lib.sh")).expect("rm");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/deleted-lib.sh".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 4,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: Some(deleted_src.as_bytes().to_vec()),
    }];
    let api = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api.removed.iter().any(|s| s.name == "helper"),
        "削除ファイルをリテラル source する参照が残る削除は blocking を維持すべき。removed={:?} removed_dead={:?}",
        api.removed.iter().map(|s| &s.name).collect::<Vec<_>>(),
        api.removed_dead.iter().map(|s| &s.name).collect::<Vec<_>>()
    );
}

#[test]
fn detect_api_changes_skips_removed_when_no_old_source_available() {
    // `git show base:old_path` が失敗し、かつ deleted_old_source も無い場合は
    // 従来通り何も報告しない (false positive を出さない)。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(repo, &[("README.md", "# repo\n")], "initial");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "src/old.py".to_string(),
        new_path: "/dev/null".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 2,
            new_start: 0,
            new_count: 0,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    assert!(
        api_changes.removed.is_empty(),
        "旧ソース取得不能時は removed に出すべきではない"
    );
}

#[test]
fn detect_api_changes_python_property_to_field_replacement_is_not_removed() {
    // 報告再現: Python の `@property def x(self) -> str` を `@dataclass` フィールド
    // `x: str` に置き換えると、`obj.x` 属性アクセス API は維持されるため
    // `api.rm` ではなく `property_to_field` カテゴリに分類されるべき。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let old_content = "\
from dataclasses import dataclass
from urllib.parse import urlparse


@dataclass
class ReviewConfig:
    project_url: str

    @property
    def gitlab_base_url(self) -> str:
        parsed = urlparse(self.project_url)
        return f\"{parsed.scheme}://{parsed.netloc}\"
";
    git_commit_files(repo, &[("scripts/review_mr.py", old_content)], "initial");

    let new_content = "\
from dataclasses import dataclass


@dataclass
class ReviewConfig:
    project_url: str
    gitlab_base_url: str
";
    fs::write(repo.join("scripts/review_mr.py"), new_content).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "scripts/review_mr.py".to_string(),
        new_path: "scripts/review_mr.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 12,
            new_start: 1,
            new_count: 7,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_names: std::collections::HashSet<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed_names.contains(&"ReviewConfig.gitlab_base_url"),
        "@property → dataclass field 置き換えは api.rm に残らないべき。got: {removed_names:?}"
    );

    let p2f_names: Vec<&str> = api_changes
        .property_to_field
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert!(
        p2f_names.contains(&"ReviewConfig.gitlab_base_url"),
        "@property → dataclass field 置き換えは property_to_field に積まれるべき。got: {p2f_names:?}"
    );
}

#[test]
fn detect_api_changes_python_property_removed_without_field_remains_removed() {
    // 安全網: クラスから @property を削除し、対応するフィールドも追加しない場合は
    // 通常通り api.rm として残るべき。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let old_content = "\
from dataclasses import dataclass


@dataclass
class Foo:
    name: str

    @property
    def computed(self) -> str:
        return self.name.upper()
";
    git_commit_files(repo, &[("foo.py", old_content)], "initial");

    let new_content = "\
from dataclasses import dataclass


@dataclass
class Foo:
    name: str
";
    fs::write(repo.join("foo.py"), new_content).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "foo.py".to_string(),
        new_path: "foo.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 10,
            new_start: 1,
            new_count: 6,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_names: std::collections::HashSet<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed_names.contains(&"Foo.computed"),
        "対応 field が無い @property 削除は api.rm に残るべき。got: {removed_names:?}"
    );
    assert!(
        api_changes.property_to_field.is_empty(),
        "対応 field が無い場合は property_to_field に積まれないべき。got: {:?}",
        api_changes.property_to_field
    );
}

/// 他ファイルから参照されていない exported シンボルを削除した場合、
/// `removed` ではなく `removed_dead` カテゴリに振り分けられること
/// (Issue 2026-05-28-meet-virtual-you-gemini-multi-select 対応)。
/// HEAD ツリーで参照 0 件 = repo 内 dead removal を informational として提示。
#[test]
fn detect_api_changes_unreferenced_removal_goes_to_removed_dead_not_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // foo / bar 両方を定義。caller なし (dead-code 想定)。
    git_commit_files(
        repo,
        &[("mod.py", "def foo():\n    pass\n\ndef bar():\n    pass\n")],
        "initial",
    );
    // bar を削除 (HEAD で bar への参照は 0 件)
    fs::write(repo.join("mod.py"), "def foo():\n    pass\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "mod.py".to_string(),
        new_path: "mod.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_dead_names: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_names: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed_dead_names.contains(&"bar"),
        "HEAD で参照 0 件の削除は removed_dead に振り分けられるべき。got removed_dead: {removed_dead_names:?}, removed: {removed_names:?}"
    );
    assert!(
        !removed_names.contains(&"bar"),
        "removed_dead に振り分けられた symbol は removed には残ってはならない。got: {removed_names:?}"
    );
}

/// HEAD ツリーで他ファイルから参照されているシンボル (alive) の削除は、
/// `removed_dead` ではなく `removed` に残ること (副作用回帰防止)。
/// 「破壊的削除」と「dead-code 整理」の区別が機能していることを確認。
#[test]
fn detect_api_changes_referenced_removal_stays_in_removed_not_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // foo / bar を定義。caller.py で bar を参照 (alive)。
    git_commit_files(
        repo,
        &[
            ("mod.py", "def foo():\n    pass\n\ndef bar():\n    pass\n"),
            ("caller.py", "from mod import bar\nbar()\n"),
        ],
        "initial",
    );
    // bar を削除 (caller.py はそのままで bar への参照を維持 = 破壊的削除)
    fs::write(repo.join("mod.py"), "def foo():\n    pass\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "mod.py".to_string(),
        new_path: "mod.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_names: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_dead_names: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed_names.contains(&"bar"),
        "HEAD で参照ありのシンボル削除は removed (破壊的削除) に残るべき。got removed: {removed_names:?}, removed_dead: {removed_dead_names:?}"
    );
    assert!(
        !removed_dead_names.contains(&"bar"),
        "参照ありの削除は removed_dead に振り分けてはならない。got: {removed_dead_names:?}"
    );
}

/// 削除した interface `Config` の唯一の HEAD 参照が外部パッケージ (tailwindcss) の同名
/// import 由来なら、別モジュールの型として参照カウントから除外し api.rm ではなく
/// api.rm_dead に振り分ける。(レポート 2026-06-03-extension-task-only-cleanup の再現)
#[test]
fn detect_api_changes_removed_symbol_with_external_import_same_name_is_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "package.json",
                "{\n  \"devDependencies\": { \"tailwindcss\": \"^3.4.0\" }\n}\n",
            ),
            (
                "lib/config.ts",
                "export interface Config {\n  url: string;\n}\nexport function getConfig(): Config {\n  return { url: '' };\n}\n",
            ),
            (
                "tailwind.config.ts",
                "import type { Config } from \"tailwindcss\";\nexport default {} satisfies Config;\n",
            ),
        ],
        "initial",
    );
    // lib/config.ts から Config / getConfig を削除 (tailwind.config.ts は無関係な別 Config)
    fs::write(repo.join("lib/config.ts"), "export const VERSION = '1';\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib/config.ts".to_string(),
        new_path: "lib/config.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 6,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_dead: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed.contains(&"Config"),
        "外部 import (tailwindcss) の同名 Config は参照に数えず、Config は api.rm に出ない。got removed: {removed:?}"
    );
    assert!(
        removed_dead.contains(&"Config"),
        "Config は removed_dead に振り分けられるべき。got removed_dead: {removed_dead:?}"
    );
}

/// 削除シンボルが内部 (相対 import) で実際に参照されている場合は、外部 import 除外の
/// 対象外で api.rm (破壊的削除) を維持する (false negative 防止)。
#[test]
fn detect_api_changes_removed_symbol_with_internal_relative_reference_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "package.json",
                "{\n  \"devDependencies\": { \"tailwindcss\": \"^3.4.0\" }\n}\n",
            ),
            (
                "lib/config.ts",
                "export interface Config {\n  url: string;\n}\n",
            ),
            (
                "app.ts",
                "import type { Config } from \"./lib/config\";\nexport const c: Config = { url: '' };\n",
            ),
        ],
        "initial",
    );
    // Config を削除するが app.ts は相対 import で参照を維持 (破壊的削除)
    fs::write(repo.join("lib/config.ts"), "export const X = 1;\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib/config.ts".to_string(),
        new_path: "lib/config.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.contains(&"Config"),
        "相対 import で内部参照される Config は api.rm を維持すべき。got removed: {removed:?}"
    );
}

/// 外部 alias import (`import { Config as TailwindConfig } from "tailwindcss"`、local は
/// TailwindConfig) と内部相対 import の同名 Config が同一ファイルに共存する場合、削除した
/// Config は内部参照が残るので api.rm を維持する (codex 指摘: 逆 alias false negative 防止)。
#[test]
fn detect_api_changes_removed_symbol_external_alias_with_internal_reference_stays_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "package.json",
                "{\n  \"devDependencies\": { \"tailwindcss\": \"^3.4.0\" }\n}\n",
            ),
            (
                "lib/config.ts",
                "export interface Config {\n  url: string;\n}\n",
            ),
            (
                "app.ts",
                "import { Config as TailwindConfig } from \"tailwindcss\";\nimport type { Config } from \"./lib/config\";\nexport const c: Config = { url: '' };\nexport const t = {} as TailwindConfig;\n",
            ),
        ],
        "initial",
    );
    fs::write(repo.join("lib/config.ts"), "export const X = 1;\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib/config.ts".to_string(),
        new_path: "lib/config.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.contains(&"Config"),
        "外部 alias import (Config as TailwindConfig) があっても内部相対 import の Config 参照が残れば api.rm 維持。got removed: {removed:?}"
    );
}

/// 削除した内部 Config に実参照がなく、別ファイルに外部 alias import の import 元名
/// `Config` だけが残る場合 (`import { Config as TailwindConfig } from "tailwindcss"`、
/// Config 自体は未使用) は、import 元名を別モジュールの export として除外し removed_dead に
/// 振り分ける (codex 指摘: alias-only false positive 防止)。
#[test]
fn detect_api_changes_removed_symbol_external_alias_only_import_name_is_dead() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[
            (
                "package.json",
                "{\n  \"devDependencies\": { \"tailwindcss\": \"^3.4.0\" }\n}\n",
            ),
            (
                "lib/config.ts",
                "export interface Config {\n  url: string;\n}\n",
            ),
            (
                "app.ts",
                "import { Config as TailwindConfig } from \"tailwindcss\";\nexport const t = {} as TailwindConfig;\n",
            ),
        ],
        "initial",
    );
    fs::write(repo.join("lib/config.ts"), "export const X = 1;\n").expect("write");
    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib/config.ts".to_string(),
        new_path: "lib/config.ts".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 1,
        }],
        deleted_old_source: None,
    }];
    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_dead: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed.contains(&"Config"),
        "外部 alias import の import 元名のみの Config は api.rm に出ない。got removed: {removed:?}"
    );
    assert!(
        removed_dead.contains(&"Config"),
        "Config は removed_dead に振り分けられるべき。got removed_dead: {removed_dead:?}"
    );
}

/// detect_api_changes の早期 continue 経路 (closed-in-diff for api.rm) でも
/// qualname 対応が機能すること (codex 2 回目指摘への回帰防止)。
/// 「qualname method 削除 + 同ファイルに新規関数追加 + 外部 caller 残存」のケースで
/// removed_dead に誤分類されず removed に残る。
#[test]
fn detect_api_changes_qualname_method_with_inline_addition_and_external_caller_stays_in_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // 旧: Foo.bar あり、caller.py で Foo().bar() を参照
    git_commit_files(
        repo,
        &[
            (
                "foo.py",
                "class Foo:\n    def bar(self):\n        return 1\n",
            ),
            (
                "caller.py",
                "from foo import Foo\n\ndef use():\n    return Foo().bar()\n",
            ),
        ],
        "initial",
    );
    // 新: bar を削除し、同ファイルに新規関数 helper を追加
    // → new_symbols_in_current_file が空でないので closed-in-diff 早期 continue
    //   経路に入る (line 1836 周辺)
    fs::write(
        repo.join("foo.py"),
        "class Foo:\n    pass\n\n\ndef helper():\n    return 0\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "foo.py".to_string(),
        new_path: "foo.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 5,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_names: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_dead_names: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    // 早期 continue 経路でも bare name + def_count 判定が効く
    assert!(
        removed_names.iter().any(|n| n.contains("bar")),
        "qualname method 削除 + 同ファイル新規追加 + 外部 caller 残存は removed に残るべき。got removed: {removed_names:?}, removed_dead: {removed_dead_names:?}"
    );
    assert!(
        !removed_dead_names.iter().any(|n| n.contains("bar")),
        "上記ケースを removed_dead に振り分けてはならない。got: {removed_dead_names:?}"
    );
}

/// qualname (`Container.method`) 形式の class method 削除でも、別ファイルから
/// bare name で参照されていれば破壊的削除として `removed` に残ること
/// (codex 指摘 1: qualname 誤分類への回帰防止)。
#[test]
fn detect_api_changes_qualname_method_with_external_caller_stays_in_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    // class Foo の method bar を削除するが、caller.py で Foo().bar() を呼んでいる
    git_commit_files(
        repo,
        &[
            (
                "foo.py",
                "class Foo:\n    def bar(self):\n        return 1\n",
            ),
            (
                "caller.py",
                "from foo import Foo\n\ndef use():\n    return Foo().bar()\n",
            ),
        ],
        "initial",
    );
    // method bar を削除
    fs::write(repo.join("foo.py"), "class Foo:\n    pass\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "foo.py".to_string(),
        new_path: "foo.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 3,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed_names: Vec<&str> = api_changes
        .removed
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    let removed_dead_names: Vec<&str> = api_changes
        .removed_dead
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    // bare name 'bar' で検索すると caller.py の Foo().bar() で参照あり
    // qualname を bare で正規化していなければ常に refs 0 件で removed_dead に
    // 誤分類される
    assert!(
        removed_names.iter().any(|n| n.contains("bar")),
        "外部 caller がいる qualname method 削除は removed に残るべき。got removed: {removed_names:?}, removed_dead: {removed_dead_names:?}"
    );
    assert!(
        !removed_dead_names.iter().any(|n| n.contains("bar")),
        "外部 caller がいる qualname method 削除を removed_dead に振り分けてはならない。got: {removed_dead_names:?}"
    );
}

#[test]
fn detect_api_changes_still_detects_genuine_removal() {
    // リネームではなく純粋に関数を削除した場合は api.rm が発報される。
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(
        repo,
        &[("mod.py", "def foo():\n    pass\n\ndef bar():\n    pass\n")],
        "initial",
    );
    // bar を削除
    fs::write(repo.join("mod.py"), "def foo():\n    pass\n").expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "mod.py".to_string(),
        new_path: "mod.py".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.contains(&"bar"),
        "純粋な関数削除は api.rm として検出されるべき。got: {removed:?}"
    );
}

#[test]
fn detect_api_changes_cpp_h_header_inheritance_redefinition_is_modified_not_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    git_commit_files(
        repo,
        &[(
            "error.h",
            "template <typename T> struct BaseError {};\n\
struct OmnisError {\n\
    void set_error(int code);\n\
    int code;\n\
};\n",
        )],
        "initial",
    );
    fs::write(
        repo.join("error.h"),
        "template <typename T> struct BaseError {};\n\
struct OmnisError : public BaseError<OmnisError> {};\n",
    )
    .expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "error.h".to_string(),
        new_path: "error.h".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 5,
            new_start: 1,
            new_count: 2,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);
    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    let modified: Vec<&str> = api_changes
        .modified
        .iter()
        .chain(api_changes.modified_closed_in_diff.iter())
        .map(|s| s.name.as_str())
        .collect();

    assert!(
        !removed.contains(&"OmnisError"),
        ".h の C++ 継承付き再定義を api.rm にしてはならない。removed={removed:?}, modified={modified:?}"
    );
    assert!(
        modified.contains(&"OmnisError"),
        "継承付き再定義は削除ではなく変更として扱うべき。modified={modified:?}"
    );
}

/// Bash の未 export 関数を caller ごと同一 diff 内で削除した場合は api.rm に出さない。
/// (レポート 2026-05-01-bash-private-function-removal-flagged-as-api-rm.md の再現)
/// `dump_shallow_state` / `boundary_is_old_enough` のように、CLI スクリプト内の
/// クロージャ的なヘルパー関数を、同 diff 内で全 caller と一緒に削除したとき、
/// `export -f` が無いなら外部 API 面ではないため除外する必要がある。
#[test]
fn detect_api_changes_bash_pure_removal_without_export_is_not_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "#!/usr/bin/env bash\n\
dump_shallow_state() {\n    echo state\n}\n\n\
boundary_is_old_enough() {\n    return 0\n}\n\n\
main() {\n    dump_shallow_state\n    while ! boundary_is_old_enough; do\n        sleep 1\n    done\n}\nmain\n";
    git_commit_files(repo, &[("qa_diff.sh", before)], "initial");

    let after = "#!/usr/bin/env bash\n\
main() {\n    echo done\n}\nmain\n";
    fs::write(repo.join("qa_diff.sh"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "qa_diff.sh".to_string(),
        new_path: "qa_diff.sh".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 14,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        !removed.contains(&"dump_shallow_state"),
        "未 export な bash 関数を caller ごと同一 diff で削除した場合は api.rm に出してはならない。got: {removed:?}"
    );
    assert!(
        !removed.contains(&"boundary_is_old_enough"),
        "未 export な bash 関数を caller ごと同一 diff で削除した場合は api.rm に出してはならない。got: {removed:?}"
    );
}

/// Bash で `export -f <name>` されている関数の削除は api.rm に残す。
/// 他リポジトリ消費者向け API として残す必要があるため false negative を避ける。
#[test]
fn detect_api_changes_bash_exported_function_removal_is_still_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "#!/usr/bin/env bash\n\
public_helper() {\n    echo public\n}\nexport -f public_helper\n\n\
main() {\n    echo hi\n}\nmain\n";
    git_commit_files(repo, &[("lib.sh", before)], "initial");

    let after = "#!/usr/bin/env bash\n\
main() {\n    echo hi\n}\nmain\n";
    fs::write(repo.join("lib.sh"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "lib.sh".to_string(),
        new_path: "lib.sh".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 8,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.contains(&"public_helper"),
        "`export -f` された bash 関数の削除は api.rm に残すべき。got: {removed:?}"
    );
}

/// Bash の未 export 関数でも、他ファイルから参照されているなら api.rm に残す。
/// `source common.sh` 経由で他スクリプトが呼ぶケースを考慮し、
/// cross-file refs が 1 件以上なら除外しない。
#[test]
fn detect_api_changes_bash_unexported_function_with_cross_file_ref_is_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);

    let before = "#!/usr/bin/env bash\n\
shared_helper() {\n    echo shared\n}\n\n\
main() {\n    shared_helper\n}\nmain\n";
    let consumer = "#!/usr/bin/env bash\n\
source ./common.sh\nshared_helper\n";
    git_commit_files(
        repo,
        &[("common.sh", before), ("consumer.sh", consumer)],
        "initial",
    );

    let after = "#!/usr/bin/env bash\n\
main() {\n    echo hi\n}\nmain\n";
    fs::write(repo.join("common.sh"), after).expect("write");

    let diff_files = vec![crate::models::impact::DiffFile {
        old_path: "common.sh".to_string(),
        new_path: "common.sh".to_string(),
        hunks: vec![crate::models::impact::HunkInfo {
            old_start: 1,
            old_count: 7,
            new_start: 1,
            new_count: 4,
        }],
        deleted_old_source: None,
    }];

    let api_changes = detect_api_changes(repo.to_str().expect("utf-8 path"), "HEAD", &diff_files);

    let removed: Vec<&str> = api_changes
        .removed
        .iter()
        .chain(api_changes.removed_dead.iter())
        .map(|s| s.name.as_str())
        .collect();
    assert!(
        removed.contains(&"shared_helper"),
        "他ファイルから source 経由で参照されている bash 関数の削除は api.rm に残すべき。got: {removed:?}"
    );
}
