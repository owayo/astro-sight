# Swift grammar patch

This directory vendors `tree-sitter-swift` 0.7.3 under its original MIT license.
The published crate is the baseline: SHA-256
`fe36052155b9dd69ca82b3b8f1b4ccfb2d867125ac1a4db1dd7331829242668c`.
Its VCS record names upstream commit
`b8b22bffbb3441780e6471665bacfb263741c86a` with `dirty = true`, so the crate
must not be assumed to equal that commit. The missing `grammar.js` and
`tree-sitter.json` come from that commit; the grammar rules match the crate's
`src/grammar.json`. Runtime sources, Rust bindings, queries and license originate
from the crate. Packaging is reduced to the Rust build and grammar regeneration;
contact email metadata is omitted. Redundant blank lines at EOF are trimmed
in the scanner and three query files; query contents otherwise match the crate. Standalone Rust tests use the same tree-sitter
0.27 runtime as the CLI, matching the generated ABI 15 parser.

Upstream: <https://github.com/alex-pinkus/tree-sitter-swift>.
Local regression: <https://github.com/owayo/astro-sight/issues/32>.
Empty-tuple regression: <https://github.com/owayo/astro-sight/issues/41>.
Conditional try/await regression: <https://github.com/owayo/astro-sight/issues/200>.
Windows forced-try regression: <https://github.com/owayo/astro-sight/issues/201>.

## Change

An optional type suffix needs an adjacent `??`, while nil coalescing can follow
whitespace, comments or a newline. The original scanner used the same external
token for both and could consume a multiline nil coalescing operator as a type
suffix after `as? String`.

Append a dedicated `_immediate_double_quest` external token and use it only in
`optional_type`. Scan it before skipping whitespace or comments. Keep the
existing nil coalescing token and scanner serialization unchanged. Disable this
new token during the all-symbols-valid error-recovery probe. Generated node-type
entries retain the upstream schema.

Swift also permits an empty tuple `()` as a value and pattern. Both
`tuple_expression` and `_tuple_pattern` now allow zero elements so valid uses
such as `.success(())`, `[()]`, and `case .success(())` do not produce missing
tokens or error nodes. The generated `tuple_expression.value` field is optional.

The operand of `try` also gives a direct `await_expression` the same left
associativity as a direct call, as a dynamically lower-priority alternative.
The original broad-operand interpretations remain preferred for binary,
ternary, and postfix expressions. Without the alternative, `if let x = try? await load()` can
absorb the conditional block as a trailing closure and consume subsequent
declarations during recovery. A conflict with `_primary_expression` keeps the
alternative postfix interpretation available. Existing node names and fields,
the scanner, and source bytes are unchanged; actual trailing closures remain
part of their calls.

The scanner's suppression mask for `FAKE_TRY_BANG` (token 32) uses `1ULL`
instead of `1UL`. Windows has a 32-bit `unsigned long`, so shifting `1UL` by 32
cannot construct the required 64-bit mask. The failed suppression lets the
external scanner consume the `!` in `try!` as boolean negation. The 64-bit
literal matches the existing mask storage and reader. Token order and scanner
serialization are unchanged, and this scanner-only fix needs no regeneration.
Regression tests distinguish `try!` from `try !flag()` and exercise forced
try/await bindings through CLI definitions, calls, references, and Unicode/CRLF
positions.

## Regenerate

Use Node.js and tree-sitter CLI **0.26.6**, with grammar ABI **15**:

```sh
cd vendor/tree-sitter-swift
tree-sitter generate --abi 15
```

The generated C parser and headers are checked in. Normal Cargo builds need no
Node.js or grammar generator. A generator upgrade may change unrelated tables;
keep it separate from this patch.

Before removing this vendor patch in favor of an upstream release, run the Swift
grammar regression tests (including double optional closure parameters), the
CLI grammar tests and the upstream corpus. The original crate, its unmodified
regeneration and the patched parser produced identical full ASTs for all 238
upstream corpus cases. The regression tests additionally cover the reported
multiline cast/coalescing expression and source ranges.
The empty-tuple patch keeps the full ASTs of the current upstream corpus sources
byte-identical and adds a CLI regression for values, patterns, and unaffected
parenthesized expressions, calls, and function types.
The try/await patch also preserves full ASTs, including ranges, for all 238
corpus cases from the pinned upstream commit. Parser and CLI regressions cover
conditional bindings (`if`, `guard`, `while`, and `case`), ordinary calls and
trailing closures, broad binary/ternary/postfix operands, CRLF/Unicode positions, incremental repair, diagnostics for
malformed input, and following definitions, references, and calls.

## Existing limitation

The adjacent-comment expression `value as? String/**/?? "fallback"` retains an
upstream AST ambiguity (`optional_type` plus `infix_expression`) in all three
parsers. This patch fixes the reported multiline expression; it does not resolve
that existing comment-trivia case. Spaced comments and newline cases are covered
by the regression tests.
