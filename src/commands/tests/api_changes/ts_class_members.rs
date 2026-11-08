//! TypeScript のクラスの公開のメンバー・enum のメンバー・オーバーロードの宣言 (Issue #286)。

#[allow(unused_imports)]
use crate::commands::tests::common::*;
#[allow(unused_imports)]
use std::fs;

const TSCONFIG: &str = "{ \"compilerOptions\": { \"declaration\": true } }\n";
const STRIP: &str = "{ \"compilerOptions\": { \"declaration\": true, \"stripInternal\": true } }\n";

/// `tsconfig` と `src/lib.ts` (`old`) をコミットし、`src/lib.ts` を `new` に書き換えたときの
/// API の差分。
fn api_with(tsconfig: &str, old: &str, new: &str) -> crate::models::review::ApiChanges {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = dir.path();
    init_git_repo_for_test(repo);
    git_commit_files(
        repo,
        &[("tsconfig.json", tsconfig), ("src/lib.ts", old)],
        "base",
    );
    fs::write(repo.join("src/lib.ts"), new).expect("write");
    detect_api_changes_from_worktree(repo)
}

fn api(old: &str, new: &str) -> crate::models::review::ApiChanges {
    api_with(TSCONFIG, old, new)
}

/// `name` の区分 (`mod` / `compat` / `closed` / `rm` / `rm_dead` / `add` / `none`)。
fn bucket(api: &crate::models::review::ApiChanges, name: &str) -> &'static str {
    if api.modified.iter().any(|change| change.name == name) {
        "mod"
    } else if api
        .compatible_modified
        .iter()
        .any(|change| change.name == name)
    {
        "compat"
    } else if api
        .modified_closed_in_diff
        .iter()
        .any(|change| change.name == name)
    {
        "closed"
    } else if api.removed.iter().any(|symbol| symbol.name == name) {
        "rm"
    } else if api.removed_dead.iter().any(|symbol| symbol.name == name) {
        "rm_dead"
    } else if api.added.iter().any(|symbol| symbol.name == name) {
        "add"
    } else {
        "none"
    }
}

const CLIENT: &str = "export class Client {\n  token: string = \"\";\n  constructor(public retries: number) {}\n  send(): void {}\n}\n";

/// Issue #286 の再現: フィールド・パラメータプロパティ・enum のメンバー・オーバーロードの宣言の
/// 削除は、入れ物の blocking な api.mod。
#[test]
fn member_and_overload_removals_are_modified() {
    let old = "export class Client {\n  token: string = \"\";\n  constructor(public retries: number) {}\n}\n\nexport enum Mode {\n  Public,\n  Internal,\n}\n\nexport function f(x: string): string;\nexport function f(x: number): number;\nexport function f(x: string | number): string | number {\n  return x;\n}\n";
    let new = "export class Client {\n  constructor(retries: number) {}\n}\n\nexport enum Mode {\n  Public,\n}\n\nexport function f(x: string): string;\nexport function f(x: string | number): string | number {\n  return x;\n}\n";
    let changes = api(old, new);
    for name in ["Client", "Mode", "f"] {
        assert_eq!(bucket(&changes, name), "mod", "{name}: {changes:?}");
    }
    // 1 つずつでも
    for (new, name) in [
        (
            "export class Client {\n  constructor(public retries: number) {}\n  send(): void {}\n}\n",
            "Client",
        ),
        (
            "export class Client {\n  token: string = \"\";\n  constructor(retries: number) {}\n  send(): void {}\n}\n",
            "Client",
        ),
        (
            "export class Client {\n  token: number = 0;\n  constructor(public retries: number) {}\n  send(): void {}\n}\n",
            "Client",
        ),
        (
            "export class Client {\n  token: string = \"\";\n  constructor(public retries: string) {}\n  send(): void {}\n}\n",
            "Client",
        ),
    ] {
        let changes = api(CLIENT, new);
        assert_eq!(bucket(&changes, name), "mod", "{new}: {changes:?}");
    }
}

/// 外から見えない変更 (private・`#private`・本体・注釈のある初期値・整形) は報告しない。optional の
/// フィールドの追加だけは互換で、必須のフィールドの追加は blocking。
#[test]
fn hidden_changes_and_member_additions() {
    let changes = api(
        CLIENT,
        "export class Client {\n  // note\n  token: string = \"changed\";\n  private cache = new Map<string, number>();\n  #secret = 1;\n  constructor(\n    public retries: number,\n  ) {\n    this.#secret = 2;\n  }\n  send(): void {\n    console.log(this.token);\n  }\n}\n",
    );
    assert_eq!(bucket(&changes, "Client"), "none", "{changes:?}");

    let changes = api(
        CLIENT,
        "export class Client {\n  token: string = \"\";\n  label?: string;\n  constructor(public retries: number) {}\n  send(): void {}\n}\n",
    );
    assert_eq!(bucket(&changes, "Client"), "compat", "{changes:?}");
    assert!(
        changes
            .compatible_modified
            .iter()
            .any(|change| change.name == "Client"
                && change.reason == "ts_class_optional_members_added"),
        "{changes:?}"
    );

    let changes = api(
        CLIENT,
        "export class Client {\n  token: string = \"\";\n  label: string = \"\";\n  constructor(public retries: number) {}\n  send(): void {}\n}\n",
    );
    assert_eq!(bucket(&changes, "Client"), "mod", "{changes:?}");
}

/// アクセサは型の読み書きを持つプロパティとしてクラスに入り、別のメソッドとしては数えない。
#[test]
fn accessors_are_class_members() {
    let old = "export class Client {\n  get value(): number {\n    return 1;\n  }\n  set value(v: number) {}\n}\n";
    let changes = api(
        old,
        "export class Client {\n  get value(): string {\n    return \"1\";\n  }\n  set value(v: string) {}\n}\n",
    );
    assert_eq!(bucket(&changes, "Client"), "mod", "{changes:?}");
    assert_eq!(bucket(&changes, "Client.value"), "none", "{changes:?}");
    let changes = api(old, "export class Client {}\n");
    assert_eq!(bucket(&changes, "Client"), "mod", "{changes:?}");
    assert_eq!(bucket(&changes, "Client.value"), "none", "{changes:?}");
}

/// 同じファイルの中だけで使うクラスでも、外の利用者を直せないので除外しない。
#[test]
fn same_file_uses_do_not_hide_member_changes() {
    let changes = api(
        "export class Client {\n  token: string = \"\";\n}\n\nexport function read(client: Client): string {\n  return client.token;\n}\n",
        "export class Client {\n  key: string = \"\";\n}\n\nexport function read(client: Client): string {\n  return client.key;\n}\n",
    );
    assert_eq!(bucket(&changes, "Client"), "mod", "{changes:?}");
}

/// enum はメンバーと値を比べる (暗黙の値と同じ明示の値は同じ)。メンバーの追加も
/// `Record<Mode, T>` や網羅的な分岐を壊し得るので blocking。
#[test]
fn enum_members_and_values_are_compared() {
    let old = "export enum Mode {\n  A,\n  B,\n}\n";
    assert_eq!(
        bucket(
            &api(old, "export enum Mode {\n  A = 0,\n  B = 1,\n}\n"),
            "Mode"
        ),
        "none"
    );
    for new in [
        "export enum Mode {\n  A,\n  B = 5,\n}\n",
        "export enum Mode {\n  A,\n  B,\n  C,\n}\n",
        "export enum Mode {\n  B,\n  A,\n}\n",
    ] {
        let changes = api(old, new);
        assert_eq!(bucket(&changes, "Mode"), "mod", "{new}: {changes:?}");
    }
}

/// オーバーロードがあれば、宣言ファイルに出るオーバーロードの並びを比べる。実装の宣言だけの変更は
/// 報告せず、オーバーロードの追加は互換にしない。
#[test]
fn overloads_are_compared_as_declared() {
    let old =
        "export function f(x: string): string;\nexport function f(x: any): any {\n  return x;\n}\n";
    let changes = api(
        old,
        "export function f(x: string): string;\nexport function f(x: unknown): unknown {\n  return x;\n}\n",
    );
    assert_eq!(bucket(&changes, "f"), "none", "{changes:?}");
    let changes = api(
        old,
        "export function f(x: string): string;\nexport function f(x: string, y?: number): string;\nexport function f(x: any, y?: number): any {\n  return x;\n}\n",
    );
    assert_eq!(bucket(&changes, "f"), "mod", "{changes:?}");
    // メソッドのオーバーロードも
    let changes = api(
        "export class Client {\n  send(a: string): void;\n  send(a: number): void;\n  send(a: any): void {}\n}\n",
        "export class Client {\n  send(a: string): void;\n  send(a: any): void {}\n}\n",
    );
    assert_eq!(bucket(&changes, "Client.send"), "mod", "{changes:?}");
}

/// `stripInternal` で宣言ファイルから消えるメンバーの変更は報告しない。enum の後のメンバーの値は
/// 消える前の宣言から求める。
#[test]
fn internal_members_follow_strip_internal() {
    let changes = api_with(
        STRIP,
        "export class Client {\n  token: string = \"\";\n  /** @internal */\n  debug: boolean = false;\n}\n",
        "export class Client {\n  token: string = \"\";\n}\n",
    );
    assert_eq!(bucket(&changes, "Client"), "none", "{changes:?}");
    let changes = api_with(
        STRIP,
        "export enum Mode {\n  A,\n  /** @internal */\n  B,\n  C,\n}\n",
        "export enum Mode {\n  A,\n  C = 2,\n}\n",
    );
    assert_eq!(bucket(&changes, "Mode"), "none", "{changes:?}");
    // 消えないときは報告する
    let changes = api(
        "export class Client {\n  token: string = \"\";\n  /** @internal */\n  debug: boolean = false;\n}\n",
        "export class Client {\n  token: string = \"\";\n}\n",
    );
    assert_eq!(bucket(&changes, "Client"), "mod", "{changes:?}");
}

/// abstract のメソッドの宣言はクラスの公開の形に入る (派生クラスが実装する契約)。
#[test]
fn abstract_method_declarations_are_class_members() {
    let changes = api(
        "export abstract class Shape {\n  abstract area(): number;\n  abstract name(): string;\n}\n",
        "export abstract class Shape {\n  abstract area(): number;\n}\n",
    );
    assert_eq!(bucket(&changes, "Shape"), "mod", "{changes:?}");
}

/// 名前の無い `export default class` はクラスの形を作らないので、アクセサは従来どおりメソッド。
#[test]
fn anonymous_default_class_accessors_stay_methods() {
    let changes = api(
        "export default class {\n  get value(): number {\n    return 1;\n  }\n  other(): void {}\n}\n",
        "export default class {\n  other(): void {}\n}\n",
    );
    assert_ne!(bucket(&changes, "value"), "none", "{changes:?}");
}

/// `stripInternal` で消えるオーバーロードの宣言の削除は報告しない。
#[test]
fn internal_overloads_follow_strip_internal() {
    let changes = api_with(
        STRIP,
        "export function f(x: string): string;\n/** @internal */\nexport function f(x: number): number;\nexport function f(x: any): any {\n  return x;\n}\n",
        "export function f(x: string): string;\nexport function f(x: any): any {\n  return x;\n}\n",
    );
    assert_eq!(bucket(&changes, "f"), "none", "{changes:?}");
}

/// optional のフィールドの追加だけでも、フィールドの型が名指す同じファイルの型が変われば blocking。
#[test]
fn optional_additions_do_not_hide_dependency_changes() {
    let changes = api(
        "type Value = string;\n\nexport class Client {\n  value: Value = \"\" as Value;\n}\n",
        "type Value = number;\n\nexport class Client {\n  value: Value = 0 as Value;\n  note?: string;\n}\n",
    );
    assert_eq!(bucket(&changes, "Client"), "mod", "{changes:?}");
}
