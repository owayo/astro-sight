//! package のソースの `use` の宣言と module の位置のマクロを字句で読み、名前を経路に使い得るかを確かめる
//! (Issue #306・#318)。
//!
//! 再公開した名前を比べる門 (crate 全体の置き場と名前を作るものを旧新で作るので重い) を、非公開の取り込み
//! だけが変わった差分で開くかを決める。変わった名前を、`use` の経路が使い得るときだけ開く: 経路の
//! 先頭か途中 (後ろにセグメントが続く。直前が別名の module でもよい) に現れるか、その module を通って (直前が
//! module の最後の名前・`super`・`self`・`crate`) 取り込むか、取り込み直した別名 (`use x::Map as Other;` の
//! `Other`) が同じく使われる。module の位置 (ファイルと `mod` の本体の直下) のマクロの呼び出しと
//! `macro_rules!` の字句に現れる名前も使い得るとする (`use` を作り得る)。構文解析はせず、コメント・文字列を
//! 飛ばした字句の並びの `use` から `;` までを読む (マクロの本体や関数の中の `use` も数える)。迷えば使い得る
//! とする。

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// 読むファイルの大きさの上限 (超えるファイルは使い得るとする)。
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// 下見で読むソースの合計の上限。超えれば使い得るとする (全てのソースを抱えない)。
const MAX_SCANNED_BYTES: u64 = 64 * 1024 * 1024;

/// 字句。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Ident(String),
    Punct(&'static str),
}

/// ソース `source` の字句 (名前と、`use` の木とマクロの呼び出しに要る記号)。コメント・文字列・文字・
/// lifetime は飛ばす。
fn lex(source: &str) -> Vec<Token> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                let mut depth = 0usize;
                while i < bytes.len() {
                    if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                        depth += 1;
                        i += 2;
                    } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'"' => i = skip_string(bytes, i + 1),
            b'\'' => {
                // 文字 (`'a'`・`'\n'`) か lifetime (`'a`)。
                if bytes.get(i + 1) == Some(&b'\\') {
                    i += 2;
                    while i < bytes.len() && bytes[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                } else if bytes.get(i + 2) == Some(&b'\'') {
                    i += 3;
                } else {
                    i += 1;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                    {
                        i += 1;
                    }
                }
            }
            b':' if bytes.get(i + 1) == Some(&b':') => {
                tokens.push(Token::Punct("::"));
                i += 2;
            }
            b'{' | b'}' | b'(' | b')' | b'[' | b']' | b',' | b';' | b'*' | b'.' | b'!' => {
                tokens.push(Token::Punct(match c {
                    b'{' => "{",
                    b'}' => "}",
                    b'(' => "(",
                    b')' => ")",
                    b'[' => "[",
                    b']' => "]",
                    b',' => ",",
                    b';' => ";",
                    b'*' => "*",
                    b'.' => ".",
                    _ => "!",
                }));
                i += 1;
            }
            _ if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                // 生の文字列 (`r"..."`・`r#"..."#`・`br"..."`・`cr#"..."#`) と、バイト文字列 (`b"..."`)・
                // C 文字列 (`c"..."`)。
                let mut j = i;
                if (c == b'b' || c == b'c') && bytes.get(j + 1) == Some(&b'r') {
                    j += 1;
                }
                let raw = bytes[j] == b'r';
                if raw || c == b'b' || c == b'c' {
                    let mut k = j + 1;
                    let mut hashes = 0usize;
                    if raw {
                        while bytes.get(k) == Some(&b'#') {
                            hashes += 1;
                            k += 1;
                        }
                    } else {
                        k = i + 1;
                    }
                    if bytes.get(k) == Some(&b'"') {
                        if raw {
                            i = skip_raw_string(bytes, k + 1, hashes);
                        } else {
                            i = skip_string(bytes, k + 1);
                        }
                        continue;
                    }
                }
                // 識別子 (`r#name` は `name`)。
                let start = if c == b'r' && bytes.get(i + 1) == Some(&b'#') {
                    i + 2
                } else {
                    i
                };
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric()
                        || bytes[end] == b'_'
                        || bytes[end] >= 0x80)
                {
                    end += 1;
                }
                if end == start {
                    i += 1;
                    continue;
                }
                tokens.push(Token::Ident(
                    String::from_utf8_lossy(&bytes[start..end]).into_owned(),
                ));
                i = end;
            }
            _ => i += 1,
        }
    }
    tokens
}

/// `"` の次の位置 `start` から、文字列の終わりの次の位置。
fn skip_string(bytes: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// 生の文字列の `"` の次の位置 `start` から、`"` と `#` `hashes` 個の終わりの次の位置。
fn skip_raw_string(bytes: &[u8], start: usize, hashes: usize) -> usize {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'"' && (0..hashes).all(|offset| bytes.get(i + 1 + offset) == Some(&b'#')) {
            return i + 1 + hashes;
        }
        i += 1;
    }
    i
}

/// `use` の木の葉 1 つ (経路のセグメント (glob は最後が `*`、group の `self` は `self`) と別名)。
#[derive(Clone, Debug)]
struct Leaf {
    segments: Vec<String>,
    alias: Option<String>,
}

/// 字句 `tokens` の `use` の宣言の葉。
fn use_leaves(tokens: &[Token]) -> Vec<Leaf> {
    let mut leaves = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let is_use = tokens[i] == Token::Ident("use".to_owned())
            && (i == 0 || tokens[i - 1] != Token::Punct("."));
        if !is_use {
            i += 1;
            continue;
        }
        // `use` から `;` まで (group の `{}` の中の `;` は無い)。
        let end = tokens[i + 1..]
            .iter()
            .position(|token| *token == Token::Punct(";"))
            .map_or(tokens.len(), |at| i + 1 + at);
        parse_tree(&tokens[i + 1..end], &[], &mut leaves);
        i = end + 1;
    }
    leaves
}

/// use の木 (`a::b::{c, d as e}`・`a::*`) を経路 `prefix` に続けて読み、葉を `leaves` に足す。読んだ字句の数
/// (1 以上)。
fn parse_tree(tokens: &[Token], prefix: &[String], leaves: &mut Vec<Leaf>) -> usize {
    let mut segments = prefix.to_vec();
    let mut i = 0;
    // 先頭の `::` (外部 crate)。
    if tokens.first() == Some(&Token::Punct("::")) {
        i += 1;
    }
    loop {
        match tokens.get(i) {
            Some(Token::Ident(word)) => {
                segments.push(word.clone());
                i += 1;
                if tokens.get(i) == Some(&Token::Punct("::")) {
                    i += 1;
                    continue;
                }
                let mut alias = None;
                if tokens.get(i) == Some(&Token::Ident("as".to_owned())) {
                    if let Some(Token::Ident(name)) = tokens.get(i + 1) {
                        alias = Some(name.clone());
                    }
                    i += 2;
                }
                leaves.push(Leaf { segments, alias });
                return i;
            }
            Some(Token::Punct("*")) => {
                segments.push("*".to_owned());
                leaves.push(Leaf {
                    segments,
                    alias: None,
                });
                return i + 1;
            }
            Some(Token::Punct("{")) => {
                i += 1;
                loop {
                    match tokens.get(i) {
                        None => return i,
                        Some(Token::Punct("}")) => return i + 1,
                        Some(Token::Punct(",")) => i += 1,
                        Some(_) => i += parse_tree(&tokens[i..], &segments, leaves),
                    }
                }
            }
            _ => return i.max(1),
        }
    }
}

/// 字句 `tokens` の、module の位置 (ファイルと `mod` の本体の直下) のマクロの呼び出しと `macro_rules!` の
/// 字句に現れる名前。
fn module_macro_names(tokens: &[Token]) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    // 開いている `{` ごとの、`mod` の本体か。
    let mut stack: Vec<bool> = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let module_level = stack.iter().all(|module| *module);
        match &tokens[i] {
            Token::Ident(called)
                if module_level && tokens.get(i + 1) == Some(&Token::Punct("!")) =>
            {
                // `name!(..)`・`name![..]`・`name! {..}`・`macro_rules! name {..}`。呼ぶマクロの名前 (別名の
                // 取り込みかもしれない) と、定義する名前も数える。呼び出しの経路の名前 (`crate::selected::m!`
                // の `selected`) も。
                names.insert(called.clone());
                let mut at = i;
                while at >= 2 && tokens[at - 1] == Token::Punct("::") {
                    let Token::Ident(segment) = &tokens[at - 2] else {
                        break;
                    };
                    names.insert(segment.clone());
                    at -= 2;
                }
                let mut open = i + 2;
                if let Some(Token::Ident(defined)) = tokens.get(open) {
                    names.insert(defined.clone());
                    open += 1;
                }
                if matches!(tokens.get(open), Some(Token::Punct("(" | "[" | "{"))) {
                    let end = closing(tokens, open);
                    for token in &tokens[open..end] {
                        if let Token::Ident(word) = token {
                            names.insert(word.clone());
                        }
                    }
                    i = end;
                } else {
                    i += 1;
                }
            }
            Token::Punct("{") => {
                let module = i >= 2
                    && matches!(tokens[i - 1], Token::Ident(_))
                    && tokens[i - 2] == Token::Ident("mod".to_owned());
                stack.push(module);
                i += 1;
            }
            Token::Punct("}") => {
                stack.pop();
                i += 1;
            }
            _ => i += 1,
        }
    }
    names
}

/// 字句 `tokens` の位置 `open` の開き括弧に対応する閉じ括弧の次の位置 (無ければ終わり)。
fn closing(tokens: &[Token], open: usize) -> usize {
    let mut depth = 0usize;
    for (at, token) in tokens.iter().enumerate().skip(open) {
        match token {
            Token::Punct("(" | "[" | "{") => depth += 1,
            Token::Punct(")" | "]" | "}") => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return at + 1;
                }
            }
            _ => {}
        }
    }
    tokens.len()
}

/// 葉 `leaf` が、名前 `wanted` (名前 → それを持つ module。取り込み直した別名は空) のどれかを経路に使い得るか。
fn leaf_uses(leaf: &Leaf, wanted: &BTreeMap<String, Vec<Vec<String>>>) -> bool {
    let segments = &leaf.segments;
    segments.iter().enumerate().any(|(position, segment)| {
        let Some(modules) = wanted.get(segment) else {
            return false;
        };
        // 経路の先頭か途中 (直前が別名の module でも、後ろにセグメントが続けば経路に使う)。
        if position == 0 || position + 1 < segments.len() {
            return true;
        }
        // module を通って取り込む (直前が module の最後の名前・`super`・`self`・`crate`)。
        let before = segments[position - 1].as_str();
        matches!(before, "super" | "self" | "crate")
            || modules
                .iter()
                .any(|module| module.last().map(String::as_str) == Some(before))
    })
}

/// ソース `sources` の `use` の宣言か module の位置のマクロのどれかが、(module, 名前) `names` の名前を経路に
/// 使い得るか。
fn mentioned_in(sources: &[String], names: &[(Vec<String>, String)]) -> bool {
    let mut wanted: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
    for (module, name) in names {
        wanted.entry(name.clone()).or_default().push(module.clone());
    }
    // 字句は名前を含むファイルだけ分ける (別名が増えたら、それを含むファイルも)。名前の数によらず、ファイルの
    // 単語を 1 度ずつ引く (コメント・文字列の中も数える下見)。
    // 単語は字句と同じく、ASCII の英数字・`_` と ASCII でない文字の並び (結合文字を含む識別子も切らない)。
    let has_word = |source: &str, wanted: &BTreeMap<String, Vec<Vec<String>>>| {
        source
            .split(|c: char| c.is_ascii() && !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|word| !word.is_empty() && wanted.contains_key(word))
    };
    let mut scanned: Vec<Option<(Vec<Leaf>, BTreeSet<String>)>> = vec![None; sources.len()];
    loop {
        let mut added = false;
        for (index, source) in sources.iter().enumerate() {
            if scanned[index].is_none() && !has_word(source, &wanted) {
                continue;
            }
            let (leaves, macro_names) = scanned[index].get_or_insert_with(|| {
                let tokens = lex(source);
                (use_leaves(&tokens), module_macro_names(&tokens))
            });
            if macro_names.iter().any(|name| wanted.contains_key(name)) {
                return true;
            }
            for leaf in leaves.iter() {
                if leaf_uses(leaf, &wanted) {
                    return true;
                }
                // 取り込み直した別名も、同じく使われるかを調べる。
                if let (Some(last), Some(alias)) = (leaf.segments.last(), &leaf.alias)
                    && wanted.contains_key(last)
                    && !wanted.contains_key(alias)
                {
                    wanted.insert(alias.clone(), Vec::new());
                    added = true;
                }
            }
        }
        if !added {
            return false;
        }
    }
}

/// package `package` (`dir` 相対) の `.rs` のソース (作業ツリー) と、旧版のソース `old_sources` の `use` の宣言か
/// module の位置のマクロのどれかが、(module, 名前) `names` の名前を経路に使い得るか。読めないファイルか大き過ぎる
/// ファイルがあるか、ソースの合計が上限を超えれば使い得るとする。旧版のソースは 1 つずつ受け取り、上限を超えたら
/// 残りを読まない。
pub(crate) fn mentions(
    dir: &str,
    package: &str,
    names: &[(Vec<String>, String)],
    old_sources: impl Iterator<Item = Vec<u8>>,
) -> bool {
    mentions_within(dir, package, names, old_sources, MAX_SCANNED_BYTES)
}

/// [`mentions`] の、読むソースの合計の上限を `limit` にしたもの。
fn mentions_within(
    dir: &str,
    package: &str,
    names: &[(Vec<String>, String)],
    old_sources: impl Iterator<Item = Vec<u8>>,
    limit: u64,
) -> bool {
    if names.is_empty() {
        return false;
    }
    let mut sources: Vec<String> = Vec::new();
    let mut scanned: u64 = 0;
    for source in old_sources {
        if source.len() as u64 > MAX_FILE_BYTES {
            return true;
        }
        scanned += source.len() as u64;
        if scanned > limit {
            return true;
        }
        sources.push(
            String::from_utf8(source)
                .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned()),
        );
    }
    let root = Path::new(dir).join(package);
    for entry in ignore::WalkBuilder::new(&root).build().flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let Some(size) = path
            .metadata()
            .ok()
            .map(|meta| meta.len())
            .filter(|&size| size <= MAX_FILE_BYTES)
        else {
            return true;
        };
        scanned += size;
        if scanned > limit {
            return true;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            return true;
        };
        sources.push(source);
    }
    mentioned_in(&sources, names)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn module(path: &[&str]) -> Vec<String> {
        path.iter().map(|segment| (*segment).to_owned()).collect()
    }

    fn uses(sources: &[&str], name: &str) -> bool {
        let sources: Vec<String> = sources.iter().map(|source| (*source).to_owned()).collect();
        mentioned_in(&sources, &[(module(&["review", "hook"]), name.to_owned())])
    }

    #[test]
    fn paths_through_the_module_or_at_the_head_use_the_name() {
        assert!(uses(&["pub use crate::review::hook::Map;"], "Map"));
        assert!(uses(&["use super::{Map, Other};"], "Map"));
        assert!(uses(&["pub use hook::{inner::{x}, Map as M};"], "Map"));
        assert!(uses(&["use Map::Item;"], "Map"));
        assert!(!uses(&["use std::collections::Map;"], "Map"));
        assert!(!uses(&["use std::{collections::Map};"], "Map"));
        // 別名・コメント・文字列の中の名前は数えない
        assert!(!uses(&["use std::collections::BTreeMap as Map;"], "Map"));
        assert!(!uses(
            &["// use crate::review::hook::Map;\nlet s = \"use self::Map\";"],
            "Map"
        ));
        assert!(!uses(
            &["let r = r#\"use self::Map;\"#; let c = '\\'';"],
            "Map"
        ));
        // C 文字列・raw C 文字列の中の `"` で、後ろの `use` を飲み込まない
        assert!(uses(
            &["const C: &CStr = cr#\"\"\"#;\npub use crate::review::hook::Map;"],
            "Map"
        ));
        assert!(uses(
            &["const C: &CStr = c\"\\\"\";\npub use crate::review::hook::Map;"],
            "Map"
        ));
        assert!(!uses(&["let c = cr\"use self::Map;\";"], "Map"));
        // 経路で呼ぶマクロの経路の名前は、名前を使い得る
        assert!(uses(&["crate::review::hook::Map::make!();"], "Map"));
    }

    /// 経路の途中の名前は、直前が別名の module でも使う。取り込み直した別名を経路に使えば使う。
    #[test]
    fn paths_through_aliases_use_the_name() {
        assert!(uses(
            &["use crate as root_alias;\npub use root_alias::Map::item;"],
            "Map"
        ));
        assert!(uses(&["pub use root_alias::{Map::item};"], "Map"));
        assert!(uses(&["pub use other::Map::*;"], "Map"));
        assert!(uses(
            &[
                "use std::collections::Map as Other;",
                "pub use Other::Inner;"
            ],
            "Map"
        ));
        assert!(!uses(&["use std::collections::Map as Other;"], "Map"));
    }

    /// module の位置のマクロの呼び出しと `macro_rules!` の字句の名前は使い得る。関数の中は数えない。
    #[test]
    fn macros_at_module_level_use_the_name() {
        assert!(uses(&["reexport!(Map);"], "Map"));
        assert!(uses(&["mod inner {\n    reexport! { Map }\n}"], "Map"));
        assert!(uses(
            &["macro_rules! make {\n    () => { pub type Alias = $crate::Map; };\n}"],
            "Map"
        ));
        assert!(!uses(
            &["fn f() {\n    println!(\"{}\", Map::new());\n}"],
            "Map"
        ));
        assert!(!uses(&["impl S {\n    reexport!(Map);\n}"], "Map"));
        // 呼ぶマクロの名前 (別名の取り込みかもしれない) も
        assert!(uses(&["pub mod api {\n    super::Map!();\n}"], "Map"));
    }

    /// 結合文字を含む識別子も、下見で落とさない。
    #[test]
    fn identifiers_with_combining_characters_are_scanned() {
        let name = "cafe\u{301}";
        let source = format!("pub use {name}::item;");
        assert!(uses(&[source.as_str()], name));
    }

    /// 読むソースの合計が上限を超えれば、名前を使うかを調べずに使い得るとする (全てのソースを抱えない)。
    #[test]
    fn sources_over_the_limit_may_use_the_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("notes").join("src");
        std::fs::create_dir_all(&src).expect("mkdir");
        std::fs::write(src.join("lib.rs"), "use std::fmt;\n").expect("write");
        std::fs::write(src.join("other.rs"), "pub fn other() {}\n").expect("write");
        let dir = dir.path().to_str().expect("utf-8");
        let names = [(module(&["review", "hook"]), "Map".to_owned())];
        let none = std::iter::empty::<Vec<u8>>;
        assert!(!mentions_within(dir, "notes", &names, none(), 1024));
        assert!(mentions_within(dir, "notes", &names, none(), 16));
        // 旧版のソースも数える (上限を超えたら残りを読まない)。
        let old = || std::iter::once("// ".repeat(8).into_bytes());
        assert!(!mentions_within(dir, "notes", &names, old(), 1024));
        assert!(mentions_within(dir, "notes", &names, old(), 40));
        assert!(mentions_within(
            dir,
            "notes",
            &names,
            old().chain(std::iter::from_fn(|| panic!("読み過ぎ"))),
            16
        ));
    }
}
