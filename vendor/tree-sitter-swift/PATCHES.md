# Swift grammar patch

This directory vendors `tree-sitter-swift` 0.7.4 under its original MIT license.
The published crate is the baseline: SHA-256
`2688c06564afa397313e0db17d140a73e2c6a47f3884794614246167c9dd5dfd`.
Its VCS record names upstream commit
`171fa3bc343233fe07f5e17205aaada33d778825` (tag `0.7.4`) with `dirty = true`.
The missing `grammar.js` and `tree-sitter.json` come from that commit.
Regenerating that unmodified `grammar.js` with the CLI below reproduces the
crate's `src/grammar.json` and `src/parser.c` byte for byte. Runtime sources,
Rust bindings, queries and license originate from the crate. Packaging is
reduced to the Rust build and grammar regeneration; contact email metadata is
omitted. Redundant blank lines at EOF are trimmed in three query files; query
contents otherwise match the crate. Standalone Rust tests use the same
tree-sitter 0.27 runtime as the CLI, matching the generated ABI 15 parser.

Upstream: <https://github.com/alex-pinkus/tree-sitter-swift>.
Empty-tuple regression: <https://github.com/owayo/astro-sight/issues/41>.
Conditional try/await regression: <https://github.com/owayo/astro-sight/issues/200>.

## Changes

Swift permits an empty tuple `()` as a value and pattern. Both
`tuple_expression` and `_tuple_pattern` allow zero elements so valid uses such
as `.success(())`, `[()]`, and `case .success(())` do not produce missing tokens
or error nodes. Upstream 0.7.4 still requires an element and parses `()` as a
tuple holding a zero-width `bang`. The generated `tuple_expression.value` field
is optional.

Upstream 0.7.4 gives the operand of `try` a direct `await_expression`
alternative with the same left associativity as a direct call, so that
`if let x = try? await load() { ... }` no longer absorbs the conditional block
as a trailing closure. Without a dynamic precedence, that alternative also wins
over the broad operands, and `try` loses the rest of the expression:
`try await load().count` parses as `(try await load()).count`, and
`try await ready() ? yes() : no()` puts the ternary inside `await`. The patch
gives the alternative a dynamic precedence of -1, as the 0.7.3-based vendor did.
Binary, ternary, nil-coalescing and postfix operands then stay inside `try`,
and the conditional block stays out of the call. The scanner, node names and
fields are unchanged.

## Patches taken over by upstream

The 0.7.3-based vendor carried two more patches. Upstream 0.7.4 fixes the same
problems, so they were dropped. Their regression tests stay in place and pass
with the upstream implementations.

- Multiline nil coalescing after `as? String`
  (<https://github.com/owayo/astro-sight/issues/32>): upstream commit
  `b5fa894c60` adds the `_double_optional` external token for an adjacent
  `??` type suffix, replacing the local `_immediate_double_quest` token.
- `try!` parsed as boolean negation on Windows
  (<https://github.com/owayo/astro-sight/issues/201>): upstream commit
  `6ab8d1d74e` uses the same `1ULL` mask for `FAKE_TRY_BANG`.

## Regenerate

Use Node.js and tree-sitter CLI **0.26.6**, with grammar ABI **15**:

```sh
cd vendor/tree-sitter-swift
tree-sitter generate --abi 15
```

The generated C parser and headers are checked in, and the command reproduces
them byte for byte. Normal Cargo builds need no Node.js or grammar generator.
The published crate's `src/node-types.json` lacks the anonymous `?` and `??`
entries that this CLI writes, and its `src/tree_sitter/array.h` comes from a
newer generator (C++ casts only); the checked-in files are the CLI 0.26.6
output. A generator upgrade may change unrelated tables; keep it separate from
this patch.

Before removing this vendor patch in favor of an upstream release, run the Swift
grammar regression tests (including double optional closure parameters and the
`try await` operand spans), the CLI grammar tests and the upstream corpus. For
all 296 corpus cases of the pinned upstream commit, the unmodified regeneration
and the patched parser produce identical full ASTs, including ranges; across the
corpus and the Swift fixtures, only the empty-tuple fixture differs. The corpus
has no `try await` with a postfix, navigation, ternary or nil-coalescing
operand, so the parser regressions cover those spans.

## Existing limitation

The adjacent-comment expression `value as? String/**/?? "fallback"` retains an
upstream AST ambiguity (`optional_type` plus `infix_expression`). Upstream
0.7.4 and the patched parser parse it the same way as the 0.7.3-based vendor
did. Spaced comments and newline cases parse as nil coalescing and are covered
by the regression tests.

`unsafe` (SE-0458), new in upstream 0.7.4, binds only the next primary
expression: `unsafe load().count` parses as `(unsafe load()).count`. Following
declarations are kept.
