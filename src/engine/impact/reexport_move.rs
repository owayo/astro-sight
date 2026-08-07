//! 「シンボル定義を別モジュールへ移動し、旧公開パスを再輸出 (`pub use`) で維持した」
//! 変更を diff から検出するための索引。
//!
//! ## 何を解決するか
//!
//! 定義をモジュール間で移動し、旧パスを `pub use` で維持すると、旧パス側に残る
//! **未変更の利用行**が「未解決の影響」として blocking 報告されていた:
//!
//! ```text
//! // background.rs (移動先)
//! +pub const CONST_A: f32 = BASE_Y + BAND_H * 0.5;
//!
//! // consumer.rs (移動元 = 旧公開パス)
//! -pub const CONST_A: f32 = -50.0;
//! +pub use super::background::CONST_A;
//!  fn placement(&self) -> f32 { CONST_A + self.size * 0.5 }   // ← 無変更なのに blocking
//! ```
//!
//! 同一 diff 内で名前解決が完結しており修正箇所は存在しないため、この形の参照は
//! `CallerRoute::Informational` へ降格する (Issue 2026-08-07-reexport-treated-as-unresolved-impact)。
//!
//! ## 抑制条件を連言にする理由
//!
//! 個々の条件を**単独の**抑制条件にすると検出漏れ (FN) を生む:
//!
//! - 「影響先ファイルも同一 diff で変更済み」だけ → 無関係な編集でも抑制されてしまう
//! - 「同一 diff に `use` が追加された」だけ → 別名・glob・可視性で実際の解決先が異なりうる
//! - 「定数値が変わっていない」だけ → 値が同じでも公開元・型・依存方向は変わる
//!
//! そのため本索引は次の 3 つが**すべて**揃った場合だけ成立させる:
//!
//! 1. 参照ファイルが同一 diff で自前の同名定義を削除している (= 単なる import 追加ではなく移動)
//! 2. 参照ファイルが同一 diff で、その名前を束縛する `use` / `pub use` を追加している
//! 3. その `use` のモジュールパスが影響元ファイルへ解決でき、影響元が同一 diff で
//!    同名の定義を追加している (= 移動先が確かにそのファイル)
//!
//! 別名 (`as OTHER`) と glob (`use x::*`) は解決先を静的に確定できないため成立させない
//! (fail-closed)。対象は Rust のみで、他言語は常に `false` を返す。
use std::collections::HashMap;

use crate::engine::diff::{HunkBodyLine, HunkProgress, parse_hunk_header};

/// Rust の item 定義を導入するキーワード。`rust_line_defines` で「キーワードの次の
/// 識別子」を定義名として照合する。`use` は定義ではないので**含めない**
/// (含めると再輸出行自身を「定義」と誤認し、条件 1 が常に成立してしまう)。
const RUST_DEFINITION_KEYWORDS: &[&str] = &[
    "const",
    "static",
    "fn",
    "struct",
    "enum",
    "trait",
    "type",
    "union",
    "mod",
    "macro_rules",
];

/// 定義キーワードと名前の間に挟まりうる修飾子。名前として採択せず読み飛ばす。
/// (`pub static mut COUNTER` の `mut`、`pub const unsafe fn f` の `unsafe` を
/// 定義名と誤認しないため。`extern "C"` の `"C"` は文字列リテラル除去で消える。)
const RUST_NAME_MODIFIERS: &[&str] = &["mut", "unsafe", "extern", "async", "default"];

/// 初期化子 (`=` 以降) を宣言ヘッダから落とす対象の item 種別。
///
/// `const` / `static` だけに限定するのが要点:
/// - `type Alias = Foo;` で `=` を切ると別名先の型変更を見逃す (FN)
/// - `struct Foo<T = Bar>` のデフォルト型パラメータを切ると既定型の変更を見逃す (FN)
/// - `const fn f<const N: usize = 1>(a: i32)` は **item 種別が `fn`** なので対象外。
///   ここを「行に const が出現するか」で判定すると、const generics の既定値で
///   引数リストごと切り落とし、引数変更を見逃す (FN)。
const RUST_INITIALIZER_KINDS: &[&str] = &["const", "static"];

/// 追加された `use` のモジュールパス。crate root の位置は Cargo の設定に依存して
/// 静的に確定できないため、相対 (`self` / `super`) と `crate` 起点で解決規則を分ける。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModulePath {
    /// `self::a::b` / `super::a::b` — 参照ファイルのモジュール chain から相対解決する。
    /// `supers` は `super` の個数 (`self` は 0)。
    Relative {
        supers: usize,
        segments: Vec<String>,
    },
    /// `crate::a::b` — crate root が不明なため、影響元のモジュール chain に対する
    /// suffix 一致 + 残余 prefix が `src` で終わることを要求する。
    CrateRooted { segments: Vec<String> },
}

/// 参照ファイルで「自前定義の削除 + 再輸出の追加」が揃ったシンボル 1 件分の候補。
#[derive(Debug, Clone)]
struct MoveCandidate {
    /// 追加された `use` のモジュールパス。
    module: ModulePath,
    /// 削除された定義の宣言ヘッダ (`definition_header` で正規化済み)。
    /// 移動先に**同一ヘッダ**の定義が現れることを要求し、移動と同時にシグネチャが
    /// 変わったケース (呼び出し側が壊れる) を降格対象から外す。
    removed_header: String,
}

/// diff 1 回分の再輸出移動インデックス。
///
/// `analyze_impact_streaming` で 1 度だけ構築し、Pass 2 の routing 判定から参照する。
/// 保持量は diff のサイズで抑えられる (追加された `use` 行と定義行の名前のみ)。
#[derive(Debug, Default)]
pub(super) struct ReexportMoveIndex {
    /// 参照ファイルパス → シンボル名 → 移動候補。
    /// 条件 1 (自前定義の削除) を満たしたものだけを登録する。
    ///
    /// タプルキー `(String, String)` にすると `covers` の呼び出しごとに
    /// `to_string()` が 2 回走る (参照 1 件 × 変更ファイル数だけ呼ばれるホットパス)。
    /// 入れ子にして `&str` のまま引けるようにする。
    moves: HashMap<String, HashMap<String, Vec<MoveCandidate>>>,
    /// ファイルパス → シンボル名 → 同一 diff で**追加**された定義の宣言ヘッダ群。
    /// 条件 3 の照合に使う。
    added_defs: HashMap<String, HashMap<String, Vec<String>>>,
}

impl ReexportMoveIndex {
    /// unified diff を 1 度走査して索引を構築する。
    pub(super) fn build(diff_input: &str) -> Self {
        let per_file = collect_rust_changed_lines(diff_input);

        let mut added_defs: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
        let mut moves: HashMap<String, HashMap<String, Vec<MoveCandidate>>> = HashMap::new();

        for (path, lines) in &per_file {
            // 条件 3 の材料: このファイルで定義が追加されたシンボル名と宣言ヘッダ。
            let mut defs: HashMap<String, Vec<String>> = HashMap::new();
            for line in &lines.added {
                let header = definition_header(line);
                for name in rust_defined_names(line) {
                    defs.entry(name).or_default().push(header.clone());
                }
            }
            if !defs.is_empty() {
                added_defs.insert(path.clone(), defs);
            }

            // 条件 2: 追加された use / pub use が束縛する名前とモジュールパス。
            for line in &lines.added {
                let Some((module, names)) = parse_rust_use_line(line) else {
                    continue;
                };
                for name in names {
                    // 条件 1: 同一ファイルで同名の定義が削除されていること。
                    // これが「単なる import 追加」と「定義の移動」を分ける唯一の証拠。
                    let Some(removed) = lines
                        .removed
                        .iter()
                        .find(|removed| rust_line_defines(removed, &name))
                    else {
                        continue;
                    };
                    moves
                        .entry(path.clone())
                        .or_default()
                        .entry(name)
                        .or_default()
                        .push(MoveCandidate {
                            module: module.clone(),
                            removed_header: definition_header(removed),
                        });
                }
            }
        }

        Self { moves, added_defs }
    }

    /// `ref_path` の `symbol` 参照が、`source_path` への定義移動 + 再輸出で
    /// 既に解決済みかを判定する。
    pub(super) fn covers(&self, ref_path: &str, symbol: &str, source_path: &str) -> bool {
        let Some(candidates) = self
            .moves
            .get(ref_path)
            .and_then(|by_symbol| by_symbol.get(symbol))
        else {
            return false;
        };
        // 条件 3: 移動先が本当に影響元ファイルであること。名前だけ一致する別モジュールへの
        // 再輸出で降格しないよう、定義追加の有無とモジュールパス解決の両方を要求する。
        let Some(added_headers) = self
            .added_defs
            .get(source_path)
            .and_then(|defs| defs.get(symbol))
        else {
            return false;
        };
        candidates.iter().any(|candidate| {
            module_resolves_to(ref_path, &candidate.module, source_path)
                // 宣言ヘッダが一致する = 純粋な移動。移動と同時に引数や型が変わった場合は
                // 再輸出があっても呼び出し側が壊れるため blocking のまま残す。
                && added_headers
                    .iter()
                    .any(|header| header == &candidate.removed_header)
        })
    }
}

/// 定義行から宣言ヘッダ (本体・初期化子を除いた部分) を取り出して正規化する。
///
/// `pub const CONST_A: f32 = -50.0;`  → `pub const CONST_A: f32`
/// `pub fn f(a: i32) -> f32 {`        → `pub fn f(a: i32) -> f32`
/// `pub struct Foo<T = Bar> {`        → `pub struct Foo<T = Bar>`
/// `pub type Alias = Foo;`            → `pub type Alias = Foo`
///
/// 初期化子 (`=` 以降) を落とすのは `const` / `static` だけ。定数の**値**だけが変わった
/// 移動を同一ヘッダとして扱うため (値の変化自体は API 差分の `const_value` が別途報告する)。
/// 一方 `type` の別名先や `struct` のデフォルト型パラメータまで切り落とすと、
/// 呼び出し側が壊れる変更を見逃す (FN) ので `=` では切らない。
fn definition_header(line: &str) -> String {
    // item 種別で判断する (「行に const が出現するか」では `const fn` を取り違える)。
    let has_initializer = rust_definition(line)
        .is_some_and(|(kind, _)| RUST_INITIALIZER_KINDS.contains(&kind.as_str()));
    // リテラルを潰した (長さは不変の) 行で切断位置を求め、元の行に適用する。
    let scan = strip_literals(line);
    let cut = header_cut_pos(&scan, has_initializer);
    let head = cut.map_or(line, |ix| &line[..ix]);
    head.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(';')
        .trim()
        .to_string()
}

/// 1 ファイル分の追加行 / 削除行 (先頭の `+` / `-` を除いた本文)。
#[derive(Default)]
struct ChangedLines {
    added: Vec<String>,
    removed: Vec<String>,
}

/// unified diff から Rust ファイルの追加行 / 削除行だけを収集する。
///
/// ヘッダ判定の規約は `diff::extract_changed_new_lines` と揃える (hunk 本体を消費中は
/// ヘッダに見える行も本体行として扱い、本文中の `+++ b/...` で誤ってファイルが
/// 切り替わるのを防ぐ)。
fn collect_rust_changed_lines(input: &str) -> HashMap<String, ChangedLines> {
    let mut result: HashMap<String, ChangedLines> = HashMap::new();
    let mut current_path: Option<String> = None;
    let mut active_hunk: Option<HunkProgress> = None;

    for line in input.lines() {
        if let Some(progress) = active_hunk.as_mut() {
            let consumed = progress.consume(line);
            if let Some(path) = current_path.as_ref() {
                match consumed {
                    HunkBodyLine::Added(text) => {
                        result
                            .entry(path.clone())
                            .or_default()
                            .added
                            .push(text.to_string());
                    }
                    HunkBodyLine::Removed(text) => {
                        result
                            .entry(path.clone())
                            .or_default()
                            .removed
                            .push(text.to_string());
                    }
                    HunkBodyLine::Context | HunkBodyLine::Metadata => {}
                }
            }
            if progress.is_complete() {
                active_hunk = None;
            }
            continue;
        }

        if line.starts_with("--- ") {
            current_path = None;
        } else if let Some(path) = line.strip_prefix("+++ b/") {
            // Rust 以外は解析対象外 (本索引は Rust 限定)。
            current_path = path.ends_with(".rs").then(|| path.to_string());
        } else if line.starts_with("+++ ") {
            current_path = None;
        } else if line.starts_with("@@ ")
            && let Some(hunk) = parse_hunk_header(line)
        {
            active_hunk = Some(HunkProgress::new(&hunk));
        }
    }

    result
}

/// `line` が `name` の item 定義ヘッダかを判定する。
fn rust_line_defines(line: &str, name: &str) -> bool {
    rust_definition(line).is_some_and(|(_, defined)| defined == name)
}

/// `line` が定義しているシンボル名 (0 個か 1 個)。
fn rust_defined_names(line: &str) -> Vec<String> {
    rust_definition(line)
        .map(|(_, name)| vec![name])
        .unwrap_or_default()
}

/// `line` が定義している (item 種別, シンボル名) を返す。
///
/// 識別子トークン列を走査し、`RUST_DEFINITION_KEYWORDS` の直後に来る識別子を定義名、
/// その直前の定義キーワードを item 種別とする。`macro_rules! NAME` は `!` が
/// 区切り文字になるため同じ規則で拾える。
///
/// **最初の 1 件を見つけた時点で走査を打ち切る**。1 行は 1 item を宣言するのが通常で、
/// 続けて走査すると型注釈内のキーワードから偽の定義を拾う
/// (`pub const CALLBACK: fn(i32) -> i32 = f;` から `("fn", "i32")` が出る)。
/// 偽の定義は「参照ファイルが自前定義を削除した」という条件 1 を不当に成立させ、
/// 本来 blocking であるべき参照を降格しかねない。1 行に複数 item を書く病的な入力では
/// 検出数が減るだけで、抑制が成立しにくくなる = fail-closed 側に倒れる。
///
/// キーワードと修飾子は名前として採択せず読み飛ばす。これが無いと
/// `pub const fn f()` が `fn` を、`pub static mut C` が `mut` を、
/// `pub const unsafe fn f()` が `unsafe` を定義名と誤認し、本来成立すべき
/// 移動判定が成立しなくなる。`extern "C"` の `"C"` はリテラル除去で消す
/// (トークン化すると `C` が名前として採択されてしまうため)。
///
/// item 種別を返すのは `definition_header` が初期化子の有無を判断するため。
/// 「行に `const` が出現するか」では `const fn` を取り違える。
fn rust_definition(line: &str) -> Option<(String, String)> {
    let stripped = strip_literals(line);
    let mut pending_kind: Option<&str> = None;
    for token in stripped.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if token.is_empty() {
            continue;
        }
        if let Some(kw) = RUST_DEFINITION_KEYWORDS.iter().find(|kw| **kw == token) {
            // `const fn f` のようにキーワードが連続する場合は後勝ち (種別は `fn`)。
            pending_kind = Some(kw);
            continue;
        }
        if pending_kind.is_some() && RUST_NAME_MODIFIERS.contains(&token) {
            continue;
        }
        if let Some(kind) = pending_kind {
            return Some((kind.to_string(), token.to_string()));
        }
    }
    None
}

/// 文字列リテラル (`"..."`) と文字リテラル (`'x'`) の中身をスペースに潰す。
/// `extern "C"` の `C` のような「識別子に見えるがコードではないトークン」を
/// 走査から除き、`'{'` のようなリテラル内の括弧を切断位置判定から外すために使う。
///
/// **バイト長を変えない** (潰す範囲を同じ長さのスペースに置換する) ので、
/// この文字列上で求めた位置は元の行にそのまま使える。非 ASCII はリテラル外なら
/// そのまま残し、リテラル内ならバイト単位でスペースに置き換える。
///
/// ライフタイム (`&'a str`) は閉じ引用符を持たないため文字リテラルと区別する:
/// `'` の直後が `\` か、2 バイト先が `'` の場合だけ文字リテラルとみなす。
fn strip_literals(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out: Vec<u8> = bytes.to_vec();
    let mut ix = 0;
    while ix < bytes.len() {
        let quote = match bytes[ix] {
            b'"' => b'"',
            b'\'' => {
                let is_escaped_char = bytes.get(ix + 1) == Some(&b'\\');
                let is_simple_char = bytes.get(ix + 2) == Some(&b'\'');
                if !(is_escaped_char || is_simple_char) {
                    // ライフタイムはそのまま残す。
                    ix += 1;
                    continue;
                }
                b'\''
            }
            _ => {
                ix += 1;
                continue;
            }
        };
        let start = ix;
        ix += 1;
        while ix < bytes.len() {
            if bytes[ix] == b'\\' {
                ix += 2;
                continue;
            }
            if bytes[ix] == quote {
                break;
            }
            ix += 1;
        }
        let end = ix.min(out.len());
        for slot in out[start..end].iter_mut() {
            *slot = b' ';
        }
        ix = end + 1;
    }
    // すべて ASCII スペースへの置換なので UTF-8 として不正にはならない。
    String::from_utf8(out).unwrap_or_else(|_| line.to_string())
}

/// 宣言ヘッダの切断位置 (本体 `{` または初期化子 `=` のうち先に来るもの) を返す。
///
/// **括弧の入れ子の外側だけを対象にする**のが要点:
/// - `pub const VAL: [u8; { 1 + 1 }] = [0, 0];` の配列長ブロックの `{` で切らない
/// - `Type<Assoc = u32>` の型引数束縛や `==` / `=>` / `>=` 等の演算子で切らない
///
/// `->` と `=>` の `>` は山括弧の閉じではないため深さを減らさない
/// (`pub const F: fn(i32) -> i32 = foo;` を正しく切るために必要)。
///
/// リテラル除去済みの文字列を渡すこと (`'{'` のようなリテラル内の括弧を除くため)。
/// 除去は同じ長さのスペースへ置換するので、返る位置は元の行にそのまま使える。
fn header_cut_pos(line: &str, allow_initializer: bool) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut depth: i32 = 0;
    for (ix, &b) in bytes.iter().enumerate() {
        let prev = ix.checked_sub(1).map(|p| bytes[p]);
        let next = bytes.get(ix + 1).copied();
        match b {
            b'<' | b'(' | b'[' => depth += 1,
            b'>' => {
                // `->` の `>` と `=>` の `>` は閉じ括弧ではない。
                if prev != Some(b'-') && prev != Some(b'=') {
                    depth -= 1;
                }
            }
            b')' | b']' => depth -= 1,
            b'{' => {
                if depth <= 0 {
                    return Some(ix);
                }
                depth += 1;
            }
            b'}' => depth -= 1,
            b'=' if allow_initializer => {
                let is_operator = prev == Some(b'=')
                    || prev == Some(b'!')
                    || prev == Some(b'<')
                    || prev == Some(b'>')
                    || next == Some(b'=')
                    || next == Some(b'>');
                if depth <= 0 && !is_operator {
                    return Some(ix);
                }
            }
            _ => {}
        }
    }
    None
}

/// `use` / `pub use` 行を解析し、(モジュールパス, 束縛される名前) を返す。
///
/// 解決先を静的に確定できない形は `None` に倒す (fail-closed):
/// glob (`use a::*`)、別名 (`use a::B as C`)、ネストした波括弧、`crate` / `self` / `super`
/// 以外を起点とするパス (外部 crate の可能性がある)。
fn parse_rust_use_line(line: &str) -> Option<(ModulePath, Vec<String>)> {
    // 行末コメントを落とす (`pub use super::b::X; // moved from consumer`)。
    // 残すと `;` 除去後にコメント本文が use tree に混ざり、識別子検査で弾かれる。
    let line = line.split("//").next().unwrap_or(line);
    let mut rest = line.trim();
    // 可視性修飾子を剥がす (`pub`, `pub(crate)`, `pub(super)`, `pub(in path)`)。
    if let Some(after) = rest.strip_prefix("pub") {
        let after = match after.strip_prefix('(') {
            Some(paren) => paren.split_once(')')?.1,
            None => after,
        };
        // `pub` と `pubfoo` を区別する: 修飾子の直後は空白でなければならない。
        if !after.starts_with(char::is_whitespace) {
            return None;
        }
        rest = after.trim_start();
    }
    let after_use = rest.strip_prefix("use")?;
    if !after_use.starts_with(char::is_whitespace) {
        return None;
    }
    let tree = after_use.trim().trim_end_matches(';').trim();
    // glob と別名は解決先を確定できない。
    if tree.contains('*') || tree.contains(" as ") {
        return None;
    }

    let (prefix, names) = match tree.split_once('{') {
        Some((prefix, items)) => {
            let items = items.strip_suffix('}')?;
            // ネストした波括弧 (`use a::{b::{c}}`) は対象外。
            if items.contains('{') || items.contains('}') {
                return None;
            }
            let names: Vec<String> = items
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty() && *item != "self")
                .map(str::to_string)
                .collect();
            (prefix.trim().trim_end_matches(':'), names)
        }
        None => {
            let (prefix, name) = tree.rsplit_once("::")?;
            (prefix.trim(), vec![name.trim().to_string()])
        }
    };

    if names.is_empty() || names.iter().any(|n| !is_plain_identifier(n)) {
        return None;
    }

    let segments: Vec<&str> = prefix
        .split("::")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let module = classify_module_path(&segments)?;
    Some((module, names))
}

/// use のモジュールパス segments を `ModulePath` に分類する。
///
/// 起点が `crate` / `self` / `super` のいずれでもないパスは、2018 edition では
/// 外部 crate を指すため対象外にする (fail-closed)。
fn classify_module_path(segments: &[&str]) -> Option<ModulePath> {
    let mut iter = segments.iter().peekable();
    match iter.peek() {
        Some(&&"crate") => {
            iter.next();
            let rest: Vec<String> = iter.map(|s| s.to_string()).collect();
            // `use crate::NAME;` (crate root 直下) は module 階層を持たず照合できない。
            if rest.is_empty() {
                return None;
            }
            Some(ModulePath::CrateRooted { segments: rest })
        }
        Some(&&"self") => {
            iter.next();
            let rest: Vec<String> = iter.map(|s| s.to_string()).collect();
            if rest.is_empty() {
                return None;
            }
            Some(ModulePath::Relative {
                supers: 0,
                segments: rest,
            })
        }
        Some(&&"super") => {
            let mut supers = 0;
            while iter.peek() == Some(&&"super") {
                iter.next();
                supers += 1;
            }
            let rest: Vec<String> = iter.map(|s| s.to_string()).collect();
            if rest.is_empty() {
                return None;
            }
            Some(ModulePath::Relative {
                supers,
                segments: rest,
            })
        }
        _ => None,
    }
}

/// ファイルパスをモジュール chain へ変換する。
///
/// `src/game/background.rs` → `["src", "game", "background"]`
/// `src/game/background/mod.rs` → `["src", "game", "background"]` (mod/lib/main は畳む)
fn module_chain(path: &str) -> Option<Vec<&str>> {
    let normalized = path.trim_start_matches("./");
    let mut comps: Vec<&str> = normalized
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    let file = comps.pop()?;
    let stem = file.strip_suffix(".rs")?;
    if !matches!(stem, "mod" | "lib" | "main") {
        comps.push(stem);
    }
    Some(comps)
}

/// 追加された `use` のモジュールパスが影響元ファイルを指すか判定する。
fn module_resolves_to(ref_path: &str, module: &ModulePath, source_path: &str) -> bool {
    let Some(source_chain) = module_chain(source_path) else {
        return false;
    };
    match module {
        ModulePath::Relative { supers, segments } => {
            let Some(mut base) = module_chain(ref_path) else {
                return false;
            };
            // `super` 1 つにつき 1 階層戻る。crate root を越える指定は不正なので不成立。
            if base.len() < *supers {
                return false;
            }
            base.truncate(base.len() - supers);
            base.extend(segments.iter().map(String::as_str));
            base == source_chain
        }
        ModulePath::CrateRooted { segments } => {
            if source_chain.len() <= segments.len() {
                return false;
            }
            let split = source_chain.len() - segments.len();
            if source_chain[split..] != segments[..] {
                return false;
            }
            // crate root の位置は静的に確定できないため、標準的な Cargo レイアウト
            // (`src/` 直下が crate root) のみ成立させる。これが無いと
            // `crate::background` が `src/other/background.rs` にも一致してしまう。
            source_chain[..split].last() == Some(&"src")
        }
    }
}

/// 識別子として妥当か (use tree の項目が複雑な形でないことの確認)。
fn is_plain_identifier(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue の再現形をそのまま diff にしたもの。
    /// background.rs へ定義が移動し、consumer.rs は旧パスを再輸出で維持する。
    fn move_diff() -> String {
        "\
--- a/src/game/background.rs
+++ b/src/game/background.rs
@@ -1,2 +1,3 @@
 pub const BASE_Y: f32 = -70.0;
-pub const BAND_H: f32 = 30.0;
+pub const BAND_H: f32 = 40.0;
+pub const CONST_A: f32 = BASE_Y + BAND_H * 0.5;
--- a/src/game/consumer.rs
+++ b/src/game/consumer.rs
@@ -1,1 +1,1 @@
-pub const CONST_A: f32 = -50.0;
+pub use super::background::CONST_A;
"
        .to_string()
    }

    #[test]
    fn covers_reexport_move_from_sibling_module() {
        let index = ReexportMoveIndex::build(&move_diff());
        assert!(index.covers("src/game/consumer.rs", "CONST_A", "src/game/background.rs"));
    }

    // 対照 1: 移動元の定義削除が無い (単に import を足しただけ) なら降格しない。
    #[test]
    fn does_not_cover_when_old_definition_is_not_removed() {
        let diff = "\
--- a/src/game/background.rs
+++ b/src/game/background.rs
@@ -1,1 +1,2 @@
 pub const BASE_Y: f32 = -70.0;
+pub const CONST_A: f32 = 1.0;
--- a/src/game/consumer.rs
+++ b/src/game/consumer.rs
@@ -1,1 +1,2 @@
 // header
+pub use super::background::CONST_A;
";
        let index = ReexportMoveIndex::build(diff);
        assert!(!index.covers("src/game/consumer.rs", "CONST_A", "src/game/background.rs"));
    }

    // 対照 2: 影響元が定義を追加していない (移動先ではない) なら降格しない。
    #[test]
    fn does_not_cover_when_source_does_not_add_definition() {
        let diff = "\
--- a/src/game/background.rs
+++ b/src/game/background.rs
@@ -1,2 +1,2 @@
 pub const BASE_Y: f32 = -70.0;
-pub const BAND_H: f32 = 30.0;
+pub const BAND_H: f32 = 40.0;
--- a/src/game/consumer.rs
+++ b/src/game/consumer.rs
@@ -1,1 +1,1 @@
-pub const CONST_A: f32 = -50.0;
+pub use super::background::CONST_A;
";
        let index = ReexportMoveIndex::build(diff);
        assert!(!index.covers("src/game/consumer.rs", "CONST_A", "src/game/background.rs"));
    }

    // 対照 3: 別名付き再輸出は参照名を束縛しないので降格しない。
    #[test]
    fn does_not_cover_aliased_reexport() {
        let diff = move_diff().replace(
            "+pub use super::background::CONST_A;",
            "+pub use super::background::CONST_A as OTHER;",
        );
        let index = ReexportMoveIndex::build(&diff);
        assert!(!index.covers("src/game/consumer.rs", "CONST_A", "src/game/background.rs"));
    }

    // 対照 4: glob 再輸出は解決先を確定できないので降格しない。
    #[test]
    fn does_not_cover_glob_reexport() {
        let diff = move_diff().replace(
            "+pub use super::background::CONST_A;",
            "+pub use super::background::*;",
        );
        let index = ReexportMoveIndex::build(&diff);
        assert!(!index.covers("src/game/consumer.rs", "CONST_A", "src/game/background.rs"));
    }

    // 対照 5: 同名の別モジュールを再輸出している場合は降格しない。
    #[test]
    fn does_not_cover_when_module_path_points_elsewhere() {
        let diff = move_diff().replace(
            "+pub use super::background::CONST_A;",
            "+pub use super::other::CONST_A;",
        );
        let index = ReexportMoveIndex::build(&diff);
        assert!(!index.covers("src/game/consumer.rs", "CONST_A", "src/game/background.rs"));
    }

    // 対照 6: 移動と同時にシグネチャが変わった場合は、再輸出があっても呼び出し側が
    // 壊れるため blocking のまま残す (FN 防止)。対照として、同じ diff 形でシグネチャが
    // 一致していれば降格することも確認する。
    #[test]
    fn does_not_cover_move_with_signature_change() {
        let changed = "\
--- a/src/game/background.rs
+++ b/src/game/background.rs
@@ -1,1 +1,2 @@
 // header
+pub fn placement(a: i32, b: i32) -> i32 { a + b }
--- a/src/game/consumer.rs
+++ b/src/game/consumer.rs
@@ -1,1 +1,1 @@
-pub fn placement(a: i32) -> i32 { a }
+pub use super::background::placement;
";
        let index = ReexportMoveIndex::build(changed);
        assert!(!index.covers(
            "src/game/consumer.rs",
            "placement",
            "src/game/background.rs"
        ));

        // 対照: 引数が一致していれば純粋な移動なので降格する。
        let same = changed.replace(
            "+pub fn placement(a: i32, b: i32) -> i32 { a + b }",
            "+pub fn placement(a: i32) -> i32 { a * 2 }",
        );
        let index = ReexportMoveIndex::build(&same);
        assert!(index.covers(
            "src/game/consumer.rs",
            "placement",
            "src/game/background.rs"
        ));
    }

    // 定数は初期化子が変わっても宣言ヘッダが同じなら移動として扱う
    // (値の変化自体は API 差分の const_value が別途報告する)。
    #[test]
    fn definition_header_ignores_initializer_but_keeps_params() {
        assert_eq!(
            definition_header("pub const CONST_A: f32 = -50.0;"),
            "pub const CONST_A: f32"
        );
        assert_eq!(
            definition_header("pub const CONST_A: f32 = BASE_Y + BAND_H * 0.5;"),
            "pub const CONST_A: f32"
        );
        assert_ne!(
            definition_header("pub fn f(a: i32) {"),
            definition_header("pub fn f(a: i32, b: i32) {")
        );
        // 型が変われば別ヘッダ。
        assert_ne!(
            definition_header("pub const CONST_A: f32 = 1.0;"),
            definition_header("pub const CONST_A: f64 = 1.0;")
        );
    }

    #[test]
    fn covers_brace_grouped_reexport() {
        let diff = move_diff().replace(
            "+pub use super::background::CONST_A;",
            "+pub use super::background::{BASE_Y, CONST_A};",
        );
        let index = ReexportMoveIndex::build(&diff);
        assert!(index.covers("src/game/consumer.rs", "CONST_A", "src/game/background.rs"));
    }

    #[test]
    fn covers_crate_rooted_reexport() {
        let diff = move_diff().replace(
            "+pub use super::background::CONST_A;",
            "+pub use crate::game::background::CONST_A;",
        );
        let index = ReexportMoveIndex::build(&diff);
        assert!(index.covers("src/game/consumer.rs", "CONST_A", "src/game/background.rs"));
    }

    // crate 相対は `src/` 直下を crate root とみなす。residual prefix が `src` で
    // 終わらない一致は別モジュールの可能性があるため成立させない。
    #[test]
    fn crate_rooted_requires_src_prefix() {
        assert!(module_resolves_to(
            "src/game/consumer.rs",
            &ModulePath::CrateRooted {
                segments: vec!["game".into(), "background".into()]
            },
            "src/game/background.rs"
        ));
        assert!(!module_resolves_to(
            "src/game/consumer.rs",
            &ModulePath::CrateRooted {
                segments: vec!["background".into()]
            },
            "src/other/background.rs"
        ));
    }

    #[test]
    fn module_chain_folds_mod_rs() {
        assert_eq!(
            module_chain("src/game/background/mod.rs"),
            Some(vec!["src", "game", "background"])
        );
        assert_eq!(
            module_chain("src/game/background.rs"),
            Some(vec!["src", "game", "background"])
        );
        assert_eq!(module_chain("src/lib.rs"), Some(vec!["src"]));
        assert_eq!(module_chain("src/notrust.ts"), None);
    }

    #[test]
    fn self_path_resolves_to_child_module() {
        assert!(module_resolves_to(
            "src/game/consumer.rs",
            &ModulePath::Relative {
                supers: 0,
                segments: vec!["background".into()]
            },
            "src/game/consumer/background.rs"
        ));
    }

    // `crate::a::b::consumer` から `super::super` は `crate::a` を指す。
    #[test]
    fn nested_super_walks_up_multiple_levels() {
        assert!(module_resolves_to(
            "src/a/b/consumer.rs",
            &ModulePath::Relative {
                supers: 2,
                segments: vec!["background".into()]
            },
            "src/a/background.rs"
        ));
        // 1 階層違う先には一致しない。
        assert!(!module_resolves_to(
            "src/a/b/consumer.rs",
            &ModulePath::Relative {
                supers: 2,
                segments: vec!["background".into()]
            },
            "src/a/b/background.rs"
        ));
    }

    #[test]
    fn use_line_parsing_rejects_external_crate_root() {
        // 起点が crate/self/super でないパスは外部 crate の可能性があるため対象外。
        assert!(parse_rust_use_line("use serde::Serialize;").is_none());
    }

    // キーワード連続 (`const fn`) と修飾子 (`static mut`) で定義名を取り違えない。
    // 取り違えると条件 1 が成立せず、本来降格すべき移動が blocking のまま残る。
    #[test]
    fn defined_names_skip_keywords_and_modifiers() {
        assert_eq!(
            rust_defined_names("pub const fn placement(v: i32) -> i32 {"),
            vec!["placement".to_string()]
        );
        assert_eq!(
            rust_defined_names("pub static mut COUNTER: u32 = 0;"),
            vec!["COUNTER".to_string()]
        );
        assert_eq!(
            rust_defined_names("pub async fn run() {"),
            vec!["run".to_string()]
        );
        assert_eq!(
            rust_defined_names("pub unsafe fn raw() {"),
            vec!["raw".to_string()]
        );
    }

    // `const fn` の移動でも降格が成立する (上のトークン走査バグの E2E 相当)。
    #[test]
    fn covers_const_fn_move() {
        let diff = "\
--- a/src/game/background.rs
+++ b/src/game/background.rs
@@ -1,2 +1,3 @@
 pub const BASE_Y: f32 = -70.0;
-pub const BAND_H: f32 = 30.0;
+pub const BAND_H: f32 = 40.0;
+pub const fn placement(v: i32) -> i32 { v * 3 }
--- a/src/game/consumer.rs
+++ b/src/game/consumer.rs
@@ -1,1 +1,1 @@
-pub const fn placement(v: i32) -> i32 { v * 2 }
+pub use super::background::placement;
";
        let index = ReexportMoveIndex::build(diff);
        assert!(index.covers(
            "src/game/consumer.rs",
            "placement",
            "src/game/background.rs"
        ));
    }

    // `type` の別名先と `struct` のデフォルト型パラメータは `=` で切らない。
    // 切ると「別名先が変わった移動」を純粋な移動と誤認して降格してしまう (FN)。
    #[test]
    fn definition_header_keeps_type_alias_rhs_and_default_type_params() {
        assert_ne!(
            definition_header("pub type Alias = Foo;"),
            definition_header("pub type Alias = Bar;")
        );
        assert_ne!(
            definition_header("pub struct Foo<T = Bar> {"),
            definition_header("pub struct Foo<T = Baz> {")
        );
        // const / static は初期化子を落とす (値変更は移動として扱う)。
        assert_eq!(
            definition_header("pub static mut C: u32 = 1;"),
            definition_header("pub static mut C: u32 = 2;")
        );
    }

    // `const fn` の item 種別は `fn` なので `=` では切らない。
    // 「行に const が出現するか」で判定すると const generics の既定値 `= 1` で
    // 引数リストごと切り落とし、引数変更を見逃す (FN)。
    #[test]
    fn definition_header_const_fn_keeps_params_despite_const_generic_default() {
        let one = definition_header("pub const fn calc<const N: usize = 1>(a: i32) -> i32 {");
        let two =
            definition_header("pub const fn calc<const N: usize = 1>(a: i32, b: i32) -> i32 {");
        assert!(one.contains("(a: i32)"), "引数リストが残るべき: {one}");
        assert_ne!(one, two, "引数の増減はヘッダ差分として現れるべき");
    }

    // const の型指定内にある型引数束縛の `=` では切らない (型変更の見逃し防止)。
    #[test]
    fn definition_header_const_ignores_assoc_type_binding_equals() {
        assert_ne!(
            definition_header("pub const FOO: Type<Assoc = u32> = make();"),
            definition_header("pub const FOO: Type<Assoc = u64> = make();")
        );
        // 関数ポインタ型の `->` を山括弧の閉じと誤認しない。
        assert_eq!(
            definition_header("pub const F: fn(i32) -> i32 = foo;"),
            "pub const F: fn(i32) -> i32"
        );
    }

    // `extern "C"` の `"C"` や `unsafe` を定義名と誤認しない。
    #[test]
    fn defined_names_skip_extern_abi_and_unsafe() {
        assert_eq!(
            rust_defined_names("pub const unsafe fn raw(v: i32) -> i32 {"),
            vec!["raw".to_string()]
        );
        assert_eq!(
            rust_defined_names("pub const extern \"C\" fn ffi(v: i32) -> i32 {"),
            vec!["ffi".to_string()]
        );
    }

    // item 種別は後勝ち: `const fn f` の種別は fn、`const F` の種別は const。
    #[test]
    fn definitions_report_item_kind() {
        assert_eq!(
            rust_definition("pub const fn calc(a: i32) -> i32 {"),
            Some(("fn".to_string(), "calc".to_string()))
        );
        assert_eq!(
            rust_definition("pub const CONST_A: f32 = -50.0;"),
            Some(("const".to_string(), "CONST_A".to_string()))
        );
        // 型注釈内のキーワードから偽の定義を拾わない (走査は最初の 1 件で打ち切る)。
        assert_eq!(
            rust_definition("pub const CALLBACK: fn(i32) -> i32 = double;"),
            Some(("const".to_string(), "CALLBACK".to_string()))
        );
        assert_eq!(
            rust_defined_names("pub const CALLBACK: fn(i32) -> i32 = double;"),
            vec!["CALLBACK".to_string()]
        );
    }

    // 関数ポインタ型の定数は item 種別 const のままで、初期化子が落ちる。
    #[test]
    fn definition_header_function_pointer_const_drops_initializer() {
        assert_eq!(
            definition_header("pub const CALLBACK: fn(i32) -> i32 = double;"),
            "pub const CALLBACK: fn(i32) -> i32"
        );
        assert_eq!(
            definition_header("pub const CALLBACK: fn(i32) -> i32 = double;"),
            definition_header("pub const CALLBACK: fn(i32) -> i32 = triple;")
        );
    }

    // 配列長のブロック式や文字リテラル内の `{` で宣言ヘッダを切らない。
    #[test]
    fn definition_header_ignores_braces_inside_type_and_literals() {
        assert_eq!(
            definition_header("pub const VAL: [u8; { 1 + 1 }] = [0, 0];"),
            "pub const VAL: [u8; { 1 + 1 }]"
        );
        assert_eq!(
            definition_header("pub const BRACE: char = '{';"),
            "pub const BRACE: char"
        );
    }

    // ライフタイムを文字リテラルと誤認しない (`'a` で潰すと型が消える)。
    #[test]
    fn strip_literals_keeps_lifetimes_and_preserves_length() {
        let line = "pub const NAME: &'static str = \"fn foo\";";
        let stripped = strip_literals(line);
        assert_eq!(stripped.len(), line.len(), "位置照合のため長さは不変");
        assert!(
            stripped.contains("'static"),
            "ライフタイムは残る: {stripped}"
        );
        assert!(
            !stripped.contains("foo"),
            "文字列の中身は消える: {stripped}"
        );
        // 文字列内の `fn foo` を定義と誤認しない。
        assert_eq!(
            rust_definition(line),
            Some(("const".to_string(), "NAME".to_string()))
        );
    }

    // 行末コメント付きの再輸出でも降格が成立する。
    #[test]
    fn covers_reexport_with_trailing_comment() {
        let diff = move_diff().replace(
            "+pub use super::background::CONST_A;",
            "+pub use super::background::CONST_A; // moved out of consumer",
        );
        let index = ReexportMoveIndex::build(&diff);
        assert!(index.covers("src/game/consumer.rs", "CONST_A", "src/game/background.rs"));
    }

    #[test]
    fn definition_keywords_do_not_match_use_lines() {
        assert!(!rust_line_defines(
            "pub use super::background::CONST_A;",
            "CONST_A"
        ));
        assert!(rust_line_defines(
            "pub const CONST_A: f32 = -50.0;",
            "CONST_A"
        ));
        assert!(rust_line_defines(
            "fn placement(&self) -> f32 {",
            "placement"
        ));
        assert!(rust_line_defines("macro_rules! my_macro {", "my_macro"));
    }
}
