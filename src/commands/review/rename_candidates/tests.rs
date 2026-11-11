use super::*;

fn candidates(path: &str, old: &str, new: &str, limit: usize) -> RenameReport {
    candidates_in_files(&[(path, old)], &[(path, new)], limit)
}

fn candidates_in_files(old: &[(&str, &str)], new: &[(&str, &str)], limit: usize) -> RenameReport {
    let mut report = RenameReport::default();
    let mut before = Vec::new();
    let mut after = Vec::new();
    for (path, source) in old {
        collect_declarations(
            path,
            source.as_bytes(),
            RenameSourceSide::Old,
            &mut before,
            &mut report,
        );
    }
    for (path, source) in new {
        collect_declarations(
            path,
            source.as_bytes(),
            RenameSourceSide::New,
            &mut after,
            &mut report,
        );
    }
    pair_declarations(before, after, &mut report, limit);
    report
}

#[test]
fn rename_candidates_cross_file_name_collisions_do_not_erase_pairs() {
    for other in ["double", "twice"] {
        let before = format!("pub fn {other}(v: u32) -> u32 {{ v * 9 }}");
        let after = format!("pub fn {other}(v: u32) -> u32 {{ v * 10 }}");
        let report = candidates_in_files(
            &[
                ("a.rs", "pub fn double(v: u32) -> u32 { v * 2 }"),
                ("b.rs", &before),
            ],
            &[
                ("a.rs", "pub fn twice(v: u32) -> u32 { v * 2 }"),
                ("b.rs", &after),
            ],
            MAX_CANDIDATES,
        );
        assert_eq!(report.candidates.len(), 1, "{other}");
        let candidate = &report.candidates[0];
        assert_eq!(candidate.old.file, "a.rs");
        assert_eq!(candidate.old.name, "double");
        assert_eq!(candidate.new.file, "a.rs");
        assert_eq!(candidate.new.name, "twice");
    }
    let report = candidates_in_files(
        &[
            ("a.rs", "pub fn double() {}"),
            ("b.rs", "pub fn double() {}"),
        ],
        &[
            ("a.rs", "pub fn twice() {}"),
            ("b.rs", "pub fn double() {}"),
        ],
        MAX_CANDIDATES,
    );
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].old.file, "a.rs");
    assert_eq!(report.candidates[0].new.file, "a.rs");
    assert!(!report.candidates[0].ambiguous);
}

#[test]
fn rename_candidates_chained_names_keep_three_competing_edges_and_exact_omissions() {
    let old = "pub fn foo() {}\npub fn bar() {}\n";
    let new = "pub fn bar() {}\npub fn baz() {}\n";
    let report = candidates("api.rs", old, new, MAX_CANDIDATES);
    let pairs: Vec<_> = report
        .candidates
        .iter()
        .map(|item| (item.old.name.as_str(), item.new.name.as_str()))
        .collect();
    assert_eq!(pairs, vec![("foo", "bar"), ("foo", "baz"), ("bar", "baz")]);
    for candidate in &report.candidates {
        assert!(candidate.ambiguous);
        assert_eq!(
            candidate.new_candidates_for_old,
            if candidate.old.name == "foo" { 2 } else { 1 }
        );
        assert_eq!(
            candidate.old_candidates_for_new,
            if candidate.new.name == "baz" { 2 } else { 1 }
        );
    }
    let capped = candidates("api.rs", old, new, 2);
    assert_eq!(capped.candidates.len(), 2);
    assert_eq!(capped.diagnostics[0].omitted, Some(1));
    // 残る同名の対応は出力辺に数えず、移動・再配置の代替として曖昧さには残す。
    let swapped = candidates(
        "api.rs",
        old,
        "pub fn bar() {}\npub fn foo() {}\n",
        MAX_CANDIDATES,
    );
    assert_eq!(swapped.candidates.len(), 2);
    for candidate in swapped.candidates {
        assert!(candidate.ambiguous);
        assert_eq!(candidate.new_candidates_for_old, 1);
        assert_eq!(candidate.old_candidates_for_new, 1);
    }
}

#[test]
fn rename_candidates_same_name_moves_and_spelling_changes_are_not_renames() {
    let report = candidates_in_files(
        &[("old.rs", "pub fn stable() {}")],
        &[("new.rs", "pub fn stable() {}")],
        MAX_CANDIDATES,
    );
    assert!(report.candidates.is_empty());
    assert!(
        candidates(
            "api.rs",
            "pub fn r#stable() {}",
            "pub fn stable() {}",
            MAX_CANDIDATES
        )
        .candidates
        .is_empty()
    );
}

#[test]
fn rename_candidates_require_name_independent_ast_equality() {
    for (path, old, new) in [
        (
            "src/lib.rs",
            "pub fn double(v: u32) -> u32 { v * 2 }",
            "pub fn twice(v: u32) -> u32 { v * 2 }",
        ),
        (
            "api.ts",
            "export function double(v: number): number { return v * 2; }",
            "export function twice(v: number): number { return v * 2; }",
        ),
        (
            "api.py",
            "def double(v):\n    return v * 2\n",
            "def twice(v):\n    return v * 2\n",
        ),
    ] {
        let report = candidates(path, old, new, MAX_CANDIDATES);
        assert_eq!(
            report.candidates.len(),
            1,
            "{path}: {:?}",
            report.diagnostics
        );
        let candidate = &report.candidates[0];
        assert_eq!(candidate.old.name, "double");
        assert_eq!(candidate.new.name, "twice");
        assert!(!candidate.ambiguous);
        assert_eq!(candidate.old.file, path);
        let json = serde_json::to_value(candidate).unwrap();
        assert_eq!(json["status"], "candidate");
        assert_eq!(
            json["evidence"],
            serde_json::json!(["name_independent_ast_equal"])
        );
        assert_eq!(json["old_api_presence"]["status"], "unverified");
    }
    for new in [
        "pub fn twice(v: u32) -> u32 { v * 3 }",
        "pub fn twice(v: u64) -> u64 { v * 2 }",
        "pub struct twice { v: u32 }",
        "pub fn double(v: u32) -> u32 { v * 2 }",
    ] {
        assert!(
            candidates(
                "src/lib.rs",
                "pub fn double(v: u32) -> u32 { v * 2 }",
                new,
                MAX_CANDIDATES
            )
            .candidates
            .is_empty(),
            "{new}"
        );
    }
}

#[test]
fn rename_candidates_keep_many_to_one_and_competing_pairs_ambiguous() {
    let old = "pub fn first(v: u32) -> u32 { v * 2 }\npub fn second(v: u32) -> u32 { v * 2 }";
    for (new, expected, degree) in [
        ("pub fn merged(v: u32) -> u32 { v * 2 }", 2, 1),
        (
            "pub fn left(v: u32) -> u32 { v * 2 }\npub fn right(v: u32) -> u32 { v * 2 }",
            4,
            2,
        ),
    ] {
        let report = candidates("src/lib.rs", old, new, MAX_CANDIDATES);
        assert_eq!(report.candidates.len(), expected);
        for candidate in &report.candidates {
            assert!(candidate.ambiguous);
            assert_eq!(candidate.old_candidates_for_new, 2);
            assert_eq!(candidate.new_candidates_for_old, degree);
        }
        let repeated = candidates("src/lib.rs", old, new, MAX_CANDIDATES);
        assert_eq!(
            serde_json::to_string(&report.candidates).unwrap(),
            serde_json::to_string(&repeated.candidates).unwrap()
        );
    }
    // 統合後の本体が両方の旧宣言と異なれば、形だけ似た候補も出さない。
    assert!(
        candidates(
            "src/lib.rs",
            old,
            "pub fn merged(v: u32) -> u32 { v * 4 }",
            MAX_CANDIDATES
        )
        .candidates
        .is_empty()
    );
}

#[test]
fn rename_candidates_reexport_presence_is_independent_and_conservative() {
    let old = "pub fn double(v: u32) -> u32 { v * 2 }\n";
    for (alias, present) in [
        ("pub use self::twice as double;", true),
        ("pub use twice as double;", true),
        ("use self::twice as double;", false),
        (
            "#[cfg(feature = \"legacy\")]\npub use self::twice as double;",
            false,
        ),
        ("pub use other::twice as double;", false),
        ("pub use self::unknown as double;", false),
    ] {
        let new = format!("pub fn twice(v: u32) -> u32 {{ v * 2 }}\n{alias}\n");
        let report = candidates("src/lib.rs", old, &new, MAX_CANDIDATES);
        assert_eq!(
            report.candidates.len(),
            1,
            "{alias}: {:?}",
            report.diagnostics
        );
        let json = serde_json::to_value(&report.candidates[0]).unwrap();
        assert_eq!(
            json["old_api_presence"]["status"],
            if present {
                "direct_reexport_present"
            } else {
                "unverified"
            },
            "{alias}: {json}"
        );
        assert_eq!(json["old_api_presence"]["compatibility"], "unverified");
        assert_eq!(
            json["old_api_presence"]["evidence"]
                .as_array()
                .unwrap()
                .len(),
            usize::from(present)
        );
    }
}

#[test]
fn rename_candidates_limits_and_parse_failures_are_declared() {
    let old = "pub fn first() {}\npub fn second() {}\n";
    let new = "pub fn left() {}\npub fn right() {}\n";
    let report = candidates("src/lib.rs", old, new, 3);
    assert_eq!(report.candidates.len(), 3);
    let diagnostic = serde_json::to_value(&report.diagnostics).unwrap();
    assert_eq!(diagnostic[0]["reason"], "candidate_limit");
    assert_eq!(diagnostic[0]["omitted"], 1);
    let report = candidates("src/lib.rs", old, "pub fn invalid( {", MAX_CANDIDATES);
    assert!(report.candidates.is_empty());
    assert_eq!(
        serde_json::to_value(&report.diagnostics).unwrap()[0]["reason"],
        "parse_error"
    );
    let mut budget = 1;
    let tree = parser::parse_source(new.as_bytes(), LangId::Rust).unwrap();
    assert!(!spend_nodes(tree.root_node(), &mut budget));
}

#[test]
fn rename_candidates_do_not_change_existing_hook_categories_or_collections() {
    use crate::models::review::{ApiSymbol, ReviewResult};
    let report = candidates(
        "src/lib.rs",
        "pub fn old() {}",
        "pub fn new() {}",
        MAX_CANDIDATES,
    );
    assert_eq!(report.candidates.len(), 1);
    for blocking in [false, true] {
        let mut result = ReviewResult::default();
        if blocking {
            result.api_changes.removed.push(ApiSymbol {
                name: "old".into(),
                file: "src/lib.rs".into(),
                kind: "function".into(),
                inline_module: None,
                refs_internal: 0,
                cfg_availability: None,
            });
        }
        let baseline = crate::commands::review::hook::build_review_hook_json(&result, ".", false);
        let before = serde_json::to_value(&result).unwrap();
        result.rename_candidates = report.candidates.clone();
        let current = crate::commands::review::hook::build_review_hook_json(&result, ".", false);
        assert_eq!(baseline.is_blocking, current.is_blocking);
        let mut after = serde_json::to_value(&result).unwrap();
        after.as_object_mut().unwrap().remove("rename_candidates");
        assert_eq!(before, after);
        let mut hook = current.value.unwrap();
        assert_eq!(hook["rename_candidates"][0]["old"]["name"], "old");
        assert_eq!(
            hook["blocking_categories"],
            if blocking {
                serde_json::json!(["api.rm"])
            } else {
                serde_json::json!([])
            }
        );
        hook.as_object_mut().unwrap().remove("rename_candidates");
        if let Some(baseline) = baseline.value {
            assert_eq!(baseline, hook);
        }
    }
}
