//! API シグネチャの抽出と正規化。言語別の binding shape と Tauri / TypeScript の正規化を含む。

use super::*;

/// dead_symbols のうち、宣言行が今回の diff の追加行 (`+` 行) と重なるもののみを残す。
///
/// `--dead-scope touched-symbols` の実装。`review --hook` のデフォルトとして使われ、
/// 「changed file 内に元からあった dead」がレビューノイズとして毎回出る UX 問題を
/// 解消する。
///
/// 注意: `HunkInfo` の `new_start` / `new_count` は context 行も含むため
/// hunk 範囲全体を「touched」と扱うと既存 dead まで残してしまう。ここでは
/// `extract_changed_new_lines` で **実際に追加された行** だけを set 化して照合する。
pub(crate) fn extract_symbol_lines(
    dir: &str,
    file_path: &str,
) -> Option<std::collections::HashMap<String, usize>> {
    use std::collections::HashMap;
    let full = std::path::Path::new(dir).join(file_path);
    let utf8 = camino::Utf8Path::new(full.to_str()?);
    let source = parser::read_file(utf8).ok()?;
    let lang_id = parser::detect_lang(utf8, &source).ok()?;

    let symbols = if let crate::language::DetectedLang::LexerOnly(lexer_lang) = lang_id.detected() {
        crate::engine::lexer::extract_symbols(&source, lexer_lang)
    } else {
        let tree = parser::parse_source(&source, lang_id).ok()?;
        crate::engine::symbols::extract_symbols(tree.root_node(), &source, lang_id).ok()?
    };

    let mut map = HashMap::new();
    for s in symbols {
        // 同名シンボルが複数ある場合、最初に出現した行を保持する。
        map.entry(s.name).or_insert(s.range.start.line);
    }
    Some(map)
}

/// シンボルの種類に応じた API シグネチャを抽出する。
/// 関数/メソッド → 宣言行、struct/enum/trait/interface/class → 宣言行のみ。
///
/// クラス/型は宣言行（`class Foo(Bar):` や `struct Foo {` など）のみをシグネチャとする。
/// 本体（メソッド本体や private フィールド）の変更でクラス全体の API 変更として
/// 再検出されるのを避けるため、メンバーの集約はしない。
/// メンバー個々の変更は method シンボル単独で検出される。
///
/// function / method の場合は tree-sitter ノードで「宣言開始から body 直前まで」を
/// 抽出し、whitespace を正規化して signature とする。これにより `where` 句や複数行
/// generics で先頭行が同一でも引数列が変わったケース (Issue
/// 2026-05-14-rename-and-multiline-signature) を検出できる。
/// 関数/メソッドノードの body 開始 byte を返す。tree-sitter の "body" フィールドを優先し、
/// 取得できない grammar (tree-sitter-kotlin 0.3.5 の `function_declaration` は
/// `fields: []` でフィールド名を持たず、body は `function_body` 型の直接子) では直接の
/// named child から既知の body ノード kind を fallback で探す。body を持たない宣言
/// (Swift protocol requirement / Rust trait fn / Kotlin abstract fun) では None を返し、
/// 呼び出し側が `end_byte()` (= 宣言全体 = 署名のみ) に倒す。
/// これを入れないと body フィールドを持たない言語で「関数全体」が署名になり、
/// body のみ変更が api.mod に誤検出される (Kotlin body-only 変更の false positive 対策)。
pub(crate) fn function_body_start_byte(node: tree_sitter::Node<'_>) -> Option<usize> {
    if let Some(body) = node.child_by_field_name("body") {
        return Some(body.start_byte());
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|child| {
            matches!(
                child.kind(),
                "function_body" | "block" | "statement_block" | "compound_statement"
            )
        })
        .map(|child| child.start_byte())
}

/// body が無い (interface method / abstract 等) や node 取得失敗時は先頭行を fallback。
pub(crate) fn extract_api_signature(
    sym: &crate::models::symbol::Symbol,
    root: tree_sitter::Node<'_>,
    source: &[u8],
    lines: &[&str],
    lang_id: crate::language::LangId,
) -> String {
    use crate::models::symbol::SymbolKind;
    if matches!(sym.kind, SymbolKind::Function | SymbolKind::Method) {
        let start = tree_sitter::Point {
            row: sym.range.start.line,
            column: sym.range.start.column,
        };
        let end = tree_sitter::Point {
            row: sym.range.end.line,
            column: sym.range.end.column,
        };
        if let Some(node) = root.descendant_for_point_range(start, end) {
            let mut cur = node;
            loop {
                match cur.kind() {
                    "function_item"
                    | "function_declaration"
                    | "function_definition"
                    | "method_declaration"
                    | "method_definition"
                    | "function_signature_item"
                    // Swift protocol requirement (body なしの宣言)。複数行 requirement でも
                    // 先頭行 fallback でなく AST から signature 全体を抽出する (codex 指摘)。
                    | "protocol_function_declaration" => {
                        let s = cur.start_byte();
                        let e =
                            function_body_start_byte(cur).unwrap_or_else(|| cur.end_byte());
                        // TS/TSX の関数 destructured params (`function foo({ a, b }: T)`) は
                        // `{ ... }` 内の variable 列が変わっても呼び出し側契約 (`: T` 型注釈)
                        // に影響しないため、signature 比較から除外する。React の Props
                        // 拡張 (optional prop 追加 + destructure 受け取り追加) で api.mod に
                        // 出る false positive を防ぐ (Issue
                        // 2026-05-28-api-mod-optional-props-additive 対応)。
                        if matches!(
                            lang_id,
                            crate::language::LangId::Typescript | crate::language::LangId::Tsx
                        ) {
                            return normalize_typescript_destructure_signature(cur, source, s, e);
                        }
                        // Tauri command (`#[tauri::command]` / `#[command]`) の自動注入型引数
                        // (AppHandle / State / Window 等) は実行時に Tauri が注入し JS 側 invoke()
                        // の引数には現れないため、signature 比較から除外する
                        // (Issue 2026-05-29-swift-sidecar-api-mod パターンB)。
                        if lang_id == crate::language::LangId::Rust
                            && let Some(sig) =
                                normalize_rust_tauri_command_signature(cur, source, s, e)
                        {
                            return sig;
                        }
                        if let Some(bytes) = source.get(s..e) {
                            return normalize_signature_whitespace(bytes);
                        }
                        break;
                    }
                    _ => {}
                }
                match cur.parent() {
                    Some(p) => cur = p,
                    None => break,
                }
            }
        }
    }

    // フォールバック: 先頭行のみ
    lines
        .get(sym.range.start.line)
        .unwrap_or(&"")
        .trim()
        .to_string()
}

/// 値バインディング (const / static / export const) の宣言から抽出した shape 情報。
/// initializer (= 右辺) を除いた宣言の骨格と、value-only 変更を安全に判定するための補助情報。
pub(crate) struct BindingShape {
    /// initializer を除いた正規化済み宣言テキスト (名前・型・visibility・binding kind を含む)。
    shape: String,
    /// 不変バインディング (Rust `const` / 非 mut `static`、TS/JS `const`) なら true。
    /// mutable (`static mut` / `let` / `var`) は false。
    is_const_binding: bool,
    /// 型注釈を持つなら true (TS の型注釈なし initializer の安全判定に使う)。
    has_type_annotation: bool,
    /// initializer が scalar literal (数値 / 文字列 / 真偽値 / null 等) なら true。
    /// 関数 / object / array / call 等の複雑な式は false。
    initializer_is_scalar: bool,
}

/// `node` を起点に、指定 kind のいずれかに最初に一致する子孫ノードを深さ優先で探す。
/// signature 文字列は単一宣言なので export_statement 等のラップを潜るために使う。
pub(crate) fn find_first_descendant_of_kinds<'a>(
    node: tree_sitter::Node<'a>,
    kinds: &[&str],
) -> Option<tree_sitter::Node<'a>> {
    if kinds.contains(&node.kind()) {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(found) = find_first_descendant_of_kinds(child, kinds) {
            return Some(found);
        }
    }
    None
}

/// value 手前で切った宣言テキストを正規化する。末尾に残る `=` と前後・連続空白を畳む。
pub(crate) fn normalize_binding_shape_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let trimmed = s.trim_end();
    // value の直前で切ると末尾に `= ` が残るため取り除く。
    let without_eq = trimmed
        .strip_suffix('=')
        .map(str::trim_end)
        .unwrap_or(trimmed);
    without_eq.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// signature 文字列を AST パースし、値バインディングなら initializer を除いた shape を返す。
/// 対象外 (関数 / 型 / バインディング以外) や抽出失敗時は None を返し、呼び出し側は保守的に
/// 従来どおり api.mod へ倒す (codex 設計合意: テキストの `=` 分割ではなく AST ベース)。
pub(crate) fn extract_binding_shape(
    sig: &str,
    lang_id: crate::language::LangId,
) -> Option<BindingShape> {
    // lexer-only 言語は tree-sitter を持たないため対象外。
    if lang_id.is_lexer_only() {
        return None;
    }
    let source = sig.as_bytes();
    let tree = parser::parse_source(source, lang_id).ok()?;
    let root = tree.root_node();
    match lang_id {
        crate::language::LangId::Rust => {
            let decl = find_first_descendant_of_kinds(root, &["const_item", "static_item"])?;
            extract_rust_binding_shape(decl, source)
        }
        crate::language::LangId::Typescript
        | crate::language::LangId::Tsx
        | crate::language::LangId::Javascript => {
            let decl = find_first_descendant_of_kinds(
                root,
                &["lexical_declaration", "variable_declaration"],
            )?;
            extract_js_binding_shape(decl, source)
        }
        _ => None,
    }
}

/// Rust の const_item / static_item から shape を抽出する。
pub(crate) fn extract_rust_binding_shape(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<BindingShape> {
    // static mut は mutable_specifier を子に持つ。const は常に不変。
    let mut cursor = node.walk();
    let is_mut = node
        .children(&mut cursor)
        .any(|c| c.kind() == "mutable_specifier");
    let value = node.child_by_field_name("value");
    let has_type_annotation = node.child_by_field_name("type").is_some();
    let shape_end = value
        .map(|v| v.start_byte())
        .unwrap_or_else(|| node.end_byte());
    let shape_bytes = source.get(node.start_byte()..shape_end)?;
    let initializer_is_scalar = value.map(rust_value_is_scalar).unwrap_or(false);
    Some(BindingShape {
        shape: normalize_binding_shape_text(shape_bytes),
        is_const_binding: !is_mut,
        has_type_annotation,
        initializer_is_scalar,
    })
}

/// TS/JS の lexical_declaration / variable_declaration から shape を抽出する。
pub(crate) fn extract_js_binding_shape(
    node: tree_sitter::Node<'_>,
    source: &[u8],
) -> Option<BindingShape> {
    // binding kind (`const` / `let` / `var`) を最初の anonymous child から判定する。
    let mut decl_cursor = node.walk();
    let binding_kw = node
        .children(&mut decl_cursor)
        .find(|c| matches!(c.kind(), "const" | "let" | "var"))
        .map(|c| c.kind());
    let is_const_binding = binding_kw == Some("const");

    // 複数 declarator (`const a = 1, b = 2;`) は shape 抽出が壊れるため対象外。
    let mut declarators = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "variable_declarator" {
            declarators.push(child);
        }
    }
    if declarators.len() != 1 {
        return None;
    }
    let declarator = declarators[0];
    let value = declarator.child_by_field_name("value");
    let has_type_annotation = declarator.child_by_field_name("type").is_some();

    // visibility (export) を shape に含めるため、親が export_statement なら起点を遡る。
    let shape_start = match node.parent() {
        Some(p) if p.kind() == "export_statement" => p.start_byte(),
        _ => node.start_byte(),
    };
    let shape_end = value
        .map(|v| v.start_byte())
        .unwrap_or_else(|| declarator.end_byte());
    let shape_bytes = source.get(shape_start..shape_end)?;
    let initializer_is_scalar = value.map(js_value_is_scalar).unwrap_or(false);
    Some(BindingShape {
        shape: normalize_binding_shape_text(shape_bytes),
        is_const_binding,
        has_type_annotation,
        initializer_is_scalar,
    })
}

/// Rust の値ノードが scalar literal かを判定する (型注釈なし経路の安全弁、誤検出側に倒す)。
pub(crate) fn rust_value_is_scalar(value: tree_sitter::Node<'_>) -> bool {
    matches!(
        value.kind(),
        "integer_literal"
            | "float_literal"
            | "string_literal"
            | "raw_string_literal"
            | "char_literal"
            | "boolean_literal"
    )
}

/// JS/TS の値ノードが scalar literal かを判定する。関数 / object / array / call は false。
pub(crate) fn js_value_is_scalar(value: tree_sitter::Node<'_>) -> bool {
    matches!(
        value.kind(),
        "number" | "string" | "true" | "false" | "null" | "undefined"
    )
}

/// old/new signature が「const / 非 mut static / export const の値のみ変更 (shape 不変)」かを
/// 判定する。true なら api.mod ではなく const_value_changes (informational) に振り分ける。
///
/// gate: (1) kind が value binding (constant/variable)、(2) 言語が Rust/TS/TSX/JS、
/// (3) 両者が不変バインディング、(4) shape 一致、(5) TS で型注釈なしなら両者 scalar literal。
/// いずれか外れる / 抽出失敗時は false を返し、保守的に api.mod へ倒す。
pub(crate) fn is_const_value_only_change(
    old_sig: &str,
    new_sig: &str,
    kind: &str,
    lang_id: crate::language::LangId,
) -> bool {
    // 値バインディングの kind のみ (Rust const/static="constant"、TS/JS const="variable")。
    if !matches!(kind, "constant" | "variable") {
        return false;
    }
    if !matches!(
        lang_id,
        crate::language::LangId::Rust
            | crate::language::LangId::Typescript
            | crate::language::LangId::Tsx
            | crate::language::LangId::Javascript
    ) {
        return false;
    }
    let (Some(old), Some(new)) = (
        extract_binding_shape(old_sig, lang_id),
        extract_binding_shape(new_sig, lang_id),
    ) else {
        return false;
    };
    // mutable バインディング (static mut / let / var) は demote しない。
    if !old.is_const_binding || !new.is_const_binding {
        return false;
    }
    // shape (名前・型・visibility・binding kind) が変われば破壊的変更の可能性 → api.mod。
    if old.shape != new.shape {
        return false;
    }
    // TS/JS で型注釈がない場合、関数 / object / array / call initializer は shape 推定が
    // 危険なため scalar literal 同士のときだけ demote する (codex 指摘)。
    if matches!(
        lang_id,
        crate::language::LangId::Typescript
            | crate::language::LangId::Tsx
            | crate::language::LangId::Javascript
    ) {
        let both_typed = old.has_type_annotation && new.has_type_annotation;
        let both_scalar = old.initializer_is_scalar && new.initializer_is_scalar;
        if !both_typed && !both_scalar {
            return false;
        }
    }
    true
}

/// Tauri command の自動注入型 (実行時に Tauri が注入し JS-facing な invoke() 引数に現れない型)。
/// `Channel<T>` は JS 側から渡す引数なので含めない (signature 差分の対象に残す)。
pub(crate) const TAURI_INJECTED_TYPES: &[&str] = &[
    "AppHandle",
    "Window",
    "Webview",
    "WebviewWindow",
    "State",
    "Request",
    "CommandScope",
    "GlobalScope",
];

/// Rust の型ノードから base 名 (パス・参照・ジェネリクスを剥がした末尾型名) を取り出す。
pub(crate) fn rust_type_base_name(ty: tree_sitter::Node<'_>, source: &[u8]) -> Option<String> {
    match ty.kind() {
        "type_identifier" => ty.utf8_text(source).ok().map(str::to_string),
        // tauri::AppHandle → name 子 'AppHandle'
        "scoped_type_identifier" => ty
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(source).ok())
            .map(str::to_string),
        // State<'_, T> → base 'State'
        "generic_type" => ty
            .child_by_field_name("type")
            .and_then(|t| rust_type_base_name(t, source)),
        // &State<...> / &AppHandle → 内側の型
        "reference_type" => ty
            .child_by_field_name("type")
            .and_then(|t| rust_type_base_name(t, source)),
        _ => None,
    }
}

/// function_item が Tauri command 属性 (`#[tauri::command]` / `#[command]`) を持つか判定する。
/// Rust では属性は function_item の前方兄弟 (attribute_item) に並ぶ。
pub(crate) fn rust_fn_has_tauri_command_attr(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
) -> bool {
    let mut sib = fn_node.prev_sibling();
    while let Some(s) = sib {
        match s.kind() {
            "attribute_item" => {
                if let Ok(text) = s.utf8_text(source) {
                    let inner = text
                        .trim_start_matches("#[")
                        .trim_start_matches("#![")
                        .trim_end_matches(']')
                        .trim();
                    if inner == "tauri::command"
                        || inner.starts_with("tauri::command(")
                        || inner == "command"
                        || inner.starts_with("command(")
                    {
                        return true;
                    }
                }
            }
            // 属性とコメントは読み飛ばし、それ以外に到達したら属性列の終端
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sib = s.prev_sibling();
    }
    false
}

/// Tauri command 関数の signature から自動注入型引数を除外して返す。
/// Tauri command でなければ None を返し、呼び出し側で通常の signature 抽出にフォールバックする。
pub(crate) fn normalize_rust_tauri_command_signature(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
    s: usize,
    e: usize,
) -> Option<String> {
    if !rust_fn_has_tauri_command_attr(fn_node, source) {
        return None;
    }
    let params = fn_node.child_by_field_name("parameters")?;
    let mut kept: Vec<String> = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        match child.kind() {
            "parameter" => {
                let injected = child
                    .child_by_field_name("type")
                    .and_then(|t| rust_type_base_name(t, source))
                    .is_some_and(|n| TAURI_INJECTED_TYPES.contains(&n.as_str()));
                if !injected && let Ok(t) = child.utf8_text(source) {
                    kept.push(t.to_string());
                }
            }
            "self_parameter" => {
                if let Ok(t) = child.utf8_text(source) {
                    kept.push(t.to_string());
                }
            }
            _ => {}
        }
    }
    let prefix = source.get(s..params.start_byte())?;
    let suffix = source.get(params.end_byte()..e)?;
    let rebuilt = format!(
        "{}({}){}",
        String::from_utf8_lossy(prefix),
        kept.join(", "),
        String::from_utf8_lossy(suffix)
    );
    Some(normalize_signature_whitespace(rebuilt.as_bytes()))
}

/// TS/TSX 関数の signature を抽出し、parameters 直下の `object_pattern`
/// (destructured params) を `{}` に正規化する。
///
/// `function foo({ a, b, c = 0 }: Props)` と `function foo({ a, b }: Props)` は
/// どちらも呼び出し側契約は `: Props` のみで、destructure 中身は内部 binding。
/// 正規化することで Props 拡張に伴う destructure 行の追加が api.mod に出ない。
///
/// 型注釈側の inline object type (`function foo({x}: {x: string, y: number})` の
/// `{x: string, y: number}`) は `type_annotation` 子なので置換対象外。
///
/// 「引数なし `()` から省略可能な destructured 引数追加」の互換性判定は、
/// signature 単独では行わない (型注釈変更だけ起きるケースを誤って互換扱いする
/// リスクがあるため)。両側 signature を見て判定するロジックは
/// [`is_ts_no_arg_to_optional_destructured_compatible`] が detect_api_changes
/// 経路で行う。
pub(crate) fn normalize_typescript_destructure_signature(
    fn_node: tree_sitter::Node<'_>,
    source: &[u8],
    start_byte: usize,
    end_byte: usize,
) -> String {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    if let Some(params) = fn_node.child_by_field_name("parameters") {
        collect_parameter_object_pattern_ranges(params, &mut ranges);
    }
    if ranges.is_empty() {
        if let Some(bytes) = source.get(start_byte..end_byte) {
            return normalize_signature_whitespace(bytes);
        }
        return String::new();
    }
    ranges.sort_by_key(|r| r.0);

    let mut buf: Vec<u8> = Vec::with_capacity(end_byte - start_byte);
    let mut cursor = start_byte;
    for (op_start, op_end) in &ranges {
        if *op_start < cursor || *op_end > end_byte {
            continue;
        }
        if let Some(bytes) = source.get(cursor..*op_start) {
            buf.extend_from_slice(bytes);
        }
        buf.extend_from_slice(b"{}");
        cursor = *op_end;
    }
    if let Some(bytes) = source.get(cursor..end_byte) {
        buf.extend_from_slice(bytes);
    }
    normalize_signature_whitespace(&buf)
}

/// TS/TSX の formal_parameters 直下にある `object_pattern` のバイト範囲を集める。
///
/// パラメータの `type_annotation` (inline object type など) には踏み込まないため、
/// 型注釈側の object type は影響を受けない。required_parameter / optional_parameter の
/// `pattern` フィールドを直接見て object_pattern かを判定する。
pub(crate) fn collect_parameter_object_pattern_ranges(
    params: tree_sitter::Node<'_>,
    ranges: &mut Vec<(usize, usize)>,
) {
    let mut cursor = params.walk();
    for child in params.children(&mut cursor) {
        match child.kind() {
            "required_parameter" | "optional_parameter" => {
                if let Some(pattern) = child.child_by_field_name("pattern")
                    && pattern.kind() == "object_pattern"
                {
                    ranges.push((pattern.start_byte(), pattern.end_byte()));
                }
            }
            // 無型 JS スタイル: parameter ノードがなく object_pattern が直接子に来る
            // ケース。安全側に倒して同様に正規化する (TS/TSX に限定済み)。
            "object_pattern" => {
                ranges.push((child.start_byte(), child.end_byte()));
            }
            _ => {}
        }
    }
}

/// signature bytes を whitespace で分割して 1 つの space で結合し正規化する。
/// 改行・タブ・連続スペース・末尾の `{` 直前空白を一括で潰す。
pub(crate) fn normalize_signature_whitespace(bytes: &[u8]) -> String {
    std::str::from_utf8(bytes)
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
