//! Named and star barrels must carry impact to the final use site.

use super::support::*;

fn paths(value: &serde_json::Value, bucket: &str) -> Vec<String> {
    value["changes"][0][bucket]
        .as_array()
        .map(|callers| {
            callers
                .iter()
                .filter_map(|caller| caller["path"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn ts_barrels_propagate_only_proven_origins() {
    let repo = TestRepo::new();
    repo.create_dir_all("src/lib/storage");
    repo.write(
        "tsconfig.json",
        "{\n // project alias\n \"compilerOptions\": {\"paths\": {\"@/*\": [\"src/*\", // fallback\n ],},},\n}\n",
    );
    repo.write("src/lib/storage/impl.ts", "export const LIMIT = 10;\n");
    repo.write(
        "src/lib/storage/index.ts",
        "export { LIMIT } from './impl';\n",
    );
    repo.write("src/lib/empty.ts", "export const OTHER = 1;\n");
    repo.write(
        "src/lib/index.ts",
        "export * from './empty';\nexport * from './storage';\n",
    );
    repo.write(
        "src/a_rel.ts",
        "import { LIMIT } from './lib/storage';\nexport const a = LIMIT + 1;\n",
    );
    repo.write(
        "src/b_alias.ts",
        "import { LIMIT } from '@/lib/storage';\nexport const b = LIMIT + 1;\n",
    );
    repo.write(
        "src/c_multi.ts",
        "import { LIMIT } from '@/lib';\nexport const c = LIMIT + 1;\n",
    );
    repo.write("src/other.ts", "export const LIMIT = 99;\n");
    repo.write(
        "src/z_unrelated.ts",
        "import { LIMIT } from './other';\nexport const z = LIMIT + 1;\n",
    );
    repo.write(
        "src/property.ts",
        "import { LIMIT } from '@/lib/storage';\nexport const property = object.LIMIT;\n",
    );
    repo.create_dir_all("apps/web/src/lib/storage");
    repo.write(
        "apps/web/tsconfig.json",
        "{\"compilerOptions\":{\"paths\":{\"@/*\":[\"./src/*\"]}}}",
    );
    repo.write(
        "apps/web/src/lib/storage/index.ts",
        "export const LIMIT = 3;\n",
    );
    repo.write(
        "apps/web/src/page.ts",
        "import { LIMIT } from '@/lib/storage';\nexport const page = LIMIT;\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("src/lib/storage/impl.ts", "export const LIMIT = 20;\n");

    let context = repo.run_json("context", &["--git"]);
    assert_eq!(context["changes"][0]["path"], "src/lib/storage/impl.ts");
    let impacted = paths(&context, "impacted_callers");
    for expected in ["src/a_rel.ts", "src/b_alias.ts", "src/c_multi.ts"] {
        assert!(
            impacted.contains(&expected.to_owned()),
            "{expected}: {context}"
        );
    }
    assert!(
        !impacted.contains(&"src/z_unrelated.ts".to_owned()),
        "{context}"
    );
    assert!(
        !impacted.contains(&"apps/web/src/page.ts".to_owned()),
        "{context}"
    );
    assert!(
        !impacted.contains(&"src/property.ts".to_owned()),
        "{context}"
    );
    assert!(
        paths(&context, "low_confidence_callers").contains(&"src/z_unrelated.ts".to_owned()),
        "{context}"
    );
    assert!(
        paths(&context, "informational_callers").contains(&"src/lib/storage/index.ts".to_owned()),
        "{context}"
    );
    let impact = cargo_bin()
        .args(["impact", "--dir", repo.root().to_str().unwrap(), "--git"])
        .output()
        .unwrap();
    assert!(
        !impact.status.success(),
        "{}",
        String::from_utf8_lossy(&impact.stderr)
    );
}

#[test]
fn ts_root_barrel_resolves_relative_paths_without_a_parent_directory() {
    let repo = TestRepo::new();
    repo.write("impl.ts", "export const LIMIT = 10;\n");
    repo.write("index.ts", "export { LIMIT } from './impl.js';\n");
    repo.write(
        "consumer.ts",
        "import { LIMIT } from './index';\nexport const result = LIMIT;\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write("impl.ts", "export const LIMIT = 20;\n");
    let context = repo.run_json("context", &["--git"]);
    assert!(
        paths(&context, "impacted_callers").contains(&"consumer.ts".to_owned()),
        "{context}"
    );
}

#[test]
fn ts_barrel_ambiguity_and_local_export_do_not_prove_origin() {
    let repo = TestRepo::new();
    repo.create_dir_all("src");
    repo.write(
        "tsconfig.json",
        "{\"compilerOptions\":{\"paths\":{\"@/storage\":[\"src/other.ts\"],\"@/*\":[\"src/*\"]}}}",
    );
    repo.write(
        "src/impl.ts",
        "export const LIMIT = 10;\nexport const OTHER = 5;\n",
    );
    repo.write("src/other.ts", "export const LIMIT = 99;\n");
    repo.write("src/storage.ts", "export { LIMIT } from './impl';\n");
    repo.write(
        "src/ambiguous.ts",
        "export * from './impl';\nexport * from './other';\n",
    );
    repo.write(
        "src/overridden.ts",
        "export * from './impl';\nexport class LIMIT {}\n",
    );
    repo.write(
        "src/unresolved.ts",
        "export * from './impl';\nexport * from './missing';\n",
    );
    repo.write(
        "src/namespace.ts",
        "export * from './impl';\nexport * as LIMIT from './other';\n",
    );
    repo.write(
        "src/destructured.ts",
        "export * from './impl';\nexport const { LIMIT } = source;\n",
    );
    repo.create_dir_all("src/bad");
    repo.write("src/bad.ts", "export const LIMIT = ;\n");
    repo.write("src/bad/index.ts", "export * from '../impl';\n");
    repo.write(
        "src/renamed.ts",
        "export { OTHER as LIMIT } from './impl';\n",
    );
    repo.write(
        "src/a.ts",
        "import { LIMIT } from './ambiguous';\nexport const a = LIMIT;\n",
    );
    repo.write(
        "src/b.ts",
        "import { LIMIT } from './overridden';\nexport const b = LIMIT;\n",
    );
    repo.write(
        "src/c.ts",
        "import { LIMIT } from './unresolved';\nexport const c = LIMIT;\n",
    );
    repo.write(
        "src/d.ts",
        "import { LIMIT } from './namespace';\nexport const d = LIMIT;\n",
    );
    repo.write(
        "src/e.ts",
        "import { LIMIT } from './renamed';\nexport const e = LIMIT;\n",
    );
    repo.write(
        "src/f.ts",
        "import { LIMIT } from './bad';\nexport const f = LIMIT;\n",
    );
    repo.write(
        "src/g.ts",
        "import { LIMIT } from '@/storage';\nexport const g = LIMIT;\n",
    );
    repo.write(
        "src/h.ts",
        "import { LIMIT } from './destructured';\nexport const h = LIMIT;\n",
    );
    repo.init_git();
    repo.commit_all("initial");
    repo.write(
        "src/impl.ts",
        "export const LIMIT = 20;\nexport const OTHER = 5;\n",
    );

    let context = repo.run_json("context", &["--git"]);
    let impacted = paths(&context, "impacted_callers");
    assert!(!impacted.contains(&"src/a.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/b.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/c.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/d.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/e.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/f.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/g.ts".to_owned()), "{context}");
    assert!(!impacted.contains(&"src/h.ts".to_owned()), "{context}");
}
#[test]
fn ts_const_import_alias_reports_only_reads_of_the_resolved_binding() {
    let repo = TestRepo::new();
    let old = "export const VALUE: string = 'old';\n";
    let new = "export const VALUE: number = 42;\n";
    let consumer = "import { VALUE as TMP } from './values.js';\nexport const use: string = TMP;\nexport function inner(TMP: string) { return TMP; }\n{ const TMP = 'local'; console.log(TMP); }\nexport const obj = { TMP: 1 };\nexport const field = obj.TMP;\nexport const shorthand = { TMP };\n";
    repo.write("values.ts", old);
    repo.write("use.tsx", consumer);
    repo.write(
        "escapes.ts",
        r#"import { "V\u0041LUE" as \u0054MP } from './values';
export const read: string = \u0054MP;
export function shadow(\u0054MP: string) { return TMP; }
export namespace Nested { export const read: string = TMP; }
declare function local(TMP: string): typeof TMP;
type Fn = (TMP: string) => typeof TMP;
export function readDefault(x: typeof TMP = TMP) { return x; }
"#,
    );

    repo.write("other.ts", "export const VALUE: string = 'other';\n");
    repo.write(
        "other-use.ts",
        "import {VALUE as TMP} from './other';\nexport const value = TMP;\n",
    );
    repo.write(
        "external.ts",
        "import { VALUE as TMP } from 'external';\nexport const value = TMP;\n",
    );
    repo.write(
        "types.ts",
        "import type {VALUE as TMP} from './values';\nexport type Current = typeof TMP;\n",
    );
    repo.init_git();
    repo.commit_all("base");
    repo.write("values.ts", new);
    repo.stage_all();
    for staged in [false, true] {
        if staged {
            repo.write("values.ts", old);
            repo.write("use.tsx", "export const use = 1;\n");
        }
        let args: &[&str] = if staged {
            &["--git", "--staged"]
        } else {
            &["--git"]
        };
        let review = repo.run_json("review", args);
        let changes = review["impact"]["changes"].as_array().unwrap();
        let file = changes
            .iter()
            .find(|file| file["path"] == "values.ts")
            .unwrap();
        let reads: Vec<_> = file["impacted_callers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|caller| {
                caller["symbols"]
                    .as_array()
                    .is_some_and(|names| names.iter().any(|n| n == "VALUE"))
            })
            .map(|caller| {
                (
                    caller["path"].as_str().unwrap(),
                    caller["line"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            reads,
            [
                ("escapes.ts", 1),
                ("escapes.ts", 3),
                ("escapes.ts", 6),
                ("types.ts", 1),
                ("use.tsx", 1),
                ("use.tsx", 6)
            ],
            "{review}"
        );
        assert_ne!(
            review["api_changes"]["modified"][0]["no_resolved_internal_callers"], true,
            "{review}"
        );
        let mut cmd = cargo_bin();
        let output = cmd
            .args(["review", "--hook", "--dir"])
            .arg(repo.root())
            .args(args)
            .output()
            .unwrap();
        let hook: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert!(
            hook["impacts"][0]["refs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["p"] == "use.tsx" && r["ln"] == 1),
            "{hook}"
        );
        assert_ne!(hook["api"]["mod"][0]["no_callers"], true, "{hook}");
        if !staged {
            let refs = repo.run_json("refs", &["--name", "VALUE", "--max-results", "unlimited"]);
            assert!(
                !refs["refs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|r| r["path"] == "use.tsx" && r["ln"] == 1),
                "{refs}"
            );
            let impact = cargo_bin()
                .args(["impact", "--git", "--dir"])
                .arg(repo.root())
                .output()
                .unwrap();
            assert_eq!(impact.status.code(), Some(1));
        }
    }
}

#[test]
fn ts_const_import_alias_requires_the_resolved_module() {
    let repo = TestRepo::new();
    let config = r#"{"compilerOptions":{"paths":{"@values":["./values"]}}}"#;
    repo.write("tsconfig.json", config);
    repo.write("values.ts", "export const VALUE: string = 'old';\n");
    repo.write(
        "values.ios.ts",
        "export const VALUE: string = 'untouched';\n",
    );
    repo.write("barrel.ts", "export {VALUE} from '@values';\n");
    for (file, module) in [
        ("relative.ts", "./values"),
        ("mapped.ts", "@values"),
        ("forwarded.ts", "./barrel"),
    ] {
        repo.write(
            file,
            format!(
                "import {{VALUE as LOCAL}} from '{module}';\nexport const use: string = LOCAL;\n"
            ),
        );
    }
    repo.init_git();
    repo.commit_all("base");
    repo.write("values.ts", "export const VALUE: number = 42;\n");
    let reads = || {
        let context = repo.run_json("context", &["--git"]);
        let change = context["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["path"] == "values.ts")
            .unwrap();
        change["impacted_callers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["line"] == 1)
            .map(|r| r["path"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(reads(), ["forwarded.ts", "mapped.ts", "relative.ts"]);
    repo.write(
        "tsconfig.json",
        config.replace(
            "\"paths\":",
            "\"moduleSuffixes\":[\".ios\",\"\"],\"paths\":",
        ),
    );
    assert!(reads().is_empty());
}
