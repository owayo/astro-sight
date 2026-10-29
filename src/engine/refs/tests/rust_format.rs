use super::*;
use crate::engine::refs::definition::rust_format::captures;
use crate::engine::tree_walk::{Children, Walk, walk_preorder};

fn capture_names(source: &str) -> Vec<String> {
    let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
    assert!(
        !tree.root_node().has_error(),
        "{}",
        tree.root_node().to_sexp()
    );
    let mut found = Vec::new();
    let _: std::ops::ControlFlow<()> = walk_preorder(tree.root_node(), Children::All, |node, _| {
        let mut ancestors = Vec::new();
        let mut parent = node.parent();
        while let Some(n) = parent {
            ancestors.push(n);
            parent = n.parent();
        }
        ancestors.reverse();
        found.extend(
            captures(node, &ancestors, source.as_bytes())
                .into_iter()
                .map(|(name, at)| {
                    assert!(source[at..].starts_with(&name) || source[at..].contains('\\'));
                    name
                }),
        );
        std::ops::ControlFlow::Continue(Walk::Descend)
    });
    found
}

#[test]
fn rust_format_capture_syntax_and_explicit_arguments() {
    let source = r####"fn run() {
        format!("{VALUE} {VALUE:0width$.precision$} {{IGNORED}}", unused = 1);
        format_args!("{:width$} {:.*}", 1, 2, 3);
        print!("{VALUE:?}"); println!("{VALUE:#x}");
        eprint!("{VALUE}"); eprintln!("{VALUE}");
        write!(writer::<T, U>(), "{VALUE}");
        writeln!((writer, other), r##"{VALUE}"##);
        std /* comment */ :: format!("{VALUE}");
        assert!(::std::format!("{NESTED}").is_empty());
        format!("{SHADOW}", SHADOW = call::<A, B>(1, 2));
        format!("{type}", r#type = 4);
        format!("{имя}");
    }"####;
    assert_eq!(
        capture_names(source),
        [
            "VALUE",
            "VALUE",
            "width",
            "precision",
            "width",
            "VALUE",
            "VALUE",
            "VALUE",
            "VALUE",
            "VALUE",
            "VALUE",
            "VALUE",
            "NESTED",
            "имя"
        ]
    );
}

#[test]
fn rust_format_capture_escapes_and_non_format_literals() {
    let source = r####"fn run() {
        let ordinary = "{IGNORED}";
        ordinary!("{IGNORED}"); custom::format!("{IGNORED}");
        stringify!(format!("{IGNORED}")); ordinary!(format!("{IGNORED}"));
        crate::format!("{IGNORED}"); assert!(self::format!("{IGNORED}"));
        macro_rules! template { () => { format!("{IGNORED}") }; }
        format!(concat!("{IGNORED}")); format!(b"{IGNORED}");
        format!("{{IGNORED}} {{}} {0} {:?}", 1, 2);
        format!("{{{VALUE}}}");
        format!("\n{VALUE}\t{VALUE}");
        format!("\x7bVALUE}"); format!("\u{7b}VALUE}");
        format!("{VA\u{4c}UE}");
        format!("{VALUE"); format!("{r#type}");
    }"####;
    assert_eq!(
        capture_names(source),
        ["VALUE", "VALUE", "VALUE", "VALUE", "VALUE", "VALUE"]
    );
}

#[test]
fn rust_format_capture_single_batch_visitor_and_count_agree() {
    let source = "pub const TARGET: u32 = 1;\nfn run() { format!(\"{TARGET} {{TARGET}}\"); println!(\"{TARGET:width$}\"); }\n";
    let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
    let root = tree.root_node();
    let kinds = definition_node_kinds(LangId::Rust);
    let names = vec!["TARGET".to_string(), "width".to_string()];
    let batch = collect_batch_refs_for_test(
        root,
        source.as_bytes(),
        &names,
        "test.rs",
        kinds,
        LangId::Rust,
    );
    for (name, entries) in names.iter().zip(&batch) {
        let single = collect_single_refs_for_test(
            root,
            source.as_bytes(),
            name,
            "test.rs",
            kinds,
            LangId::Rust,
        );
        assert_eq!(ref_fingerprints(&single), ref_fingerprints(entries));
    }
    assert_eq!(batch[0].len(), 3);
    assert_eq!(batch[1].len(), 1);
    assert_eq!(
        count_refs_for_test(root, source.as_bytes(), &names, kinds, LangId::Rust, 2),
        [2, 1]
    );
    struct Recorder(Vec<(usize, usize, bool)>);
    impl RefVisitor for Recorder {
        fn on_ref(&mut self, event: RefVisitEvent<'_>) {
            self.0.push((event.line, event.column, event.is_def));
        }
    }
    let matcher = SingleMatcher {
        lang_id: LangId::Rust,
        target: "TARGET",
    };
    let mut recorder = Recorder(Vec::new());
    run_ref_walk(
        root,
        source.as_bytes(),
        LangId::Rust,
        kinds,
        &matcher,
        &mut super::super::walker::VisitorAdapter {
            visitor: &mut recorder,
        },
    );
    let expected: Vec<_> = batch[0]
        .iter()
        .map(|r| (r.line, r.column, r.kind == Some(RefKind::Definition)))
        .collect();
    assert_eq!(recorder.0, expected);
}

#[test]
fn rust_format_capture_node_kinds_exist() {
    let language = LangId::Rust.ts_language();
    for kind in [
        "macro_definition",
        "scoped_identifier",
        "macro_invocation",
        "token_tree",
        "identifier",
        "string_literal",
        "raw_string_literal",
        "assignment_expression",
        "arguments",
    ] {
        tree_sitter::Query::new(&language, &format!("({kind}) @node")).unwrap();
    }
}

#[test]
fn rust_format_capture_keeps_closure_shadowing_exclusion() {
    let source = "pub fn target() {}\nfn run() { let f = |target: u32| format!(\"{target}\"); let _: fn() = target; }\n";
    let tree = parser::parse_source(source.as_bytes(), LangId::Rust).unwrap();
    let kinds = definition_node_kinds(LangId::Rust);
    let refs = collect_single_refs_for_test(
        tree.root_node(),
        source.as_bytes(),
        "target",
        "test.rs",
        kinds,
        LangId::Rust,
    );
    assert_eq!(refs.len(), 2, "定義と closure 外の参照だけ: {refs:?}");
    assert_eq!(
        count_refs_for_test(
            tree.root_node(),
            source.as_bytes(),
            &["target".into()],
            kinds,
            LangId::Rust,
            1
        ),
        [1]
    );
}
