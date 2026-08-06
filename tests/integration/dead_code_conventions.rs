//! dead-code のフレームワーク規約除外 (テストランナー / 自動生成 / DI) の統合テスト。

#[allow(unused_imports)]
use super::support::*;
#[allow(unused_imports)]
use std::process::{Command, Stdio};

/// GitLab issue #24 回帰: Flyway の Java マイグレーションクラス
/// (`extends BaseJavaMigration`) は dead-code 検出から除外される。
#[test]
fn dead_code_excludes_flyway_java_migration() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("app/migration/src/main/java/db/migration")).unwrap();

    // Flyway migration クラス (extends BaseJavaMigration) ─ 除外対象
    let flyway_class = "package db.migration;\n\
                        import org.flywaydb.core.api.migration.BaseJavaMigration;\n\
                        import org.flywaydb.core.api.migration.Context;\n\
                        public class V2021_01_02__Zipcode extends BaseJavaMigration {\n\
                            public void migrate(Context context) throws Exception {}\n\
                        }\n";
    std::fs::write(
        root.join("app/migration/src/main/java/db/migration/V2021_01_02__Zipcode.java"),
        flyway_class,
    )
    .unwrap();

    // 通常の Java クラス (Flyway 非継承) ─ 直接参照なしなので dead に残るべき
    let regular_class = "package app.util;\n\
                         public class OrphanService {\n\
                             public void noOp() {}\n\
                         }\n";
    std::fs::create_dir_all(root.join("app/util")).unwrap();
    std::fs::write(root.join("app/util/OrphanService.java"), regular_class).unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead_names
            .iter()
            .any(|n| n.contains("V2021_01_02__Zipcode")),
        "Flyway BaseJavaMigration 継承クラスは framework entrypoint として除外されるべき: {dead_names:?}"
    );
    assert!(
        dead_names.iter().any(|n| n.contains("OrphanService")),
        "Flyway を継承しない通常の Java クラスは dead として残るべき (回帰担保): {dead_names:?}"
    );
}

/// GitLab issue #21: Laravel Eloquent リレーション戻り型 (`BelongsTo` 等) を持つ public
/// method は dead-code 検出から除外される。`->with('x')` 文字列 / magic property 経由で
/// Eloquent が呼ぶため、静的 caller が 0 件でも dead ではない。
#[test]
fn dead_code_excludes_laravel_eloquent_relation_methods() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("app/Models")).unwrap();
    let model_src = "<?php
namespace App\\Models;
use Illuminate\\Database\\Eloquent\\Relations\\BelongsTo;
use Illuminate\\Database\\Eloquent\\Relations\\HasMany;
class QueueModel extends Model {
    public function omataseGuidance(): BelongsTo { return $this->belongsTo(Guidance::class); }
    public function tags(): HasMany { return $this->hasMany(Tag::class); }
    public function plainHelper(): string { return ''; }
}
";
    std::fs::write(root.join("app/Models/QueueModel.php"), model_src).unwrap();
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    for name in ["omataseGuidance", "tags"] {
        assert!(
            !dead_names.iter().any(|n| n.contains(name)),
            "Eloquent relation メソッド `{name}` は除外されるべき: {dead_names:?}"
        );
    }
    // 戻り型が string の通常メソッドは除外対象外 (回帰担保)。
    assert!(
        dead_names.iter().any(|n| n.contains("plainHelper")),
        "Eloquent relation でない自前 method は dead として残るべき (回帰担保): {dead_names:?}"
    );
}

/// GitLab issue #22: `implements CanResetPasswordContract` クラスの
/// `getEmailForPasswordReset` / `sendPasswordResetNotification` は Laravel framework
/// (PasswordBroker / Notification) が contract 経由で呼ぶため dead から除外する。
#[test]
fn dead_code_excludes_laravel_can_reset_password_contract_methods() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("app/Models")).unwrap();
    let model_src = "<?php
namespace App\\Models;
class AccountEloquent extends Model implements CanResetPasswordContract {
    public function getEmailForPasswordReset(): string { return $this->email; }
    public function sendPasswordResetNotification($token): void {}
    public function someOtherMethod(): string { return ''; }
}
";
    std::fs::write(root.join("app/Models/AccountEloquent.php"), model_src).unwrap();
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    for name in ["getEmailForPasswordReset", "sendPasswordResetNotification"] {
        assert!(
            !dead_names.iter().any(|n| n.contains(name)),
            "CanResetPasswordContract 実装メソッド `{name}` は除外されるべき: {dead_names:?}"
        );
    }
    // contract 実装ではない自前 method は除外対象外 (回帰担保)。
    assert!(
        dead_names.iter().any(|n| n.contains("someOtherMethod")),
        "contract 実装ではない自前 method は dead として残るべき (回帰担保): {dead_names:?}"
    );
}

/// GitLab issue #20: `implements ControlValueAccessor` の Angular 装飾クラスの 4 規約
/// メソッド (writeValue / registerOnChange / registerOnTouched / setDisabledState) は
/// dead-code 検出から除外される。Angular Forms ランタイムが NG_VALUE_ACCESSOR provider
/// 経由で ngModel/formControl バインド時に呼ぶため、静的 caller が 0 件でも dead ではない。
#[test]
fn dead_code_excludes_angular_control_value_accessor_methods() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/app/shared")).unwrap();
    let cva_src = "\
import { Directive } from '@angular/core';
import { ControlValueAccessor } from '@angular/forms';
@Directive()
export abstract class AbstractBaseControl implements ControlValueAccessor {
    writeValue(obj: any) {}
    registerOnChange(fn: any) {}
    registerOnTouched(fn: any) {}
    setDisabledState(isDisabled: boolean) {}
    customHelper(): void {}
}
";
    std::fs::write(root.join("src/app/shared/abstract.ts"), cva_src).unwrap();
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    for name in [
        "writeValue",
        "registerOnChange",
        "registerOnTouched",
        "setDisabledState",
    ] {
        assert!(
            !dead_names.iter().any(|n| n.contains(name)),
            "ControlValueAccessor 規約メソッド `{name}` は除外されるべき: {dead_names:?}"
        );
    }
    // CVA 規約名でない自前メソッドは除外対象外 (回帰担保)。
    assert!(
        dead_names.iter().any(|n| n.contains("customHelper")),
        "CVA 規約外の自前メソッドは dead として残るべき (回帰担保): {dead_names:?}"
    );
}

/// GitLab issue #25: `@Directive()` / `@Component` 装飾の無い abstract 基底クラスでも
/// `implements ControlValueAccessor` を伴えば CVA 規約メソッドを dead から除外する。
/// 具象子クラスが別ファイルで `@Component({...NG_VALUE_ACCESSOR provider...})` を宣言し
/// `extends` する Angular の慣用パターン (装飾なし AbstractBaseControl 系) に対応。
#[test]
fn dead_code_excludes_cva_methods_in_undecorated_abstract_base() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/app/shared")).unwrap();
    // 抽象基底クラス: Angular デコレータ無し、`implements ControlValueAccessor` のみ
    let base_src = "\
import { ControlValueAccessor } from '@angular/forms';
export abstract class AbstractBaseControlComponent implements ControlValueAccessor {
    writeValue(obj: any) {}
    registerOnChange(fn: any) {}
    registerOnTouched(fn: any) {}
    customHelper() {}
}
";
    std::fs::write(
        root.join("src/app/shared/abstract.base.control.component.ts"),
        base_src,
    )
    .unwrap();
    // 具象子クラス: 別ファイルで @Component + NG_VALUE_ACCESSOR provider、extends 基底
    let concrete_src = "\
import { Component } from '@angular/core';
import { NG_VALUE_ACCESSOR } from '@angular/forms';
import { AbstractBaseControlComponent } from './abstract.base.control.component';
@Component({
    selector: 'bz-input-control',
    template: '<input />',
    providers: [{ provide: NG_VALUE_ACCESSOR, useExisting: InputControlComponent, multi: true }],
})
export class InputControlComponent extends AbstractBaseControlComponent {}
";
    std::fs::write(
        root.join("src/app/shared/input-control.component.ts"),
        concrete_src,
    )
    .unwrap();
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    for name in ["writeValue", "registerOnChange", "registerOnTouched"] {
        assert!(
            !dead_names.iter().any(|n| n.contains(name)),
            "装飾なし abstract 基底クラスの CVA 規約メソッド `{name}` は除外されるべき: {dead_names:?}"
        );
    }
    // CVA 規約外の自前メソッド customHelper は除外対象外 (装飾なし基底クラスでも誤抑止しない、回帰担保)。
    assert!(
        dead_names.iter().any(|n| n.contains("customHelper")),
        "CVA 規約外の自前メソッドは dead として残るべき (装飾なし基底でも誤抑止しない、回帰担保): {dead_names:?}"
    );
}

/// GitLab issue #23: `@HostListener` 付きメソッドは Angular ランタイムがイベント発火時に
/// 呼ぶため、静的 caller 0 件でも dead から除外する。
#[test]
fn dead_code_excludes_angular_host_listener_method() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/app")).unwrap();
    let component_src = "\
import { Component, HostListener } from '@angular/core';
@Component({ template: '' })
export class AppComponent {
    @HostListener('window:beforeunload', ['$event'])
    beforeUnloadHandler() {}
    plainHelper() {}
}
";
    std::fs::write(root.join("src/app/app.component.ts"), component_src).unwrap();
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead_names.iter().any(|n| n.contains("beforeUnloadHandler")),
        "@HostListener 付きメソッドは dead から除外されるべき: {dead_names:?}"
    );
    // member decorator が無い通常メソッドは除外対象外 (回帰担保)。
    assert!(
        dead_names.iter().any(|n| n.contains("plainHelper")),
        "member decorator のない通常メソッドは dead として残るべき (回帰担保): {dead_names:?}"
    );
}

#[test]
fn dead_code_excludes_angular_component_referenced_by_selector_tag() {
    // GitLab #26: standalone component class は TS 上の直接参照が無くても、
    // template の custom element tag (`<bz-popup>`) が selector に一致すれば live。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::write(
        root.join("src/app/popup.component.ts"),
        "\
import { Component } from '@angular/core';
@Component({ selector: 'bz-popup', template: '' })
export class PopupComponent {}
",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/unused.component.ts"),
        "\
import { Component } from '@angular/core';
@Component({ selector: 'bz-unused', template: '' })
export class UnusedComponent {}
",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/host.component.html"),
        "<section><bz-popup></bz-popup></section>\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead_names.iter().any(|n| n.contains("PopupComponent")),
        "template selector `<bz-popup>` に対応する component class は live になるべき: {dead_names:?}"
    );
    assert!(
        dead_names.iter().any(|n| n.contains("UnusedComponent")),
        "未使用 selector の component は dead として残るべき: {dead_names:?}"
    );
}

#[test]
fn dead_code_excludes_angular_provider_option_callback() {
    // GitLab #26: RECAPTCHA_LOADER_OPTIONS の useValue callback は ng-recaptcha 側から
    // 呼ばれるため、直接 caller が 0 件でも dead ではない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::write(
        root.join("src/app/app.component.ts"),
        "\
import { Component } from '@angular/core';
import { RECAPTCHA_LOADER_OPTIONS } from 'ng-recaptcha-2';
@Component({
  template: '',
  providers: [{
    provide: RECAPTCHA_LOADER_OPTIONS,
    useValue: {
      onBeforeLoad(url: URL) { return url; },
      otherCallback() { return 1; },
    }
  }]
})
export class AppComponent {}
",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead_names.iter().any(|n| n.contains("onBeforeLoad")),
        "RECAPTCHA_LOADER_OPTIONS の onBeforeLoad callback は dead から除外されるべき: {dead_names:?}"
    );
    assert!(
        dead_names.iter().any(|n| n.contains("otherCallback")),
        "allowlist 外の provider object method は dead として残るべき: {dead_names:?}"
    );
}

#[test]
fn dead_code_keeps_c_struct_live_when_used_as_type() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("sample.c"),
        "\
struct text_server_data {
    int command;
};

struct unused_data {
    int value;
};

struct forward_only;
struct forward_only {
    int y;
};

typedef struct forward_alias forward_alias;
struct forward_alias {
    int z;
};

int parse_header(struct text_server_data* header) {
    struct text_server_data local;
    local.command = header->command;
    return local.command;
}
",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead_names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !dead_names.iter().any(|n| n == "text_server_data"),
        "型として使われている struct は dead ではない: {dead_names:?}"
    );
    assert!(
        dead_names.iter().any(|n| n == "unused_data"),
        "未使用 struct は dead として残るべき: {dead_names:?}"
    );
    assert!(
        dead_names.iter().any(|n| n == "forward_only"),
        "forward declaration だけでは body 付き struct を live にしない: {dead_names:?}"
    );
    assert!(
        dead_names.iter().any(|n| n == "forward_alias"),
        "typedef forward declaration だけでは body 付き struct を live にしない: {dead_names:?}"
    );
}

#[test]
fn dead_code_skips_linguist_generated_files() {
    // .gitattributes で linguist-generated 指定されたファイルは
    // dead-code 検出から除外する（tree-sitter parser.c 等の生成物対応）。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join(".gitattributes"),
        "src/generated_sample.rs linguist-generated\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/generated_sample.rs"),
        "pub fn unused_generated_symbol() {}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/hand_written_sample.rs"),
        "pub fn unused_hand_written_symbol() {}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.contains(&"unused_generated_symbol"),
        "linguist-generated ファイルのシンボルは報告すべきでない: {names:?}"
    );
    assert!(
        names.contains(&"unused_hand_written_symbol"),
        "通常ファイルの未参照シンボルは dead として報告されるべき: {names:?}"
    );
}

#[test]
fn dead_code_excludes_vendor_dir_by_default() {
    // パッケージマネージャ配下 (vendor/, node_modules/, .venv/ 等) は
    // 既定で dead-code 走査から除外される。`--include-vendor` で opt-in 可能。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("vendor/pkg-a/src")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("vendor/pkg-a/src/lib.php"),
        "<?php\nfunction vendor_only_helper(): void { echo 'x'; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app.php"),
        "<?php\nfunction app_only_helper(): void { echo 'y'; }\n",
    )
    .unwrap();

    // 既定: vendor 除外
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"vendor_only_helper"),
        "vendor/ 配下の symbol は既定で dead-code に含めない: {names:?}"
    );
    assert!(
        names.contains(&"app_only_helper"),
        "src/ 配下の未参照 symbol は dead として報告される: {names:?}"
    );

    // opt-in: --include-vendor で vendor も含める
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-vendor",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&"vendor_only_helper"),
        "--include-vendor で vendor/ 配下も対象に含まれる: {names:?}"
    );
}

#[test]
fn dead_code_excludes_tests_dir_by_default() {
    // テストディレクトリ (tests/, Tests/, __tests__/, spec/, testdata/) は
    // 既定で dead-code 走査から除外される。`--include-tests` で opt-in 可能。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("tests/unit")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    // 意図的に PHPUnit 命名規約 (`^test[A-Z_]`) に合致しない関数名を使い、
    // 「ディレクトリ除外」単独での効果を検証する。
    std::fs::write(
        root.join("tests/unit/sample_case.php"),
        "<?php\nfunction fixture_assertion_helper(): void { echo 'y'; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/runtime.php"),
        "<?php\nfunction runtime_only_helper(): void { echo 'z'; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        !names.contains(&"fixture_assertion_helper"),
        "tests/ 配下の symbol は既定で dead-code に含めない: {names:?}"
    );
    assert!(
        names.contains(&"runtime_only_helper"),
        "src/ 配下の未参照 symbol は dead として報告される: {names:?}"
    );

    // opt-in: --include-tests
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--include-tests",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<&str> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&"fixture_assertion_helper"),
        "--include-tests で tests/ 配下も対象に含まれる: {names:?}"
    );
}

#[test]
fn dead_code_framework_laravel_excludes_migrations_and_controllers() {
    // --framework laravel で Laravel の規約的エントリポイント
    // (database/migrations, app/Http/Controllers 等) が除外されることを検証。
    // 一次創作の Laravel-ish な最小構造を使い、対象プロジェクトのコードは引用しない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("app/Http/Controllers")).unwrap();
    std::fs::create_dir_all(root.join("app/Services")).unwrap();
    std::fs::create_dir_all(root.join("database/migrations")).unwrap();
    std::fs::create_dir_all(root.join("database/seeds")).unwrap();

    std::fs::write(
        root.join("app/Http/Controllers/SampleController.php"),
        "<?php\nclass SampleController {\n    public function index() { return 'x'; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/Services/SampleService.php"),
        "<?php\nclass SampleService {\n    public function loadProfile() { return []; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("database/migrations/2025_01_example.php"),
        "<?php\nclass CreateExampleTable {\n    public function up() {}\n    public function down() {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("database/seeds/ExampleSeeder.php"),
        "<?php\nclass ExampleSeeder {\n    public function run() {}\n}\n",
    )
    .unwrap();

    // --framework laravel なし: Controllers / migrations / seeds も dead 候補に出る
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n.contains("SampleController")),
        "framework preset なしでは Controller が dead 候補に出る: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("CreateExampleTable")),
        "framework preset なしでは migration class が dead 候補に出る: {names:?}"
    );

    // --framework laravel: Controllers / migrations / seeds は除外、Services は残る
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for banned in ["SampleController", "CreateExampleTable", "ExampleSeeder"] {
        assert!(
            !names.iter().any(|n| n.contains(banned)),
            "{banned} は Laravel preset で除外されるべき: {names:?}"
        );
    }
    assert!(
        names
            .iter()
            .any(|n| n.contains("SampleService") || n.contains("loadProfile")),
        "app/Services/ 配下は Laravel preset でも dead 判定される: {names:?}"
    );
}

#[test]
fn dead_code_excludes_php_pseudo_enum_factory() {
    // Laravel / DDD 系の AbstractValueObject 派生クラスの擬似 enum パターンが dead-code から
    // 除外されることを検証する。dead-code 経路は常に exclude_framework_entrypoints=true
    // で呼ばれるため、preset 指定の有無に関わらず擬似 enum は除外される。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/Models/Menu")).unwrap();
    std::fs::write(
        root.join("src/Models/Menu/MenuName.php"),
        r#"<?php
class MenuName extends AbstractValueObjectString {
    public static function MENU_HOME(): self {
        return new self('MENU_HOME');
    }
    public static function MENU_DASHBOARD(): static {
        return new static('MENU_DASHBOARD');
    }
    /** メソッド名と new self('...') の文字列が不一致 → 擬似 enum ではない */
    public static function notPseudo(): self {
        return new self('different_name');
    }
    public function getValue(): string {
        return 'unused-impl';
    }
}
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    // 擬似 enum (MENU_HOME / MENU_DASHBOARD) は除外される
    for banned in ["MENU_HOME", "MENU_DASHBOARD"] {
        assert!(
            !names.iter().any(|n| n.contains(banned)),
            "PHP 擬似 enum factory ({banned}) は除外されるべき: {names:?}"
        );
    }
    // 擬似 enum でない static method (notPseudo) は残る
    assert!(
        names.iter().any(|n| n.contains("notPseudo")),
        "メソッド名と new self() の引数が不一致なら擬似 enum ではないので dead に残る: {names:?}"
    );
}

#[test]
fn dead_code_excludes_php_method_with_runtime_annotation() {
    // @TypeItem などの runtime annotation 付きメソッドは reflection 経由で
    // 動的呼び出しされるため dead-code から除外する。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/Bases/Meta")).unwrap();
    std::fs::write(
        root.join("src/Bases/Meta/EntityType.php"),
        r#"<?php
class EntityType extends AbstractValueObjectString {
    /**
     * @\App\Annotations\TypeItem(id=1, name="SurveyNode", alt_name="survey-node")
     * @return static
     */
    public static function SurveyNode(): self {
        return new self('SurveyNode');
    }

    /** 通常の static method (annotation なし) */
    public static function plainMethod(): self {
        return new self('plainMethod');
    }
}
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    assert!(
        !names.iter().any(|n| n.contains("SurveyNode")),
        "@TypeItem 付きメソッドは dead から除外されるべき: {names:?}"
    );
    // plainMethod は擬似 enum パターンなので、こちらも除外される (= dead に出ない)
}

/// `--framework nextjs` で Next.js App Router の規約 entrypoint
/// (page / layout / route / loading 等) と Pages Router 配下が dead-code から除外される。
/// (レポート 2026-05-04-next-page-and-react-memo-false-positives.md パターン2 の再現)
#[test]
fn dead_code_framework_nextjs_excludes_app_and_pages_routes() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/app/(authenticated)/dashboard")).unwrap();
    std::fs::create_dir_all(root.join("src/app/api/users")).unwrap();
    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::create_dir_all(root.join("src/pages/api")).unwrap();
    std::fs::create_dir_all(root.join("src/services")).unwrap();

    std::fs::write(
        root.join("src/app/(authenticated)/dashboard/page.tsx"),
        "export default function DashboardPage() { return null; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/api/users/route.ts"),
        "export function GET() { return new Response('ok'); }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/layout.tsx"),
        "export default function RootLayout({ children }: { children: React.ReactNode }) { return <>{children}</>; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/pages/api/legacy.ts"),
        "export default function handler() { return null; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/services/orphan.ts"),
        "export function unusedService() { return 1; }\n",
    )
    .unwrap();

    // --framework なし: 全部 dead に出る
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "DashboardPage"),
        "preset なしでは page default export も dead 判定: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "unusedService"),
        "preset なしでは services 配下も dead 判定: {names:?}"
    );

    // --framework nextjs: app/page, app/route, app/layout, pages/** は除外、services は残る
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "nextjs",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    for banned in ["DashboardPage", "GET", "RootLayout", "handler"] {
        assert!(
            !names.iter().any(|n| n == banned),
            "{banned} は Next.js preset で除外されるべき: {names:?}"
        );
    }
    assert!(
        names.iter().any(|n| n == "unusedService"),
        "src/services/ 配下は Next.js preset でも dead 判定される: {names:?}"
    );
}

/// `--framework` 未指定でも `package.json` の `dependencies.next` を検出して nextjs
/// プリセットを自動適用する。Issue 2026-05-20 で 3 回再発した `app/**/page.tsx` の
/// default export が dead 判定される問題への対応。
#[test]
fn dead_code_auto_detect_nextjs_from_package_json_dependencies() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/app/(authenticated)/admin")).unwrap();
    std::fs::create_dir_all(root.join("src/services")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "demo", "dependencies": { "next": "^15.0.0", "react": "^19.0.0" } }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/(authenticated)/admin/page.tsx"),
        "export default function AdminPage() { return null; }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/services/orphan.ts"),
        "export function unusedService() { return 1; }\n",
    )
    .unwrap();

    // --framework 指定なし: package.json の next 依存から自動的に nextjs プリセット適用
    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    assert!(
        !names.iter().any(|n| n == "AdminPage"),
        "package.json に next 依存があれば --framework 未指定でも AdminPage は除外されるべき: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "unusedService"),
        "src/services/ 配下は auto-detect でも dead 判定される (誤除外しない): {names:?}"
    );
}

/// `devDependencies` 経由の `next` 依存も自動検出対象に含める (Next.js は通常
/// dependencies に置かれるが、SSG 専用や CLI tooling として dev に置くプロジェクトもある)。
#[test]
fn dead_code_auto_detect_nextjs_from_package_json_dev_dependencies() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("app")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "demo", "devDependencies": { "next": "^15.0.0" } }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("app/page.tsx"),
        "export default function HomePage() { return null; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !names.iter().any(|n| n == "HomePage"),
        "devDependencies.next からも自動検出されるべき: {names:?}"
    );
}

/// `package.json` が無いプロジェクトでは auto-detect は発動せず、`app/**/page.tsx` の
/// default export は dead 判定のまま残る (誤検出しない方向への保守的フォールバック)。
#[test]
fn dead_code_auto_detect_skipped_without_package_json() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::write(
        root.join("src/app/page.tsx"),
        "export default function NotNextPage() { return null; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "NotNextPage"),
        "package.json が無ければ auto-detect は発動せず、page.tsx の default export は dead 判定のまま: {names:?}"
    );
}

/// `peerDependencies` / `optionalDependencies` 経由の `next` は誤爆しやすいため
/// auto-detect の対象外とする (Next.js ライブラリやテスト fixture 対策)。
#[test]
fn dead_code_auto_detect_ignores_peer_dependencies_next() {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("src/app")).unwrap();
    std::fs::write(
        root.join("package.json"),
        r#"{ "name": "next-helper-lib", "peerDependencies": { "next": ">=13" } }"#,
    )
    .unwrap();
    std::fs::write(
        root.join("src/app/page.tsx"),
        "export default function PeerOnlyPage() { return null; }\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        names.iter().any(|n| n == "PeerOnlyPage"),
        "peerDependencies のみの next は auto-detect 対象外。page.tsx の default export は dead 判定のまま: {names:?}"
    );
}

#[test]
fn dead_code_exclude_glob_and_exclude_dir_drop_targets() {
    // --exclude-glob 'app/Legacy/**' と --exclude-dir custom_dir で
    // それぞれサブツリーが除外されることを検証。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::create_dir_all(root.join("app/Legacy")).unwrap();
    std::fs::create_dir_all(root.join("custom_dir/sub")).unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();

    std::fs::write(
        root.join("app/Legacy/OldHandler.php"),
        "<?php\nclass OldHandler {\n    public function legacyEntry() {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("custom_dir/sub/PluginBridge.php"),
        "<?php\nclass PluginBridge {\n    public function pluginEntry() {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/runtime.php"),
        "<?php\nfunction runtime_only_entry() {}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--exclude-glob",
            "app/Legacy/**",
            "--exclude-dir",
            "custom_dir",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();

    assert!(
        !names
            .iter()
            .any(|n| n.contains("OldHandler") || n.contains("legacyEntry")),
        "--exclude-glob 指定パターンは除外される: {names:?}"
    );
    assert!(
        !names
            .iter()
            .any(|n| n.contains("PluginBridge") || n.contains("pluginEntry")),
        "--exclude-dir 指定ディレクトリは除外される: {names:?}"
    );
    assert!(
        names.iter().any(|n| n == "runtime_only_entry"),
        "非除外 src/ の未参照シンボルは dead として報告される: {names:?}"
    );
}

#[test]
fn dead_code_framework_laravel_preset_works_when_dir_is_app_root() {
    // F1 回帰テスト: `--dir <fixture>/app --framework laravel` の場合も
    // プリセット glob が効き、`Http/Controllers/` 配下が dead_symbols から除外されること。
    // 従来は `**/app/Http/Controllers/**` が `--dir` 相対パス (`Http/Controllers/...`) に
    // マッチせず Controller が全件 FP になっていた。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("app/Http/Controllers")).unwrap();
    std::fs::create_dir_all(root.join("app/Services")).unwrap();

    std::fs::write(
        root.join("app/Http/Controllers/SampleController.php"),
        "<?php\nclass SampleController {\n    public function index() { return 'x'; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/Services/SampleService.php"),
        "<?php\nclass SampleService {\n    public function loadProfile() { return []; }\n}\n",
    )
    .unwrap();

    // --dir を `app/` 直下に指定 — 旧挙動では Laravel プリセット無効で SampleController が dead
    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.join("app").to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    assert!(
        !names.iter().any(|n| n.contains("SampleController")),
        "--dir が app/ 直下でも Laravel preset で Controllers/ は除外されるべき (F1 regression): {names:?}"
    );
    // app/Services は Laravel プリセットの対象外なので dead 判定が残るのが正しい
    assert!(
        names
            .iter()
            .any(|n| n.contains("SampleService") || n.contains("loadProfile")),
        "app/Services は Laravel preset 対象外のため dead 判定されるべき: {names:?}"
    );
}

#[test]
fn dead_code_framework_laravel_excludes_exceptions_handler() {
    // F2 回帰テスト: Laravel プリセットに `**/app/Exceptions/**` が含まれること。
    // App\Exceptions\Handler::report は bootstrap/app.php の規約で登録される
    // フレームワーク hook で、dead_symbols に含めるべきでない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("app/Exceptions")).unwrap();
    std::fs::create_dir_all(root.join("app/Services")).unwrap();

    std::fs::write(
        root.join("app/Exceptions/Handler.php"),
        "<?php\nclass Handler {\n    public function report(\\Throwable $e) { return null; }\n    public function render($request, \\Throwable $e) { return null; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("app/Services/SampleService.php"),
        "<?php\nclass SampleService {\n    public function loadProfile() { return []; }\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(output.status.success());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    for banned in ["Handler", "report", "render"] {
        assert!(
            !names.iter().any(|n| n.contains(banned)),
            "{banned} は app/Exceptions/ 配下なので Laravel preset で除外されるべき: {names:?}"
        );
    }
}

#[test]
fn dead_code_framework_laravel_covers_renamed_app_dir() {
    // F6 (F1 拡張で代替): Laravel プリセットの `**/X/**` 省略版マッチにより、
    // `app/` を `core/` のようにリネームした独自レイアウトや、
    // モノレポでサブディレクトリ配下に Laravel 規約構造を持つ場合でも
    // プリセットが効くこと。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    // `core/` (Laravel 標準の `app/` をリネームした想定) 配下に規約構造を作る
    std::fs::create_dir_all(root.join("core/Http/Controllers")).unwrap();
    std::fs::create_dir_all(root.join("core/Services")).unwrap();
    std::fs::create_dir_all(root.join("packages/sub/Http/Middleware")).unwrap();

    std::fs::write(
        root.join("core/Http/Controllers/SampleController.php"),
        "<?php\nclass SampleController {\n    public function index() { return 'x'; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("core/Services/SampleService.php"),
        "<?php\nclass SampleService {\n    public function loadProfile() { return []; }\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("packages/sub/Http/Middleware/SampleMiddleware.php"),
        "<?php\nclass SampleMiddleware {\n    public function handle() { return null; }\n}\n",
    )
    .unwrap();

    let output = cargo_bin()
        .args([
            "dead-code",
            "--dir",
            root.to_str().unwrap(),
            "--framework",
            "laravel",
        ])
        .output()
        .expect("failed to run");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let names: Vec<String> = json["dead_symbols"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect();
    // F1 で追加した `**/Http/Controllers/**` `**/Http/Middleware/**` 等の省略版マッチが
    // Laravel 標準外 (`core/`, `packages/sub/`) のレイアウトにも効くこと
    assert!(
        !names.iter().any(|n| n.contains("SampleController")),
        "core/Http/Controllers 配下 (リネーム済み app/) は preset で除外されるべき: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n.contains("SampleMiddleware")),
        "packages/sub/Http/Middleware (モノレポ配下) は preset で除外されるべき: {names:?}"
    );
    // core/Services は preset 対象外なので残る
    assert!(
        names
            .iter()
            .any(|n| n.contains("SampleService") || n.contains("loadProfile")),
        "core/Services は preset 対象外で dead 判定が残るべき: {names:?}"
    );
}

#[test]
fn dead_code_excludes_kotlin_override() {
    // Kotlin の `override` メソッドは親 interface / superclass 経由で呼ばれるため
    // cross-file refs では追跡できず、dead-code 判定で偽陽性になる。
    // AdapterView.OnItemSelectedListener / TextWatcher の override は除外されるべき。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("MainActivity.kt"),
        r#"package com.example

class MainActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {}

    fun setup() {
        val watcher = object : TextWatcher {
            override fun afterTextChanged(s: Editable?) {}
        }
    }

    override fun onItemSelected(parent: AdapterView<*>?, view: View?, position: Int, id: Long) {}
    override fun onNothingSelected(parent: AdapterView<*>?) {}

    fun unusedRegular() {}
}
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    for override_name in [
        "MainActivity.onCreate",
        "MainActivity.onItemSelected",
        "MainActivity.onNothingSelected",
        "MainActivity.afterTextChanged",
    ] {
        assert!(
            !names.contains(&override_name),
            "Kotlin の override メソッド {override_name} は dead に含めるべきでない: {names:?}"
        );
    }
    // override でない通常メソッドは dead として残る
    assert!(
        names.contains(&"MainActivity.unusedRegular"),
        "override でない未参照メソッドは dead として報告されるべき: {names:?}"
    );
}

#[test]
fn dead_code_excludes_java_override_annotation() {
    // Java の `@Override` アノテーション付きメソッドも dead から除外される。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("Sample.java"),
        r#"package com.example;

public class Sample extends Base {
    @Override
    public void handleEvent() {}

    public void plainUnused() {}
}
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.iter().any(|n| n.ends_with(".handleEvent")),
        "@Override 付きメソッドは dead に含めるべきでない: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.ends_with(".plainUnused")),
        "@Override のない未参照メソッドは dead として報告されるべき: {names:?}"
    );
}

#[test]
fn dead_code_excludes_android_manifest_activity() {
    // AndroidManifest.xml で `android:name=".MainActivity"` と宣言された
    // activity は Android OS から起動されるため dead に含めるべきでない。
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();

    std::fs::write(
        root.join("AndroidManifest.xml"),
        r#"<manifest xmlns:android="http://schemas.android.com/apk/res/android">
  <application>
    <activity android:name=".MainActivity" android:exported="true">
      <intent-filter>
        <action android:name="android.intent.action.MAIN" />
        <category android:name="android.intent.category.LAUNCHER" />
      </intent-filter>
    </activity>
  </application>
</manifest>
"#,
    )
    .unwrap();

    std::fs::write(
        root.join("MainActivity.kt"),
        r#"package com.example

class MainActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {}
}
"#,
    )
    .unwrap();

    let output = cargo_bin()
        .args(["dead-code", "--dir", root.to_str().unwrap()])
        .output()
        .expect("failed to run");
    assert!(output.status.success(), "dead-code は成功するべき");

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("invalid JSON");
    let dead = json["dead_symbols"].as_array().expect("dead_symbols 配列");
    let names: Vec<&str> = dead.iter().filter_map(|s| s["name"].as_str()).collect();

    assert!(
        !names.contains(&"MainActivity"),
        "AndroidManifest.xml で宣言された activity は dead 扱いすべきでない: {names:?}"
    );
}
