//! 手書き lexer による fallback 解析。
//!
//! tree-sitter が現実的に使えない言語 (現状 Xojo) で symbols / refs を抽出する。
//! バイト列を一度走査するだけで、識別子トークンと定義ヘッダ行を列挙する。
//! メモリ消費は入力サイズに対して定数倍 (token Vec のみ)、parse table は保持しない。
//!
//! ## モジュール構成
//! - `mod.rs` (このファイル): 共通ロジック (Scanner, extract_symbols, find_identifier_refs)
//! - `xojo.rs`: Xojo 言語固有の `LexerProfile` 定義と言語固有テスト
//!
//! 新しい lexer-only 言語を追加する際は:
//! 1. `language.rs` の `LexerLang` に variant 追加
//! 2. このモジュール直下に `<lang>.rs` を新規作成し `pub static PROFILE` を定義
//! 3. `profile_for()` に match arm を追加

pub mod xojo;

use crate::language::LexerLang;
use crate::models::location::{Point, Range};
use crate::models::symbol::{Symbol, SymbolKind};

/// 言語別 lexer プロファイル。
#[derive(Debug, Clone, Copy)]
pub struct LexerProfile {
    pub lang: LexerLang,
    /// 識別子の大文字小文字を区別しないか。
    pub case_insensitive: bool,
    /// 行コメント開始トークン。最長一致優先で並べる。
    pub line_comment_starts: &'static [&'static str],
    /// ブロックコメント (開始, 終了) ペア。
    pub block_comments: &'static [(&'static str, &'static str)],
    /// 文字列リテラル区切り文字。
    pub string_delimiters: &'static [char],
    /// 修飾子 keyword (Public / Private / Shared など)。定義 keyword の前に許容する。
    pub modifier_keywords: &'static [&'static str],
    /// 定義 keyword と SymbolKind の対応。Xojo の `Class Foo` `Sub Greet` 等。
    pub definition_keywords: &'static [(&'static str, SymbolKind)],
    /// 定義 keyword を抑制する prefix (例: `End Sub` の `End`)。
    pub end_prefix_keywords: &'static [&'static str],
}

/// 識別子トークンの位置情報。
#[derive(Debug, Clone, Copy)]
pub struct IdentToken<'a> {
    pub text: &'a str,
    pub line: usize,
    pub column: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// 識別子の判定 (ASCII 範囲)。Xojo は実用上 ASCII 識別子で十分。
#[inline]
fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

#[inline]
fn is_ident_cont(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// 識別子トークンを走査するイテレータ。コメント・文字列内の同名トークンはスキップする。
pub struct IdentScanner<'a> {
    profile: &'static LexerProfile,
    src: &'a [u8],
    pos: usize,
    line: usize,
    line_start: usize,
}

impl<'a> IdentScanner<'a> {
    pub fn new(src: &'a [u8], profile: &'static LexerProfile) -> Self {
        Self {
            profile,
            src,
            pos: 0,
            // tree-sitter::Point と同じ 0-indexed で揃える (models::location::Point も 0-indexed)。
            line: 0,
            line_start: 0,
        }
    }

    /// 現在位置から `needle` が始まるかをチェックする (大文字小文字無視オプション付き)。
    fn starts_with_ci(&self, needle: &str) -> bool {
        let bytes = needle.as_bytes();
        if self.pos + bytes.len() > self.src.len() {
            return false;
        }
        let slice = &self.src[self.pos..self.pos + bytes.len()];
        if self.profile.case_insensitive {
            slice.eq_ignore_ascii_case(bytes)
        } else {
            slice == bytes
        }
    }

    /// 改行を処理して line/line_start を更新する。
    fn advance_one(&mut self) {
        let b = self.src[self.pos];
        self.pos += 1;
        if b == b'\n' {
            self.line += 1;
            self.line_start = self.pos;
        }
    }

    /// 1 つの行コメントを最後まで読み飛ばす。
    fn skip_line_comment(&mut self) {
        while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
            self.pos += 1;
        }
    }

    /// ブロックコメントを終端まで読み飛ばす (改行があれば line を増やす)。
    fn skip_block_comment(&mut self, end: &str) {
        let end_bytes = end.as_bytes();
        while self.pos + end_bytes.len() <= self.src.len() {
            if &self.src[self.pos..self.pos + end_bytes.len()] == end_bytes {
                self.pos += end_bytes.len();
                return;
            }
            self.advance_one();
        }
        // 終端なしは EOF まで。
        self.pos = self.src.len();
    }

    /// 文字列リテラルを終端まで読み飛ばす。Xojo は `""` を escape として扱う。
    fn skip_string(&mut self, quote: u8) {
        // 開始 quote を消費
        self.pos += 1;
        while self.pos < self.src.len() {
            let b = self.src[self.pos];
            if b == quote {
                // double-quote escape (Xojo): 次も同じ quote なら escape
                if self.pos + 1 < self.src.len() && self.src[self.pos + 1] == quote {
                    self.pos += 2;
                    continue;
                }
                self.pos += 1;
                return;
            }
            self.advance_one();
        }
    }

    /// コメント/文字列を読み飛ばす。何かを skip したら true。
    fn try_skip_noise(&mut self) -> bool {
        if self.pos >= self.src.len() {
            return false;
        }
        // ブロックコメント
        for (start, end) in self.profile.block_comments {
            if self.starts_with_ci(start) {
                self.pos += start.len();
                self.skip_block_comment(end);
                return true;
            }
        }
        // 行コメント
        for start in self.profile.line_comment_starts {
            if self.starts_with_ci(start) {
                self.skip_line_comment();
                return true;
            }
        }
        // 文字列リテラル
        let b = self.src[self.pos];
        for &q in self.profile.string_delimiters {
            if b == q as u8 {
                self.skip_string(q as u8);
                return true;
            }
        }
        false
    }
}

impl<'a> Iterator for IdentScanner<'a> {
    type Item = IdentToken<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos >= self.src.len() {
                return None;
            }
            if self.try_skip_noise() {
                continue;
            }
            let b = self.src[self.pos];
            if is_ident_start(b) {
                let start = self.pos;
                let col = self.pos - self.line_start;
                while self.pos < self.src.len() && is_ident_cont(self.src[self.pos]) {
                    self.pos += 1;
                }
                // SAFETY: ASCII identifier だけを抽出するので、UTF-8 境界違反は無い。
                let text = std::str::from_utf8(&self.src[start..self.pos]).ok()?;
                return Some(IdentToken {
                    text,
                    line: self.line,
                    column: col,
                    start_byte: start,
                    end_byte: self.pos,
                });
            }
            // 識別子でなければ 1 バイト進める (改行追跡含む)。
            self.advance_one();
        }
    }
}

/// 行ベースで定義ヘッダを抽出する。
/// 戻り値: (SymbolKind, name, 元行頭からのオフセット).
pub(super) fn extract_definition_from_line(
    profile: &LexerProfile,
    line_text: &str,
    _line_num: usize,
) -> Option<(SymbolKind, String, usize)> {
    let leading_ws = line_text.len() - line_text.trim_start().len();
    let mut rest = &line_text[leading_ws..];

    // `End Sub` 等の closing 文を除外。
    for end_kw in profile.end_prefix_keywords {
        if let Some(after) = strip_keyword_prefix(rest, end_kw, profile.case_insensitive) {
            // `End Sub` パターン: `End ` の後に何か続いていたら closing と判定して skip。
            if after.trim_start().chars().next().is_some() {
                return None;
            }
        }
    }

    // optional modifier (複数修飾子を順番に消費する)。
    // Xojo の `Public Shared Sub ...` のように可視性修飾子と Shared/Static を併記する記法に
    // 対応するため、1 修飾子で break せず可能な限り続けて剥がす。
    // 同一 keyword の重複は consumed で除外 (`Public Public ...` のような病的入力でも
    // 無限ループにならないように)。
    let mut consumed: smallvec::SmallVec<[&'static str; 4]> = smallvec::SmallVec::new();
    loop {
        let mut matched: Option<&'static str> = None;
        for modifier in profile.modifier_keywords {
            if consumed.iter().any(|m| std::ptr::eq(*m, *modifier)) {
                continue;
            }
            if let Some(after) = strip_keyword_prefix(rest, modifier, profile.case_insensitive) {
                rest = after.trim_start();
                matched = Some(modifier);
                break;
            }
        }
        match matched {
            Some(kw) => consumed.push(kw),
            None => break,
        }
    }

    // definition keyword
    for (kw, kind) in profile.definition_keywords {
        if let Some(after) = strip_keyword_prefix(rest, kw, profile.case_insensitive) {
            let after_trim = after.trim_start();
            let name_end = after_trim
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(after_trim.len());
            if name_end == 0 {
                return None;
            }
            let name = &after_trim[..name_end];
            if !name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            {
                return None;
            }
            // 列番号は元の line_text 上での after_trim の開始位置。
            let original_after_trim_offset = line_text.len() - after_trim.len();
            return Some((*kind, name.to_string(), original_after_trim_offset));
        }
    }

    None
}

/// `text` が `keyword` で始まり (case_insensitive オプション)、その直後が単語境界なら
/// keyword 部分を消費した後の slice を返す。
pub(super) fn strip_keyword_prefix<'a>(text: &'a str, keyword: &str, ci: bool) -> Option<&'a str> {
    let kw_bytes = keyword.as_bytes();
    let text_bytes = text.as_bytes();
    if text_bytes.len() < kw_bytes.len() {
        return None;
    }
    let head = &text_bytes[..kw_bytes.len()];
    let matches = if ci {
        head.eq_ignore_ascii_case(kw_bytes)
    } else {
        head == kw_bytes
    };
    if !matches {
        return None;
    }
    // 単語境界チェック。
    if text_bytes.len() > kw_bytes.len() && is_ident_cont(text_bytes[kw_bytes.len()]) {
        return None;
    }
    Some(&text[kw_bytes.len()..])
}

/// identifier 出現位置 (1 件分)。
#[derive(Debug, Clone, Copy)]
pub struct IdentMatch {
    pub line: usize,
    pub column: usize,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// 指定 names に一致する identifier の出現位置を列挙する。
/// case-insensitive プロファイル (Xojo) では大文字小文字を区別しない。
/// 戻り値は引数 `names` と同じ順序で並ぶ (各 name に対する match 列)。
pub fn find_identifier_refs(
    source: &[u8],
    names: &[String],
    lang: LexerLang,
) -> Vec<(String, Vec<IdentMatch>)> {
    let profile = profile_for(lang);
    let normalized_keys: Vec<String> = names
        .iter()
        .map(|n| {
            if profile.case_insensitive {
                n.to_ascii_lowercase()
            } else {
                n.clone()
            }
        })
        .collect();

    let mut buckets: Vec<Vec<IdentMatch>> = vec![Vec::new(); names.len()];
    for token in IdentScanner::new(source, profile) {
        let key = if profile.case_insensitive {
            token.text.to_ascii_lowercase()
        } else {
            token.text.to_string()
        };
        for (i, nk) in normalized_keys.iter().enumerate() {
            if &key == nk {
                buckets[i].push(IdentMatch {
                    line: token.line,
                    column: token.column,
                    start_byte: token.start_byte,
                    end_byte: token.end_byte,
                });
            }
        }
    }

    names.iter().cloned().zip(buckets).collect()
}

/// dead-code 用の参照カウント (count-only)。
///
/// `find_identifier_refs` と同じ識別子列挙ロジックを使うが、戻り値は count のみ。
/// `Reference` / 位置情報を構築しないため、dead-code の hot path で per-symbol Vec を
/// 確保せずに済み、PHPUnit などの大規模 monorepo でピーク RSS を抑えられる。
///
/// 定義行に出現する identifier も name 一致すれば count に含む点に注意。dead-code 検出側は
/// 「**非定義参照**の合計」を求めているので、ここでは extract_symbols で得た定義行 (line)
/// セットを使って定義出現を差し引く。
///
/// 戻り値: names と同じ順序の `Vec<usize>`、各 name の **非定義参照** 件数。
pub fn count_non_definition_refs(source: &[u8], names: &[String], lang: LexerLang) -> Vec<usize> {
    let profile = profile_for(lang);
    let normalize = |s: &str| -> String {
        if profile.case_insensitive {
            s.to_ascii_lowercase()
        } else {
            s.to_string()
        }
    };
    let normalized_keys: Vec<String> = names.iter().map(|n| normalize(n)).collect();

    // 定義行を name (正規化済み) -> 行集合のマップで把握する。
    // refs 走査時に「line が def_lines[name] に含まれていれば skip」で非定義参照のみ数える。
    let mut def_lines: std::collections::HashMap<String, std::collections::HashSet<usize>> =
        std::collections::HashMap::new();
    for sym in extract_symbols(source, lang) {
        def_lines
            .entry(normalize(&sym.name))
            .or_default()
            .insert(sym.range.start.line);
    }

    let mut counts: Vec<usize> = vec![0; names.len()];
    for token in IdentScanner::new(source, profile) {
        let key = normalize(token.text);
        for (i, nk) in normalized_keys.iter().enumerate() {
            if &key == nk {
                let is_def = def_lines
                    .get(nk)
                    .map(|lines| lines.contains(&token.line))
                    .unwrap_or(false);
                if !is_def {
                    counts[i] += 1;
                }
            }
        }
    }
    counts
}

/// 行を走査して定義ヘッダを Symbol として抽出する。
/// コメント・文字列内の "Class Foo" 等を誤検出しないよう、行頭の prefix のみ見る。
pub fn extract_symbols(source: &[u8], lang: LexerLang) -> Vec<Symbol> {
    let profile = profile_for(lang);
    let Ok(src_str) = std::str::from_utf8(source) else {
        return Vec::new();
    };

    let mut symbols = Vec::new();
    let mut in_block_comment: Option<&str> = None;

    // line_num は 0-indexed (Symbol::range.start.line と整合する)。
    for (line_num, line) in src_str.split_inclusive('\n').enumerate() {
        let line_clean = line.trim_end_matches(['\r', '\n']);

        // ブロックコメント継続中ならスキップ。
        if let Some(end) = in_block_comment {
            if let Some(end_pos) = line_clean.find(end) {
                let after = &line_clean[end_pos + end.len()..];
                in_block_comment = None;
                if let Some((kind, name, col)) =
                    extract_definition_from_line(profile, after, line_num)
                {
                    symbols.push(Symbol {
                        name,
                        kind,
                        range: single_line_range(line_num, col),
                        doc: None,
                        complexity: None,
                        container: None,
                        children: Vec::new(),
                    });
                }
            }
            continue;
        }

        // 新規ブロックコメント開始?
        let mut effective_line = line_clean;
        for (start, end) in profile.block_comments {
            if let Some(s_pos) = effective_line.find(start) {
                if effective_line[s_pos + start.len()..].find(end).is_some() {
                    // 単一行内で閉じる: コメント部分は無視。
                    effective_line = &line_clean[..s_pos];
                    break;
                } else {
                    // 改行をまたぐブロックコメント開始
                    effective_line = &line_clean[..s_pos];
                    in_block_comment = Some(end);
                    break;
                }
            }
        }

        if let Some((kind, name, col)) =
            extract_definition_from_line(profile, effective_line, line_num)
        {
            symbols.push(Symbol {
                name,
                kind,
                range: single_line_range(line_num, col),
                doc: None,
                complexity: None,
                container: None,
                children: Vec::new(),
            });
        }
    }

    assign_containers(&mut symbols, profile);
    symbols
}

/// Class/Module の中で定義された Method/Property/Const に container を付与する。
fn assign_containers(symbols: &mut [Symbol], profile: &LexerProfile) {
    // 単純なネスト推定: Class/Module 出現後の同レベル定義に container を付ける。
    // Xojo のソースは class 内に method がフラットに並ぶ構造なので、これで十分。
    let _ = profile;
    let mut current_container: Option<String> = None;
    for sym in symbols.iter_mut() {
        match sym.kind {
            SymbolKind::Class | SymbolKind::Module => {
                current_container = Some(sym.name.clone());
            }
            SymbolKind::Function
            | SymbolKind::Method
            | SymbolKind::Field
            | SymbolKind::Constant
            | SymbolKind::Enum => {
                if let Some(parent) = &current_container
                    && sym.container.is_none()
                {
                    sym.container = Some(parent.clone());
                }
            }
            _ => {}
        }
    }
}

fn single_line_range(line: usize, col: usize) -> Range {
    Range {
        start: Point { line, column: col },
        end: Point {
            line,
            column: col + 1,
        },
    }
}

/// 言語別プロファイル取得。
pub fn profile_for(lang: LexerLang) -> &'static LexerProfile {
    match lang {
        LexerLang::Xojo => &xojo::PROFILE,
    }
}

/// lexer-only 言語向けの dead-code / API 差分用 export 候補抽出。
///
/// 戻り値は `(name, kind_str, signature)` のタプル列。
/// tree-sitter 経路の `filter_exported_symbols` と入出力形式を揃え、
/// commands 層から透過的に dispatch できるようにする。
///
/// Xojo は Public/Protected/Private 修飾子を持つが、現状の lexer は修飾子の有無を
/// 記録していない。保守的に全 Class/Module/Function/Method/Property/Const/Enum を
/// export 候補として返す (refs が 0 件なら dead と判定される)。
/// signature は空文字 (lexer profile では本格的な signature 抽出をしない)。
///
/// `exclude_runtime_entrypoints` が true (dead-code 経路) のときは、フレームワーク /
/// ランタイムが暗黙に呼ぶ entrypoint (Xojo の `#tag Event` ハンドラ、XojoUnit の
/// `TestGroup` 派生クラスの `*Test` メソッド等) を候補から除外する。静的 caller を
/// 持たないため参照カウントで追跡できず偽陽性源になる。API 差分経路 (false) では
/// 公開 API 面として残す。
pub fn extract_exported_symbols(
    source: &[u8],
    lexer_lang: LexerLang,
    exclude_runtime_entrypoints: bool,
) -> Vec<(String, String, String)> {
    // entrypoint は定義行 (range.start.line) で照合除外する。bare name 除外だと
    // 汎用名 (Action / KeyDown) で別の通常メソッドまで巻き込むため使わない。
    let entrypoint_lines = if exclude_runtime_entrypoints {
        extract_runtime_entrypoints(source, lexer_lang)
    } else {
        std::collections::HashSet::new()
    };
    extract_symbols(source, lexer_lang)
        .into_iter()
        .filter(|s| {
            matches!(
                s.kind,
                SymbolKind::Class
                    | SymbolKind::Module
                    | SymbolKind::Function
                    | SymbolKind::Method
                    | SymbolKind::Constant
                    | SymbolKind::Enum
                    | SymbolKind::Field
                    | SymbolKind::Interface
                    | SymbolKind::Struct
            )
        })
        .filter(|s| !entrypoint_lines.contains(&s.range.start.line))
        .map(|s| {
            let kind_str = match s.kind {
                SymbolKind::Class => "class",
                SymbolKind::Module => "module",
                SymbolKind::Function => "function",
                SymbolKind::Method => "method",
                SymbolKind::Constant => "constant",
                SymbolKind::Enum => "enum",
                SymbolKind::Field => "field",
                SymbolKind::Interface => "interface",
                SymbolKind::Struct => "struct",
                _ => "unknown",
            }
            .to_string();
            (s.name, kind_str, String::new())
        })
        .collect()
}

/// lexer-only 言語の runtime entrypoint (フレームワーク / ランタイムが暗黙に呼ぶため
/// 静的 caller を持たないシンボル) の **定義行 (0-indexed)** 集合を返す。
/// dead-code 検出でこれらを候補から除外するのに使う (API 差分検出では使わない)。
pub fn extract_runtime_entrypoints(
    source: &[u8],
    lang: LexerLang,
) -> std::collections::HashSet<usize> {
    match lang {
        LexerLang::Xojo => xojo::runtime_entrypoint_lines(source),
    }
}
