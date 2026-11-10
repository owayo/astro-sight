//! 契約が名指すパスを、版ごとの文脈で識別の字面に解く (Issue #283・#300)。
//!
//! 解く場所は「どのファイルの索引の、どの inline module のスコープか」([`ModuleRef`]) で持つ。版の
//! crate ([`CrateFiles`]) があれば、ファイルの外へ出るパス (`crate::`・ファイルの外の `super::`・
//! inline でない子の `mod x;`) は、その版のそのファイルの索引で解き続ける。無ければ、今までどおり
//! 字面 (`crate:`・`mod-file:`) で識別する。

use super::*;
use crate::commands::api_changes::rust_cfg_predicate::{Atoms, Predicate};
use std::rc::Rc;

/// 共有するファイルの索引。
pub(crate) type File = Rc<DependencyIndex>;

/// 版の crate の、module のファイルを開く手段 (Issue #300)。
pub(crate) trait CrateFiles {
    /// crate root (lib) のファイル。
    fn root(&self) -> Opened;
    /// ファイル `parent` のスコープ `scope` にある inline でない `mod name;` の宣言 `declaration` の中身の
    /// ファイル。
    fn child(
        &self,
        parent: &DependencyIndex,
        scope: &[String],
        name: &str,
        declaration: &ModuleDeclaration,
    ) -> Opened;
}

/// module のファイルを開いた結果。
pub(crate) enum Opened {
    File(File),
    /// 差分に無い (旧新で同じ中身の) ファイルの構文エラー (パス)。その先は字面で識別し、旧新で
    /// 同じとみなす。
    Unparsed(String),
    /// 開けない (場所を決められない・読めない・差分の中のファイルの構文エラー)。
    Unknown,
}

/// module のスコープ (どのファイルの索引の、どの inline module の経路か)。
#[derive(Clone)]
pub(crate) struct ModuleRef {
    pub(crate) file: File,
    pub(crate) scope: Vec<String>,
}

impl PartialEq for ModuleRef {
    fn eq(&self, other: &Self) -> bool {
        self.file.path == other.file.path
            && self.file.file_module == other.file.file_module
            && self.scope == other.scope
    }
}

impl Eq for ModuleRef {}

impl std::fmt::Debug for ModuleRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}#{}", self.file.path, self.scope.join("::"))
    }
}

/// 束縛の先。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// module のスコープ。
    Module(ModuleRef),
    /// 識別の字面。
    Identity(String),
}

/// 束縛の候補。条件 (cfg) の付いた束縛は、条件ごとの候補に分かれる。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    /// 候補の条件 (cfg のトークン)。外側の条件から順に並ぶ。
    pub(crate) conditions: Vec<String>,
    pub(crate) target: Target,
    /// 名前を引いたスコープ (か同じファイルの祖先のスコープ) で見つけた型別名・定数・static・項目の
    /// 定義で、項目を置き換え得る属性が無いか (Issue #312)。条件 `conditions` がそのまま定義の有る条件
    /// なので、宣言の有る設定を覆うかの証明に使える。`use`・glob・module の経路・ほかのファイルを
    /// 通った候補は、経路の条件や先の有無が欠け得るので使わない。
    pub(crate) direct: bool,
}

impl Candidate {
    fn new(conditions: &[String], target: Target) -> Self {
        Self {
            conditions: conditions.to_vec(),
            target,
            direct: false,
        }
    }

    /// 名前を引いたスコープの定義の候補。`opaque` は覆いの証明に使えないか (項目を置き換え得る属性が
    /// 付いている、値の名前で引いた名前付きのフィールドの構造体)。
    fn definition(conditions: &[String], target: Target, opaque: bool) -> Self {
        Self {
            conditions: conditions.to_vec(),
            target,
            direct: !opaque,
        }
    }
}

/// 束縛を解いた結果 (候補の並び)。
#[derive(Debug)]
pub(crate) struct Resolved(pub(crate) Vec<Candidate>);

impl Resolved {
    /// 条件の無い 1 つの先。
    fn single(target: Target) -> Self {
        Self(vec![Candidate::new(&[], target)])
    }

    /// 各候補の条件の前に、外側の条件 `conditions` を足した候補。
    fn under(self, conditions: &[String]) -> Vec<Candidate> {
        self.0
            .into_iter()
            .map(|mut candidate| {
                let mut all = conditions.to_vec();
                all.append(&mut candidate.conditions);
                candidate.conditions = all;
                // 経路 (`use`・glob・module) を通った候補は、名前を引いたスコープの定義ではない。
                candidate.direct = false;
                candidate
            })
            .collect()
    }
}

/// 版ごとの、束縛を解く文脈。
pub(crate) struct Resolver<'c> {
    /// ほかのファイルの module を開く、その版の crate。無ければファイルの外へ出るパスは字面で
    /// 識別する。
    pub(crate) crate_files: Option<&'c dyn CrateFiles>,
    /// 比べるファイル。版の crate が無ければ、その中の定義はファイル相対の場所で識別する。
    pub(crate) current: File,
    /// 新しい版で、移した定義 (移した先の module の経路と名前 → 移す前の module の経路)。移す前の
    /// 場所で識別し、別名の右辺の中で移しただけの定義を変化にしない。
    pub(crate) moves: Option<&'c MoveMap>,
    /// 束縛の変わり得る名前と module。契約のパスがどれにも触れなければ、ほかのファイルを開かずに
    /// 字面で比べる (旧新で必ず同じ)。None なら全てのパスを版の crate で解く。
    pub(crate) changes: Option<&'c ChangeSet>,
    /// 引いている最中の束縛。同じものを引き直したら循環 (`pub(super) use super::X;` を glob で
    /// 再公開する親など) なので、その枝では無かったことにする。
    active: std::cell::RefCell<Vec<LookupKey>>,
    /// 経路を解いている最中の glob。その経路の module の区切りを、同じ glob から引かない。
    visiting_globs: std::cell::RefCell<Vec<GlobKey>>,
    /// 束縛を解いている宣言の有る条件 (Issue #312。[`Self::declaration_dependencies`] の間だけ)。
    availability: std::cell::RefCell<Option<Rc<AvailabilityFilter>>>,
    /// 候補の条件のトークン → 有る条件の属性の鍵 ([`condition_keys`]。読めなければ None)。
    condition_keys: std::cell::RefCell<HashMap<Vec<String>, Option<ConditionKeys>>>,
    /// 型別名・定数の右辺を描かずに候補を集める下見の最中か ([`Self::resolve_candidates`])。
    skip_renders: std::cell::Cell<bool>,
}

/// 有る条件の属性の鍵の並び (`cfg` と、`cfg` だけに絞った `cfg_attr`)。
type ConditionKeys = Vec<Vec<String>>;

/// [`Resolver::swap_filter`] で差し替えた宣言の有る条件を、落ちるときに戻す (`?` で抜けても戻す)。
struct FilterGuard<'a> {
    cell: &'a std::cell::RefCell<Option<Rc<AvailabilityFilter>>>,
    saved: Option<Rc<AvailabilityFilter>>,
}

impl Drop for FilterGuard<'_> {
    fn drop(&mut self) {
        *self.cell.borrow_mut() = self.saved.take();
    }
}

/// [`Resolver::skipping_renders`] で右辺を描かないようにした状態を、落ちるときに戻す。
struct SkipGuard<'a> {
    cell: &'a std::cell::Cell<bool>,
    saved: bool,
}

impl Drop for SkipGuard<'_> {
    fn drop(&mut self) {
        self.cell.set(self.saved);
    }
}

/// 宣言の有る条件 (Issue #312)。宣言の有る設定と、どの設定でも両立しない条件の束縛の候補は、
/// その宣言からは使われないので比べない (`#[cfg(not(x))] pub fn run(_: Text)` は、
/// `#[cfg(x)] type Text = ..;` の枝を使わない)。条件の読み方は項目の有る設定 (Issue #291) と同じ
/// (`rust_cfg_predicate`) だが、組み込みの cfg の関係 (`unix` と `windows` は同時に真にならない、など)
/// は `--cfg` で上書きでき得るので使わない ([`Atoms::independent`])。両立するかを決められない (読めない
/// 属性・原子が多すぎる) 候補は残す。
///
/// 宣言の有る条件は宣言自身の条件だけ (親の module の条件は含めないので、実際に有る設定を含む)。
/// 宣言の有る設定が旧新で変わるときは、呼び出し側が絞らない (`compare_dependencies`)。
pub(crate) struct AvailabilityFilter {
    /// 宣言自身の有る条件の属性の鍵 (`cfg` と、`cfg` だけに絞った `cfg_attr`。全て成り立つ)。
    own: ConditionKeys,
}

impl AvailabilityFilter {
    /// 宣言自身の有る条件の属性 `own` の。条件が無いか読めなければ (絞らないので) None。
    pub(crate) fn new(own: Option<&[AvailabilityAttr]>) -> Option<Self> {
        let own: ConditionKeys = own?.iter().map(|attribute| attribute.key.clone()).collect();
        (!own.is_empty()).then_some(Self { own })
    }

    /// 宣言の有る設定の述語。
    fn predicate(&self, atoms: &mut Atoms) -> Predicate {
        Predicate::All(self.own.iter().map(|key| atoms.presence(key)).collect())
    }

    /// 有る条件の属性の鍵 `keys` (候補の条件。読めなければ None) の付いた束縛が、宣言の有るどれかの
    /// 設定で有り得るか。決められなければ真。
    fn compatible(&self, keys: Option<&ConditionKeys>) -> bool {
        let Some(keys) = keys else {
            return true;
        };
        if keys.is_empty() {
            return true;
        }
        // 組み込みの cfg は `--cfg` で上書きでき得るので、その関係は使わない (見逃しを避ける)。
        let mut atoms = Atoms::independent();
        let declaration = self.predicate(&mut atoms);
        let candidate = Predicate::All(keys.iter().map(|key| atoms.presence(key)).collect());
        let Some(mut configs) = atoms.configs() else {
            return true;
        };
        configs.any(|config| declaration.holds(config) && candidate.holds(config))
    }

    /// 有る条件の属性の鍵の並び `branches` (残す候補の条件) のどれかが、宣言の有る全ての設定で
    /// 成り立つか。決められなければ偽。
    fn covered(&self, branches: &[ConditionKeys]) -> bool {
        if branches.iter().any(Vec::is_empty) {
            return true;
        }
        let mut atoms = Atoms::independent();
        let declaration = self.predicate(&mut atoms);
        let present = Predicate::Any(
            branches
                .iter()
                .map(|keys| Predicate::All(keys.iter().map(|key| atoms.presence(key)).collect()))
                .collect(),
        );
        let Some(mut configs) = atoms.configs() else {
            return false;
        };
        !configs.any(|config| declaration.holds(config) && !present.holds(config))
    }
}

/// 条件のトークン (`cfg`・`cfg_attr` の属性 (`#[..]`・`#![..]`) を [`push_tokens`] で並べたもの) の、
/// 有る条件に効く属性の鍵 (`project_attribute` で `cfg` に絞ったもの。`cfg` を含まない `cfg_attr` は
/// 入れない)。属性ごとに字面へ戻して読み直す。読めなければ None。
fn condition_keys(conditions: &[String]) -> Option<ConditionKeys> {
    let mut keys = Vec::new();
    for attribute in condition_attributes(conditions)? {
        // 内側の属性 (`#![..]`) は、外側の属性として読む (付く項目の有る条件は同じ)。
        let tokens = match attribute {
            [hash, bang, rest @ ..] if hash == "#" && bang == "!" => rest,
            [hash, rest @ ..] if hash == "#" => rest,
            _ => return None,
        };
        let mut text = String::from("#");
        for token in tokens {
            let (kind, plain) = token_text(token);
            text.push(' ');
            text.push_str(plain);
            if kind == Some("line_comment") {
                text.push('\n');
            }
        }
        text.push_str("\nstruct AstroSightCondition;\n");
        let source = text.as_bytes();
        let tree = parser::parse_source(source, crate::language::LangId::Rust).ok()?;
        let root = tree.root_node();
        if root.has_error() || root.named_child_count() != 2 {
            return None;
        }
        let item = root
            .named_child(0)
            .filter(|node| node.kind() == "attribute_item")?;
        if let Some(attribute) =
            super::super::rust_type_api::project_attribute(item, source, &["cfg"])?
        {
            keys.push(attribute.key);
        }
    }
    Some(keys)
}

/// 条件のトークンを、属性 (`#` で始まる並び) ごとに分ける。括弧の中の `#` (`cfg_attr` の中の属性の
/// 引数) では分けない。`#` で始まらなければ None。
fn condition_attributes(conditions: &[String]) -> Option<Vec<&[String]>> {
    if conditions.first().is_none_or(|first| first != "#") {
        return None;
    }
    let mut attributes = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (index, token) in conditions.iter().enumerate() {
        match token.as_str() {
            "#" if depth == 0 && index > start => {
                attributes.push(&conditions[start..index]);
                start = index;
            }
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    attributes.push(&conditions[start..]);
    Some(attributes)
}

/// [`push_tokens`] のトークンの、名前付きの葉の種類 (`identifier:x` の `identifier`) と字面。
fn token_text(token: &str) -> (Option<&str>, &str) {
    match token.split_once(':') {
        Some((kind, text))
            if !kind.is_empty()
                && kind
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_') =>
        {
            (Some(kind), text)
        }
        _ => (None, token),
    }
}

/// 引いている束縛 (ファイル, ファイルの module の経路, スコープ, 名前, 名前空間, module の区切りか,
/// 取り込む側の module)。同じファイルを別の module として読む (`#[path]`) こともあるので、module の
/// 経路も入れる。
type LookupKey = (
    String,
    Option<Vec<String>>,
    Vec<String>,
    String,
    Namespace,
    bool,
    Option<Vec<String>>,
);

/// 経路を解いている glob (ファイル, ファイルの module の経路, スコープ, glob の添字)。
type GlobKey = (String, Option<Vec<String>>, Vec<String>, usize);

impl<'c> Resolver<'c> {
    pub(crate) fn new(
        crate_files: Option<&'c dyn CrateFiles>,
        current: File,
        moves: Option<&'c MoveMap>,
        changes: Option<&'c ChangeSet>,
    ) -> Self {
        Self {
            crate_files,
            current,
            moves,
            changes,
            active: std::cell::RefCell::new(Vec::new()),
            visiting_globs: std::cell::RefCell::new(Vec::new()),
            availability: std::cell::RefCell::new(None),
            condition_keys: std::cell::RefCell::new(HashMap::new()),
            skip_renders: std::cell::Cell::new(false),
        }
    }
}

/// スコープ `module` の crate root からの経路 (ファイルの module の経路が分からなければ None)。
fn scope_module(module: &ModuleRef) -> Option<Vec<String>> {
    let mut path = module.file.file_module.clone()?;
    path.extend(module.scope.iter().cloned());
    Some(path)
}

/// crate root からの module の経路の字面 (`crate`・`crate::a::b`)。
pub(crate) fn absolute_place<'a>(segments: impl IntoIterator<Item = &'a String>) -> String {
    let mut place = String::from("crate");
    for segment in segments {
        place.push_str("::");
        place.push_str(segment);
    }
    place
}

impl Resolver<'_> {
    /// 版の crate を使わない (ファイルの外へ出るパスを字面で識別する) 文脈。
    pub(crate) fn textual(current: File) -> Resolver<'static> {
        Resolver::new(None, current, None, None)
    }

    /// module の場所の字面。版の crate が無ければ、比べるファイルの中はファイル相対の inline module
    /// の経路 (ファイルの rename で変えない)。ほかは crate root からの経路 (`crate`・`crate::a::b`)。
    /// 相対の経路は `crate` で始まらない (`crate` という名前の module は無い)。
    pub(crate) fn place(&self, file: &DependencyIndex, scope: &[String]) -> String {
        if self.crate_files.is_none() && file.path == self.current.path {
            return scope.join("::");
        }
        absolute_place(file.file_module.iter().flatten().chain(scope))
    }

    /// 定義 `name` (ファイル `file` のスコープ `scope`) の場所の字面。新しい版で移した定義は、移す前の
    /// 場所にする。
    fn definition_place(&self, file: &DependencyIndex, scope: &[String], name: &str) -> String {
        if let (Some(moves), Some(file_module)) = (self.moves, &file.file_module) {
            let mut module = file_module.clone();
            module.extend(scope.iter().cloned());
            if let Some(from) = moves.get(&(module, name.to_owned())) {
                return absolute_place(from);
            }
        }
        self.place(file, scope)
    }

    /// 条件 `conditions` (候補の cfg のトークン) の有る条件の属性の鍵 (読めなければ None)。
    fn keys_of(&self, conditions: &[String]) -> Option<ConditionKeys> {
        if conditions.is_empty() {
            return Some(Vec::new());
        }
        self.condition_keys
            .borrow_mut()
            .entry(conditions.to_vec())
            .or_insert_with(|| condition_keys(conditions))
            .clone()
    }

    /// 宣言の有る条件 (絞り込み) を `filter` に差し替える。返すガードが落ちるときに元へ戻す。絞り込みは、
    /// 直接の定義をたどる経路 ([`Self::resolve_candidates`] の祖先の枝と、[`Self::lookup_bindings`] の
    /// 直接の定義の右辺) でだけ使い、ほかは外す (Issue #312)。
    fn swap_filter(&self, filter: Option<Rc<AvailabilityFilter>>) -> FilterGuard<'_> {
        let saved = std::mem::replace(&mut *self.availability.borrow_mut(), filter);
        FilterGuard {
            cell: &self.availability,
            saved,
        }
    }

    /// 候補 `candidates` のうち宣言の有る条件 `filter` と両立するもの (添字) が、全て直接の定義で、宣言の
    /// 有る全ての設定を覆うなら、その添字。覆えば、その設定では明示の束縛が glob・prelude を隠すので、
    /// 両立しない候補は使われず、残す候補の右辺も同じ設定で絞れる。覆わない・決められないときは、その
    /// 設定で名前がほかで解け得るので None (全ての候補を、右辺も絞らずに比べる)。
    fn covering(
        &self,
        filter: &AvailabilityFilter,
        candidates: &[Candidate],
    ) -> Option<Vec<usize>> {
        let keys: Vec<Option<ConditionKeys>> = candidates
            .iter()
            .map(|candidate| self.keys_of(&candidate.conditions))
            .collect();
        let kept: Vec<usize> = (0..candidates.len())
            .filter(|&index| filter.compatible(keys[index].as_ref()))
            .collect();
        // 覆いを証明できるのは、名前を引いたスコープの直接の定義だけ (条件がそのまま有る条件)。
        if !kept.iter().all(|&index| candidates[index].direct) {
            return None;
        }
        let branches = kept
            .iter()
            .map(|&index| keys[index].clone())
            .collect::<Option<Vec<_>>>()?;
        filter.covered(&branches).then_some(kept)
    }

    /// 型別名・定数の右辺を描かずに候補を集める下見にする。返すガードが落ちるときに元へ戻す。
    fn skipping_renders(&self) -> SkipGuard<'_> {
        let saved = self.skip_renders.replace(true);
        SkipGuard {
            cell: &self.skip_renders,
            saved,
        }
    }

    /// 候補の、条件を前置きした識別の字面。
    fn identity(&self, candidate: &Candidate) -> String {
        let identity = match &candidate.target {
            Target::Module(module) => format!("mod:{}", self.place(&module.file, &module.scope)),
            Target::Identity(identity) => identity.clone(),
        };
        conditioned(&candidate.conditions, identity)
    }

    /// 候補の識別を並べ替えて `|` でつないだ字面。
    pub(crate) fn joined_identity(&self, candidates: &[Candidate]) -> String {
        let mut identities: Vec<String> = candidates
            .iter()
            .map(|candidate| self.identity(candidate))
            .collect();
        identities.sort();
        identities.dedup();
        identities.join("|")
    }

    /// 比べるファイルの、`start` (行, 列) に始まる宣言の契約が名指す名前の、字面ごとの束縛 (解け
    /// なければ None の要素)。宣言を引けなければ None。`availability` は宣言の有る条件 (Issue #312)。
    /// 両立しない条件の束縛の候補は、残りが宣言の有る設定を覆うときに比べない。
    pub(crate) fn declaration_dependencies(
        &self,
        start: (usize, usize),
        inline_module: &str,
        availability: Option<Rc<AvailabilityFilter>>,
    ) -> Option<HashMap<String, Vec<Option<String>>>> {
        let file = &self.current;
        let declaration = declaration_at(file.tree.root_node(), start)?;
        let module = ModuleRef {
            file: file.clone(),
            scope: if inline_module.is_empty() {
                Vec::new()
            } else {
                inline_module.split("::").map(str::to_owned).collect()
            },
        };
        // 宣言を囲む module の crate root からの経路。
        let declaration_module: Vec<String> = file
            .file_module
            .iter()
            .flatten()
            .chain(&module.scope)
            .cloned()
            .collect();
        let textual = Resolver::textual(file.clone());
        *self.availability.borrow_mut() = availability.clone();
        *textual.availability.borrow_mut() = availability;
        let mut found: HashMap<String, Vec<Option<String>>> = HashMap::new();
        for reference in contract_references(declaration, &file.source) {
            // 束縛の変わり得る名前や module に触れないパスは、旧新で同じ束縛なので字面で比べる
            // (無関係なパスの解けない経路を、比べる対象に持ち込まない)。
            let touches =
                |changes: &ChangeSet| changes.affects(&reference.segments, &declaration_module);
            let identities = match (self.crate_files, self.changes) {
                (Some(_), Some(changes)) if touches(changes) => {
                    self.resolve_identities(&module, &reference)
                }
                (Some(_), None) => self.resolve_identities(&module, &reference),
                // 比べるファイルの module を決められない (版の crate で解けない) とき、変わった束縛に
                // 触れるパスは同じと言えない。
                (None, Some(changes)) if touches(changes) => None,
                _ => textual.resolve_identities(&module, &reference),
            };
            // 候補ごとの識別の集まりで持つ (宣言を cfg の分岐に分けて、分岐ごとに絞っても同じになる)。
            let entry = found.entry(reference.text()).or_default();
            match identities {
                Some(identities) => {
                    for identity in identities.into_iter().map(Some) {
                        if !entry.contains(&identity) {
                            entry.push(identity);
                        }
                    }
                }
                None if !entry.contains(&None) => entry.push(None),
                None => {}
            }
        }
        *self.availability.borrow_mut() = None;
        for identities in found.values_mut() {
            identities.sort();
        }
        Some(found)
    }

    /// スコープ `module` で、パス `reference` を解いた識別。解けなければ None。
    pub(super) fn resolve_path(
        &self,
        module: &ModuleRef,
        reference: &Reference,
        depth: usize,
    ) -> Option<String> {
        let candidates = self.resolve_candidates(module, reference, depth)?;
        Some(self.joined_identity(&candidates))
    }

    /// パス `reference` を解いた候補ごとの識別の字面 (並べ替えて重なりを除く)。解けなければ None。
    fn resolve_identities(&self, module: &ModuleRef, reference: &Reference) -> Option<Vec<String>> {
        let candidates = self.resolve_candidates(module, reference, 0)?;
        let mut identities: Vec<String> = candidates
            .iter()
            .map(|candidate| self.identity(candidate))
            .collect();
        identities.sort();
        identities.dedup();
        Some(identities)
    }

    /// スコープ `module` で、パス `reference` を解いた候補 (宣言の有る条件で絞ったもの)。解けなければ
    /// (候補が多すぎるときも) None。
    fn resolve_candidates(
        &self,
        module: &ModuleRef,
        reference: &Reference,
        depth: usize,
    ) -> Option<Vec<Candidate>> {
        // 絞り込みは、直接の定義をたどる経路でだけ使う (経路の module の部分を解く間や、`use`・glob・
        // ほかのファイルを通る枝では外す)。局所の束縛を作り得る式の中の参照は、局所の `use`・項目が
        // 名前を隠し得るので、候補の右辺を描く先まで絞らない。
        let filter = self
            .availability
            .borrow()
            .clone()
            .filter(|_| !reference.local);
        let _off = self.swap_filter(None);
        // 覆いを証明できるかを、右辺を描かずに候補の条件だけで先に決める (描かないので再帰の手間は
        // 増えない)。証明できるときだけ、残す直接の定義の右辺も絞って描く。覆わずに全ての候補を残す
        // とき、右辺を絞ると main が比べていた枝を落とす。
        let filter = match filter {
            Some(filter) => {
                let candidates = {
                    let _skip = self.skipping_renders();
                    self.branch_candidates(module, reference, depth, None)?
                };
                self.covering(&filter, &candidates)
                    .is_some()
                    .then_some(filter)
            }
            None => None,
        };
        let candidates = self.branch_candidates(module, reference, depth, filter.clone())?;
        let candidates = match filter {
            Some(filter) => match self.covering(&filter, &candidates) {
                Some(kept) => candidates
                    .into_iter()
                    .enumerate()
                    .filter(|(index, _)| kept.contains(index))
                    .map(|(_, candidate)| candidate)
                    .collect(),
                // 下見で決めた覆いが崩れた (起きないはず) なら、絞らずに解き直す。
                None => self.branch_candidates(module, reference, depth, None)?,
            },
            None => candidates,
        };
        (candidates.len() <= MAX_CANDIDATES).then_some(candidates)
    }

    /// スコープ `module` で、パス `reference` を解いた候補 (除かない)。`filter` は、同じファイルの祖先
    /// (か自身) のスコープを `self`・`super`・`crate` だけで名指した枝で、直接の定義の右辺を描くときの
    /// 宣言の有る条件。
    fn branch_candidates(
        &self,
        module: &ModuleRef,
        reference: &Reference,
        depth: usize,
        filter: Option<Rc<AvailabilityFilter>>,
    ) -> Option<Vec<Candidate>> {
        let (name, prefix) = reference.segments.split_last()?;
        if name.starts_with("<macro ") {
            return Some(vec![Candidate::new(
                &[],
                Target::Identity(format!(
                    "{name}{SEPARATOR}{}",
                    module.file.macro_definitions.join(" ")
                )),
            )]);
        }
        // パスの module の部分が `self`・`super`・`crate` だけ (か無い) なら、`use` や module の
        // 束縛を通らずにスコープを決める。
        let keywords = prefix
            .iter()
            .all(|segment| matches!(segment.as_str(), "self" | "super" | "crate"));
        let mut candidates = Vec::new();
        for branch in self.resolve_module_prefix(module, prefix, depth)? {
            match branch.target {
                Target::Module(target) => {
                    // 同じファイルの祖先 (か自身) のスコープを `self::`・`super::`・`crate::` だけで
                    // 名指す経路は、定義の有る条件を変えないので、直接の定義はそのまま (Issue #312)。
                    // その直接の定義の右辺も絞って描く。
                    let ancestor = keywords
                        && branch.conditions.is_empty()
                        && target.file.path == module.file.path
                        && target.file.file_module == module.file.file_module
                        && module.scope.starts_with(&target.scope);
                    if ancestor {
                        let resolved = {
                            let _on = self.swap_filter(filter.clone());
                            self.resolve_name(&target, name, reference.namespace, depth)?
                        };
                        candidates.extend(resolved.0);
                    } else {
                        candidates.extend(
                            self.resolve_name(&target, name, reference.namespace, depth)?
                                .under(&branch.conditions),
                        );
                    }
                }
                Target::Identity(outside) => candidates.push(Candidate::new(
                    &branch.conditions,
                    Target::Identity(outside_path(&outside, std::slice::from_ref(name))),
                )),
            }
        }
        Some(candidates)
    }

    /// スコープ `module` からパスの module の部分 `segments` (`crate`・`self`・`super`・`::` で
    /// 始まり得る) を解いた枝 (module のスコープか、外のパスの識別)。空ならそのスコープ。
    fn resolve_module_prefix(
        &self,
        module: &ModuleRef,
        segments: &[String],
        depth: usize,
    ) -> Option<Vec<Candidate>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let outside =
            |identity: String| Some(vec![Candidate::new(&[], Target::Identity(identity))]);
        let Some((first, rest)) = segments.split_first() else {
            return Some(vec![Candidate::new(&[], Target::Module(module.clone()))]);
        };
        match first.as_str() {
            "::" => outside(outside_path("global:", rest)),
            "crate" => self.walk_absolute(&module.file, rest.to_vec(), depth),
            "self" => self.walk_modules(module.clone(), rest, depth),
            "super" => {
                let supers = segments
                    .iter()
                    .take_while(|segment| segment.as_str() == "super")
                    .count();
                let rest = &segments[supers..];
                let scope = &module.scope;
                if supers <= scope.len() {
                    return self.walk_modules(
                        ModuleRef {
                            file: module.file.clone(),
                            scope: scope[..scope.len() - supers].to_vec(),
                        },
                        rest,
                        depth,
                    );
                }
                // ファイルの外の祖先の module は、crate root からの経路にする。
                let up = supers - scope.len();
                match &module.file.file_module {
                    Some(file_module) if up <= file_module.len() => {
                        let mut absolute = file_module[..file_module.len() - up].to_vec();
                        absolute.extend(rest.iter().cloned());
                        self.walk_absolute(&module.file, absolute, depth)
                    }
                    _ => outside(format!("outside:{}", segments.join("::"))),
                }
            }
            _ => self.walk_modules(module.clone(), segments, depth),
        }
    }

    /// ファイル `file` から、crate root からの module の経路 `absolute` をたどる。そのファイルの中なら
    /// その束縛でたどる。外なら、版の crate があれば crate root からたどり、無ければ `crate:` の識別に
    /// する。
    fn walk_absolute(
        &self,
        file: &File,
        absolute: Vec<String>,
        depth: usize,
    ) -> Option<Vec<Candidate>> {
        let Some(file_module) = &file.file_module else {
            return Some(vec![Candidate::new(
                &[],
                Target::Identity(outside_path("crate:", &absolute)),
            )]);
        };
        if absolute.starts_with(file_module) {
            return self.walk_modules(
                ModuleRef {
                    file: file.clone(),
                    scope: Vec::new(),
                },
                &absolute[file_module.len()..],
                depth,
            );
        }
        let Some(crate_files) = self.crate_files else {
            return Some(vec![Candidate::new(
                &[],
                Target::Identity(outside_path("crate:", &absolute)),
            )]);
        };
        match crate_files.root() {
            Opened::File(root) => self.walk_modules(
                ModuleRef {
                    file: root,
                    scope: Vec::new(),
                },
                &absolute,
                depth,
            ),
            Opened::Unparsed(path) => Some(vec![Candidate::new(
                &[],
                Target::Identity(outside_path(&format!("unparsed:{path}:"), &absolute)),
            )]),
            Opened::Unknown => None,
        }
    }

    /// module `module` から、module の名前の並び `segments` をたどった枝。別名 (`use a as m`) も
    /// 解き、条件付きの別名は条件ごとの枝に分ける。外へ出た枝は、残りの名前を足した識別にする。
    fn walk_modules(
        &self,
        module: ModuleRef,
        segments: &[String],
        depth: usize,
    ) -> Option<Vec<Candidate>> {
        let mut walking = vec![Candidate::new(&[], Target::Module(module))];
        let mut done = Vec::new();
        for (index, segment) in segments.iter().enumerate() {
            let mut next = Vec::new();
            for branch in walking {
                let Target::Module(module) = &branch.target else {
                    continue;
                };
                for candidate in self
                    .resolve_module_segment(module, segment, depth + 1)?
                    .under(&branch.conditions)
                {
                    match candidate.target {
                        Target::Module(_) => next.push(candidate),
                        Target::Identity(identity) => done.push(Candidate {
                            conditions: candidate.conditions,
                            target: Target::Identity(outside_path(
                                &identity,
                                &segments[index + 1..],
                            )),
                            direct: false,
                        }),
                    }
                }
            }
            walking = next;
            if walking.len() + done.len() > MAX_CANDIDATES {
                return None;
            }
        }
        done.extend(walking);
        Some(done)
    }

    /// スコープ `module` で名前 `name` を解く。明示の束縛が無ければ、glob、マクロ、外の glob、
    /// prelude の候補をまとめる。型の名前空間で見つからなければ、値の名前空間も引く (const
    /// ジェネリクスの引数)。
    pub(super) fn resolve_name(
        &self,
        module: &ModuleRef,
        name: &str,
        namespace: Namespace,
        depth: usize,
    ) -> Option<Resolved> {
        if depth > MAX_DEPTH {
            return None;
        }
        let namespaces: &[Namespace] = match namespace {
            Namespace::Type => &[Namespace::Type, Namespace::Value],
            Namespace::Value => &[Namespace::Value],
        };
        for &namespace in namespaces {
            if let Some(found) = self.lookup(module, name, namespace, depth, false, None)? {
                return Some(found);
            }
        }
        // glob・マクロ・prelude の候補は直接の定義でないので、その先は絞らない。
        let _off = self.swap_filter(None);
        let Some(bindings) = module.file.scopes.get(&module.scope) else {
            return Some(Resolved::single(Target::Identity(format!(
                "outside:{}::{name}",
                module.scope.join("::")
            ))));
        };
        let mut certain = Vec::new();
        let mut uncertain = Vec::new();
        for (index, glob) in bindings.globs.iter().enumerate() {
            for branch in self.glob_modules(module, index, glob, depth)? {
                let mut conditions = glob.conditions.clone();
                conditions.extend(branch.conditions);
                match branch.target {
                    Target::Module(target) => {
                        // glob は、取り込むスコープから見える宣言と `use` だけを持ち込む (`use super::*;` は
                        // 親の非公開の宣言も、`pub(crate) use child::*;` は child の非公開の
                        // `use super::X;` を持ち込まない)。
                        let importer = scope_module(module);
                        for &namespace in namespaces {
                            if let Some(found) = self.lookup(
                                &target,
                                name,
                                namespace,
                                depth + 1,
                                false,
                                importer.as_deref(),
                            )? {
                                certain.extend(found.under(&conditions));
                                break;
                            }
                        }
                    }
                    Target::Identity(target) => uncertain.push(Candidate::new(
                        &conditions,
                        Target::Identity(format!("glob:{target}")),
                    )),
                }
            }
        }
        if !bindings.macros.is_empty() {
            uncertain.push(Candidate::new(
                &[],
                Target::Identity(format!(
                    "macros:{}{SEPARATOR}{}",
                    bindings.macros.join(" "),
                    module.file.macro_definitions.join(" ")
                )),
            ));
        }
        // glob で見つかり、外の glob とマクロが無ければ、その先だけにする。
        if certain.is_empty() || !uncertain.is_empty() {
            certain.extend(uncertain);
            certain.push(Candidate::new(
                &[],
                Target::Identity(format!("prelude:{name}")),
            ));
        }
        (certain.len() <= MAX_CANDIDATES).then_some(Resolved(certain))
    }

    /// ファイル `file` のスコープ `scope` の glob の先 (module のスコープか、外のパスの識別)。解け
    /// なければ None。
    pub(crate) fn glob_targets(&self, file: &File, scope: &[String]) -> Option<Vec<Target>> {
        let module = ModuleRef {
            file: file.clone(),
            scope: scope.to_vec(),
        };
        let mut targets = Vec::new();
        for (index, glob) in file.scopes.get(scope)?.globs.iter().enumerate() {
            for branch in self.glob_modules(&module, index, glob, 0)? {
                targets.push(branch.target);
            }
        }
        Some(targets)
    }

    /// スコープ `module` の `index` 番目の glob `glob` の経路の module を解いた枝。経路を解いている
    /// 最中の glob は、循環なので空にする。
    fn glob_modules(
        &self,
        module: &ModuleRef,
        index: usize,
        glob: &UseTarget,
        depth: usize,
    ) -> Option<Vec<Candidate>> {
        let key = (
            module.file.path.clone(),
            module.file.file_module.clone(),
            module.scope.clone(),
            index,
        );
        if self.visiting_globs.borrow().contains(&key) {
            return Some(Vec::new());
        }
        self.visiting_globs.borrow_mut().push(key);
        let found = self.resolve_use_module(module, glob, depth);
        self.visiting_globs.borrow_mut().pop();
        found
    }

    /// スコープ `module` で、パスの module の区切り `name` (`std::path::X` の `std`) を解く。明示の
    /// 束縛に無ければ、crate の中の module への glob が持ち込む同じ名前 (`use crate::names::*;` の
    /// `selected::T`。`use selected::T;` の経路も) を引き、それも無ければ外の crate の名前として
    /// 識別する。経路を解いている最中の glob は引かない (循環する)。外の crate への glob が持ち込む
    /// module は引かない (中身が分からない)。
    fn resolve_module_segment(
        &self,
        module: &ModuleRef,
        name: &str,
        depth: usize,
    ) -> Option<Resolved> {
        if depth > MAX_DEPTH {
            return None;
        }
        for namespace in [Namespace::Type, Namespace::Value] {
            if let Some(found) = self.lookup(module, name, namespace, depth, true, None)? {
                return Some(found);
            }
        }
        if let Some(bindings) = module.file.scopes.get(&module.scope) {
            let mut found = Vec::new();
            for (index, glob) in bindings.globs.iter().enumerate() {
                // 経路が crate の中へ向かうと字面で分かる glob (`crate::`・`self::`・`super::` か、同じ
                // スコープの module・use の名前で始まる) だけを引く。外の crate への glob は中身が分から
                // ないので引かず、その経路を解き直す手間 (glob の並びの順列) もかけない。経路が同じ
                // 名前で始まる glob は、その名前を自分で決められない。
                let internal = match glob.origin {
                    UsePathOrigin::Anchored => true,
                    UsePathOrigin::Bare => glob.module.first().is_some_and(|first| {
                        first != name
                            && (bindings.modules.contains_key(first)
                                || bindings.uses.contains_key(first))
                    }),
                    UsePathOrigin::Global => false,
                };
                if !internal {
                    continue;
                }
                for branch in self.glob_modules(module, index, glob, depth)? {
                    let Target::Module(target) = branch.target else {
                        continue;
                    };
                    let importer = scope_module(module);
                    if let Some(resolved) = self.lookup(
                        &target,
                        name,
                        Namespace::Type,
                        depth + 1,
                        true,
                        importer.as_deref(),
                    )? {
                        let mut conditions = glob.conditions.clone();
                        conditions.extend(branch.conditions);
                        found.extend(resolved.under(&conditions));
                    }
                }
            }
            if !found.is_empty() {
                return (found.len() <= MAX_CANDIDATES).then_some(Resolved(found));
            }
        }
        Some(Resolved::single(Target::Identity(format!("extern:{name}"))))
    }

    /// スコープ `module` の `use` の先の module の部分を解いた枝。
    fn resolve_use_module(
        &self,
        module: &ModuleRef,
        target: &UseTarget,
        depth: usize,
    ) -> Option<Vec<Candidate>> {
        match target.origin {
            // crate root からの経路 (`crate`・`self`・`super` を、use を書いたファイルの module で
            // 解いた後)。
            UsePathOrigin::Anchored => {
                self.walk_absolute(&module.file, target.module.clone(), depth)
            }
            // 2018 以降の識別子で始まる経路は、同じスコープの束縛 (子の module・別名) から読む。
            UsePathOrigin::Bare => self.walk_modules(module.clone(), &target.module, depth),
            UsePathOrigin::Global => Some(vec![Candidate::new(
                &[],
                Target::Identity(outside_path("global:", &target.module)),
            )]),
        }
    }

    /// スコープ `module` の明示の束縛 (宣言・型別名・定数・module・`use`) で名前 `name` を引く。
    /// `module_segment` はパスの module の区切りを引くとき、`importer` は glob で取り込む側の module
    /// (crate root からの経路。そこから見えない宣言と `use` は引かない。None なら全て引く)。無ければ
    /// Some(None)、解けなければ None。
    fn lookup(
        &self,
        module: &ModuleRef,
        name: &str,
        namespace: Namespace,
        depth: usize,
        module_segment: bool,
        importer: Option<&[String]>,
    ) -> Option<Option<Resolved>> {
        let key = (
            module.file.path.clone(),
            module.file.file_module.clone(),
            module.scope.clone(),
            name.to_owned(),
            namespace,
            module_segment,
            importer.map(<[String]>::to_vec),
        );
        if self.active.borrow().contains(&key) {
            return Some(None);
        }
        self.active.borrow_mut().push(key);
        let found = self.lookup_bindings(module, name, namespace, depth, module_segment, importer);
        self.active.borrow_mut().pop();
        found
    }

    /// [`Self::lookup`] の本体。
    fn lookup_bindings(
        &self,
        module: &ModuleRef,
        name: &str,
        namespace: Namespace,
        depth: usize,
        module_segment: bool,
        importer: Option<&[String]>,
    ) -> Option<Option<Resolved>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let visible = |visibility: &Visibility| {
            importer.is_none_or(|importer| visibility.visible_from(importer))
        };
        let file = &module.file;
        let scope = &module.scope;
        let Some(bindings) = file.scopes.get(scope) else {
            return Some(None);
        };
        let place = |name: &str| self.definition_place(file, scope, name);
        // 絞り込みは、このスコープで直接見つけた型別名・定数のうち、項目を置き換え得る属性の無いものの
        // 右辺を描くときだけ使う (`use` の先などは絞らない)。
        let filter = self.availability.borrow().clone();
        let _off = self.swap_filter(None);
        let direct_filter = |definition: &Definition| {
            (!definition.opaque_attributes)
                .then(|| filter.clone())
                .flatten()
        };
        let mut found: Vec<Candidate> = Vec::new();
        if namespace == Namespace::Type {
            let declarations: Vec<&ModuleDeclaration> = bindings
                .modules
                .get(name)
                .into_iter()
                .flatten()
                .filter(|declaration| visible(&declaration.visibility))
                .collect();
            if !declarations.is_empty() {
                let mut path = scope.clone();
                path.push(name.to_owned());
                let inline = declarations.iter().any(|declaration| declaration.inline)
                    && file.scopes.contains_key(&path);
                let outlined = declarations.iter().any(|declaration| !declaration.inline);
                // 中身がこのファイルの module。版の crate で解くときは、条件で分けた別のファイルの
                // 宣言 (`#[cfg(windows)] #[path = "win.rs"] mod selected;`) も候補にする。
                if inline && (self.crate_files.is_none() || !outlined) {
                    return Some(Some(Resolved::single(Target::Module(ModuleRef {
                        file: file.clone(),
                        scope: path,
                    }))));
                }
                if inline {
                    found.push(Candidate::new(
                        &[],
                        Target::Module(ModuleRef {
                            file: file.clone(),
                            scope: path.clone(),
                        }),
                    ));
                }
                match self.crate_files {
                    // 中身が別のファイルの module は、版の crate が無ければ字面で識別する。
                    None => found.push(Candidate::new(
                        &[],
                        Target::Identity(format!("mod-file:{}", path.join("::"))),
                    )),
                    Some(crate_files) => {
                        for declaration in declarations.iter().filter(|d| !d.inline) {
                            let target = match crate_files.child(file, scope, name, declaration) {
                                Opened::File(child) => Target::Module(ModuleRef {
                                    file: child,
                                    scope: Vec::new(),
                                }),
                                Opened::Unparsed(path) => {
                                    Target::Identity(format!("unparsed:{path}:"))
                                }
                                Opened::Unknown => return None,
                            };
                            found.push(Candidate::new(&declaration.conditions, target));
                        }
                    }
                }
            }
            for alias in bindings.aliases.get(name).into_iter().flatten() {
                if !visible(&alias.visibility) {
                    continue;
                }
                let rendered = if self.skip_renders.get() {
                    String::new()
                } else {
                    let _on = self.swap_filter(direct_filter(alias));
                    self.render_definition(file, scope, alias, depth + 1)?
                };
                found.push(Candidate::definition(
                    &alias.conditions,
                    Target::Identity(format!("alias:{}#{name}:{rendered}", place(name))),
                    alias.opaque_attributes,
                ));
            }
        } else {
            for constant in bindings.constants.get(name).into_iter().flatten() {
                if !visible(&constant.visibility) {
                    continue;
                }
                let rendered = if self.skip_renders.get() {
                    String::new()
                } else {
                    let _on = self.swap_filter(direct_filter(constant));
                    self.render_definition(file, scope, constant, depth + 1)?
                };
                found.push(Candidate::definition(
                    &constant.conditions,
                    Target::Identity(format!("const:{}#{name}:{rendered}", place(name))),
                    constant.opaque_attributes,
                ));
            }
        }
        for item in bindings.items.get(name).into_iter().flatten() {
            if visible(&item.visibility) && item_namespaces(item.kind).contains(&namespace) {
                // 名前付きのフィールドの構造体は値を作らない (値の覆いの証明に使わない)。
                found.push(Candidate::definition(
                    &item.conditions,
                    Target::Identity(format!("item:{}#{name}:{}", place(name), item.kind)),
                    item.opaque_attributes || (namespace == Namespace::Value && item.named_fields),
                ));
            }
        }
        for target in bindings.uses.get(name).into_iter().flatten() {
            if !visible(&target.visibility) {
                continue;
            }
            let item = target.item.as_deref()?;
            // 経路が同じ名前で始まる use (`use anyhow::{Result, anyhow};` の `anyhow`) は、その経路の
            // 先頭の module の区切りにならない (先頭は外の crate かほかの束縛で、引き直すと循環する)。
            if module_segment
                && target.origin == UsePathOrigin::Bare
                && target.module.first().is_some_and(|first| first == name)
            {
                continue;
            }
            // `use name;` は同じスコープの子の module か外部 crate (自分の束縛を引き直さない)。
            if target.origin == UsePathOrigin::Bare && target.module.is_empty() {
                let mut path = scope.clone();
                path.push(item.to_owned());
                let resolved = if file.scopes.contains_key(&path) {
                    Target::Module(ModuleRef {
                        file: file.clone(),
                        scope: path,
                    })
                } else if bindings.modules.contains_key(item)
                    || bindings.items.contains_key(item)
                    || bindings.aliases.contains_key(item)
                    || bindings.constants.contains_key(item)
                    || (item != name && bindings.uses.contains_key(item))
                {
                    // 同じスコープの束縛の別名 (`mod types; use types as selected;`・
                    // `use Raw as Out;`)。外の crate とみなす前に、同じスコープで引く。
                    if let Some(resolved) =
                        self.lookup(module, item, namespace, depth + 1, module_segment, None)?
                    {
                        found.extend(resolved.under(&target.conditions));
                    }
                    continue;
                } else {
                    Target::Identity(format!("extern:{item}"))
                };
                found.push(Candidate::new(&target.conditions, resolved));
                continue;
            }
            for branch in self.resolve_use_module(module, target, depth + 1)? {
                let mut conditions = target.conditions.clone();
                conditions.extend(branch.conditions);
                match branch.target {
                    Target::Module(target_module) => {
                        // 同じスコープの同じ名前を指す use (宣言の再公開) は、宣言で表す。
                        if target_module == *module && item == name {
                            continue;
                        }
                        found.extend(
                            self.resolve_name(&target_module, item, namespace, depth + 1)?
                                .under(&conditions),
                        );
                    }
                    Target::Identity(prefix) => found.push(Candidate {
                        conditions,
                        target: Target::Identity(outside_path(&prefix, &[item.to_owned()])),
                        direct: false,
                    }),
                }
            }
        }
        if found.is_empty() {
            return Some(None);
        }
        (found.len() <= MAX_CANDIDATES).then_some(Some(Resolved(found)))
    }

    /// 型別名・定数の宣言 (ファイル `file` のスコープ `scope`) を、右辺の名前を解いた識別に置き換えた
    /// 並びにする。宣言の名前・見える範囲の指定・コメントは入れない。
    fn render_definition(
        &self,
        file: &File,
        scope: &[String],
        definition: &Definition,
        depth: usize,
    ) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }
        let node = file
            .tree
            .root_node()
            .descendant_for_byte_range(definition.range.0, definition.range.1)?;
        let generics = generic_parameter_names(node, &file.source);
        let module = ModuleRef {
            file: file.clone(),
            scope: scope.to_vec(),
        };
        let mut tokens = Vec::new();
        for (field, namespace) in [
            ("type_parameters", Namespace::Type),
            ("type", Namespace::Type),
            ("value", Namespace::Value),
        ] {
            if let Some(child) = node.child_by_field_name(field) {
                tokens.push(format!("<{field}>"));
                self.render(child, &module, namespace, &generics, depth, &mut tokens)?;
            }
        }
        Some(tokens.join(SEPARATOR))
    }

    /// `node` (`module` のファイルのノード) の字面を、名前を解いた識別に置き換えて `tokens` に足す。
    /// 深い入れ子でもスタックを溢れさせないよう、明示のスタックで前順にたどる。
    fn render(
        &self,
        node: tree_sitter::Node<'_>,
        module: &ModuleRef,
        namespace: Namespace,
        generics: &[String],
        depth: usize,
        tokens: &mut Vec<String>,
    ) -> Option<()> {
        // ノードと名前空間と、局所の束縛を作り得る式の中か ([`local_scope`])。
        enum Work<'tree> {
            Node(tree_sitter::Node<'tree>, Namespace, bool),
            Marker(&'static str),
        }
        let source = &module.file.source;
        let mut stack = vec![Work::Node(node, namespace, false)];
        while let Some(work) = stack.pop() {
            let (node, namespace, local) = match work {
                Work::Node(node, namespace, local) => (node, namespace, local),
                Work::Marker(marker) => {
                    tokens.push(marker.to_owned());
                    continue;
                }
            };
            // 型の名前は値の式の中 (`size_of::<T>()`) でも型の名前空間で解き、値の名前は値の
            // 位置でだけ解く。
            let reference_namespace = match node.kind() {
                "type_identifier" | "scoped_type_identifier" => Some(Namespace::Type),
                "identifier" | "scoped_identifier" if namespace == Namespace::Value => {
                    Some(Namespace::Value)
                }
                _ => None,
            };
            if let Some(reference_namespace) = reference_namespace
                && let Some(segments) = path_segments(node, source)
            {
                if let Some(index) = generics.iter().position(|g| Some(g) == segments.first()) {
                    tokens.push(format!("<generic {index}>"));
                    if segments.len() > 1 {
                        tokens.push(segments[1..].join("::"));
                    }
                } else {
                    tokens.push(self.resolve_path(
                        module,
                        &Reference {
                            segments,
                            namespace: reference_namespace,
                            local,
                        },
                        depth + 1,
                    )?);
                }
                continue;
            }
            let children_namespace = match node.kind() {
                "line_comment" | "block_comment" => continue,
                "array_type" => {
                    if let Some(length) = node.child_by_field_name("length") {
                        stack.push(Work::Node(length, Namespace::Value, local));
                        stack.push(Work::Marker("<length>"));
                    }
                    if let Some(element) = node.child_by_field_name("element") {
                        stack.push(Work::Node(element, Namespace::Type, local));
                    }
                    continue;
                }
                "block" if namespace == Namespace::Type => Namespace::Value,
                _ => namespace,
            };
            if node.child_count() == 0 {
                push_tokens(node, source, tokens);
                continue;
            }
            let local = local || local_scope(node.kind());
            let mut cursor = node.walk();
            let children: Vec<_> = node.children(&mut cursor).collect();
            stack.extend(
                children
                    .into_iter()
                    .rev()
                    .map(|child| Work::Node(child, children_namespace, local)),
            );
        }
        Some(())
    }

    /// ファイル `file` のスコープ `scope` の名前 `name` の、条件の無いただ 1 つの定義 (宣言・型別名・
    /// 定数) の識別 ([`Self::lookup`] と同じ字面)。無いか、条件付きか、複数あれば None。
    pub(crate) fn sole_definition(
        &self,
        file: &File,
        scope: &[String],
        name: &str,
    ) -> Option<String> {
        let bindings = file.scopes.get(scope)?;
        let place = self.definition_place(file, scope, name);
        let mut found = Vec::new();
        for alias in bindings.aliases.get(name).into_iter().flatten() {
            let rendered = self.render_definition(file, scope, alias, 1)?;
            found.push((
                &alias.conditions,
                format!("alias:{place}#{name}:{rendered}"),
            ));
        }
        for constant in bindings.constants.get(name).into_iter().flatten() {
            let rendered = self.render_definition(file, scope, constant, 1)?;
            found.push((
                &constant.conditions,
                format!("const:{place}#{name}:{rendered}"),
            ));
        }
        for item in bindings.items.get(name).into_iter().flatten() {
            found.push((
                &item.conditions,
                format!("item:{place}#{name}:{}", item.kind),
            ));
        }
        match found.as_slice() {
            [(conditions, identity)] if conditions.is_empty() => Some(identity.clone()),
            _ => None,
        }
    }

    /// ファイル `file` のスコープ `scope` で、名前 `name` を型と値の名前空間で解いた識別と、その先の
    /// module のスコープ (Issue #300。束縛の変化を、ほかのファイルへ伝えるときに使う)。解けなければ
    /// 識別は None。
    pub(crate) fn resolve_binding(
        &self,
        file: &File,
        scope: &[String],
        name: &str,
    ) -> (Option<String>, Vec<ModuleRef>) {
        let module = ModuleRef {
            file: file.clone(),
            scope: scope.to_vec(),
        };
        let mut identities = Vec::new();
        let mut modules = Vec::new();
        for namespace in [Namespace::Type, Namespace::Value] {
            match self.lookup(&module, name, namespace, 0, false, None) {
                Some(Some(resolved)) => {
                    for candidate in &resolved.0 {
                        if let Target::Module(target) = &candidate.target {
                            modules.push(target.clone());
                        }
                    }
                    identities.push(self.joined_identity(&resolved.0));
                }
                Some(None) => identities.push(String::new()),
                None => return (None, modules),
            }
        }
        (Some(identities.join("||")), modules)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 属性 `attributes` (ソースの先頭に並べたもの) を [`push_tokens`] で並べた条件。
    fn conditions(attributes: &str) -> Vec<String> {
        let source = format!("{attributes}\nstruct S;\n");
        let tree = parser::parse_source(source.as_bytes(), crate::language::LangId::Rust).unwrap();
        let root = tree.root_node();
        let mut tokens = Vec::new();
        let mut cursor = root.walk();
        for child in root.named_children(&mut cursor) {
            if matches!(child.kind(), "attribute_item" | "inner_attribute_item") {
                push_tokens(child, source.as_bytes(), &mut tokens);
            }
        }
        tokens
    }

    #[test]
    fn condition_tokens_are_read_back_as_cfg_keys() {
        let keys = condition_keys(&conditions(
            "#![cfg(unix)]\n#[cfg(all(feature = \"x\", not(r#test)))]\n#[cfg_attr(windows, derive(Debug))]\n#[cfg_attr(target_os = \"linux\", cfg(a::b), allow(dead_code))]",
        ))
        .expect("keys");
        let shown: Vec<String> = keys.iter().map(|key| key.join(" ")).collect();
        assert_eq!(
            shown,
            [
                "cfg ( unix )",
                "cfg ( all ( feature = \"x\" , not ( r#test ) ) )",
                "cfg_attr ( target_os = \"linux\" , cfg ( a :: b ) )",
            ],
            "{keys:?}"
        );
        // `#` で始まらない並びは読めない
        assert_eq!(condition_keys(&["cfg".to_owned()]), None);
        assert_eq!(
            token_text("identifier:feature"),
            (Some("identifier"), "feature")
        );
        assert_eq!(token_text("::"), (None, "::"));
        assert_eq!(
            token_text("string_literal:\"a:b\""),
            (Some("string_literal"), "\"a:b\"")
        );
    }

    #[test]
    fn availability_filter_keeps_compatible_and_unreadable_conditions() {
        let key = |text: &str| -> Vec<String> { text.split(' ').map(str::to_owned).collect() };
        let narrow = AvailabilityAttr {
            key: key("cfg ( not ( feature = \"wide\" ) )"),
            shown: "cfg(not(feature = \"wide\"))".to_owned(),
        };
        let filter = AvailabilityFilter::new(Some(std::slice::from_ref(&narrow))).expect("filter");
        let compatible =
            |attributes: &[String]| filter.compatible(condition_keys(attributes).as_ref());
        assert!(!compatible(&conditions("#[cfg(feature = \"wide\")]")));
        assert!(compatible(&conditions("#[cfg(not(feature = \"wide\"))]")));
        assert!(compatible(&conditions("#[cfg(unix)]")));
        assert!(filter.compatible(Some(&Vec::new())));
        // 読めない条件は残す
        assert!(compatible(&["#".to_owned(), "[".to_owned()]));
        // 残す枝が宣言の有る設定を覆うか
        assert!(filter.covered(&[vec![key("cfg ( not ( feature = \"wide\" ) )")]]));
        assert!(!filter.covered(&[vec![key("cfg ( unix )")]]));
        assert!(filter.covered(&[vec![]]));
        // 原子を独立に扱う (`unix` と `windows` は同時に真になり得る)
        let unix = AvailabilityAttr {
            key: key("cfg ( unix )"),
            shown: "cfg(unix)".to_owned(),
        };
        let filter = AvailabilityFilter::new(Some(std::slice::from_ref(&unix))).expect("filter");
        assert!(filter.compatible(condition_keys(&conditions("#[cfg(windows)]")).as_ref()));
        // 条件の無い宣言と、読めない条件の宣言は絞らない
        assert!(AvailabilityFilter::new(Some(&[])).is_none());
        assert!(AvailabilityFilter::new(None).is_none());
    }
}
