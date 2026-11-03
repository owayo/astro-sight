//! 在庫の外のファイルの中身を流して読み、語や印の出現を探す (Issue #208)。
//!
//! ファイルの全体をメモリに載せず、一定の大きさずつ読む。ファイル数には上限を掛けず、読む量と
//! 解析する量の合計にだけ上限を掛ける。上限は 1 回の API 変更の検出の全体で共有する。前後の文字が
//! 窓の外にあって決められない出現は、出現として数える (保守側)。

use std::cell::Cell;
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

use aho_corasick::AhoCorasick;

const CHUNK_BYTES: usize = 256 * 1024;
/// `\u` の escape から集める文字の種類の上限。超えたら、どの文字も escape で書かれ得るとみなす。
const MAX_ESCAPED_CHARS: usize = 1024;

/// 在庫の外のファイルを読むときの上限。
#[derive(Clone, Copy, Debug)]
pub(super) struct ScanLimits {
    /// 在庫の外のファイルを流して読む量の合計。
    pub(super) stream_bytes: u64,
    /// 在庫の外のファイルを読んで解析する量の合計。
    pub(super) parse_bytes: u64,
}

impl ScanLimits {
    pub(super) const DEFAULT: Self = Self {
        stream_bytes: 1024 * 1024 * 1024,
        parse_bytes: 64 * 1024 * 1024,
    };
}

/// 1 回の検出の全体で共有する、読む量と解析する量の残り。
pub(super) struct Budget {
    stream: Cell<u64>,
    parse: Cell<u64>,
}

impl Budget {
    pub(super) fn new(limits: ScanLimits) -> Self {
        Self {
            stream: Cell::new(limits.stream_bytes),
            parse: Cell::new(limits.parse_bytes),
        }
    }

    fn take(cell: &Cell<u64>, bytes: u64) -> bool {
        match cell.get().checked_sub(bytes) {
            Some(rest) => {
                cell.set(rest);
                true
            }
            None => {
                cell.set(0);
                false
            }
        }
    }

    /// 流して読む量を使う。残りが足りなければ false。
    pub(super) fn take_stream(&self, bytes: u64) -> bool {
        Self::take(&self.stream, bytes)
    }

    /// 解析する量を使う。残りが足りなければ false。
    pub(super) fn take_parse(&self, bytes: u64) -> bool {
        Self::take(&self.parse, bytes)
    }
}

/// 探す文字列。
pub(super) struct Needle {
    bytes: Vec<u8>,
    ignore_case: bool,
    /// 前後が JS の識別子の文字でない出現 (識別子の単語) に限る。
    word: bool,
}

impl Needle {
    /// JS の識別子の単語として現れる `text`。
    pub(super) fn word(text: &str) -> Self {
        Self {
            bytes: text.as_bytes().to_vec(),
            ignore_case: false,
            word: true,
        }
    }

    /// どこかに現れる `text` (大文字小文字を区別する)。
    pub(super) fn text(text: &str) -> Self {
        Self {
            bytes: text.as_bytes().to_vec(),
            ignore_case: false,
            word: false,
        }
    }

    /// どこかに現れる `text` (ASCII の大文字小文字を区別しない)。
    pub(super) fn ignore_case(text: &str) -> Self {
        Self {
            bytes: text.as_bytes().to_vec(),
            ignore_case: true,
            word: false,
        }
    }
}

/// 探す文字列の組。多数の語を 1 回の読みで照合する。
pub(super) struct Needles {
    needles: Vec<Needle>,
    /// 大文字小文字を区別する needle と、区別しない needle の automaton。値は `needles` の番号。
    automata: Vec<(AhoCorasick, Vec<usize>)>,
    keep: usize,
}

impl Needles {
    /// 空の needle は照合しない (現れなかったものとする)。
    pub(super) fn new(needles: Vec<Needle>) -> Self {
        let mut automata = Vec::new();
        for ignore_case in [false, true] {
            let indices: Vec<usize> = needles
                .iter()
                .enumerate()
                .filter(|(_, needle)| needle.ignore_case == ignore_case && !needle.bytes.is_empty())
                .map(|(index, _)| index)
                .collect();
            if indices.is_empty() {
                continue;
            }
            let automaton = aho_corasick::AhoCorasick::builder()
                .ascii_case_insensitive(ignore_case)
                .build(indices.iter().map(|index| &needles[*index].bytes))
                .expect("needles are plain byte strings");
            automata.push((automaton, indices));
        }
        let keep = needles
            .iter()
            .map(|needle| needle.bytes.len())
            .max()
            .unwrap_or(0)
            + 4;
        Self {
            needles,
            automata,
            keep,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.needles.len()
    }

    fn mark(&self, window: &[u8], found: &mut [bool]) {
        for (automaton, indices) in &self.automata {
            for matched in automaton.find_overlapping_iter(window) {
                let index = indices[matched.pattern().as_usize()];
                if found[index] {
                    continue;
                }
                let needle = &self.needles[index];
                found[index] = !needle.word || {
                    let before = matched.start().checked_sub(1).map(|at| window[at]);
                    let after = window.get(matched.end()).copied();
                    before.is_none_or(|byte| !is_identifier_byte(byte))
                        && after.is_none_or(|byte| !is_identifier_byte(byte))
                };
            }
        }
    }
}

/// 1 ファイルを読んだ結果。
pub(super) struct Scan {
    found: Vec<bool>,
    /// `\uXXXX`・`\u{...}` の escape が表す文字。
    escaped: BTreeSet<char>,
    escaped_overflow: bool,
}

impl Scan {
    /// `index` 番目の needle がそのまま現れたか。
    pub(super) fn found(&self, index: usize) -> bool {
        self.found.get(index).copied().unwrap_or(true)
    }

    pub(super) fn found_any(&self) -> bool {
        self.found.iter().any(|found| *found)
    }

    /// `text` の文字のどれかを表す escape があるか (`Panel` のように、escape を混ぜて名前を
    /// 書いた可能性)。
    pub(super) fn escapes_any(&self, text: &str) -> bool {
        self.escaped_overflow || text.chars().any(|ch| self.escaped.contains(&ch))
    }

    /// ASCII の英数字・`_`・`$` を表す escape があるか (識別子を escape で書いた可能性)。
    pub(super) fn escapes_identifier(&self) -> bool {
        self.escaped_overflow
            || self
                .escaped
                .iter()
                .any(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '$'))
    }
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')
}

/// `\uXXXX` と `\u{...}` の escape を、窓をまたいでも読めるように 1 バイトずつ読む。
#[derive(Default)]
struct Escapes {
    state: EscapeState,
    value: u64,
    digits: u32,
    chars: BTreeSet<char>,
    overflow: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    #[default]
    Normal,
    Backslash,
    U,
    Fixed,
    Braced,
}

impl Escapes {
    fn feed(&mut self, bytes: &[u8]) {
        let mut index = 0;
        while index < bytes.len() {
            // escape の外では、次の `\` まで読み飛ばす。
            if self.state == EscapeState::Normal {
                match memchr::memchr(b'\\', &bytes[index..]) {
                    Some(offset) => index += offset,
                    None => return,
                }
            }
            self.step(bytes[index]);
            index += 1;
        }
    }

    fn step(&mut self, byte: u8) {
        let hex = (byte as char).to_digit(16).map(u64::from);
        self.state = match (self.state, hex) {
            (EscapeState::Normal, _) if byte == b'\\' => EscapeState::Backslash,
            (EscapeState::Normal, _) => EscapeState::Normal,
            // `\\` は escape した `\` で、続く `u` は escape を始めない。
            (EscapeState::Backslash, _) if byte == b'u' => EscapeState::U,
            (EscapeState::Backslash, _) => EscapeState::Normal,
            (EscapeState::U, _) if byte == b'{' => {
                self.value = 0;
                self.digits = 0;
                EscapeState::Braced
            }
            (EscapeState::U, Some(digit)) => {
                self.value = digit;
                self.digits = 1;
                EscapeState::Fixed
            }
            (EscapeState::Fixed, Some(digit)) => {
                self.value = self.value * 16 + digit;
                self.digits += 1;
                if self.digits == 4 {
                    self.record();
                    EscapeState::Normal
                } else {
                    EscapeState::Fixed
                }
            }
            (EscapeState::Braced, Some(digit)) => {
                // 先頭の 0 はいくつでも書ける。値は飽和させ、範囲外は記録しない。
                self.value = self.value.saturating_mul(16).saturating_add(digit);
                EscapeState::Braced
            }
            (EscapeState::Braced, None) if byte == b'}' => {
                self.record();
                EscapeState::Normal
            }
            (_, _) if byte == b'\\' => EscapeState::Backslash,
            (_, _) => EscapeState::Normal,
        };
    }

    fn record(&mut self) {
        let Some(ch) = u32::try_from(self.value).ok().and_then(char::from_u32) else {
            return;
        };
        if self.chars.len() >= MAX_ESCAPED_CHARS {
            self.overflow = true;
        } else {
            self.chars.insert(ch);
        }
    }
}

/// `path` を流して読み、`needles` の出現と escape の文字を集める。読めないとき・読む量の残りを
/// 使い切ったときは None。
pub(super) fn scan_file(path: &Path, needles: &Needles, budget: &Budget) -> Option<Scan> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut found = vec![false; needles.len()];
    let mut escapes = Escapes::default();
    let mut chunk = vec![0u8; CHUNK_BYTES];
    let mut window: Vec<u8> = Vec::with_capacity(CHUNK_BYTES + needles.keep);
    loop {
        let read = match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return None,
        };
        if !budget.take_stream(read as u64) {
            return None;
        }
        escapes.feed(&chunk[..read]);
        window.extend_from_slice(&chunk[..read]);
        needles.mark(&window, &mut found);
        // 次の窓と重ねて、窓の境目をまたぐ出現を読み落とさない。
        if window.len() > needles.keep {
            window.drain(..window.len() - needles.keep);
        }
    }
    Some(Scan {
        found,
        escaped: escapes.chars,
        escaped_overflow: escapes.overflow,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(text: &[u8], needles: Vec<Needle>) -> Scan {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("source.js");
        std::fs::write(&path, text).unwrap();
        let budget = Budget::new(ScanLimits::DEFAULT);
        scan_file(&path, &Needles::new(needles), &budget).unwrap()
    }

    fn needles() -> Vec<Needle> {
        vec![
            Needle::word("Panel"),
            Needle::text("JSX"),
            Needle::ignore_case("<reference"),
            Needle::word("Pan"),
        ]
    }

    #[test]
    fn source_scan_finds_words_across_chunks_and_escapes() {
        // 窓の境目をまたぐ出現も読む。
        let mut text = vec![b' '; CHUNK_BYTES - 2];
        text.extend_from_slice(b"Panel(1); /// <REFERENCE path=\"x\" />\n");
        let result = scan(&text, needles());
        assert!(result.found(0) && !result.found(1) && result.found(2) && !result.found(3));
        // 識別子の一部 (`Panels`・`$Panel`) は単語として数えない。重なる語も別々に数える。
        let result = scan(b"const Panels = $Panel; JSX Pan", needles());
        assert!(!result.found(0) && result.found(1) && result.found(3));
        // ASCII でない文字の隣は区切りとみなす (保守側)。
        assert!(scan("é Panel、".as_bytes(), needles()).found(0));
        // escape は、窓をまたいでも文字として読む。
        let mut escaped = vec![b' '; CHUNK_BYTES - 3];
        escaped.extend_from_slice(b"P\\u0061nel \\u{0000004A}");
        let result = scan(&escaped, needles());
        assert!(!result.found(0));
        assert!(result.escapes_any("Panel") && result.escapes_any("JSX"));
        assert!(result.escapes_identifier());
        // escape した `\` の後の `u` と、ASCII の英数字でない文字の escape は数えない。
        let result = scan(b"'\\\\u0050' '\\u00e9' '\\u2028'", needles());
        assert!(!result.escapes_any("Panel") && !result.escapes_identifier());
    }

    #[test]
    fn source_scan_shares_the_read_budget() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("big.js");
        std::fs::write(&path, vec![b'a'; 4096]).unwrap();
        let needles = Needles::new(vec![Needle::word("Panel")]);
        let budget = Budget::new(ScanLimits {
            stream_bytes: 6000,
            parse_bytes: 0,
        });
        assert!(scan_file(&path, &needles, &budget).is_some());
        // 残りの 1904 バイトでは、同じファイルをもう一度は読めない。
        assert!(scan_file(&path, &needles, &budget).is_none());
        assert!(!budget.take_parse(1));
        // 読めないファイルも None。
        let budget = Budget::new(ScanLimits::DEFAULT);
        assert!(scan_file(&directory.path().join("missing.js"), &needles, &budget).is_none());
    }
}
