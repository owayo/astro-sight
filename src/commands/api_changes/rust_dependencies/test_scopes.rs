//! テストの module (`#[cfg(test)]` の付いた宣言の module か、その中) の判定。公開の契約はテストでない
//! build のものなので、旧新の両方でテストの module のスコープの変化は、シグネチャの依存の比較
//! (`dependents.rs`) にも、不透明な戻り値の比較 (`rust_opaque_return.rs`。Issue #382) にも入れない。

use super::crates::{CrateVersion, Located};
use std::collections::HashMap;

/// 旧新の両方でテストの module (`#[cfg(test)]` の付いた宣言の module か、その中) のスコープ。
pub(crate) struct TestScopes<'v, 'a> {
    old: &'v CrateVersion<'a>,
    new: &'v CrateVersion<'a>,
    known: std::cell::RefCell<HashMap<Vec<String>, bool>>,
}

impl<'v, 'a> TestScopes<'v, 'a> {
    /// 旧い版 `old` と新しい版 `new` の lib crate のテストの module の判定。
    pub(crate) fn new(old: &'v CrateVersion<'a>, new: &'v CrateVersion<'a>) -> Self {
        Self {
            old,
            new,
            known: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// スコープ `scope` (crate root からの経路) が、旧新の両方でテストの module の中か。
    pub(crate) fn contains(&self, scope: &[String]) -> bool {
        if let Some(&known) = self.known.borrow().get(scope) {
            return known;
        }
        // 旧新のどちらかでテストの module で、もう一方でもテストの module か、無い。
        let (old, new) = (test_module(self.old, scope), test_module(self.new, scope));
        let found = old != TestModule::Other
            && new != TestModule::Other
            && (old == TestModule::Test || new == TestModule::Test);
        self.known.borrow_mut().insert(scope.to_vec(), found);
        found
    }
}

/// その版での module の様子。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestModule {
    /// `#[cfg(test)]` の付いた宣言の module か、その中。
    Test,
    /// その経路の module が無い。
    Absent,
    /// それ以外 (決められないときも)。
    Other,
}

/// module `module` (crate root からの経路) の、その版での様子。祖先のどれかが `#[cfg(test)]` の付いた
/// 宣言の module なら Test。
fn test_module(version: &CrateVersion<'_>, module: &[String]) -> TestModule {
    for depth in 1..=module.len() {
        let (parent, name) = (&module[..depth - 1], &module[depth - 1]);
        let Located::Found(file, scope) = version.locate_scope(parent) else {
            return TestModule::Other;
        };
        let declarations = file.module_declarations(&scope, name);
        if declarations.is_empty() {
            return TestModule::Absent;
        }
        if declarations
            .iter()
            .all(|declaration| declaration.only_in_tests())
        {
            return TestModule::Test;
        }
    }
    TestModule::Other
}
