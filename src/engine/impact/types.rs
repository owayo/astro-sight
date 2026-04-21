//! impact streaming Pass 共通の型定義と軽量な interning pool。
//!
//! ここに集約する理由:
//!   - `TypedCallerMap` / `CallerMap` / `SymEntries` / `StringPool` は
//!     Pass 2 (収集) と Pass 3 (最終組み立て) の両方から参照され、親 mod.rs
//!     以外の submodule から `pub(super)` で使う。
//!   - 型と helper を 1 ファイルに寄せることで、メモリレイアウトの変更（例: `u32` ID、
//!     `SmallVec` inline 数、`ahash` への切り替え）が 1 箇所に収まり、他 Pass への
//!     波及を抑えられる。
use std::collections::HashMap;

/// 最終出力の `(path, line) -> (caller_name, Vec<sym_name>)` マップ。
/// すべて `String` で保持し、`FileImpact` 生成直前の per-fc 1 件分だけ materialize する。
pub(super) type CallerMap = HashMap<(String, usize), (String, Vec<String>)>;

/// 1 caller における (sym_ix, sym_name_id) のリスト。
/// 実測的に 1-2 件が大半のため、`SmallVec` で inline 保持して heap allocation を避ける。
pub(super) type SymEntries = smallvec::SmallVec<[(u32, u32); 2]>;

/// Pass 2 内部用。caller_map の key と value を interned ID で保持する中間表現。
/// `hashbrown + ahash` を採用して `u32` key のバケット overhead を削減する。
///   key:   (path_id, line)
///   value: (caller_name_id, SymEntries)
pub(super) type TypedCallerMap =
    hashbrown::HashMap<(u32, usize), (u32, SymEntries), ahash::RandomState>;

/// `TypedCallerMap` の空インスタンスを生成する（`ahash` state を明示初期化）。
pub(super) fn new_typed_caller_map() -> TypedCallerMap {
    hashbrown::HashMap::with_hasher(ahash::RandomState::new())
}

/// 文字列の重複を取り除くための小さな interning pool。
///
/// caller_map のキー (path)・caller_name・sym_name は同じ文字列が大量に繰り返されるため、
/// `u32` の ID に置き換えて保持することで hashmap の key/value サイズと heap allocation
/// 数を大幅に削減する。workers=1 の streaming Pass で使うことを想定し、内部状態は
/// 単一スレッドから更新される前提（マルチ worker 利用時は `Mutex` で包んで使う）。
pub(crate) struct StringPool {
    strings: Vec<String>,
    /// `hashbrown` + `ahash` で integer-friendly なハッシュに切替。SipHash より高速で
    /// allocation/バケット overhead も小さい。
    index: hashbrown::HashMap<String, u32, ahash::RandomState>,
}

impl Default for StringPool {
    fn default() -> Self {
        Self {
            strings: Vec::new(),
            index: hashbrown::HashMap::with_hasher(ahash::RandomState::new()),
        }
    }
}

impl StringPool {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// 文字列を登録し ID を返す。既存の文字列は再利用される。
    pub(super) fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.index.get(s) {
            return id;
        }
        let id = self.strings.len() as u32;
        let owned = s.to_string();
        self.strings.push(owned.clone());
        self.index.insert(owned, id);
        id
    }

    /// ID に対応する文字列を返す。
    pub(super) fn get(&self, id: u32) -> &str {
        &self.strings[id as usize]
    }
}
