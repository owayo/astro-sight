const LABEL: &str = "\u{4f_dd}\u{5b__58_}します。";
fn after_unicode() -> &'static str { LABEL }
fn main() { let prefix = "日本語"; let label = after_unicode(); assert_eq!((prefix, label), ("日本語", "保存します。")); }
