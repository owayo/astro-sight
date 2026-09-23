use super::location::Range;
use serde::{Deserialize, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SymbolKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Interface,
    Trait,
    Variable,
    Constant,
    Module,
    Import,
    Type,
    Field,
    Parameter,
}

/// ソースコードから抽出したシンボル定義。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub complexity: Option<usize>,
    /// 直接の enclosing container (class/struct/trait/interface/enum/type) 名。
    /// Rust の `impl Default for A { fn default() {} }` の `default` には `A` を付与し、
    /// 同一ファイル内の同名メソッドを見分けられるようにする。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub children: Vec<Symbol>,
    /// 名前ノードの位置。`range` (宣言全体) を複数シンボルが共有する場合にだけ設定する
    /// (JS/TS の分割代入 `const { a, b } = obj` は `a` / `b` が同じ declarator を range に持つ)。
    /// range から名前を引き直せない下流 (export 判定・束縛ごとの signature・宣言行) が使う
    /// 内部情報なので出力には含めない。
    #[serde(skip)]
    pub name_range: Option<Range>,
}

/// トークン最適化出力用の compact シンボル。
#[derive(Debug, Clone, Serialize)]
pub struct CompactSymbol {
    pub name: String,
    #[serde(serialize_with = "serialize_compact_kind")]
    pub kind: SymbolKind,
    #[serde(rename = "ln")]
    pub line: usize,
    /// 循環的複雑度（関数/メソッドのみ）
    #[serde(rename = "cx", skip_serializing_if = "Option::is_none")]
    pub complexity: Option<usize>,
    /// enclosing container 名 (例: `impl Default for A` の中の method なら "A")。
    /// 同名メソッドの見分けを付けやすくするため compact 出力でも残す。
    #[serde(rename = "cn", skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub children: Vec<CompactSymbol>,
}

fn serialize_compact_kind<S: Serializer>(kind: &SymbolKind, s: S) -> Result<S::Ok, S::Error> {
    let short = match kind {
        SymbolKind::Function => "fn",
        SymbolKind::Method => "method",
        SymbolKind::Class => "class",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Interface => "iface",
        SymbolKind::Trait => "trait",
        SymbolKind::Variable => "var",
        SymbolKind::Constant => "const",
        SymbolKind::Module => "mod",
        SymbolKind::Import => "import",
        SymbolKind::Type => "type",
        SymbolKind::Field => "field",
        SymbolKind::Parameter => "param",
    };
    s.serialize_str(short)
}

impl Symbol {
    /// このシンボルを一意に指す位置。宣言を共有するシンボルは名前ノード、それ以外は宣言。
    /// export 判定のように「どの名前か」を区別する必要がある判定に渡す。
    pub fn identity_range(&self) -> &Range {
        self.name_range.as_ref().unwrap_or(&self.range)
    }

    /// 名前が現れる行 (0-indexed)。複数行の分割代入でも各名前の行を返す。
    pub fn name_line(&self) -> usize {
        self.identity_range().start.line
    }

    pub fn to_compact(&self, include_doc: bool) -> CompactSymbol {
        CompactSymbol {
            name: self.name.clone(),
            kind: self.kind,
            line: self.name_line(),
            complexity: self.complexity,
            container: self.container.clone(),
            doc: if include_doc { self.doc.clone() } else { None },
            children: self
                .children
                .iter()
                .map(|c| c.to_compact(include_doc))
                .collect(),
        }
    }
}

/// シンボルへの参照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reference {
    pub name: String,
    pub range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}
