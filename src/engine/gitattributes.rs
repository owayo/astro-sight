//! `.gitattributes` の `linguist-generated` を Git と同じ規則で解決する。
//!
//! 生成物を外す経路 (refs / symbols の走査、dead-code の候補、API 差分の対象、TS の literal
//! union の在庫、cochange) は、この解決と [`crate::engine::generated::judge`] の判定を共通に使う。
//!
//! 解決は gix (gitoxide) に任せ、git を起動しない (refs の呼び出しごとに数十 ms かかるため)。
//! Git と同じく、作業ツリーの各ディレクトリの `.gitattributes` (深い方が優先、同じファイルの中は
//! 後勝ち、属性ごとの上書き、マクロ)、`$GIT_COMMON_DIR/info/attributes` (最優先)、利用者の
//! `core.attributesFile` (無ければ XDG の `git/attributes`)、作業ツリーに無い `.gitattributes` の
//! index からの補完を読む。システムの属性ファイル (`$(prefix)/etc/gitattributes`) と、システム・
//! Git のインストール先の設定 (そこで設定した `core.attributesFile` を含む) は読まない (Windows では
//! 場所を知るのに git を起動し、1 回の解決が約 9 ms から約 100 ms になるため)。
//!
//! git の作業ツリーが見つからないディレクトリは、走査の根を境界にしてその下の `.gitattributes`
//! だけを読む (親のディレクトリの属性もグローバルな属性も読まない)。staged の解析が使う index の
//! スナップショット ([`register_snapshot`]) は、スナップショットの根の下の `.gitattributes`
//! (index の内容) に加えて、元のリポジトリの info と利用者の属性ファイルを読む。
//!
//! 読めない属性ファイルは、gix が Windows で「無い」と同じに扱うので、ディレクトリごとに自前で
//! 確かめる。読めない、index を読めない、リポジトリを開けないときは [`AttributeLookupError`] を
//! 返し、呼び出し側はそのファイルを生成物として外さずに申告する。
//!
//! gix は `info/attributes` を根の `.gitattributes` の直後に積むので、子ディレクトリの
//! `.gitattributes` の方が強くなる (Git では info が最優先)。info はディレクトリの積み重ねから外し、
//! 別に照合して最後に上書きする。info の中で使うマクロは、info の中か組み込みのもの (`binary`) に
//! 限る (根の `.gitattributes` で定義したマクロは info の照合では展開しない)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use gix::bstr::{BStr, BString, ByteSlice};

/// 判定に使う属性の名前。
const LINGUIST_GENERATED: &str = "linguist-generated";

/// `linguist-generated` の状態 (Git の 4 状態)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinguistGenerated {
    /// `linguist-generated`
    Set,
    /// `-linguist-generated`
    Unset,
    /// 指定が無い、または `!linguist-generated` で未指定に戻した。
    Unspecified,
    /// `linguist-generated=<値>`
    Value(String),
}

impl LinguistGenerated {
    /// GitHub Linguist の解釈。`Some(true)` は生成物、`Some(false)` は生成物でない (名前と
    /// ヘッダーの判定より優先する)、`None` は名前とヘッダーで判定する。
    ///
    /// Linguist は値が文字列 `false` でなければ生成物とする (`0` や `unset` という値、空の値、
    /// 大文字の `FALSE` も生成物)。
    pub fn generated(&self) -> Option<bool> {
        match self {
            Self::Set => Some(true),
            Self::Unset => Some(false),
            Self::Value(value) => Some(value != "false"),
            Self::Unspecified => None,
        }
    }
}

/// 属性を解決できなかった原因。同じ原因のファイルはまとめて申告する。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AttributeLookupError {
    /// 影響する範囲 (解決器を作ったディレクトリからの相対、`/` 区切り)。空ならディレクトリ全体。
    pub scope: String,
    /// 原因の説明 (英語。絶対パスを含めない)。
    pub cause: String,
}

impl AttributeLookupError {
    fn whole(cause: impl Into<String>) -> Self {
        Self {
            scope: String::new(),
            cause: cause.into(),
        }
    }
}

/// 1 回の走査ぶんの属性の解決器。照会はスレッドごとに [`Self::lookup`] で作る。
pub struct AttributeResolver {
    /// 照会するパスの基準 (呼び出し側が渡したディレクトリ。与えられた形のまま)。
    base: PathBuf,
    /// `base` の、属性の根からの相対 (`/` 区切り。根なら空)。
    prefix: BString,
    source: Source,
    /// 返された照会の状態。rayon の `map_init` は照会をワーカーの数より多く作りうるので、
    /// 作るたびに属性ファイルと index を読み直さないよう使い回す (同時に使う数までしか作らない)。
    pool: Mutex<Vec<PooledLookup>>,
}

/// 照会の状態と、確かめたディレクトリ (根からの相対) → 読めない属性ファイルによる失敗。
struct PooledLookup {
    state: Result<LookupState, AttributeLookupError>,
    probed: HashMap<BString, Option<AttributeLookupError>>,
}

enum Source {
    /// git の作業ツリー。作業ツリーに無い `.gitattributes` を index から補う。
    Repository {
        repo: Box<gix::ThreadSafeRepository>,
        index: gix::worktree::Index,
        attributes: DirectoryAttributes,
    },
    /// git の管理外のディレクトリ、または index のスナップショット。
    Directory(DirectoryAttributes),
    /// 解決できない。
    Unavailable(AttributeLookupError),
}

/// 属性を引く根と、根の外から読む属性ファイル。
#[derive(Clone)]
struct DirectoryAttributes {
    root: PathBuf,
    /// 最も弱い属性ファイル (利用者の `core.attributesFile` か XDG の `git/attributes`)。
    globals: Vec<PathBuf>,
    /// 最も強い `$GIT_COMMON_DIR/info/attributes`。
    info: Option<PathBuf>,
    ignore_case: bool,
}

impl DirectoryAttributes {
    /// git の管理外のディレクトリ。`root` の下の `.gitattributes` だけを読む。
    fn standalone(root: PathBuf) -> Self {
        Self {
            root,
            globals: Vec::new(),
            info: None,
            ignore_case: false,
        }
    }
}

impl AttributeResolver {
    /// `dir` の下のファイルの属性を引く解決器を作る。失敗しても作り、照会のたびに失敗を返す。
    pub fn open(dir: &Path) -> Self {
        let (source, prefix) = match open_source(dir) {
            Ok(opened) => opened,
            Err(error) => (Source::Unavailable(error), BString::default()),
        };
        Self {
            base: dir.to_path_buf(),
            prefix,
            source,
            pool: Mutex::new(Vec::new()),
        }
    }

    /// 照会する (スレッドごとに 1 つ作り、同じスレッドの照会で使い回す)。捨てた照会の状態は
    /// 解決器に戻り、次の照会で使い回す。
    pub fn lookup(&self) -> AttributeLookup<'_> {
        let pooled = self
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
            .unwrap_or_else(|| PooledLookup {
                state: self.new_state(),
                probed: HashMap::new(),
            });
        AttributeLookup {
            resolver: self,
            pooled: Some(pooled),
        }
    }

    fn new_state(&self) -> Result<LookupState, AttributeLookupError> {
        let unreadable = |error: std::io::Error| {
            AttributeLookupError::whole(format!(
                "the attribute files could not be read: {}",
                error.kind()
            ))
        };
        match &self.source {
            Source::Repository {
                repo,
                index,
                attributes,
            } => {
                let repo = repo.to_thread_local();
                LookupState::new(attributes, Some(index), Some(repo)).map_err(unreadable)
            }
            Source::Directory(directory) => {
                LookupState::new(directory, None, None).map_err(unreadable)
            }
            Source::Unavailable(error) => Err(error.clone()),
        }
    }

    fn root(&self) -> Option<&Path> {
        match &self.source {
            Source::Repository { attributes, .. } | Source::Directory(attributes) => {
                Some(&attributes.root)
            }
            Source::Unavailable(_) => None,
        }
    }
}

/// `dir` の属性の根と、根からの `dir` の相対を決める。
fn open_source(dir: &Path) -> Result<(Source, BString), AttributeLookupError> {
    let canonical_dir = std::fs::canonicalize(dir).map_err(|error| {
        AttributeLookupError::whole(format!(
            "the directory could not be resolved: {}",
            error.kind()
        ))
    })?;
    if let Some((directory, prefix)) = registered_snapshot(&canonical_dir) {
        return Ok((Source::Directory(directory?), prefix));
    }
    let standalone = || {
        (
            Source::Directory(DirectoryAttributes::standalone(canonical_dir.clone())),
            BString::default(),
        )
    };
    let repo = match discover(&canonical_dir) {
        Ok(repo) => repo,
        Err(Discovery::NotFound) => return Ok(standalone()),
        Err(Discovery::Failed(error)) => return Err(error),
    };
    let local = repo.to_thread_local();
    // bare リポジトリや `.git` の中など、作業ツリーの外を走査するときは管理外と同じに扱う。
    let Some((root, prefix)) = local
        .workdir()
        .and_then(|workdir| std::fs::canonicalize(workdir).ok())
        .and_then(|root| relative_slash_path(&root, &canonical_dir).map(|prefix| (root, prefix)))
    else {
        return Ok(standalone());
    };
    let attributes = repository_attributes(&local, root)?;
    let index = local.index_or_empty().map_err(|error| {
        AttributeLookupError::whole(format!("the index could not be read: {error}"))
    })?;
    Ok((
        Source::Repository {
            repo: Box::new(repo),
            index,
            attributes,
        },
        prefix,
    ))
}

/// リポジトリの利用者の属性ファイルと info を、`root` を根にして引く材料にする。読めない
/// ファイルがあれば、gix が「無い」と同じに扱うので全体の失敗にする。
fn repository_attributes(
    repo: &gix::Repository,
    root: PathBuf,
) -> Result<DirectoryAttributes, AttributeLookupError> {
    let globals = repo.attribute_global_paths().map_err(|error| {
        AttributeLookupError::whole(format!(
            "the global attribute file could not be located: {error}"
        ))
    })?;
    let info = repo.common_dir().join("info").join("attributes");
    for file in globals.iter().chain([&info]) {
        if let Some(kind) = unreadable(file) {
            return Err(AttributeLookupError::whole(format!(
                "a global attribute file could not be read: {kind}"
            )));
        }
    }
    let ignore_case = repo
        .config_snapshot()
        .boolean("core.ignoreCase")
        .unwrap_or(false);
    Ok(DirectoryAttributes {
        root,
        globals,
        info: Some(info),
        ignore_case,
    })
}

enum Discovery {
    NotFound,
    Failed(AttributeLookupError),
}

/// システムと Git のインストール先の設定を読まない権限で、上へたどって開く。
fn discover(dir: &Path) -> Result<gix::ThreadSafeRepository, Discovery> {
    let mut permissions = gix::open::Permissions::default();
    permissions.config.git_binary = false;
    permissions.config.system = false;
    permissions.attributes.git_binary = false;
    permissions.attributes.system = false;
    let options = gix::open::Options::default().permissions(permissions);
    gix::ThreadSafeRepository::discover_opts(
        dir,
        gix::discover::upwards::Options::default(),
        gix::sec::trust::Mapping {
            full: options.clone(),
            reduced: options,
        },
    )
    .map_err(|error| {
        use gix::discover::upwards::Error;
        match error.downcast_any_ref::<Error>() {
            Some(
                Error::NoGitRepository { .. }
                | Error::NoGitRepositoryWithinCeiling { .. }
                | Error::NoGitRepositoryWithinFs { .. },
            ) => Discovery::NotFound,
            _ => Discovery::Failed(AttributeLookupError::whole(format!(
                "the git repository could not be opened: {error}"
            ))),
        }
    })
}

/// 属性ファイルがあるのに読めないときだけ、その理由を返す (無い・ディレクトリなら `None`)。
fn unreadable(path: &Path) -> Option<std::io::ErrorKind> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => None,
        Ok(_) => std::fs::File::open(path).err().map(|error| error.kind()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(error.kind()),
    }
}

/// `base` から `path` への相対 (`/` 区切り)。`path` が `base` の下に無ければ `None`。
fn relative_slash_path(base: &Path, path: &Path) -> Option<BString> {
    slash_path(path.strip_prefix(base).ok()?)
}

/// 相対パスを `/` 区切りにする。`..` などを含むなら `None`。
fn slash_path(relative: &Path) -> Option<BString> {
    let mut out = BString::default();
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return None;
        };
        if !out.is_empty() {
            out.push(b'/');
        }
        out.extend_from_slice(gix::path::os_str_into_bstr(part).ok()?);
    }
    Some(out)
}

struct LookupState {
    /// 利用者の属性ファイルと、根から下の各ディレクトリの `.gitattributes`。
    stack: gix::worktree::Stack,
    outcome: gix::attrs::search::Outcome,
    /// 最後に上書きする info (`stack` には積まない)。
    info: Option<InfoAttributes>,
    case: gix::glob::pattern::Case,
    /// id mapping の blob を読むためのリポジトリ (管理外とスナップショットでは使わない)。
    repo: Option<gix::Repository>,
}

struct InfoAttributes {
    search: gix::attrs::Search,
    collection: gix::attrs::search::MetadataCollection,
    outcome: gix::attrs::search::Outcome,
}

impl LookupState {
    fn new(
        attributes: &DirectoryAttributes,
        index: Option<&gix::index::State>,
        repo: Option<gix::Repository>,
    ) -> std::io::Result<Self> {
        let case = if attributes.ignore_case {
            gix::glob::pattern::Case::Fold
        } else {
            gix::glob::pattern::Case::Sensitive
        };
        let mut buf = Vec::new();
        let mut collection = gix::attrs::search::MetadataCollection::default();
        let globals = gix::attrs::Search::new_globals(
            attributes.globals.iter().cloned(),
            &mut buf,
            &mut collection,
        )?;
        let state = gix::worktree::stack::State::AttributesStack(
            gix::worktree::stack::state::Attributes::new(
                globals,
                None,
                gix::worktree::stack::state::attributes::Source::WorktreeThenIdMapping,
                collection,
            ),
        );
        let id_mappings = index
            .map(|index| state.id_mappings_from_index(index, index.path_backing(), case))
            .unwrap_or_default();
        let stack =
            gix::worktree::Stack::new(attributes.root.clone(), state, case, buf, id_mappings);
        let outcome = stack.selected_attribute_matches([LINGUIST_GENERATED]);
        let info = match &attributes.info {
            Some(info) => {
                let mut buf = Vec::new();
                let mut collection = gix::attrs::search::MetadataCollection::default();
                let search =
                    gix::attrs::Search::new_globals([info.clone()], &mut buf, &mut collection)?;
                let mut outcome = gix::attrs::search::Outcome::default();
                outcome.initialize_with_selection(&collection, [LINGUIST_GENERATED]);
                Some(InfoAttributes {
                    search,
                    collection,
                    outcome,
                })
            }
            None => None,
        };
        Ok(Self {
            stack,
            outcome,
            info,
            case,
            repo,
        })
    }
}

/// 照合の結果の `linguist-generated`。`None` はどのパターンにも当たらなかった。
fn matched_state(outcome: &gix::attrs::search::Outcome) -> Option<LinguistGenerated> {
    let matched = outcome.iter_selected().next()?;
    // 当たらなかったときは空のパターンの仮の結果が返る。
    if matched.pattern.text.is_empty() {
        return None;
    }
    Some(match matched.assignment.state {
        gix::attrs::StateRef::Set => LinguistGenerated::Set,
        gix::attrs::StateRef::Unset => LinguistGenerated::Unset,
        gix::attrs::StateRef::Unspecified => LinguistGenerated::Unspecified,
        gix::attrs::StateRef::Value(value) => {
            LinguistGenerated::Value(value.as_bstr().to_str_lossy().into_owned())
        }
    })
}

/// スレッドごとの照会。
pub struct AttributeLookup<'r> {
    resolver: &'r AttributeResolver,
    /// 捨てるときに解決器へ戻す (それまでは常に `Some`)。
    pooled: Option<PooledLookup>,
}

impl Drop for AttributeLookup<'_> {
    fn drop(&mut self) {
        if let Some(pooled) = self.pooled.take() {
            self.resolver
                .pool
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(pooled);
        }
    }
}

impl AttributeLookup<'_> {
    /// `path` (解決器を作ったディレクトリの下のファイル) の `linguist-generated`。ディレクトリの外の
    /// パスは属性が無いもの (`Unspecified`) とする。
    pub fn linguist_generated(
        &mut self,
        path: &Path,
    ) -> Result<LinguistGenerated, AttributeLookupError> {
        let resolver = self.resolver;
        let pooled = self.pooled.as_mut().expect("照会の状態は捨てるまで持つ");
        let state = pooled.state.as_mut().map_err(|error| error.clone())?;
        let Some(relative) = relative_slash_path(&resolver.base, path) else {
            return Ok(LinguistGenerated::Unspecified);
        };
        if relative.is_empty() {
            return Ok(LinguistGenerated::Unspecified);
        }
        let mut full = resolver.prefix.clone();
        if !full.is_empty() {
            full.push(b'/');
        }
        full.extend_from_slice(&relative);
        if let Some(root) = resolver.root() {
            probe_directories(&mut pooled.probed, root, &resolver.prefix, full.as_bstr())?;
        }
        let objects: &dyn gix::objs::Find = match &state.repo {
            Some(repo) => &repo.objects,
            None => &gix::objs::find::Never,
        };
        let platform = state
            .stack
            .at_entry(full.as_bstr(), Some(gix::index::entry::Mode::FILE), objects)
            .map_err(|error| AttributeLookupError {
                scope: scope_text(&resolver.prefix, parent(full.as_bstr())),
                cause: format!("the attribute files could not be read: {}", error.kind()),
            })?;
        let case = state.case;
        platform.matching_attributes(&mut state.outcome);
        let from_stack = matched_state(&state.outcome);
        // info は Git では最も強い (gix のスタックの積み方では子ディレクトリに負ける)。
        if let Some(info) = &mut state.info {
            info.outcome.initialize(&info.collection);
            info.search.pattern_matching_relative_path(
                full.as_bstr(),
                case,
                Some(false),
                &mut info.outcome,
            );
            if let Some(state) = matched_state(&info.outcome) {
                return Ok(state);
            }
        }
        Ok(from_stack.unwrap_or(LinguistGenerated::Unspecified))
    }
}

/// `full` (根からの相対) の親ディレクトリを根から順に確かめ、読めない `.gitattributes` があれば
/// 失敗にする。確かめた結果はディレクトリごとに覚える。
fn probe_directories(
    probed: &mut HashMap<BString, Option<AttributeLookupError>>,
    root: &Path,
    prefix: &BString,
    full: &BStr,
) -> Result<(), AttributeLookupError> {
    let mut directory = BString::default();
    let mut parts = full.split_str("/").peekable();
    while let Some(part) = parts.next() {
        let result = probed.entry(directory.clone()).or_insert_with(|| {
            let kind = match gix::path::from_bstr(directory.as_bstr()) {
                Ok(relative) => unreadable(&root.join(relative).join(".gitattributes")),
                // パスから作った相対パスなので戻せないことは無いが、戻せなければ確かめられない。
                Err(_) => Some(std::io::ErrorKind::InvalidData),
            };
            kind.map(|kind| AttributeLookupError {
                scope: scope_text(prefix, directory.as_bstr()),
                cause: format!("an attribute file could not be read: {kind}"),
            })
        });
        if let Some(error) = result {
            return Err(error.clone());
        }
        // 最後の部分はファイル名なので、そのディレクトリは確かめない。
        if parts.peek().is_none() {
            break;
        }
        if !directory.is_empty() {
            directory.push(b'/');
        }
        directory.extend_from_slice(part);
    }
    Ok(())
}

fn parent(full: &BStr) -> &BStr {
    match full.rfind_byte(b'/') {
        Some(at) => full[..at].as_bstr(),
        None => b"".as_bstr(),
    }
}

/// 根からの相対のディレクトリを、解決器を作ったディレクトリ (根から `prefix`) からの相対にする。
/// それより上の属性ファイルはディレクトリ全体に効くので、空にする。
fn scope_text(prefix: &BString, directory: &BStr) -> String {
    if prefix.is_empty() {
        return directory.to_str_lossy().into_owned();
    }
    if directory.len() > prefix.len()
        && directory.starts_with(prefix.as_slice())
        && directory[prefix.len()] == b'/'
    {
        return directory[prefix.len() + 1..].to_str_lossy().into_owned();
    }
    String::new()
}

/// 登録した index のスナップショットの、元のリポジトリの属性 (読めなかったならその失敗)。
type SnapshotAttributes = Result<DirectoryAttributes, AttributeLookupError>;

/// 登録した index のスナップショット (根 → 元のリポジトリの属性)。
static SNAPSHOTS: Mutex<Vec<(PathBuf, SnapshotAttributes)>> = Mutex::new(Vec::new());

/// [`register_snapshot`] の登録。捨てると登録を外す。
pub struct SnapshotRegistration {
    root: PathBuf,
}

impl Drop for SnapshotRegistration {
    fn drop(&mut self) {
        let mut snapshots = SNAPSHOTS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(at) = snapshots.iter().position(|(root, _)| *root == self.root) {
            drop(snapshots.remove(at));
        }
    }
}

/// `root` (index の tree を書き出した一時ディレクトリ) を、`original` のリポジトリの index の
/// スナップショットとして登録する。登録の間、`root` の下の照会は `root` の下の `.gitattributes`
/// (index の内容) と、元のリポジトリの info と利用者の属性ファイルを読む。
pub fn register_snapshot(root: &Path, original: &Path) -> SnapshotRegistration {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let attributes = snapshot_attributes(&root, original);
    SNAPSHOTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((root.clone(), attributes));
    SnapshotRegistration { root }
}

fn snapshot_attributes(root: &Path, original: &Path) -> SnapshotAttributes {
    let repo = match discover(original) {
        Ok(repo) => repo.to_thread_local(),
        Err(Discovery::NotFound) => {
            return Err(AttributeLookupError::whole(
                "the repository of the staged snapshot could not be found",
            ));
        }
        Err(Discovery::Failed(error)) => return Err(error),
    };
    repository_attributes(&repo, root.to_path_buf())
}

/// `dir` を含む登録済みのスナップショット。
fn registered_snapshot(dir: &Path) -> Option<(SnapshotAttributes, BString)> {
    let snapshots = SNAPSHOTS.lock().unwrap_or_else(PoisonError::into_inner);
    let (root, attributes) = snapshots
        .iter()
        .filter(|(root, _)| dir.starts_with(root))
        .max_by_key(|(root, _)| root.components().count())?;
    let prefix = relative_slash_path(root, dir)?;
    Some((attributes.clone(), prefix))
}

#[cfg(test)]
mod tests;
