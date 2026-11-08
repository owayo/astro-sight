//! Rust の `#[cfg]`・`#[cfg_attr]` の述語 (Issue #276・#277)。
//!
//! 公開 enum と union の形を、cfg の原子 (`unix`・`feature = "x"` など) の真偽の組み合わせ (設定) ごとに
//! 比べるための部品。`#[cfg(P)]` は P、`#[cfg_attr(P, cfg(Q))]` は `not(P) or Q` として読み、入れ子の
//! `#[cfg_attr]` の中の属性は、述語が全て成り立つときに効くものとする。1 つの値しか取らない鍵
//! (`target_os` など) の原子は同時に真にせず、`cfg(true)`・`cfg(false)` は定数、文字列の値は中身で
//! 比べる (エスケープを解く。解けない値の原子は、ほかの値と同時に真になり得るものとする)。

use std::collections::{BTreeMap, BTreeSet};

use super::rust_type_api::{
    Attr, closing_paren, conditional_repr, keep_widest_align, repr_items, split_repr_items, unraw,
};

/// 比べる述語の原子の数の上限 (組み合わせは 2 のその数乗)。超えれば壊す扱いにする。
pub(super) const MAX_ATOMS: usize = 16;

/// cfg の述語。原子 (`unix`・`feature = "x"` など) は番号で表す。`All(vec![])` は真、
/// `Any(vec![])` は偽。
#[derive(Clone, Debug)]
pub(super) enum Predicate {
    Atom(usize),
    All(Vec<Predicate>),
    Any(Vec<Predicate>),
    Not(Box<Predicate>),
}

impl Predicate {
    pub(super) fn always() -> Self {
        Self::All(Vec::new())
    }

    /// 原子の真偽 `config` (番号のビット) で成り立つか。
    pub(super) fn holds(&self, config: u64) -> bool {
        match self {
            Self::Atom(index) => config & (1 << index) != 0,
            Self::All(parts) => parts.iter().all(|part| part.holds(config)),
            Self::Any(parts) => parts.iter().any(|part| part.holds(config)),
            Self::Not(inner) => !inner.holds(config),
        }
    }
}

/// `name ( .. )` の形 (外側の括弧が組になる) なら、名前 (`r#` を外す) と括弧の中。
fn call(tokens: &[String]) -> Option<(&str, &[String])> {
    let (name, rest) = tokens.split_first()?;
    let inner = rest
        .strip_prefix(&["(".to_owned()])?
        .strip_suffix(&[")".to_owned()])?;
    let mut depth = 0usize;
    for token in inner {
        match token.as_str() {
            "(" => depth += 1,
            ")" => depth = depth.checked_sub(1)?,
            _ => {}
        }
    }
    (depth == 0).then_some((unraw(name), inner))
}

/// 括弧の外の `,` で分けた並び (空のものは除く)。
fn split_commas(tokens: &[String]) -> Vec<&[String]> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (index, token) in tokens.iter().enumerate() {
        match token.as_str() {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth = depth.saturating_sub(1),
            "," if depth == 0 => {
                if index > start {
                    parts.push(&tokens[start..index]);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    if tokens.len() > start {
        parts.push(&tokens[start..]);
    }
    parts
}

/// 1 つの値しか取らない cfg の鍵 (`target_os = "linux"` と `target_os = "windows"` は同時に
/// 真にならない)。`feature`・`target_family`・`target_feature` などは複数の値を取る。
const SINGLE_VALUED: [&str; 8] = [
    "target_arch",
    "target_os",
    "target_env",
    "target_abi",
    "target_endian",
    "target_pointer_width",
    "target_vendor",
    "panic",
];

/// 文字列のリテラル (`"x"`・`r"x"`・`r#"x"#`) の中身。エスケープ (`\n`・`\x6c`・`\u{6c}`・行の
/// 継続) は Rust の字句の規則で解き、CRLF は LF にする (Rust はソースの CRLF を LF として読む)。
/// 読めなければ None。
fn string_value(literal: &str) -> Option<String> {
    if let Some(rest) = literal.strip_prefix('r') {
        let hashes = rest.len() - rest.trim_start_matches('#').len();
        let inner = rest.get(hashes..rest.len().checked_sub(hashes)?)?;
        let inner = inner.strip_prefix('"')?.strip_suffix('"')?;
        return Some(inner.replace("\r\n", "\n"));
    }
    let inner = literal.strip_prefix('"')?.strip_suffix('"')?;
    let mut value = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\r' && chars.next_if_eq(&'\n').is_some() {
            value.push('\n');
            continue;
        }
        if character != '\\' {
            value.push(character);
            continue;
        }
        value.push(match chars.next()? {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            '0' => '\0',
            escaped @ ('\\' | '"' | '\'') => escaped,
            // `\x` と 2 桁の 16 進。文字列では 0x7F まで (1 桁目は 0〜7)。
            'x' => {
                let high = chars.next()?.to_digit(8)?;
                let low = chars.next()?.to_digit(16)?;
                char::from_u32(high * 16 + low)?
            }
            // `\u{..}`: 1〜6 桁の 16 進 (桁の後の `_` は読み飛ばす)。
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let (mut code, mut digits) = (0u32, 0);
                loop {
                    match chars.next()? {
                        '}' if digits > 0 => break,
                        '_' if digits > 0 => {}
                        digit => {
                            code = code * 16 + digit.to_digit(16)?;
                            digits += 1;
                            if digits > 6 {
                                return None;
                            }
                        }
                    }
                }
                char::from_u32(code)?
            }
            // 行の継続: `\` と改行 (LF か CRLF) と、その後の空白を読み飛ばす。
            '\n' => {
                while chars
                    .next_if(|next| matches!(next, ' ' | '\t' | '\n' | '\r'))
                    .is_some()
                {}
                continue;
            }
            '\r' if chars.next_if_eq(&'\n').is_some() => {
                while chars
                    .next_if(|next| matches!(next, ' ' | '\t' | '\n' | '\r'))
                    .is_some()
                {}
                continue;
            }
            _ => return None,
        });
    }
    Some(value)
}

/// 述語の原子の表。同じ字面の原子は同じ番号にする (旧新で共有する)。
#[derive(Default)]
pub(super) struct Atoms(Vec<Vec<String>>);

impl Atoms {
    /// 1 つの値しか取らない鍵ごとの、原子の番号のビット。値を解けない原子 (読めないエスケープ) は、
    /// 同じ値の別の書き方かもしれないので含めない。
    pub(super) fn exclusive(&self) -> Vec<u64> {
        let mut masks: BTreeMap<&str, u64> = BTreeMap::new();
        for (index, atom) in self.0.iter().enumerate() {
            if let [key, equals, value] = atom.as_slice()
                && equals == "="
                && SINGLE_VALUED.contains(&key.as_str())
                && string_value(value).is_some()
            {
                *masks.entry(key.as_str()).or_default() |= 1 << index;
            }
        }
        masks.into_values().collect()
    }

    fn atom(&mut self, tokens: &[String]) -> Predicate {
        // 識別子の `r#` は外す (`r#feature` は `feature`)。`key = "value"` の値は、書き方 (raw
        // 文字列・エスケープ) に依らず中身で比べる。
        let unrawn: Vec<String> = tokens.iter().map(|token| unraw(token).to_owned()).collect();
        let normalized: Vec<String> = match unrawn.as_slice() {
            [key, equals, value] if equals == "=" => match string_value(value) {
                Some(value) => vec![key.clone(), equals.clone(), format!("{value:?}")],
                None => unrawn.clone(),
            },
            _ => unrawn.clone(),
        };
        let tokens = normalized.as_slice();
        let index = match self.0.iter().position(|atom| atom.as_slice() == tokens) {
            Some(index) => index,
            None => {
                self.0.push(tokens.to_vec());
                self.0.len() - 1
            }
        };
        Predicate::Atom(index)
    }

    /// cfg の述語 (`all(..)`・`any(..)`・`not(..)` と原子)。読めない形は字面全体を 1 つの原子にする。
    pub(super) fn predicate(&mut self, tokens: &[String]) -> Predicate {
        // `cfg(true)`・`cfg(false)` は定数。
        match tokens {
            [literal] if literal == "true" => return Predicate::always(),
            [literal] if literal == "false" => return Predicate::Any(Vec::new()),
            _ => {}
        }
        if let Some((name, inner)) = call(tokens)
            && matches!(name, "all" | "any" | "not")
        {
            let parts: Vec<Predicate> = split_commas(inner)
                .into_iter()
                .map(|part| self.predicate(part))
                .collect();
            match name {
                "all" => return Predicate::All(parts),
                "any" => return Predicate::Any(parts),
                _ => {
                    if let Ok([only]) = <[Predicate; 1]>::try_from(parts) {
                        return Predicate::Not(Box::new(only));
                    }
                }
            }
        }
        self.atom(tokens)
    }

    /// 属性 `key` (`cfg(..)` と、`cfg` を含む `cfg_attr(..)`) の付いたものが有る条件。
    /// `cfg_attr(P, cfg(Q))` は P のときだけ `cfg(Q)` を付けるので `not(P) or Q`。
    pub(super) fn presence(&mut self, key: &[String]) -> Predicate {
        match call(key) {
            // `cfg(P,)` の末尾の `,` は無視される。述語が 1 つでなければ読めない形。
            Some(("cfg", inner)) => match split_commas(inner).as_slice() {
                [only] => self.predicate(only),
                _ => self.atom(key),
            },
            Some(("cfg_attr", inner)) => {
                let parts = split_commas(inner);
                let Some((predicate, attributes)) = parts.split_first() else {
                    return self.atom(key);
                };
                let predicate = self.predicate(predicate);
                let applied = Predicate::All(
                    attributes
                        .iter()
                        .map(|attribute| self.presence(attribute))
                        .collect(),
                );
                Predicate::Any(vec![Predicate::Not(Box::new(predicate)), applied])
            }
            _ => self.atom(key),
        }
    }

    /// 属性 `key` の中の、効かせる属性 (`non_exhaustive`・`repr(..)`) と、それが効く条件
    /// (入れ子の `cfg_attr` の述語が全て成り立つとき)。
    pub(super) fn payloads(&mut self, key: &[String]) -> Vec<(Predicate, Vec<String>)> {
        let Some(("cfg_attr", inner)) = call(key) else {
            return vec![(Predicate::always(), key.to_vec())];
        };
        let parts = split_commas(inner);
        let Some((predicate, attributes)) = parts.split_first() else {
            return Vec::new();
        };
        let predicate = self.predicate(predicate);
        let mut payloads = Vec::new();
        for attribute in attributes {
            for (inner, payload) in self.payloads(attribute) {
                payloads.push((Predicate::All(vec![predicate.clone(), inner]), payload));
            }
        }
        payloads
    }

    /// 原子の数。
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    /// 属性 (`#[cfg]` と、`cfg` を含む `#[cfg_attr]`) が全て付いたものが有る条件。
    pub(super) fn present(&mut self, attributes: &[Attr]) -> Predicate {
        Predicate::All(
            attributes
                .iter()
                .map(|attribute| self.presence(&attribute.key))
                .collect(),
        )
    }

    /// `#[repr]` と、それを含む `#[cfg_attr]` (`layout`) の項目と、それが効く条件 (無条件のものは
    /// 真)。`Rust` は既定の表現と同じなので除く。
    pub(super) fn reprs(&mut self, layout: &[Attr]) -> Vec<(Predicate, BTreeSet<Vec<String>>)> {
        let mut unconditional = repr_items(layout);
        unconditional.remove(&vec!["Rust".to_owned()]);
        let mut reprs = vec![(Predicate::always(), unconditional)];
        for key in conditional_repr(layout) {
            for (predicate, payload) in self.payloads(&key) {
                let mut items = conditional_repr_items(&payload);
                items.remove(&vec!["Rust".to_owned()]);
                reprs.push((predicate, items));
            }
        }
        reprs
    }

    /// 比べる設定 (原子の真偽の組み合わせのうち起こり得るもの)。原子が [`MAX_ATOMS`] を超えれば
    /// None (呼び出し側は壊す扱いにする)。
    pub(super) fn configs(&self) -> Option<impl Iterator<Item = u64> + use<>> {
        if self.0.len() > MAX_ATOMS {
            return None;
        }
        let exclusive = self.exclusive();
        Some((0..1u64 << self.0.len()).filter(move |config| possible(*config, &exclusive)))
    }
}

/// 設定 `config` で効く `#[repr]` の項目 ([`Atoms::reprs`] の結果から。`align` は最大のものが効く)。
pub(super) fn effective_repr(
    reprs: &[(Predicate, BTreeSet<Vec<String>>)],
    config: u64,
) -> BTreeSet<Vec<String>> {
    let mut items = BTreeSet::new();
    for (predicate, written) in reprs {
        if predicate.holds(config) {
            items.extend(written.iter().cloned());
        }
    }
    keep_widest_align(&mut items);
    items
}

/// `#[cfg_attr(.., repr(..))]` の鍵の中の `repr` の項目 (意味の同じ書き方にそろえる)。
pub(super) fn conditional_repr_items(key: &[String]) -> BTreeSet<Vec<String>> {
    let mut items = BTreeSet::new();
    let mut position = 0;
    while position + 1 < key.len() {
        if unraw(&key[position]) != "repr" || key[position + 1] != "(" {
            position += 1;
            continue;
        }
        let end = closing_paren(key, position + 2);
        items.extend(split_repr_items(&key[position + 2..end]));
        position = end + 1;
    }
    items
}

/// 起こり得る組み合わせか (1 つの値しか取らない鍵の原子は、同時に 2 つ真にならない)。
pub(super) fn possible(config: u64, exclusive: &[u64]) -> bool {
    exclusive
        .iter()
        .all(|mask| (config & mask).count_ones() <= 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicates_follow_cfg_and_cfg_attr() {
        let tokens = |text: &str| -> Vec<String> { text.split(' ').map(str::to_owned).collect() };
        let mut atoms = Atoms::default();
        // any / all / not と原子
        let predicate = atoms.predicate(&tokens("any ( unix , all ( windows , not ( test ) ) )"));
        assert_eq!(atoms.len(), 3);
        assert!(predicate.holds(0b001));
        assert!(predicate.holds(0b010));
        assert!(!predicate.holds(0b110));
        // cfg_attr(P, cfg(Q)) は not(P) or Q
        let mut atoms = Atoms::default();
        let presence = atoms.presence(&tokens("cfg_attr ( a , cfg ( b ) )"));
        assert!(presence.holds(0b00));
        assert!(!presence.holds(0b01));
        assert!(presence.holds(0b11));
        // 入れ子の cfg_attr の中の属性は、述語が全て成り立つときに効く
        let mut atoms = Atoms::default();
        let payloads = atoms.payloads(&tokens("cfg_attr ( a , cfg_attr ( b , repr ( C ) ) )"));
        let [(predicate, payload)] = payloads.as_slice() else {
            panic!("{payloads:?}");
        };
        assert_eq!(payload, &tokens("repr ( C )"));
        assert!(!predicate.holds(0b01));
        assert!(predicate.holds(0b11));
    }

    #[test]
    fn raw_identifiers_and_repr_spellings_are_normalized() {
        let tokens = |text: &str| -> Vec<String> { text.split(' ').map(str::to_owned).collect() };
        // `r#cfg` は `cfg`、`r#feature` は `feature`
        let mut atoms = Atoms::default();
        let raw = atoms.presence(&tokens("r#cfg ( r#feature = \"x\" )"));
        let plain = atoms.presence(&tokens("cfg ( feature = \"x\" )"));
        assert_eq!(atoms.len(), 1);
        assert!(raw.holds(0b1) && plain.holds(0b1) && !raw.holds(0b0));
        // `cfg_attr` の中の `r#repr` と、`packed(1,)`・`align(0x8,)` の書き方
        assert_eq!(
            conditional_repr_items(&tokens(
                "cfg_attr ( all ( ) , r#repr ( C , packed ( 1 , ) , align ( 0x8 , ) ) )"
            )),
            BTreeSet::from([tokens("C"), tokens("align ( 8 )"), tokens("packed")])
        );
        // `cfg(true)` は定数だが、`cfg(r#true)` は `true` という名前の原子
        let mut atoms = Atoms::default();
        assert!(atoms.predicate(&tokens("true")).holds(0));
        assert!(!atoms.predicate(&tokens("r#true")).holds(0));
        assert_eq!(atoms.len(), 1);
    }

    #[test]
    fn string_values_follow_rust_escapes() {
        for (literal, expected) in [
            (r#""linux""#, Some("linux")),
            (r#""\x6cinux""#, Some("linux")),
            (r#""\u{6c}inux""#, Some("linux")),
            (r#""\u{0_6_c}inux""#, Some("linux")),
            ("\"lin\\\n    ux\"", Some("linux")),
            ("\"lin\\\r\n\tux\"", Some("linux")),
            ("\"a\r\nb\"", Some("a\nb")),
            (r##"r#"lin"ux"#"##, Some("lin\"ux")),
            (r#""\x80""#, None),
            (r#""\u{d800}""#, None),
            (r#""\u{}""#, None),
            (r#""\u{_6c}""#, None),
            (r#""\u{1234567}""#, None),
            (r#""\q""#, None),
            ("\"lin\\\rux\"", None),
        ] {
            assert_eq!(string_value(literal).as_deref(), expected, "{literal}");
        }
        let tokens = |text: &str| -> Vec<String> { text.split(' ').map(str::to_owned).collect() };
        // 同じ値の別の書き方は同じ原子で、1 つの値しか取らない鍵の別の値とは同時に真にならない
        let mut atoms = Atoms::default();
        let plain = atoms.predicate(&tokens("target_os = \"linux\""));
        let escaped = atoms.predicate(&tokens("target_os = \"\\x6cinux\""));
        let other = atoms.predicate(&tokens("target_os = \"macos\""));
        assert_eq!(atoms.len(), 2);
        assert!(plain.holds(0b01) && escaped.holds(0b01) && other.holds(0b10));
        assert_eq!(atoms.exclusive(), vec![0b11]);
        // 解けない値の原子は、ほかの値と同時に真になり得る
        let mut atoms = Atoms::default();
        atoms.predicate(&tokens("target_os = \"linux\""));
        atoms.predicate(&tokens("target_os = \"\\qinux\""));
        assert_eq!(atoms.exclusive(), vec![0b01]);
    }

    #[test]
    fn trailing_commas_and_raw_conditions_keep_their_meaning() {
        let tokens = |text: &str| -> Vec<String> { text.split(' ').map(str::to_owned).collect() };
        // `cfg(all(),)` は `cfg(all())` (いつも真)
        let mut atoms = Atoms::default();
        assert!(atoms.presence(&tokens("cfg ( all ( ) , )")).holds(0));
        assert_eq!(atoms.len(), 0);
        // `cfg_attr(r#true, ..)` の条件は名前の付いた cfg で、偽の設定では repr が付かない
        let mut atoms = Atoms::default();
        let reprs = atoms.reprs(&[Attr {
            key: tokens("cfg_attr ( r#true , repr ( C , align ( 4 ) , align ( 8 ) ) )"),
            shown: String::new(),
        }]);
        assert_eq!(atoms.len(), 1);
        assert!(effective_repr(&reprs, 0).is_empty());
        // 複数の `align` は最大のものが効く
        assert_eq!(
            effective_repr(&reprs, 1),
            BTreeSet::from([tokens("C"), tokens("align ( 8 )")])
        );
    }
}
