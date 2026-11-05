# TypeScript / TSX grammar patch

This directory vendors `tree-sitter-typescript` 0.23.2 under its original MIT
license. The published crate is the baseline: SHA-256
`6c5f76ed8d947a75cc446d5fccd8b602ebf0cde64ccf2ffa434d873d7a575eff`.
Its VCS record names upstream commit
`f975a621f4e7f532fe322e13c4f79495e0a7b2e7`. The crate omits the license and
regeneration metadata. The license and `tree-sitter.json` are restored from that
commit; the minimal private npm manifest and lock are created for reproducibility
using the JavaScript grammar version pinned by upstream. Rust bindings,
queries, scanners and the shared grammar originate from the crate. Packaging is
reduced to the Rust build and grammar regeneration; contact emails are omitted.
Standalone tests use the CLI's tree-sitter 0.27 runtime (compatible with ABI 14).
The regeneration dependency is pinned to upstream's `tree-sitter-javascript`
**0.23.1**, with an integrity-checked npm lock file.

Upstream: <https://github.com/tree-sitter/tree-sitter-typescript>.
Local regressions: <https://github.com/owayo/astro-sight/issues/33> and
<https://github.com/owayo/astro-sight/issues/160>, and
<https://github.com/owayo/astro-sight/issues/235>.

## Type query conflicts (#33)

The static precedence between `call_expression` and `_type_query_call_expression`
commits too early when a type argument contains `typeof import("module")`, such
as `await load<typeof import("node:fs")>()`. Move only that pair from static
precedences into runtime conflicts in `common/define-grammar.js`, shared by
TypeScript and TSX. Keep the separate type-annotation precedence unchanged.
The patch changes no node kinds, fields or subtypes. CLI 0.26.6 adds
`extra: true` metadata to the existing `comment` and `html_comment` entries;
these two additive flags also appear when regenerating the unmodified baseline.
The patched node schemas equal those unmodified regenerated schemas.

## Import type arguments (#160)

The original `generic_type` rule only accepts local and namespace-qualified type
identifiers as its `name`. An import type such as
`import('./box.js').Box<string>` consequently stops at the existing
`member_expression`, leaving `<string>` in an `ERROR` node. Allow the existing
import-only `_type_query_member_expression_in_type_annotation` alias as one
more `generic_type.name` alternative. The import path and qualifier retain their
existing `member_expression` / `call_expression` structure; the complete type
is a `generic_type` with a `name` and `type_arguments`.

This also composes with the existing array, union, intersection, lookup and
nested generic type rules, including qualified imports such as
`import('./box.js').Namespace.Box<string>`. It does not change the expression
grammar, the earlier type-query conflict patch, uninstantiated import types,
or `typeof import(...)`. No node kind or field is added; the only schema change
is that `generic_type.name` also permits `member_expression` in both grammars.
Scanners, headers, Rust bindings, queries and regeneration dependencies are
unchanged. Generated C parser tables, grammar JSON and node schemas are checked
in for both TypeScript and TSX.

Parser and CLI regressions cover the reported annotation, qualified and nested
types, later declarations and calls, import/reference extraction, raw byte
ranges with Unicode and CRLF, incremental recovery, and malformed input that
must still report diagnostics. Existing type-query, expression, generic arrow
and JSX controls remain part of the grammar regression suite.

Against the preceding patched grammar, all 112 shared upstream corpus cases
from `f975a621f4e7f532fe322e13c4f79495e0a7b2e7` and 16 expression controls
retained identical complete trees, fields and ranges in TypeScript and TSX;
TSX also retained its JSX control (128 TypeScript / 129 TSX cases). TypeScript
7.0.2 accepted the new valid import-type variants in both file kinds with
`--strict --noEmit --module nodenext --jsx preserve`.

## Generic tagged templates (#235)

Accept a template string in the existing generic call's `arguments` field.
The ordinary call branch already owns `type_arguments`; using the same prefix
keeps generic calls and tagged templates ambiguous until their argument delimiter
is available. Adding type arguments to the higher-precedence `template_call`
branch instead commits too early and breaks ordinary `call<Item>()` parsing.
The untyped template branch and optional-call branch are unchanged. No node kind,
field or node schema changes; both generated parsers and grammar JSON are updated.

Regressions cover TypeScript and TSX, object/array/nested/import type arguments,
substitutions, Unicode/CRLF byte ranges, incremental recovery, malformed input,
and unchanged ordinary-call, comparison, generic-arrow and JSX controls.
CLI regressions check the reported async function through normal/index review
and hook output: optional/default tail additions are compatible, while required
parameters, changed parameter types and genuine body parse errors remain blocking.

All 112 cases in the pinned upstream corpus pass through the configured language
modes. TypeScript 7.0.2 accepts the reported optional-tail examples in both file
kinds with strict checking. Regenerating with the pinned CLI reproduces both
parser C files, grammar JSON and node schemas byte-for-byte.

## Regenerate

Use tree-sitter CLI **0.26.6**, with grammar ABI **14**, and Node.js/npm:

```sh
cd vendor/tree-sitter-typescript
npm ci --ignore-scripts --no-audit --no-fund
(cd typescript && tree-sitter generate --abi 14)
(cd tsx && tree-sitter generate --abi 14)
```

The npm package is private and used only for regeneration. `node_modules` is not
vendored. Generated C parsers and headers are checked in; normal Cargo builds
need no Node.js, npm or grammar generator. A generator upgrade may alter unrelated
tables and should be reviewed separately.

Before removing this patch in favor of an upstream release, run the TypeScript
and TSX grammar regression tests, CLI grammar tests and upstream corpus. The
original crate, its unmodified regeneration and the patched parser produced
identical full ASTs for all 112 upstream corpus cases in each language. Expression
controls also retained their complete ASTs; these include comparisons, dynamic
imports, generic calls, optional calls, tagged templates and JSX.

## Known upstream limitations

The original 0.23.2 grammar places `await_expression` in the function field of
a bare `await call<Item>()`. The CLI's `calls` command consequently has no callee
for that form. `await (call<Item>())` has the expected outer await and inner call,
and its callee is extracted. That ordinary-call precedence limitation remains.
Generic tagged templates such as `` tag<Item>`value` `` were represented as
binary expressions by the original grammar; the #235 patch above deliberately
corrects their structure to `call_expression`.
