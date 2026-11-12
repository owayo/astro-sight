//! シグネチャの変わった宣言の判定: ファイルの組の文脈・宣言ごとの材料・順序付きの規則。

use super::*;

/// シグネチャの変わったシンボルを判定する、旧新のファイルの組ごとの文脈。
///
/// 変更ファイル (rename を含む) と、型の移動と同じ差分でシグネチャの変わったメンバー
/// ([`process_moved_member_changes`])、移動と同じ差分でシグネチャの変わった自由関数・定数
/// ([`process_moved_symbol_changes`])、再公開の先の宣言との比較 ([`process_reexported_removals`]) が
/// 同じ規則で判定するために共有する。ファイル内の対応付け (同名の複数定義・引数パターンだけの変更) の
/// 検査は呼び出し側が済ませる。ここにはファイルの組で共有するものだけを置き、宣言ごとの判定材料は
/// [`SignatureCandidate`] で渡す。
pub(super) struct ChangedSignatureFiles<'a> {
    pub(super) type_dependencies: Option<&'a super::ts_signature::DependencyChanges>,
    pub(super) old_path: &'a str,
    pub(super) new_path: &'a str,
    /// 移動と同じ差分でシグネチャの変わった宣言なら移動元 (`old_path`)。
    pub(super) moved_from: Option<&'a str>,
    pub(super) in_file_callees: &'a std::collections::HashSet<String>,
    /// Rust bin-only crate 判定 (api.mod 抑制用)。lib → bin / bin → lib どちらかが bin-only なら
    /// 外部 API 面の変更ではないとみなす。
    pub(super) skip_for_binary_crate: bool,
    /// 値バインディングの value-only 変更を const_value_changes へ振り分けるための言語判定。
    pub(super) lang_id: Option<crate::language::LangId>,
    /// Rust の同一ファイル内 public type alias は、最初の候補で old/new source を 1 回だけ
    /// 読み込んで以降の候補へ再利用する。候補ごとの git show + parse を避ける。
    pub(super) rust_type_aliases:
        Option<Option<crate::engine::rust_signature::RustTypeAliasEquivalence>>,
    /// 関数に `const` を足しただけと Phase 0 で証明した qualname。型の移動と同じ差分の
    /// メンバーは旧新で別ファイルを比べるので持たない。
    pub(super) rust_const_additions: Option<&'a std::collections::HashSet<String>>,
    /// 自由関数の移動で、const 追加と別ファイルの型所有元まで証明した名前。
    pub(super) rust_moved_const_additions: std::collections::HashSet<String>,
}

/// シグネチャの変わった宣言 1 件の判定材料。
///
/// Rust の宣言を宣言の場所で組にした変更なら、その場所 (Issue #282) と旧い版の宣言の文脈
/// (Issue #315。inherent impl の項目の公開 API 面を対象の型で決める) を持つ。再公開の先の宣言との
/// 比較は、場所だけを持ち旧い版の文脈を持たない。
#[derive(Clone, Copy)]
pub(super) struct SignatureCandidate<'s> {
    pub(super) name: &'s str,
    pub(super) kind: &'s str,
    pub(super) old_sig: &'s str,
    pub(super) new_sig: &'s str,
    pub(super) rust_site: Option<RustDeclarationSite>,
    pub(super) rust_old_context: Option<&'s RustDeclarationContext>,
}

impl<'s> SignatureCandidate<'s> {
    pub(super) fn new(name: &'s str, kind: &'s str, old_sig: &'s str, new_sig: &'s str) -> Self {
        Self {
            name,
            kind,
            old_sig,
            new_sig,
            rust_site: None,
            rust_old_context: None,
        }
    }

    /// 名前が、ファイルの複数の宣言に共有されている (名前で引く判定は別の宣言に当たり得る)。
    fn name_is_shared(&self) -> bool {
        self.rust_site.is_some_and(|site| site.name_shared)
    }

    /// 名前の末尾と同じ名前の関数・メソッドが、新しい版のファイルに複数ある
    /// (同じファイルの中の呼び出しがどの宣言のものか決められない)。
    fn callee_is_shared(&self) -> bool {
        self.rust_site.is_some_and(|site| site.callee_shared)
    }
}

impl<'a> ChangedSignatureFiles<'a> {
    pub(super) fn new(
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        old_path: &'a str,
        new_path: &'a str,
        in_file_callees: &'a std::collections::HashSet<String>,
    ) -> Self {
        let skip_for_binary_crate = state
            .rust_public
            .is_binary_only_at_base(inputs.base_blobs, old_path)
            || is_binary_only_rust_crate(inputs.dir, new_path);
        Self {
            type_dependencies: None,
            old_path,
            new_path,
            moved_from: None,
            in_file_callees,
            skip_for_binary_crate,
            lang_id: crate::language::LangId::from_path(camino::Utf8Path::new(new_path)).ok(),
            rust_type_aliases: None,
            rust_const_additions: None,
            rust_moved_const_additions: std::collections::HashSet::new(),
        }
    }

    /// Rust の宣言を宣言の場所で組にした変更 1 件を判定し、対応する bucket へ入れる。形は
    /// 組にした宣言そのものから読み、同じ名前の宣言がファイルに複数あれば、名前で引く判定は
    /// 使わない (Issue #282)。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn classify_rust(
        &mut self,
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        name: &str,
        kind: &str,
        old_sig: &str,
        new_sig: &str,
        site: RustDeclarationSite,
        old_context: &RustDeclarationContext,
        inline_module: Option<String>,
        display: Option<(String, String)>,
    ) {
        let candidate = SignatureCandidate {
            rust_site: Some(site),
            rust_old_context: Some(old_context),
            ..SignatureCandidate::new(name, kind, old_sig, new_sig)
        };
        let (mut change, decision) = self.decide(inputs, state, candidate);
        change.inline_module = inline_module;
        // 互換の項目の signature は、impact の互換の証拠の照合に使うので書き換えない。
        if let Some((old_signature, new_signature)) = display {
            change.old_signature = Some(old_signature);
            change.new_signature = Some(new_signature);
        }
        record_signature_decision(inputs, state, change, decision);
    }

    /// シグネチャの変わったシンボル 1 件を判定し、対応する bucket へ入れる。
    pub(super) fn classify(
        &mut self,
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        name: &str,
        kind: &str,
        old_sig: &str,
        new_sig: &str,
    ) {
        let candidate = SignatureCandidate::new(name, kind, old_sig, new_sig);
        let (change, decision) = self.decide(inputs, state, candidate);
        record_signature_decision(inputs, state, change, decision);
    }

    /// シグネチャの変わったシンボル 1 件を判定する。bucket は書き換えない。
    ///
    /// 規則は次の順に評価し、最初に決まったものを返す (順序は判定の一部なので入れ替えない):
    /// 契約が名指す名前の変化 → Rust の定数の値だけの変更 → bin-only crate・公開 API 面の外 →
    /// 中の利用を直しても外の利用者を直せない形の変更 (inherent impl の見出し・Rust の構造体・enum・
    /// union・TypeScript のクラス・enum) → 移動と同時の const の追加 → TypeScript の型の依存の変化 →
    /// 同じファイルの中だけの利用 → const の追加・透過的な型別名 → 同じ名前の宣言・TypeScript の
    /// オーバーロード → 互換と呼び出し側の追随 ([`decide_signature_change`])。
    pub(super) fn decide(
        &mut self,
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        candidate: SignatureCandidate<'_>,
    ) -> (ApiSymbolChange, SignatureDecision) {
        // 参照の証拠と契約の種別は、blocking な api.mod に確定した時点で埋める
        // (record_signature_decision)。
        let change = ApiSymbolChange::new(
            candidate.name.to_owned(),
            candidate.kind.to_owned(),
            self.new_path.to_owned(),
            Some(candidate.old_sig.to_owned()),
            Some(candidate.new_sig.to_owned()),
        );
        if let Some(decision) = self.rust_dependency_decision(inputs, state, &candidate) {
            return (change, decision);
        }
        if self.is_rust_const_value_change(&candidate) {
            return (change, SignatureDecision::ConstValue);
        }
        // private module 抑制 (inherent impl の項目は impl の対象の型で決める。Issue #315)
        if self.skip_for_binary_crate || self.outside_public_api_surface(inputs, state, &candidate)
        {
            return (change, SignatureDecision::Omit);
        }
        if let Some(decision) = self.rust_impl_header_decision(inputs, &candidate) {
            return (change, decision);
        }
        if let Some(decision) = self.rust_shape_decision(inputs, &candidate) {
            return (change, decision);
        }
        if let Some(decision) = self.ts_shape_decision(inputs, state, &candidate) {
            return (change, decision);
        }
        // 移動先への全参照と const 追加を証明した組は、同一ファイル内だけの caller でも記録する。
        if self.lang_id == Some(crate::language::LangId::Rust)
            && self.rust_moved_const_additions.contains(candidate.name)
        {
            let site = self.compatible_site(inputs, &candidate);
            return (
                change,
                SignatureDecision::MovedConstAddition(site.compatible("rust_const_fn_added")),
            );
        }
        if self
            .type_dependencies
            .and_then(|types| types.get(candidate.name))
            == Some(&super::ts_signature::DependencyStatus::Changed)
        {
            let site = self.compatible_site(inputs, &candidate);
            let mut sources =
                SignatureSourceCache::with_base_blobs(inputs.base_blobs, inputs.strip_internal);
            let decision = local_props_alias_removal_decision(
                inputs,
                state.closure_caches,
                &site,
                &mut sources,
            )
            .unwrap_or(SignatureDecision::Blocking(None));
            return (change, decision);
        }
        // closed-in-diff: 同一ファイル内でしか呼ばれていない関数のシグネチャ変更は除外。
        //
        // ただし Python の TypedDict `total=` 変更候補だけはここで落とさない。
        // 落とすと契約変更の 3 値判定に到達できず、「キーが必須化する破壊的変更」が
        // api.mod にすら出なくなる。判定が `NotApplicable` なら
        // `decide_signature_change` 側で従来どおり捨てる。
        let internally_closed = self.is_internally_closed(inputs, state, &candidate);
        if internally_closed
            && !may_be_python_typed_dict_total_change(
                candidate.kind,
                candidate.old_sig,
                candidate.new_sig,
                self.lang_id,
            )
        {
            return (change, SignatureDecision::Omit);
        }
        if let Some(decision) = self.rust_const_addition_decision(inputs, &candidate) {
            return (change, decision);
        }
        if let Some(decision) = self.rust_type_alias_decision(inputs, &candidate) {
            return (change, decision);
        }
        // 同じ名前の宣言がファイルに複数あると、呼び出し側の追随も名前では宣言に結び付けられない
        // ので、互換・追随済みの証明に進まず blocking にする (Issue #282)。
        if candidate.name_is_shared() || self.has_ts_overloads(inputs, &candidate) {
            return (change, SignatureDecision::Blocking(None));
        }
        let site = SignatureChangeSite {
            change,
            old_path: self.old_path,
            new_path: self.new_path,
            moved_from: self.moved_from,
            kind: candidate.kind,
            old_sig: candidate.old_sig,
            new_sig: candidate.new_sig,
            lang_id: self.lang_id,
            internally_closed,
        };
        let decision =
            decide_signature_change(inputs, state.closure_caches, &site, self.type_dependencies);
        (site.change, decision)
    }

    /// `candidate` の互換判定器の共通入力。参照を束ねるだけで、読み込みや解析は始めない。
    fn compatible_site<'s>(
        &'s self,
        inputs: &'s DetectionInputs<'_>,
        candidate: &SignatureCandidate<'s>,
    ) -> CompatibleModSite<'s> {
        CompatibleModSite {
            dir: inputs.dir,
            base: inputs.base,
            old_path: self.old_path,
            new_path: self.new_path,
            name: candidate.name,
            kind: candidate.kind,
            old_sig: candidate.old_sig,
            new_sig: candidate.new_sig,
            lang_id: self.lang_id,
        }
    }

    /// 旧い版の宣言が公開 API 面の外にある (private module。inherent impl の項目は impl の対象の型で
    /// 決める。Issue #315)。
    fn outside_public_api_surface(
        &self,
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        candidate: &SignatureCandidate<'_>,
    ) -> bool {
        rust_declaration_outside_public_api_surface(
            inputs.rust_crates,
            state.rust_public,
            inputs.dir,
            inputs.base_blobs,
            true,
            self.old_path,
            candidate.name,
            candidate.rust_old_context,
        )
    }

    /// 契約が名指す名前の指す先が変わったか、同じと確かめられなければ、公開面の外でない限り、
    /// 値だけの変更・互換・同じファイルの中だけ・追随済みとせず blocking にする (Issue #283)。
    fn rust_dependency_decision(
        &self,
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        candidate: &SignatureCandidate<'_>,
    ) -> Option<SignatureDecision> {
        let reason = candidate.rust_site?.dependency?;
        Some(
            if self.skip_for_binary_crate
                || self.outside_public_api_surface(inputs, state, candidate)
            {
                SignatureDecision::Omit
            } else {
                SignatureDecision::Unverified(ApiModificationUnverified {
                    reason,
                    first_unproven: None,
                })
            },
        )
    }

    /// 通常の Rust の定数の値だけの変更か。値変更の方針は外部 API 面から独立なので、公開面の
    /// 抑制より先に残す。移動では公開面や型所有元の検査を省けないため、従来の判定経路を使う。
    /// 囲む inherent impl の見出しが変わった関連定数は、値だけの変更にしない (Issue #307)。
    fn is_rust_const_value_change(&self, candidate: &SignatureCandidate<'_>) -> bool {
        self.lang_id == Some(crate::language::LangId::Rust)
            && self.moved_from.is_none()
            && self.old_path == self.new_path
            && candidate
                .rust_site
                .and_then(|site| site.impl_header)
                .is_none()
            && (is_const_value_only_change(
                candidate.old_sig,
                candidate.new_sig,
                candidate.kind,
                crate::language::LangId::Rust,
            ) || candidate
                .rust_site
                .is_some_and(|site| site.impl_value_changed))
    }

    /// Rust の inherent impl の見出し (対象の型・型の引数の境界・`where`) の変更は、メソッドと関連定数を
    /// 使える型を変える (Issue #307)。中の利用を同じ差分で直しても外の利用者は直せないので、閉包や同じ
    /// ファイルの中だけの利用による除外より先に決める: 対象の型を変えるか境界を強めるか読めなければ
    /// blocking。境界を緩めるだけで signature が同じなら互換。signature も変わったなら、その判定に任せる。
    fn rust_impl_header_decision(
        &self,
        inputs: &DetectionInputs<'_>,
        candidate: &SignatureCandidate<'_>,
    ) -> Option<SignatureDecision> {
        use super::rust_type_api::ShapeChange;
        let site = candidate.rust_site?;
        match site.impl_header? {
            ShapeChange::Breaking => Some(SignatureDecision::Blocking(None)),
            // 関連定数の値の対応も変わったなら、見出しが互換でも値の変更を確かめられないので blocking。
            ShapeChange::Compatible if site.impl_value_changed => {
                Some(SignatureDecision::Blocking(None))
            }
            // signature の字面の違いが見出しの型の引数の名前の付け直しだけなら、同じ signature とする。
            ShapeChange::Compatible
                if candidate.old_sig == candidate.new_sig || site.impl_member_renamed =>
            {
                Some(SignatureDecision::Compatible(
                    self.compatible_site(inputs, candidate)
                        .compatible("rust_impl_header_compatible"),
                ))
            }
            _ => None,
        }
    }

    /// Rust の公開構造体・enum・union の形の変更は、中の利用を同じ差分で直しても外の利用者
    /// (構造体式・フィールドの読み書き・variant の構築と照合) を直せないので、閉包や同じファイルの
    /// 中だけの利用による除外より先に決める (Issue #270・#276・#277)。旧新の形を読めなければ
    /// blocking に残す。
    fn rust_shape_decision(
        &self,
        inputs: &DetectionInputs<'_>,
        candidate: &SignatureCandidate<'_>,
    ) -> Option<SignatureDecision> {
        if self.lang_id != Some(crate::language::LangId::Rust)
            || !matches!(candidate.kind, "struct" | "enum" | "union")
        {
            return None;
        }
        let shapes = match candidate.rust_site.and_then(|site| site.shape_override) {
            // `#[cfg]` の分岐の代表の組は、分岐の組ごとに比べた結果 (Issue #293)。
            Some(change) => Some(change),
            None => super::source_pair::load_new_source(inputs.dir, self.new_path)
                .and_then(|new| {
                    super::source_pair::load_old_source_with_new(
                        inputs.base_blobs,
                        self.old_path,
                        self.new_path,
                        new,
                    )
                })
                .and_then(|sources| {
                    let (old, new) = (&sources.old[..], &sources.new[..]);
                    let (site, name) = (candidate.rust_site, candidate.name);
                    match candidate.kind {
                        "struct" => compare_rust_nominal_shapes(
                            old,
                            new,
                            site,
                            name,
                            super::struct_shape_at,
                            super::rust_struct_api::unique_struct_shape,
                            super::rust_struct_api::classify,
                        ),
                        "enum" => compare_rust_nominal_shapes(
                            old,
                            new,
                            site,
                            name,
                            super::enum_shape_at,
                            super::rust_enum_api::unique_enum_shape,
                            super::rust_enum_api::classify,
                        ),
                        _ => compare_rust_nominal_shapes(
                            old,
                            new,
                            site,
                            name,
                            super::union_shape_at,
                            super::rust_union_api::unique_union_shape,
                            super::rust_union_api::classify,
                        ),
                    }
                }),
        };
        let reason = match candidate.kind {
            "struct" => "rust_struct_shape_compatible",
            "enum" => "rust_enum_shape_compatible",
            _ => "rust_union_shape_compatible",
        };
        Some(match shapes {
            // 整形・型の引数の改名・外から見えない変更だけで、公開の形は同じ。
            Some(super::rust_type_api::ShapeChange::Unchanged) => SignatureDecision::Omit,
            Some(super::rust_type_api::ShapeChange::Compatible) => SignatureDecision::Compatible(
                self.compatible_site(inputs, candidate).compatible(reason),
            ),
            Some(super::rust_type_api::ShapeChange::Breaking) | None => {
                SignatureDecision::Blocking(None)
            }
        })
    }

    /// TypeScript のクラスの公開のメンバー (フィールド・パラメータプロパティ・アクセサ・
    /// コンストラクタなど) と enum のメンバーの変更も、中の利用を同じ差分で直しても外の利用者
    /// (フィールドの読み書き・継承・`implements`・`Record<Mode, T>` など) を直せないので、閉包や
    /// 同じファイルの中だけの利用による除外より先に決める (Issue #286)。見出しだけの変更は従来
    /// どおりに判定する (None)。旧新の形を読めなければ blocking に残す。
    fn ts_shape_decision(
        &self,
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        candidate: &SignatureCandidate<'_>,
    ) -> Option<SignatureDecision> {
        if !matches!(
            self.lang_id,
            Some(crate::language::LangId::Typescript | crate::language::LangId::Tsx)
        ) || !matches!(candidate.kind, "class" | "enum")
        {
            return None;
        }
        if candidate.kind == "enum" {
            return super::ts_class_api::enum_members_changed(candidate.old_sig, candidate.new_sig)
                .then_some(SignatureDecision::Blocking(None));
        }
        let site = self.compatible_site(inputs, candidate);
        let name = candidate.name;
        let mut sources =
            SignatureSourceCache::with_base_blobs(inputs.base_blobs, inputs.strip_internal);
        let shape_change = site.lang_id.and_then(|lang| {
            let pair = sources.get(&site)?;
            let (old, new) = pair.parse_pair(lang)?;
            Some(super::ts_class_api::classify_class(
                &super::ts_class_api::unique_class_shape(old.root_node(), &pair.old, name)?,
                &super::ts_class_api::unique_class_shape(new.root_node(), &pair.new, name)?,
            ))
        });
        match shape_change {
            Some(super::ts_class_api::ClassChange::HeaderOnly) => None,
            // optional のフィールドの追加だけでも、メンバーの型が名指す同じファイルの型が
            // 変わっていれば互換と言えない。
            Some(super::ts_class_api::ClassChange::Compatible)
                if self.type_dependencies.and_then(|types| types.get(name))
                    == Some(&super::ts_signature::DependencyStatus::Changed) =>
            {
                Some(SignatureDecision::Blocking(None))
            }
            Some(super::ts_class_api::ClassChange::Compatible) => Some(
                SignatureDecision::Compatible(site.compatible("ts_class_optional_members_added")),
            ),
            Some(super::ts_class_api::ClassChange::Breaking) => {
                // コンストラクタの省略可能な引数追加も形の差になる。ほかの構造と依存が
                // 変わらない証明を通したものだけ、クラスの早期 blocking から分ける。
                detect_trailing_optional_params_compatible_mod(&site, &mut sources).map_or(
                    Some(SignatureDecision::Blocking(None)),
                    |compat| {
                        let status = self
                            .type_dependencies
                            .and_then(|types| types.get(name))
                            .copied()
                            .unwrap_or_default();
                        let dependencies_unchanged = status
                            != super::ts_signature::DependencyStatus::Changed
                            && (status != super::ts_signature::DependencyStatus::Unknown
                                || super::ts_signature::compatible_dependencies_unchanged(
                                    &site,
                                    &mut sources,
                                    &mut state.closure_caches.optional_removal,
                                    inputs.diff_files,
                                    &compat.reason,
                                    self.type_dependencies,
                                ));
                        Some(if dependencies_unchanged {
                            SignatureDecision::Compatible(compat)
                        } else {
                            SignatureDecision::Blocking(None)
                        })
                    },
                )
            }
            None => Some(SignatureDecision::Blocking(None)),
        }
    }

    /// 同じファイルの中でしか呼ばれておらず、ほかのファイルからの参照も無いか。
    ///
    /// default export / 別名 export (`export { f as g }`) の利用者は宣言名で現れないので、
    /// 宣言名の cross-file 参照 0 件は「同一ファイル内でしか使われていない」証明にならない。
    /// 同じ名前の宣言がファイルに複数あると、同じファイルの中の呼び出しがどの宣言のものか
    /// 決められないので、同じファイルの中だけで使う証明にしない (Issue #282)。
    fn is_internally_closed(
        &self,
        inputs: &DetectionInputs<'_>,
        state: &mut DetectionState<'_>,
        candidate: &SignatureCandidate<'_>,
    ) -> bool {
        let name = candidate.name;
        !candidate.name_is_shared()
            && !candidate.callee_is_shared()
            && is_internally_connected(self.in_file_callees, name)
            && !has_cross_file_refs(inputs.ref_index, self.new_path, name)
            && !super::js_public_name::published_names_for_diff_file(
                inputs.dir,
                inputs.base_blobs,
                self.old_path,
                self.new_path,
                state.closure_caches,
            )
            .is_some_and(|names| !name.contains('.') && names.may_differ(name))
    }

    /// Rust の関数に `const` を足しただけなら、const の文脈の外からの呼び出しは変わらない
    /// ので非 blocking (Issue #113)。外す変更とほかの部分も変えた変更は証明に入らない。
    fn rust_const_addition_decision(
        &self,
        inputs: &DetectionInputs<'_>,
        candidate: &SignatureCandidate<'_>,
    ) -> Option<SignatureDecision> {
        (self.lang_id == Some(crate::language::LangId::Rust)
            && self
                .rust_const_additions
                .is_some_and(|names| names.contains(candidate.name)))
        .then(|| {
            SignatureDecision::Compatible(
                self.compatible_site(inputs, candidate)
                    .compatible("rust_const_fn_added"),
            )
        })
    }

    /// Rust の透過的な root public type alias の導入・展開だけで説明できる表記変更は
    /// 非 blocking。同一ファイルの候補間では old/new alias index を再利用する。
    /// 同じ名前の宣言がファイルに複数あると、名前で引く関数の見出しが別の宣言のものになり
    /// 得るので証明しない (Issue #282)。
    fn rust_type_alias_decision(
        &mut self,
        inputs: &DetectionInputs<'_>,
        candidate: &SignatureCandidate<'_>,
    ) -> Option<SignatureDecision> {
        if self.lang_id != Some(crate::language::LangId::Rust)
            || candidate.kind != "function"
            || candidate.name_is_shared()
        {
            return None;
        }
        let (dir, base_blobs) = (inputs.dir, inputs.base_blobs);
        let (old_path, new_path) = (self.old_path, self.new_path);
        let context = self.rust_type_aliases.get_or_insert_with(|| {
            let new_source = super::source_pair::load_new_source(dir, new_path)?;
            let prepared =
                crate::engine::rust_signature::RustTypeAliasEquivalence::prepare_new_source(
                    &new_source,
                )?;
            let sources = super::source_pair::load_old_source_with_new(
                base_blobs, old_path, new_path, new_source,
            )?;
            prepared.with_old_source(&sources.old)
        });
        // 移動では旧新が別のファイルなので、名前が同じものを指すことも確かめる。
        let moved = self.moved_from.is_some();
        let proven = context.as_ref().is_some_and(|context| {
            if moved {
                context.prove_moved_function(candidate.name)
            } else {
                context.prove_function(candidate.name)
            }
        });
        proven.then(|| {
            SignatureDecision::Compatible(
                self.compatible_site(inputs, candidate)
                    .compatible("transparent_rust_type_alias"),
            )
        })
    }

    /// TypeScript の関数・メソッドのオーバーロード: 宣言ファイルに出るのはオーバーロードの宣言の
    /// 並びで、既存の判定 (末尾の optional 引数の追加などの互換・呼び出し側の追随) は 1 つの
    /// シグネチャを前提にする。旧新のどちらかにオーバーロードがあれば blocking にする
    /// (Issue #286)。オーバーロードの足し引きと並べ替えは、呼び出しの解決や推論を変え得る。
    fn has_ts_overloads(
        &self,
        inputs: &DetectionInputs<'_>,
        candidate: &SignatureCandidate<'_>,
    ) -> bool {
        if !matches!(
            self.lang_id,
            Some(crate::language::LangId::Typescript | crate::language::LangId::Tsx)
        ) || !matches!(candidate.kind, "function" | "method")
        {
            return false;
        }
        let site = self.compatible_site(inputs, candidate);
        let name = candidate.name;
        let mut sources =
            SignatureSourceCache::with_base_blobs(inputs.base_blobs, inputs.strip_internal);
        let overloaded = site.lang_id.and_then(|lang| {
            let pair = sources.get(&site)?;
            let (old, new) = pair.parse_pair(lang)?;
            Some(
                super::ts_class_api::has_overloads(old.root_node(), &pair.old, name) == Some(true)
                    || super::ts_class_api::has_overloads(new.root_node(), &pair.new, name)
                        == Some(true),
            )
        });
        overloaded == Some(true)
    }
}

/// Rust の構造体・enum・union の旧新の形を比べる。宣言の場所で組にした変更はその場所の宣言、
/// それ以外は名前で一意に引いた宣言の形を読む (旧い版から読み、読めなければ新しい版は読まない)。
/// 形を読めなければ None (呼び出し側は blocking に残す)。
pub(super) fn compare_rust_nominal_shapes<S>(
    old: &[u8],
    new: &[u8],
    site: Option<RustDeclarationSite>,
    name: &str,
    shape_at: fn(&[u8], (usize, usize)) -> Option<S>,
    unique_shape: fn(&[u8], &str) -> Option<S>,
    classify: fn(&S, &S, bool) -> super::rust_type_api::ShapeChange,
) -> Option<super::rust_type_api::ShapeChange> {
    let (old_shape, new_shape) = match site {
        Some(site) => (
            shape_at(old, site.old_start)?,
            shape_at(new, site.new_start)?,
        ),
        None => (unique_shape(old, name)?, unique_shape(new, name)?),
    };
    Some(classify(
        &old_shape,
        &new_shape,
        site.is_some_and(|site| site.availability_compared),
    ))
}

/// シグネチャ変更 1 件の分類結果。判定 ([`decide_signature_change`]) と bucket への格納
/// ([`record_signature_decision`]) を分け、格納を 1 か所にまとめる。
pub(super) enum SignatureDecision {
    /// どの bucket にも出さない (bin-only crate・private module・同一ファイル内でしか
    /// 使われていない変更)。
    Omit,
    /// 型注釈の表記だけが変わり、型は同じ (unchanged_type_annotations)。
    UnchangedTypeAnnotation,
    /// 型注釈だけの変更 (type_annotation_changes)。
    TypeAnnotationOnly,
    /// 値バインディングの値だけの変更 (const_value_changes)。
    ConstValue,
    /// 型注釈のない静的 container の整形だけの変更 (unchanged_static_consts)。
    UnchangedStaticConst,
    /// 呼び出し側を壊さない変更 (compatible_modified)。
    Compatible(CompatibleApiModification),
    /// const 追加に加え、移動元と移動先の型所有元まで証明した自由関数。
    MovedConstAddition(CompatibleApiModification),
    /// 全 cross-file 参照が同一 diff で追随済み (modified_closed_in_diff)。
    ClosedInDiff,
    /// callback 本体だけの変更 (callback_body_changes)。
    CallbackBodyOnly,
    /// blocking な api.mod。型契約の種別を確定できたときだけ添える。
    Blocking(Option<crate::models::review::ApiContractChange>),
    Unverified(ApiModificationUnverified),
}

impl SignatureDecision {
    /// 移動と同じ差分で変わった自由関数・定数の組 ([`process_moved_symbol_changes`]) を移動と
    /// みなせる判定か。宣言の比較だけで決まり、移動先で名前が同じものを指すことを確かめられる
    /// 証拠 (値だけの変更と、Rust の透過的な型別名) に限る。互換性を確かめていない変更
    /// (`TypeAnnotationOnly`・`CallbackBodyOnly`)、出さないだけの変更 (`Omit`)、呼び出し側の追随で
    /// 閉じた変更 (`ClosedInDiff`)、参照の用途で決まる互換判定 (旧パスを指す参照を区別しない) は
    /// 含めない。区分や理由を新しく足しても、ここに足さない限り移動には使わない。
    pub(super) fn proves_compatible_move(&self) -> bool {
        match self {
            Self::ConstValue | Self::MovedConstAddition(_) => true,
            Self::Compatible(compat) => compat.reason == "transparent_rust_type_alias",
            _ => false,
        }
    }
}
