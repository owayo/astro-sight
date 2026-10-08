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
Local regression: <https://github.com/owayo/astro-sight/issues/33>.

## Change

The static precedence between `call_expression` and `_type_query_call_expression`
commits too early when a type argument contains `typeof import("module")`, such
as `await load<typeof import("node:fs")>()`. Move only that pair from static
precedences into runtime conflicts in `common/define-grammar.js`, shared by
TypeScript and TSX. Keep the separate type-annotation precedence unchanged.
The patch changes no node kinds, fields or subtypes. CLI 0.26.6 adds
`extra: true` metadata to the existing `comment` and `html_comment` entries;
these two additive flags also appear when regenerating the unmodified baseline.
The patched node schemas equal those unmodified regenerated schemas.

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
and its callee is extracted. Generic tagged templates such as
`` tag<Item>`value` `` are already represented as binary expressions by the
original grammar. The patched parsers preserve the original full ASTs for these
controls. They remain separate precedence limitations; this patch fixes the
reported import type query parse error and restores subsequent declarations.
