use super::support::{TestRepo, cargo_bin};

fn fixture(extension: &str, badge: &str, caller: &str) -> TestRepo {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    let javascript = |code: &str| {
        code.replace(": readonly string[]", "")
            .replace(": string", "")
            .replace(": boolean", "")
    };
    let code = |code: &str| {
        if extension == "js" {
            javascript(code)
        } else {
            code.to_owned()
        }
    };
    repo.write(format!("src/days.{extension}"), code("export function isOpen(day: string): boolean {\n  return day !== 'sun';\n}\n\nexport function dayLabel(day: string): string {\n  return day.toUpperCase();\n}\n"));
    repo.write(format!("src/badge.{extension}"), badge);
    repo.write(format!("src/main.{extension}"), code("import { isOpen } from './days';\nexport function today(day: string): boolean {\n  return isOpen(day);\n}\n"));
    repo.write(
        format!("src/entry.{extension}"),
        "import { today } from './main'; today('mon');\n",
    );
    repo.commit_all("initial");
    repo.write(format!("src/days.{extension}"), code("export function isOpen(day: string, openDays: readonly string[]): boolean {\n  return openDays.includes(day);\n}\n\nexport function dayLabel(day: string): string {\n  return day.toUpperCase();\n}\n"));
    repo.write(format!("src/main.{extension}"), code(caller));
    repo
}

const UPDATED: &str = "import { isOpen } from './days';\nexport function today(day: string): boolean {\n  return isOpen(day, ['mon']);\n}\n";
const BADGE: &str = "interface BadgeProps {\n  day: string;\n  isOpen?: boolean;\n}\n\nexport function badgeLabel({ day, isOpen }: BadgeProps): string {\n  return isOpen ? day : 'closed';\n}\n";

fn hook(repo: &TestRepo) -> (i32, serde_json::Value) {
    let output = cargo_bin()
        .args(["review", "--git", "--hook", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    let json = serde_json::from_slice(&output.stderr).unwrap();
    (output.status.code().unwrap(), json)
}

#[test]
fn unrelated_truthiness_parameter_closes_updated_api() {
    for extension in ["ts", "tsx"] {
        let repo = fixture(extension, BADGE, UPDATED);
        let (code, json) = hook(&repo);
        assert_eq!(
            json["api"]["mod_closed"][0]["n"], "isOpen",
            "{extension}: {json}"
        );
        assert_eq!(code, 0, "{json}");
    }
}

#[test]
fn same_module_other_import_closes_api_without_changing_impact() {
    let repo = fixture(
        "ts",
        &format!(
            "import {{ dayLabel }} from './days';\n{}",
            BADGE.replace("isOpen ? day", "isOpen ? dayLabel(day)")
        ),
        UPDATED,
    );
    let (code, json) = hook(&repo);
    assert_eq!(json["api"]["mod_closed"][0]["n"], "isOpen", "{json}");
    assert_eq!(
        code, 0,
        "closed API suppresses related hook impacts: {json}"
    );
    let impact = cargo_bin()
        .args(["impact", "--git", "--dir"])
        .arg(repo.root())
        .output()
        .unwrap();
    assert_eq!(
        impact.status.code(),
        Some(1),
        "standalone impact belongs to the follow-up"
    );
    let details = String::from_utf8_lossy(&impact.stderr);
    assert!(
        details.contains("src/badge.ts") && details.contains("isOpen"),
        "{details}"
    );
}

#[test]
fn signature_sensitive_parameter_uses_stay_blocking() {
    for badge in [
        "export function badge(isOpen) { return isOpen('mon'); }",
        "export function badge(isOpen) { return () => isOpen('mon'); }",
        "export function badge(isOpen) { return isOpen?.('mon'); }",
        "export function badge(isOpen) { return new isOpen('mon'); }",
        "export function badge(isOpen) { return isOpen`mon`; }",
        "export function badge(isOpen) { return isOpen; }",
        "export function badge(isOpen) { return consume(isOpen); }",
        "export function badge(isOpen) { return {isOpen}; }",
        "export function badge(isOpen) { isOpen = fallback; return !!isOpen; }",
        "export function badge(isOpen) { return isOpen || fallback; }",
        "export function badge(isOpen) { return isOpen && isOpen('mon'); }",
        "export function badge({isOpen: open}) { return open('mon'); }",
        "export function badge(isOpen = fallback) { return !!isOpen; }",
        "class Badge { constructor(private isOpen: boolean) {} }",
        "export function badge(cb: typeof isOpen) { return !!cb; }",
        "export function badge(isOpen) { function inner(isOpen) { return !!isOpen; } return !!isOpen; }",
        "interface DaysApi { isOpen(day: string): boolean; }",
        "interface DaysApi { isOpen: (day: string) => boolean; }",
        "interface DaysApi { isOpen: unknown; }",
        "vi.mock('./days', () => ({isOpen: (day) => true}));",
        "export function badge(isOpen) { return (isOpen as boolean) ? 'a' : 'b'; }",
        "export function badge(isOpen) { return isOpen! ? 'a' : 'b'; }",
        "export function badge(isOpen) { return typeof isOpen; }",
        "export function badge(isOpen) { eval('isOpen(1)'); return !!isOpen; }",
        "export function badge(isOpen) { return !!isOpen; } const broken = ;",
        "declare module './days' { interface Props { isOpen?: boolean; } }",
    ] {
        let repo = fixture("ts", badge, UPDATED);
        let (_, json) = hook(&repo);
        assert!(json["api"]["mod_closed"].is_null(), "{badge}: {json}");
        assert_eq!(json["api"]["mod"][0]["n"], "isOpen", "{badge}: {json}");
    }
}

#[test]
fn actual_unupdated_caller_is_not_hidden_by_truthiness_sites() {
    for caller in [
        "import { isOpen } from './days';\nexport function today(day: string) { return isOpen(day); }\n",
        "import { isOpen as check } from './days';\nexport function today(day: string) { return check(day); }\n",
        "import * as days from './days';\nexport function today(day: string) { return days.isOpen(day); }\n",
        "import { isOpen } from './days';\nexport function today(day: string) { return isOpen(day, ...[]); }\n",
    ] {
        let repo = fixture("ts", BADGE, caller);
        let (_, json) = hook(&repo);
        assert!(json["api"]["mod_closed"].is_null(), "{caller}: {json}");
        assert_eq!(json["api"]["mod"][0]["n"], "isOpen", "{caller}: {json}");
    }
}

#[test]
fn import_and_insensitive_sites_need_an_actual_updated_target_call() {
    let repo = fixture("ts", BADGE, "import { isOpen } from './days';\n");
    let (_, json) = hook(&repo);
    assert!(json["api"]["mod_closed"].is_null(), "{json}");
}

#[test]
fn declaration_file_contract_stays_blocking() {
    let repo = fixture("ts", "", UPDATED);
    repo.write("contract.d.ts", BADGE);
    let (_, json) = hook(&repo);
    assert_eq!(json["api"]["mod"][0]["n"], "isOpen", "{json}");
}

#[test]
fn javascript_truthiness_parameter_closes_updated_api() {
    let repo = fixture(
        "js",
        "export function badgeLabel({day, isOpen}) { return isOpen ? day : 'closed'; }\n",
        UPDATED,
    );
    let (status, json) = hook(&repo);
    assert_eq!(json["api"]["mod_closed"][0]["n"], "isOpen", "{json}");
    assert_eq!(status, 0, "{json}");
}

#[test]
fn multibyte_column_and_reference_contracts() {
    let badge = "interface BadgeProps { isOpen?: boolean; }\nexport function badgeLabel({isOpen}: BadgeProps) { const text = '日本語'; return isOpen ? text : ''; }\n";
    let repo = fixture("ts", badge, UPDATED);
    let single = repo.run_json("refs", &["--name", "isOpen", "--glob", "src/badge.ts"]);
    let batch = repo.run_json("refs", &["--names", "isOpen", "--glob", "src/badge.ts"]);
    assert_eq!(single, batch);
    let positions: Vec<_> = single["refs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["ln"].as_u64().unwrap(),
                r["col"].as_u64().unwrap(),
                r["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        positions,
        [(0, 23, "ref".to_owned()), (1, 84, "ref".to_owned())]
    );
    let before_dead = repo.run_json("dead-code", &["--glob", "src/badge.ts"]);
    let (code, json) = hook(&repo);
    assert_eq!(json["api"]["mod_closed"][0]["n"], "isOpen", "{json}");
    assert_eq!(code, 0, "{json}");
    assert_eq!(
        repo.run_json("dead-code", &["--glob", "src/badge.ts"]),
        before_dead
    );
    assert_eq!(
        repo.run_json("refs", &["--name", "isOpen", "--glob", "src/badge.ts"]),
        single
    );
}

#[test]
fn arguments_alias_keeps_parameter_signature_sensitive() {
    for use_site in [
        "arguments[0]['isOpen']('mon');",
        "const alias = {arguments}; alias.arguments[0]['isOpen']('mon');",
    ] {
        let badge = format!(
            "import * as days from './days';\nexport function badgeLabel({{isOpen}}) {{\n  {use_site}\n  return !!isOpen;\n}}\nbadgeLabel(days);\n"
        );
        let repo = fixture("ts", &badge, UPDATED);
        let (_, json) = hook(&repo);
        assert!(json["api"]["mod_closed"].is_null(), "{use_site}: {json}");
        assert_eq!(json["api"]["mod"][0]["n"], "isOpen", "{use_site}: {json}");
    }
}

#[test]
fn value_to_function_change_keeps_truthiness_references_blocking() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/days.ts", "export const isOpen = false;\n");
    repo.write("src/badge.ts", "import * as days from './days';\nexport function badgeLabel({isOpen}) { return !!isOpen; }\nbadgeLabel(days);\n");
    repo.write("src/main.ts", "import { isOpen } from './days';\nexport function today(): boolean {\n  return !!isOpen;\n}\n");
    repo.write("src/entry.ts", "import { today } from './main'; today();\n");
    repo.commit_all("initial");
    repo.write("src/days.ts", "export function isOpen(day: string, openDays: readonly string[]): boolean {\n  return openDays.includes(day);\n}\n");
    repo.write("src/main.ts", "import { isOpen } from './days';\nexport function today(): boolean {\n  return isOpen('mon', ['mon']);\n}\n");
    let (_, json) = hook(&repo);
    assert!(json["api"]["mod_closed"].is_null(), "{json}");
    assert_eq!(json["api"]["mod"][0]["n"], "isOpen", "{json}");
}

#[test]
fn sloppy_function_arguments_property_keeps_old_call_blocking() {
    let mut missed = Vec::new();
    for access in [".arguments", "['arguments']", "[`arguments`]"] {
        let badge = format!(
            "function badge({{isOpen}}) {{\n  badge{access}[0]['isOpen']('mon');\n  return !!isOpen;\n}}\nimport('./days').then(badge);\n"
        );
        let repo = fixture("js", &badge, UPDATED);
        let (_, json) = hook(&repo);
        if json["api"]["mod_closed"].is_array() {
            missed.push(format!("{access}: {json}"));
        }
    }
    assert!(missed.is_empty(), "{}", missed.join("\n"));
}

#[test]
fn escaped_shorthand_parameter_escape_stays_blocking() {
    let badge = "import * as days from './days';\nexport function badge({isOpen}) {\n  if (isOpen) {}\n  const escaped = {is\\u004fpen};\n  return Object.values(escaped)[0]('mon');\n}\nbadge(days);\n";
    let repo = fixture("js", badge, UPDATED);
    let (_, json) = hook(&repo);
    assert!(json["api"]["mod_closed"].is_null(), "{json}");
    assert_eq!(json["api"]["mod"][0]["n"], "isOpen", "{json}");
}

#[test]
fn escaped_shorthand_old_rebinding_stays_blocking() {
    let repo = TestRepo::new();
    repo.init_git();
    repo.create_dir_all("src");
    repo.write("src/days.js", "export function isOpen(day) { return day !== 'sun'; }\n({is\\u004fpen} = {isOpen: false});\n");
    repo.write("src/badge.js", "import * as days from './days';\nexport function badge({isOpen}) { return !!isOpen; }\nbadge(days);\n");
    repo.write(
        "src/main.js",
        "import { isOpen } from './days';\nexport function today() {\n  return !!isOpen;\n}\n",
    );
    repo.write("src/entry.js", "import { today } from './main'; today();\n");
    repo.commit_all("initial");
    repo.write(
        "src/days.js",
        "export function isOpen(day, openDays) {\n  return openDays.includes(day);\n}\n",
    );
    repo.write("src/main.js", "import { isOpen } from './days';\nexport function today() {\n  return isOpen('mon', ['mon']);\n}\n");
    let (_, json) = hook(&repo);
    assert!(json["api"]["mod_closed"].is_null(), "{json}");
    assert_eq!(json["api"]["mod"][0]["n"], "isOpen", "{json}");
}
