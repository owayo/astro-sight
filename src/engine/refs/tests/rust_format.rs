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

/// 行継続 (`\` と改行と続く空白) をはさんだ名前も、キャプチャとして取り出す。
#[test]
fn rust_format_capture_decodes_line_continuations() {
    let source = "fn run() {\n    format!(\"{TO\\\n        KEN}\");\n    format!(\"{TO\\\r\n\tKEN:wid\\\n   th$}\");\n}\n";
    assert_eq!(capture_names(source), ["TOKEN", "TOKEN", "width"]);
}

/// 復号した写しは、キャプチャの抽出と同じ規則でエスケープを読む。
/// 不正なエスケープでは打ち切らず、`\` を残して続ける。
#[test]
fn rust_format_decode_escapes_follows_literal_rules() {
    use crate::engine::refs::definition::rust_format::decode_escapes;
    for (source, expected) in [
        (r#"{\x54OKEN}"#, "{TOKEN}"),
        (r#"{\u{54}OK\u{0_045}N}"#, "{TOKEN}"),
        ("{TO\\\n        KEN}", "{TOKEN}"),
        ("{TO\\\r\n\t KEN}", "{TOKEN}"),
        (r#"\n\r\t\0\\\'\""#, "\n\r\t\0\\'\""),
        // `\\` の後ろはエスケープにしない。
        (r#"{\\x54OKEN}"#, r#"{\x54OKEN}"#),
        // 不正なエスケープ・範囲外の値・閉じない波括弧はそのまま残して続ける。
        (
            r#"\q\x80\x7\u{D800}\u{110000}\u{41"#,
            r#"\q\x80\x7\u{D800}\u{110000}\u{41"#,
        ),
        (r#"\d+ \x41"#, r#"\d+ A"#),
        ("tail\\", "tail\\"),
        ("名前\\u{524d}", "名前前"),
    ] {
        assert_eq!(
            String::from_utf8(decode_escapes(source.as_bytes())).unwrap(),
            expected,
            "{source:?}"
        );
    }
}

/// 事前フィルタは、生のソースに無い名前を、エスケープを復号した写しでだけ見つける。
/// 印が揃っても綴りの無い名前は出現なしのままで、全件解析に回さない。
#[test]
fn rust_format_prefilter_searches_decoded_view_only_for_missing_names() {
    let names = vec!["TOKEN".to_string(), "OTHER".to_string(), "Seen".to_string()];
    let acs = build_batch_acs(&names).unwrap();
    let present = |source: &str| {
        let mut found: Vec<usize> = ac_present_indices_multi(&acs, source.as_bytes(), names.len())
            .into_iter()
            .collect();
        found.sort_unstable();
        found
    };
    // 行継続だけで綴った名前 (修正前は印にならず見落としていた)。
    assert_eq!(present("format!(\"{TO\\\n    KEN}\");"), [0]);
    assert_eq!(present("format!(\"{TO\\\r\n    KEN}\");"), [0]);
    assert_eq!(present("format!(\"{\\x54OKEN} {\\u{53}een}\");"), [0, 2]);
    // 印が揃っても綴りが無ければ出現なし (修正前は全部の名前を出現ありとしていた)。
    assert!(present("format!(\"\\x41 {value}\"); // \\\n").is_empty());
    // `\\` で始まる綴りは名前にならない。生のソースでの一致は従来どおり。
    assert!(present("format!(\"{\\\\x54OKEN}\");").is_empty());
    assert_eq!(present("let seen = TOKEN; format!(\"\\x41\");"), [0, 2]);
    // 書式マクロの印が無いファイルは復号しない (生のソースだけで判定する)。
    assert!(present("let s = \"{TO\\\n    KEN}\";").is_empty());

    // リテラルの外のバックスラッシュ・未完のエスケープ・raw / byte 文字列の後でも、
    // 続くリテラルの本文は先頭から復号される。
    let noisy = concat!(
        "// C:\\path \\u{ \\x\n",
        "let c = '\\\\'; let r = r\"\\\"; let b = b\"\\x80\\\\\";\n",
        "let u = \"\\u{41\"; format!(\"{TO\\\n    KEN}\");\n",
    );
    assert_eq!(present(noisy), [0]);

    let ac = build_ac_case_insensitive(&names).unwrap();
    let mut single: Vec<usize> =
        ac_present_indices(&ac, b"println!(\"{TO\\\n KEN}\");", names.len())
            .into_iter()
            .collect();
    single.sort_unstable();
    assert_eq!(single, [0]);
}

/// 読み込みに失敗したファイルのストリーム走査は、行継続も印として扱い、2 種類の印が
/// 離れたチャンクにどちらの順で現れても、全部の名前を参照しうるとする。
#[test]
fn rust_format_stream_scan_keeps_continuation_markers_across_chunks() {
    let names = vec!["TOKEN".to_string(), "OTHER".to_string()];
    let acs = build_batch_acs(&names).unwrap();
    let padding = " ".repeat(40);
    for source in [
        "format!(\"{TO\\\n    KEN}\");".to_string(),
        format!("format!(\"x\");{padding}let s = \"a\\\n b\";"),
        format!("let s = \"a\\\r\n b\";{padding}println!(\"x\");"),
    ] {
        for chunk in 1..=16 {
            assert_eq!(
                stream_present_indices_from(&acs, &names, source.as_bytes(), chunk, u64::MAX),
                Some(vec![0, 1]),
                "chunk={chunk}: {source:?}"
            );
        }
    }
    // 対照: 書式マクロの印が無ければ、行継続があっても生のソースの一致だけ。
    let plain = format!("let s = \"a\\\n b\";{padding}let t = TOKEN;");
    for chunk in 1..=16 {
        assert_eq!(
            stream_present_indices_from(&acs, &names, plain.as_bytes(), chunk, u64::MAX),
            Some(vec![0]),
            "chunk={chunk}"
        );
    }
}
